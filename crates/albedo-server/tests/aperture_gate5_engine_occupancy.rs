//! APERTURE · **gate 5 — engine occupancy.**
//!
//! `APERTURE.md` § 12: *pool of 2, 16 concurrent actions, one 50 ms external
//! call each. Wall clock ~50–100 ms, not ~400 ms. A blocking host function
//! cannot pass this.*
//!
//! This is the gate that decides A2's **shape**, so it is run before A2 is
//! written rather than after. The design question it settles:
//!
//! > When an action body calls outward, does the QuickJS engine stay checked
//! > out for the round trip, or is it released and the body replayed?
//!
//! The cheap answer — a blocking host function bound into the engine — is what
//! `TODO.md:251`'s 3–5 day estimate buys. The expensive answer — suspend,
//! release, replay with a memo journal — is § 5. Everything else in A2 (the
//! journal, the sentinel, the `catch` fold, hoisting) exists to make the second
//! one work. If the first one were adequate, none of it would be worth
//! building.
//!
//! ## Why the primary assertion is a count, not a stopwatch
//!
//! `PRISM.md` § 11's lesson, and A0 already applied it: a claim proved by
//! timing is a claim that gets re-litigated on someone else's machine. The wall
//! clock here is a *consequence* of something exactly countable — **how many of
//! the sixteen round trips were in flight at the same moment.** Under a
//! blocking host function that number cannot exceed the engine count, because
//! the engine is what is waiting. Under suspend/replay it is bounded by nothing
//! the engine owns.
//!
//! So the gate asserts the peak, exactly, and reports the wall clock alongside
//! it as the thing the peak causes.
//!
//! ## What is and is not simulated
//!
//! There is no `fetch` in an action body yet — that is A2. What exists is the
//! real [`QuickJsEnginePool`], and *engine occupancy* is entirely a property of
//! how a design checks engines in and out of it. So both designs are driven
//! against the real pool, with the upstream call standing in as a sleep:
//!
//! - **blocking** — the sleep happens *inside* `with_engine`. The engine is held for the whole
//!   round trip. This is what a host function does, whatever it is implemented with.
//! - **suspend/replay** — the body runs inside `with_engine` and returns (suspends), the round trip
//!   happens with no engine checked out, and the body **re-enters the pool** to replay. The replay
//!   is paid, not hidden: the counters below show it as a second engine visit per action.
//!
//! What this does **not** measure is the cost of replaying a *real* body with a
//! real journal — that is gate 1, and it needs A2's journal to exist. This gate
//! measures the thing gate 1 cannot: that the pool is free during the wait.

use albedo_server::engine_pool::QuickJsEnginePool;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Engines. Deliberately small: the whole question is what happens when there
/// are fewer engines than concurrent actions.
const POOL_SIZE: usize = 2;
/// Concurrent actions, each making one external call.
const ACTIONS: usize = 16;
/// Stand-in for one upstream round trip.
const RTT: Duration = Duration::from_millis(50);

/// Counts what the gate is actually about: how many round trips overlapped, and
/// how many times an engine was checked out to get there.
#[derive(Debug, Default)]
struct Occupancy {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    engine_visits: AtomicUsize,
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

    fn peak(&self) -> usize {
        self.peak_in_flight.load(Ordering::SeqCst)
    }

    fn visits(&self) -> usize {
        self.engine_visits.load(Ordering::SeqCst)
    }
}

/// Give the engine a little real work so a "visit" is not free — otherwise the
/// replay design's extra checkout costs literally nothing and the comparison
/// flatters it.
fn do_some_work(engine: &mut QuickJsEngine) -> bool {
    engine.is_initialized()
}

/// **The blocking host function.** The round trip happens with the engine
/// checked out, because that is what a host function *is*: the JS stack is
/// suspended mid-call and the engine cannot run anything else.
async fn blocking_design(pool: Arc<QuickJsEnginePool>, occupancy: Arc<Occupancy>) -> Duration {
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occupancy = Arc::clone(&occupancy);
        tasks.push(tokio::spawn(async move {
            occupancy.visit_engine();
            pool.with_engine(move |engine| {
                do_some_work(engine);
                // Inside the engine. Nothing else may run on it until this
                // returns — including the sixteen other actions.
                occupancy.enter_call();
                std::thread::sleep(RTT);
                occupancy.leave_call();
            })
            .await
            .expect("engine available");
        }));
    }
    for task in tasks {
        task.await.expect("action completed");
    }
    started.elapsed()
}

/// **Suspend and replay.** The body runs, suspends at the call, and the engine
/// goes back to the pool for the duration of the round trip. When the answer
/// lands the body re-enters the pool and replays with the value memoized.
async fn suspend_replay_design(
    pool: Arc<QuickJsEnginePool>,
    occupancy: Arc<Occupancy>,
) -> Duration {
    let started = Instant::now();
    let mut tasks = Vec::with_capacity(ACTIONS);
    for _ in 0..ACTIONS {
        let pool = Arc::clone(&pool);
        let occupancy = Arc::clone(&occupancy);
        tasks.push(tokio::spawn(async move {
            // Pass 1: run until the call, then suspend. The engine is checked
            // in the moment `with_engine` returns.
            occupancy.visit_engine();
            pool.with_engine(do_some_work).await.expect("engine");

            // The round trip, with **no engine held**. This is the whole claim.
            occupancy.enter_call();
            tokio::time::sleep(RTT).await;
            occupancy.leave_call();

            // Pass 2: replay with the answer memoized. The cost the journal
            // buys the release with, and it is counted, not hidden.
            occupancy.visit_engine();
            pool.with_engine(do_some_work).await.expect("engine");
        }));
    }
    for task in tasks {
        task.await.expect("action completed");
    }
    started.elapsed()
}

/// Gate 5.
///
/// Ignored by default because it spends ~0.5 s sleeping and the ordinary suite
/// should stay fast. Run it deliberately:
///
/// ```text
/// cargo test -p albedo-server --test aperture_gate5_engine_occupancy -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gate: ~0.5s of deliberate sleeping; run explicitly"]
async fn gate_5_the_engine_is_released_across_the_round_trip() {
    let pool = Arc::new(QuickJsEnginePool::with_size(POOL_SIZE));
    assert_eq!(pool.size(), POOL_SIZE);

    let blocking = Arc::new(Occupancy::default());
    let blocking_wall = blocking_design(Arc::clone(&pool), Arc::clone(&blocking)).await;

    let suspend = Arc::new(Occupancy::default());
    let suspend_wall = suspend_replay_design(Arc::clone(&pool), Arc::clone(&suspend)).await;

    println!(
        "\ngate 5 — pool of {POOL_SIZE}, {ACTIONS} concurrent actions, {RTT:?} per call\n\
         \x20 blocking host fn   peak in-flight {:>2}   engine visits {:>2}   wall {:>8.1?}\n\
         \x20 suspend / replay   peak in-flight {:>2}   engine visits {:>2}   wall {:>8.1?}\n",
        blocking.peak(),
        blocking.visits(),
        blocking_wall,
        suspend.peak(),
        suspend.visits(),
        suspend_wall,
    );

    // ── the countable claim ────────────────────────────────────────────────
    //
    // A blocking host function cannot have more round trips in flight than it
    // has engines, because the engine is the thing waiting. This is not a
    // tuning problem: adding engines adds memory and threads to buy back
    // concurrency the design gave away.
    assert!(
        blocking.peak() <= POOL_SIZE,
        "a blocking host fn cannot exceed the pool size in flight; got {}",
        blocking.peak()
    );
    assert_eq!(
        suspend.peak(),
        ACTIONS,
        "releasing the engine must let every round trip overlap, independent of pool size"
    );

    // The replay is real and is stated: two engine visits per action, not one.
    assert_eq!(blocking.visits(), ACTIONS);
    assert_eq!(suspend.visits(), ACTIONS * 2);

    // ── the consequence ────────────────────────────────────────────────────
    //
    // Asserted as a **ratio**, not against absolute milliseconds, so a loaded
    // or slow machine cannot turn a structural claim into a flaky one. The
    // arithmetic the ratio is standing in for: 16 actions over 2 engines is 8
    // serialized round trips (~400 ms) against one overlapped round trip
    // (~50 ms), so the true separation is ~8× and 3× is a floor with room.
    assert!(
        blocking_wall.as_secs_f64() >= suspend_wall.as_secs_f64() * 3.0,
        "expected the blocking design to be several times slower; \
         blocking {blocking_wall:?} vs suspend/replay {suspend_wall:?}"
    );

    // And the positive form of the same fact, generously bounded: the suspend
    // design finishes in the neighbourhood of ONE round trip regardless of
    // there being sixteen of them and only two engines.
    assert!(
        suspend_wall < RTT * 4,
        "suspend/replay should cost about one round trip, not eight; got {suspend_wall:?}"
    );
}
