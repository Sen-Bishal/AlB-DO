//! Phase O.3 · bundle-byte budget integration test.
//!
//! Drives the public path the `albedo build` gate exercises: emit
//! bundle artifacts to a temp dir, run the bundle byte report, run
//! the bundle-budget evaluator. Covers:
//!
//!   1. A small Tier-B component passes the 1 KB default ceiling.
//!   2. A Tier-B component pumped past the ceiling triggers a
//!      `TierBBundleKbPerComponent` violation with the actionable
//!      diff text the sprint plan's demo asks for.
//!   3. Bumping the per-component ceiling via the config relaxes
//!      the gate.

use dom_render_compiler::budget::{
    compute_bundle_byte_report, evaluate_bundle_budget, format_report_pretty, BudgetDefaults,
    TierBudget, ViolationKind,
};
use dom_render_compiler::bundler::emit::{
    emit_bundle_artifacts_to_dir, BundleEmitReport, EmittedArtifact,
};
use dom_render_compiler::bundler::{build_bundle_plan, BundlePlan, BundlePlanOptions};
use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier,
};
use std::path::PathBuf;
use tempfile::tempdir;

fn entry(id: u64, name: &str, tier: Tier, weight_bytes: u64) -> ComponentManifestEntry {
    ComponentManifestEntry {
        id,
        name: name.to_string(),
        module_path: format!("src/{name}.tsx"),
        tier,
        weight_bytes,
        priority: 1.0,
        dependencies: Vec::new(),
        can_defer: false,
        hydration_mode: HydrationMode::None,
    }
}

fn manifest_with(components: Vec<ComponentManifestEntry>) -> RenderManifestV2 {
    let mut m = RenderManifestV2::legacy_defaults();
    m.components = components;
    m
}

#[test]
fn small_tier_b_component_passes_default_bundle_ceiling() {
    let temp = tempdir().unwrap();
    let manifest = manifest_with(vec![entry(1, "Counter", Tier::B, 200)]);
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let emit_report = emit_bundle_artifacts_to_dir(&plan, temp.path()).unwrap();

    let bundle_report = compute_bundle_byte_report(&emit_report, &plan, &manifest);
    let budget = TierBudget::default();
    let report = evaluate_bundle_budget(&bundle_report, &budget);

    assert!(
        report.is_ok(),
        "small Counter should fit under 1 KB ceiling; violations: {:?}",
        report.violations
    );
}

/// Attaches a synthetic per-component artifact at the plan's wrapper path.
///
/// ⚠️ **These tests used to inflate an artifact the emit had really produced.**
/// Wrapper modules are no longer written (they were unloaded by anything and
/// leaked the build host's absolute paths), so there is nothing on disk to
/// inflate and the byte source has to be injected.
///
/// That is not a testing inconvenience, it is the finding: `budget_bytes()` is
/// `wrapper_bytes`, and a wrapper was a fixed 4-line trampoline whose length
/// varied only with **how long the source file's path was**. The gate never
/// measured a component's cost — it measured its path. So per-component bundle
/// attribution is inert in a real build today, and these tests now cover the
/// evaluator and its diff text *only*.
///
/// 🔗 The real number exists: `compile_client_island_module`, the same lowering
/// item 4.6 used to replace the fabricated tier-report figure. Pointing
/// attribution at it is the scoped follow-up (`OPTIMIZATIONS.md` § O.3).
fn with_component_bytes(
    emit_report: &BundleEmitReport,
    plan: &BundlePlan,
    component_id: u64,
    bytes: usize,
) -> BundleEmitReport {
    let wrapper_path = plan
        .modules
        .iter()
        .find(|m| m.component_id == component_id)
        .map(|m| m.wrapper_module_path.clone())
        .expect("component present in plan");

    let mut inflated = emit_report.clone();
    inflated.artifacts.push(EmittedArtifact {
        relative_path: wrapper_path,
        bytes,
    });
    inflated
}

#[test]
fn oversized_tier_b_wrapper_trips_bundle_ceiling_with_actionable_diff() {
    let temp = tempdir().unwrap();
    let manifest = manifest_with(vec![entry(2, "BloatedIsland", Tier::B, 200)]);
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let emit_report = emit_bundle_artifacts_to_dir(&plan, temp.path()).unwrap();

    // 142 KB — the same number the sprint plan's lodash example uses, so the
    // diff text matches the spec.
    let inflated = with_component_bytes(&emit_report, &plan, 2, 142 * 1024);

    let bundle_report = compute_bundle_byte_report(&inflated, &plan, &manifest);
    let budget = TierBudget::default();
    let report = evaluate_bundle_budget(&bundle_report, &budget);

    assert!(!report.is_ok(), "oversized wrapper must trip the ceiling");
    let violation = report
        .violations
        .iter()
        .find(|v| v.kind == ViolationKind::TierBBundleKbPerComponent)
        .expect("expected per-component bundle violation");
    assert_eq!(violation.limit, 1024);
    assert_eq!(violation.actual, 142 * 1024);

    let pretty = format_report_pretty(&report);
    // The exact diff text the sprint plan demo asks for.
    assert!(
        pretty.contains("tier-b component bundle exceeded"),
        "expected the bundle ceiling label, got:\n{pretty}"
    );
    assert!(pretty.contains("BloatedIsland"), "expected component name in diff");
    assert!(
        pretty.contains("Move heavy imports in BloatedIsland to Tier-C"),
        "expected the actionable hint, got:\n{pretty}"
    );
    assert!(
        pretty.contains("tier_b_bundle_max_kb_per_component = 142"),
        "expected the suggested ceiling raise"
    );
}

#[test]
fn raising_bundle_ceiling_via_budget_config_relaxes_the_gate() {
    let temp = tempdir().unwrap();
    let manifest = manifest_with(vec![entry(3, "BigIsland", Tier::B, 200)]);
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let emit_report = emit_bundle_artifacts_to_dir(&plan, temp.path()).unwrap();

    let inflated = with_component_bytes(&emit_report, &plan, 3, 50 * 1024);

    let bundle_report = compute_bundle_byte_report(&inflated, &plan, &manifest);

    // Default ceiling (1 KB) — should fail.
    let default_budget = TierBudget::default();
    assert!(!evaluate_bundle_budget(&bundle_report, &default_budget).is_ok());

    // Relax to 100 KB — should pass.
    let relaxed = TierBudget {
        defaults: BudgetDefaults {
            tier_b_bundle_max_kb_per_component: 100,
            ..BudgetDefaults::default()
        },
        routes: Default::default(),
    };
    assert!(evaluate_bundle_budget(&bundle_report, &relaxed).is_ok());
}

#[test]
fn tier_a_and_tier_c_wrappers_are_never_flagged_by_bundle_gate() {
    let temp = tempdir().unwrap();
    let manifest = manifest_with(vec![
        entry(1, "StaticHero", Tier::A, 100),
        entry(2, "StreamedFeed", Tier::C, 100),
    ]);
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let emit_report = emit_bundle_artifacts_to_dir(&plan, temp.path()).unwrap();

    let inflated = with_component_bytes(&emit_report, &plan, 1, 250 * 1024);
    let inflated = with_component_bytes(&inflated, &plan, 2, 250 * 1024);

    let bundle_report = compute_bundle_byte_report(&inflated, &plan, &manifest);
    let report = evaluate_bundle_budget(&bundle_report, &TierBudget::default());
    assert!(
        report.is_ok(),
        "Tier-A / Tier-C wrappers should be skipped; violations: {:?}",
        report.violations
    );
}

// Ensure the EmittedArtifact type is referenced so the dev-dep is
// recognised as used even in a future where the test file changes.
#[allow(dead_code)]
fn _shape(_: EmittedArtifact, _: PathBuf) {}
