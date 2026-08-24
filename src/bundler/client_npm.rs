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
//! which, unlike a project's, bound to nothing. [`HOST_MODULES`] binds it
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
use crate::runtime::react_host::{
    build_host_module_records_script, host_provides, HOST_MODULES, REFUSED_MODULES,
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

/// The host externals both bundles share: every module
/// [`crate::runtime::react_host`] provides itself.
fn host_externals() -> BTreeMap<String, ExternalTarget> {
    let mut externals: BTreeMap<String, ExternalTarget> = BTreeMap::new();
    for module in HOST_MODULES {
        let provides: BTreeSet<String> = host_provides(module).into_iter().collect();
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
    externals
}

/// The option set every **client** bundle is built with.
#[must_use]
pub fn client_shake_options() -> ShakeOptions {
    let mut externals = host_externals();
    for module in REFUSED_MODULES {
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

/// The option set every **server** bundle is built with.
///
/// Same hosts, **no refusals**. Two deliberate differences from the client:
///
/// * A package the browser must decline can still be *loaded* on the server —
///   `react-dom` is imported all over the Radix layer, and refusing it here
///   would turn 79.6% measured npm coverage into a build error for every action
///   that merely imports something Radix-shaped. Loading is not rendering.
/// * The defines are the same, and they have to be: the QuickJS prelude already
///   sets `process.env.NODE_ENV = 'production'`, so folding against any other
///   value would emit a branch whose dependency was never bundled.
///
/// 🔑 **Why the server externalises `react` at all.** Not as an optimisation —
/// as the only way it can render. Real React's `forwardRef`/`memo` return
/// *objects*, and the QuickJS `h` shim can only call functions; an object falls
/// through to its tag-name branch and emits the literal text
/// `<[object Object]>`. There is no case in which the real React on the server
/// produced correct output, so this cannot regress one.
#[must_use]
pub fn server_shake_options() -> ShakeOptions {
    ShakeOptions::new(host_externals(), Defines::browser())
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

    out.push_str(&build_host_module_records_script());
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
        for module in HOST_MODULES {
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
    fn react_dom_client_is_refused_rather_than_stubbed() {
        let options = client_shake_options();
        // 9.3 narrowed this: bare `react-dom` is a host module now (it provides
        // `createPortal`), while the entry points that own a render lifecycle
        // stay refused. The property under test is unchanged — a capability the
        // host lacks fails at BUILD, never as a throwing stub in a browser.
        let Some(ExternalTarget::Refused { reason }) =
            options.externals().get("react-dom/client")
        else {
            panic!("react-dom/client must be refused");
        };
        assert!(reason.contains("createRoot"));
        assert!(
            !matches!(
                options.externals().get("react-dom"),
                Some(ExternalTarget::Refused { .. })
            ),
            "bare react-dom must no longer be refused wholesale"
        );
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
