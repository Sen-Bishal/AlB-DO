//! TODO #1 item 4.9 **T2** · **measure the classifier, do not change it.**
//!
//! The tiering ladder is the product's pillar, and nothing has ever measured
//! what it actually decides on code that was not written for us. This harness
//! produces that distribution.
//!
//! It is **a measurement, not a gate.** It is `#[ignore]`d and it **asserts no
//! threshold on purpose** — same discipline as [`npm_coverage_probe`] and the
//! `ALBEDO_CONFORMANCE_CORPUS` widening. A ratchet here would only tempt us to
//! sample components that flatter the classifier.
//!
//! Run it with:
//!
//! ```text
//! ALBEDO_TIERING_CORPUS=<manifest.json> \
//!   cargo test --test tiering_corpus_probe -- --ignored --nocapture
//! ```
//!
//! The manifest is a JSON array:
//!
//! ```json
//! [{ "corpus": "scaffold", "kind": "control", "dir": "scaffold/src" },
//!  { "corpus": "bulletproof-react", "kind": "foreign", "dir": "C:/tmp/bp/src" }]
//! ```
//!
//! 🪤 **`kind` is load-bearing, not a label.** Item 4.75 already learned this the
//! expensive way: *"`declined: 0` is a fact about the CORPUS, not the
//! evaluator"* — every fixture in this tree was authored against what we can
//! already do, so of course it classifies cleanly. **A `control` corpus cannot
//! support any claim about real-world code.** The report keeps the two totals
//! apart and refuses to print a combined headline.
//!
//! ## What this calls
//!
//! The production path, deliberately: [`ProjectScanner::scan_directory_with_mode`]
//! → [`ProjectScanner::build_compiler`] → [`decide_tier_and_hydration`]. The
//! scanner is what populates `effect_profile`, `is_interactive`,
//! `is_client_interactive` and `weight`, so this measures the real inputs rather
//! than a re-derivation of them.
//!
//! ⚠️ **One seam.** `manifest::tiering_inputs_from_options` is private, so the
//! four threshold values are copied out of [`ManifestOptions::default()`] here.
//! If a fifth input is ever added to [`TieringInputs`], this file must follow —
//! it will fail to compile, which is the intended alarm.
//!
//! ⚠️ **`reason` is not in the manifest.** [`ComponentManifestEntry`] stores
//! `tier` and `hydration_mode` and **drops `TieringReason` on the floor**, so
//! "which reason dominates" is unanswerable from build output. That is why this
//! probe calls the decision function rather than reading a manifest — and it is
//! itself a finding worth keeping.

use dom_render_compiler::effects::{
    decide_tier_and_hydration, TieringInputs, TieringReason,
};
use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::scanner::{ProjectScanner, ScanMode};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Whether a corpus can support a claim about real-world code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Written in this repo, against this compiler's abilities. Useful as a
    /// baseline and to prove the instrument runs — **never** as evidence.
    Control,
    /// Third-party code that has never heard of ALBEDO. The only kind that
    /// answers the question.
    Foreign,
}

impl Kind {
    fn parse(raw: &str) -> Self {
        match raw {
            "foreign" => Kind::Foreign,
            "control" => Kind::Control,
            other => panic!("unknown corpus kind '{other}' (expected control|foreign)"),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Control => "control",
            Kind::Foreign => "FOREIGN",
        }
    }
}

const TIERS: [Tier; 3] = [Tier::A, Tier::B, Tier::C];

// 🪤 A hand-maintained list, and `reason_index` panics with "known reason" on
// anything absent from it. `RequestScoped` has been missing since AUTH § 3 added
// it — the probe simply never met a component that reads `user`, so the gap was
// invisible until a second reason arrived. Both are here now; a third addition
// will break this the same way, and `reason_label`'s `match` below is the thing
// that will make the compiler say so first.
const REASONS: [TieringReason; 9] = [
    TieringReason::PureStaticEligible,
    TieringReason::ServerOwnedState,
    TieringReason::HookDrivenHydration,
    TieringReason::AsyncBoundary,
    TieringReason::IoBoundary,
    TieringReason::SideEffectBoundary,
    TieringReason::WeightBasedPromotion,
    TieringReason::RequestScoped,
    TieringReason::NpmDependency,
];

fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::A => "A",
        Tier::B => "B",
        Tier::C => "C",
    }
}

fn reason_label(reason: TieringReason) -> &'static str {
    match reason {
        TieringReason::PureStaticEligible => "PureStaticEligible",
        TieringReason::ServerOwnedState => "ServerOwnedState",
        TieringReason::HookDrivenHydration => "HookDrivenHydration",
        TieringReason::AsyncBoundary => "AsyncBoundary",
        TieringReason::IoBoundary => "IoBoundary",
        TieringReason::SideEffectBoundary => "SideEffectBoundary",
        TieringReason::WeightBasedPromotion => "WeightBasedPromotion",
        TieringReason::RequestScoped => "RequestScoped",
        TieringReason::NpmDependency => "NpmDependency",
    }
}

fn tier_index(tier: Tier) -> usize {
    TIERS.iter().position(|t| *t == tier).expect("known tier")
}

fn reason_index(reason: TieringReason) -> usize {
    REASONS
        .iter()
        .position(|r| *r == reason)
        .expect("known reason")
}

/// One classified component.
struct Row {
    tier: Tier,
    reason: TieringReason,
    weight: u64,
    is_interactive: bool,
    is_client_interactive: bool,
}

/// Tests, stories and mocks are real code but they are **not shipped
/// components**, and they skew the distribution hard — a `.test.tsx` is dense
/// with handlers and hooks that no user ever downloads. `ProjectScanner` accepts
/// every `.ts/.tsx/.js/.jsx` it walks, so this filter has to live here.
///
/// 🪤 Leaving them in would inflate the interactive tiers with code that has no
/// tier, which is the same shape of error as counting util modules as "provably
/// A". The count is reported rather than dropped silently.
fn is_non_shipping(path: &str) -> bool {
    let p = path.replace('\\', "/").to_lowercase();
    const MARKERS: &[&str] = &[
        ".test.", ".spec.", ".stories.", ".cy.", ".bench.",
        "/__tests__/", "/__mocks__/", "/__snapshots__/",
        "/e2e/", "/tests/", "/test/", "/cypress/", "/playwright/",
    ];
    MARKERS.iter().any(|marker| p.contains(marker))
}

/// Everything measured for one corpus. Kept per-corpus rather than pooled so a
/// single huge project cannot silently dominate the aggregate.
struct CorpusResult {
    name: String,
    kind: Kind,
    /// Files the scanner rejected. A high rate makes every number below
    /// meaningless — surfaced, never buried.
    parse_failures: usize,
    /// Graph nodes that are data/util modules, not renderable components. The
    /// manifest builder skips these for tiering, so counting them would inflate
    /// "provably A" with utility files.
    module_only: usize,
    /// Components living in test/story/mock files — see [`is_non_shipping`].
    non_shipping: usize,
    rows: Vec<Row>,
}

impl CorpusResult {
    fn tier_counts(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for row in &self.rows {
            counts[tier_index(row.tier)] += 1;
        }
        counts
    }

    fn reason_counts(&self) -> [usize; REASONS.len()] {
        let mut counts = [0usize; REASONS.len()];
        for row in &self.rows {
            counts[reason_index(row.reason)] += 1;
        }
        counts
    }

    /// `[tier][reason]`.
    fn matrix(&self) -> [[usize; REASONS.len()]; TIERS.len()] {
        let mut matrix = [[0usize; REASONS.len()]; TIERS.len()];
        for row in &self.rows {
            matrix[tier_index(row.tier)][reason_index(row.reason)] += 1;
        }
        matrix
    }

    /// Weights of the components where **no effect signal fired and weight
    /// decided the tier**. If this set is large, tiering is measuring file size.
    fn weight_decided(&self) -> Vec<u64> {
        let mut weights: Vec<u64> = self
            .rows
            .iter()
            .filter(|row| row.reason == TieringReason::WeightBasedPromotion)
            .map(|row| row.weight)
            .collect();
        weights.sort_unstable();
        weights
    }
}

fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "—".to_string();
    }
    format!("{:.1}%", (part as f64 / whole as f64) * 100.0)
}

fn measure(name: &str, kind: Kind, dir: &Path) -> CorpusResult {
    let scanner = ProjectScanner::new();
    let report = scanner
        .scan_directory_with_mode(dir, ScanMode::Lenient)
        .unwrap_or_else(|err| panic!("scan '{}': {err}", dir.display()));

    let parse_failures = report.failures.len();
    for failure in &report.failures {
        eprintln!(
            "   parse-fail {}: {}",
            failure.path.display(),
            failure.message
        );
    }

    let compiler = scanner.build_compiler(report.components);
    let components = compiler.graph().components();

    // The four thresholds the manifest path uses. See the seam note in the
    // module header.
    let options = ManifestOptions::default();
    let inputs = TieringInputs {
        tier_a_inline_max_bytes: options.tier_a_inline_max_bytes,
        tier_c_split_min_bytes: options.tier_c_split_min_bytes,
        tier_b_mode: options.tier_b_mode,
        tier_c_mode: options.tier_c_mode,
    };

    let mut module_only = 0usize;
    let mut non_shipping = 0usize;
    let mut rows = Vec::new();

    for component in components {
        // Mirrors the manifest builder's child-walk, which skips module-only
        // nodes. Counting a util file as "provably A" would be exactly the kind
        // of rounding-in-our-favour the npm probe was corrected for.
        if component.is_module_only {
            module_only += 1;
            continue;
        }

        if is_non_shipping(&component.file_path) {
            non_shipping += 1;
            continue;
        }

        let weight = component.weight.max(0.0).round() as u64;
        let decision = decide_tier_and_hydration(
            component.effect_profile,
            component.is_interactive,
            component.is_client_interactive,
            component.state_escapes,
            component.reads_principal,
            component.imports_npm,
            component.is_above_fold,
            weight,
            inputs,
        );

        rows.push(Row {
            tier: decision.tier,
            reason: decision.reason,
            weight,
            is_interactive: component.is_interactive,
            is_client_interactive: component.is_client_interactive,
        });
    }

    CorpusResult {
        name: name.to_string(),
        kind,
        parse_failures,
        module_only,
        non_shipping,
        rows,
    }
}

fn write_corpus_section(out: &mut String, result: &CorpusResult) {
    let total = result.rows.len();
    let counts = result.tier_counts();
    let matrix = result.matrix();

    let _ = writeln!(
        out,
        "\n### {} · `{}`\n",
        result.name,
        result.kind.label()
    );
    let _ = writeln!(
        out,
        "{total} renderable components · {} module-only · {} test/story \
         (both excluded) · {} parse failures",
        result.module_only, result.non_shipping, result.parse_failures
    );

    if result.parse_failures * 4 > total.max(1) {
        let _ = writeln!(
            out,
            "\n> ⚠️ **Parse failure rate is high — treat every number in this \
             section as unreliable.** The classifier only sees what parsed."
        );
    }

    let _ = writeln!(out, "\n| tier | n | share |\n|---|---:|---:|");
    for tier in TIERS {
        let n = counts[tier_index(tier)];
        let _ = writeln!(out, "| {} | {} | {} |", tier_label(tier), n, pct(n, total));
    }

    let _ = writeln!(out, "\n**Tier × reason**\n");
    let _ = writeln!(out, "| reason | A | B | C |\n|---|---:|---:|---:|");
    for reason in REASONS {
        let r = reason_index(reason);
        let row = [matrix[0][r], matrix[1][r], matrix[2][r]];
        if row.iter().all(|n| *n == 0) {
            continue;
        }
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            reason_label(reason),
            row[0],
            row[1],
            row[2]
        );
    }

    let interactive = result.rows.iter().filter(|r| r.is_interactive).count();
    let client_interactive = result
        .rows
        .iter()
        .filter(|r| r.is_client_interactive)
        .count();
    let _ = writeln!(
        out,
        "\n**Interactivity levers** — `is_interactive` {} ({}) · \
         `is_client_interactive` {} ({})",
        interactive,
        pct(interactive, total),
        client_interactive,
        pct(client_interactive, total)
    );

    let weights = result.weight_decided();
    if weights.is_empty() {
        let _ = writeln!(
            out,
            "\n**Weight-decided:** none — every component was classified by an \
             effect signal."
        );
    } else {
        let median = weights[weights.len() / 2];
        let _ = writeln!(
            out,
            "\n**Weight-decided:** {} of {} ({}) — no effect signal fired and \
             `weight_bytes` chose the tier. Weights min/median/max = {} / {} / {} \
             against thresholds 8192 (A ceiling) and 40960 (C floor).",
            weights.len(),
            total,
            pct(weights.len(), total),
            weights.first().copied().unwrap_or(0),
            median,
            weights.last().copied().unwrap_or(0)
        );
    }
}

#[test]
#[ignore = "needs a corpus on disk; run explicitly with ALBEDO_TIERING_CORPUS set"]
fn measure_tier_distribution_across_corpora() {
    let Ok(manifest_path) = std::env::var("ALBEDO_TIERING_CORPUS") else {
        eprintln!("SKIP: set ALBEDO_TIERING_CORPUS to a corpus JSON file");
        return;
    };

    let raw = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("read corpus manifest '{manifest_path}': {err}"));
    let manifest: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("parse corpus manifest: {err}"));
    let entries = manifest.as_array().expect("corpus manifest is a JSON array");

    let mut results: Vec<CorpusResult> = Vec::new();

    for entry in entries {
        let name = entry["corpus"].as_str().expect("corpus name");
        let kind = Kind::parse(entry["kind"].as_str().expect("corpus kind"));
        let dir = PathBuf::from(entry["dir"].as_str().expect("corpus dir"));

        if !dir.is_dir() {
            eprintln!("SKIP corpus '{name}': no directory at {}", dir.display());
            continue;
        }

        eprintln!("── {name} ({}) — scanning {} ──", kind.label(), dir.display());
        results.push(measure(name, kind, &dir));
    }

    let mut out = String::new();
    let _ = writeln!(out, "# TIER DISTRIBUTION — item 4.9 T2\n");
    let _ = writeln!(
        out,
        "Harness: `tests/tiering_corpus_probe.rs`. Asserts no threshold on purpose."
    );

    // Two separate totals. Merging them would produce exactly the number 4.75
    // warned about: a headline that a control corpus quietly props up.
    for kind in [Kind::Foreign, Kind::Control] {
        let group: Vec<&CorpusResult> = results.iter().filter(|r| r.kind == kind).collect();
        if group.is_empty() {
            continue;
        }

        let total: usize = group.iter().map(|r| r.rows.len()).sum();
        let mut counts = [0usize; 3];
        let mut reasons = [0usize; REASONS.len()];
        for result in &group {
            let c = result.tier_counts();
            let r = result.reason_counts();
            for i in 0..3 {
                counts[i] += c[i];
            }
            for i in 0..7 {
                reasons[i] += r[i];
            }
        }

        let _ = writeln!(out, "\n---\n\n## {} corpora — {} components\n", kind.label(), total);

        if kind == Kind::Control {
            let _ = writeln!(
                out,
                "> 🪤 **Control only. This says nothing about real-world code** — every \
                 component here was written against what this compiler can already do.\n"
            );
        }

        let _ = writeln!(out, "| tier | n | share |\n|---|---:|---:|");
        for tier in TIERS {
            let n = counts[tier_index(tier)];
            let _ = writeln!(out, "| **{}** | {} | **{}** |", tier_label(tier), n, pct(n, total));
        }

        let mut ranked: Vec<(usize, TieringReason)> = REASONS
            .iter()
            .map(|r| (reasons[reason_index(*r)], *r))
            .filter(|(n, _)| *n > 0)
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0));

        let _ = writeln!(out, "\n| reason (ranked) | n | share |\n|---|---:|---:|");
        for (n, reason) in ranked {
            let _ = writeln!(out, "| `{}` | {} | {} |", reason_label(reason), n, pct(n, total));
        }

        for result in group {
            write_corpus_section(&mut out, result);
        }
    }

    println!("{out}");

    if let Ok(path) = std::env::var("ALBEDO_TIERING_REPORT") {
        std::fs::write(&path, &out).unwrap_or_else(|err| panic!("write report '{path}': {err}"));
        eprintln!("report written to {path}");
    }

    // Deliberately no assertion on the distribution. The only thing worth
    // failing on is the instrument measuring nothing at all.
    assert!(
        results.iter().any(|r| !r.rows.is_empty()),
        "no corpus yielded a single classified component — the probe is broken, \
         not the classifier"
    );
}
