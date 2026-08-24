//! Boolean `aria-*` props must server-render as the **word**, not a bare name.
//!
//! ## The defect
//!
//! All three renderers emitted a `true`-valued JSX prop as a bare HTML
//! attribute. That is right for a real HTML boolean attribute — `disabled`,
//! `checked`, `hidden` signal by *presence* and their value is ignored — and
//! wrong for `aria-*`, which are **enumerated** attributes whose value space is
//! the two literal strings `"true"` and `"false"`. A bare `aria-expanded` is the
//! empty string, which is in neither, and assistive technology reads it as *not
//! expanded*. React renders `aria-expanded={true}` as `aria-expanded="true"`.
//!
//! Observed 2026-08-24 in real markup from `@radix-ui/react-slot` served by
//! `albedo serve`:
//!
//! ```text
//! <button class="slot-added mine" type="button" data-state="open" aria-expanded>
//! ```
//!
//! The mirror-image half was wrong too: a `false` value was skipped entirely, so
//! `aria-hidden={false}` shipped nothing. "Not hidden" is a claim, not the
//! absence of one — it is what stops an ancestor's `aria-hidden="true"` from
//! being inherited over a subtree that has to stay reachable.
//!
//! ## Why this test starts from real Radix
//!
//! Radix wires **every** compound component's accessibility through boolean
//! `aria-*` props, so the blast radius is the whole shadcn/UI layer, and `useId`
//! + `asChild` were built specifically to make that wiring work. A hand-built
//! props object would have proven the attribute loop and nothing about the path
//! that actually produces those props: real `Slot` spreads `slotProps`, merges
//! the child's props over them in `mergeProps`, and puts the result back through
//! `cloneElement`. Three chances for a boolean to be normalised, stringified or
//! dropped before the renderer ever sees it — and the bug was only *visible*
//! once `cloneElement` (landed 2026-08-24) got Radix's props onto a real tag for
//! the first time.
//!
//! So the package here is the real, unmodified `@radix-ui/react-slot@1.3.3` from
//! `tests/fixtures/npm` (see its README), bundled through the real npm pipeline
//! and rendered on a real engine.
//!
//! The byte-for-byte agreement between the two server renderers is a separate
//! gate: `tests/fixtures/render_quickjs/boolean_attributes` is a conformance
//! corpus case, and `tests/client_hydration.rs` proves the client runtime spells
//! it the same way.

use dom_render_compiler::bundler::client_npm::server_shake_options;
use dom_render_compiler::bundler::npm::bundle_npm_dependency;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::path::{Path, PathBuf};

const MODULE_SPEC: &str = "Component.tsx";
const SLOT_PACKAGE: &str = "@radix-ui/react-slot";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn fixture_npm_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("npm")
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Plant the vendored packages as a real `node_modules` tree, so resolution goes
/// through the same walk-up, `exports` map and ESM entry a user's install would.
fn plant_radix(root: &Path) {
    copy_tree(&fixture_npm_root(), &root.join("node_modules"));
    // The vendored README is documentation, not a package — it must not sit in
    // `node_modules` pretending to be one.
    let _ = std::fs::remove_file(root.join("node_modules").join("README.md"));
}

fn render_through_quickjs(root: &Path, component: &str) -> String {
    let bundle = bundle_npm_dependency(root, SLOT_PACKAGE, &server_shake_options())
        .expect("real @radix-ui/react-slot bundles for the server");

    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    for artifact in &bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .unwrap_or_else(|err| panic!("artifact {} failed to register: {err}", artifact.key));
    }
    engine
        .load_module_with_spec(MODULE_SPEC, component, Some(MODULE_SPEC))
        .expect("component loads");
    engine
        .render_component_with_host(MODULE_SPEC, "{}", "")
        .expect("component renders")
        .html
}

// ---------------------------------------------------------------------------
// A small attribute reader
// ---------------------------------------------------------------------------

/// One attribute as it appears in the markup: `None` is the **bare** form.
///
/// The distinction this whole file is about is bare-vs-`="false"`, and
/// `contains("aria-expanded")` cannot see it — it matches both spellings. So the
/// tag is parsed and the bare form is a distinguishable value rather than an
/// absence to be inferred.
type Attribute = (String, Option<String>);

/// Attributes of every `<tag ...>` opening tag with the given name.
fn opening_tags(html: &str, tag: &str) -> Vec<Vec<Attribute>> {
    let needle = format!("<{tag}");
    let mut tags = Vec::new();
    let mut rest = html;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + needle.len()..];
        // `<button` must not match `<buttonish`.
        let boundary = after.starts_with([' ', '>', '/', '\n', '\t']);
        let end = after.find('>').expect("an opening tag must close");
        if boundary {
            tags.push(parse_attributes(&after[..end]));
        }
        rest = &after[end..];
    }
    tags
}

fn parse_attributes(inside: &str) -> Vec<Attribute> {
    let mut attrs = Vec::new();
    let bytes: Vec<char> = inside.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_whitespace() || bytes[i] == '/' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && !bytes[i].is_whitespace() && bytes[i] != '=' && bytes[i] != '/' {
            i += 1;
        }
        let name: String = bytes[start..i].iter().collect();
        if i < bytes.len() && bytes[i] == '=' {
            i += 1;
            let quote = bytes.get(i).copied().unwrap_or('"');
            assert!(
                quote == '"' || quote == '\'',
                "unquoted attribute value in {inside:?}"
            );
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let value: String = bytes[value_start..i].iter().collect();
            i += 1;
            attrs.push((name, Some(value)));
        } else {
            attrs.push((name, None));
        }
    }
    attrs
}

fn attribute<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a Option<String>> {
    attrs
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map(|(_, value)| value)
}

// ---------------------------------------------------------------------------
// The component under test
// ---------------------------------------------------------------------------

/// A disclosure trigger in Radix's own shape: `asChild`, so the props are put on
/// the caller's own `<button>` by `Slot`, not by a tag this file writes.
///
/// The two instances cover all four cells of the rule on semantically coherent
/// markup — an expanded, ready trigger and a collapsed, busy one — rather than
/// stacking contradictory props on one tag to save a render:
///
/// | | `aria-expanded` | `aria-disabled` | `disabled` |
/// |---|---|---|---|
/// | first | `"true"` | `"false"` | absent |
/// | second | `"false"` | `"true"` | bare |
///
/// `className` on both the `Slot` and the child is what proves the props went
/// *through* Radix's `mergeProps` rather than around it: only `mergeProps`
/// concatenates the two.
const COMPONENT: &str = r#"
    import { Slot } from "@radix-ui/react-slot";

    function DisclosureTrigger(props) {
        return (
            <Slot
                className="trigger"
                aria-expanded={props.open}
                aria-disabled={props.busy}
                disabled={props.busy}
                data-state={props.open ? "open" : "closed"}
            >
                {props.children}
            </Slot>
        );
    }

    export default function Disclosure() {
        return (
            <div>
                <DisclosureTrigger open={true} busy={false}>
                    <button type="button" className="mine">Open &amp; see</button>
                </DisclosureTrigger>
                <DisclosureTrigger open={false} busy={true}>
                    <button type="button" className="mine">Busy</button>
                </DisclosureTrigger>
            </div>
        );
    }
"#;

fn render_the_disclosure() -> Vec<Vec<Attribute>> {
    let dir = tempfile::tempdir().unwrap();
    plant_radix(dir.path());
    let html = render_through_quickjs(dir.path(), COMPONENT);
    let buttons = opening_tags(&html, "button");
    assert_eq!(
        buttons.len(),
        2,
        "both triggers must reach a real <button>; got {html}"
    );
    buttons
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The precondition. If Radix's merge did not run, everything below is testing
/// this file's own JSX and would pass while the real path stayed broken.
#[test]
fn radix_mergeprops_actually_ran() {
    let buttons = render_the_disclosure();
    for button in &buttons {
        assert_eq!(
            attribute(button, "class"),
            Some(&Some("trigger mine".to_string())),
            "`mergeProps` concatenates the Slot's className with the child's; \
             anything else means the props did not go through Radix"
        );
        assert_eq!(
            attribute(button, "type"),
            Some(&Some("button".to_string())),
            "the child's own props must survive the merge"
        );
        assert!(
            attribute(button, "ref").is_none(),
            "`Slot` sets `mergedProps.ref`; a ref is not an attribute"
        );
    }
}

/// The headline: `aria-expanded={true}` is the word, not a bare name.
#[test]
fn a_true_aria_prop_renders_the_word_true() {
    let buttons = render_the_disclosure();
    assert_eq!(
        attribute(&buttons[0], "aria-expanded"),
        Some(&Some("true".to_string())),
        "a bare `aria-expanded` is the empty string, which assistive technology \
         reads as NOT expanded"
    );
}

/// The mirror image, and the half that is easy to miss: `false` is a value here,
/// not a reason to drop the attribute.
#[test]
fn a_false_aria_prop_renders_the_word_false_and_is_not_dropped() {
    let buttons = render_the_disclosure();
    assert_eq!(
        attribute(&buttons[0], "aria-disabled"),
        Some(&Some("false".to_string())),
        "`aria-disabled={{false}}` must say so; hiding is not the same as saying \
         \"not hidden\""
    );
    assert_eq!(
        attribute(&buttons[1], "aria-expanded"),
        Some(&Some("false".to_string()))
    );
}

/// The other side of the rule, which the fix must not break: a real HTML boolean
/// attribute still signals by presence. `disabled="false"` is still disabled.
#[test]
fn a_real_html_boolean_attribute_is_still_bare_or_absent() {
    let buttons = render_the_disclosure();
    assert!(
        attribute(&buttons[0], "disabled").is_none(),
        "`disabled={{false}}` removes the attribute — a value would enable it"
    );
    assert_eq!(
        attribute(&buttons[1], "disabled"),
        Some(&None),
        "`disabled={{true}}` is the bare name; `disabled=\"true\"` would be a \
         different (and equally disabled) spelling the client does not emit"
    );
}

/// A `data-*` state marker is untouched by any of this: Radix writes those as
/// strings, and this codebase's own `data-albedo-link` is a boolean prop that
/// stays bare on purpose.
#[test]
fn data_state_markers_are_unchanged() {
    let buttons = render_the_disclosure();
    assert_eq!(
        attribute(&buttons[0], "data-state"),
        Some(&Some("open".to_string()))
    );
    assert_eq!(
        attribute(&buttons[1], "data-state"),
        Some(&Some("closed".to_string()))
    );
}

/// The invariant behind all of the above, stated once over the whole document
/// rather than per attribute: **no `aria-*` attribute may be bare.**
///
/// This is the assertion that would have caught the original defect without
/// anyone knowing which aria attribute Radix was going to emit — which is the
/// situation every future Radix primitive puts us in.
#[test]
fn no_aria_attribute_anywhere_is_bare() {
    let dir = tempfile::tempdir().unwrap();
    plant_radix(dir.path());
    let html = render_through_quickjs(dir.path(), COMPONENT);

    let mut bare = Vec::new();
    for tag in ["button", "div"] {
        for attrs in opening_tags(&html, tag) {
            for (name, value) in attrs {
                if name.starts_with("aria-") && value.is_none() {
                    bare.push(name);
                }
            }
        }
    }
    assert!(
        bare.is_empty(),
        "bare aria attributes carry the empty string, which is not `true` and \
         not `false`: {bare:?}\n{html}"
    );
}
