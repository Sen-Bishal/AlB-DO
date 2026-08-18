//! Tier C · Phase 2 — npm in the browser: the client chunk and its linker.
//!
//! ## The constraint that decided the design
//!
//! Island scripts are **inlined into the page HTML**, and a client `import` is
//! resolved by *recursive inlining* with a depth cap of 8
//! (`rewrite_import_for_client_with_modules`). **There is no module loader in
//! the browser.** So extending the inliner is the wrong shape: a real package
//! graph blows the cap, duplicates itself once per island, and lands in
//! uncacheable inline HTML.
//!
//! 🔑 **This reuses the server's npm record format instead** — lazy factories, an
//! alias table, and `__albedo_require_record` — emitted as separate
//! content-hashed assets. That format is plain JS with nothing server-specific
//! in it, factories stay lazy, and the linker itself is *the same Rust-generated
//! string the QuickJS prelude installs* ([`npm_record_linker_script`]), so the
//! two sides cannot drift.
//!
//! ## Chunking: one chunk per package, unioned across the whole build
//!
//! An artifact is assigned to the chunk of **the package that owns it**, read
//! off its own record key — not the package that happened to demand it. So
//! `prop-types`, pulled in by `lucide-react` and by anything else, is emitted
//! **once** for the whole build rather than duplicated into each dependent's
//! chunk.
//!
//! ⚖️ The demand for a package is unioned across every island, which means a
//! route needing one icon downloads whatever icons the rest of the site needs
//! too. That is the deliberate trade: one URL per package, content-hashed,
//! cached once for the whole site and across deploys that do not change it,
//! against per-route chunks that would re-transfer the overlap on every
//! navigation. Albedo navigations are page loads, so the shared cache wins.
//!
//! ## What externalising React buys
//!
//! Phase 1 measured shaken `lucide-react` at **156 991 B, of which 3 507 B was
//! lucide's own code.** The rest was `react` (98 kB across a development *and* a
//! production build), `prop-types` (34 kB), `react-is` (13 kB) and
//! `object-assign` — dragged in by the *package's* own `import … from 'react'`,
//! which, unlike a project's, bound to nothing. [`CLIENT_HOST_MODULES`] binds it
//! to the client runtime, and [`crate::bundler::defines`] collapses the
//! `NODE_ENV` fork so only one arm of each is reachable.

use crate::bundler::defines::Defines;
use crate::bundler::npm::{
    bundle_npm_dependency_for_demand, is_bare_npm_specifier, ExternalTarget, NpmArtifact,
    ShakeOptions, STAR_DEMAND,
};
use crate::runtime::engine::stable_source_hash;
use crate::runtime::quickjs_engine::{
    npm_record_linker_script, ClientNpmBinding, ClientNpmBindings,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{ImportSpecifier, Module, ModuleDecl, ModuleExportName, ModuleItem};
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};

/// URL prefix every client npm chunk is served under.
pub const CLIENT_NPM_CHUNK_PREFIX: &str = "/_albedo/npm/";

/// URL of the browser-side linker + host modules. Fixed (not content-hashed)
/// because its bytes ride with the binary, exactly like `/_albedo/client.js`.
pub const CLIENT_NPM_RUNTIME_URL: &str = "/_albedo/npm-runtime.js";

/// A module the **browser host** provides itself, so no npm copy of it is ever
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
/// except the four called out under [`CLIENT_REFUSED_MODULES`] and in the
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

  function __albedo_isValidElement(value) {
    return value !== null && typeof value === 'object' && value.__vnode === true;
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
        ("createElement", "globalThis.h"),
        ("Fragment", "globalThis.Fragment"),
        ("forwardRef", "__albedo_forwardRef"),
        ("memo", "__albedo_memo"),
        ("createRef", "__albedo_createRef"),
        ("isValidElement", "__albedo_isValidElement"),
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
/// [`CLIENT_REFUSED_MODULES`] on `Children`).
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
        ("Fragment", "globalThis.Fragment"),
    ],
    default_is_namespace: true,
};

/// Every module the browser host provides.
pub const CLIENT_HOST_MODULES: &[HostModule] = &[REACT_HOST, JSX_RUNTIME_HOST];

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
pub const CLIENT_REFUSED_MODULES: &[RefusedModule] = &[
    RefusedModule {
        specifiers: &["react-dom", "react-dom/client", "react-dom/server"],
        reason: "Albedo's client runtime is not react-dom — `createPortal` \
                 (TODO 9.3), `flushSync` and `createRoot` have no implementation \
                 here. A Tier-C island cannot use a package that reaches for \
                 react-dom.",
    },
];

/// The option set every **client** bundle is built with.
///
/// Composed from the host table and the browser defines, so adding a host module
/// or a refusal is a one-line change in one place.
#[must_use]
pub fn client_shake_options() -> ShakeOptions {
    let mut externals: BTreeMap<String, ExternalTarget> = BTreeMap::new();
    for module in CLIENT_HOST_MODULES {
        let mut provides: BTreeSet<String> = module
            .exports
            .iter()
            .map(|(name, _)| (*name).to_string())
            .collect();
        if module.default_is_namespace {
            provides.insert("default".to_string());
        }
        for specifier in module.specifiers {
            externals.insert(
                (*specifier).to_string(),
                ExternalTarget::Host {
                    record_key: module.record_key.to_string(),
                    provides: provides.clone(),
                },
            );
        }
    }
    for module in CLIENT_REFUSED_MODULES {
        for specifier in module.specifiers {
            externals.insert(
                (*specifier).to_string(),
                ExternalTarget::Refused {
                    reason: module.reason.to_string(),
                },
            );
        }
    }
    ShakeOptions::new(externals, Defines::browser())
}

/// The browser-side npm runtime: the shared record linker, the host modules, and
/// an inert `process`.
///
/// Served at [`CLIENT_NPM_RUNTIME_URL`], after `/_albedo/client.js` (whose
/// globals the host modules bind to) and before any chunk.
///
/// 🔒 **`process` is inert and frozen.** It exists because bundled packages read
/// `process.env.NODE_ENV`, and it carries exactly that one key — the same value
/// [`Defines::browser`] folded into the bundle, so a branch that survived the
/// fold agrees with the branch the runtime takes. Nothing else about the server's
/// environment is reachable from client code, by construction rather than by
/// filtering.
#[must_use]
pub fn build_browser_npm_runtime_script() -> String {
    let mut out = String::new();
    out.push_str(
        "// Generated by albedo (bundler::client_npm). The record linker below is\n\
         // the same string the server's QuickJS prelude installs.\n",
    );
    out.push_str(&npm_record_linker_script());
    out.push_str(&format!(
        "\n(function() {{\n  \
         if (typeof globalThis.process === 'undefined') {{\n    \
         globalThis.process = Object.freeze({{ env: Object.freeze({{ NODE_ENV: '{node_env}' }}) }});\n  \
         }}\n\
         }})();\n",
        node_env = crate::bundler::defines::NODE_ENV_VALUE
    ));

    for module in CLIENT_HOST_MODULES {
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
        // Aliases so a bundled file that resolved to this record by *specifier*
        // (a CJS `require('react')` whose resolve map was not rewritten) still
        // lands here.
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

/// One island's identity and source, as the graph builder needs it.
#[derive(Debug, Clone)]
pub struct ClientIsland<'a> {
    /// The module path the island is registered under; also the bindings key.
    pub module_path: &'a str,
    /// The island's TSX/TS source.
    pub source: &'a str,
}

/// One emitted chunk: every artifact belonging to a single npm package.
#[derive(Debug, Clone)]
pub struct ClientNpmChunk {
    /// The npm package the chunk carries.
    pub package: String,
    /// Content-hashed URL, safe to cache immutably.
    pub url: String,
    /// The chunk body — factory registrations, nothing else.
    pub script: String,
}

impl ClientNpmChunk {
    /// Transferred size of this chunk, in bytes.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.script.len()
    }
}

/// Why one island's npm import could not be resolved.
#[derive(Debug, Clone)]
pub struct ClientNpmFailure {
    /// The island that asked.
    pub module_path: String,
    /// The specifier that failed.
    pub specifier: String,
    /// The bundler's own message, verbatim.
    pub reason: String,
}

/// The whole build's client npm graph.
#[derive(Debug, Clone, Default)]
pub struct ClientNpmGraph {
    chunks: Vec<ClientNpmChunk>,
    bindings: BTreeMap<String, ClientNpmBindings>,
    /// module path → the packages its bundles touched, in chunk order.
    packages: BTreeMap<String, BTreeSet<String>>,
    failures: Vec<ClientNpmFailure>,
}

impl ClientNpmGraph {
    /// Every chunk this build emitted, in stable package order.
    #[must_use]
    pub fn chunks(&self) -> &[ClientNpmChunk] {
        &self.chunks
    }

    /// The npm bindings one island's compile needs, if it imports any package.
    #[must_use]
    pub fn bindings_for(&self, module_path: &str) -> Option<&ClientNpmBindings> {
        self.bindings.get(module_path)
    }

    /// The chunk URLs a page containing `module_paths` must load, deduped and in
    /// chunk order so the tag list is deterministic.
    #[must_use]
    pub fn chunk_urls_for<'a, I>(&self, module_paths: I) -> Vec<&str>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let needed: BTreeSet<&String> = module_paths
            .into_iter()
            .filter_map(|path| self.packages.get(path))
            .flatten()
            .collect();
        self.chunks
            .iter()
            .filter(|chunk| needed.contains(&chunk.package))
            .map(|chunk| chunk.url.as_str())
            .collect()
    }

    /// Resolve a chunk URL to its body, for the asset handler.
    #[must_use]
    pub fn chunk_by_url(&self, url: &str) -> Option<&ClientNpmChunk> {
        self.chunks.iter().find(|chunk| chunk.url == url)
    }

    /// Everything that failed to resolve, for the build report and the island
    /// compile's error message.
    #[must_use]
    pub fn failures(&self) -> &[ClientNpmFailure] {
        &self.failures
    }

    /// `true` when no island imports npm at all — the common case today, and the
    /// signal to emit no runtime or chunk tags.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// Build the client npm graph for a whole build.
///
/// `project_root` is the directory the `node_modules` search starts from.
///
/// Nothing here fails the build: a specifier that will not resolve is recorded
/// in [`ClientNpmGraph::failures`] and simply produces no binding, so the
/// island's own compile refuses it — loudly, at the one place that already owns
/// that error and already names the island.
#[must_use]
pub fn build_client_npm_graph(project_root: &Path, islands: &[ClientIsland<'_>]) -> ClientNpmGraph {
    let options = client_shake_options();

    // Pass 1 — the union demand per specifier, across every island.
    let mut demand: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut per_island: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for island in islands {
        let scanned = scan_client_npm_demand(island.source);
        if scanned.is_empty() {
            continue;
        }
        for (specifier, names) in &scanned {
            demand
                .entry(specifier.clone())
                .or_default()
                .extend(names.iter().cloned());
        }
        per_island.insert(island.module_path.to_string(), scanned);
    }

    // Pass 2 — shake each specifier once, against the unioned demand.
    let mut graph = ClientNpmGraph::default();
    let mut resolved: BTreeMap<String, BundledSpecifier> = BTreeMap::new();
    let mut artifacts_by_package: BTreeMap<String, Vec<NpmArtifact>> = BTreeMap::new();
    let mut emitted_keys: BTreeSet<String> = BTreeSet::new();

    for (specifier, names) in &demand {
        match bundle_npm_dependency_for_demand(project_root, specifier, names, &options) {
            Ok(bundle) => {
                let mut packages = BTreeSet::new();
                for artifact in &bundle.artifacts {
                    let package = owning_package(&artifact.key, &bundle.package_name);
                    packages.insert(package.clone());
                    // Deduped by record key across the whole build: `prop-types`
                    // reached from two packages is emitted once.
                    if emitted_keys.insert(artifact.key.clone()) {
                        artifacts_by_package
                            .entry(package)
                            .or_default()
                            .push(artifact.clone());
                    }
                }
                resolved.insert(
                    specifier.clone(),
                    BundledSpecifier {
                        bindings: bundle.bindings,
                        packages,
                    },
                );
            }
            Err(err) => {
                for (module_path, scanned) in &per_island {
                    if scanned.contains_key(specifier) {
                        graph.failures.push(ClientNpmFailure {
                            module_path: module_path.clone(),
                            specifier: specifier.clone(),
                            reason: err.to_string(),
                        });
                    }
                }
            }
        }
    }

    // Pass 3 — one content-hashed chunk per package.
    for (package, artifacts) in artifacts_by_package {
        let mut script = String::new();
        for artifact in &artifacts {
            script.push_str(&artifact.script);
            if !script.ends_with('\n') {
                script.push('\n');
            }
        }
        let url = format!(
            "{CLIENT_NPM_CHUNK_PREFIX}{slug}.{hash:016x}.js",
            slug = slugify_package(&package),
            hash = stable_source_hash(&script)
        );
        graph.chunks.push(ClientNpmChunk {
            package,
            url,
            script,
        });
    }

    // Pass 4 — per-island bindings and chunk membership.
    for (module_path, scanned) in per_island {
        let mut bindings = ClientNpmBindings::default();
        let mut packages = BTreeSet::new();
        for specifier in scanned.keys() {
            let Some(bundled) = resolved.get(specifier) else {
                continue;
            };
            packages.extend(bundled.packages.iter().cloned());
            for (name, export) in &bundled.bindings {
                bindings.insert(
                    specifier,
                    name,
                    ClientNpmBinding {
                        record_key: export.record_key.clone(),
                        export_name: export.export_name.clone(),
                    },
                );
            }
        }
        if !packages.is_empty() {
            graph.packages.insert(module_path.clone(), packages);
        }
        if !bindings.is_empty() {
            graph.bindings.insert(module_path, bindings);
        }
    }

    graph
}

/// One resolved specifier, kept between the shake and the per-island pass.
struct BundledSpecifier {
    bindings: BTreeMap<String, crate::bundler::npm::ResolvedExport>,
    packages: BTreeSet<String>,
}

/// The package a record key belongs to — `npm:<name>@<version>/<path>`.
///
/// This, not the demanding specifier, is what decides a chunk: an artifact
/// belongs to whoever published it, so a shared dependency is emitted once for
/// the whole build.
fn owning_package(record_key: &str, fallback: &str) -> String {
    let Some(rest) = record_key.strip_prefix("npm:") else {
        // An alias artifact is keyed by the bare specifier, not a record key.
        return fallback.to_string();
    };
    // `@scope/name@version/...` — the version separator is the LAST `@`
    // before the first `/` that follows the (possibly scoped) name.
    let name_end = rest
        .char_indices()
        .skip(usize::from(rest.starts_with('@')))
        .find(|(_, c)| *c == '@')
        .map(|(index, _)| index);
    match name_end {
        Some(index) => rest[..index].to_string(),
        None => fallback.to_string(),
    }
}

/// A package name as a filesystem/URL-safe slug: `@scope/pkg` → `scope__pkg`.
fn slugify_package(package: &str) -> String {
    package
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The bare npm specifiers an island imports, and the names it binds from each.
///
/// A parse failure yields an empty map — discovery must never fail a build the
/// component parser already accepted; the island's own compile is where a broken
/// source is reported.
///
/// A **side-effect-only** import (`import "some-polyfill"`) demands
/// [`STAR_DEMAND`], which declines shaking and takes the package whole. That is
/// the only correct reading: the import exists precisely for what running the
/// module does, and no export graph can see that.
#[must_use]
pub fn scan_client_npm_demand(source: &str) -> BTreeMap<String, BTreeSet<String>> {
    let Some(module) = parse_island_module(source) else {
        return BTreeMap::new();
    };

    let mut demand: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        let (specifier, names): (String, BTreeSet<String>) = match decl {
            ModuleDecl::Import(import) => {
                if import.type_only {
                    continue;
                }
                let names = import
                    .specifiers
                    .iter()
                    .filter_map(|specifier| match specifier {
                        ImportSpecifier::Default(_) => Some("default".to_string()),
                        ImportSpecifier::Namespace(_) => Some(STAR_DEMAND.to_string()),
                        ImportSpecifier::Named(named) if !named.is_type_only => Some(
                            named
                                .imported
                                .as_ref()
                                .map_or_else(|| named.local.sym.to_string(), export_name_text),
                        ),
                        ImportSpecifier::Named(_) => None,
                    })
                    .collect::<BTreeSet<_>>();
                let names = if names.is_empty() {
                    // Side-effect import: take the module whole.
                    [STAR_DEMAND.to_string()].into_iter().collect()
                } else {
                    names
                };
                (import.src.value.to_string(), names)
            }
            ModuleDecl::ExportNamed(named) => {
                let Some(src) = named.src.as_ref() else {
                    continue;
                };
                let names = named
                    .specifiers
                    .iter()
                    .map(|specifier| match specifier {
                        swc_ecma_ast::ExportSpecifier::Named(entry) => export_name_text(&entry.orig),
                        swc_ecma_ast::ExportSpecifier::Default(_) => "default".to_string(),
                        swc_ecma_ast::ExportSpecifier::Namespace(_) => STAR_DEMAND.to_string(),
                    })
                    .collect();
                (src.value.to_string(), names)
            }
            ModuleDecl::ExportAll(all) => (
                all.src.value.to_string(),
                [STAR_DEMAND.to_string()].into_iter().collect(),
            ),
            _ => continue,
        };

        if !is_bare_npm_specifier(&specifier) {
            continue;
        }
        demand.entry(specifier).or_default().extend(names);
    }
    demand
}

fn export_name_text(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(literal) => literal.value.to_string(),
    }
}

/// Parse an island source as TSX. The island *is* TSX by construction (it is a
/// project component), so one syntax is enough.
fn parse_island_module(source: &str) -> Option<Module> {
    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom("island".to_string()).into(),
        source.to_string(),
    );
    Parser::new(
        Syntax::Typescript(TsSyntax {
            tsx: true,
            decorators: true,
            ..Default::default()
        }),
        StringInput::from(&*file),
        None,
    )
    .parse_module()
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_table_and_the_emitted_runtime_cannot_disagree() {
        let script = build_browser_npm_runtime_script();
        let options = client_shake_options();
        for module in CLIENT_HOST_MODULES {
            for (name, _) in module.exports {
                assert!(
                    script.contains(&format!("__albedo_exports['{name}']")),
                    "{name} is advertised but not emitted"
                );
            }
            for specifier in module.specifiers {
                let Some(ExternalTarget::Host { provides, .. }) = options.externals().get(*specifier)
                else {
                    panic!("{specifier} should be a host external");
                };
                for (name, _) in module.exports {
                    assert!(provides.contains(*name), "{specifier} must accept {name}");
                }
            }
        }
    }

    #[test]
    fn the_runtime_carries_the_shared_linker_and_an_inert_process() {
        let script = build_browser_npm_runtime_script();
        assert!(script.contains("__albedo_require_record"));
        assert!(script.contains("__ALBEDO_NPM_FACTORIES"));
        assert!(
            script.contains("Object.freeze"),
            "`process` must be inert, not a writable bag the page can extend"
        );
        assert!(
            script.contains(crate::bundler::defines::NODE_ENV_VALUE),
            "the stub must report the value the bundle was folded against"
        );
    }

    #[test]
    fn react_dom_is_refused_rather_than_stubbed() {
        let options = client_shake_options();
        let Some(ExternalTarget::Refused { reason }) = options.externals().get("react-dom") else {
            panic!("react-dom must be refused");
        };
        assert!(reason.contains("createPortal"));
    }

    #[test]
    fn demand_scanning_reads_every_import_form() {
        let demand = scan_client_npm_demand(
            r#"
            import { Check, ArrowLeft as Back } from "lucide-react";
            import clsx from "clsx";
            import * as dates from "date-fns";
            import "some-polyfill";
            import type { Foo } from "typed-only";
            import { useState } from "react";
            import local from "./local";
            "#,
        );
        assert_eq!(
            demand.get("lucide-react").map(|d| d.iter().cloned().collect::<Vec<_>>()),
            Some(vec!["ArrowLeft".to_string(), "Check".to_string()]),
            "the IMPORTED name is what the package exports, not the local alias"
        );
        assert_eq!(
            demand.get("clsx"),
            Some(&["default".to_string()].into_iter().collect())
        );
        assert_eq!(
            demand.get("date-fns"),
            Some(&[STAR_DEMAND.to_string()].into_iter().collect())
        );
        assert_eq!(
            demand.get("some-polyfill"),
            Some(&[STAR_DEMAND.to_string()].into_iter().collect()),
            "a side-effect import must take the module whole"
        );
        assert!(!demand.contains_key("typed-only"), "type-only imports erase");
        assert!(!demand.contains_key("react"), "framework runtime imports bind to globals");
        assert!(!demand.contains_key("./local"), "relative imports are inlined");
    }

    #[test]
    fn an_unparseable_island_yields_no_demand_rather_than_a_panic() {
        assert!(scan_client_npm_demand("function ( { ) }").is_empty());
    }

    #[test]
    fn a_record_key_names_its_own_package() {
        assert_eq!(owning_package("npm:clsx@2.1.0/dist/clsx.m.js", "x"), "clsx");
        assert_eq!(
            owning_package("npm:@radix-ui/react-slot@1.0.2/dist/index.mjs", "x"),
            "@radix-ui/react-slot"
        );
        // An alias artifact is keyed by the bare specifier and falls back.
        assert_eq!(owning_package("lucide-react", "lucide-react"), "lucide-react");
    }

    #[test]
    fn slugs_are_url_safe() {
        assert_eq!(slugify_package("@radix-ui/react-slot"), "_radix-ui_react-slot");
        assert_eq!(slugify_package("date-fns"), "date-fns");
    }
}
