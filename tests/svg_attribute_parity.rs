//! SVG attribute names, and the three renderers that have to agree on them.
//!
//! ## What this is guarding
//!
//! An SVG presentation attribute is **case- and spelling-sensitive in a way the
//! browser answers by ignoring you**. `strokeWidth="2"` on an `<svg>` is parsed
//! as `strokewidth`, which is not an attribute, so the stroke silently renders
//! at its default. `viewBox` must keep its camelCase or the icon has no
//! coordinate system at all. There is no error anywhere — the picture is just
//! wrong.
//!
//! Three renderers emit these: the pure-Rust one (Tier A/B static markup),
//! the QuickJS `h` shim (Tier B per-request, Tier C island SSR), and
//! `assets/albedo-client.js` (hydration and client updates). The first two are
//! required to agree **byte-for-byte** — the QuickJS shim's own comment says so
//! about the void-element spelling — because hydration *adopts* the server's
//! DOM rather than rebuilding it.
//!
//! Until `runtime::jsx_attributes` existed the rule lived in three places: a
//! `match` in Rust, a ternary chain in the QuickJS prelude, and a pair of `if`s
//! in the client. **None of the three had the SVG half**, so hand-authored SVG
//! in a Tier-A component had been shipping inert attributes since Tier A
//! existed. This test starts from what the renderers actually produce, which is
//! the rule the `action_ids` stub cost us.
//!
//! The client's copy is checked separately, by
//! `runtime::jsx_attributes::tests::the_client_runtime_table_matches_this_one`,
//! because hand-written JavaScript cannot be generated from the Rust table.

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::jsx_attributes::JSX_ATTRIBUTE_RENAMES;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

const MODULE_SPEC: &str = "Component.tsx";
const FIXTURE: &str = "svg_icon";

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(FIXTURE)
}

fn rust_render() -> String {
    let project = CompiledProject::load_from_dir(fixture_dir()).expect("fixture compiles");
    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    render_entry_with_bindings(
        &project,
        MODULE_SPEC,
        &Value::Object(Default::default()),
        &slots,
        &RenderOptions { hook_compile: true },
    )
    .expect("renders")
    .html
}

fn quickjs_render() -> String {
    let source = std::fs::read_to_string(fixture_dir().join(MODULE_SPEC)).expect("fixture read");
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    engine
        .load_module_with_spec(MODULE_SPEC, &source, Some(MODULE_SPEC))
        .expect("module loads");
    engine
        .render_component_with_host(MODULE_SPEC, "{}", "")
        .expect("renders")
        .html
}

/// Every attribute name in `html`, in document order.
fn attribute_names(html: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes: Vec<char> = html.chars().collect();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != '<' {
            index += 1;
            continue;
        }
        // Walk one tag, collecting `name=` pairs.
        index += 1;
        while index < bytes.len() && bytes[index] != '>' {
            if bytes[index].is_whitespace() {
                index += 1;
                let start = index;
                while index < bytes.len()
                    && !bytes[index].is_whitespace()
                    && bytes[index] != '='
                    && bytes[index] != '>'
                    && bytes[index] != '/'
                {
                    index += 1;
                }
                if index < bytes.len() && bytes[index] == '=' && index > start {
                    names.push(bytes[start..index].iter().collect::<String>());
                    // Skip the quoted value so `a="b c"` does not look like two
                    // attributes.
                    index += 1;
                    if index < bytes.len() && (bytes[index] == '"' || bytes[index] == '\'') {
                        let quote = bytes[index];
                        index += 1;
                        while index < bytes.len() && bytes[index] != quote {
                            index += 1;
                        }
                        index += 1;
                    }
                }
                continue;
            }
            index += 1;
        }
    }
    names
}

/// The headline: same component, both server renderers, identical attribute
/// spellings in identical order.
#[test]
fn both_server_renderers_spell_svg_attributes_the_same_way() {
    let rust_html = rust_render();
    let quickjs_html = quickjs_render();

    let rust_attrs = attribute_names(&rust_html);
    let quickjs_attrs = attribute_names(&quickjs_html);

    assert!(
        !rust_attrs.is_empty(),
        "the fixture must emit attributes; got {rust_html}"
    );
    assert_eq!(
        rust_attrs, quickjs_attrs,
        "attribute spellings diverged between the renderers.\n  \
         rust:    {rust_html}\n  quickjs: {quickjs_html}"
    );
}

/// The bug this closes, stated as the thing a browser does: an SVG presentation
/// attribute must be hyphenated, or it is not an attribute at all.
#[test]
fn presentation_attributes_are_hyphenated_by_both_renderers() {
    for html in [rust_render(), quickjs_render()] {
        for hyphenated in [
            "stroke-width",
            "stroke-linecap",
            "stroke-linejoin",
            "stroke-dasharray",
            "fill-rule",
            "clip-rule",
            "pointer-events",
            "text-anchor",
            "font-family",
            "font-size",
            "letter-spacing",
        ] {
            assert!(
                html.contains(&format!("{hyphenated}=")),
                "{hyphenated} missing — the browser ignores the camelCase form: {html}"
            );
        }
        for camel in ["strokeWidth", "fillRule", "textAnchor", "fontSize"] {
            assert!(
                !html.contains(&format!("{camel}=")),
                "{camel} must not survive as an attribute name: {html}"
            );
        }
    }
}

/// `viewBox` is the other direction and the easy one to break while fixing the
/// first: it is *already* camelCase in SVG and must not be hyphenated or
/// lowercased.
#[test]
fn already_camel_case_svg_attributes_are_left_alone() {
    for html in [rust_render(), quickjs_render()] {
        assert!(html.contains("viewBox="), "viewBox must survive: {html}");
        assert!(
            html.contains("preserveAspectRatio="),
            "preserveAspectRatio must survive: {html}"
        );
        assert!(!html.contains("view-box="), "{html}");
    }
}

/// The HTML renames that were already correct must stay correct — this table
/// grew, and a regression here disconnects every `<label>` on every page.
#[test]
fn the_html_renames_still_hold_in_both_renderers() {
    for html in [rust_render(), quickjs_render()] {
        assert!(html.contains("class=\"icon-button\""), "{html}");
        assert!(!html.contains("className="), "{html}");
    }
}

/// Nothing in the shared table may pass through as itself. A renderer that
/// forgot to consult the table would still satisfy the fixture-based tests
/// above for any attribute the fixture happens not to use; this checks the
/// table's own contract instead.
#[test]
fn no_renamed_prop_is_ever_its_own_attribute_name() {
    for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
        assert_ne!(
            prop, attribute,
            "{prop} is an identity entry — delete it rather than maintain it"
        );
    }
}
