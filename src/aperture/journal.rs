//! APERTURE · A2 — the journal.
//!
//! `development-plan/APERTURE.md` § 5.2 calls this *"the single load-bearing
//! decision in this document"*, and § 5.3 says why: **an append-only log with a
//! stable step index, not a `HashMap<key, value>` with an ordinal hack.** The
//! position of a step in this log is the idempotency key of the request it
//! records, so the log's ordering is not an implementation detail — it is the
//! thing the guarantee is made of.
//!
//! ## What a step is
//!
//! Every non-deterministic act a handler body performs. Today that is one kind
//! — an outbound call — and the enum exists so that `Date.now()` and
//! `Math.random()` (§ 11 R2) become rows rather than special cases.
//!
//! ## Why it can be in-memory today
//!
//! A2's journal lives for the length of one action dispatch, and that is enough
//! to buy suspend/replay and derived idempotency keys. A3 persists it to FORGE
//! and the same log then survives a crash — *the interface is the thing that
//! has to be right now* (§ 5.2). Nothing here knows where it is stored.
//!
//! ## What is deliberately not journaled
//!
//! **Request headers.** § 11 R6: the digest covers method, URL and body, and
//! credentials are attached by the client at send time from the source
//! declaration. A journal dump must not be a credential dump.

use serde_json::{json, Value};

/// Maximum steps one workflow may record (§ 8, "unbounded journal growth").
///
/// A body that loops issuing calls is a bug, and an unbounded journal turns
/// that bug into memory exhaustion in the server rather than an error in the
/// handler.
pub const DEFAULT_STEP_CAP: usize = 64;

/// Maximum number of body passes one dispatch may take.
///
/// Distinct from the step cap and bounding a different failure: a body that
/// suspends without ever recording progress (a `fetch` inside a `catch` that
/// swallows, a digest that changes every pass) would otherwise replay forever.
/// Each pass must either complete or add at least one step, so this is a
/// backstop against a body that does neither.
pub const DEFAULT_PASS_CAP: usize = 34;

/// What kind of non-deterministic act a step records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// An outbound HTTP call.
    Fetch,
}

impl StepKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Fetch => "fetch",
        }
    }
}

/// How a step ended.
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    /// The act completed and this is what the body sees on every later pass.
    Completed(Value),
    /// The act failed in a way the body should observe as a thrown error.
    Failed(String),
    /// **Indeterminate** — the request may or may not have reached the
    /// upstream. § 11 R4 exists for this row: it replays under the *same*
    /// derived key, so a retry is a retry rather than a second effect.
    Unknown,
}

/// One recorded act.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// Position in the log. Equal to the index in [`Journal::steps`] — carried
    /// explicitly because it is the idempotency key, not a vector offset.
    pub index: u32,
    /// What kind of act this was.
    pub kind: StepKind,
    /// Digest of the request as the body described it (method, URL, body —
    /// never headers). A replay that produces a different digest at the same
    /// index has diverged, and § 10 requires that to be loud.
    pub request_digest: String,
    /// The result the body observes on replay.
    pub outcome: StepOutcome,
}

/// Why an append was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalError {
    /// A step arrived out of order. The log is append-only by index; a gap or
    /// a repeat means two passes disagree about where they are, and accepting
    /// it would silently re-key every later request.
    OutOfOrder { expected: u32, got: u32 },
    /// The step cap was reached.
    StepCap { cap: usize },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::OutOfOrder { expected, got } => write!(
                f,
                "aperture: journal step {got} arrived out of order (expected {expected}); \
                 the log is append-only and its index is the idempotency key"
            ),
            JournalError::StepCap { cap } => write!(
                f,
                "aperture: workflow exceeded its step cap of {cap} outbound calls"
            ),
        }
    }
}

impl std::error::Error for JournalError {}

/// The append-only log of one workflow.
#[derive(Debug, Clone, PartialEq)]
pub struct Journal {
    workflow_id: String,
    build_id: String,
    steps: Vec<Step>,
    step_cap: usize,
}

impl Journal {
    /// A fresh log.
    ///
    /// `build_id` binds the workflow to the code that started it (§ 11 R8): an
    /// `albedo dev` world swap mid-workflow would otherwise replay new code
    /// against steps recorded by old code, which is R2's divergence arriving
    /// through the back door.
    #[must_use]
    pub fn new(workflow_id: impl Into<String>, build_id: impl Into<String>) -> Self {
        Self {
            workflow_id: workflow_id.into(),
            build_id: build_id.into(),
            steps: Vec::new(),
            step_cap: DEFAULT_STEP_CAP,
        }
    }

    /// Override the step cap (tests, and a future per-app setting).
    #[must_use]
    pub fn with_step_cap(mut self, cap: usize) -> Self {
        self.step_cap = cap;
        self
    }

    #[must_use]
    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }

    /// The `build_id` this workflow was started under.
    #[must_use]
    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    #[must_use]
    pub fn get(&self, index: u32) -> Option<&Step> {
        self.steps.get(index as usize)
    }

    /// Append a completed act at the next index.
    ///
    /// # Errors
    /// [`JournalError::OutOfOrder`] when `index` is not the next position, and
    /// [`JournalError::StepCap`] when the log is full.
    pub fn append(
        &mut self,
        index: u32,
        kind: StepKind,
        request_digest: impl Into<String>,
        outcome: StepOutcome,
    ) -> Result<(), JournalError> {
        let expected = u32::try_from(self.steps.len()).unwrap_or(u32::MAX);
        if index != expected {
            return Err(JournalError::OutOfOrder {
                expected,
                got: index,
            });
        }
        if self.steps.len() >= self.step_cap {
            return Err(JournalError::StepCap { cap: self.step_cap });
        }
        self.steps.push(Step {
            index,
            kind,
            request_digest: request_digest.into(),
            outcome,
        });
        Ok(())
    }

    /// The idempotency key for a step: **the journal position is the key**
    /// (§ 5.3). `POST /v1/charges` at step 3 of `w_abc` ships `w_abc:3`.
    ///
    /// The author never types one, and cannot get it wrong. A retry of an
    /// indeterminate step reuses this exact string, which is what makes the
    /// retry safe against an upstream that implements the server half — as
    /// Stripe and every serious payments API do.
    #[must_use]
    pub fn idempotency_key(&self, index: u32) -> String {
        format!("{}:{index}", self.workflow_id)
    }

    /// The log as the handler script sees it: a **dense array**, one slot per
    /// step, in index order.
    ///
    /// Dense and positional on purpose. `fetch()` reads
    /// `__albedo_journal[step]`, so alignment between the array and the log is
    /// the whole mechanism; a sparse or filtered encoding would silently
    /// re-key every step after the first gap. An [`StepOutcome::Unknown`] step
    /// encodes as `null` — a miss, so the body re-issues it — and it keeps its
    /// slot, so every later step keeps its index and therefore its key.
    #[must_use]
    pub fn to_script_value(&self) -> Value {
        Value::Array(
            self.steps
                .iter()
                .map(|step| match &step.outcome {
                    StepOutcome::Completed(value) => json!({
                        "d": step.request_digest,
                        "ok": true,
                        "v": value,
                    }),
                    StepOutcome::Failed(message) => json!({
                        "d": step.request_digest,
                        "ok": false,
                        "e": message,
                    }),
                    StepOutcome::Unknown => Value::Null,
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(body: &str) -> StepOutcome {
        StepOutcome::Completed(json!({ "status": 200, "body": body }))
    }

    #[test]
    fn steps_read_back_by_index_in_the_order_they_were_appended() {
        let mut journal = Journal::new("w_abc", "build-1");
        journal.append(0, StepKind::Fetch, "d0", completed("a")).unwrap();
        journal.append(1, StepKind::Fetch, "d1", completed("b")).unwrap();

        assert_eq!(journal.len(), 2);
        assert_eq!(journal.get(0).unwrap().request_digest, "d0");
        assert_eq!(journal.get(1).unwrap().request_digest, "d1");
        assert_eq!(journal.get(2), None);
    }

    /// The index is the key, so an out-of-order append is refused rather than
    /// tolerated: accepting a gap would re-key every later request, and the
    /// keys are what an upstream deduplicates on.
    #[test]
    fn an_out_of_order_append_is_refused() {
        let mut journal = Journal::new("w", "b");
        journal.append(0, StepKind::Fetch, "d0", completed("a")).unwrap();

        assert_eq!(
            journal.append(2, StepKind::Fetch, "d2", completed("c")),
            Err(JournalError::OutOfOrder { expected: 1, got: 2 })
        );
        assert_eq!(
            journal.append(0, StepKind::Fetch, "d0", completed("a")),
            Err(JournalError::OutOfOrder { expected: 1, got: 0 }),
            "a repeat is the same failure as a gap — the log is append-only"
        );
    }

    #[test]
    fn the_step_cap_bounds_a_looping_body() {
        let mut journal = Journal::new("w", "b").with_step_cap(2);
        journal.append(0, StepKind::Fetch, "d", completed("a")).unwrap();
        journal.append(1, StepKind::Fetch, "d", completed("a")).unwrap();
        assert_eq!(
            journal.append(2, StepKind::Fetch, "d", completed("a")),
            Err(JournalError::StepCap { cap: 2 })
        );
    }

    /// § 5.3 — the author never writes a key, and the same step always derives
    /// the same one, which is what makes a retry a retry.
    #[test]
    fn the_idempotency_key_is_the_workflow_and_the_position() {
        let journal = Journal::new("w_abc", "b");
        assert_eq!(journal.idempotency_key(3), "w_abc:3");
        assert_eq!(journal.idempotency_key(3), journal.idempotency_key(3));
    }

    /// An indeterminate step must keep its slot. If it were dropped, every
    /// later step would shift down one index — and since the index is the key,
    /// a retry would present a *different* key for the same call and the
    /// upstream would treat a duplicate as new. This is the row § 11 R4 is
    /// about, and the encoding is where it is either safe or not.
    #[test]
    fn an_unknown_step_encodes_as_a_miss_but_keeps_its_slot() {
        let mut journal = Journal::new("w", "b");
        journal.append(0, StepKind::Fetch, "d0", completed("a")).unwrap();
        journal.append(1, StepKind::Fetch, "d1", StepOutcome::Unknown).unwrap();
        journal.append(2, StepKind::Fetch, "d2", completed("c")).unwrap();

        let seeded = journal.to_script_value();
        let array = seeded.as_array().expect("array");
        assert_eq!(array.len(), 3, "dense — one slot per step");
        assert!(array[1].is_null(), "the unknown step reads as a miss");
        assert_eq!(array[2]["d"], "d2");
        assert_eq!(
            journal.idempotency_key(2),
            "w:2",
            "and step 2 keeps the key it had before the retry"
        );
    }

    #[test]
    fn a_failed_step_replays_as_a_throw_rather_than_a_value() {
        let mut journal = Journal::new("w", "b");
        journal
            .append(0, StepKind::Fetch, "d0", StepOutcome::Failed("boom".into()))
            .unwrap();

        let seeded = journal.to_script_value();
        assert_eq!(seeded[0]["ok"], false);
        assert_eq!(seeded[0]["e"], "boom");
        assert!(seeded[0].get("v").is_none());
    }
}
