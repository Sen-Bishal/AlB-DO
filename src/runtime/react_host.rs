//! The React surface Albedo's own runtimes provide, and the modules they refuse.
//!
//! ## Why this is one table for two runtimes
//!
//! An npm package's `import … from 'react'` cannot bind to the real React —
//! neither of Albedo's renderers speaks React's component protocol. The browser
//! runtime builds vnodes; the QuickJS `h` shim builds HTML strings **eagerly**.
//! What they *do* share is the set of global names they expose (`h`, `useState`,
//! `useEffect`, `useRef`, `useMemo`, `useCallback`, `createContext`,
//! `useContext`), which is exactly the surface a package reaches for.
//!
//! 🔑 **So one table serves both hosts**, and the record a package binds to is
//! generated from it by [`build_host_module_records_script`] — emitted into the
//! browser's `/_albedo/npm-runtime.js` and into the QuickJS prelude from the
//! same function. A package therefore gets the *same* `forwardRef` on both
//! sides, which is the precondition for hydration adopting the server's DOM
//! instead of replacing it.
//!
//! Before this existed the two disagreed structurally: the browser bound to
//! Albedo's host module (a function), the server bound to the real React (an
//! object), and the server's `h` stringified the object into a tag name — every
//! React component library rendered as the literal text `<[object Object]>`.
//!
//! ## The divergences, and how they stay named
//!
//! Three exports cannot share a body, and all three for the same underlying
//! reason: **the browser's element is a vnode (`value.__vnode === true`) and the
//! server's is an `AlbedoHtml`** — finished HTML, because the QuickJS `h` is
//! eager.
//!
//! | export | browser | server |
//! |---|---|---|
//! | `isValidElement` | `value.__vnode === true` | `instanceof AlbedoHtml` |
//! | `createPortal` (`react-dom`) | builds a vnode | renders nothing |
//! | `cloneElement` | rebuilds the vnode through `h` | **re-renders one host tag** |
//!
//! Each routes through a named global (`__albedo_is_element`,
//! `__albedo_createPortal`, `__albedo_clone_element`) that each runtime defines
//! for itself — so the *table* stays single-sourced and the difference is a
//! named function rather than a second table.
//!
//! ⚠️ `cloneElement` is the one that is not merely a different spelling of the
//! same idea. On an eager renderer an element's props are already bytes, so the
//! server can only clone by having retained the call's arguments and re-running
//! that one tag; the how, and what it deliberately cannot do, is documented at
//! `quickjs_engine`'s `__albedo_element` and `__albedo_clone_element`. It is
//! also the API that makes shadcn's `asChild` work, because Radix's `Slot` is
//! built from it.
//!
//! ## Refusals are host-specific, deliberately
//!
//! [`REFUSED_MODULES`] applies to **client** bundles only. `react-dom` has no
//! browser implementation here (`createPortal` is `TODO.md` 9.3), so a Tier-C
//! island reaching for it must fail at build rather than ship a stub that throws
//! in a user's browser. On the **server** the same package must still *load* —
//! it is 79.6% npm coverage's business to load, not to render — and refusing it
//! there would turn a measured capability into a build error for every action
//! that merely imports something Radix-shaped.

/// A module the **host runtime** provides itself, so no npm copy of it is ever
/// walked, bundled or transferred.
pub struct HostModule {
    /// Bare specifiers that resolve to this module.
    pub specifiers: &'static [&'static str],
    /// Record key the linker publishes it under.
    pub record_key: &'static str,
    /// JS evaluated once in the runtime script's private scope, before the
    /// factory is registered. Non-trivial implementations live here.
    pub prelude: &'static str,
    /// `(export name, JS expression)`.
    ///
    /// 🔑 **One list, two consumers.** The emitted record and the `provides` set
    /// that the build-time import check uses are both derived from this, so a
    /// name cannot be advertised without being implemented, or implemented
    /// without being accepted. That is the `albedo doctor` rule — derivations,
    /// never a maintained list beside the thing it describes.
    pub exports: &'static [(&'static str, &'static str)],
    /// Bind `default` to the record itself (the CJS-interop shape: `import
    /// React from 'react'` sees the namespace).
    pub default_is_namespace: bool,
}

/// A module the host **declines** to provide.
///
/// 🔑 **Refused at build, never stubbed to throw at run time.** A throwing stub
/// moves a fact the compiler already has into the user's browser, where it
/// arrives as a blank island instead of a build error naming the package.
pub struct RefusedModule {
    /// Bare specifiers this refusal covers.
    pub specifiers: &'static [&'static str],
    /// Reason, surfaced verbatim in the build error.
    pub reason: &'static str,
}

/// The React surface Albedo's client runtime can honour.
///
/// ## How this list was chosen
///
/// Not by copying React's exports. Every name here maps onto something
/// `assets/albedo-client.js` actually implements, and a package importing a name
/// that is *not* here fails the build with the list of what is. The set covers
/// every name the corpus at `C:/Development/albedo-corpus` imports from `react`
/// except the four called out under [`REFUSED_MODULES`] and in the
/// comments below.
///
/// ⚠️ **One documented deviation: `useLayoutEffect` is `useEffect`.** The client
/// runtime has a single post-commit effect phase, so a layout effect runs after
/// paint rather than before it. The observable difference is a possible flash on
/// a measure-then-reposition pattern — not a correctness failure, and the
/// alternative (refusing the name) would decline seven import sites in the
/// corpus for a timing nuance. Said out loud here rather than discovered later.
const REACT_HOST: HostModule = HostModule {
    specifiers: &["react"],
    record_key: "albedo:host/react",
    prelude: r#"
  var __albedo_has_own = Object.prototype.hasOwnProperty;

  // `forwardRef` in a function-component VDOM: pull `ref` out of props and hand
  // it to the render function as its second argument. The DOM side of this —
  // attaching a real node to the ref instead of stringifying it into an
  // attribute — lives in `assets/albedo-client.js`'s `applyProp`; without that
  // half, `forwardRef` would return a component that quietly emits
  // `ref="[object Object]"`.
  function __albedo_forwardRef(render) {
    function AlbedoForwardRef(props) {
      var rest = {};
      var ref = null;
      if (props) {
        for (var key in props) {
          if (!__albedo_has_own.call(props, key)) { continue; }
          if (key === 'ref') { ref = props[key]; } else { rest[key] = props[key]; }
        }
      }
      return render(rest, ref);
    }
    AlbedoForwardRef.__albedoForwardRef = true;
    return AlbedoForwardRef;
  }

  // `memo` is an optimization, never a semantic. Returning the component
  // unchanged is correct and slower; returning a wrapper that guessed at
  // equality would be neither.
  function __albedo_memo(component) { return component; }

  function __albedo_createRef() { return { current: null }; }

  // `React.Children` — the traversal helpers.
  //
  // Found by building a real Radix Dialog rather than by reading React's
  // exports: `DialogPortal` calls `React.Children.map(children, …)` on every
  // render, so the whole shadcn overlay layer died on `Cannot read properties
  // of undefined (reading 'map')` — with `createPortal` implemented and never
  // reached.
  //
  // 🔑 **One implementation serves both runtimes.** A "child" is a vnode in the
  // browser and an `AlbedoHtml` (or a string) on the server, but nothing here
  // inspects a child: the walk only flattens nested arrays and recognises the
  // EMPTY slots, and `null`/`undefined`/`boolean` mean the same thing on both
  // sides. So unlike `isValidElement` and `createPortal`, this needs no
  // per-runtime global.
  //
  // The empty-slot rule is React's, including the parts that look inconsistent:
  //   * `map`/`forEach` invoke the callback for EVERY slot, empty ones included
  //     (React's `mapIntoArray` sets `invokeCallback` for an invalid child), and
  //     a `null` RESULT is dropped from the output;
  //   * `count` counts every slot, empty ones included — `count([a, null, b])`
  //     is 3, while `toArray([a, null, b])` has length 2.
  // Faithful rather than tidy, because a package written against React's
  // behaviour is the only consumer.
  function __albedo_child_is_empty(child) {
    return child === null || child === undefined || typeof child === 'boolean';
  }

  function __albedo_children_each(children, visit) {
    // The server defers a component's children (see
    // `transforms::thunk_children`), so a package inspecting `props.children`
    // would otherwise be handed the closure instead of the child. Forcing here
    // is enough for ALL of `React.Children`, because every method funnels
    // through this one walk.
    //
    // Guarded on the hook's existence rather than on a platform test: the
    // client runtime has no thunks and does not define it, so this whole branch
    // is inert there and the two runtimes keep one implementation.
    //
    // ⚠️ `__albedo_child_view`, NOT `__albedo_force_thunk`: a **deferred
    // element** must be handed to the caller UNFORCED. It already carries a real
    // `type` and `props`, which is everything `Children.map`/`only` and their
    // callers inspect, and forcing it here would render it outside whatever
    // Provider the caller is about to wrap it in — reintroducing the bug one
    // level up. Only an opaque app-code thunk, which has nothing to inspect, is
    // forced.
    if (typeof globalThis.__albedo_child_view === 'function') {
      children = globalThis.__albedo_child_view(children);
    }
    if (Array.isArray(children)) {
      for (var i = 0; i < children.length; i++) {
        __albedo_children_each(children[i], visit);
      }
      return;
    }
    visit(children);
  }

  var __albedo_Children = {
    map: function(children, fn) {
      // React returns the argument untouched when there is nothing to map,
      // rather than an empty array.
      if (children === null || children === undefined) { return children; }
      var out = [];
      var index = 0;
      __albedo_children_each(children, function(child) {
        var mapped = fn(child, index++);
        if (mapped !== null && mapped !== undefined) { out.push(mapped); }
      });
      return out;
    },
    forEach: function(children, fn) {
      if (children === null || children === undefined) { return; }
      var index = 0;
      __albedo_children_each(children, function(child) { fn(child, index++); });
    },
    count: function(children) {
      if (children === null || children === undefined) { return 0; }
      var n = 0;
      __albedo_children_each(children, function() { n++; });
      return n;
    },
    toArray: function(children) {
      var out = [];
      if (children === null || children === undefined) { return out; }
      __albedo_children_each(children, function(child) {
        if (!__albedo_child_is_empty(child)) { out.push(child); }
      });
      return out;
    },
    only: function(children) {
      var found = [];
      __albedo_children_each(children, function(child) {
        if (!__albedo_child_is_empty(child)) { found.push(child); }
      });
      if (found.length !== 1) {
        throw new Error('React.Children.only expected to receive a single React element child.');
      }
      return found[0];
    }
  };

  // Published on the global as well as through the record, because there are
  // two ways `React.Children` is reached and only one of them is this record.
  // An npm package binds to the record; a user's own island writing `import
  // React from "react"` is rewritten to a namespace object of `globalThis.*`
  // shims (`quickjs_engine::rewrite_framework_runtime_import`), which cannot
  // see this scope's `var`. One implementation, two doors.
  globalThis.__albedo_Children = __albedo_Children;

  // `React.createElement` — the CLASSIC runtime's element constructor, and the
  // other half of the deferral `__albedo_jsx` performs for the automatic one.
  //
  // 🪤 **Not every package ships automatic JSX.** `@radix-ui/react-select` in the
  // corpus is compiled to
  // `createElement(Root, popperScope, createElement(SelectProvider, {…}, …))` —
  // classic — so it kept rendering its children while building the outer
  // Provider's arguments and threw `` `SelectTrigger` must be used within
  // `Select` `` long after every `jsx`-compiled primitive worked. Mapping
  // `createElement` straight to `h` was the hole.
  //
  // Deliberately NOT achieved by making `globalThis.h` itself lazy: `h` is also
  // the pragma app JSX lowers to, where `transforms::thunk_children` already
  // handles ordering, and changing `h` would move every existing golden.
  function __albedo_createElement(type, props) {
    var children = Array.prototype.slice.call(arguments, 2);
    if (typeof globalThis.__albedo_lazy_element === 'function') {
      var kids;
      if (children.length === 1) { kids = children[0]; }
      else if (children.length > 1) { kids = children; }
      return globalThis.__albedo_lazy_element(type, props || {}, kids);
    }
    return globalThis.h.apply(null, [type, props].concat(children));
  }

  // `useReducer` on top of `useState`. The dispatch identity is stable because
  // `useState`'s setter is recreated per render but only ever closes over the
  // same hook cell; the reducer is read from a ref so a dispatch always applies
  // the latest one, matching React.
  function __albedo_useReducer(reducer, initialArg, init) {
    var pair = globalThis.useState(function () {
      return typeof init === 'function' ? init(initialArg) : initialArg;
    });
    var latest = globalThis.useRef(reducer);
    latest.current = reducer;
    var setState = pair[1];
    var dispatch = globalThis.useCallback(function (action) {
      setState(function (previous) { return latest.current(previous, action); });
    }, []);
    return [pair[0], dispatch];
  }

  // `useImperativeHandle` is the other half of `forwardRef`. React runs it in
  // the layout phase; here it rides the same single effect phase as
  // `useLayoutEffect`, with the same documented timing deviation.
  function __albedo_useImperativeHandle(ref, create, deps) {
    globalThis.useEffect(function () {
      var value = create();
      if (typeof ref === 'function') { ref(value); }
      else if (ref) { ref.current = value; }
      return function () {
        if (typeof ref === 'function') { ref(null); }
        else if (ref) { ref.current = null; }
      };
    }, deps);
  }
"#,
    exports: &[
        ("createElement", "__albedo_createElement"),
        ("Fragment", "globalThis.h.Fragment"),
        ("forwardRef", "__albedo_forwardRef"),
        ("memo", "__albedo_memo"),
        ("createRef", "__albedo_createRef"),
        ("Children", "__albedo_Children"),
        ("isValidElement", "globalThis.__albedo_is_element"),
        // `TODO.md` 9.2. Radix's `Slot` — the whole of `asChild`, and so the
        // whole of shadcn's composition story — is four `cloneElement` calls
        // and nothing else. Found by grepping what the *packages* call rather
        // than what app code imports; see the module note on the divergences
        // for why this one is the expensive half.
        ("cloneElement", "globalThis.__albedo_clone_element"),
        ("createContext", "globalThis.createContext"),
        ("useState", "globalThis.useState"),
        ("useEffect", "globalThis.useEffect"),
        // ⚠️ See the type-level comment: one effect phase, so layout effects run
        // after paint.
        ("useLayoutEffect", "globalThis.useEffect"),
        ("useRef", "globalThis.useRef"),
        // `TODO.md` 9.2. Radix calls this on every primitive to wire
        // `aria-controls`/`aria-labelledby`, so it is a shadcn prerequisite. The
        // server/client agreement it depends on is documented at the
        // implementation in `quickjs_engine.rs`'s prelude.
        ("useId", "globalThis.useId"),
        ("useMemo", "globalThis.useMemo"),
        ("useCallback", "globalThis.useCallback"),
        ("useContext", "globalThis.useContext"),
        ("useReducer", "__albedo_useReducer"),
        ("useImperativeHandle", "__albedo_useImperativeHandle"),
    ],
    default_is_namespace: true,
};

/// The automatic JSX runtime, which most modern packages are compiled against.
///
/// 🔑 **`jsx` is where `props.children` becomes variadic children**, which is
/// the shape `h` takes — `jsx(type, {..., children}, key)` pulls `children`
/// back out of `props` and hands it to `h` positionally, same as the classic
/// `createElement(type, props, ...children)` call shape does natively. Both
/// paths converge on `h`, so both depend on `h` folding those positional
/// children back into `props.children` for a component type — the shape a
/// component actually reads. That fold lives once in each host's `h`
/// (`quickjs_engine.rs` server-side, `assets/albedo-client.js` client-side)
/// rather than here, so `jsx` and the classic path are two callers of the
/// same fix, not two implementations of it.
const JSX_RUNTIME_HOST: HostModule = HostModule {
    specifiers: &["react/jsx-runtime", "react/jsx-dev-runtime"],
    record_key: "albedo:host/react-jsx-runtime",
    prelude: r#"
  var __albedo_jsx_has_own = Object.prototype.hasOwnProperty;

  function __albedo_jsx(type, config, key) {
    var props = {};
    var children;
    if (config) {
      for (var name in config) {
        if (!__albedo_jsx_has_own.call(config, name)) { continue; }
        if (name === 'children') { children = config[name]; } else { props[name] = config[name]; }
      }
    }
    if (key !== undefined && key !== null) { props.key = key; }
    // 🔑 **On the server this returns a DEFERRED element, not markup.**
    //
    // A package builds its props object before calling `jsx`, so
    // `jsx(Provider, { children: jsx(Inner, …) })` evaluates `Inner` FIRST —
    // the same argument-order defeat that `transforms::thunk_children` removes
    // for app code, but located inside pre-compiled package source no transform
    // can reach. Radix nests providers exactly this way
    // (`jsx(AccordionImplProvider, { children: jsx(Collection.Slot, { children:
    // jsx(Primitive.div, { ...appProps }) }) })`), so every compound primitive
    // rendered its children before its own Provider pushed.
    //
    // Deferring here makes nothing render until something needs the bytes, and
    // the force descends THROUGH the Provider — which is the ordering React has.
    //
    // Guarded on the hook's existence rather than a platform test: the client
    // runtime's `h` already builds vnodes lazily and defines no such hook, so it
    // keeps the eager path below and the two runtimes share one `jsx`.
    if (typeof globalThis.__albedo_lazy_element === 'function') {
      return globalThis.__albedo_lazy_element(type, props, children);
    }
    if (children === undefined) { return globalThis.h(type, props); }
    if (Array.isArray(children)) {
      return globalThis.h.apply(null, [type, props].concat(children));
    }
    return globalThis.h(type, props, children);
  }
"#,
    exports: &[
        ("jsx", "__albedo_jsx"),
        ("jsxs", "__albedo_jsx"),
        // The dev runtime passes extra source/self arguments after `key`; they
        // are positional and simply ignored.
        ("jsxDEV", "__albedo_jsx"),
        ("Fragment", "globalThis.h.Fragment"),
    ],
    default_is_namespace: true,
};

/// `react-dom`, narrowed to the one export Albedo actually implements.
///
/// `TODO.md` 9.3. Radix routes `Dialog`, `Popover`, `Tooltip`, `Select` and
/// `DropdownMenu` content through `createPortal`, so the whole shadcn overlay
/// layer sat behind this one name.
///
/// 🔑 **The `exports` list is the refusal.** The build-time import check derives
/// its `provides` set from this table, so a package importing `createRoot`,
/// `hydrateRoot`, `flushSync` or `render` still fails at build with the name it
/// asked for — the loud error the blanket refusal used to give, now per-export
/// rather than per-module. A half-right `flushSync` would be worse than
/// refusing it: it turns a build error into a subtly wrong batch.
///
/// `createPortal` is the second export whose implementation cannot be shared
/// between the two runtimes (`isValidElement` is the first), so it routes
/// through a global each defines for itself — a vnode in
/// `assets/albedo-client.js`, empty markup in the QuickJS prelude.
const REACT_DOM_HOST: HostModule = HostModule {
    specifiers: &["react-dom"],
    record_key: "albedo:host/react-dom",
    prelude: "",
    exports: &[("createPortal", "globalThis.__albedo_createPortal")],
    default_is_namespace: true,
};

/// Every module the browser host provides.
pub const HOST_MODULES: &[HostModule] = &[REACT_HOST, JSX_RUNTIME_HOST, REACT_DOM_HOST];

/// Modules a client bundle refuses, with the reason a user sees.
///
/// 🪤 **This list got shorter when 9.3 landed, and the reason is worth keeping.**
/// It used to refuse `react-dom` whole, on the grounds that `createPortal`
/// needed the SSR renderer and hydration to agree on where portal content lands
/// in the HTML. That premise was wrong: React's own server renderer *throws* on
/// portals, so there is no server markup to agree about, and the feature was
/// never as large as the refusal implied. A refusal is a claim about what the
/// host cannot do, and this one outlived its evidence.
///
/// What remains refused are the entry points that own a render lifecycle Albedo
/// already owns (`createRoot`, `hydrateRoot`, `renderToString`). `react-dom`'s
/// other named exports are refused by a sharper mechanism now: they are simply
/// absent from [`REACT_DOM_HOST`]'s `exports`, so the import check names the
/// export rather than the package.
///
/// 🪤 **`react-is` was briefly on this list and should not have been.** It is
/// ordinary JavaScript that reads `$$typeof` tags and works standalone, and
/// refusing it turned a bundle that would have built into a build error. It is
/// also unreachable in practice once `NODE_ENV` folds, because the only thing
/// that imports it is `prop-types`' development arm. **A refusal must name a
/// capability the host genuinely lacks, not a package that looks React-shaped.**
pub const REFUSED_MODULES: &[RefusedModule] = &[
    RefusedModule {
        specifiers: &["react-dom/client", "react-dom/server"],
        reason: "Albedo's client runtime is not react-dom — `createRoot`, \
                 `hydrateRoot` and `renderToString` own a render lifecycle \
                 Albedo already owns. Bare `react-dom` IS available for \
                 `createPortal`; these entry points are not.",
    },
];

/// The factory registrations for every host module, as JavaScript.
///
/// Evaluated by both runtimes: the browser gets it inside
/// `/_albedo/npm-runtime.js`, QuickJS gets it in its prelude right after the
/// record linker. Idempotent — re-evaluating is a no-op — because both runtimes
/// install their preludes per context and a second install must not clobber a
/// live record.
#[must_use]
pub fn build_host_module_records_script() -> String {
    let mut out = String::new();
    for module in HOST_MODULES {
        out.push_str("\n(function() {\n");
        out.push_str(&format!(
            "  if (globalThis.__ALBEDO_NPM_FACTORIES['{key}']) {{ return; }}\n",
            key = module.record_key
        ));
        out.push_str(module.prelude);
        out.push_str(&format!(
            "  globalThis.__ALBEDO_NPM_FACTORIES['{key}'] = function (__albedo_exports) {{\n",
            key = module.record_key
        ));
        for (name, expression) in module.exports {
            out.push_str(&format!(
                "    __albedo_exports['{name}'] = {expression};\n"
            ));
        }
        if module.default_is_namespace {
            out.push_str("    __albedo_exports['default'] = __albedo_exports;\n");
        }
        out.push_str("  };\n");
        // Aliases so a bundled file that reached this record by *specifier* — a
        // CJS `require('react')` whose resolve map was not rewritten — still
        // lands here rather than throwing MODULE_MISSING.
        for specifier in module.specifiers {
            out.push_str(&format!(
                "  globalThis.__ALBEDO_NPM_ALIASES['{specifier}'] = '{key}';\n",
                key = module.record_key
            ));
        }
        out.push_str("})();\n");
    }
    out
}

/// The record key a bare specifier binds to, if a host provides it.
#[must_use]
pub fn host_record_key(specifier: &str) -> Option<&'static str> {
    HOST_MODULES.iter().find_map(|module| {
        module
            .specifiers
            .contains(&specifier)
            .then_some(module.record_key)
    })
}

/// Every name a host module provides, including `default` when the record is
/// its own namespace.
#[must_use]
pub fn host_provides(module: &HostModule) -> Vec<String> {
    let mut names: Vec<String> = module
        .exports
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    if module.default_is_namespace {
        names.push("default".to_string());
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_name_is_emitted() {
        let script = build_host_module_records_script();
        for module in HOST_MODULES {
            for (name, _) in module.exports {
                assert!(
                    script.contains(&format!("__albedo_exports['{name}']")),
                    "{name} is advertised but not emitted"
                );
            }
        }
    }

    /// The two expressions that must NOT be runtime-specific literals, because
    /// only one of the two hosts would satisfy them.
    #[test]
    fn the_shared_expressions_are_host_neutral() {
        let react = HOST_MODULES
            .iter()
            .find(|module| module.specifiers.contains(&"react"))
            .expect("react host exists");
        let lookup = |name: &str| {
            react
                .exports
                .iter()
                .find(|(export, _)| *export == name)
                .map(|(_, expression)| *expression)
        };
        assert_eq!(
            lookup("Fragment"),
            Some("globalThis.h.Fragment"),
            "`globalThis.Fragment` exists only in the browser runtime"
        );
        assert_eq!(
            lookup("isValidElement"),
            Some("globalThis.__albedo_is_element"),
            "an element is a vnode in one runtime and an AlbedoHtml in the other"
        );
        assert_eq!(
            lookup("cloneElement"),
            Some("globalThis.__albedo_clone_element"),
            "cloning rebuilds a vnode in one runtime and re-renders one tag in the other"
        );
    }

    /// 🔑 **A name this table advertises must be one some runtime defines.**
    ///
    /// Every export is a *string* naming a global, so a row can reference a
    /// function nobody wrote and nothing complains: the build-time import check
    /// derives its `provides` set from this same list, so the package passes
    /// the build, ships, and throws `undefined is not a function` in a user's
    /// browser on first render. That is the exact shape this codebase has hit
    /// five times — a correct mechanism reached by no input — and it is the
    /// only half of the pair that cannot be generated, because
    /// `assets/albedo-client.js` is hand-written JavaScript served to browsers.
    ///
    /// So it is checked instead. The QuickJS side is Rust in this crate and
    /// fails loudly under test; the browser side would fail in production.
    #[test]
    fn every_global_the_table_names_is_defined_by_some_runtime() {
        const CLIENT_RUNTIME: &str = include_str!("../../assets/albedo-client.js");

        let mut haystack = String::from(CLIENT_RUNTIME);
        for module in HOST_MODULES {
            haystack.push_str(module.prelude);
        }

        for module in HOST_MODULES {
            for (name, expression) in module.exports {
                // `globalThis.h.Fragment` is owned by whoever defines `h`.
                let identifier = expression
                    .trim_start_matches("globalThis.")
                    .split('.')
                    .next()
                    .expect("an export expression names something");
                let defined = [
                    format!("function {identifier}"),
                    format!("var {identifier} ="),
                    format!("const {identifier} ="),
                    format!("{identifier} = "),
                ]
                .iter()
                .any(|pattern| haystack.contains(pattern));
                assert!(
                    defined,
                    "`{name}` is advertised as `{expression}`, but nothing in                      assets/albedo-client.js or any host prelude defines                      `{identifier}` — the browser would get `undefined`"
                );
            }
        }
    }

    #[test]
    fn react_dom_provides_create_portal_and_refuses_the_render_lifecycle() {
        assert_eq!(host_record_key("react"), Some("albedo:host/react"));
        // 9.3: bare `react-dom` is now a host module, because `createPortal` is
        // implemented. This assertion is the inverse of the one it replaced.
        assert_eq!(host_record_key("react-dom"), Some("albedo:host/react-dom"));
        assert!(!REFUSED_MODULES
            .iter()
            .any(|module| module.specifiers.contains(&"react-dom")));

        // The narrowing must stay narrow. The entry points that own a render
        // lifecycle are still refused by module...
        for entry in ["react-dom/client", "react-dom/server"] {
            assert!(
                REFUSED_MODULES
                    .iter()
                    .any(|module| module.specifiers.contains(&entry)),
                "{entry} must stay refused"
            );
        }

        // ...and everything else react-dom exports is refused by ABSENCE from
        // the table, which is the sharper mechanism: the import check derives
        // `provides` from `exports`, so the error names the export. A
        // `flushSync` that quietly appeared here would be a subtly wrong batch
        // rather than a build error.
        let react_dom = HOST_MODULES
            .iter()
            .find(|module| module.specifiers.contains(&"react-dom"))
            .expect("react-dom is a host module");
        let provided: Vec<&str> = react_dom.exports.iter().map(|(name, _)| *name).collect();
        assert_eq!(provided, vec!["createPortal"]);
    }
}
