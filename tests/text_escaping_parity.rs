//! Text-position escaping, asserted on BOTH renderers.
//!
//! ## The defect this pins
//!
//! `ComponentProject::render_children` took an `escape_expr_children: bool`.
//! Both call sites — fragments and elements — passed `false`, so the escaping
//! branch was unreachable and every JSX expression child was emitted raw. The
//! attribute path escaped correctly the whole time, which is why the hole
//! survived: the obvious probe (`title={x}`) came back clean.
//!
//! Nothing in `tests/` asserted text escaping before this file. The string
//! `&lt;script` appeared nowhere in the suite, and the 90-case conformance
//! corpus had no case with a markup-significant character in a text position,
//! so neither the goldens nor the two-renderer gate could see it.
//!
//! ## Why it is a security defect and not a cosmetic one
//!
//! The evaluator renders the structural contract: markup baked into the
//! manifest and shipped with no client code that could correct it afterwards
//! (`development-plan/CONFORMANCE.md` § 1). Route params arrive as props on this
//! path, so `{params.id}` put a URL segment into the page unescaped.
//!
//! ## Why the one-word fix was wrong
//!
//! Flipping both call sites to `true` made `xs.map(x => <li/>)` render as
//! `&lt;li&gt;a&lt;/li&gt;` — an expression child whose value is a mapped JSX
//! array arrives as ALREADY-RENDERED markup. The evaluator now carries that
//! distinction the way QuickJS's `h()` shim does, via a marker
//! (`component::make_html_value` / `AlbedoHtml`), and `map_iteration` below is
//! the case that fails if the distinction is ever dropped again.

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Every character the two escape rules disagree about, in one value: `<`, `>`
/// and `&` must be escaped in text; `"` and `'` must NOT be (neither can
/// terminate a text node, and escaping them would diverge from QuickJS, which
/// leaves them alone too).
const HOSTILE: &str = "<script>alert(1)</script> & \"quoted\" 'single'";

fn fixture(group: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(group)
        .join(name)
}

fn slots() -> SessionSlotView {
    SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()))
}

/// The pure-Rust evaluator's markup for `entry`.
fn render_evaluator(project: &CompiledProject, entry: &str, props: &Value) -> String {
    render_entry_with_bindings(
        project,
        entry,
        props,
        &slots(),
        &RenderOptions { hook_compile: false },
    )
    .expect("evaluator render succeeds")
    .html
}

/// The same component's markup through the real JS engine.
fn render_quickjs(project: &CompiledProject, entry: &str, props: &Value) -> String {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    project
        .render_entry_quickjs(&mut engine, entry, props, &slots())
        .expect("quickjs render succeeds")
        .html
}

/// A props-supplied string containing `< > & " '` is escaped in text position,
/// escaped harder in attribute position, and the two renderers agree byte for
/// byte about all of it.
#[test]
fn a_hostile_props_string_is_escaped_in_text_position_by_both_renderers() {
    let project = CompiledProject::load_from_dir(fixture(
        "render_quickjs",
        "escaped_props_text",
    ))
    .expect("fixture compiles");
    let props = json!({ "bio": HOSTILE });

    let evaluator = render_evaluator(&project, "Component.tsx", &props);
    let quickjs = render_quickjs(&project, "Component.tsx", &props);

    // Conformance first: a shared mistake is still a divergence from HTML, but
    // a divergence between the two renderers is a defect no assertion about
    // either one alone would catch.
    assert_eq!(
        evaluator, quickjs,
        "the two renderers must produce identical bytes for the same props"
    );

    // Text position: `& < >` escaped, quotes left alone.
    let expected_text = "&lt;script&gt;alert(1)&lt;/script&gt; &amp; \"quoted\" 'single'";
    assert!(
        evaluator.contains(expected_text),
        "expected the escaped text {expected_text:?} in: {evaluator}"
    );

    // Attribute position: the same, plus `"` — otherwise the value closes the
    // attribute it sits in.
    let expected_attr =
        "title=\"&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;quoted&quot; 'single'\"";
    assert!(
        evaluator.contains(expected_attr),
        "expected the escaped attribute {expected_attr:?} in: {evaluator}"
    );

    // The blunt statement of the bug: no live `<script>` reached the page from
    // either renderer, in either position.
    for (label, html) in [("evaluator", &evaluator), ("quickjs", &quickjs)] {
        assert!(
            !html.contains("<script>"),
            "{label} emitted a live <script> tag from props: {html}"
        );
    }
}

/// The other half of the contract: markup the renderer itself produced must
/// still pass through raw. `xs.map(x => <li/>)` is the case that breaks if
/// escaping is applied to expression children indiscriminately.
#[test]
fn renderer_produced_markup_still_passes_through_unescaped() {
    let project =
        CompiledProject::load_from_dir(fixture("jsx_matrix", "map_iteration")).expect("compiles");
    let props = json!({});

    let evaluator = render_evaluator(&project, "Component.tsx", &props);
    assert!(
        evaluator.contains("<li") && !evaluator.contains("&lt;li"),
        "a mapped JSX array is markup, not text, and must not be escaped: {evaluator}"
    );

    // And through props: children handed to a component arrive as values, so the
    // same distinction has to survive the trip through `props.children`.
    let project = CompiledProject::load_from_dir(fixture("render_quickjs", "list"))
        .expect("list fixture compiles");
    let html = render_evaluator(
        &project,
        "Component.tsx",
        &json!({ "items": ["a & b", "<c>"] }),
    );
    assert!(
        html.contains("<li") && !html.contains("&lt;li"),
        "the <li> wrappers are markup: {html}"
    );
    assert!(
        html.contains("a &amp; b") && html.contains("&lt;c&gt;"),
        "the mapped ITEMS are data and must be escaped: {html}"
    );
}
