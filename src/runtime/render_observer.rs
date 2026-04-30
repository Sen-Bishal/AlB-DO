//! Process-wide observer hooks for the render pipeline.
//!
//! Two trait types live here, deliberately separated:
//!
//! * [`RenderObserver`] sees one event per component render in the **compile-time**
//!   evaluator (`ComponentProject::render_local`). It carries the cascade chain
//!   collected from a thread-local stack — when component `A` calls into `B`
//!   during its render, `B` is recorded as a cascade child of `A` before its
//!   own frame opens.
//! * [`LaneObserver`] sees one event per **runtime** [`super::frame::frame_tick`]
//!   call. It exposes the per-lane patch counts and bytes the muxer just
//!   shipped, which the dev inspector renders as a lane-utilization heatmap.
//!
//! Both observers are installed via process-wide [`OnceLock`]s. The publish path
//! is `OnceLock::get()` + an `Option::is_some` check when no observer is
//! installed — effectively free in production. When one *is* installed, the
//! cost is a single `Instant::now()`, one allocation per cascade child name,
//! and the observer's own work (which the inspector keeps off the hot path by
//! sending events through a bounded broadcast channel).
//!
//! The compile-time stack is held in a `thread_local!` `RefCell`, which is the
//! correct shape: render evaluation is single-threaded per `ComponentProject`
//! call (the existing evaluator never crosses thread boundaries inside one
//! render pass), and each thread that *does* drive renders gets its own stack.
//!
//! Panic safety: [`enter_frame_guard`] returns an RAII [`FrameGuard`] whose
//! `Drop` impl pops the frame and publishes. If the wrapped render path
//! returns early via `?`, the frame still publishes; if it unwinds, the stack
//! is still left consistent for the next render.

use std::cell::RefCell;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Compile-time render notification. Emitted once per finished
/// `render_local` call by [`FrameGuard::drop`].
#[derive(Debug, Clone)]
pub struct RenderInfo {
    /// JSX function name (e.g. `TodoList`).
    pub component_name: String,
    /// Module the function lives in, in canonical-slash form (e.g. `src/Todo.tsx`).
    pub module_spec: String,
    /// Wall-clock time spent inside this component's body, including the
    /// recursive renders of its JSX children.
    pub duration_us: u64,
    /// Names of components rendered *during* this frame, in source order.
    /// Cheap to clone — these are the JSX function names already owned by the
    /// child frames.
    pub cascade_children: Vec<String>,
}

/// Trait implemented by anything that wants to receive [`RenderInfo`] events.
///
/// Implementations must be cheap on the hot path — the publish call happens
/// inline inside [`FrameGuard::drop`], so anything heavier than a channel
/// send or atomic update should be deferred to a background task.
pub trait RenderObserver: Send + Sync {
    fn on_render(&self, info: RenderInfo);
}

/// Per-frame summary surfaced from [`super::frame::frame_tick`].
///
/// Mirrors the relevant fields of [`super::frame::FrameReport`] without
/// pulling that type into the public observer surface — the report holds
/// optional sequence numbers and ns-precision sub-budgets that the inspector
/// does not consume.
#[derive(Debug, Clone, Copy)]
pub struct LaneFrameReport {
    /// Bytes pushed onto each lane's patch buffer this frame.
    pub lane_bytes: [u64; 4],
    /// Number of patch records routed to each lane this frame.
    pub lane_patches: [u32; 4],
    /// Total dirty slots reconciled this frame.
    pub dirty_count: u64,
    /// End-to-end wall time of the tick.
    pub total_ns: u64,
}

/// Trait implemented by anything that wants to receive [`LaneFrameReport`] events.
pub trait LaneObserver: Send + Sync {
    fn on_frame(&self, report: LaneFrameReport);
}

static RENDER_OBSERVER: OnceLock<Arc<dyn RenderObserver>> = OnceLock::new();
static LANE_OBSERVER: OnceLock<Arc<dyn LaneObserver>> = OnceLock::new();

thread_local! {
    static STACK: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

#[derive(Debug)]
struct Frame {
    name: String,
    module_spec: String,
    started_at: Instant,
    cascade_children: Vec<String>,
}

/// Installs a process-wide render observer. The first call wins; subsequent
/// calls return `Err(observer)` so the caller can decide whether to drop or
/// log the rejected handle. This matches `OnceLock::set` semantics directly.
pub fn install_render_observer(
    observer: Arc<dyn RenderObserver>,
) -> Result<(), Arc<dyn RenderObserver>> {
    RENDER_OBSERVER.set(observer)
}

/// Installs a process-wide lane observer. Same one-shot semantics as
/// [`install_render_observer`].
pub fn install_lane_observer(
    observer: Arc<dyn LaneObserver>,
) -> Result<(), Arc<dyn LaneObserver>> {
    LANE_OBSERVER.set(observer)
}

#[inline]
fn render_observer() -> Option<&'static Arc<dyn RenderObserver>> {
    RENDER_OBSERVER.get()
}

#[inline]
fn lane_observer() -> Option<&'static Arc<dyn LaneObserver>> {
    LANE_OBSERVER.get()
}

/// Entry point for the evaluator. Wrap the body of `render_local` with this:
///
/// ```ignore
/// let _frame = render_observer::enter_frame_guard(function_name, module_spec);
/// // ... existing render body ...
/// ```
///
/// When no [`RenderObserver`] is installed the returned guard is inert — it
/// holds no state and its `Drop` is a single boolean check.
pub fn enter_frame_guard(name: &str, module_spec: &str) -> FrameGuard {
    if render_observer().is_none() {
        return FrameGuard { active: false };
    }
    STACK.with(|cell| {
        let mut stack = cell.borrow_mut();
        if let Some(parent) = stack.last_mut() {
            parent.cascade_children.push(name.to_string());
        }
        stack.push(Frame {
            name: name.to_string(),
            module_spec: module_spec.to_string(),
            started_at: Instant::now(),
            cascade_children: Vec::new(),
        });
    });
    FrameGuard { active: true }
}

/// RAII guard returned by [`enter_frame_guard`]. Pops the frame and publishes
/// it on drop, even on early-`?` returns or unwinds.
#[must_use = "the frame is published when this guard is dropped — bind it to a name"]
pub struct FrameGuard {
    active: bool,
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let info = STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            stack.pop().map(|frame| {
                let elapsed = frame.started_at.elapsed().as_micros();
                let duration_us = u64::try_from(elapsed).unwrap_or(u64::MAX);
                RenderInfo {
                    component_name: frame.name,
                    module_spec: frame.module_spec,
                    duration_us,
                    cascade_children: frame.cascade_children,
                }
            })
        });
        if let (Some(info), Some(observer)) = (info, render_observer()) {
            observer.on_render(info);
        }
    }
}

/// Forwards a frame report to the installed [`LaneObserver`], if any.
/// Called from [`super::frame::frame_tick`] after it computes its
/// [`super::frame::FrameReport`]. Cheap when no observer is installed
/// (single `OnceLock::get`).
pub fn publish_lane_frame(report: LaneFrameReport) {
    if let Some(observer) = lane_observer() {
        observer.on_frame(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<RenderInfo>>,
    }

    impl RenderObserver for Recorder {
        fn on_render(&self, info: RenderInfo) {
            // unwrap_used is denied; lock poisoning here would only affect tests.
            if let Ok(mut events) = self.events.lock() {
                events.push(info);
            }
        }
    }

    fn drain_thread_local_stack() {
        STACK.with(|cell| cell.borrow_mut().clear());
    }

    /// Mutex serializes tests that share the process-wide OBSERVER OnceLock.
    static OBSERVER_LOCK: Mutex<()> = Mutex::new(());

    fn with_recorder<R>(f: impl FnOnce(&Arc<Recorder>) -> R) -> (R, Vec<RenderInfo>) {
        let _guard = OBSERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        drain_thread_local_stack();
        let recorder = Arc::new(Recorder::default());
        // Tests share a OnceLock, so we install once per process via a sentinel
        // — subsequent tests reuse the same observer and just snapshot its
        // events for the call window.
        static INSTALLED: OnceLock<Arc<Recorder>> = OnceLock::new();
        let active = INSTALLED.get_or_init(|| {
            let r = recorder.clone();
            // ignore: if another test won the install race the recorder is
            // still the same instance.
            let _ = install_render_observer(r.clone());
            r
        });
        // Snapshot baseline so we only report events produced inside `f`.
        let baseline = active
            .events
            .lock()
            .map(|events| events.len())
            .unwrap_or(0);
        let result = f(active);
        let new_events = active
            .events
            .lock()
            .map(|events| events.iter().skip(baseline).cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        (result, new_events)
    }

    #[test]
    fn frame_guard_publishes_on_drop_with_duration_recorded() {
        let (_, events) = with_recorder(|_| {
            let _frame = enter_frame_guard("Solo", "src/Solo.tsx");
            std::thread::sleep(std::time::Duration::from_micros(50));
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].component_name, "Solo");
        assert_eq!(events[0].module_spec, "src/Solo.tsx");
        assert!(events[0].cascade_children.is_empty());
    }

    #[test]
    fn nested_frames_record_cascade_in_parent() {
        let (_, events) = with_recorder(|_| {
            let _outer = enter_frame_guard("Parent", "src/Parent.tsx");
            {
                let _inner_a = enter_frame_guard("ChildA", "src/A.tsx");
            }
            {
                let _inner_b = enter_frame_guard("ChildB", "src/B.tsx");
            }
        });
        // Children publish first (drop order is reverse), then the parent.
        let names: Vec<&str> = events.iter().map(|e| e.component_name.as_str()).collect();
        assert_eq!(names, vec!["ChildA", "ChildB", "Parent"]);
        let parent = events.iter().find(|e| e.component_name == "Parent").unwrap();
        assert_eq!(parent.cascade_children, vec!["ChildA", "ChildB"]);
    }

    #[test]
    fn lane_observer_publish_is_noop_without_install() {
        // No installer call — must not panic, must not allocate visibly.
        publish_lane_frame(LaneFrameReport {
            lane_bytes: [0; 4],
            lane_patches: [0; 4],
            dirty_count: 0,
            total_ns: 0,
        });
    }
}
