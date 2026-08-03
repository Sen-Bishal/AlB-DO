//! Item 4.8 · a Tier-C island receives the props its parent passed it.
//!
//! ## What was broken
//!
//! An island's props existed for exactly one moment — while the parent
//! rendered `<Badge start={41} label="clicks" />` — and the renderer threw them
//! away at that moment. The island-skip branch in `runtime::eval::core`
//! returned the placeholder `<div>` before it ever read the element's
//! attributes, and every later consumer rendered the island **standalone, from
//! a module path**, by which point there was nothing left to pass. All three
//! rendered from `"{}"`:
//!
//! * the island's server-side SSR markup,
//! * the fine-grained reactive payload (binding mode),
//! * the client hydration payload.
//!
//! So `<Badge start={41} label="clicks" />` served `<span class="label"></span>`
//! and a counter that opened on `undefined`. `TierCNode` even had an
//! `initial_props` field — holding `{component_id, component_name}`, which is
//! identity, under a name that promises props, read by nothing.
//!
//! ## What these assert
//!
//! The capture, and then each of the three consumers separately — because they
//! are three code paths and fixing one says nothing about the others.

use dom_render_compiler::hydration::payload::build_hydration_payload;
use dom_render_compiler::hydration::plan::build_hydration_plan;
use dom_render_compiler::manifest::schema::RenderManifestV2;
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::scanner::ProjectScanner;
use std::path::PathBuf;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("island_props")
}

/// Build the manifest the way `albedo build` does — through the scanner, so
/// the parser sets `is_interactive` / `effect_profile` and the tiering actually
/// classifies `Badge` as Tier C. Hand-registering `Component`s skips the parse
/// and everything lands in Tier A, which would make this file assert nothing.
fn build_manifest_for(root: &PathBuf) -> RenderManifestV2 {
    let scanner = ProjectScanner::new();
    let compiler = scanner.scan_and_build(root).expect("scan");
    let result = compiler.optimize().expect("optimize");
    dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    )
}

fn build_manifest() -> RenderManifestV2 {
    build_manifest_for(&fixture_root())
}

fn badge_node(
    manifest: &RenderManifestV2,
) -> &dom_render_compiler::manifest::schema::TierCNode {
    manifest
        .routes
        .values()
        .flat_map(|route| route.tier_c.iter())
        .find(|node| node.component_id == "Badge")
        .expect("Badge must be a Tier-C island — it has a hook and a handler")
}

/// The capture itself: the parent's JSX attributes survive onto the manifest
/// node, which is the only place anything downstream can read them from.
#[test]
fn the_parents_props_reach_the_island_node() {
    let manifest = build_manifest();
    let props = &badge_node(&manifest).initial_props;

    assert_eq!(
        props.get("start").and_then(|v| v.as_f64()),
        Some(41.0),
        "`start={{41}}` must survive the parent render; got {props}"
    );
    assert_eq!(
        props.get("label").and_then(|v| v.as_str()),
        Some("clicks"),
        "`label=\"clicks\"` must survive the parent render; got {props}"
    );
}

/// Handlers are not data. A closure cannot be serialised into a payload, and
/// the island's own `onClick` is compiled into its bundle — shipping a
/// stringified closure as a prop would be worse than useless.
#[test]
fn event_handler_attributes_are_not_captured_as_props() {
    let manifest = build_manifest();
    let props = &badge_node(&manifest).initial_props;
    let object = props.as_object().expect("props must be an object");

    for key in object.keys() {
        assert!(
            !key.starts_with("on"),
            "`{key}` is a handler and must not be captured as a prop; got {props}"
        );
    }
}

/// Consumer 1 — the client hydration payload. Every island is seeded from its
/// own captured props now; before, only the route-entry island was seeded and
/// every nested one hydrated from `{}`, so a client island's first render
/// disagreed with the server markup it was supposed to be adopting.
#[test]
fn the_hydration_payload_seeds_the_island_with_its_captured_props() {
    let manifest = build_manifest();
    let badge_module = manifest
        .components
        .iter()
        .find(|c| c.name == "Badge")
        .map(|c| c.module_path.clone())
        .expect("Badge component entry");

    let plan = build_hydration_plan(&manifest, &badge_module);
    let payload = build_hydration_payload(&manifest, &plan, "{}").expect("payload builds");

    let island = payload
        .islands
        .iter()
        .find(|i| i.module_path == badge_module)
        .expect("Badge island in the payload");

    assert_eq!(
        island.props.get("start").and_then(|v| v.as_f64()),
        Some(41.0),
        "the client must mount from the same props the server rendered; got {}",
        island.props
    );
    assert_eq!(
        island.props.get("label").and_then(|v| v.as_str()),
        Some("clicks"),
        "got {}",
        island.props
    );
}

/// An island under a **Tier-B** parent gets its props too.
///
/// Worth asserting separately rather than assuming: a Tier-A parent's markup is
/// produced by `render_static_component_html` and a Tier-B parent's by
/// `render_tier_b_inline`, which are different callers reaching the evaluator
/// through different entry points. They happen to share the island-placeholder
/// guard installed around `traverse`, which is *why* one capture site serves
/// both — but "happens to" is exactly the kind of claim that stops being true
/// without anyone noticing.
#[test]
fn an_island_under_a_tier_b_parent_also_receives_its_props() {
    let manifest = build_manifest();

    let ticker = manifest
        .routes
        .values()
        .flat_map(|route| route.tier_c.iter())
        .find(|node| node.component_id == "Ticker")
        .expect("Ticker must be a Tier-C island");

    assert_eq!(
        ticker.initial_props.get("seed").and_then(|v| v.as_f64()),
        Some(7.0),
        "`seed={{7}}` was passed by an async (Tier-B) parent and must survive \
         the same way a static parent's props do; got {}",
        ticker.initial_props
    );
}

/// An island nobody passed anything to keeps rendering from nothing.
///
/// The pre-4.8 behaviour has to survive exactly, because it is the common case:
/// most islands take no props, and `Null` is what every consumer reads as "no
/// props".
#[test]
fn an_island_with_no_props_is_unchanged() {
    // The shipped scaffold is the control: its `<Counter />` is a Tier-C island
    // that nobody passes anything to, which is what most islands are. Using the
    // real starter rather than a bespoke fixture means this also fails if the
    // thing every new user runs first regresses.
    let scaffold = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scaffold/src");
    let manifest = build_manifest_for(&scaffold);

    let islands: Vec<_> = manifest
        .routes
        .values()
        .flat_map(|route| route.tier_c.iter())
        .collect();
    assert!(
        !islands.is_empty(),
        "the scaffold must still carry a Tier-C island, or this asserts nothing"
    );

    for node in islands {
        assert!(
            node.initial_props.is_null()
                || node.initial_props.as_object().is_some_and(|o| o.is_empty()),
            "`{}` is passed nothing, so it must carry no props — not the \
             identity blob `initial_props` used to hold; got {}",
            node.component_id,
            node.initial_props
        );
    }
}
