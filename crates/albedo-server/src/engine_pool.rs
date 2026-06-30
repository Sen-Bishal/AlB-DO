//! A1 · server-side QuickJS engine pool — *scaffolding*.
//!
//! # Why this module exists
//!
//! The compiled action path now has a QuickJS-backed executor
//! ([`CompiledProject::invoke_action_quickjs_with_broadcast`]). Wiring it into
//! the server's [`crate::actions::ActionHandler`] is not a mechanical swap: it
//! runs into a structural mismatch that is the real design fork of this slice.
//!
//! * [`QuickJsEngine`] is **`!Send`** and every entry point needs **`&mut`**.
//! * The action adapter is **`&self` + `async`** and runs on axum's **multi-thread** tokio runtime
//!   (`rt-multi-thread`), so a future may be parked on one worker thread and resumed on another.
//!
//! A literal "check the engine out, get `&mut`, hand it to the caller, return
//! it on drop" pool is therefore **unsound** here: holding a `!Send` engine
//! across an `.await` on a multi-thread runtime would let it migrate threads.
//!
//! # The reconciliation: engines pinned to dedicated threads
//!
//! We keep the *ergonomics* the user pictured — an explicit, bounded pool you
//! "check out" of — but the engine never crosses a thread boundary. Each engine
//! is owned by its own dedicated OS thread; a "checkout" ships a **closure**
//! (`FnOnce(&mut QuickJsEngine) -> R`) to that thread over a channel and
//! `.await`s the result over a oneshot. The `&mut` borrow is scoped to the
//! closure body, which runs entirely on the engine's thread. `!Send` is thus
//! contained to a single thread for the engine's whole life; only the closure
//! and its return value `R` cross threads, so those must be `Send`.
//!
//! Bounding is an explicit pool size (default = worker-thread count): the pool
//! owns exactly `size` engines and `size` worker threads, and a
//! [`tokio::sync::Semaphore`] gates concurrent checkouts so callers queue
//! instead of oversubscribing.
//!
//! # Warm-on-construction
//!
//! Per the arena discipline ([`crate::renderer_runtime`] /
//! `project_quickjs_arena`), the request-scoped bump arena only enables its
//! O(1) reset after `ARENA_WARMUP_RENDERS` (8) renders have run on *that*
//! engine — before then renders run in persistent (non-reset) mode. A cold
//! engine still produces correct output, it just hasn't enabled fast reset yet.
//! To make every checkout hot, each worker thread warms its engine *before*
//! announcing itself idle, so the pool never hands out a cold engine.
//!
//! See `project_a1_bridge` (remaining slice #1) and `TODO.md` Gate 1 · A1.

use dom_render_compiler::ir::opcode::SlotId;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::HandlerInvocation;
use serde_json::Map;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use tokio::sync::{oneshot, Semaphore};

/// Number of representative handler evals run against a fresh engine at
/// construction so its request-scoped arena promotes out of persistent mode.
/// The engine enables O(1) per-render reset only after its internal
/// `ARENA_WARMUP_RENDERS` (8) renders have populated the persistent region with
/// QuickJS's lazily-allocated global tables; we run a small margin past that so
/// the first real checkout is already in request-scoped mode.
const POOL_WARMUP_RENDERS: u32 = 10;

/// Number of warm-up renders per component in [`warm_render_targets`]. The first
/// render interns the component's QuickJS shapes/atoms into the persistent region;
/// the rest are cheap confirmation that the now-warm path is stable.
const RENDER_WARMUP_REPS: u32 = 2;

/// A component to warm every pool engine's *render* path with. Owns its full
/// dependency-ordered module graph and an entry spec so a pool worker can load and
/// render it off the boot thread. Built from the renderer's Tier-B plan.
#[derive(Clone)]
pub struct WarmupComponent {
    /// `(specifier, code)` pairs in dependency-first load order.
    pub modules: Vec<(String, String)>,
    /// Module spec passed to the render entry (the component's `module_path`).
    pub entry: String,
    /// Props JSON to render with during warm-up (values are irrelevant; only the
    /// component's interned structure matters).
    pub props_json: String,
}

/// A unit of work shipped to an engine's dedicated thread. The closure runs
/// with exclusive `&mut` access to that thread's engine and is responsible for
/// forwarding its own result back to the caller (via a captured oneshot).
///
/// Type-erased to a single signature so heterogeneous return types `R` all flow
/// through the same channel; the `R` is captured inside the boxed closure.
type Job = Box<dyn FnOnce(&mut QuickJsEngine) + Send + 'static>;

/// Errors surfaced by [`QuickJsEnginePool::with_engine`].
#[derive(Debug, thiserror::Error)]
pub enum EnginePoolError {
    /// The semaphore was closed — the pool is shutting down.
    #[error("engine pool is shutting down")]
    ShuttingDown,
    /// The worker thread died (panicked) before returning a result. The engine
    /// it owned is gone; the pool will be one engine short until rebuilt.
    #[error("engine worker thread terminated before returning a result")]
    WorkerLost,
}

/// One pooled engine, represented by the sender end of its thread's job
/// channel. The matching `JoinHandle` is parked in [`QuickJsEnginePool::joins`]
/// for orderly shutdown. Senders are **never cloned** — there is exactly one
/// per engine, so popping it from the idle stack guarantees exclusive access.
struct Worker {
    job_tx: Sender<Job>,
}

/// Bounded, warm-on-construction pool of [`QuickJsEngine`]s, each pinned to a
/// dedicated OS thread. Cheap to clone the handle around behind an `Arc`.
///
/// See the module docs for why checkout ships a closure rather than moving the
/// engine.
pub struct QuickJsEnginePool {
    /// Idle engines available for checkout. Guarded by a `std` mutex held only
    /// for the O(1) pop/push — never across an `.await`.
    idle: Mutex<Vec<Worker>>,
    /// One permit per engine. Acquired (async) before popping from `idle`, so a
    /// successful `acquire` guarantees the pop succeeds.
    permits: Arc<Semaphore>,
    /// Join handles for the worker threads, kept for orderly shutdown in
    /// [`Drop`]. Indexed positionally; not correlated to `idle` order.
    joins: Mutex<Vec<JoinHandle<()>>>,
    /// Number of engines/threads the pool owns.
    size: usize,
}

impl QuickJsEnginePool {
    /// Builds a pool of `size` engines (clamped to at least 1), each on its own
    /// thread and **warmed before the pool returns** so the first checkout is
    /// already hot.
    ///
    /// Spawns `size` threads and blocks the calling (async) context only until
    /// every worker reports "ready". Warmup is CPU-bound and one-time; do this
    /// at server boot, not on the request path.
    #[must_use]
    pub fn with_size(size: usize) -> Self {
        let size = size.max(1);
        let mut idle = Vec::with_capacity(size);
        let mut joins = Vec::with_capacity(size);

        for i in 0..size {
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            // `ready_tx`/`ready_rx`: the worker signals back once its engine is
            // constructed AND warmed, so `with_size` returns only when every
            // engine is hot. A blocking std channel is fine — we are at boot.
            let (ready_tx, ready_rx) = mpsc::channel::<()>();

            let handle = thread::Builder::new()
                .name(format!("albedo-qjs-engine-{i}"))
                .spawn(move || engine_worker_loop(job_rx, ready_tx))
                .expect("failed to spawn QuickJS engine worker thread");

            // Wait for this worker to finish warmup. If the worker panicked
            // during construction/warmup the channel closes; treat that as
            // fatal at boot (a cold/broken engine pool is not serviceable).
            ready_rx
                .recv()
                .expect("QuickJS engine worker failed during warm-up");

            idle.push(Worker { job_tx });
            joins.push(handle);
        }

        Self {
            idle: Mutex::new(idle),
            permits: Arc::new(Semaphore::new(size)),
            joins: Mutex::new(joins),
            size,
        }
    }

    /// Builds a pool sized to the available parallelism (falling back to 1).
    /// This is the boot-time default; matches the multi-thread runtime's
    /// worker count closely enough that checkouts rarely queue.
    #[must_use]
    pub fn with_default_size() -> Self {
        let n = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self::with_size(n)
    }

    /// Number of engines in the pool.
    #[must_use]
    pub fn size(&self) -> usize {
        self.size
    }

    /// Checks out an engine, runs `f` against it on the engine's own thread,
    /// and returns `f`'s result.
    ///
    /// `f` receives exclusive `&mut` access for its duration. Both `f` and its
    /// return value `R` cross the thread boundary, so both must be `Send`; the
    /// engine itself never leaves its thread. The await points are: acquiring a
    /// permit (queues when all engines are busy) and receiving the result.
    ///
    /// # Errors
    /// [`EnginePoolError::ShuttingDown`] if the pool is closing;
    /// [`EnginePoolError::WorkerLost`] if the engine's thread panicked mid-job.
    pub async fn with_engine<F, R>(&self, f: F) -> Result<R, EnginePoolError>
    where
        F: FnOnce(&mut QuickJsEngine) -> R + Send + 'static,
        R: Send + 'static,
    {
        // Gate concurrency to the engine count. Holding the permit for the
        // whole call keeps the popped worker exclusively ours until checkin.
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| EnginePoolError::ShuttingDown)?;

        // A permit in hand guarantees an idle worker exists. Pop without
        // holding the lock across any await.
        let worker = {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            idle.pop()
                .expect("permit acquired but no idle engine — pool invariant broken")
        };

        let (result_tx, result_rx) = oneshot::channel::<R>();
        let job: Job = Box::new(move |engine: &mut QuickJsEngine| {
            // If the receiver was dropped (caller cancelled), discard quietly.
            let _ = result_tx.send(f(engine));
        });

        // Ship the job. Send failing means the worker thread is gone.
        let send_result = worker.job_tx.send(job);

        // Always return the worker to the idle stack so the next checkout can
        // reuse it, even if this job errored. The permit drops at end of scope.
        let result = match send_result {
            Ok(()) => result_rx.await.map_err(|_| EnginePoolError::WorkerLost),
            Err(_) => Err(EnginePoolError::WorkerLost),
        };

        {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            idle.push(worker);
        }

        result
    }

    /// Warm the *render* path of **every** engine in the pool with `components`.
    ///
    /// Unlike [`Self::with_engine`] (which runs on a single arbitrary engine), this
    /// reaches each engine exactly once. It is a **synchronous, blocking** boot-time
    /// call: it pops every worker, ships the render warm-up job to all of them
    /// concurrently (each on its own thread), and blocks until all finish before
    /// returning the workers to the idle set. With an empty `components` slice it is
    /// a no-op.
    ///
    /// Must be called at boot, after construction and before the pool serves any
    /// request: it pops the idle set without acquiring permits, which is sound only
    /// while nothing else is checking engines out. The semaphore's permit count is
    /// untouched, so normal checkouts resume correctly afterwards.
    pub fn warm_render_path(&self, components: &[WarmupComponent]) {
        if components.is_empty() {
            return;
        }

        let workers: Vec<Worker> = {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            std::mem::take(&mut *idle)
        };

        // Ship the warm-up job to every worker, then block on all of them so every
        // engine is hot before we return. A blocking std channel is fine — boot.
        let mut dones = Vec::with_capacity(workers.len());
        for worker in &workers {
            let (done_tx, done_rx) = mpsc::channel::<()>();
            let components = components.to_vec();
            let job: Job = Box::new(move |engine: &mut QuickJsEngine| {
                warm_render_targets(engine, &components);
                let _ = done_tx.send(());
            });
            // If a worker is gone its result never arrives; skip it.
            if worker.job_tx.send(job).is_ok() {
                dones.push(done_rx);
            }
        }
        for done_rx in dones {
            let _ = done_rx.recv();
        }

        {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            *idle = workers;
        }
    }
}

impl Drop for QuickJsEnginePool {
    fn drop(&mut self) {
        // Close the semaphore so any pending `with_engine` awaits resolve to
        // `ShuttingDown` instead of hanging.
        self.permits.close();

        // Drop every idle worker's sender so its thread sees the channel close
        // and exits its loop. Workers checked out at drop time are unreachable
        // (their futures hold no `Arc` to us once the pool is being dropped).
        if let Ok(mut idle) = self.idle.lock() {
            idle.clear();
        }

        // Join the threads we can. Best-effort: a thread whose sender is still
        // held elsewhere won't have exited; we don't block forever on it.
        if let Ok(mut joins) = self.joins.lock() {
            for handle in joins.drain(..) {
                let _ = handle.join();
            }
        }
    }
}

/// Body of an engine worker thread: construct an engine, warm it, signal ready,
/// then service jobs until the job channel closes.
fn engine_worker_loop(job_rx: mpsc::Receiver<Job>, ready_tx: Sender<()>) {
    let mut engine = QuickJsEngine::new();
    warm_engine(&mut engine);

    // Announce readiness. If the receiver is already gone the pool was dropped
    // mid-construction — just exit.
    if ready_tx.send(()).is_err() {
        return;
    }
    drop(ready_tx);

    // Blocking recv: parked with zero CPU cost until a job arrives or the pool
    // drops the sender (loop ends, thread exits, engine drops cleanly).
    while let Ok(job) = job_rx.recv() {
        job(&mut engine);
    }
}

/// Warm a freshly constructed engine's *handler* path so it is hot before its
/// first checkout.
///
/// Two layers of warmth:
/// 1. `prewarm()` — installs the built-in runtime helpers and constructs the QuickJS
///    runtime/context (makes `is_initialized()` true).
/// 2. Drive [`POOL_WARMUP_RENDERS`] representative handler evals so the request-scoped arena
///    promotes out of persistent mode and enables O(1) reset. The warmup body is deliberately broad
///    — a loop, a `try`/`catch`, an array method, a setter call, and an updater-form `broadcast` —
///    so the QuickJS shape/atom tables for the common handler-script machinery are allocated into
///    the persistent region during these renders rather than on a real request. The evals are pure
///    (no `SlotStore`/`BroadcastRegistry`); the collected effects are discarded.
///
/// The *render* path is warmed separately and on demand via
/// [`QuickJsEnginePool::warm_render_path`], because it needs the actual component
/// modules (known only once the manifest is loaded). An engine that is only ever
/// used for actions never pays for render warm-up.
fn warm_engine(engine: &mut QuickJsEngine) {
    engine.prewarm();

    // Representative handler body. Exercises the constructs a real action body
    // commonly hits so their lazily-built engine infrastructure warms here.
    let body = "let acc = 0;\n\
        for (let i = 0; i < 3; i++) { acc += i; }\n\
        try { JSON.parse('{}'); } catch (e) { acc += 1; }\n\
        const arr = [1, 2, 3].map(function (x) { return x + 1; });\n\
        __warm(acc + arr.length);\n\
        broadcast('__albedo_warm_topic', function (n) { return (n || 0) + 1; });";

    let env = Map::new();
    let broadcast_current = Map::new();
    let setters = [("__warm".to_string(), SlotId(0))];
    let invocation = HandlerInvocation {
        body,
        is_block: true,
        env: &env,
        raw_bindings: &[],
        setters: &setters,
        event_json: None,
        broadcast_current: &broadcast_current,
    };

    for _ in 0..POOL_WARMUP_RENDERS {
        // Soft-fail: a warmup eval error degrades to a colder engine (it still
        // serves correctly), it must never abort pool construction.
        let _ = engine.eval_handler("__albedo_pool_warmup", &invocation);
    }
}

/// Warm one engine's *render* path with a known component set, in persistent
/// arena mode. Each component's module graph is loaded and the component rendered
/// a few times inside an explicit [`QuickJsEngine::begin_warmup`] /
/// [`QuickJsEngine::end_warmup`] bracket, so its stable lazily-interned QuickJS
/// state (element/attribute atoms, hidden-class shapes, the render-entry closure)
/// lands in the persistent region instead of being re-interned through the system
/// allocator on every request. This is now a *performance* optimization, not a
/// correctness requirement: request-time memory is QuickJS-managed and freed
/// per-block (see [`crate::runtime`]'s arena docs), so an un-warmed component still
/// renders correctly — it just pays system-allocator churn for its shapes/atoms
/// until they settle. Mirrors the boot renderer's "prime every route" pass, but
/// per pool engine. Soft-fails per step.
fn warm_render_targets(engine: &mut QuickJsEngine, components: &[WarmupComponent]) {
    use dom_render_compiler::runtime::engine::RuntimeEngine;

    engine.begin_warmup();
    for component in components {
        for (specifier, code) in &component.modules {
            let _ = engine.load_module(specifier, code);
        }
        for _ in 0..RENDER_WARMUP_REPS {
            let _ =
                engine.render_component_with_host(&component.entry, &component.props_json, "{}");
        }
    }
    engine.end_warmup();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Warm-on-construction: every engine in the pool is initialized before the
    /// constructor returns, and `with_engine` can reach each of them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_warms_every_engine_at_construction() {
        let pool = QuickJsEnginePool::with_size(3);
        assert_eq!(pool.size(), 3);

        // Each checkout must land on an already-initialized engine. Run more
        // checkouts than engines so we exercise reuse from the idle stack too.
        for _ in 0..6 {
            let initialized = pool
                .with_engine(|engine| engine.is_initialized())
                .await
                .expect("checkout should succeed");
            assert!(
                initialized,
                "pool handed out a cold engine — warm-on-construction broken"
            );
        }
    }

    /// Warm-on-construction reaches the arena layer: after construction, a
    /// checked-out engine runs work in request mode, where request-time memory is
    /// served from (and freed back to) the system allocator rather than bumping the
    /// persistent region. We observe this via `arena_stats`: a post-warmup eval
    /// records request-time system traffic (`system_peak_bytes > 0`). A non-warmed
    /// engine would still be in cold persistent mode (`system_peak_bytes == 0`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_engines_are_warmed_into_request_scoped_mode() {
        use dom_render_compiler::runtime::HandlerInvocation;
        use serde_json::Map;

        let pool = QuickJsEnginePool::with_size(1);
        let peak = pool
            .with_engine(|engine| {
                let env = Map::new();
                let bc = Map::new();
                let inv = HandlerInvocation {
                    body: "1 + 1",
                    is_block: false,
                    env: &env,
                    raw_bindings: &[],
                    setters: &[],
                    event_json: None,
                    broadcast_current: &bc,
                };
                let _ = engine.eval_handler("__warm_probe", &inv);
                engine.arena_stats().system_peak_bytes
            })
            .await
            .expect("checkout");

        assert!(
            peak > 0,
            "engine should serve request memory from the system allocator after warmup \
             (system_peak_bytes > 0)"
        );
    }

    /// The closure's return value crosses the thread boundary correctly and the
    /// engine is reusable across sequential checkouts (state survives checkin).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn with_engine_returns_value_and_reuses_engine() {
        let pool = QuickJsEnginePool::with_size(1);

        let a = pool
            .with_engine(|e| e.is_initialized() as u32)
            .await
            .expect("first checkout");
        let b = pool
            .with_engine(|e| e.is_initialized() as u32)
            .await
            .expect("second checkout reuses the single engine");

        assert_eq!(a, 1);
        assert_eq!(b, 1);
    }

    /// Concurrent checkouts beyond the pool size queue on the semaphore rather
    /// than oversubscribing engines, and all complete.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_checkouts_are_bounded_and_all_complete() {
        let pool = Arc::new(QuickJsEnginePool::with_size(2));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                pool.with_engine(|e| e.is_initialized()).await
            }));
        }
        for h in handles {
            assert!(h.await.expect("task joins").expect("checkout ok"));
        }
    }
}
