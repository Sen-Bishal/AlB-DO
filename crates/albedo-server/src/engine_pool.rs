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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// SANDGATE-A · confine an engine's realm after every checkout on which
    /// third-party code could have run. Off via `ALBEDO_SANDGATE=0`.
    confine_after_use: bool,
    /// How many checkouts have been followed by a realm rebuild.
    confinements: Arc<AtomicU64>,
    /// How many of those rebuilds failed. Non-zero means an engine in this pool
    /// is under-populated and will fail the next render loudly.
    confinement_failures: Arc<AtomicU64>,
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
            confine_after_use: std::env::var("ALBEDO_SANDGATE")
                .map(|value| value != "0")
                .unwrap_or(true),
            confinements: Arc::new(AtomicU64::new(0)),
            confinement_failures: Arc::new(AtomicU64::new(0)),
        }
    }

    /// SANDGATE-A · `(confinements, failures)` this pool has performed.
    ///
    /// Exposed because "we rebuild the realm every request" is a claim, and a
    /// pool serving an app with no npm at all correctly reports zero. A reader
    /// who cannot tell those apart cannot tell whether confinement is on.
    #[must_use]
    pub fn confinement_counts(&self) -> (u64, u64) {
        (
            self.confinements.load(Ordering::Relaxed),
            self.confinement_failures.load(Ordering::Relaxed),
        )
    }

    /// Whether this pool confines realms between checkouts.
    #[must_use]
    pub fn confines(&self) -> bool {
        self.confine_after_use
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
        let confine = self.confine_after_use;
        let confinements = Arc::clone(&self.confinements);
        let failures = Arc::clone(&self.confinement_failures);
        let job: Job = Box::new(move |engine: &mut QuickJsEngine| {
            // If the receiver was dropped (caller cancelled), discard quietly.
            let _ = result_tx.send(f(engine));

            // ── SANDGATE-A · the request boundary ─────────────────────────
            //
            // 🔑 **After the result is sent, not before it.** The caller's
            // future resolves on the line above, so the response leaves while
            // this thread rebuilds. The next job for this engine simply queues
            // on its channel — which is the "rebuild it in the background
            // before it is handed out again" design from `SANDGATE.md` § 3,
            // without a second pool to own the dirty engines.
            //
            // The dirty bit has two halves and both are needed.
            // `holds_third_party_code` is tracked in Rust and says npm is
            // *registered*; `third_party_code_ran` asks the realm whether a
            // package factory actually *executed*. Asking the suspect is
            // normally the wrong move, and it is safe here only because the
            // sealed holder exposes a one-way latch — a package can force extra
            // rebuilds and cannot suppress one. Fails closed.
            //
            // 📏 Registration is not execution: a project that depends on Radix
            // but serves a route importing nothing has an untouched realm, and
            // confining it costs the full 1.06 ms replay to protect against
            // nothing.
            if confine && engine.third_party_code_ran() {
                confinements.fetch_add(1, Ordering::Relaxed);
                if let Err(err) = engine.confine() {
                    failures.fetch_add(1, Ordering::Relaxed);
                    // Loud, and not swallowed: this engine's realm is now
                    // missing modules, so the next render on it fails with a
                    // missing-module error that names a package rather than
                    // this. `project_silent_island_death` is the precedent —
                    // a failure nobody can hear is a failure that gets
                    // rediscovered from the symptom.
                    tracing::error!(
                        target: "albedo.sandgate",
                        error = %err,
                        "SANDGATE confinement failed; this engine's realm is                          incompletely populated and the next render on it will fail"
                    );
                }
            }
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

    /// A2 · register the project's npm bundles on **every** engine in the pool.
    ///
    /// # Why this lives on the pool and not on its callers
    ///
    /// 🔴 **Because four separate callers had the same bug and a fifth would
    /// have inherited it.** Every consumer of these engines loads a
    /// boot-precomputed list of *project* modules and renders — the Tier-B
    /// registry's `call`, its `call_metadata`, the row projector, and
    /// [`Self::warm_render_path`] itself — and **not one of them registered an
    /// npm bundle**. The action path escaped only because it routes through
    /// `CompiledProject::invoke_action_quickjs_pass`, which preloads them
    /// itself. The result was that `import clsx from "clsx"` in any per-request
    /// component threw `__ALBEDO_MODULE_MISSING__` and the component vanished
    /// from the page under an HTTP 200.
    ///
    /// npm bundles are a property of the **project**, not of a component, so
    /// the engine is the right owner. Installing here means a new pool consumer
    /// cannot forget: the engine it is handed already has them.
    ///
    /// # Ordering
    ///
    /// Call this **before** [`Self::warm_render_path`]. Warm-up renders real
    /// components, and a component module links its imports *eagerly at load*,
    /// so a warm that runs first would fail on exactly the components this
    /// exists to support.
    ///
    /// Registration is lazy on the JS side — each artifact installs a *factory*
    /// and nothing executes until a module is actually imported — and
    /// `load_precompiled_module` is memoised by `(specifier, source_hash)`, so
    /// calling this again after a dev reload re-registers only what changed.
    ///
    /// Returns the number of artifacts that failed to register. A failure is not
    /// fatal — a project can legitimately carry a package only a Tier-A path
    /// uses — but it is never silent, because the component that imports it
    /// would otherwise fail at render with the missing-module error this whole
    /// change exists to remove.
    pub fn install_npm_bundles(&self, artifacts: &[NpmArtifactRegistration]) -> usize {
        if artifacts.is_empty() {
            return 0;
        }

        let workers: Vec<Worker> = {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            std::mem::take(&mut *idle)
        };

        let mut dones = Vec::with_capacity(workers.len());
        for worker in &workers {
            let (done_tx, done_rx) = mpsc::channel::<usize>();
            let artifacts = artifacts.to_vec();
            let job: Job = Box::new(move |engine: &mut QuickJsEngine| {
                let _ = done_tx.send(register_npm_artifacts(engine, &artifacts));
            });
            if worker.job_tx.send(job).is_ok() {
                dones.push(done_rx);
            }
        }
        // Every engine registers the same set, so the per-engine failure counts
        // agree; take the max rather than the sum so the number reported is
        // "how many artifacts are broken", not "how many times we noticed".
        let mut failed = 0usize;
        for done_rx in dones {
            failed = failed.max(done_rx.recv().unwrap_or(0));
        }

        {
            let mut idle = self.idle.lock().expect("engine pool idle mutex poisoned");
            *idle = workers;
        }
        failed
    }
}

/// One npm artifact as the pool registers it: `(record key, registration
/// script, source hash)`. The tuple mirrors `NpmArtifact`'s fields without
/// making this crate depend on that type's shape.
pub type NpmArtifactRegistration = (String, String, u64);

/// Register every artifact on one engine, returning how many were refused.
///
/// Each failure is logged with the artifact's key: the alternative is a
/// component that imports it failing later with `__ALBEDO_MODULE_MISSING__`,
/// which names the *package* and gives no hint that its registration was the
/// thing that went wrong.
fn register_npm_artifacts(
    engine: &mut QuickJsEngine,
    artifacts: &[NpmArtifactRegistration],
) -> usize {
    use dom_render_compiler::runtime::engine::RuntimeEngine;

    let mut failed = 0usize;
    for (key, script, source_hash) in artifacts {
        if let Err(err) = engine.load_precompiled_module(key, script, *source_hash) {
            failed += 1;
            tracing::warn!(
                target: "albedo.renderer",
                artifact = %key,
                error = %err,
                "npm artifact failed to register on a pool engine; components importing it \
                 will not render"
            );
        }
    }
    failed
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
    let broadcast_current: Vec<(String, Vec<u8>)> = Vec::new();
    let setters = [("__warm".to_string(), SlotId(0))];
    let invocation = HandlerInvocation {
        body,
        is_block: true,
        env: &env,
        raw_bindings: &[],
        setters: &setters,
        event_json: None,
        broadcast_current: &broadcast_current,
        journal: None,
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

    // ── A2 · npm bundles reach the pooled engines ──────────────────────
    //
    // The bug: nothing that renders on a pooled engine ever registered an npm
    // bundle. The Tier-B registry, its metadata path, the row projector and the
    // render warm-up all load only the boot-precomputed *project* modules; the
    // action path escaped because it routes through `CompiledProject`, which
    // preloads them itself. So `import clsx from "clsx"` in a per-request
    // component threw `__ALBEDO_MODULE_MISSING__` and the component silently
    // vanished from an HTTP 200 page.

    /// A factory + alias pair, in the shape the npm bundler emits.
    fn fake_npm_artifacts() -> Vec<NpmArtifactRegistration> {
        vec![
            (
                "npm:testpkg@1.0.0/index.js".to_string(),
                r#"globalThis.__ALBEDO_NPM_FACTORIES["npm:testpkg@1.0.0/index.js"] = function(e) { e.default = 1; };"#
                    .to_string(),
                11,
            ),
            (
                "testpkg".to_string(),
                r#"globalThis.__ALBEDO_NPM_ALIASES["testpkg"] = "npm:testpkg@1.0.0/index.js";"#
                    .to_string(),
                22,
            ),
        ]
    }

    /// Throws unless the alias table knows `testpkg` — the exact lookup a
    /// compiled component's `import` performs, so a pass here means an import
    /// would resolve.
    ///
    /// 🪤 Shaped as a **module with an export and a single expression**, not as
    /// an `if`/`throw` block: `load_module` lowers a body through the component
    /// module compiler, which puts it in expression position, and a statement
    /// there fails to parse for reasons that have nothing to do with what this
    /// is testing. Indexing a missing table throws a `TypeError` on its own,
    /// which is the assertion.
    const ALIAS_PROBE: &str =
        r#"export const alias = globalThis.__ALBEDO_NPM_ALIASES["testpkg"].length;"#;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn installed_npm_bundles_reach_every_pooled_engine() {
        use dom_render_compiler::runtime::engine::RuntimeEngine;

        let pool = QuickJsEnginePool::with_size(3);
        let failed = pool.install_npm_bundles(&fake_npm_artifacts());
        assert_eq!(failed, 0, "well-formed artifacts must register");

        // More checkouts than engines, so every engine is exercised and reuse
        // from the idle stack is covered too. `install_npm_bundles` reaches each
        // engine exactly once, and this is what proves it.
        for _ in 0..6 {
            let outcome = pool
                .with_engine(|engine| {
                    engine
                        .load_module("__probe__", ALIAS_PROBE)
                        .map_err(|err| err.to_string())
                })
                .await
                .expect("checkout succeeds");
            assert!(
                outcome.is_ok(),
                "a pooled engine had no npm alias table — this is the bug that made \
                 every per-request component's `import` fail: {outcome:?}"
            );
        }
    }

    /// The control. Without the install the identical probe must fail — a test
    /// that passes on both sides proves nothing about the wiring.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn without_installation_a_pooled_engine_has_no_npm_aliases() {
        use dom_render_compiler::runtime::engine::RuntimeEngine;

        let pool = QuickJsEnginePool::with_size(1);
        let resolved = pool
            .with_engine(|engine| engine.load_module("__probe__", ALIAS_PROBE).is_ok())
            .await
            .expect("checkout succeeds");
        assert!(
            !resolved,
            "a fresh pool must not already know the package — if this passes, the \
             positive test above is not measuring the installation"
        );
    }

    /// A broken artifact is counted and reported, never swallowed: the component
    /// that imports it would otherwise fail later with a missing-module error
    /// that names the package and not the cause.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_broken_artifact_is_counted_rather_than_silently_dropped() {
        let pool = QuickJsEnginePool::with_size(2);
        let failed = pool.install_npm_bundles(&[(
            "npm:broken@1.0.0/index.js".to_string(),
            "this is not ( valid javascript".to_string(),
            33,
        )]);
        // One broken artifact, however many engines refused it.
        assert_eq!(failed, 1);
    }

    // ── SANDGATE-A · confinement at the checkout boundary ──────────────────

    use dom_render_compiler::runtime::engine::RuntimeEngine as _;

    /// A one-file npm artifact that poisons the realm and remembers it did.
    /// The registration script is what `install_npm_bundles` takes: a factory
    /// installed into `__ALBEDO_NPM_FACTORIES`, run only when something
    /// requires it.
    const POISON_ARTIFACT: &str = r#"
globalThis.__ALBEDO_NPM_FACTORIES['npm:poison@1.0.0/index.js'] = function (exports) {
  globalThis.__poison_marker = (globalThis.__poison_marker || 0) + 1;
  exports.tag = 'poison';
};
globalThis.__ALBEDO_NPM_ALIASES['poison'] = 'npm:poison@1.0.0/index.js';
"#;

    /// 🔑 **The property SANDGATE-A ships**, and the one every earlier gate
    /// stopped short of: state left on a pooled engine by one checkout is gone
    /// by the next.
    ///
    /// Before this, `with_engine` returned the worker to the idle stack
    /// untouched — which is exactly what `tests/quickjs_realm_isolation.rs`
    /// documented about production, and why gate 2 could not invert those
    /// tests.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pooled_engine_does_not_carry_realm_state_between_checkouts() {
        let pool = QuickJsEnginePool::with_size(1);
        assert!(
            pool.confines(),
            "confinement is off by default — the rest of this test is vacuous"
        );
        assert_eq!(
            pool.install_npm_bundles(&[(
                "npm:poison@1.0.0/index.js".to_string(),
                POISON_ARTIFACT.to_string(),
                7,
            )]),
            0
        );

        // Checkout 1 · poison the realm directly (the factory body is what a
        // required package would run).
        let marked = pool
            .with_engine(|engine| {
                engine
                    .load_module("routes/attacker.tsx", ATTACKER_ROUTE)
                    .and_then(|()| engine.render_component("routes/attacker.tsx", "{}"))
                    .map(|out| out.html)
                    .unwrap_or_default()
            })
            .await
            .expect("checkout 1");
        assert!(
            marked.contains('1'),
            "CONTROL — the package must actually have run in checkout 1. Got: {marked}"
        );

        // Checkout 2 · the marker must be gone. If the realm were reused it
        // would read 2, because the route re-imports the package and the
        // factory increments.
        let after = pool
            .with_engine(|engine| {
                engine
                    .render_component("routes/attacker.tsx", "{}")
                    .map(|out| out.html)
                    .unwrap_or_default()
            })
            .await
            .expect("checkout 2");
        assert!(
            after.contains('1'),
            "🔴 SANDGATE-A IS NOT WIRED: the realm survived the checkout boundary, so              the poison counter kept climbing. Got: {after}"
        );

        // The counter is eventually consistent, and reading it straight after a
        // checkout is a race: confinement runs AFTER the result is sent — that
        // is the point, so the response does not wait on it — which means
        // `with_engine` can return before the rebuild it triggered has
        // incremented anything.
        //
        // A third checkout is the synchronisation, not a sleep: the worker's
        // job channel is FIFO, so this job cannot start until the pending
        // rebuild has finished.
        pool.with_engine(|engine| engine.is_initialized())
            .await
            .expect("checkout 3 - flushes the pending confinement");

        let (confinements, failures) = pool.confinement_counts();
        assert!(
            confinements >= 2,
            "the pool reported {confinements} confinements for 3 checkouts on a dirty              engine — the dirty bit is not latching"
        );
        assert_eq!(failures, 0, "a confinement failed; the engine is under-populated");
    }

    const ATTACKER_ROUTE: &str = r#"import { tag } from "poison";
export default function A() { return <b data-tag={tag}>{String(globalThis.__poison_marker)}</b>; }"#;

    /// An app with no npm must not pay for confinement. The dirty bit is what
    /// keeps the cost proportional to the risk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pool_serving_no_npm_never_confines() {
        let pool = QuickJsEnginePool::with_size(1);
        for _ in 0..3 {
            pool.with_engine(|engine| {
                let _ = engine.load_module(
                    "routes/plain.tsx",
                    "export default function P() { return <b>hi</b>; }",
                );
                let _ = engine.render_component("routes/plain.tsx", "{}");
            })
            .await
            .expect("checkout");
        }
        // Flush, for the reason given in the test above: a zero read too early
        // is indistinguishable from a zero that is correct.
        pool.with_engine(|engine| engine.is_initialized())
            .await
            .expect("flush");
        assert_eq!(
            pool.confinement_counts(),
            (0, 0),
            "🔴 a project with no npm dependency is paying for a realm rebuild on              every request and getting nothing for it"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn installing_nothing_is_a_no_op() {
        let pool = QuickJsEnginePool::with_size(1);
        assert_eq!(pool.install_npm_bundles(&[]), 0);
        // The pool is still serviceable afterwards — the early return must not
        // leave the idle set drained.
        assert!(pool.with_engine(|engine| engine.is_initialized()).await.unwrap());
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
                let bc: Vec<(String, Vec<u8>)> = Vec::new();
                let inv = HandlerInvocation {
                    body: "1 + 1",
                    is_block: false,
                    env: &env,
                    raw_bindings: &[],
                    setters: &[],
                    event_json: None,
                    broadcast_current: &bc,
                    journal: None,
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
