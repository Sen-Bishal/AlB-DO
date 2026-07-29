//! APERTURE · **gate 5c — the pool of contexts.**
//!
//! Gate 5b ended on a red row. Lowering an action body to a generator and
//! suspending it deletes the replay tax (one body run instead of two, and with
//! it R1's N+1, R2's determinism constraint and R3's sentinel machinery) — but
//! the paused generator lives in a specific engine's heap, so the workflow
//! **pins the engine it started on**. Measured against today's
//! [`QuickJsEnginePool`] that came out at `peak 2 / 404.5 ms` on a pool of 2:
//! byte for byte the blocking host function gate 5 exists to rule out.
//!
//! `APERTURE-CONTINUATIONS.md` § 2 named the fix and priced it as a guess: make
//! the pool a pool of **contexts** rather than engines, because a QuickJS
//! `Runtime` can host many `Context`s and a paused generator only needs its own
//! context, not its own runtime. This gate is the measurement that guess was
//! standing in for, and it is the decision point for the whole direction:
//!
//! > **Gate 5b's third row must go from `peak 2` to `peak 16`. If a context is
//! > not meaningfully cheaper than a runtime, keep replay and close the thread.**
//!
//! Three sections, in the order that can falsify the design fastest:
//!
//! - **§ 1 — does it even work?** A generator paused in one context, with fifteen other contexts
//!   running JS in between, resumed with a value. If a paused generator cannot survive that, the
//!   price does not matter.
//! - **§ 2 — the price.** A context on a live runtime versus a whole engine, which is what gate
//!   5b.2 could only bound from above.
//! - **§ 3 — the occupancy row.** The same shape as gate 5/5b so the numbers line up, with the pool
//!   made of contexts.
//!
//! ```text
//! cargo test -p albedo-server --test aperture_gate5c_context_pool -- --ignored --nocapture
//! ```

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use rquickjs::{Context, Runtime};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, Semaphore};

/// Same shape as gates 5 and 5b, so the rows are comparable line for line.
const ACTIONS: usize = 16;
const RTT: Duration = Duration::from_millis(50);
/// Gate 5b's pool size, kept for the "engines cost a thread each" comparison.
const ENGINE_POOL_SIZE: usize = 2;

// ── The pool ────────────────────────────────────────────────────────────────

/// A job to run against one context, on the runtime's own thread.
type Job = Box<dyn FnOnce(&Context) + Send>;

/// One QuickJS runtime, many contexts, all on a single thread.
///
/// The thread is not incidental. `rquickjs::Runtime` is not `Sync` and QuickJS
/// serialises execution per runtime anyway, so the honest model is: **the
/// runtime is a thread, and contexts are the things a workflow holds.** That is
/// precisely the split the design needs — a suspended workflow keeps its
/// context (where its paused generator lives) and gives back the thread.
struct ContextPool {
    jobs: mpsc::Sender<(usize, Job)>,
    idle: Mutex<Vec<usize>>,
    permits: Semaphore,
    size: usize,
}

impl ContextPool {
    fn with_size(size: usize) -> Self {
        let (tx, rx) = mpsc::channel::<(usize, Job)>();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        thread::Builder::new()
            .name("albedo-qjs-context-pool".to_string())
            .spawn(move || {
                let runtime = Runtime::new().expect("runtime");
                let contexts: Vec<Context> = (0..size)
                    .map(|_| Context::full(&runtime).expect("context"))
                    .collect();
                ready_tx.send(()).ok();
                while let Ok((index, job)) = rx.recv() {
                    job(&contexts[index]);
                }
            })
            .expect("spawn");

        ready_rx.recv().expect("pool ready");
        Self {
            jobs: tx,
            idle: Mutex::new((0..size).collect()),
            permits: Semaphore::new(size),
            size,
        }
    }

    /// Take a context out of the pool. It stays out until the guard is returned
    /// — which is the entire point: a suspended workflow holds this across its
    /// round trip while the runtime thread serves everyone else.
    async fn checkout(&self) -> Checkout<'_> {
        let permit = self
            .permits
            .acquire()
            .await
            .expect("pool open")
            .forget();
        let _ = permit;
        let index = self.idle.lock().expect("idle").pop().expect("permit implies idle");
        Checkout { pool: self, index }
    }
}

struct Checkout<'a> {
    pool: &'a ContextPool,
    index: usize,
}

impl Checkout<'_> {
    /// Run one piece of JS in this context, on the runtime thread, and wait for
    /// it. The thread is busy only for the duration of the call.
    async fn run<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&Context) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = oneshot::channel::<R>();
        let job: Job = Box::new(move |ctx| {
            let _ = tx.send(f(ctx));
        });
        self.pool.jobs.send((self.index, job)).expect("worker alive");
        rx.await.expect("job ran")
    }
}

impl Drop for Checkout<'_> {
    fn drop(&mut self) {
        self.pool.idle.lock().expect("idle").push(self.index);
        self.pool.permits.add_permits(1);
    }
}

// ── Counters, identical to gate 5b's ────────────────────────────────────────

#[derive(Debug, Default)]
struct Occupancy {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    body_runs: AtomicUsize,
}

impl Occupancy {
    fn enter_call(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::SeqCst);
    }
    fn leave_call(&self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
    fn run_body(&self) {
        self.body_runs.fetch_add(1, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }
    fn runs(&self) -> usize {
        self.body_runs.load(Ordering::SeqCst)
    }
}

/// The workflow body, as the `async_to_generator` lowering would leave it: it
/// yields its request and resumes with the answer.
const BODY: &str = r#"
globalThis.__start = function () {
  globalThis.__gen = (function* () {
    const answer = yield 'GET /charge';
    return 'charged:' + answer;
  })();
  return String(globalThis.__gen.next().value);
};
globalThis.__resume = function (value) {
  const step = globalThis.__gen.next(value);
  return String(step.value) + '|' + String(step.done);
};
"#;

// ── § 1 · does a paused generator survive its neighbours? ───────────────────

/// The falsifying test, run first and deliberately hostile: pause a generator in
/// one context, then execute JS in **every other context on the same runtime**,
/// including one that defines its own `__gen`, and only then resume.
///
/// If contexts did not isolate globals, or if a paused generator did not survive
/// other contexts running, the whole direction dies here and no timing matters.
#[test]
#[ignore = "gate: run explicitly"]
fn gate_5c_1_a_paused_generator_survives_other_contexts_running() {
    let runtime = Runtime::new().expect("runtime");
    let contexts: Vec<Context> = (0..ACTIONS)
        .map(|_| Context::full(&runtime).expect("context"))
        .collect();

    // Pause a generator in context 0.
    let asked: String = contexts[0].with(|ctx| {
        ctx.eval::<(), _>(BODY).expect("body evals");
        ctx.eval::<String, _>("__start()").expect("start")
    });
    assert_eq!(asked, "GET /charge", "the body yielded its request");

    // Now run JS in every other context, each defining its own `__gen`, which
    // would clobber context 0's if globals were shared.
    for context in contexts.iter().skip(1) {
        let out: String = context.with(|ctx| {
            ctx.eval::<(), _>(BODY).expect("body evals");
            ctx.eval::<String, _>("__start()").expect("start")
        });
        assert_eq!(out, "GET /charge");
    }

    // And resume the first one, which has been parked the whole time.
    let finished: String = contexts[0].with(|ctx| {
        ctx.eval::<String, _>("__resume('ch_1')")
            .expect("resume")
    });
    assert_eq!(
        finished, "charged:ch_1|true",
        "the paused body resumed from where it stopped, with the value it was given"
    );

    println!(
        "\ngate 5c.1 — a generator paused in context 0 survived {} other contexts \
         running on the same runtime, then resumed correctly.\n",
        ACTIONS - 1
    );
}

// ── § 2 · what a context costs ──────────────────────────────────────────────

/// Gate 5b.2 measured a whole engine (~949 µs) and called it an upper bound on
/// the context price, assuming a context is much cheaper than a runtime.
/// **This measures that assumption, and it does not survive.**
///
/// The decisive observation is arithmetic, not a benchmark: **globals are
/// per-context.** `h`, the hooks, the form contract and the runtime helpers all
/// live on the global object, so a context is not a unit the pool can hand out
/// until the bootstrap has been evaluated *into it* — and that cost is paid per
/// context under either design. What context-per-workflow actually saves is
/// therefore exactly one term, `Runtime::new()`, and nothing else:
///
/// ```text
///   engine per workflow   = Runtime::new + Context::full + bootstrap
///   context per workflow  =               Context::full + bootstrap
///   saving                = Runtime::new
/// ```
///
/// which is measurable directly, needing no access to the private bootstrap
/// source and no proxy script to argue about.
#[test]
#[ignore = "gate: run explicitly"]
fn gate_5c_2_what_one_more_context_costs() {
    // A runtime on its own — the entire saving, isolated.
    let runtime_started = Instant::now();
    let mut bare_runtimes = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        bare_runtimes.push(Runtime::new().expect("runtime"));
    }
    let runtime_each = runtime_started.elapsed() / ACTIONS as u32;

    // A context on a runtime that already has one.
    let runtime = Runtime::new().expect("runtime");
    // Bound (not `_`) so it stays alive: every later context is then genuinely
    // "one more on a runtime that already has one", which is the number wanted.
    let _first = Context::full(&runtime).expect("context");
    let context_started = Instant::now();
    let mut contexts = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        contexts.push(Context::full(&runtime).expect("context"));
    }
    let context_each = context_started.elapsed() / ACTIONS as u32;

    // The unit the pool hands out TODAY: a fully bootstrapped engine. The same
    // measurement gate 5b.2 makes, repeated here so both sides of the comparison
    // come off one run on one machine.
    let engine_started = Instant::now();
    let mut engines = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("init");
        engines.push(engine);
    }
    let engine_each = engine_started.elapsed() / ACTIONS as u32;

    // Derived, and labelled as derived: what evaluating the bootstrap into a
    // fresh global object costs — the term neither design escapes.
    let bootstrap_each = engine_each.saturating_sub(runtime_each + context_each);
    let saving_pct = 100.0 * runtime_each.as_secs_f64() / engine_each.as_secs_f64();

    println!(
        "\ngate 5c.2 — the price of one more IN-FLIGHT WORKFLOW\n\
         \x20 Runtime::new alone            {runtime_each:>10.2?}   <- the whole saving\n\
         \x20 Context::full on live runtime {context_each:>10.2?}\n\
         \x20 bootstrap into a context      {bootstrap_each:>10.2?}   (derived: engine - runtime - context)\n\
         \x20 ---\n\
         \x20 engine per workflow (today)   {engine_each:>10.2?}\n\
         \x20 context per workflow          {:>10.2?}\n\
         \x20 saving                        {saving_pct:>9.1}%\n",
        engine_each.saturating_sub(runtime_each),
    );

    assert_eq!(contexts.len(), ACTIONS);
    assert_eq!(engines.len(), ACTIONS);
    assert_eq!(bare_runtimes.len(), ACTIONS);
    // No invented budget. The numbers are the deliverable; the judgement about
    // them belongs in the doc, not smuggled in as an assert.
}

// ── § 3 · the occupancy row gate 5b left red ────────────────────────────────

/// Generator suspension against a pool of **contexts**, same shape as gate 5b's
/// third row.
///
/// The body runs once (the generator win) and the round trip happens with the
/// context checked out but the runtime thread **free**, which is the entire
/// difference. Compare to gate 5b: `peak 2 / 404.5 ms`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gate: ~1s of deliberate sleeping; run explicitly"]
async fn gate_5c_3_a_context_pool_lets_every_workflow_be_in_flight_at_once() {
    // Sized by CONCURRENCY, which is the thing that just became affordable.
    let pool = Arc::new(ContextPool::with_size(ACTIONS));
    let occ = Arc::new(Occupancy::default());

    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occ = Arc::clone(&occ);
        tasks.push(tokio::spawn(async move {
            let checkout = pool.checkout().await;

            // gen.next() → the body runs ONCE and yields its request.
            let counter = Arc::clone(&occ);
            let asked = checkout
                .run(move |ctx| {
                    ctx.with(|ctx| {
                        ctx.eval::<(), _>(BODY).expect("body");
                        counter.run_body();
                        ctx.eval::<String, _>("__start()").expect("start")
                    })
                })
                .await;
            assert_eq!(asked, "GET /charge");

            // The round trip. The context is held — the paused generator is in
            // it — but the runtime thread is back in service for everyone else.
            // That is the sentence the whole gate exists to make true.
            occ.enter_call();
            tokio::time::sleep(RTT).await;
            occ.leave_call();

            let finished = checkout
                .run(move |ctx| {
                    ctx.with(|ctx| ctx.eval::<String, _>("__resume('ch_1')").expect("resume"))
                })
                .await;
            assert_eq!(finished, "charged:ch_1|true");
        }));
    }
    for task in tasks {
        task.await.expect("done");
    }
    let wall = started.elapsed();

    println!(
        "\ngate 5c.3 — pool of {} CONTEXTS, {ACTIONS} concurrent actions, {RTT:?} per call\n\
         \x20 generator, context-held   peak {:>2}   body runs {:>2}   wall {:>8.1?}\n\
         \x20 (gate 5b, engine-affine:  peak {:>2}   body runs {:>2}   wall ~404.5ms)\n",
        pool.size,
        occ.peak(),
        occ.runs(),
        wall,
        ENGINE_POOL_SIZE,
        ACTIONS,
    );

    // The generator win, unchanged: no replay.
    assert_eq!(occ.runs(), ACTIONS, "one body run per action, no replay");

    // 🟢 The red row from gate 5b, turned. This is the assertion the whole
    // direction was gated on.
    assert_eq!(
        occ.peak(),
        ACTIONS,
        "every workflow must be able to be in flight at once; a context pool \
         sized to concurrency is what buys that, and gate 5b's engine-affine row \
         could not exceed {ENGINE_POOL_SIZE}"
    );

    // Wall clock follows occupancy. Asserted loosely, as gate 5 argues.
    assert!(
        wall < RTT * 4,
        "16 overlapping {RTT:?} calls must not serialise; got {wall:?}"
    );
}

// ── § 4 · the bill for the win ──────────────────────────────────────────────

/// § 3's workflows do trivial JS, so its wall clock says nothing about the cost
/// a context pool actually imposes: **one runtime executes JS on one thread.**
/// Today's pool is one engine per thread, so N engines give N-way parallelism;
/// N contexts on one runtime give one-way, whatever N is.
///
/// That matters because the same pool renders Tier-B components, which is
/// CPU-bound work, not a round trip. So this measures the trade instead of
/// assuming it: CPU-bound JS, once across a 2-context pool and once across a
/// 16-context pool, both on a single runtime. If the two are the same, the extra
/// contexts bought no throughput and the serialisation is real.
///
/// The conclusion this is here to support: the production shape cannot be one
/// runtime with many contexts. It has to be **M runtimes (≈ cores) × K contexts
/// each** — parallelism from M, in-flight suspensions from M×K.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gate: CPU-bound; run explicitly"]
async fn gate_5c_4_one_runtime_executes_js_on_one_thread() {
    // Enough arithmetic to be measurable, with no allocation to muddy it.
    const BURN: &str = "(function(){var s=0;for(var i=0;i<2000000;i++){s+=i%7;}return s;})()";

    async fn burn_across(pool: Arc<ContextPool>) -> Duration {
        let started = Instant::now();
        let mut tasks = Vec::with_capacity(ACTIONS);
        for _ in 0..ACTIONS {
            let pool = Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                let checkout = pool.checkout().await;
                checkout
                    .run(|ctx| {
                        ctx.with(|ctx| ctx.eval::<i64, _>(BURN).expect("burn"));
                    })
                    .await;
            }));
        }
        for task in tasks {
            task.await.expect("done");
        }
        started.elapsed()
    }

    let narrow = burn_across(Arc::new(ContextPool::with_size(2))).await;
    let wide = burn_across(Arc::new(ContextPool::with_size(ACTIONS))).await;

    println!(
        "\ngate 5c.4 — CPU-bound JS, {ACTIONS} jobs, one runtime\n\
         \x20  2 contexts   {narrow:>8.1?}\n\
         \x20 {ACTIONS} contexts   {wide:>8.1?}\n\
         \x20 ratio        {:>8.2}x   (1.0 = more contexts bought no throughput)\n\
         \x20 => contexts buy CONCURRENCY of suspensions, never PARALLELISM of execution.\n\
         \x20    Production shape must be M runtimes x K contexts, not 1 x N.\n",
        narrow.as_secs_f64() / wide.as_secs_f64().max(f64::MIN_POSITIVE),
    );

    // Deliberately no tight assertion on the ratio — a loaded machine must not
    // make a structural claim flaky, and the claim is structural: QuickJS has no
    // intra-runtime parallelism, so widening the pool cannot speed this up.
    // Asserted only in the direction that would falsify it.
    assert!(
        wide.as_secs_f64() > narrow.as_secs_f64() * 0.5,
        "16 contexts on one runtime must not approach 8x the throughput of 2; \
         got {narrow:?} vs {wide:?} — if this fires, QuickJS gained intra-runtime \
         parallelism and the M x K recommendation needs revisiting"
    );
}
