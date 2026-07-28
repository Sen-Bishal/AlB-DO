//! APERTURE · **gate 5b — context affinity.**
//!
//! Gate 5 settled A2's shape against a *blocking host function*. This gate asks
//! the question that only came up after A2 shipped and the engine was probed:
//!
//! > If the body is lowered to a generator and **suspended** rather than
//! > replayed, the paused state lives in the JS heap — so something must hold it
//! > across the round trip. What does that cost?
//!
//! The premise gate 5 was argued from (`APERTURE.md` § 5.4, *"the engine cannot
//! `await`"*) turned out to be false: `the_engine_drives_generators_and_resumes_them_with_a_value`
//! shows QuickJS pausing a body, handing the pause point to Rust, and resuming
//! it with a value. The real constraint is that the engine cannot **block**, and
//! those are different. Suspension without replay is therefore available, and it
//! deletes the entire replay tax — R1's N+1 body runs, R2's determinism
//! constraint, R3's sentinel and its two defences.
//!
//! **This gate exists to find the catch, and there is one.** A suspended
//! generator is an object in a specific engine's heap, so a workflow that
//! suspends **pins the engine it started on**. Against today's
//! [`QuickJsEnginePool`] — one engine per slot, checked out and back — that is
//! not merely worse than replay, it is *exactly the blocking host function
//! again*, which is what gate 5 was built to rule out.
//!
//! So the measurement below is deliberately unkind to the design I expect to
//! prefer. It runs three designs, not two, and the third is the one being
//! proposed:
//!
//! - **blocking** — the round trip happens with the engine checked out.
//! - **suspend / replay** — today's A2. Nothing held; the body runs twice.
//! - **generator suspension, engine-affine** — the body runs once, and holds its engine across the
//!   round trip because that is where its paused state lives.
//!
//! If the third comes out looking like the first, the proposal is not "lower to
//! generators" — it is "lower to generators **and** change what the pool is a
//! pool of", and the cost of that is what section 2 measures.

use albedo_server::engine_pool::QuickJsEnginePool;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Same shape as gate 5, so the numbers are comparable line for line.
const POOL_SIZE: usize = 2;
const ACTIONS: usize = 16;
const RTT: Duration = Duration::from_millis(50);

#[derive(Debug, Default)]
struct Occupancy {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    engine_visits: AtomicUsize,
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
    fn visit_engine(&self) {
        self.engine_visits.fetch_add(1, Ordering::SeqCst);
    }
    fn run_body(&self) {
        self.body_runs.fetch_add(1, Ordering::SeqCst);
    }
    fn peak(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }
    fn visits(&self) -> usize {
        self.engine_visits.load(Ordering::SeqCst)
    }
    fn runs(&self) -> usize {
        self.body_runs.load(Ordering::SeqCst)
    }
}

fn do_some_work(engine: &mut QuickJsEngine) -> bool {
    engine.is_initialized()
}

async fn blocking_design(pool: Arc<QuickJsEnginePool>, occ: Arc<Occupancy>) -> Duration {
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occ = Arc::clone(&occ);
        tasks.push(tokio::spawn(async move {
            occ.visit_engine();
            pool.with_engine(move |engine| {
                do_some_work(engine);
                occ.run_body();
                occ.enter_call();
                std::thread::sleep(RTT);
                occ.leave_call();
            })
            .await
            .expect("engine");
        }));
    }
    for task in tasks {
        task.await.expect("done");
    }
    started.elapsed()
}

async fn suspend_replay_design(pool: Arc<QuickJsEnginePool>, occ: Arc<Occupancy>) -> Duration {
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occ = Arc::clone(&occ);
        tasks.push(tokio::spawn(async move {
            occ.visit_engine();
            let first = Arc::clone(&occ);
            pool.with_engine(move |e| {
                first.run_body();
                do_some_work(e)
            })
            .await
            .expect("engine");

            occ.enter_call();
            tokio::time::sleep(RTT).await;
            occ.leave_call();

            occ.visit_engine();
            let second = Arc::clone(&occ);
            pool.with_engine(move |e| {
                second.run_body();
                do_some_work(e)
            })
            .await
            .expect("engine");
        }));
    }
    for task in tasks {
        task.await.expect("done");
    }
    started.elapsed()
}

/// **Generator suspension, engine-affine.** The body runs ONCE — the win — but
/// the paused generator lives in this engine's heap, so the engine cannot be
/// returned to the pool until the workflow finishes. The round trip therefore
/// happens inside `with_engine`, exactly as the blocking design's does.
///
/// The `await` inside the closure is what a real implementation would have to
/// do, and it is the whole problem: the shape of the win (one body run) and the
/// shape of the cost (one pinned engine) are independent, and this design takes
/// both.
async fn generator_affine_design(pool: Arc<QuickJsEnginePool>, occ: Arc<Occupancy>) -> Duration {
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occ = Arc::clone(&occ);
        tasks.push(tokio::spawn(async move {
            occ.visit_engine();
            pool.with_engine(move |engine| {
                do_some_work(engine);
                // gen.next() → yields the request. One body run, no replay.
                occ.run_body();
                occ.enter_call();
                // The paused generator is in THIS engine. Nothing else may use
                // it until gen.next(response) resumes and finishes.
                std::thread::sleep(RTT);
                occ.leave_call();
                // gen.next(response) → the body finishes from where it paused.
            })
            .await
            .expect("engine");
        }));
    }
    for task in tasks {
        task.await.expect("done");
    }
    started.elapsed()
}

/// § 1 — the three designs against the same pool.
///
/// ```text
/// cargo test -p albedo-server --test aperture_gate5b_context_affinity -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gate: ~1s of deliberate sleeping; run explicitly"]
async fn gate_5b_a_suspended_generator_pins_the_engine_it_paused_in() {
    let pool = Arc::new(QuickJsEnginePool::with_size(POOL_SIZE));

    let blocking = Arc::new(Occupancy::default());
    let blocking_wall = blocking_design(Arc::clone(&pool), Arc::clone(&blocking)).await;

    let replay = Arc::new(Occupancy::default());
    let replay_wall = suspend_replay_design(Arc::clone(&pool), Arc::clone(&replay)).await;

    let affine = Arc::new(Occupancy::default());
    let affine_wall = generator_affine_design(Arc::clone(&pool), Arc::clone(&affine)).await;

    println!(
        "\ngate 5b — pool of {POOL_SIZE}, {ACTIONS} concurrent actions, {RTT:?} per call\n\
         \x20 blocking host fn        peak {:>2}   engine visits {:>2}   body runs {:>2}   wall {:>8.1?}\n\
         \x20 suspend / replay        peak {:>2}   engine visits {:>2}   body runs {:>2}   wall {:>8.1?}\n\
         \x20 generator, engine-affine peak {:>2}   engine visits {:>2}   body runs {:>2}   wall {:>8.1?}\n",
        blocking.peak(), blocking.visits(), blocking.runs(), blocking_wall,
        replay.peak(), replay.visits(), replay.runs(), replay_wall,
        affine.peak(), affine.visits(), affine.runs(), affine_wall,
    );

    // The win is real and it is the point of the proposal: the body runs ONCE
    // where replay runs it twice, so R1's N+1 and everything R2/R3 exist to
    // police go away with it.
    assert_eq!(affine.runs(), ACTIONS, "one body run per action");
    assert_eq!(replay.runs(), ACTIONS * 2, "replay pays a second run");

    // 🔴 And here is the catch, stated as a count so it cannot be argued with:
    // holding the paused state in an engine's heap makes the engine the thing
    // waiting, which is the definition gate 5 used for a blocking host function.
    assert!(
        affine.peak() <= POOL_SIZE,
        "an engine-affine suspension cannot exceed the pool size in flight; got {}",
        affine.peak()
    );
    assert_eq!(
        affine.peak(),
        blocking.peak(),
        "engine-affine generator suspension has the SAME occupancy as the blocking \
         host function gate 5 ruled out — the win is in body runs, not in concurrency"
    );
    assert_eq!(
        replay.peak(),
        ACTIONS,
        "only releasing the engine buys overlap"
    );

    // Wall clock follows occupancy, not body count. Reported rather than
    // asserted tightly, for the reason gate 5 gives.
    assert!(
        affine_wall.as_secs_f64() >= replay_wall.as_secs_f64() * 3.0,
        "affine {affine_wall:?} vs replay {replay_wall:?}"
    );
}

/// § 2 — what an engine costs, which is what "give every in-flight workflow its
/// own context" would have to be paid in.
///
/// § 1 says engine-affine suspension only works if the pool is sized by
/// *concurrency* rather than by CPU. That is affordable only if an engine is
/// cheap, so this measures it instead of assuming it. It mints
/// [`ACTIONS`] engines the way a context-per-workflow pool would have to and
/// reports the per-engine cost.
///
/// **This is a lower bound on the real answer, and deliberately labelled as
/// one.** A `QuickJsEngine` here is a whole runtime; the design being costed
/// would use one runtime with many *contexts* (`rquickjs::AsyncRuntime`), which
/// is cheaper. If the full-runtime price is already tolerable, the context price
/// certainly is — and if it is not, the proposal needs the context split before
/// it needs anything else.
#[test]
#[ignore = "gate: mints 16 engines; run explicitly"]
fn gate_5b_2_what_one_more_live_engine_costs() {
    let started = Instant::now();
    let mut engines = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let mut engine = QuickJsEngine::new();
        engine.init(&BootstrapPayload::default()).expect("init");
        engines.push(engine);
    }
    let elapsed = started.elapsed();

    println!(
        "\ngate 5b.2 — {ACTIONS} live engines minted in {elapsed:.1?} \
         ({:.2?} each)\n\
         \x20 (upper bound: a context-per-workflow design pays LESS than a runtime each)\n",
        elapsed / ACTIONS as u32,
    );

    assert_eq!(engines.len(), ACTIONS);
    // No threshold asserted. The number is the deliverable; a threshold here
    // would be a made-up budget dressed as a requirement.
    assert!(engines.iter().all(QuickJsEngine::is_initialized));
}
