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
//! ## The one divergence, and how it stays a divergence of one
//!
//! `isValidElement` is the only export whose implementation cannot be shared:
//! the browser's element is a vnode (`value.__vnode === true`), the server's is
//! an `AlbedoHtml`. It routes through `globalThis.__albedo_is_element`, which
//! each runtime defines for itself — so the *table* stays single-sourced and the
//! difference is one named global rather than two tables.
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
        ("createElement", "globalThis.h"),
        ("Fragment", "globalThis.h.Fragment"),
        ("forwardRef", "__albedo_forwardRef"),
        ("memo", "__albedo_memo"),
        ("createRef", "__albedo_createRef"),
        ("isValidElement", "globalThis.__albedo_is_element"),
        ("createContext", "globalThis.createContext"),
        ("useState", "globalThis.useState"),
        ("useEffect", "globalThis.useEffect"),
        // ⚠️ See the type-level comment: one effect phase, so layout effects run
        // after paint.
        ("useLayoutEffect", "globalThis.useEffect"),
        ("useRef", "globalThis.useRef"),
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
/// 🔑 **`jsx` is where `props.children` becomes variadic children**, which is the
/// shape `h` takes. A package compiled with the automatic runtime therefore
/// renders its children correctly even though the classic
/// `createElement(Component, props, child)` path does not (see
/// [`REFUSED_MODULES`] on `Children`).
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

/// Every module the browser host provides.
pub const HOST_MODULES: &[HostModule] = &[REACT_HOST, JSX_RUNTIME_HOST];

/// Modules a client bundle refuses, with the reason a user sees.
///
/// `react-dom` is the one that matters: `createPortal` needs the SSR renderer
/// and hydration to agree on where portal content lands in the HTML, which is
/// `TODO.md` item 9.3 and genuinely unbuilt. Shipping a stub that throws would
/// turn a build error into a blank island.
///
/// 🪤 **`react-is` was briefly on this list and should not have been.** It is
/// ordinary JavaScript that reads `$$typeof` tags and works standalone, and
/// refusing it turned a bundle that would have built into a build error. It is
/// also unreachable in practice once `NODE_ENV` folds, because the only thing
/// that imports it is `prop-types`' development arm. **A refusal must name a
/// capability the host genuinely lacks, not a package that looks React-shaped.**
pub const REFUSED_MODULES: &[RefusedModule] = &[
    RefusedModule {
        specifiers: &["react-dom", "react-dom/client", "react-dom/server"],
        reason: "Albedo's client runtime is not react-dom — `createPortal` \
                 (TODO 9.3), `flushSync` and `createRoot` have no implementation \
                 here. A Tier-C island cannot use a package that reaches for \
                 react-dom.",
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
    }

    #[test]
    fn react_is_a_host_and_react_dom_is_only_refused() {
        assert_eq!(host_record_key("react"), Some("albedo:host/react"));
        assert_eq!(host_record_key("react-dom"), None);
        assert!(REFUSED_MODULES
            .iter()
            .any(|module| module.specifiers.contains(&"react-dom")));
    }
}
