//! AUTH § 8.1.3 · a route's actions reach the manifest attributed to that route.
//!
//! ## Why this test exists
//!
//! `RouteManifest.action_ids` is what the server's action gate consults to
//! answer *which route declared this, and does that route require a principal?*
//! For its whole life the builder's `collect_route_action_ids` was
//! `Vec::new()` — a stub with a comment promising a later stream would fill it —
//! so the field was empty in **every** manifest the compiler had ever emitted.
//!
//! 🪤 **That is why this is an end-to-end test over a real source tree and not a
//! unit test on the collector.** A gate built on the stub compiles, passes unit
//! tests that hand it a populated set, passes an integration test against a
//! hand-written manifest — and refuses nothing in a real build. It is the third
//! instance of that shape in this codebase (APERTURE's egress check nothing
//! routed to; P2's CSRF token no served form could carry), and each time the
//! test that would have caught it is the one that starts from what the compiler
//! actually produces.
//!
//! So: compile a fixture, read the manifest, and assert the attribution is
//! there. If `collect_route_action_ids` ever returns to a stub, this fails.

use dom_render_compiler::manifest::schema::{RenderManifestV2, RouteAuth};
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::transforms::allocate_form_action_id;
use dom_render_compiler::types::{Component, ComponentId};
use dom_render_compiler::RenderCompiler;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("route_action_attribution")
}

fn build_manifest() -> RenderManifestV2 {
    let mut compiler = RenderCompiler::new();
    for (id, name, rel) in [
        (0_u64, "Home", "routes/index.tsx"),
        (1, "Dash", "routes/dash.tsx"),
    ] {
        let mut component = Component::new(ComponentId::new(id), name.to_string());
        component.file_path = fixture_root().join(rel).display().to_string();
        component.weight = 512.0;
        compiler.add_component(component);
    }

    let result = compiler.optimize().expect("optimize");
    dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    )
}

fn action_names(manifest: &RenderManifestV2, route: &str) -> Vec<String> {
    let mut names: Vec<String> = manifest
        .routes
        .get(route)
        .unwrap_or_else(|| panic!("route {route} present in {:?}", manifest.routes.keys()))
        .action_ids
        .iter()
        .map(|entry| entry.name.clone())
        .collect();
    names.sort();
    names
}

/// The headline, and the assertion the stub failed: a route's declared actions
/// are in its manifest entry at all.
#[test]
fn a_routes_declared_actions_reach_its_manifest_entry() {
    let manifest = build_manifest();

    assert_eq!(
        action_names(&manifest, "/dash"),
        vec!["dash_purge".to_string(), "dash_write".to_string()],
        "a route that declares two actions must carry both — an empty list here \
         silently disables the § 8.1.3 action gate for the whole project"
    );
}

/// Attribution, not a project-wide list. The gate applies **this** route's
/// declaration, so an action must land under the route that wrote it and no
/// other — otherwise one gated route would gate every action in the app.
#[test]
fn actions_are_attributed_to_the_route_that_declares_them() {
    let manifest = build_manifest();

    assert_eq!(
        action_names(&manifest, "/"),
        vec!["sign_guestbook".to_string()],
        "the public route carries its own action and not the gated route's"
    );
    assert!(
        !action_names(&manifest, "/dash").contains(&"sign_guestbook".to_string()),
        "and the gated route does not acquire the public route's action"
    );
}

/// The id is the wire id, not a fresh naming scheme. If these ever diverge the
/// gate would look up an id the dispatcher never presents — refusing nothing,
/// silently, which is the failure this whole area keeps producing.
#[test]
fn the_recorded_id_is_the_one_the_dispatcher_presents() {
    let manifest = build_manifest();
    let entry = manifest
        .routes
        .get("/dash")
        .expect("/dash present")
        .action_ids
        .iter()
        .find(|entry| entry.name == "dash_write")
        .expect("dash_write recorded");

    assert_eq!(entry.action_id, allocate_form_action_id("dash_write"));
}

/// The gate and the actions it governs are read off one module. This pins that
/// they arrive together — a route with `auth = "required"` whose `action_ids`
/// came back empty would look correctly gated and be wide open on the action
/// path.
#[test]
fn the_gate_and_the_actions_it_governs_arrive_together() {
    let manifest = build_manifest();
    let dash = manifest.routes.get("/dash").expect("/dash present");

    assert_eq!(dash.auth, RouteAuth::Required);
    assert!(!dash.action_ids.is_empty());

    let home = manifest.routes.get("/").expect("/ present");
    assert_eq!(home.auth, RouteAuth::Public);
}
