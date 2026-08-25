//! SSR context propagation — a Provider's value reaching consumers **in the
//! server pass**, with no hydration involved.
//!
//! ## What this is testing, and why the shape matters
//!
//! The failing shape was never `<Provider><Consumer /></Provider>` written in
//! one component. It is the one every context library actually composes with:
//!
//! ```jsx
//! function Root({ children }) { return <Ctx.Provider value={v}>{children}</Ctx.Provider>; }
//! <Root><Consumer /></Root>
//! ```
//!
//! `<Consumer />` is evaluated at the **caller's** site. JSX lowers to
//! `h(Root, null, h(Consumer))` and JS evaluates arguments before the call, so
//! the consumer finished rendering before `Root` — let alone the Provider —
//! ever ran. `transforms::thunk_children` defers it; `h` brackets a Provider's
//! forcing with a context push/pop.
//!
//! ⚠️ **The old failure was not a wrong value, it was a throw.** Radix's
//! `createContextScope` raises on a missing context rather than returning the
//! default, so a real Dialog died with `` `DialogTrigger` must be used within
//! `Dialog` ``, its island SSR failed, no `data-albedo-island` marker was
//! emitted, and the component was absent from the page. A test that only
//! asserted "the default came back" would have called that healthy.

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;

const MODULE_SPEC: &str = "Component.tsx";

/// Rendered with **no** anchor-stamp spec: `data-albedo-id` is orthogonal to
/// context and would only make every assertion here a substring puzzle.
fn render(source: &str) -> String {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    engine
        .load_module_with_spec(MODULE_SPEC, source, None)
        .expect("component loads");
    engine
        .render_component_with_host(MODULE_SPEC, "{}", "")
        .expect("component renders")
        .html
}

/// The load-bearing case: children handed in from the caller, exactly as Radix
/// composes.
#[test]
fn a_provider_reaches_a_consumer_passed_in_as_children() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() {
          const theme = useContext(Theme);
          return <span>{theme}</span>;
        }
        function Root(props) {
          return <Theme.Provider value="dark">{props.children}</Theme.Provider>;
        }
        export default function Component() {
          return <Root><Consumer /></Root>;
        }
        "#,
    );
    assert!(
        html.contains("<span>dark</span>"),
        "the Provider's value did not reach a consumer passed in as children — \
         this is the shape every Radix compound component uses.\nHTML: {html}"
    );
}

/// A consumer with no Provider above it must still render, not throw. This is
/// what keeps an ordinary `useContext` from taking a whole route's markup with
/// it — the failure mode that made the old bug invisible.
#[test]
fn a_consumer_outside_any_provider_reads_the_default() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() {
          const theme = useContext(Theme);
          return <span>{theme}</span>;
        }
        export default function Component() {
          return <Consumer />;
        }
        "#,
    );
    assert!(
        html.contains("<span>light</span>"),
        "a consumer with no Provider must fall back to the default.\nHTML: {html}"
    );
}

/// Nesting: the innermost Provider wins, and the outer value is restored after
/// its subtree closes. Asserted together because the second half is what a
/// naive "set on push" implementation gets wrong — it leaks the inner value to
/// everything that follows.
#[test]
fn the_innermost_provider_wins_and_the_outer_one_is_restored() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() {
          const theme = useContext(Theme);
          return <span>{theme}</span>;
        }
        function Wrap(props) {
          return <Theme.Provider value={props.value}>{props.children}</Theme.Provider>;
        }
        export default function Component() {
          return (
            <div>
              <Wrap value="outer">
                <Consumer />
                <Wrap value="inner"><Consumer /></Wrap>
                <Consumer />
              </Wrap>
              <Consumer />
            </div>
          );
        }
        "#,
    );
    let expected = "<div><span>outer</span><span>inner</span><span>outer</span><span>light</span></div>";
    assert!(
        html.contains(expected),
        "nesting/restore is wrong.\nexpected to contain: {expected}\nHTML: {html}"
    );
}

/// Siblings must not see each other's values — the property that makes the
/// implementation a STACK rather than a map of current values.
#[test]
fn sibling_providers_do_not_leak_into_each_other() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() {
          const theme = useContext(Theme);
          return <span>{theme}</span>;
        }
        function Wrap(props) {
          return <Theme.Provider value={props.value}>{props.children}</Theme.Provider>;
        }
        export default function Component() {
          return (
            <div>
              <Wrap value="a"><Consumer /></Wrap>
              <Wrap value="b"><Consumer /></Wrap>
            </div>
          );
        }
        "#,
    );
    let expected = "<div><span>a</span><span>b</span></div>";
    assert!(
        html.contains(expected),
        "sibling Providers leaked.\nexpected to contain: {expected}\nHTML: {html}"
    );
}

/// A consumer nested several plain components deep still sees the value — the
/// thunk has to survive being passed through intermediate `props.children`.
#[test]
fn the_value_survives_intermediate_components() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() {
          const theme = useContext(Theme);
          return <span>{theme}</span>;
        }
        function Middle(props) { return <div>{props.children}</div>; }
        function Outer(props) { return <section>{props.children}</section>; }
        function Root(props) {
          return <Theme.Provider value="dark">{props.children}</Theme.Provider>;
        }
        export default function Component() {
          return <Root><Outer><Middle><Consumer /></Middle></Outer></Root>;
        }
        "#,
    );
    assert!(
        html.contains("<span>dark</span>"),
        "the value did not survive intermediate components.\nHTML: {html}"
    );
}

/// Radix wraps its Providers several components deep (`Root` → `Impl` →
/// `Provider`) and threads `children` down through each as a prop. This is that
/// shape: the deferral has to survive being handed along, not just one level.
#[test]
fn a_provider_nested_several_components_deep_still_pushes() {
    let html = render(
        r#"
        import { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() { return <span>{useContext(Theme)}</span>; }
        function Impl(props) { return <Theme.Provider value="dark">{props.children}</Theme.Provider>; }
        function Mid(props) { return <Impl>{props.children}</Impl>; }
        function Root(props) { return <Mid>{props.children}</Mid>; }
        export default function Component() {
          return <Root><Consumer /></Root>;
        }
        "#,
    );
    assert!(
        html.contains("<span>dark</span>"),
        "a Provider three components deep did not reach the consumer.\nHTML: {html}"
    );
}

/// Radix's `createContextScope` Provider computes its value through `useMemo`
/// and its consumer does `if (context) return context;` before throwing — so a
/// Provider whose value came back `undefined` reads as "no Provider at all".
#[test]
fn a_provider_value_from_use_memo_survives() {
    let html = render(
        r#"
        import { createContext, useContext, useMemo } from "react";
        const Theme = createContext(undefined);
        function Consumer() {
          const ctx = useContext(Theme);
          if (!ctx) { throw new Error("must be used within Provider"); }
          return <span>{ctx.tone}</span>;
        }
        function Root(props) {
          const value = useMemo(() => ({ tone: "dark" }), []);
          return <Theme.Provider value={value}>{props.children}</Theme.Provider>;
        }
        export default function Component() {
          return <Root><Consumer /></Root>;
        }
        "#,
    );
    assert!(
        html.contains("<span>dark</span>"),
        "a useMemo-derived Provider value did not survive.\nHTML: {html}"
    );
}

// ---------------------------------------------------------------------------
// KNOWN LIMIT — characterised, not fixed
// ---------------------------------------------------------------------------

/// 🔴 **A Provider cannot reach children that were already forced before it
/// pushed.** This asserts the CURRENT, WRONG-vs-React behaviour on purpose, so
/// the boundary is a fact in the test suite rather than folklore.
///
/// Deferring a component's children fixes the case where they are handed
/// straight through (`{props.children}`). It cannot help when something forces
/// them first — here `React.Children.toArray`, which reads every child before
/// the Provider wraps them. React is lazy and does not care; this renderer
/// evaluates on read, so the consumer runs outside the context and sees the
/// default.
///
/// ⚠️ **The same shape, and the reason most compound Radix components still
/// fail, occurs INSIDE pre-compiled package code** where no transform of app
/// source can reach it. `react-accordion` emits
/// `jsx(AccordionImplProvider, { children: jsx(Collection.Slot, { children:
/// jsx(Primitive.div, { ...accordionProps }) }) })` — `accordionProps` carries
/// the app's `children`, `Primitive.div` is a host tag so it forces them at
/// once, and the whole expression is evaluated while building the Provider's
/// props object. The children therefore render *before* the Provider pushes and
/// Radix throws `` `AccordionItem` must be used within `Accordion` ``.
///
/// Closing this needs `__albedo_jsx` to return a deferred element rather than
/// finished markup — lazy elements on the package path too. See
/// `transforms::thunk_children`.
#[test]
fn a_child_forced_before_its_provider_pushes_sees_the_default() {
    let html = render(
        r#"
        import React, { createContext, useContext } from "react";
        const Theme = createContext("light");
        function Consumer() { return <span>{useContext(Theme)}</span>; }
        function Root(props) {
          const kids = React.Children.toArray(props.children);
          return <Theme.Provider value="dark">{kids}</Theme.Provider>;
        }
        export default function Component() {
          return <Root><Consumer /></Root>;
        }
        "#,
    );
    assert!(
        html.contains("<span>light</span>"),
        "this test pins a KNOWN LIMIT. If it now renders `dark`, the limit was \
         closed — delete this test and update `thunk_children`'s docs.\nHTML: {html}"
    );
}

// ---------------------------------------------------------------------------
// The PACKAGE path — the automatic JSX runtime
// ---------------------------------------------------------------------------

/// 🔑 **The shape that broke every compound Radix primitive**, written directly
/// against the automatic runtime so it needs no vendored package and no corpus.
///
/// npm packages are compiled to `jsx(type, config)` calls and build their config
/// object *before* calling `jsx`. Radix nests providers exactly this way:
///
/// ```js
/// jsx(AccordionImplProvider, {
///   children: jsx(Collection.Slot, {
///     children: jsx(Primitive.div, { ...accordionProps })   // host tag
///   })
/// })
/// ```
///
/// `accordionProps` carries the app's children and `Primitive.div` is a host
/// tag, so with an eager `jsx` those children rendered *while the Provider's
/// config object was still being built* — before it pushed. Every compound
/// primitive threw `` `X` must be used within `Y` ``.
///
/// `__albedo_jsx` now returns a deferred element on the server, so nothing
/// renders until it is needed and the force descends through the Provider.
///
/// ⚠️ This is deliberately NOT written in JSX syntax: our transform rewrites JSX
/// into the classic `h(…)` form, which is already deferred by
/// `transforms::thunk_children`. Writing the `jsx(…)` calls by hand is the only
/// way to exercise the path a real package takes.
#[test]
fn the_package_jsx_path_defers_a_providers_children() {
    let html = render(
        r#"
        import { jsx } from "react/jsx-runtime";
        import { createContext, useContext } from "react";

        const Theme = createContext("light");

        function Consumer() {
          return jsx("span", { children: useContext(Theme) });
        }

        // A Provider whose children are a HOST TAG built as an argument — the
        // exact construction `react-accordion` emits.
        function Impl(props) {
          return jsx(Theme.Provider, {
            value: "dark",
            children: jsx("div", { children: props.children })
          });
        }

        export default function Component() {
          return jsx(Impl, { children: jsx(Consumer, {}) });
        }
        "#,
    );
    assert!(
        html.contains("<span>dark</span>"),
        "a package-shaped Provider did not reach its consumer — this is the \
         construction every compound Radix primitive uses.\nHTML: {html}"
    );
}

/// A deferred element is an element until it is forced — which is what stops
/// `React.Children.only` throwing on a portal.
///
/// Radix's `Portal` is `container ? createPortal(…) : null`, and server-side
/// there is no `document.body` and `useLayoutEffect` never runs, so it returns
/// `null`. With an eager `jsx`, `Presence` received that literal `null` where
/// React holds an unrendered element, and `Children.only(null)` threw — taking
/// the whole route's markup with it. Deferred, `only` sees an element; forcing
/// it later yields nothing, which is the correct no-JS outcome.
#[test]
fn children_only_accepts_a_deferred_element_that_renders_to_nothing() {
    let html = render(
        r#"
        import React from "react";
        import { jsx } from "react/jsx-runtime";

        function RendersNull() { return null; }

        function Gate(props) {
          // Throws if `children` is not a single valid element.
          const child = React.Children.only(props.children);
          return jsx("div", { children: child });
        }

        export default function Component() {
          return jsx(Gate, { children: jsx(RendersNull, {}) });
        }
        "#,
    );
    assert!(
        html.contains("<div></div>"),
        "a component returning null must still be a valid single child.\nHTML: {html}"
    );
}
