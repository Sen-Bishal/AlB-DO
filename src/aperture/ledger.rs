//! APERTURE · A3 — the journal, made durable.
//!
//! `journal.rs` is the log; this is where it lives when the process does not.
//! `substrate.rs`'s own docs name this work: *"crash-atomicity … crash-
//! resumability (Pillar 6's intent log) layers over it and arrives
//! separately."* This is that layer, and it is deliberately built on
//! [`DataSubstrate`]'s three primitives rather than on a declared FORGE
//! collection — a collection is a **live topic**, materialised at boot and
//! re-materialised on every write, and nothing subscribes to a workflow log.
//!
//! # 🔑 The window this exists to cover, and the obvious design that misses it
//!
//! The natural reading of *"persist the journal"* is: do what
//! `resolve_pending` already does, then write the result down. That is
//! **exactly wrong**, and it fails in the one moment durability is for.
//!
//! ```text
//!   issue request ──────────────► upstream          ← a crash HERE
//!                 ◄────────────── response
//!   append outcome ──────────────► disk
//! ```
//!
//! A crash at the arrow leaves **no record that the request was ever made**.
//! The retry replays from a shorter log, re-derives a *different* step index,
//! and issues the call again under a different idempotency key — so the upstream
//! sees a second intention. For a payment that is a double charge, arrived at by
//! adding durability.
//!
//! So the write **brackets** the call instead of following it:
//!
//! ```text
//!   record step N as UNKNOWN ────► disk
//!   issue request ───────────────► upstream         ← a crash HERE
//!                 ◄─────────────── response
//!   settle step N ───────────────► disk
//! ```
//!
//! A crash inside the bracket leaves [`StepOutcome::Unknown`] on disk — which is
//! not a gap in the log, it is a **row**, and it is precisely the row APERTURE
//! § 11 R4 was designed around. The replay finds it, keeps the step index, and
//! reissues under the *same* derived key, so the retry is a retry.
//!
//! 🔑 **`Unknown` was already in the type before this module existed.** A2 wrote
//! it for a request whose response never arrived in-process. Durability did not
//! add a state; it added the second way to reach one.
//!
//! # 📏 What it costs, and the asymmetry that decides what to optimise
//!
//! Measured (release, dev rig, ephemeral libSQL): **0.504 ms** for the bracket
//! per outbound call, **0.162 ms** to load a 200-step log. The thing being
//! bracketed is a network round trip — tens to hundreds of milliseconds, and
//! allowed 30 s by `DEFAULT_WORKFLOW_DEADLINE` — so this is a fraction of a
//! percent of what it protects.
//!
//! 🔑 **The two halves are not equally load-bearing, and that is what makes the
//! obvious optimisation the wrong one.** [`JournalLedger::begin_step`] must be
//! durable before the call leaves, or the window this module exists to cover
//! reopens. [`JournalLedger::settle_step`] need not be: losing it means a
//! completed call is re-issued on retry **under the same derived key**, which
//! the upstream deduplicates. So halving the cost by making the settle
//! fire-and-forget is available and is declined — it trades a guarantee this
//! server makes for one it would have to assume the upstream makes.
//!
//! ⏸️ Batching the begins into one statement is the other obvious win and is
//! **premature**: it only pays when one pass stages several calls at once, and
//! nothing does that until R1.3 hoisting lands (`TODO.md` 4.5 · A2, gated).
//! The cost test's assertion is written to fire before that becomes the common
//! case.
//!
//! # What is stored, and what deliberately is not
//!
//! Method, URL and body are covered by the step's *digest*, never stored raw,
//! and headers were already excluded from the digest itself (§ 11 R6:
//! credentials are attached by the client at send time). **A journal dump must
//! not be a credential dump**, and that property has to survive the journal
//! becoming a table someone can `SELECT` from.

use crate::aperture::journal::{Journal, StepKind, StepOutcome};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Result as SubstrateResult, SqlValue};

/// The table. Reserved-prefixed, so an app cannot declare a collection that
/// collides with it — `auth::schema::is_reserved` already guards `albedo_`.
pub const WORKFLOW_STEPS_TABLE: &str = "albedo_workflow_steps";

/// DDL, idempotent per the substrate contract.
///
/// The primary key is `(workflow, step)`, which is the log's own identity: a
/// step index is the idempotency key, so "the same step recorded twice" must be
/// unrepresentable rather than merely unlikely.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS albedo_workflow_steps (\
     workflow TEXT NOT NULL, \
     step INTEGER NOT NULL, \
     build_id TEXT NOT NULL, \
     kind TEXT NOT NULL, \
     digest TEXT NOT NULL, \
     state TEXT NOT NULL, \
     value TEXT, \
     started_at INTEGER NOT NULL, \
     settled_at INTEGER, \
     PRIMARY KEY (workflow, step))",
    // The sweep orders by age across every workflow, so it needs its own index;
    // the primary key is no help for that query.
    "CREATE INDEX IF NOT EXISTS albedo_workflow_steps_started \
     ON albedo_workflow_steps (started_at)",
];

/// `state` column values. Text rather than an integer because this table is
/// something a human will `SELECT` from at 3am.
const STATE_UNKNOWN: &str = "unknown";
const STATE_COMPLETED: &str = "completed";
const STATE_FAILED: &str = "failed";

/// How long a finished workflow's rows are worth keeping.
///
/// The log exists so a **retry** can find it, so the window is "how long is a
/// retry of this intention still plausible?" — a resubmit happens in seconds, a
/// user reopening a tab and hitting send happens in minutes. A day is generous
/// by orders of magnitude and costs a few rows.
///
/// It is not "how long might a workflow run": that is bounded far tighter by
/// [`crate::aperture::DEFAULT_WORKFLOW_DEADLINE`] (30 s), so no sweep at this
/// scale can ever delete a log out from under a live workflow.
pub const DEFAULT_RETENTION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// How often the sweep runs.
///
/// Hourly against a day of retention: frequent enough that the table tracks
/// reality, rare enough to be invisible.
pub const DEFAULT_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// A workflow log that survives the process.
///
/// Holds no state of its own: the substrate is the state, and this is the
/// spelling of the four operations the driver needs.
pub struct JournalLedger<'a> {
    substrate: &'a dyn DataSubstrate,
}

impl<'a> JournalLedger<'a> {
    /// Wrap a substrate. Cheap — no I/O until something is asked of it.
    #[must_use]
    pub fn new(substrate: &'a dyn DataSubstrate) -> Self {
        Self { substrate }
    }

    /// Create the table if it is not there.
    ///
    /// # Errors
    /// Propagates the substrate's migration error.
    pub async fn migrate(&self) -> SubstrateResult<()> {
        for ddl in DDL.iter().chain(RESULT_DDL.iter()) {
            self.substrate.migrate(ddl).await?;
        }
        Ok(())
    }

    /// Record that step `index` is **about to be attempted**.
    ///
    /// Called before the request leaves. See the module docs for why the order
    /// is the whole point.
    ///
    /// `INSERT OR IGNORE`, not `INSERT`: a resumed workflow re-attempts a step
    /// that is already on disk as `unknown`, and that is the normal path rather
    /// than a conflict. The row that is already there is the one that matters —
    /// it carries the original `started_at`.
    ///
    /// # Errors
    /// Propagates the substrate's write error.
    pub async fn begin_step(
        &self,
        workflow: &str,
        build_id: &str,
        index: u32,
        kind: StepKind,
        digest: &str,
        now_ms: i64,
    ) -> SubstrateResult<()> {
        self.substrate
            .execute(
                "INSERT OR IGNORE INTO albedo_workflow_steps \
                 (workflow, step, build_id, kind, digest, state, value, started_at, settled_at) \
                 VALUES (?, ?, ?, ?, ?, ?, NULL, ?, NULL)",
                &[
                    SqlValue::Text(workflow.to_string()),
                    SqlValue::Integer(i64::from(index)),
                    SqlValue::Text(build_id.to_string()),
                    SqlValue::Text(kind.as_str().to_string()),
                    SqlValue::Text(digest.to_string()),
                    SqlValue::Text(STATE_UNKNOWN.to_string()),
                    SqlValue::Integer(now_ms),
                ],
            )
            .await
            .map(|_| ())
    }

    /// Record how step `index` ended.
    ///
    /// 🔑 **Only from `unknown`.** The `WHERE state = 'unknown'` clause is not
    /// defensive tidiness: a settled step is an answer the body has already been
    /// given on a previous pass, and overwriting it would change history under a
    /// replay that has already read it. A late-arriving second settle is a
    /// no-op, which is the correct outcome and not an error.
    ///
    /// # Errors
    /// Propagates the substrate's write error.
    pub async fn settle_step(
        &self,
        workflow: &str,
        index: u32,
        outcome: &StepOutcome,
        now_ms: i64,
    ) -> SubstrateResult<()> {
        let (state, value) = match outcome {
            StepOutcome::Completed(value) => (
                STATE_COMPLETED,
                SqlValue::Text(serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())),
            ),
            StepOutcome::Failed(message) => (STATE_FAILED, SqlValue::Text(message.clone())),
            // Nothing to settle: the row already says this, and writing it again
            // would only move `settled_at` onto a step that is not settled.
            StepOutcome::Unknown => return Ok(()),
        };

        self.substrate
            .execute(
                "UPDATE albedo_workflow_steps SET state = ?, value = ?, settled_at = ? \
                 WHERE workflow = ? AND step = ? AND state = 'unknown'",
                &[
                    SqlValue::Text(state.to_string()),
                    value,
                    SqlValue::Integer(now_ms),
                    SqlValue::Text(workflow.to_string()),
                    SqlValue::Integer(i64::from(index)),
                ],
            )
            .await
            .map(|_| ())
    }

    /// Rebuild a workflow's log from disk, or `None` when it has no rows.
    ///
    /// Steps come back in index order and are appended through
    /// [`Journal::append`], so the log's own ordering invariant validates what
    /// was read — a gap on disk surfaces as [`crate::aperture::JournalError`]
    /// here rather than as a mis-keyed request later.
    ///
    /// # Errors
    /// Propagates the substrate's read error. A malformed log is returned as
    /// `Ok(None)` with the reason logged: an unreadable journal must not stop a
    /// dispatch, it must make it start over — which is safe, because every step
    /// it would replay carries its own idempotency key.
    pub async fn load(&self, workflow: &str) -> SubstrateResult<Option<Journal>> {
        let rows = self
            .substrate
            .query(
                "SELECT step, build_id, kind, digest, state, value \
                 FROM albedo_workflow_steps WHERE workflow = ? ORDER BY step",
                &[SqlValue::Text(workflow.to_string())],
            )
            .await?;

        if rows.rows.is_empty() {
            return Ok(None);
        }

        let build_id = match rows.rows[0].get(1) {
            Some(SqlValue::Text(id)) => id.clone(),
            _ => return Ok(None),
        };
        let mut journal = Journal::new(workflow, build_id);

        for row in &rows.rows {
            let index = match row.get(0) {
                Some(SqlValue::Integer(index)) => u32::try_from(*index).unwrap_or(u32::MAX),
                _ => return Ok(None),
            };
            let digest = match row.get(3) {
                Some(SqlValue::Text(digest)) => digest.clone(),
                _ => return Ok(None),
            };
            let state = match row.get(4) {
                Some(SqlValue::Text(state)) => state.as_str(),
                _ => return Ok(None),
            };
            let value = match row.get(5) {
                Some(SqlValue::Text(value)) => Some(value.clone()),
                _ => None,
            };

            let outcome = match state {
                STATE_COMPLETED => StepOutcome::Completed(
                    value
                        .as_deref()
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or(serde_json::Value::Null),
                ),
                STATE_FAILED => StepOutcome::Failed(value.unwrap_or_default()),
                _ => StepOutcome::Unknown,
            };

            if journal
                .append(index, StepKind::Fetch, &digest, outcome)
                .is_err()
            {
                // A gap or a repeat on disk. Starting over is the safe answer —
                // see the doc comment.
                tracing::warn!(
                    target: "albedo.aperture.ledger",
                    workflow = %workflow,
                    step = index,
                    "workflow log is not a contiguous sequence; starting the workflow over"
                );
                return Ok(None);
            }
        }

        Ok(Some(journal))
    }

    /// Delete every step older than `cutoff_ms`, returning how many rows went.
    ///
    /// A workflow log is worth keeping only as long as a retry of that intention
    /// is plausible. Without this the table is append-only forever, which is the
    /// same unbounded-growth failure `DEFAULT_STEP_CAP` guards inside one
    /// workflow, one level up.
    ///
    /// # Errors
    /// Propagates the substrate's write error.
    pub async fn sweep(&self, cutoff_ms: i64) -> SubstrateResult<u64> {
        let steps = self
            .substrate
            .execute(
                "DELETE FROM albedo_workflow_steps WHERE started_at < ?",
                &[SqlValue::Integer(cutoff_ms)],
            )
            .await?;
        // Both tables, in one call. A sweep that cleared the steps and left the
        // results would leave the table this exists to bound growing anyway —
        // and it is the one a caller is least likely to remember, because it is
        // the one added second.
        let results = self
            .substrate
            .execute(
                "DELETE FROM albedo_workflow_results WHERE completed_at < ?",
                &[SqlValue::Integer(cutoff_ms)],
            )
            .await?;
        Ok(steps + results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::libsql::LibSqlSubstrate;
    use serde_json::json;

    /// Real SQL, in an ephemeral database — the same thing `forge::write`'s
    /// tests use. A recording double would prove the strings were sent and
    /// nothing about whether `INSERT OR IGNORE` and the `state = 'unknown'`
    /// guard mean what this module needs them to mean, which is the entire
    /// question.
    async fn substrate() -> LibSqlSubstrate {
        LibSqlSubstrate::open_ephemeral().await.expect("ephemeral db")
    }

    async fn ledger_on(substrate: &LibSqlSubstrate) -> JournalLedger<'_> {
        let ledger = JournalLedger::new(substrate);
        ledger.migrate().await.expect("migrate");
        ledger
    }

    #[tokio::test]
    async fn a_settled_step_round_trips_through_the_substrate() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        ledger
            .begin_step("w1", "build-1", 0, StepKind::Fetch, "d0", 1_000)
            .await
            .expect("begin");
        ledger
            .settle_step("w1", 0, &StepOutcome::Completed(json!({"ok": true})), 1_050)
            .await
            .expect("settle");

        let journal = ledger.load("w1").await.expect("load").expect("some");
        assert_eq!(journal.workflow_id(), "w1");
        assert_eq!(journal.build_id(), "build-1");
        assert_eq!(journal.len(), 1);
        assert_eq!(
            journal.steps()[0].outcome,
            StepOutcome::Completed(json!({"ok": true}))
        );
    }

    /// 🔑 **The property the whole module exists for.**
    ///
    /// A crash between `begin_step` and `settle_step` is simulated by simply not
    /// calling the second one — which is exactly what a killed process does.
    /// What must survive is a **row**, not a gap: the step index is the
    /// idempotency key, so a missing row would re-key every later request.
    #[tokio::test]
    async fn a_crash_inside_the_bracket_leaves_an_unknown_row_not_a_gap() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        ledger
            .begin_step("w1", "b", 0, StepKind::Fetch, "d0", 1_000)
            .await
            .expect("begin");
        ledger
            .settle_step("w1", 0, &StepOutcome::Completed(json!("first")), 1_010)
            .await
            .expect("settle");
        // …and the process dies here, mid-flight on step 1.
        ledger
            .begin_step("w1", "b", 1, StepKind::Fetch, "d1", 1_020)
            .await
            .expect("begin");

        let journal = ledger.load("w1").await.expect("load").expect("some");
        assert_eq!(
            journal.len(),
            2,
            "the in-flight step must be a row; a gap would re-key every later step"
        );
        assert_eq!(journal.steps()[1].outcome, StepOutcome::Unknown);
        assert_eq!(
            journal.idempotency_key(1),
            "w1:1",
            "and it must re-issue under the SAME key, or the retry is a second intention"
        );
    }

    /// A settled step is an answer the body has already been given. Rewriting it
    /// would change history under a replay that has read it.
    #[tokio::test]
    async fn settling_a_settled_step_is_a_no_op() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        ledger
            .begin_step("w1", "b", 0, StepKind::Fetch, "d0", 1)
            .await
            .expect("begin");
        ledger
            .settle_step("w1", 0, &StepOutcome::Completed(json!("first")), 2)
            .await
            .expect("settle");
        ledger
            .settle_step("w1", 0, &StepOutcome::Completed(json!("second")), 3)
            .await
            .expect("settle again");

        let journal = ledger.load("w1").await.expect("load").expect("some");
        assert_eq!(
            journal.steps()[0].outcome,
            StepOutcome::Completed(json!("first")),
            "the first answer stands — the body may already have seen it"
        );
    }

    /// Re-attempting a step that is already on disk is the normal resumed path,
    /// not a conflict, and it must not lose the original `started_at`.
    #[tokio::test]
    async fn beginning_a_step_that_already_exists_keeps_the_original_row() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        ledger
            .begin_step("w1", "b", 0, StepKind::Fetch, "d0", 1_000)
            .await
            .expect("begin");
        ledger
            .begin_step("w1", "b", 0, StepKind::Fetch, "d0", 9_999)
            .await
            .expect("begin again");

        assert_eq!(ledger.load("w1").await.expect("load").expect("some").len(), 1);
        // The original age is what the sweep must judge it by.
        assert_eq!(
            ledger.sweep(1_001).await.expect("sweep"),
            1,
            "the row kept its first `started_at`, so the sweep can see its true age"
        );
    }

    #[tokio::test]
    async fn an_unknown_workflow_loads_as_none() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;
        assert!(ledger.load("never-seen").await.expect("load").is_none());
    }

    #[tokio::test]
    async fn the_sweep_removes_only_what_is_older_than_the_cutoff() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        ledger
            .begin_step("old", "b", 0, StepKind::Fetch, "d", 1_000)
            .await
            .expect("begin");
        ledger
            .begin_step("new", "b", 0, StepKind::Fetch, "d", 5_000)
            .await
            .expect("begin");

        assert_eq!(ledger.sweep(2_000).await.expect("sweep"), 1);
        assert!(ledger.load("old").await.expect("load").is_none());
        assert!(ledger.load("new").await.expect("load").is_some());
    }

    /// Two workflows are two logs. Sharing a table must not share an index
    /// space — the step index is only an idempotency key *within* a workflow.
    #[tokio::test]
    async fn two_workflows_do_not_share_a_step_sequence() {
        let substrate = substrate().await;
        let ledger = ledger_on(&substrate).await;

        for workflow in ["w1", "w2"] {
            ledger
                .begin_step(workflow, "b", 0, StepKind::Fetch, "d0", 1)
                .await
                .expect("begin");
            ledger
                .settle_step(workflow, 0, &StepOutcome::Completed(json!(workflow)), 2)
                .await
                .expect("settle");
        }

        assert_eq!(
            ledger.load("w1").await.expect("load").expect("some").steps()[0].outcome,
            StepOutcome::Completed(json!("w1"))
        );
        assert_eq!(
            ledger.load("w2").await.expect("load").expect("some").steps()[0].outcome,
            StepOutcome::Completed(json!("w2"))
        );
    }
}

/// The completed-result half of the log.
///
/// # Why this is a second table and not a step
///
/// A step records a **non-deterministic act inside** a workflow. A result
/// records that the workflow **finished** and what it answered. Storing the
/// second as a magic step index would make `journal.len()` — which is the
/// idempotency key generator — depend on whether the workflow had ended, and
/// that is precisely the number that must not move.
const RESULT_DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS albedo_workflow_results (\
     workflow TEXT PRIMARY KEY, \
     build_id TEXT NOT NULL, \
     instructions BLOB NOT NULL, \
     completed_at INTEGER NOT NULL)",
    "CREATE INDEX IF NOT EXISTS albedo_workflow_results_completed \
     ON albedo_workflow_results (completed_at)",
];

/// The table holding one row per finished workflow.
pub const WORKFLOW_RESULTS_TABLE: &str = "albedo_workflow_results";

impl JournalLedger<'_> {
    /// Record that a workflow finished, and what it answered.
    ///
    /// # 🔑 Why this is written AFTER the effects, never before
    ///
    /// A completing dispatch does two durable things: it applies the body's
    /// FORGE writes, and it records this. They are not in one transaction —
    /// `apply_writes` owns its own — so one of them is second, and which one
    /// decides what a crash in between costs:
    ///
    /// * **result first** → a crash loses the writes and the retry is answered
    ///   from the log, so the data is *silently gone*;
    /// * **writes first** → a crash replays the body and re-applies them, so
    ///   the data is *duplicated*.
    ///
    /// Duplication is recoverable and visible; silent loss is neither. And the
    /// second option is not a regression, because re-applying is exactly what
    /// happens today with no result table at all.
    ///
    /// `INSERT OR IGNORE`: two racing dispatches of one intention both finish,
    /// and the first answer is the one already sent to somebody.
    ///
    /// # Errors
    /// Propagates the substrate's write error.
    pub async fn complete(
        &self,
        workflow: &str,
        build_id: &str,
        instructions: &[u8],
        now_ms: i64,
    ) -> SubstrateResult<()> {
        self.substrate
            .execute(
                "INSERT OR IGNORE INTO albedo_workflow_results \
                 (workflow, build_id, instructions, completed_at) VALUES (?, ?, ?, ?)",
                &[
                    SqlValue::Text(workflow.to_string()),
                    SqlValue::Text(build_id.to_string()),
                    SqlValue::Blob(instructions.to_vec()),
                    SqlValue::Integer(now_ms),
                ],
            )
            .await
            .map(|_| ())
    }

    /// The answer a finished workflow already gave, if it finished.
    ///
    /// `build_id` is checked here rather than by the caller: a result recorded
    /// by different code describes a different program, and replaying it would
    /// be R8's divergence arriving through the one door that skips the body
    /// entirely. A mismatch reads as "not finished", so the workflow runs again
    /// under the code that is actually deployed.
    ///
    /// # Errors
    /// Propagates the substrate's read error.
    pub async fn completed(
        &self,
        workflow: &str,
        build_id: &str,
    ) -> SubstrateResult<Option<Vec<u8>>> {
        let rows = self
            .substrate
            .query(
                "SELECT instructions FROM albedo_workflow_results \
                 WHERE workflow = ? AND build_id = ?",
                &[
                    SqlValue::Text(workflow.to_string()),
                    SqlValue::Text(build_id.to_string()),
                ],
            )
            .await?;
        Ok(rows.rows.first().and_then(|row| match row.get(0) {
            Some(SqlValue::Blob(bytes)) => Some(bytes.clone()),
            _ => None,
        }))
    }
}

#[cfg(test)]
mod result_tests {
    use super::*;
    use crate::forge::libsql::LibSqlSubstrate;

    async fn ledger() -> LibSqlSubstrate {
        LibSqlSubstrate::open_ephemeral().await.expect("db")
    }

    /// 🔑 The property that turns "no duplicate charge" into "no duplicate
    /// submit": a finished workflow answers from the log instead of running.
    #[tokio::test]
    async fn a_finished_workflow_answers_from_the_log() {
        let substrate = ledger().await;
        let ledger = JournalLedger::new(&substrate);
        ledger.migrate().await.expect("migrate");

        assert!(ledger.completed("w", "b").await.expect("read").is_none());
        ledger.complete("w", "b", b"instructions", 1).await.expect("complete");
        assert_eq!(
            ledger.completed("w", "b").await.expect("read").as_deref(),
            Some(&b"instructions"[..])
        );
    }

    /// 🔑 A result recorded by different code describes a different program.
    /// Replaying it would be R8's divergence through the one door that skips the
    /// body entirely, so a build mismatch reads as *not finished*.
    #[tokio::test]
    async fn a_result_from_another_build_is_not_answered() {
        let substrate = ledger().await;
        let ledger = JournalLedger::new(&substrate);
        ledger.migrate().await.expect("migrate");
        ledger.complete("w", "old-build", b"x", 1).await.expect("complete");

        assert!(
            ledger.completed("w", "new-build").await.expect("read").is_none(),
            "the workflow must run again under the code that is actually deployed"
        );
    }

    /// Two racing dispatches of one intention both finish. The first answer is
    /// the one already sent to somebody, so it is the one that stands.
    #[tokio::test]
    async fn the_first_recorded_result_wins() {
        let substrate = ledger().await;
        let ledger = JournalLedger::new(&substrate);
        ledger.migrate().await.expect("migrate");

        ledger.complete("w", "b", b"first", 1).await.expect("complete");
        ledger.complete("w", "b", b"second", 2).await.expect("complete again");
        assert_eq!(
            ledger.completed("w", "b").await.expect("read").as_deref(),
            Some(&b"first"[..])
        );
    }

    /// 🔴 The sweep must clear BOTH tables. Clearing steps and leaving results
    /// would leave the table this exists to bound growing anyway — and it is the
    /// one a caller is least likely to remember, because it was added second.
    #[tokio::test]
    async fn the_sweep_clears_results_as_well_as_steps() {
        let substrate = ledger().await;
        let ledger = JournalLedger::new(&substrate);
        ledger.migrate().await.expect("migrate");

        ledger
            .begin_step("w", "b", 0, StepKind::Fetch, "d", 1_000)
            .await
            .expect("begin");
        ledger.complete("w", "b", b"x", 1_000).await.expect("complete");

        assert_eq!(
            ledger.sweep(2_000).await.expect("sweep"),
            2,
            "one step row and one result row"
        );
        assert!(ledger.load("w").await.expect("load").is_none());
        assert!(ledger.completed("w", "b").await.expect("read").is_none());
    }
}

/// What durability costs per outbound call.
///
/// ```text
/// cargo test --release --features forge --lib -- --ignored --nocapture ledger_cost
/// ```
#[cfg(test)]
mod ledger_cost {
    use super::*;
    use crate::forge::libsql::LibSqlSubstrate;
    use serde_json::json;
    use std::time::Instant;

    const REPS: usize = 200;

    #[tokio::test]
    #[ignore = "timing; run explicitly in release"]
    async fn the_bracket_costs_two_writes_per_call() {
        let substrate = LibSqlSubstrate::open_ephemeral().await.expect("db");
        let ledger = JournalLedger::new(&substrate);
        ledger.migrate().await.expect("migrate");

        // Warm the connection and the page cache before timing anything.
        for i in 0..20u32 {
            ledger
                .begin_step("warm", "b", i, StepKind::Fetch, "d", 1)
                .await
                .expect("begin");
            ledger
                .settle_step("warm", i, &StepOutcome::Completed(json!("x")), 2)
                .await
                .expect("settle");
        }

        let started = Instant::now();
        for i in 0..REPS as u32 {
            ledger
                .begin_step("timed", "b", i, StepKind::Fetch, "d", 1)
                .await
                .expect("begin");
            ledger
                .settle_step("timed", i, &StepOutcome::Completed(json!({"ok": true})), 2)
                .await
                .expect("settle");
        }
        let per_call = started.elapsed().as_secs_f64() * 1000.0 / REPS as f64;

        // The other half: what a resumed dispatch pays before it runs anything.
        let load_started = Instant::now();
        for _ in 0..REPS {
            ledger.load("timed").await.expect("load");
        }
        let per_load = load_started.elapsed().as_secs_f64() * 1000.0 / REPS as f64;

        println!("\n=== APERTURE A3 · what durability costs ===\n");
        println!("  bracket (begin + settle) per outbound call : {per_call:.3} ms");
        println!("  load a {REPS}-step log                       : {per_load:.3} ms");
        println!(
            "\n  For scale: the call being bracketed is a NETWORK round trip, and\n  \
             `DEFAULT_WORKFLOW_DEADLINE` allows it 30 s.\n"
        );

        // 🔑 The gate is on the ratio to what it protects, not on a machine
        // number. Durability that costs a meaningful fraction of the network
        // call it brackets would be a bad trade; at a few percent of even a
        // 10 ms upstream it is not a trade at all.
        assert!(
            per_call < 5.0,
            "🔴 the bracket costs {per_call:.3} ms per call, which is no longer noise \
             against a fast upstream — batch the begins into one statement before \
             R1.3 hoisting makes that the common case"
        );
    }
}
