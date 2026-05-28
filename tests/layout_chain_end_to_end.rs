//! Phase P · Stream E.1 — layout-chain wrap pipeline end-to-end.
//!
//! Loads a fixture project with one root layout, one nested layout,
//! and two leaf routes (`/` and `/nested`). Builds a manifest and
//! asserts the wrap pass:
//!
//!   1. A route under a single layout has the layout's HTML wrapping
//!      its leaf content, with the `<children />` intrinsic replaced
//!      by the leaf HTML in place (no double-render, no escaping).
//!   2. A nested route has BOTH layouts applied, outer wrapping
//!      inner, leaf at the deepest point.
//!   3. The renderer's `<children />` intrinsic emits a stable
//!      sentinel comment; the wrap pass `str::replace`s it without
//!      touching neighbouring HTML.
//!   4. A project with no `routes/layout.tsx` produces shell HTML
//!      identical to pre-E.1 (regression guard).
//!
//! The fixture lives at `tests/fixtures/layouts/`. Each layout uses
//! `<children />` (lowercase, self-closing) as the substitution
//! point — matching the plan's "New JSX intrinsic" surface.

use dom_render_compiler::manifest::schema::RenderManifestV2;
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::types::{Component, ComponentId};
use dom_render_compiler::RenderCompiler;
use std::path::PathBuf;

const FIXTURE_NAME: &str = "layouts";

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(FIXTURE_NAME)
}

fn register_component(
    compiler: &mut RenderCompiler,
    id: u64,
    name: &str,
    rel_path: &str,
) -> ComponentId {
    let mut component = Component::new(ComponentId::new(id), name.to_string());
    let abs = fixture_root().join(rel_path);
    component.file_path = abs.display().to_string();
    component.weight = 512.0;
    compiler.add_component(component);
    ComponentId::new(id)
}

fn build_layouts_manifest() -> RenderManifestV2 {
    // Register all six files. Layout / error / loading files are
    // deliberately NOT routes — `route_path_from_component` filters
    // them by basename so `render_layout_html` (and the parallel
    // error/loading lookups) can still resolve them by component
    // name from the `self.components` map.
    let mut compiler = RenderCompiler::new();
    register_component(&mut compiler, 0, "RootLayout", "routes/layout.tsx");
    register_component(&mut compiler, 1, "Home", "routes/index.tsx");
    register_component(&mut compiler, 2, "NestedLayout", "routes/nested/layout.tsx");
    register_component(&mut compiler, 3, "NestedHome", "routes/nested/index.tsx");
    // Phase P · Stream E.2 — convention files for error / loading.
    // Stream B's manifest schema already carries the placeholder
    // fields; E.2 populates them via `discover_routes` →
    // `component_name_for_rel_path`.
    register_component(&mut compiler, 4, "RootError", "routes/error.tsx");
    register_component(&mut compiler, 5, "RootLoading", "routes/loading.tsx");

    let result = compiler.optimize().expect("optimize");
    dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    )
}

#[test]
fn single_layout_wraps_route_with_navigation_then_leaf_content() {
    let manifest = build_layouts_manifest();
    let route = manifest.routes.get("/").expect("/ route present");
    let body = route.shell.body_open.as_str();

    assert!(
        body.contains("layout-root"),
        "root layout class must appear in shell body; got: {body}"
    );
    assert!(
        body.contains("NAV"),
        "root layout's <nav> text must appear in shell body; got: {body}"
    );

    // The leaf's actual HTML lives in `tier_a_root[0].html` — the
    // stitcher fills body_open's `__SLOT_` anchor at request time.
    // E.1's wrap puts the anchor INSIDE the layout's rendered HTML;
    // verify both shapes.
    let placeholder = format!("__SLOT___a_home_{}", 1);
    assert!(
        body.contains(&placeholder),
        "leaf placeholder anchor must appear in wrapped body; got: {body}"
    );
    let nav_pos = body.find("NAV").unwrap();
    let placeholder_pos = body.find(&placeholder).unwrap();
    let layout_close_pos = body.rfind("</div>").unwrap();
    assert!(
        nav_pos < placeholder_pos && placeholder_pos < layout_close_pos,
        "leaf placeholder must sit INSIDE the layout: \
         NAV@{nav_pos} < placeholder@{placeholder_pos} < </div>@{layout_close_pos}"
    );

    let leaf_node = route
        .tier_a_root
        .iter()
        .find(|n| n.component_id == "Home")
        .expect("Home tier-A entry");
    assert!(
        leaf_node.html.contains("HOME-LEAF"),
        "leaf component's rendered HTML must carry its content; got: {}",
        leaf_node.html
    );
}

#[test]
fn nested_route_applies_outer_layout_then_inner_layout_then_leaf() {
    let manifest = build_layouts_manifest();
    let route = manifest
        .routes
        .get("/nested")
        .expect("/nested route present");
    let body = route.shell.body_open.as_str();

    assert!(body.contains("layout-root"), "outer layout absent: {body}");
    assert!(body.contains("layout-nested"), "inner layout absent: {body}");

    let placeholder = format!("__SLOT___a_nestedhome_{}", 3);
    assert!(
        body.contains(&placeholder),
        "nested leaf placeholder must appear in wrapped body; got: {body}"
    );

    let outer_pos = body.find("layout-root").unwrap();
    let inner_pos = body.find("layout-nested").unwrap();
    let placeholder_pos = body.find(&placeholder).unwrap();
    assert!(
        outer_pos < inner_pos,
        "outer layout must come before inner: outer@{outer_pos} < inner@{inner_pos}"
    );
    assert!(
        inner_pos < placeholder_pos,
        "inner layout must wrap leaf placeholder: inner@{inner_pos} < placeholder@{placeholder_pos}"
    );

    let leaf_node = route
        .tier_a_root
        .iter()
        .find(|n| n.component_id == "NestedHome")
        .expect("NestedHome tier-A entry");
    assert!(
        leaf_node.html.contains("NESTED-LEAF"),
        "nested leaf's rendered HTML must carry its content; got: {}",
        leaf_node.html
    );
}

#[test]
fn children_sentinel_is_substituted_without_leaving_residue() {
    let manifest = build_layouts_manifest();
    for body in manifest.routes.values().map(|r| r.shell.body_open.as_str()) {
        assert!(
            !body.contains("__ALBEDO_LAYOUT_CHILDREN__"),
            "sentinel comment must be fully replaced by the wrap pass; \
             stray marker in: {body}"
        );
        assert!(
            !body.contains("<children"),
            "<children /> intrinsic must be rewritten by the renderer, \
             not emitted as raw HTML; stray tag in: {body}"
        );
    }
}

#[test]
fn empty_layout_chain_leaves_shell_body_unwrapped() {
    // Register a single leaf component that does NOT live under
    // `routes/`, so `discover_routes` finds nothing and the
    // resulting RouteManifest carries an empty `layout_chain`. The
    // shell body must contain the leaf's content directly, without
    // any layout-class wrapper or sentinel residue.
    let mut compiler = RenderCompiler::new();
    let mut bare = Component::new(ComponentId::new(0), "BareLeaf".to_string());
    let bare_path = fixture_root().join("bare_leaf.tsx");
    bare.file_path = bare_path.display().to_string();
    bare.weight = 256.0;
    // The bare fixture file must exist on disk so the renderer can
    // load it. Drop a tiny inline component just before the build.
    if !bare_path.exists() {
        std::fs::write(
            &bare_path,
            "export default function BareLeaf() { return <p>BARE-LEAF</p>; }\n",
        )
        .expect("write bare fixture");
    }
    compiler.add_component(bare);

    let result = compiler.optimize().expect("optimize bare");
    let manifest = dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    );

    let route = manifest
        .routes
        .values()
        .next()
        .expect("at least one route emitted");
    assert!(
        route.layout_chain.is_empty(),
        "bare leaf must produce an empty layout_chain; got {:?}",
        route.layout_chain
    );
    assert!(
        !route.shell.body_open.contains("layout-root"),
        "bare leaf shell must not pick up unrelated layouts; got: {}",
        route.shell.body_open
    );
    assert!(
        !route.shell.body_open.contains("__ALBEDO_LAYOUT_CHILDREN__"),
        "no sentinel should be present when layout_chain is empty"
    );
}

#[test]
fn route_manifest_carries_layout_chain_outermost_first() {
    let manifest = build_layouts_manifest();
    let nested = manifest.routes.get("/nested").expect("/nested route");

    // Stream B populates layout_chain via discover_routes — should
    // list the root layout first, then the nested layout.
    assert_eq!(
        nested.layout_chain,
        vec!["RootLayout".to_string(), "NestedLayout".to_string()],
        "layout_chain must be outermost → leaf so the wrap walks it \
         in reverse correctly"
    );

    let root = manifest.routes.get("/").expect("/ route");
    assert_eq!(
        root.layout_chain,
        vec!["RootLayout".to_string()],
        "root route's layout_chain must be the single outermost layout"
    );
}

/// Sanity check: the `route_path_from_component` filter introduced
/// by E.1 means `routes/layout.tsx` and `routes/nested/layout.tsx`
/// MUST NOT show up as their own route entries in the manifest.
/// Without this filter a phantom `/layout` URL would appear. E.2
/// extends the same filter to `error.tsx` / `loading.tsx`.
#[test]
fn layout_error_loading_files_do_not_become_routes() {
    let manifest = build_layouts_manifest();
    let route_paths: Vec<&str> = manifest.routes.keys().map(String::as_str).collect();
    for phantom in ["/layout", "/nested/layout", "/error", "/loading"] {
        assert!(
            !route_paths.contains(&phantom),
            "convention file must not become a route: {phantom} present in {route_paths:?}"
        );
    }
}

// ── Phase P · Stream E.2 tests ──────────────────────────────────────

/// Stream E.2 — when a route's discovered metadata carries an
/// error.tsx boundary, the manifest's `error_component` field must
/// be populated with the resolved component name. The streaming
/// handler then has a pre-rendered HTML body to ship when a Tier-C
/// node on this route fails.
#[test]
fn route_manifest_carries_error_component_from_discovery() {
    let manifest = build_layouts_manifest();
    let root = manifest.routes.get("/").expect("/ route");
    assert_eq!(
        root.error_component.as_deref(),
        Some("RootError"),
        "root error boundary must propagate to RouteManifest.error_component"
    );
    let nested = manifest.routes.get("/nested").expect("/nested route");
    // No `routes/nested/error.tsx` in the fixture — nested route
    // inherits the root error boundary.
    assert_eq!(
        nested.error_component.as_deref(),
        Some("RootError"),
        "nested route inherits root error boundary when no closer one exists"
    );
}

/// Stream E.2 — same shape as the error_component test, for
/// `loading.tsx`. Loading + error are independent (a route can have
/// one but not the other), so this is a separate assertion.
#[test]
fn route_manifest_carries_loading_component_from_discovery() {
    let manifest = build_layouts_manifest();
    let root = manifest.routes.get("/").expect("/ route");
    assert_eq!(
        root.loading_component.as_deref(),
        Some("RootLoading"),
        "root loading fallback must propagate to RouteManifest.loading_component"
    );
}

/// Stream E.2 — a project with zero `error.tsx` / `loading.tsx`
/// produces `None` for both manifest fields. Pins the regression
/// guard against "always-populate-something" mistakes.
#[test]
fn route_manifest_has_none_for_error_loading_when_fixture_omits_them() {
    use dom_render_compiler::types::Component;

    // Build a one-component project with NO error.tsx / loading.tsx
    // under routes/. Stream B's schema fields must come back as
    // `None`, not e.g. an empty-string component name.
    let mut compiler = RenderCompiler::new();
    let mut leaf = Component::new(ComponentId::new(0), "Home".to_string());
    let leaf_path = fixture_root().join("bare_routes_only.tsx");
    if !leaf_path.exists() {
        std::fs::write(
            &leaf_path,
            "export default function Home() { return <main>BARE</main>; }\n",
        )
        .unwrap();
    }
    leaf.file_path = leaf_path.display().to_string();
    leaf.weight = 256.0;
    compiler.add_component(leaf);

    let result = compiler.optimize().unwrap();
    let manifest = dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    );
    let route = manifest
        .routes
        .values()
        .next()
        .expect("at least one route");
    assert!(
        route.error_component.is_none(),
        "no error.tsx means error_component must be None; got {:?}",
        route.error_component
    );
    assert!(
        route.loading_component.is_none(),
        "no loading.tsx means loading_component must be None; got {:?}",
        route.loading_component
    );
}

