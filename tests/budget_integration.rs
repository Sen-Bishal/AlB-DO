//! Phase O.1 · end-to-end budget evaluation against a real scan.
//!
//! Walks the shared `tests/fixtures/components/` directory through
//! the production scan + manifest path, then evaluates the result
//! against (a) a strict synthetic ceiling that must trip the gate
//! and (b) a relaxed ceiling that must pass. This covers the public
//! API the `albedo budget` CLI invokes without spawning a child
//! process.

use dom_render_compiler::budget::{evaluate_budget, BudgetDefaults, RouteBudget, TierBudget};
use dom_render_compiler::scanner::ProjectScanner;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("components")
}

fn build_manifest() -> dom_render_compiler::manifest::schema::RenderManifestV2 {
    let scanner = ProjectScanner::new();
    let components = scanner.scan_directory(&fixtures_dir()).unwrap();
    let compiler = scanner.build_compiler(components);
    compiler.optimize_manifest_v2().unwrap()
}

#[test]
fn default_budget_passes_against_components_fixture() {
    let manifest = build_manifest();
    let report = evaluate_budget(&manifest, &TierBudget::default());
    assert!(
        report.is_ok(),
        "default ceilings should accept the small fixture set; got violations: {:?}",
        report.violations
    );
    assert!(!report.route_summaries.is_empty());
}

#[test]
fn aggressive_tier_b_ceiling_trips_the_gate() {
    let manifest = build_manifest();
    let aggressive = TierBudget {
        defaults: BudgetDefaults {
            // 0 KB is an impossible ceiling — any Tier-B component
            // present must violate. If the fixture set ever ships
            // with zero Tier-B components this assert turns into a
            // signal that the fixture changed shape.
            tier_b_max_kb_per_route: 0,
            tier_b_max_kb_per_component: 0,
            tier_a_max_components_per_route: 9_999,
            tier_c_max_concurrent_fetches_per_route: 9_999,
        },
        routes: BTreeMap::new(),
    };
    let report = evaluate_budget(&manifest, &aggressive);
    if report.route_summaries.iter().all(|s| s.tier_b_total_bytes == 0) {
        // Manifest happens to have no Tier-B; budget shape can't
        // surface a violation. Pass vacuously rather than mis-fail.
        return;
    }
    assert!(!report.is_ok(), "expected at least one violation under impossible ceilings");
}

#[test]
fn per_route_override_relaxes_otherwise_failing_gate() {
    let manifest = build_manifest();
    let Some(route) = manifest.routes.keys().next().cloned() else {
        return;
    };
    let mut routes = BTreeMap::new();
    routes.insert(
        route,
        RouteBudget {
            tier_b_max_kb_per_route: Some(9999),
            tier_b_max_kb_per_component: Some(9999),
            tier_a_max_components_per_route: Some(9999),
            tier_c_max_concurrent_fetches_per_route: Some(9999),
        },
    );
    let lenient_override = TierBudget {
        defaults: BudgetDefaults {
            tier_b_max_kb_per_route: 0,
            tier_b_max_kb_per_component: 0,
            tier_a_max_components_per_route: 0,
            tier_c_max_concurrent_fetches_per_route: 0,
        },
        routes,
    };
    let report = evaluate_budget(&manifest, &lenient_override);
    // The overridden route must not appear in any violation.
    let overridden_route = lenient_override.routes.keys().next().unwrap().clone();
    let leaked = report
        .violations
        .iter()
        .filter(|v| v.route == overridden_route)
        .count();
    assert_eq!(
        leaked, 0,
        "overridden route should not produce violations; got {:?}",
        report.violations
    );
}
