//! A reactive text update must not delete the static text beside it.
//!
//! ## The bug
//!
//! `<span className="tally">total: {count}</span>` server-rendered correctly as
//! `total: 0` and then destroyed itself on the first click, leaving a bare `1`.
//! The binding named the `<span>`, and the only thing the client can do to that
//! node is replace its whole `textContent` — so the literal `total: ` went with
//! it.
//!
//! It reproduced with a plain `useState(0)` and no island and no props, which
//! is what made it worth fixing at the renderer rather than anywhere further
//! out: `<span>total: {count}</span>` is not an exotic construct.
//!
//! ## The fix, and why it changes no markup
//!
//! An element whose children mix static text with reactive reads records its
//! text as a **template** on the derived rung, and the client recomputes the
//! whole string — static parts included. No wrapper element, no new anchor, not
//! one byte of markup different.
//!
//! That last part is not an aesthetic preference. A wrapper was the first
//! attempt, and `tests/renderer_conformance.rs` rejected it: the wrapper exists
//! only in the pure-Rust render, so its id landed in an opcode frame that the
//! server ships alongside **QuickJS**-rendered Tier-B markup, where that node
//! does not exist. Every id in a frame has to exist in the markup that gets
//! served, and a template keeps that true by not inventing a node at all.

use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("hook_compile")
        .join(name)
}

fn project(name: &str) -> CompiledProject {
    CompiledProject::load_from_dir(fixture(name)).expect("fixture compiles")
}

fn slots() -> SessionSlotView {
    SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()))
}

fn payload(
    name: &str,
) -> dom_render_compiler::runtime::eval::ReactivePayload {
    let project = project(name);
    project
        .build_reactive_payload(
            "Component.tsx",
            &Value::Object(Default::default()),
            &slots(),
        )
        .expect("reactive payload builds")
}

/// The recomputed text must contain the static part, or the update deletes it.
///
/// This is the regression in one assertion: run the emitted thunk's source and
/// look for `total: `. A binding that only knows how to produce the count is a
/// binding that erases the label.
#[test]
fn the_recomputed_text_still_contains_the_static_prefix() {
    let payload = payload("text_template");

    let tally = payload
        .derived
        .iter()
        .find(|binding| binding.thunk.contains("total: "))
        .unwrap_or_else(|| {
            panic!(
                "no binding rebuilds the static prefix — the update would delete \
                 it. derived: {:#?}",
                payload.derived
            )
        });

    assert!(
        tally.attr.is_none() && !tally.html,
        "the tally binding must apply as text, not as an attribute or innerHTML"
    );
    assert!(
        !tally.dep_slots.is_empty(),
        "the template must depend on the count slot, or it never recomputes"
    );
}

/// The element that *does* own its whole text keeps the cheaper binding.
///
/// Templating everything would work and would be wasteful: `<span>{count}</span>`
/// needs no recompute thunk, just a slot subscription. Both shapes are in the
/// one fixture so this cannot silently regress into one-size-fits-all.
#[test]
fn a_read_that_owns_its_element_still_binds_directly() {
    let payload = payload("text_template");
    assert!(
        !payload.texts.is_empty(),
        "`<span className=\"solo\">{{count}}</span>` owns its element's text and \
         must still bind through the plain text rung; payload: {payload:#?}"
    );
}

/// Markup is unchanged — the whole point of choosing a template over a wrapper.
#[test]
fn the_served_markup_is_byte_identical_to_a_render_with_no_bindings() {
    let project = project("text_template");
    let render = |hook_compile: bool| {
        render_entry_with_bindings(
            &project,
            "Component.tsx",
            &Value::Object(Default::default()),
            &slots(),
            &RenderOptions { hook_compile },
        )
        .expect("renders")
        .html
    };

    assert_eq!(
        render(true),
        render(false),
        "hook-compile mode must not add a node to carry this binding — a node \
         that exists in only one of the two renderers is a frame target that \
         cannot be found in the markup actually served"
    );
}

/// The initial server paint is what the client must agree with.
#[test]
fn the_server_renders_the_static_and_dynamic_parts_together() {
    let project = project("text_template");
    let html = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots(),
        &RenderOptions {
            hook_compile: true,
        },
    )
    .expect("renders")
    .html;

    assert!(
        html.contains(">total: 0</span>"),
        "server paint must carry both parts; got {html}"
    );
}

/// An element holding both a reactive read and a child ELEMENT cannot be
/// repainted as text at all — rebuilding the parent's text would delete the
/// child, and with it any anchor or handler inside it.
///
/// So it declines to binding mode and the component takes full hydration. The
/// previous behaviour was a plain text binding that destroyed the child on the
/// first update, which is the same class of bug this file exists to close.
#[test]
fn a_reactive_read_beside_a_child_element_declines_to_full_hydration() {
    let dir = std::env::temp_dir().join("albedo-text-template-mixed");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(
        dir.join("Component.tsx"),
        r#"import { useState } from "react";
export default function Mixed() {
  const [n, setN] = useState(0);
  return (
    <p>
      <button onClick={() => setN(n + 1)}>go</button>
      {n}
      <b>units</b>
    </p>
  );
}
"#,
    )
    .expect("write fixture");

    let project = CompiledProject::load_from_dir(&dir).expect("compiles");
    let result = project.build_reactive_payload(
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots(),
    );

    assert!(
        result.is_err(),
        "binding mode must decline this rather than ship a text binding that \
         deletes the <b>; got {result:#?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
