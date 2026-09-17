//! Enqueues recorded by a body, waiting for the caller to write them.
//!
//! An exact sibling of [`crate::forge::write`]'s collector, and deliberately so:
//! the problem is identical. A body runs **synchronously** inside the engine; the
//! queue is **async**; so the builtin cannot perform the write, only record the
//! intent. The caller installs a collector, runs the body, takes what was
//! recorded, and writes it where an `await` is legal.
//!
//! ## Why a collector rather than a return value
//!
//! Because the alternative is silent. Without an installed collector,
//! [`record`] answers `false` and the caller raises "enqueue() was called but
//! nothing is collecting" — the same discipline `record_forge_write` uses. A
//! builtin that quietly dropped the intent would make a body that *looks* like
//! it queued work produce nothing at all, which is the failure class this
//! codebase keeps paying to find.

use std::cell::RefCell;

use serde_json::Value;

/// One `enqueue(name, args, options)` call, before it becomes a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueIntent {
    /// The export in `src/jobs.ts` to run.
    pub name: String,
    /// The JSON argument.
    pub args: Value,
    /// A caller-supplied row id, which makes the enqueue idempotent. `None`
    /// means the caller mints a random one and the enqueue always lands.
    pub id: Option<String>,
    /// How long to wait before it may run, as written (`"5m"`).
    pub delay: Option<String>,
}

thread_local! {
    /// Intents recorded by the body currently being evaluated. `None` means no
    /// collector is installed, which is how the builtin knows it is being called
    /// somewhere that cannot queue and can say so.
    static ENQUEUES: RefCell<Option<Vec<EnqueueIntent>>> = const { RefCell::new(None) };
}

/// Collects the enqueues a body records, restoring the previous collector on
/// drop.
///
/// The guard discipline matters for the same reason it does on the FORGE
/// collector: a job body may itself dispatch, and an inner body must not steal
/// an outer one's intents.
pub struct EnqueueCollector {
    previous: Option<Vec<EnqueueIntent>>,
}

impl EnqueueCollector {
    /// The intents recorded since installation, in call order.
    #[must_use]
    pub fn take(&self) -> Vec<EnqueueIntent> {
        ENQUEUES.with(|cell| {
            cell.borrow_mut()
                .as_mut()
                .map_or_else(Vec::new, std::mem::take)
        })
    }
}

impl Drop for EnqueueCollector {
    fn drop(&mut self) {
        ENQUEUES.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
}

/// Install a collector for the duration of one dispatch.
#[must_use]
pub fn install_enqueue_collector() -> EnqueueCollector {
    let previous = ENQUEUES.with(|cell| cell.borrow_mut().replace(Vec::new()));
    EnqueueCollector { previous }
}

/// Record one intent. `false` when no collector is installed, which the caller
/// must turn into an error rather than ignore.
#[must_use]
pub fn record(intent: EnqueueIntent) -> bool {
    ENQUEUES.with(|cell| match cell.borrow_mut().as_mut() {
        Some(list) => {
            list.push(intent);
            true
        }
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(name: &str) -> EnqueueIntent {
        EnqueueIntent {
            name: name.to_string(),
            args: Value::Null,
            id: None,
            delay: None,
        }
    }

    #[test]
    fn without_a_collector_a_record_is_refused_rather_than_dropped() {
        assert!(
            !record(intent("orphan")),
            "a silently dropped enqueue is a body that looks like it queued work and did not"
        );
    }

    #[test]
    fn intents_come_back_in_call_order() {
        let collector = install_enqueue_collector();
        assert!(record(intent("first")));
        assert!(record(intent("second")));
        let taken = collector.take();
        assert_eq!(
            taken.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn taking_twice_does_not_replay() {
        let collector = install_enqueue_collector();
        assert!(record(intent("once")));
        assert_eq!(collector.take().len(), 1);
        assert!(
            collector.take().is_empty(),
            "a second take must not re-deliver what was already written"
        );
    }

    /// The guard property: an inner dispatch must not steal an outer one's
    /// intents, and must not leave the outer one holding its own.
    #[test]
    fn a_nested_collector_restores_the_one_it_replaced() {
        let outer = install_enqueue_collector();
        assert!(record(intent("outer-before")));
        {
            let inner = install_enqueue_collector();
            assert!(record(intent("inner")));
            assert_eq!(
                inner.take().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
                vec!["inner"]
            );
        }
        assert!(record(intent("outer-after")));
        assert_eq!(
            outer.take().iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["outer-before", "outer-after"],
            "the outer collector must still hold exactly its own"
        );
    }
}
