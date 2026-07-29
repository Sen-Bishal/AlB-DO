//! The two renderers must agree on `data-albedo-id`.
//!
//! A Tier-B component's opcode frame is built by the **pure-Rust** renderer at
//! build time; the markup a request actually serves is rendered by **QuickJS**.
//! The frame addresses elements by `stable_id`, so if the two renderers number
//! elements differently — or one of them does not number them at all — every
//! `BindEvent` in the frame targets an element that does not exist,
//! `_requireNode` throws, and the whole frame is dropped. No Tier-B `onClick`
//! fires, and nothing anywhere says why.
//!
//! That is what shipped: QuickJS emitted no anchors whatsoever. It stayed hidden
//! because the only Tier-B components in existence were whole routes or
//! form-driven ones, and a `<form action="…">` binds by attribute through
//! `link-forms.js` without ever touching a `BindEvent`.
//!
//! So this asserts the agreement directly, at the one place it can be checked
//! cheaply: same fixture, both renderers, compare the ids. A unit test of either
//! renderer alone cannot see this — each one is self-consistent.

use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

const MODULE_SPEC: &str = "Component.tsx";

fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

/// Every `data-albedo-id` in `html`, in document order.
fn anchors(html: &str) -> Vec<String> {
    let needle = "data-albedo-id=\"";
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(needle) {
        rest = &rest[start + needle.len()..];
        let Some(end) = rest.find('"') else { break };
        out.push(rest[..end].to_string());
        rest = &rest[end..];
    }
    out
}

fn rust_render(name: &str) -> (String, Vec<Instruction>) {
    let project = CompiledProject::load_from_dir(fixture_dir(name)).expect("fixture compiles");
    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let out = render_entry_with_bindings(
        &project,
        MODULE_SPEC,
        &Value::Object(Default::default()),
        &slots,
        &RenderOptions { hook_compile: true },
    )
    .expect("renders");
    (out.html, out.opcodes)
}

fn quickjs_render(name: &str) -> String {
    let source = std::fs::read_to_string(fixture_dir(name).join(MODULE_SPEC)).expect("fixture read");
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    // The spec is what makes the ids comparable at all: it is hashed into every
    // id, so passing the absolute path here (as the server's manifest carries)
    // would produce a self-consistent but *different* numbering.
    engine
        .load_module_with_spec(MODULE_SPEC, &source, Some(MODULE_SPEC))
        .expect("module loads");
    engine
        .render_component_with_host(MODULE_SPEC, "{}", "")
        .expect("renders")
        .html
}

/// The headline: same component, both renderers, identical anchor sequence.
#[test]
fn both_renderers_number_the_same_elements_the_same_way() {
    for fixture in ["counter", "fetching_handler", "nested_island"] {
        let (rust_html, _) = rust_render(fixture);
        let quickjs_html = quickjs_render(fixture);

        let rust_ids = anchors(&rust_html);
        let quickjs_ids = anchors(&quickjs_html);

        assert!(
            !rust_ids.is_empty(),
            "{fixture}: the pure-Rust renderer must stamp anchors; got {rust_html}"
        );
        assert_eq!(
            rust_ids, quickjs_ids,
            "{fixture}: anchor ids diverged.\n  rust:    {rust_html}\n  quickjs: {quickjs_html}"
        );
    }
}

/// The consequence, stated as the thing that actually broke: the element a
/// `BindEvent` names has to exist in the markup QuickJS serves.
#[test]
fn the_bindevent_target_exists_in_the_quickjs_markup() {
    let (_, opcodes) = rust_render("fetching_handler");
    let target = opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { stable_id, .. } => Some(stable_id.0),
            _ => None,
        })
        .expect("the fixture binds a click handler");

    let quickjs_html = quickjs_render("fetching_handler");
    assert!(
        anchors(&quickjs_html).contains(&target.to_string()),
        "BindEvent targets {target}, which is nowhere in the served markup — \
         this is the state in which no Tier-B onClick binds.\n  {quickjs_html}"
    );
}

/// Pre-order, not document-completion order. The counter is shared across the
/// render and a parent must take its id before its children, or a component
/// with any nesting at all drifts after the first element — which is exactly
/// what a bottom-up stamp inside `h()` would have produced.
#[test]
fn a_parent_is_numbered_before_its_children() {
    let quickjs_html = quickjs_render("nested_island");
    let ids = anchors(&quickjs_html);
    assert_eq!(ids.len(), 3, "div, button, span; got {quickjs_html}");

    // The wrapper is first in the document and must also be first in the
    // numbering. A bottom-up stamp — the obvious place, inside `h()` — would
    // number it LAST, because by then its children are already stringified.
    // That is why the stamp is a JSX attribute: argument evaluation builds the
    // parent's props object before any child's `h(…)` call runs.
    let div_first = quickjs_html.find(&ids[0]).expect("first id present");
    let button_second = quickjs_html.find(&ids[1]).expect("second id present");
    assert!(
        div_first < button_second,
        "the wrapper must carry the first id; got {quickjs_html}"
    );

    let (rust_html, _) = rust_render("nested_island");
    assert_eq!(
        ids,
        anchors(&rust_html),
        "and the Rust renderer, which allocates before rendering children, must \
         produce that same order"
    );
}
