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
use dom_render_compiler::bundler::{build_bundle_plan, BundlePlanOptions};
use dom_render_compiler::bundler::emit::{emit_bundle_artifacts_to_dir, EmittedArtifact};
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

#[test]
fn oversized_tier_b_wrapper_trips_bundle_ceiling_with_actionable_diff() {
    let temp = tempdir().unwrap();
    let manifest = manifest_with(vec![entry(2, "BloatedIsland", Tier::B, 200)]);
    let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());
    let emit_report = emit_bundle_artifacts_to_dir(&plan, temp.path()).unwrap();

    // The real emit produces a small wrapper (Phase J stub). For the
    // gate test we mutate the artifact report to simulate a heavy
    // import — the evaluator only cares about the reported bytes,
    // not the file's actual on-disk size. This isolates the test
    // from compiler-stage size estimates.
    //
    // Wrapper paths take the form `__albedo__/wrappers/{hash}_{slug}.mjs`
    // — derive the exact path from the plan so we don't depend on
    // hash stability across builds.
    let wrapper_path = plan
        .modules
        .iter()
        .find(|m| m.component_id == 2)
        .map(|m| m.wrapper_module_path.clone())
        .expect("BloatedIsland present in plan");

    let mut inflated = emit_report.clone();
    for artifact in inflated.artifacts.iter_mut() {
        if artifact.relative_path == wrapper_path {
            // 142 KB — the same number the sprint plan's lodash
            // example uses, so the diff text matches the spec.
            artifact.bytes = 142 * 1024;
        }
    }

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

    let wrapper_path = plan
        .modules
        .iter()
        .find(|m| m.component_id == 3)
        .map(|m| m.wrapper_module_path.clone())
        .expect("BigIsland present in plan");

    let mut inflated = emit_report.clone();
    for artifact in inflated.artifacts.iter_mut() {
        if artifact.relative_path == wrapper_path {
            artifact.bytes = 50 * 1024;
        }
    }

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

    let mut inflated = emit_report.clone();
    for artifact in inflated.artifacts.iter_mut() {
        if artifact.relative_path.ends_with(".mjs") && !artifact.relative_path.ends_with(".map") {
            artifact.bytes = 250 * 1024;
        }
    }

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
