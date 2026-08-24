//! A package's React component must **server-render**, not just hydrate.
//!
//! ## The gap this closes
//!
//! Tier C Phase 2 taught the *browser* to bind a package's `import 'react'` to
//! Albedo's own runtime. The server still bound it to the real React in
//! `node_modules` — and real React's `forwardRef` returns an **object**, while
//! the QuickJS `h` shim can only call functions. An object fell through to the
//! shim's tag-name branch and emitted the literal text `<[object Object]>` into
//! the page. Every React component library rendered as garbage server-side while
//! working perfectly once JavaScript ran.
//!
//! 🔑 **The fix is one table for both hosts** (`runtime::react_host`): the
//! QuickJS prelude and `/_albedo/npm-runtime.js` install the *same* generated
//! records, so `forwardRef` returns the same kind of thing on both sides. That
//! is also the precondition for hydration **adopting** the server's DOM rather
//! than replacing it.
//!
//! These tests start from what the pipeline actually produces — a bundled
//! package, registered on a real engine, rendered to real markup — because a
//! unit test of either half is self-consistent and would have caught none of it.

use dom_render_compiler::bundler::client_npm::server_shake_options;
use dom_render_compiler::bundler::npm::{bundle_npm_dependency, ExternalTarget};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::path::Path;

const MODULE_SPEC: &str = "Component.tsx";

/// A `sideEffects: false` package whose component is built the way every React
/// icon set is built: `forwardRef` around a `createElement('svg', …)`.
fn write_icon_package(root: &Path) {
    let pkg = root.join("node_modules").join("icons");
    std::fs::create_dir_all(pkg.join("esm")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "icons", "version": "1.0.0", "module": "esm/index.js",
             "sideEffects": false }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("esm").join("index.js"),
        "export { default as Check } from './check';",
    )
    .unwrap();
    std::fs::write(
        pkg.join("esm").join("check.js"),
        "import { forwardRef, createElement } from 'react';\n\
         const Check = forwardRef(function (props, ref) {\n\
         \x20 return createElement('svg', {\n\
         \x20   ref: ref, viewBox: '0 0 24 24', strokeWidth: 2, className: 'icon'\n\
         \x20 }, createElement('polyline', { points: '20 6 9 17 4 12' }));\n\
         });\n\
         export default Check;",
    )
    .unwrap();
}

/// Radix's `Slot`, reduced to the parts that decide whether `asChild` works —
/// and kept in Radix's own shape rather than paraphrased, because a paraphrase
/// would test the paraphrase.
///
/// Every line here is load-bearing for a reason the eager server renderer used
/// to break:
///   * `getElementRef` calls `Object.getOwnPropertyDescriptor(element.props, …)`
///     — a `TypeError` if an element has no `props` at all;
///   * `mergeProps` reads the *child's* props to compose `className` and event
///     handlers, so an element that forgot its props silently loses both;
///   * `cloneElement` is how the merged result gets back onto the child.
fn write_slot_package(root: &Path) {
    let pkg = root.join("node_modules").join("slotish");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "slotish", "version": "1.0.0", "module": "index.js",
             "sideEffects": false }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        r#"
import * as React from 'react';

function getElementRef(element) {
  var descriptor = Object.getOwnPropertyDescriptor(element.props, 'ref');
  var getter = descriptor ? descriptor.get : undefined;
  if (getter && 'isReactWarning' in getter && getter.isReactWarning) {
    return element.ref;
  }
  return element.props.ref || element.ref;
}

function mergeProps(slotProps, childProps) {
  const overrideProps = { ...childProps };
  for (const propName in childProps) {
    const slotPropValue = slotProps[propName];
    const childPropValue = childProps[propName];
    const isHandler = /^on[A-Z]/.test(propName);
    if (isHandler) {
      if (slotPropValue && childPropValue) {
        overrideProps[propName] = (...args) => {
          childPropValue(...args);
          slotPropValue(...args);
        };
      } else if (slotPropValue) {
        overrideProps[propName] = slotPropValue;
      }
    } else if (propName === 'style') {
      overrideProps[propName] = { ...slotPropValue, ...childPropValue };
    } else if (propName === 'className') {
      overrideProps[propName] = [slotPropValue, childPropValue].filter(Boolean).join(' ');
    }
  }
  return { ...slotProps, ...overrideProps };
}

export const Slot = React.forwardRef(function (props, forwardedRef) {
  const { children, ...slotProps } = props;
  if (React.Children.count(children) !== 1 || !React.isValidElement(children)) {
    throw new Error('Slot expected a single element child.');
  }
  const mergedProps = mergeProps(slotProps, children.props ?? {});
  mergedProps.ref = forwardedRef || getElementRef(children);
  return React.cloneElement(children, mergedProps);
});

// `Slot`'s other call shape: props untouched, children replaced.
export const Relabel = function (props) {
  return React.cloneElement(props.children, undefined, props.label);
};
"#,
    )
    .unwrap();
}

/// Bundle the package for the server, register it on a fresh engine, load the
/// component, render it.
fn render_through_quickjs(root: &Path, component: &str) -> String {
    render_package_through_quickjs(root, "icons", component)
}

fn render_package_through_quickjs(root: &Path, package: &str, component: &str) -> String {
    let bundle = bundle_npm_dependency(root, package, &server_shake_options())
        .expect("the package bundles for the server");

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

const COMPONENT: &str = r#"
    import { Check } from "icons";
    export default function Toolbar() {
        return <div className="bar"><Check /></div>;
    }
"#;

/// The headline: real markup, not the stringified object.
#[test]
fn a_packages_forward_ref_component_renders_to_real_markup() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let html = render_through_quickjs(dir.path(), COMPONENT);

    assert!(
        !html.contains("[object Object]"),
        "the server must not stringify a component into a tag name: {html}"
    );
    assert!(html.contains("<svg"), "the icon must render: {html}");
    assert!(html.contains("<polyline"), "its children too: {html}");
}

/// The attribute half. `strokeWidth` has to arrive hyphenated or the browser
/// ignores it, and `viewBox` has to keep its case or the icon has no coordinate
/// system — through a package, through `createElement`, through the shared
/// rename table.
#[test]
fn svg_attributes_survive_the_package_boundary() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let html = render_through_quickjs(dir.path(), COMPONENT);

    assert!(html.contains("stroke-width=\"2\""), "{html}");
    assert!(html.contains("viewBox=\"0 0 24 24\""), "{html}");
    assert!(!html.contains("strokeWidth="), "{html}");
    assert!(
        html.contains("class=\"icon\""),
        "className must still become class through a package: {html}"
    );
}

/// `ref` is a binding, not an attribute — on both sides. The server strips it;
/// the browser hands the node to it.
#[test]
fn a_forwarded_ref_never_becomes_an_attribute() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let html = render_through_quickjs(dir.path(), COMPONENT);
    assert!(!html.contains("ref="), "{html}");
    assert!(!html.contains("[object Object]"), "{html}");
}

/// 🔑 **React is never bundled for the server either.** The externalisation is
/// not a browser optimisation — it is the only way this runtime can render a
/// package's component, so the server takes the same host record.
#[test]
fn the_server_bundle_contains_no_react() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let bundle = bundle_npm_dependency(dir.path(), "icons", &server_shake_options())
        .expect("bundles");
    let keys: Vec<&str> = bundle.artifacts.iter().map(|a| a.key.as_str()).collect();
    assert!(
        !keys.iter().any(|key| key.starts_with("npm:react@")),
        "react must be a host record on the server too: {keys:?}"
    );
    assert!(
        bundle
            .artifacts
            .iter()
            .any(|artifact| artifact.script.contains("albedo:host/react")),
        "the package must resolve its react import to the host record"
    );
}

/// Unlike a client bundle, the server does **not** refuse `react-dom`: a package
/// that reaches for it must still *load*, because loading is what 79.6% npm
/// coverage measures and an action may import a Radix-shaped package without
/// ever rendering it. Rendering one is still refused — by the `h` shim, loudly.
#[test]
fn the_server_still_loads_a_package_that_imports_react_dom() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("node_modules").join("portalish");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "portalish", "version": "1.0.0", "module": "index.js" }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        "export const portal = 1;\nexport const dom = 2;",
    )
    .unwrap();
    // `react-dom` itself need not exist for the point: what matters is that the
    // server option set carries no REFUSAL that would stop a bundle.
    //
    // 9.3 made this assertion say what it always meant. It used to check that
    // the server had no entry for `react-dom` at all, which was a proxy for "no
    // refusal" and stopped being one the moment `react-dom` became a host
    // module providing `createPortal`. A proxy assertion fails on the day the
    // thing it stood for is still true, which is the worst day for it to fail.
    let options = server_shake_options();
    assert!(
        !matches!(
            options.externals().get("react-dom"),
            Some(ExternalTarget::Refused { .. })
        ),
        "the server must not refuse react-dom; only client bundles do"
    );
    assert!(
        bundle_npm_dependency(dir.path(), "portalish", &options).is_ok(),
        "an ordinary package must still bundle for the server"
    );
}

// ---------------------------------------------------------------------------
// `cloneElement` — `TODO.md` 9.2
// ---------------------------------------------------------------------------
//
// These start from Radix's `Slot` rather than from `cloneElement` in isolation,
// because `Slot` is the only consumer that matters and it exercises the whole
// chain: an element must be *readable* (`element.props`) before it can be
// cloned, and the clone must re-render the tag it came from. A unit test of
// `cloneElement` alone would have passed against an element that carried no
// props, which is precisely the state that broke `asChild`.

const SLOT_COMPONENT: &str = r#"
    import { Slot } from "slotish";
    export default function Trigger() {
        return (
            <Slot className="slot" data-state="open" aria-expanded={true}>
                <button className="mine" type="button">Open</button>
            </Slot>
        );
    }
"#;

/// The headline: `asChild` puts the wrapper's props onto the author's own tag,
/// server-side, with no JavaScript.
#[test]
fn a_slot_merges_its_props_onto_the_child_element() {
    let dir = tempfile::tempdir().unwrap();
    write_slot_package(dir.path());

    let html = render_package_through_quickjs(dir.path(), "slotish", SLOT_COMPONENT);

    assert!(
        html.starts_with("<button"),
        "the slot must render the CHILD's tag, not a wrapper: {html}"
    );
    assert!(
        html.contains("data-state=\"open\""),
        "a prop that exists only on the slot must reach the child's markup: {html}"
    );
    assert!(
        html.contains("aria-expanded"),
        "the aria wiring is the whole point of asChild: {html}"
    );
    assert!(
        html.contains("class=\"slot mine\""),
        "className must be COMPOSED, slot then child, not overwritten: {html}"
    );
    assert!(html.contains(">Open<"), "the child's own children survive: {html}");
    assert!(!html.contains("ref="), "a merged ref is still not an attribute: {html}");
}

/// The other call shape Radix uses — `cloneElement(el, undefined, children)`.
/// Props must survive untouched while children are replaced.
#[test]
fn cloning_with_no_config_keeps_every_prop_and_replaces_children() {
    let dir = tempfile::tempdir().unwrap();
    write_slot_package(dir.path());

    let html = render_package_through_quickjs(
        dir.path(),
        "slotish",
        r#"
            import { Relabel } from "slotish";
            export default function Trigger() {
                return (
                    <Relabel label="after">
                        <button className="keep" type="button">before</button>
                    </Relabel>
                );
            }
        "#,
    );

    assert!(html.contains("class=\"keep\""), "props survive a childless clone: {html}");
    assert!(html.contains("type=\"button\""), "all of them: {html}");
    assert!(html.contains(">after<"), "children are replaced: {html}");
    assert!(!html.contains("before"), "the originals are gone: {html}");
}

/// 🪤 **The trap this codebase has already fallen into once.** The children a
/// clone re-renders are the *already-escaped* ones. Re-running the escape would
/// turn `&` into `&amp;amp;` — invisible in a passing build and visible on the
/// page.
#[test]
fn cloning_does_not_escape_the_children_a_second_time() {
    let dir = tempfile::tempdir().unwrap();
    write_slot_package(dir.path());

    let html = render_package_through_quickjs(
        dir.path(),
        "slotish",
        r#"
            import { Slot } from "slotish";
            export default function Trigger() {
                const label = "Tom & <Jerry>";
                return (
                    <Slot className="slot">
                        <button type="button">{label}</button>
                    </Slot>
                );
            }
        "#,
    );

    assert!(
        html.contains("Tom &amp; &lt;Jerry&gt;"),
        "the text must be escaped exactly once: {html}"
    );
    assert!(!html.contains("&amp;amp;"), "and not twice: {html}");
    assert!(!html.contains("<Jerry>"), "nor zero times: {html}");
}

/// The documented limit, asserted so it stays a limit rather than becoming a
/// crash. A component whose render is a Fragment has no single host tag, so its
/// element carries no `type` and cloning returns it unchanged — the props are
/// dropped, loudly here and in the prelude's comment, rather than silently
/// producing `<undefined>`.
#[test]
fn cloning_a_fragment_rooted_element_passes_it_through_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    write_slot_package(dir.path());

    let html = render_package_through_quickjs(
        dir.path(),
        "slotish",
        r#"
            import { Slot } from "slotish";
            function Pair() {
                return <><span>one</span><span>two</span></>;
            }
            export default function Trigger() {
                return <Slot className="slot"><Pair /></Slot>;
            }
        "#,
    );

    // (`data-albedo-id` stamps mean the tags are matched on their text, not
    // spelled out.)
    assert!(html.contains(">one</span>"), "the element still renders: {html}");
    assert!(html.contains(">two</span>"), "both halves: {html}");
    assert!(
        !html.contains("class=\"slot\""),
        "and the slot's props are DROPPED, which is what React does here too: {html}"
    );
    assert!(
        !html.contains("undefined"),
        "an unclonable element must not become a bogus tag: {html}"
    );
}
