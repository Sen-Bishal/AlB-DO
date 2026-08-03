//! Renderer conformance — the two renderers, over one corpus, compared.
//!
//! ## Why this exists
//!
//! There are two renderers over the same source. The pure-Rust evaluator
//! ([`crate::runtime::eval`]) renders *and records provenance* — bindings,
//! slots, topics, tiering. QuickJS renders arbitrary JS correctly. They serve
//! disjoint paths, so nothing forces them to agree, and when they disagree the
//! failure is invisible: a `BindEvent` names an id that is not in the served
//! markup, `_requireNode` throws, bakabox drops the **entire** opcode frame,
//! and the user sees a button that does nothing. Nothing is logged. It cannot
//! be reproduced from the report.
//!
//! `tests/tier_b_anchor_parity.rs` closed one instance of that against one
//! fixture, on `data-albedo-id` alone. Its header states the trap plainly: *a
//! unit test of either renderer alone cannot see this — each one is
//! self-consistent.* This module generalises that check to the whole corpus and
//! to the whole of the markup.
//!
//! ## What it is worth beyond the gate
//!
//! Passing this is what promotes the evaluator from "a risky second
//! implementation" to *the semantic model of the framework*. Every construct
//! the evaluator learns becomes a new capability in the wire, the delta, the
//! topic and the tiering simultaneously — but only if moving work into it is
//! safe, and it is only safe if something checks the two against each other.
//!
//! The by-product is worth as much as the gate. Classifying *why* a case did
//! not compare produces the evaluator's coverage frontier — which constructs it
//! declines, and how often — for free, from the same walk.
//!
//! ## The one taxonomy rule
//!
//! A [`Verdict`] must never let a defect wear a coverage gap's clothes. The
//! evaluator *deliberately* refuses constructs it does not model, and that
//! refusal is correct behaviour ([`Verdict::EvaluatorDeclined`]). Any other
//! evaluator error is a defect ([`Verdict::EvaluatorFaulted`]). Collapsing the
//! two would be comfortable and wrong — the npm-coverage probe made exactly
//! that mistake and reported 95.8% where the truth was 85.2%, because
//! "your fixture is wrong" and "we cannot do this" arrived as the same error.
//! **An error taxonomy that groups those two will always round in our favour.**
//!
//! The same rule is why [`Verdict::NotComparable`] exists rather than a quiet
//! `continue`: a case the harness could not *set up* (a route needing props it
//! was not given) is not evidence of agreement, and it is counted where it can
//! be seen.

pub mod normalize;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::runtime::engine::{BootstrapPayload, RuntimeEngine};
use crate::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use crate::runtime::quickjs_engine::QuickJsEngine;
use crate::runtime::session::SessionId;
use crate::runtime::slot_store::SlotStore;

pub use normalize::Normalization;

/// Which of the evaluator's two modes a case compares.
///
/// Both are real serve paths, and they fail differently, so both are checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Contract {
    /// `hook_compile: false` — the static render. This is the markup the
    /// manifest builder bakes for a Tier-A component, so a divergence here is
    /// wrong bytes shipped to a user with no client code to correct them.
    Structural,
    /// `hook_compile: true` — the render the opcode frame is emitted from. A
    /// divergence here is the frame-drop failure: the ids in the frame and the
    /// ids in the served markup describe different documents.
    Reactive,
}

impl Contract {
    pub fn render_options(self) -> RenderOptions {
        RenderOptions {
            hook_compile: matches!(self, Contract::Reactive),
        }
    }
}

impl fmt::Display for Contract {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Contract::Structural => "structural",
            Contract::Reactive => "reactive",
        })
    }
}

/// What happened when one component was rendered both ways.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Byte-for-byte equal. The strongest result, and the one to hold ground on.
    Identical,

    /// Equal after applying the named, information-preserving normalizations in
    /// [`normalize`]. Still a pass; recorded separately so the report can show
    /// how much forgiveness the corpus needs, and so a case sliding from
    /// [`Verdict::Identical`] to here is visible.
    Equivalent { applied: Vec<Normalization> },

    /// Both renderers produced markup and it does not match. **This is the
    /// failure the harness exists for.**
    Diverge {
        rust: String,
        quickjs: String,
        /// Byte offset of the first difference in the normalized forms.
        at: usize,
    },

    /// The evaluator refused, naming a construct it does not model. Correct
    /// behaviour — the component belongs on QuickJS — and the raw material of
    /// the coverage frontier.
    EvaluatorDeclined { construct: String },

    /// The evaluator failed in a way that is *not* a declared refusal. A defect.
    EvaluatorFaulted { reason: String },

    /// QuickJS could not render it. Not a parity result: it says nothing about
    /// whether the two agree. Recorded so it cannot be silently dropped.
    QuickJsFaulted { reason: String },

    /// The harness could not set the case up — most often a route that needs
    /// props or a host seed it was not given. Never counted as agreement.
    NotComparable { reason: String },
}

impl Verdict {
    /// Whether this verdict should fail the gate.
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            Verdict::Diverge { .. } | Verdict::EvaluatorFaulted { .. }
        )
    }

    /// Whether the two renderers were actually compared. `false` for every
    /// verdict that is about the harness or one renderer alone — the honest
    /// denominator for "what fraction agrees" counts only these.
    pub fn is_comparison(&self) -> bool {
        matches!(
            self,
            Verdict::Identical | Verdict::Equivalent { .. } | Verdict::Diverge { .. }
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Identical => "identical",
            Verdict::Equivalent { .. } => "equivalent",
            Verdict::Diverge { .. } => "DIVERGE",
            Verdict::EvaluatorDeclined { .. } => "declined",
            Verdict::EvaluatorFaulted { .. } => "EVALUATOR-FAULT",
            Verdict::QuickJsFaulted { .. } => "quickjs-fault",
            Verdict::NotComparable { .. } => "not-comparable",
        }
    }
}

/// One component, in one corpus, under one contract.
#[derive(Debug, Clone)]
pub struct CaseId {
    pub corpus: String,
    pub entry: String,
    pub contract: Contract,
}

impl fmt::Display for CaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{} [{}]", self.corpus, self.entry, self.contract)
    }
}

#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub id: CaseId,
    pub verdict: Verdict,
}

impl CaseOutcome {
    /// A failure message with the first difference located and windowed.
    ///
    /// Two 2 KB markup strings printed in full are unreadable, and unreadable
    /// output is how a gate stops being read at all.
    pub fn describe(&self) -> String {
        match &self.verdict {
            Verdict::Diverge { rust, quickjs, at } => {
                format!(
                    "{}\n  first difference at byte {at}\n    rust:    …{}\n    quickjs: …{}\n\
                     \n  full rust:    {rust}\n  full quickjs: {quickjs}",
                    self.id,
                    window(rust, *at),
                    window(quickjs, *at),
                )
            }
            Verdict::EvaluatorFaulted { reason } => {
                format!(
                    "{}\n  the evaluator failed without naming an unsupported construct, \
                     which makes it a defect rather than a coverage boundary:\n    {reason}",
                    self.id
                )
            }
            other => format!("{}\n  {}", self.id, other.label()),
        }
    }
}

/// 60 bytes on either side of `at`, clipped to char boundaries.
fn window(text: &str, at: usize) -> String {
    let start = text[..at.min(text.len())]
        .char_indices()
        .rev()
        .nth(20)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = text
        .char_indices()
        .skip_while(|(i, _)| *i < at)
        .nth(60)
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    text[start..end].to_string()
}

/// Byte offset of the first difference between two strings, or `None` if equal.
fn first_difference(a: &str, b: &str) -> Option<usize> {
    a.as_bytes()
        .iter()
        .zip(b.as_bytes())
        .position(|(x, y)| x != y)
        .or(if a.len() == b.len() {
            None
        } else {
            Some(a.len().min(b.len()))
        })
}

/// Classify an evaluator error as a declared refusal or a defect.
///
/// The evaluator's refusals are a small, fixed vocabulary, listed here rather
/// than guessed at. `tests/renderer_conformance.rs` renders a fixture that
/// triggers each one, so a reworded message fails a test instead of quietly
/// getting reclassified as a fault — or, far worse, a genuine fault getting
/// reclassified as a refusal.
pub fn classify_evaluator_error(message: &str) -> Verdict {
    // `unsupported statement `for` in pure-Rust evaluator for module '…'`
    if let Some(rest) = message.split("unsupported statement `").nth(1) {
        if let Some(kind) = rest.split('`').next() {
            return Verdict::EvaluatorDeclined {
                construct: format!("statement: {kind}"),
            };
        }
    }
    if message.contains("unsupported JSX tag") {
        return Verdict::EvaluatorDeclined {
            construct: "jsx: non-identifier tag".to_string(),
        };
    }
    if message.contains("spread attributes are not supported") {
        return Verdict::EvaluatorDeclined {
            construct: "jsx: spread attribute".to_string(),
        };
    }
    if message.contains("unsupported JSX attribute name") {
        return Verdict::EvaluatorDeclined {
            construct: "jsx: namespaced attribute".to_string(),
        };
    }
    Verdict::EvaluatorFaulted {
        reason: message.to_string(),
    }
}

/// Render one entry both ways and compare.
///
/// `props` is the case's input. A route that reads `params.id` renders nothing
/// useful from `{}`, and the difference between "we gave it nothing" and "it is
/// broken" is exactly the distinction [`Verdict::NotComparable`] preserves.
pub fn compare_entry(
    project: &CompiledProject,
    entry: &str,
    props: &Value,
    contract: Contract,
) -> Verdict {
    let rust = {
        let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
        render_entry_with_bindings(project, entry, props, &slots, &contract.render_options())
    };

    // A fresh engine per case: a warm engine carries module state from earlier
    // cases, and a harness whose result depends on the order its corpus happens
    // to be walked in is not evidence of anything.
    let quickjs = match fresh_engine() {
        Ok(mut engine) => {
            preload_project_modules(project, &mut engine);
            let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
            project
                .render_entry_quickjs(&mut engine, entry, props, &slots)
                .map(|out| out.html)
        }
        Err(err) => {
            return Verdict::NotComparable {
                reason: format!("could not start a QuickJS engine: {err:#}"),
            }
        }
    };

    match (rust, quickjs) {
        (Ok(rust), Ok(quickjs)) => compare_markup(&rust.html, &quickjs, contract),
        (Err(err), _) => classify_evaluator_error(&format!("{err:#}")),
        (Ok(_), Err(err)) => Verdict::QuickJsFaulted {
            reason: format!("{err:#}"),
        },
    }
}

fn fresh_engine() -> Result<QuickJsEngine> {
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    Ok(engine)
}

/// Load every module in the project into the engine before rendering.
///
/// `render_entry_quickjs` loads only the entry, which is enough for a leaf
/// component and not enough for anything real: a route that imports two
/// components fails to link, and the corpus would silently narrow to whatever
/// happens to have no imports. Each module is loaded under the same
/// project-relative spec the evaluator hashes ids from, so the anchors agree.
///
/// A module that fails to load is skipped rather than fatal — the entry may not
/// need it, and if it does, the render fails next with a better message than
/// this loop could give.
fn preload_project_modules(project: &CompiledProject, engine: &mut QuickJsEngine) {
    let specs: Vec<String> = project.project().modules().keys().cloned().collect();
    for spec in specs {
        let Some(source) = project.project().module_source(&spec) else {
            continue;
        };
        let source = source.to_string();
        let _ = engine.load_module_with_spec(&spec, &source, Some(&spec));
    }
}

/// Compare two renders, applying the declared normalizations only as far as
/// they are needed.
///
/// Applied in order of how much they forgive, so a case that is byte-identical
/// is reported as byte-identical rather than as "equivalent after two
/// transformations that happened to be no-ops".
pub fn compare_markup(rust: &str, quickjs: &str, contract: Contract) -> Verdict {
    if rust == quickjs {
        return Verdict::Identical;
    }

    let mut applied = Vec::new();

    let (mut a, mut b) = (rust.to_string(), quickjs.to_string());

    // Only the reactive contract may introduce anchor wrappers; forgiving them
    // under the structural contract would forgive a wrapper appearing where the
    // static render must not have one.
    if contract == Contract::Reactive {
        let (sa, sb) = (
            normalize::strip_reactive_anchors(&a),
            normalize::strip_reactive_anchors(&b),
        );
        if (sa != a || sb != b) && sa == sb {
            applied.push(Normalization::ReactiveAnchorWrapper);
            return Verdict::Equivalent { applied };
        }
        if sa != a || sb != b {
            applied.push(Normalization::ReactiveAnchorWrapper);
            a = sa;
            b = sb;
        }
    }

    let (ca, cb) = (
        normalize::canonicalize_attribute_order(&a),
        normalize::canonicalize_attribute_order(&b),
    );
    if ca == cb {
        applied.push(Normalization::AttributeOrder);
        return Verdict::Equivalent { applied };
    }

    Verdict::Diverge {
        at: first_difference(&ca, &cb).unwrap_or(0),
        rust: rust.to_string(),
        quickjs: quickjs.to_string(),
    }
}

/// Render the entry in hook-compile mode and report which ids its opcode frame
/// names that the QuickJS markup does not contain.
///
/// This is the failure mode stated directly, and it is **not** implied by
/// markup equality. A build-time Tier-B render pairs a frame emitted by the
/// evaluator with markup served, at request time, by QuickJS
/// (`manifest::builder::render_tier_b_inline` produces the frame;
/// `render::tier_b` produces the markup). If the two number elements
/// differently, `_requireNode` throws on the first missing id and bakabox drops
/// the whole frame — every handler on the page, not just the one.
///
/// `Ok(vec![])` is the healthy answer. `Err` means one side could not render,
/// which this function does not classify — use [`compare_entry`] for that.
pub fn frame_addressability(
    project: &CompiledProject,
    entry: &str,
    props: &Value,
) -> Result<Vec<u32>> {
    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let rust = render_entry_with_bindings(
        project,
        entry,
        props,
        &slots,
        &Contract::Reactive.render_options(),
    )?;

    let mut engine = fresh_engine()?;
    preload_project_modules(project, &mut engine);
    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let served = project
        .render_entry_quickjs(&mut engine, entry, props, &slots)?
        .html;

    let addressed: Vec<u32> = rust
        .opcodes
        .iter()
        .filter_map(instruction_stable_id)
        .collect();

    Ok(unaddressable_ids(&addressed, &served))
}

/// The `stable_id` an instruction addresses, when it addresses one.
fn instruction_stable_id(instruction: &crate::ir::opcode::Instruction) -> Option<u32> {
    use crate::ir::opcode::Instruction as I;
    match instruction {
        I::SetText { stable_id, .. }
        | I::SetAttr { stable_id, .. }
        | I::BindEvent { stable_id, .. }
        | I::BindSlot { stable_id, .. }
        | I::SetTextRef { stable_id, .. }
        | I::SetAttrRef { stable_id, .. } => Some(stable_id.0),
        // `Create` / `Append` / `Remove` / `Patch` / `Placeholder` address
        // nodes the frame itself creates or a placeholder the shell emitted,
        // not a pre-rendered anchor — a missing id there is a different bug
        // with a different fix, so they are not folded in here.
        _ => None,
    }
}

/// Every id an opcode frame addresses must exist in the markup that will
/// actually be served, or the client drops the whole frame.
///
/// This is deliberately a *separate* question from markup equality. A render
/// pair can be perfectly equivalent under [`compare_markup`] and still be
/// unservable, because the reactive anchors that equality forgives are exactly
/// the nodes the frame points at.
pub fn unaddressable_ids(opcode_ids: &[u32], served_markup: &str) -> Vec<u32> {
    let present = anchor_ids(served_markup);
    let mut missing: Vec<u32> = opcode_ids
        .iter()
        .copied()
        .filter(|id| !present.contains(&id.to_string()))
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}

/// Every `data-albedo-id` in `html`, in document order.
pub fn anchor_ids(html: &str) -> Vec<String> {
    const NEEDLE: &str = "data-albedo-id=\"";
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(NEEDLE) {
        rest = &rest[start + NEEDLE.len()..];
        let Some(end) = rest.find('"') else { break };
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    out
}

/// The result of walking a corpus.
#[derive(Debug, Default, Clone)]
pub struct ConformanceReport {
    pub outcomes: Vec<CaseOutcome>,
}

impl ConformanceReport {
    pub fn push(&mut self, id: CaseId, verdict: Verdict) {
        self.outcomes.push(CaseOutcome { id, verdict });
    }

    pub fn failures(&self) -> Vec<&CaseOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.verdict.is_failure())
            .collect()
    }

    fn count(&self, pred: impl Fn(&Verdict) -> bool) -> usize {
        self.outcomes.iter().filter(|o| pred(&o.verdict)).count()
    }

    /// The evaluator's coverage frontier: declined constructs by frequency.
    ///
    /// This is the histogram `RESEARCH_AND_DEVELOPMENT.md` § A1 asks for, and it
    /// costs nothing extra — it is the same walk, read from the other side.
    pub fn coverage_frontier(&self) -> BTreeMap<String, usize> {
        let mut hist = BTreeMap::new();
        for outcome in &self.outcomes {
            if let Verdict::EvaluatorDeclined { construct } = &outcome.verdict {
                *hist.entry(construct.clone()).or_insert(0) += 1;
            }
        }
        hist
    }

    /// Human-readable summary.
    ///
    /// Leads with the conservative number. The fraction that agrees is over
    /// cases that were *actually compared* — cases the harness could not set up
    /// are reported on their own line and never inflate the numerator or shrink
    /// the denominator.
    pub fn summary(&self) -> String {
        let identical = self.count(|v| matches!(v, Verdict::Identical));
        let equivalent = self.count(|v| matches!(v, Verdict::Equivalent { .. }));
        let diverge = self.count(|v| matches!(v, Verdict::Diverge { .. }));
        let declined = self.count(|v| matches!(v, Verdict::EvaluatorDeclined { .. }));
        let faulted = self.count(|v| matches!(v, Verdict::EvaluatorFaulted { .. }));
        let qjs = self.count(|v| matches!(v, Verdict::QuickJsFaulted { .. }));
        let unset = self.count(|v| matches!(v, Verdict::NotComparable { .. }));
        let compared = self.outcomes.iter().filter(|o| o.verdict.is_comparison()).count();

        let mut out = String::new();
        out.push_str("RENDERER CONFORMANCE\n");
        out.push_str(&format!(
            "  compared        {compared:>4}   ({identical} identical, {equivalent} equivalent, \
             {diverge} DIVERGENT)\n"
        ));
        out.push_str(&format!(
            "  declined        {declined:>4}   evaluator refused a construct it does not model\n"
        ));
        out.push_str(&format!(
            "  evaluator fault {faulted:>4}   errors that named no construct — defects\n"
        ));
        out.push_str(&format!("  quickjs fault   {qjs:>4}\n"));
        out.push_str(&format!(
            "  not comparable  {unset:>4}   harness could not set the case up\n"
        ));
        out.push_str(&format!("  cases total     {:>4}\n", self.outcomes.len()));

        let frontier = self.coverage_frontier();
        if !frontier.is_empty() {
            out.push_str("\nEVALUATOR COVERAGE FRONTIER (declines by construct)\n");
            let mut rows: Vec<_> = frontier.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            for (construct, count) in rows {
                out.push_str(&format!("  {count:>4}  {construct}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_markup_is_reported_as_identical_not_merely_equivalent() {
        let html = "<div class=\"a\" id=\"b\">x</div>";
        assert!(matches!(
            compare_markup(html, html, Contract::Structural),
            Verdict::Identical
        ));
    }

    #[test]
    fn attribute_order_alone_is_equivalent() {
        let v = compare_markup(
            "<div class=\"a\" id=\"b\">x</div>",
            "<div id=\"b\" class=\"a\">x</div>",
            Contract::Structural,
        );
        match v {
            Verdict::Equivalent { applied } => {
                assert_eq!(applied, vec![Normalization::AttributeOrder])
            }
            other => panic!("expected equivalent, got {other:?}"),
        }
    }

    /// The structural contract is the static-render path, where an anchor
    /// wrapper has no business appearing. Forgiving it there would forgive
    /// shipping a wrapper to a user with no client code to remove it.
    #[test]
    fn a_reactive_anchor_is_not_forgiven_under_the_structural_contract() {
        let with = "<ul><span data-albedo-id=\"1\" style=\"display:contents\"><li>a</li></span></ul>";
        let without = "<ul><li>a</li></ul>";
        assert!(matches!(
            compare_markup(with, without, Contract::Structural),
            Verdict::Diverge { .. }
        ));
        assert!(matches!(
            compare_markup(with, without, Contract::Reactive),
            Verdict::Equivalent { .. }
        ));
    }

    #[test]
    fn different_text_diverges_under_every_contract() {
        for contract in [Contract::Structural, Contract::Reactive] {
            assert!(matches!(
                compare_markup("<p>a</p>", "<p>b</p>", contract),
                Verdict::Diverge { .. }
            ));
        }
    }

    /// The taxonomy rule, asserted directly: a declared refusal and an
    /// arbitrary failure must not land in the same bucket.
    #[test]
    fn a_declared_refusal_and_an_arbitrary_failure_classify_differently() {
        let declined = classify_evaluator_error(
            "unsupported statement `for-of` in pure-Rust evaluator for module 'a.tsx'",
        );
        match declined {
            Verdict::EvaluatorDeclined { construct } => {
                assert_eq!(construct, "statement: for-of")
            }
            other => panic!("expected a decline, got {other:?}"),
        }

        assert!(matches!(
            classify_evaluator_error("index out of bounds: the len is 3 but the index is 7"),
            Verdict::EvaluatorFaulted { .. }
        ));
    }

    #[test]
    fn a_fault_fails_the_gate_and_a_decline_does_not() {
        assert!(classify_evaluator_error("some internal panic").is_failure());
        assert!(!classify_evaluator_error("unsupported statement `while` in x").is_failure());
    }

    /// Cases the harness could not set up must not count toward agreement in
    /// either direction — that is the whole point of the separate verdict.
    #[test]
    fn a_not_comparable_case_is_neither_a_comparison_nor_a_failure() {
        let v = Verdict::NotComparable {
            reason: "needs props".to_string(),
        };
        assert!(!v.is_comparison());
        assert!(!v.is_failure());
    }

    #[test]
    fn unaddressable_ids_finds_the_frame_target_that_is_not_in_the_markup() {
        let markup = "<div data-albedo-id=\"10\"><b data-albedo-id=\"11\">x</b></div>";
        assert!(unaddressable_ids(&[10, 11], markup).is_empty());
        assert_eq!(unaddressable_ids(&[10, 99], markup), vec![99]);
    }

    #[test]
    fn the_summary_denominator_excludes_cases_that_were_never_compared() {
        let mut report = ConformanceReport::default();
        let id = |contract| CaseId {
            corpus: "c".into(),
            entry: "e".into(),
            contract,
        };
        report.push(id(Contract::Structural), Verdict::Identical);
        report.push(
            id(Contract::Reactive),
            Verdict::NotComparable {
                reason: "no props".into(),
            },
        );
        let summary = report.summary();
        assert!(summary.contains("compared           1"), "{summary}");
        assert!(summary.contains("not comparable     1"), "{summary}");
    }
}
