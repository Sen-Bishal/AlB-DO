//! JOBS · 15.5 — the running half of `dom_render_compiler::jobs`.
//!
//! The compiler crate finds `src/jobs.ts`, lowers each declaration and owns the
//! queue's SQL. This module owns the parts that need a process: a clock, an
//! engine pool, and a task that outlives any one request.
//!
//! ## Why there is no tick
//!
//! The obvious runner wakes every second and asks whether anything is due. This
//! one does not wake at all unless something is.
//!
//! On each pass it computes the earliest of two instants — the next scheduled
//! fire (from the declarations, in memory) and the earliest future queue row
//! (one indexed `MIN(run_at)`) — and sleeps until exactly that, or until
//! [`JobHandle::wake`] pokes it because work was enqueued in the meantime. An
//! app with a nightly job and an empty queue therefore costs **one wakeup per
//! night**, not 86 400.
//!
//! That is not tidiness. This project spent four engine-pool rounds taking a
//! concurrent request's p50 from 59 µs to 27, and a 1 Hz timer waking a parked
//! core on an idle box would hand a visible slice of that back for nothing.
//!
//! ## Missed fires are not caught up
//!
//! A schedule is a set of instants, and the runner only ever enqueues the next
//! one it is waiting for. If the process is down from Monday to Friday, Friday's
//! boot does not enqueue four nightly digests — it enqueues the next one.
//!
//! This is the behaviour an operator expects from cron and the opposite of what
//! a naive "catch up from `last_run`" scheduler does, which is to emit a burst
//! of backdated work at the worst possible moment: immediately after an outage.
//! An app that genuinely needs the missed run wants a job that reads its own
//! watermark, which it can do because it has the database.
//!
//! ## What the fan-out is for
//!
//! A scheduled job declaring `over: "users"` does not run. Its row expands, a
//! page at a time, into one child row per principal, each carrying exactly that
//! principal — so a per-user digest exists without any identity that can see
//! every user's rows. The cursor lives in the row, so an expansion interrupted
//! by a restart resumes where it stopped instead of starting over (which would
//! re-send) or giving up (which would skip everyone after the cut).

use std::sync::Arc;
use std::time::Duration;

use dom_render_compiler::forge::substrate::DataSubstrate;
use dom_render_compiler::jobs::builtin::Builtin;
use dom_render_compiler::jobs::declare::{FanOut, JobsDecl, FANOUT_BATCH};
use dom_render_compiler::jobs::queue::{self, ClaimedJob, Enqueue, DEFAULT_LEASE_MS};
use dom_render_compiler::runtime::quickjs_engine::JobRun;
use tokio::sync::Notify;

use crate::engine_pool::QuickJsEnginePool;

/// Longest the runner will sleep with nothing scheduled.
///
/// Not a poll: with an empty queue and no schedules there is nothing for this to
/// discover, and [`JobHandle::wake`] covers everything that could arrive. It is
/// a backstop against a clock that jumps — a laptop resuming from sleep, a VM
/// whose host corrected it — after which an instant computed before the jump may
/// be arbitrarily far away.
const MAX_SLEEP: Duration = Duration::from_secs(300);

/// How long after finishing a `done` row is kept before the sweep removes it.
const DONE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// How often the sweep runs, in passes of the loop.
const SWEEP_EVERY: u32 = 64;

/// A project's jobs, ready to run.
pub(crate) struct JobsPlan {
    entry: Arc<str>,
    decl: Arc<JobsDecl>,
    modules: Arc<Vec<(String, String)>>,
    pool: Arc<QuickJsEnginePool>,
    /// Where a job's `fetch()` goes out through.
    ///
    /// 🪤 The shared **cell**, not a snapshot of it. The client is installed
    /// after `build()` returns, so a plan that captured the value here would
    /// capture `None` and every job's `fetch()` would fail with "no APERTURE
    /// client installed" on a server that has one. Read at run time instead.
    client: Arc<std::sync::OnceLock<Arc<dom_render_compiler::aperture::ApertureClient>>>,
}

impl JobsPlan {
    pub(crate) fn new(
        entry: String,
        decl: JobsDecl,
        modules: Vec<(String, String)>,
        pool: Arc<QuickJsEnginePool>,
        client: Arc<std::sync::OnceLock<Arc<dom_render_compiler::aperture::ApertureClient>>>,
    ) -> Self {
        Self {
            entry: Arc::from(entry),
            decl: Arc::new(decl),
            modules: Arc::new(modules),
            pool,
            client,
        }
    }

    /// How many of this app's jobs run on a schedule. For the boot line.
    pub(crate) fn scheduled_count(&self) -> usize {
        self.decl.scheduled().count()
    }

    /// Run one body to completion, replaying `fetch()` the way a middleware
    /// pass does, and return what it recorded.
    ///
    /// Nothing is applied here. The effects cross back to the runner, which owns
    /// the substrate and the ordering — the same split the action adapter uses,
    /// and the reason a suspended pass can throw its staged effects away without
    /// anything having been committed.
    async fn run(
        &self,
        job: &ClaimedJob,
        timeout: Duration,
    ) -> Result<Vec<dom_render_compiler::runtime::bridge::HandlerEffect>, String> {
        use dom_render_compiler::aperture::{Journal, DEFAULT_PASS_CAP};

        let args: Arc<str> = Arc::from(job.args.as_str());
        // The principal this run is *for*, already resolved. `null` is anonymous
        // and there is no third option — see `jobs::declare`'s identity note.
        let user: Arc<str> = Arc::from(match &job.principal {
            Some(principal) => format!(r#"{{"id":{}}}"#, serde_json::Value::String(principal.clone())),
            None => "null".to_string(),
        });
        let mut journal: Option<Journal> = None;

        for _ in 0..DEFAULT_PASS_CAP {
            let modules = Arc::clone(&self.modules);
            let entry = Arc::clone(&self.entry);
            let args = Arc::clone(&args);
            let user = Arc::clone(&user);
            let name = job.name.clone();
            let journal_json = journal.as_ref().map(|j| j.to_script_value().to_string());

            // The deadline is enforced by the pool's own interrupt, which is
            // what makes a `while (true) {}` body recoverable: the engine is
            // interrupted, its realm rebuilt, and the slot returns to service.
            let pass = self
                .pool
                .with_engine_budget(Some(timeout), move |engine| -> Result<JobRun, String> {
                    use dom_render_compiler::runtime::engine::RuntimeEngine;
                    for (specifier, code) in modules.iter() {
                        engine
                            .load_module(specifier, code)
                            .map_err(|err| err.to_string())?;
                    }
                    engine
                        .eval_job(
                            &entry,
                            &name,
                            &args,
                            &user,
                            journal_json.as_deref().unwrap_or("[]"),
                        )
                        .map_err(|err| err.to_string())
                })
                .await
                .map_err(|err| err.to_string())?
                .map_err(|err| err)?;

            let pending = match pass {
                JobRun::Completed { effects, .. } => return Ok(effects),
                JobRun::Suspended {
                    pending,
                    journal_len,
                } => {
                    let held = journal.as_ref().map_or(0, Journal::len);
                    if journal_len as usize != held {
                        return Err(format!(
                            "a pass saw {journal_len} recorded fetch() answers but {held} exist"
                        ));
                    }
                    pending
                }
            };

            let Some(client) = self.client.get() else {
                return Err(
                    "called fetch(), but no APERTURE client is installed to send it".to_string(),
                );
            };
            // Keyed by the row, not by the attempt: a retry of the same row is
            // the same decision, so an upstream that honours idempotency keys
            // will not charge twice for a job that failed after its call landed.
            let journal = journal
                .get_or_insert_with(|| Journal::new(format!("job_{}", job.id), "job"));
            dom_render_compiler::aperture::resolve_pending(client, journal, &pending, None)
                .await
                .map_err(|err| err.to_string())?;
        }

        Err(format!(
            "made fetch() calls across more than {} passes",
            dom_render_compiler::aperture::DEFAULT_PASS_CAP
        ))
    }
}

/// What the rest of the server holds onto: a way to wake the runner, and what it
/// needs to write a row the runner will accept.
///
/// The declaration rides along because **both** paths that enqueue have to agree
/// about what a valid job name is — an action calling `enqueue()` and a job body
/// calling it. Two answers to that question is two behaviours for one typo.
#[derive(Clone)]
pub(crate) struct JobHandle {
    wake: Arc<Notify>,
    /// `None` for an app with no `src/jobs.ts`; every `enqueue()` is then a
    /// refusal, which is correct — there is nothing it could name.
    decl: Option<Arc<JobsDecl>>,
}

impl JobHandle {
    /// Tell the runner something is due sooner than it currently believes.
    ///
    /// Cheap and idempotent: a wake with nothing to do costs one pass that finds
    /// nothing and sleeps again.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// How many attempts the named job gets, or `None` if it is not declared.
    fn max_attempts_for(&self, name: &str) -> Option<u32> {
        self.decl
            .as_ref()
            .and_then(|decl| decl.get(name))
            .map(|job| job.max_attempts)
    }
}

/// Write one `enqueue()` intent as a queue row.
///
/// Shared by the action path and the job runner so a queued row has one shape
/// and one validation, whichever body asked for it.
///
/// # Errors
/// The name is not declared, the delay is unreadable, or the write is refused.
pub(crate) async fn write_enqueue(
    db: &dyn DataSubstrate,
    handle: &JobHandle,
    intent: &dom_render_compiler::jobs::EnqueueIntent,
    principal: Option<String>,
    now_ms: i64,
) -> Result<(), String> {
    // Refused rather than queued: a row naming a job that does not exist could
    // only ever dead-letter, and the author should hear about the typo from the
    // thing that made it, not from a failed row an hour later.
    let Some(max_attempts) = handle.max_attempts_for(&intent.name) else {
        return Err(format!(
            "enqueue('{}') names a job that src/jobs.ts does not declare",
            intent.name
        ));
    };

    let delay_ms = match intent.delay.as_deref() {
        None => 0,
        Some(written) => parse_delay_ms(written).ok_or_else(|| {
            format!(
                "enqueue('{}') has an unreadable delay `{written}`; write a number and a unit, \
                 like \"30s\" or \"5m\"",
                intent.name
            )
        })?,
    };

    let request = Enqueue {
        // An id makes the enqueue idempotent. Without one a random id is minted,
        // so every call lands — the honest default: two calls with nothing to
        // tell them apart are two pieces of work.
        id: intent
            .id
            .clone()
            .unwrap_or_else(|| format!("{}:{:016x}", intent.name, rand::random::<u64>())),
        name: intent.name.clone(),
        args: intent.args.to_string(),
        principal,
        run_at_ms: now_ms.saturating_add(delay_ms),
        max_attempts,
        fanout: false,
    };
    queue::enqueue(db, &request)
        .await
        .map(|_| ())
        .map_err(|err| format!("enqueue('{}') failed: {err}", intent.name))
}

/// `"30s"`, `"5m"`, `"2h"` — an `enqueue()` delay, in milliseconds.
///
/// Deliberately the same spellings [`dom_render_compiler::jobs::schedule`]
/// accepts for `every N`, so an author who learned one has learned the other.
fn parse_delay_ms(value: &str) -> Option<i64> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let number: i64 = number.parse().ok()?;
    if number < 0 {
        return None;
    }
    let scale = match unit.trim() {
        "ms" => 1,
        "s" | "sec" | "secs" | "second" | "seconds" => 1_000,
        "m" | "min" | "mins" | "minute" | "minutes" => 60_000,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        "d" | "day" | "days" => 86_400_000,
        _ => return None,
    };
    number.checked_mul(scale)
}

/// One thing that fires on its own — an app's declaration or the framework's.
///
/// The two are unified here on purpose. A built-in is not a special case with
/// its own timer: it is a row in the same table under the same lease, retries
/// and dead-lettering, so an operator reading `albedo_jobs` sees the framework's
/// work beside their own rather than having to know about a second mechanism.
struct Scheduled {
    name: String,
    schedule: dom_render_compiler::jobs::Schedule,
    max_attempts: u32,
    fanout: bool,
}

/// Everything the loop needs, so the spawned task owns one value.
pub(crate) struct JobRunner {
    /// `None` for an app with no `src/jobs.ts`. The built-ins still run.
    plan: Option<JobsPlan>,
    db: Arc<dyn DataSubstrate>,
    /// Reached for the write path's broadcast registry, FORGE schema and current
    /// row projector — the same three the action adapter reaches through, so a
    /// job's `append` and an action's `append` go through one implementation.
    live: crate::server::LiveRuntime,
    wake: Arc<Notify>,
    /// The runner's own copy of the handle, so it enqueues through the same
    /// writer and the same validation an action does.
    handle: JobHandle,
    scheduled: Vec<Scheduled>,
    /// The next instant each entry in `scheduled` fires, aligned by index.
    next_fire: Vec<Option<i64>>,
    passes: u32,
}

impl JobRunner {
    /// Build a runner and the handle the server keeps.
    pub(crate) fn new(
        plan: Option<JobsPlan>,
        db: Arc<dyn DataSubstrate>,
        live: crate::server::LiveRuntime,
        now_ms: i64,
    ) -> (Self, JobHandle) {
        let mut scheduled: Vec<Scheduled> = Vec::new();

        // The framework's own first, so a `SELECT … ORDER BY run_at` tie between
        // a built-in and an app job resolves the same way on every process.
        for builtin in Builtin::ALL {
            scheduled.push(Scheduled {
                name: builtin.name().to_string(),
                schedule: builtin.schedule(),
                max_attempts: builtin.max_attempts(),
                fanout: false,
            });
        }
        if let Some(plan) = plan.as_ref() {
            for job in plan.decl.scheduled() {
                scheduled.push(Scheduled {
                    name: job.name.clone(),
                    schedule: job
                        .schedule
                        .clone()
                        .expect("`scheduled()` yields only jobs with a schedule"),
                    max_attempts: job.max_attempts,
                    fanout: job.fan_out == Some(FanOut::Users),
                });
            }
        }

        let next_fire = scheduled
            .iter()
            .map(|entry| entry.schedule.next_after(now_ms))
            .collect();
        let wake = Arc::new(Notify::new());
        let handle = JobHandle {
            wake: Arc::clone(&wake),
            decl: plan.as_ref().map(|plan| Arc::clone(&plan.decl)),
        };
        (
            Self {
                plan,
                db,
                live,
                handle: handle.clone(),
                wake,
                scheduled,
                next_fire,
                passes: 0,
            },
            handle,
        )
    }

    /// Run until `shutdown` is cancelled.
    ///
    /// Cancellation is checked at the top of every pass and raced against the
    /// sleep, so a `SIGTERM` during a long sleep stops immediately rather than
    /// after it. A job already running is left to finish its pass: its row stays
    /// claimed, and if the process dies first the lease expires and another
    /// runner picks it up — which is the whole reason it is a lease.
    pub(crate) async fn run(mut self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                break;
            }
            let now = crate::auth::now_ms();
            self.materialize_due_fires(now).await;
            self.drain(now, &shutdown).await;

            self.passes = self.passes.wrapping_add(1);
            if self.passes % SWEEP_EVERY == 0 {
                if let Err(err) =
                    queue::sweep_done(self.db.as_ref(), now - DONE_RETENTION_MS).await
                {
                    tracing::warn!(error = %err, "the finished-job sweep failed");
                }
            }

            let sleep_for = self.sleep_for(crate::auth::now_ms()).await;
            tokio::select! {
                () = tokio::time::sleep(sleep_for) => {}
                () = self.wake.notified() => {}
                // Raced against the sleep, not checked after it: a SIGTERM
                // during a nightly job's overnight sleep must stop the runner
                // now, or the drain deadline burns waiting for a task that is
                // parked until 03:00.
                _ = shutdown.changed() => break,
            }
        }
        tracing::debug!("the job runner stopped");
    }

    /// Enqueue every scheduled fire that has come due, and advance its instant.
    async fn materialize_due_fires(&mut self, now_ms: i64) {
        for index in 0..self.scheduled.len() {
            let Some(fire_at) = self.next_fire[index] else {
                continue;
            };
            if fire_at > now_ms {
                continue;
            }
            let entry = &self.scheduled[index];
            let request = Enqueue {
                id: queue::fire_id(&entry.name, fire_at),
                name: entry.name.clone(),
                args: "{}".to_string(),
                principal: None,
                run_at_ms: fire_at,
                max_attempts: entry.max_attempts,
                fanout: entry.fanout,
            };
            match queue::enqueue(self.db.as_ref(), &request).await {
                // `false` is the normal outcome on a second process, not an
                // error: the other one got there first and the work is queued
                // exactly once either way.
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(job = %entry.name, error = %err, "a scheduled fire could not be queued");
                }
            }
            // Advance regardless. A fire that failed to queue is not retried by
            // re-deriving it: the next instant is the next instant, and a
            // scheduler that retried a past tick would emit it late and out of
            // order. The row is what remembers work, not this.
            self.next_fire[index] = self.scheduled[index].schedule.next_after(fire_at);
        }
    }

    /// Claim and run everything currently due, then return.
    async fn drain(&mut self, now_ms: i64, shutdown: &tokio::sync::watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            let claimed = match queue::claim_due(self.db.as_ref(), now_ms, DEFAULT_LEASE_MS).await {
                Ok(Some(job)) => job,
                Ok(None) => return,
                Err(err) => {
                    tracing::warn!(error = %err, "a job could not be claimed");
                    return;
                }
            };

            if claimed.is_fanout {
                self.expand(&claimed, now_ms).await;
                continue;
            }
            self.execute(&claimed, now_ms).await;
        }
    }

    /// Expand one page of a fan-out row.
    async fn expand(&mut self, row: &ClaimedJob, now_ms: i64) {
        let Some(decl) = self
            .plan
            .as_ref()
            .and_then(|plan| plan.decl.get(&row.name))
            .cloned()
        else {
            // The job was deleted from `src/jobs.ts` while its fire was queued.
            self.record_terminal(row, now_ms, "no longer declared in src/jobs.ts")
                .await;
            return;
        };

        let page = match dom_render_compiler::auth::store::principals_after(
            self.db.as_ref(),
            row.cursor.as_deref(),
            FANOUT_BATCH,
        )
        .await
        {
            Ok(page) => page,
            Err(err) => {
                self.record_failure(row, now_ms, &format!("could not read principals: {err}"))
                    .await;
                return;
            }
        };

        let exhausted = page.len() < FANOUT_BATCH;
        let last = page.last().cloned();

        for principal in page {
            let child = Enqueue {
                // Derived from the fire and the principal, so a restart
                // mid-expansion re-enqueues the same ids and inserts nothing
                // twice.
                id: queue::fanout_child_id(&row.id, &principal),
                name: decl.name.clone(),
                args: "{}".to_string(),
                principal: Some(principal),
                run_at_ms: now_ms,
                max_attempts: decl.max_attempts,
                fanout: false,
            };
            if let Err(err) = queue::enqueue(self.db.as_ref(), &child).await {
                tracing::warn!(job = %decl.name, error = %err, "a fan-out child could not be queued");
            }
        }

        let next_cursor = if exhausted { None } else { last };
        if let Err(err) =
            queue::advance_fanout(self.db.as_ref(), &row.id, next_cursor.as_deref(), now_ms).await
        {
            tracing::warn!(job = %row.name, error = %err, "a fan-out could not be advanced");
        }
    }

    /// Run one job body and record what happened.
    ///
    /// The name alone decides where it goes: a built-in runs in Rust, anything
    /// else on the engine. No lookup that could disagree with itself, because
    /// the two name spaces cannot overlap (see `jobs::builtin`).
    async fn execute(&mut self, row: &ClaimedJob, now_ms: i64) {
        if let Some(builtin) = Builtin::from_name(&row.name) {
            match self.run_builtin(builtin).await {
                Ok(()) => self.record_success(row, now_ms).await,
                Err(message) => self.record_failure(row, now_ms, &message).await,
            }
            return;
        }
        // A name under the framework's prefix that is not a built-in is a row
        // left behind by an older version of this binary. It can never run, so
        // it is a dead letter immediately rather than after N retries of the
        // same impossibility.
        if Builtin::is_builtin(&row.name) {
            self.record_terminal(row, now_ms, "no longer a built-in job in this version")
                .await;
            return;
        }

        let Some(plan) = self.plan.as_ref() else {
            self.record_terminal(row, now_ms, "this app declares no src/jobs.ts")
                .await;
            return;
        };
        let Some(decl) = plan.decl.get(&row.name) else {
            self.record_terminal(row, now_ms, "no longer declared in src/jobs.ts")
                .await;
            return;
        };
        let timeout = decl.timeout;

        let effects = match plan.run(row, timeout).await {
            Ok(effects) => effects,
            Err(message) => {
                self.record_failure(row, now_ms, &message).await;
                return;
            }
        };

        // 🔑 Applied BEFORE the row is marked done, and a failure here fails the
        // job. The alternative — complete first, then write — would record a run
        // as successful whose whole purpose was a write that did not land, and
        // the retry that should have happened never would.
        if let Err(message) = self.apply_effects(row, effects, now_ms).await {
            self.record_failure(row, now_ms, &message).await;
            return;
        }

        if let Err(err) = queue::complete(self.db.as_ref(), &row.id, now_ms).await {
            // The body ran and its writes landed. Failing to record that is bad
            // — the lease expires and it runs again — so it is loud.
            tracing::error!(
                job = %row.name, id = %row.id, error = %err,
                "a job ran but its completion could not be recorded; it will run again when \
                 its lease expires"
            );
        }
    }

    /// Apply what a body recorded: durable writes, then new queue rows.
    ///
    /// Writes first, and in one `apply_writes` call: a body that appends a row
    /// and enqueues a job to process it must not have the job queued before the
    /// row exists, or the job runs against data that is not there yet.
    async fn apply_effects(
        &self,
        row: &ClaimedJob,
        effects: Vec<dom_render_compiler::runtime::bridge::HandlerEffect>,
        now_ms: i64,
    ) -> Result<(), String> {
        use dom_render_compiler::forge::ForgeWrite;
        use dom_render_compiler::runtime::bridge::HandlerEffect;

        if effects.is_empty() {
            return Ok(());
        }

        let mut writes: Vec<ForgeWrite> = Vec::new();
        let mut enqueues: Vec<dom_render_compiler::jobs::EnqueueIntent> = Vec::new();

        for effect in effects {
            match effect {
                HandlerEffect::ForgeAppend { collection, record } => {
                    writes.push(ForgeWrite::Append { collection, record });
                }
                HandlerEffect::ForgeUpdate {
                    collection,
                    key,
                    fields,
                } => writes.push(ForgeWrite::Update {
                    collection,
                    key,
                    fields,
                }),
                HandlerEffect::ForgeDelete { collection, key } => {
                    writes.push(ForgeWrite::Delete { collection, key });
                }
                HandlerEffect::Enqueue {
                    name,
                    args,
                    id,
                    delay,
                } => enqueues.push(dom_render_compiler::jobs::EnqueueIntent {
                    name,
                    args,
                    id,
                    delay,
                }),
                // A job body has no session and no slots, so there is nothing
                // for these to write to. The builtins that produce them are not
                // bound on this path, so reaching here means the envelope was
                // forged — say so rather than ignoring it.
                HandlerEffect::SlotSet { .. } | HandlerEffect::Broadcast { .. } => {
                    return Err(
                        "recorded a slot effect, which a job body has no way to produce"
                            .to_string(),
                    )
                }
            }
        }

        if !writes.is_empty() {
            // AUTH · the principal this run is FOR. An identity-partitioned
            // collection derives the row's owner from exactly this, so an
            // anonymous scheduled run is refused the write the same way an
            // anonymous request is. There is no system identity to fall back to.
            let principal = match row.principal.as_deref() {
                None => None,
                Some(raw) => Some(
                    dom_render_compiler::auth::PrincipalId::parse(raw)
                        .map_err(|err| format!("the row's principal is unusable: {err}"))?,
                ),
            };
            let projector = self.live.projector();
            dom_render_compiler::forge::apply_writes(
                self.db.as_ref(),
                self.live.broadcast.as_ref(),
                self.live.forge_schema.as_ref(),
                &writes,
                projector.as_deref(),
                principal.as_ref(),
            )
            .await
            .map_err(|err| format!("FORGE write failed: {err}"))?;
        }

        for intent in enqueues {
            // 🔑 The identity rule, in one line: work enqueued by a run inherits
            // that run's principal. A job enqueued by alice's fan-out run stays
            // alice's; an anonymous run's work stays anonymous. Nothing widens.
            write_enqueue(
                self.db.as_ref(),
                &self.handle,
                &intent,
                row.principal.clone(),
                now_ms,
            )
            .await?;
        }
        Ok(())
    }

    /// The framework's own work, in Rust.
    async fn run_builtin(&self, builtin: Builtin) -> Result<(), String> {
        match builtin {
            Builtin::ExpireSessions => {
                // 🔑 The call this whole item exists to make. Until 15.5 this
                // function's only caller in the repository was a test, so every
                // deployed app accumulated expired session rows forever.
                let purged = dom_render_compiler::auth::store::purge_expired_sessions(
                    self.db.as_ref(),
                    crate::auth::now_ms(),
                )
                .await
                .map_err(|err| err.to_string())?;
                if purged > 0 {
                    tracing::info!(rows = purged, "expired sessions swept");
                }
                Ok(())
            }
        }
    }

    async fn record_success(&self, row: &ClaimedJob, now_ms: i64) {
        if let Err(err) = queue::complete(self.db.as_ref(), &row.id, now_ms).await {
            tracing::error!(
                job = %row.name, id = %row.id, error = %err,
                "a job ran but its completion could not be recorded; it will run again when \
                 its lease expires"
            );
        }
    }

    /// Record a failure that retrying cannot fix.
    ///
    /// Retrying an undeclared job is pure cost: the declaration will not come
    /// back within this process's life, so N attempts produce N identical
    /// failures and delay the dead letter an operator needs to see.
    async fn record_terminal(&self, row: &ClaimedJob, now_ms: i64, message: &str) {
        let exhausted = ClaimedJob {
            attempts: row.max_attempts,
            ..row.clone()
        };
        self.record_failure(&exhausted, now_ms, message).await;
    }

    async fn record_failure(&self, row: &ClaimedJob, now_ms: i64, message: &str) {
        // Full jitter in [0.5, 1.0]: enough to break a synchronised herd of
        // retries against one upstream, never enough to make a backoff useless.
        let jitter = 0.5 + rand::random::<f64>() / 2.0;
        match queue::fail(self.db.as_ref(), row, now_ms, message, jitter).await {
            Ok(Some(retry_at)) => tracing::warn!(
                job = %row.name, id = %row.id, attempt = row.attempts,
                retry_in_ms = retry_at - now_ms, error = %message,
                "a job failed and will be retried"
            ),
            Ok(None) => tracing::error!(
                job = %row.name, id = %row.id, attempts = row.attempts, error = %message,
                "a job failed for the last time and is now a dead letter"
            ),
            Err(err) => tracing::error!(
                job = %row.name, error = %err,
                "a job failed and the failure could not be recorded"
            ),
        }
    }

    /// How long to sleep: until the earliest thing that could need doing.
    async fn sleep_for(&self, now_ms: i64) -> Duration {
        let next_schedule = self.next_fire.iter().flatten().copied().min();
        let next_queued = queue::next_due_after(self.db.as_ref(), now_ms)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "could not read the next due job");
                None
            });

        let earliest = match (next_schedule, next_queued) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };

        match earliest {
            None => MAX_SLEEP,
            Some(instant) => {
                let delta = instant.saturating_sub(now_ms).max(0);
                Duration::from_millis(u64::try_from(delta).unwrap_or(0)).min(MAX_SLEEP)
            }
        }
    }
}
