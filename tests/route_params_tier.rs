//! A dynamic route's page reads `params`, so it is rendered per request.
//!
//! 🔴 Until 2026-10-07 a `[slug]` page with no hooks was Tier A: rendered ONCE,
//! at build time, with `params` as `{}`. Every URL the route served got the same
//! markup, and the slug it was for appeared nowhere in it — `/blog/a` and
//! `/blog/b` were one page. `user` had been fixed for exactly this reason
//! (`reads_principal`); `params` is the other prop only a request has.
//!
//! Driven through the scan → tier → manifest path `albedo build` runs, so the
//! tier asserted here is the tier the server serves.

use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::scanner::{ProjectScanner, ScanMode};
use std::path::Path;

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Build a project holding one page at `rel` and return `(tier, reason, served)`,
/// where `served` says how the route's manifest carries the page: `"a"` baked
/// into `tier_a_root`, or `"b:<keys>"` rendered per request with those props.
fn page(rel: &str, source: &str) -> (Tier, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    write(&src, rel, source);

    let scanner = ProjectScanner::new();
    let report = scanner
        .scan_directory_with_mode(&src, ScanMode::Strict)
        .expect("scans");
    let compiler = scanner.build_compiler(report.components).with_project_root(dir.path());
    let (manifest, tiers) = compiler
        .optimize_manifest_v2_with_tier_report()
        .expect("manifest");

    let summary = tiers
        .components
        .iter()
        .find(|c| c.name == "Post")
        .expect("the page is a component");
    let route = manifest.routes.values().next().expect("the page is a route");
    let served = if let Some(node) = route.tier_b.iter().find(|n| n.component_id == "Post") {
        format!("b:{}", node.dynamic_prop_keys.join(","))
    } else if route.tier_a_root.iter().any(|n| n.component_id == "Post") {
        "a".to_string()
    } else {
        format!("neither: {route:?}")
    };
    (summary.tier, summary.reason.clone(), served)
}

const SLUG: &str = "routes/blog/[slug].tsx";

#[test]
fn a_dynamic_page_that_takes_params_is_rendered_per_request() {
    let (tier, reason, served) = page(
        SLUG,
        "export default function Post({ params }) {\n  return <h1>{params.slug}</h1>;\n}\n",
    );
    assert_eq!(tier, Tier::B, "{reason}");
    assert_eq!(reason, "reads route `params` - rendered per request, no component JS");
    assert_eq!(served, "b:params", "the server must resolve `params` for it per request");
}

/// `props.params`, a rest element, a computed key: shapes the signature scan
/// cannot see into round toward "reads it" — slower and correct.
#[test]
fn an_opaque_signature_rounds_toward_reading_params() {
    for signature in ["props", "{ ...rest }", "{ [key]: value }", "{ params } = {}"] {
        let (tier, reason, _) = page(
            SLUG,
            &format!("export default function Post({signature}) {{\n  return <h1>x</h1>;\n}}\n"),
        );
        assert_eq!(tier, Tier::B, "`({signature})` must not be baked: {reason}");
    }
}

/// A page that provably cannot reach `params` keeps its build-time bake.
#[test]
fn a_dynamic_page_that_cannot_reach_params_stays_tier_a() {
    for signature in ["", "{ title }"] {
        let (tier, reason, served) = page(
            SLUG,
            &format!("export default function Post({signature}) {{\n  return <h1>static</h1>;\n}}\n"),
        );
        assert_eq!(tier, Tier::A, "`({signature})`: {reason}");
        assert_eq!(served, "a");
    }
}

/// Outside a `[param]` route there are no route params to read; nothing changes.
#[test]
fn a_static_route_is_unaffected() {
    let (tier, reason, served) = page(
        "routes/about.tsx",
        "export default function Post({ params }) {\n  return <h1>about</h1>;\n}\n",
    );
    assert_eq!(tier, Tier::A, "{reason}");
    assert_eq!(served, "a");
}
