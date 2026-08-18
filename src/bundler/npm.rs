//! A2 · npm dependency bundling — resolve a bare specifier (`zod`,
//! `date-fns/addDays`) through `node_modules` and lower the reachable module
//! graph to QuickJS-loadable artifacts.
//!
//! ## Shape
//!
//! There is deliberately **no scope-hoisting bundler** here. The runtime
//! already links modules through a record table
//! (`globalThis.__ALBEDO_MODULES`), so an npm package lowers naturally to one
//! **lazy factory per file** plus an **alias** mapping the bare specifier to
//! the package's entry record:
//!
//! ```text
//! __ALBEDO_NPM_FACTORIES["npm:zod@4.4.3/index.js"] = function(exports) { … };
//! __ALBEDO_NPM_ALIASES["zod"] = "npm:zod@4.4.3/index.js";
//! ```
//!
//! Factories are registered eagerly (cheap — a function definition) but run
//! lazily and memoized on first `__albedo_require_record` access, with the
//! record **published before the factory body runs**. That is exactly Node's
//! CommonJS cycle discipline: an import cycle observes a partially-initialized
//! record instead of deadlocking or recursing forever, so no topological sort
//! is needed and real-world ESM graphs (date-fns is ~250 reachable files) load
//! in any order.
//!
//! ## Resolution semantics (narrowed Node)
//!
//! * `exports` maps with conditional targets — conditions are checked in the fixed priority
//!   `import` → `module` → `default` → `require` (Node iterates object key order against the active
//!   condition set; for the import-context set the observable difference is negligible and the
//!   fixed order keeps the resolver deterministic without order-preserving JSON parsing).
//! * Subpath maps including single-`*` wildcard patterns.
//! * `module` / `main` / `index.js` fallbacks when `exports` is absent.
//! * Relative-import file probing: exact, `.js`, `.mjs`, `.cjs`, `.json`, `<dir>/index.js`,
//!   `<dir>/index.cjs`.
//! * `.js` files classify as ESM/CJS by the nearest `package.json` `"type"`, exactly like Node.
//!
//! Anything unresolvable fails **loudly** with the file and specifier that
//! caused it — never a silent fallthrough.

use crate::bundler::defines::{fold_defines, Defines};
use crate::runtime::engine::stable_source_hash;
use crate::runtime::quickjs_engine::{compile_npm_module_script, NpmModuleFormat};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{
    CallExpr, Callee, Decl, Expr, ExportSpecifier, ImportSpecifier, Lit, Module, ModuleDecl,
    ModuleExportName, ModuleItem, Pat, Program,
};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

/// Hard cap on the number of files one bare specifier may pull in. A runaway
/// graph (or a resolution bug walking outside the package) fails loudly
/// instead of bundling half of `node_modules`.
const MAX_GRAPH_FILES: usize = 4096;

/// Conditions checked against `exports` condition objects, in priority order.
const EXPORT_CONDITIONS: [&str; 4] = ["import", "module", "default", "require"];

/// A bundling failure. Every variant carries enough context to point at the
/// exact file/specifier that broke, because these surface as build/dev errors.
#[derive(Debug, thiserror::Error)]
pub enum NpmBundleError {
    /// The bare specifier's package directory was not found in any
    /// `node_modules` directory from the search root upward.
    #[error("npm package '{package}' not found in node_modules (searched upward from '{searched_from}')")]
    PackageNotFound {
        /// Package name extracted from the bare specifier.
        package: String,
        /// Directory the upward search started from.
        searched_from: PathBuf,
    },
    /// `package.json` was unreadable or invalid.
    #[error("failed to read package.json for '{package}' at '{path}': {message}")]
    PackageJson {
        /// Package the manifest belongs to.
        package: String,
        /// Path of the offending `package.json`.
        path: PathBuf,
        /// Underlying error description.
        message: String,
    },
    /// The `exports` map exists but does not expose the requested subpath.
    #[error("package '{package}' does not export subpath '{subpath}' (exports map has no matching entry)")]
    SubpathNotExported {
        /// Package whose exports were consulted.
        package: String,
        /// The subpath that failed to resolve (`.` for the package root).
        subpath: String,
    },
    /// A specifier resolved to a path that does not exist on disk (after
    /// extension/index probing).
    #[error("could not resolve '{specifier}' imported from '{importer}' (no file at '{tried}')")]
    FileNotFound {
        /// The raw specifier as written in the importing file.
        specifier: String,
        /// The importing file (or the bare specifier itself for entries).
        importer: String,
        /// The probed base path.
        tried: PathBuf,
    },
    /// A file in the graph could not be read.
    #[error("failed to read '{path}': {message}")]
    Io {
        /// File that failed to read.
        path: PathBuf,
        /// Underlying error description.
        message: String,
    },
    /// A file in the graph could not be parsed or lowered to a record script.
    #[error("failed to compile npm module '{key}' ({path}): {message}")]
    Compile {
        /// The record key of the failing module.
        key: String,
        /// Absolute path of the failing file.
        path: PathBuf,
        /// Parser/lowering error description.
        message: String,
    },
    /// The reachable graph exceeded [`MAX_GRAPH_FILES`].
    #[error("npm dependency graph for '{specifier}' exceeded {MAX_GRAPH_FILES} files — refusing to bundle")]
    GraphTooLarge {
        /// The bare specifier whose graph blew the cap.
        specifier: String,
    },
    /// A package imported a specifier the client host declines to provide.
    ///
    /// 🔑 **Refused at build, never stubbed to throw at run time.** A stub that
    /// throws moves a fact the compiler already knows into the user's browser,
    /// where it arrives as a blank island instead of a build error.
    #[error("'{specifier}' (imported from '{importer}') is not available to client code: {reason}")]
    ExternalRefused {
        /// The refused specifier as written.
        specifier: String,
        /// File that imported it.
        importer: PathBuf,
        /// Why the host declines it.
        reason: String,
    },
    /// A package imported a *name* the host module does not provide.
    #[error("'{specifier}' (imported from '{importer}') does not provide '{name}' in Albedo's client runtime — it provides: {provides}")]
    ExternalExportMissing {
        /// The host specifier.
        specifier: String,
        /// The name the importer asked for.
        name: String,
        /// File that imported it.
        importer: PathBuf,
        /// Comma-separated list of what the host does provide.
        provides: String,
    },
}

/// One QuickJS-loadable artifact: a factory-registration script (or an alias
/// script) plus the source hash used for idempotent reloads.
#[derive(Debug, Clone)]
pub struct NpmArtifact {
    /// Record key (`npm:<pkg>@<version>/<relpath>`) or, for alias artifacts,
    /// the bare specifier the alias is for.
    pub key: String,
    /// Ready-to-eval registration script.
    pub script: String,
    /// Stable hash of the originating source (alias artifacts hash the alias
    /// script itself).
    pub source_hash: u64,
}

/// The bundled, loadable form of one bare npm specifier.
#[derive(Debug, Clone)]
pub struct NpmDependencyBundle {
    /// The bare specifier as requested (`zod`, `date-fns/addDays`).
    pub specifier: String,
    /// Resolved package name (`zod`, `@scope/pkg`).
    pub package_name: String,
    /// Resolved package version (from its `package.json`).
    pub package_version: String,
    /// Record key of the entry module the specifier aliases to.
    pub entry_key: String,
    /// Per-file factory artifacts followed by the alias artifact. Load order
    /// is irrelevant (factories are lazy); the vector is deterministic anyway.
    pub artifacts: Vec<NpmArtifact>,
}

/// `true` when `specifier` is a bare npm specifier — not relative, not
/// absolute, not a URL, and not one of the framework's own runtime modules
/// (`react`, `react-dom`, `albedo`, which bind to engine globals instead).
#[must_use]
pub fn is_bare_npm_specifier(specifier: &str) -> bool {
    let s = specifier.trim();
    if s.is_empty()
        || s.starts_with('.')
        || s.starts_with('/')
        || s.starts_with('\\')
        || s.contains("://")
        || s.ends_with(".css")
    {
        return false;
    }
    // `albedo/forge` and `albedo/sources` are the generated binding modules:
    // types only, no runtime record, folded away by the transpile. Resolving
    // either through node_modules would look for a package that deliberately
    // does not exist.
    if matches!(s, "react" | "react-dom" | "albedo" | "albedo/forge")
        || s == crate::transforms::shared_slots::SOURCE_BINDINGS_MODULE
        || s.starts_with("react/")
    {
        return false;
    }
    // A Windows drive path ("C:\…" / "C:/…") is absolute, not bare.
    let mut chars = s.chars();
    if let (Some(first), Some(':'), Some(third)) =
        (chars.next(), s.chars().nth(1), s.chars().nth(2))
    {
        if first.is_ascii_alphabetic() && (third == '/' || third == '\\') {
            return false;
        }
    }
    true
}

/// Split a bare specifier into `(package_name, subpath)`. The subpath is `.`
/// for the package root, `./x/y` otherwise — the shapes `exports` maps key on.
fn split_bare_specifier(specifier: &str) -> (String, String) {
    let mut segments = specifier.splitn(if specifier.starts_with('@') { 3 } else { 2 }, '/');
    let package = if specifier.starts_with('@') {
        let scope = segments.next().unwrap_or_default();
        let name = segments.next().unwrap_or_default();
        format!("{scope}/{name}")
    } else {
        segments.next().unwrap_or_default().to_string()
    };
    let subpath = match segments.next() {
        Some(rest) if !rest.is_empty() => format!("./{rest}"),
        _ => ".".to_string(),
    };
    (package, subpath)
}

/// Minimal `package.json` view — only the fields resolution needs.
#[derive(Debug, Deserialize, Default)]
struct PackageManifest {
    name: Option<String>,
    version: Option<String>,
    #[serde(rename = "type")]
    module_type: Option<String>,
    main: Option<String>,
    module: Option<String>,
    exports: Option<serde_json::Value>,
}

fn read_manifest(package: &str, path: &Path) -> Result<PackageManifest, NpmBundleError> {
    let raw = std::fs::read_to_string(path).map_err(|err| NpmBundleError::PackageJson {
        package: package.to_string(),
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    serde_json::from_str(&raw).map_err(|err| NpmBundleError::PackageJson {
        package: package.to_string(),
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

/// Walk upward from `start_dir` looking for `node_modules/<package>`.
fn find_package_dir(start_dir: &Path, package: &str) -> Option<PathBuf> {
    let mut dir = Some(start_dir);
    while let Some(current) = dir {
        let candidate = current.join("node_modules").join(package);
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
}

/// Resolve a conditional `exports` target value to a relative path string.
/// Handles string targets, condition objects (fixed priority), and arrays
/// (first resolvable wins). `wildcard` replaces `*` in string targets.
fn resolve_export_target(target: &serde_json::Value, wildcard: Option<&str>) -> Option<String> {
    match target {
        serde_json::Value::String(s) => {
            let resolved = match wildcard {
                Some(capture) => s.replace('*', capture),
                None => s.clone(),
            };
            Some(resolved)
        }
        serde_json::Value::Object(conditions) => {
            for condition in EXPORT_CONDITIONS {
                if let Some(next) = conditions.get(condition) {
                    if let Some(resolved) = resolve_export_target(next, wildcard) {
                        return Some(resolved);
                    }
                }
            }
            None
        }
        serde_json::Value::Array(targets) => targets
            .iter()
            .find_map(|candidate| resolve_export_target(candidate, wildcard)),
        _ => None,
    }
}

/// Resolve `subpath` (`.` or `./x`) through a package's `exports` field.
fn resolve_exports_subpath(exports: &serde_json::Value, subpath: &str) -> Option<String> {
    // A bare string / condition object / array exports value applies to the
    // root subpath only.
    let is_subpath_map = exports
        .as_object()
        .map(|map| map.keys().all(|key| key.starts_with('.')))
        .unwrap_or(false);

    if !is_subpath_map {
        if subpath == "." {
            return resolve_export_target(exports, None);
        }
        return None;
    }

    let map = exports.as_object().expect("checked above");

    // Exact match first.
    if let Some(target) = map.get(subpath) {
        return resolve_export_target(target, None);
    }

    // Wildcard patterns: pick the match with the longest static prefix, like
    // Node's PATTERN_KEY_COMPARE.
    let mut best: Option<(usize, &str, &serde_json::Value)> = None;
    for (pattern, target) in map {
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            continue;
        };
        if subpath.len() >= prefix.len().saturating_add(suffix.len())
            && subpath.starts_with(prefix)
            && subpath.ends_with(suffix)
        {
            let better = best.map_or(true, |(len, _, _)| prefix.len() > len);
            if better {
                if let Some(capture) =
                    subpath.get(prefix.len()..subpath.len().saturating_sub(suffix.len()))
                {
                    best = Some((prefix.len(), capture, target));
                }
            }
        }
    }
    let (_, capture, target) = best?;
    resolve_export_target(target, Some(capture))
}

/// Fold `.` and `..` components out of a path without touching the
/// filesystem, so record keys and the visited set are canonical. (Not
/// `fs::canonicalize`, which on Windows produces `\\?\`-prefixed paths that
/// break `strip_prefix` against plainly-joined package dirs.)
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// How many nested `package.json` redirections a directory probe will follow
/// before giving up.
///
/// One is enough for every real package; the bound exists because a
/// `package.json` whose `main` points back at its own directory would otherwise
/// recurse forever, and that is a malformed package we should decline rather
/// than hang on.
const MAX_DIRECTORY_INDIRECTION: u8 = 4;

/// Probe `base` the way Node resolves a path-ish specifier: exact file, then
/// appended extensions, then **the directory's own `package.json`**, then
/// directory index files.
fn probe_file(base: &Path) -> Option<PathBuf> {
    probe_file_inner(base, 0)
}

fn probe_file_inner(base: &Path, depth: u8) -> Option<PathBuf> {
    let base = &normalize_path(base);
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    for ext in ["js", "mjs", "cjs", "json"] {
        let mut candidate = base.as_os_str().to_owned();
        candidate.push(".");
        candidate.push(ext);
        let candidate = PathBuf::from(candidate);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    if base.is_dir() {
        // Node's LOAD_AS_DIRECTORY step 1, and it has to come **before** the
        // index probe because a directory may legitimately carry both.
        if depth < MAX_DIRECTORY_INDIRECTION {
            if let Some(target) = directory_entry_target(base) {
                if let Some(resolved) = probe_file_inner(&base.join(target), depth + 1) {
                    return Some(resolved);
                }
                // A manifest that points nowhere falls through to the index
                // probe rather than failing: the folder may still be resolvable
                // the ordinary way, and a broken `main` should not veto that.
            }
        }
        for index in ["index.js", "index.mjs", "index.cjs", "index.json"] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// The entry a directory's own `package.json` names, if it has one.
///
/// ## The shape this exists for
///
/// The pre-`exports` convention for publishing a secondary entry point: ship a
/// directory containing **nothing but a `package.json`** that redirects into
/// the real build output. `react-remove-scroll-bar/constants/package.json` is
/// the case that found this — it holds only
/// `"module": "../dist/es2015/constants.js"` — and **every Radix primitive
/// depends on it**, so the whole shadcn layer failed to resolve while this
/// probe looked for `constants.js` and `constants/index.js` and concluded the
/// file did not exist.
///
/// 🪤 **It was invisible until the format classifier was fixed.** These packages
/// were being lowered as CJS, so their `import` statements were never scanned,
/// so this dependency was never resolved and never had the chance to fail —
/// they "loaded" with an unwalked graph and threw later at evaluation instead.
/// One bug was hiding the other.
///
/// 🔑 `module` before `main`, mirroring [`resolve_bare_entry`]: both fields name
/// the same code and the former is the ESM build, which is the one this pipeline
/// wants. Node reads only `main`, which is why Node reaches a different file
/// here and why this is not a place to copy Node exactly.
///
/// The directory's `exports` map is deliberately **not** consulted: `exports`
/// governs a *package* reached through `node_modules`, not a folder reached by
/// path, and these redirect stubs predate it.
fn directory_entry_target(dir: &Path) -> Option<String> {
    let manifest_path = dir.join("package.json");
    if !manifest_path.is_file() {
        return None;
    }
    let raw = std::fs::read_to_string(&manifest_path).ok()?;
    let manifest: PackageManifest = serde_json::from_str(&raw).ok()?;
    manifest.module.or(manifest.main)
}

/// How a resolved file should be lowered.
///
/// ## Why the source is consulted and Node's rule is not enough
///
/// 🔴 **Node's rule — extension, then the nearest `package.json` `"type"`,
/// defaulting to CJS — is exactly what this used to implement, and it is wrong
/// *here*, because this resolver does not resolve like Node.** Entry selection
/// prefers the **`"module"` field** (`manifest.module.or(manifest.main)`), as
/// every bundler does, and that field means ESM **by definition**. Node never
/// reads it, so Node never has to reconcile the two. We do.
///
/// `clsx` is the minimal case and the one that found this: its manifest carries
/// `"module": "dist/clsx.m.js"` and no `"type"`, so the resolver picks an ESM
/// file whose extension is `.js` (the `.m` is part of the *basename*) and the
/// old classifier called it CJS. The CJS lowering wraps source verbatim in a
/// `function(module, exports, require, …)` IIFE — so `export function clsx(){}`
/// landed inside a function body and QuickJS answered *"unsupported keyword:
/// export"*. **The lowering was innocent; the label was wrong.**
///
/// 🔑 **Provenance alone would not have been enough.** Marking only the entry
/// ESM fixes `clsx` and misses `date-fns`, whose `esm/index.js` entry relatively
/// imports a whole tree of ESM `.js` files that no manifest describes. The
/// format is a property of the *file*, so the file is what gets asked.
///
/// Node's answer is still taken first when it says ESM (an explicit
/// `"type": "module"` is a declaration, not a guess); the content check only
/// ever *promotes* a would-be CJS file, and only on evidence.
fn classify_format(path: &Path, source: &str) -> NpmModuleFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mjs") => NpmModuleFormat::Esm,
        Some("cjs") => NpmModuleFormat::Cjs,
        Some("json") => NpmModuleFormat::Json,
        _ => {
            // `.js` (or anything else): nearest package.json `"type"` first.
            let mut dir = path.parent();
            while let Some(current) = dir {
                let manifest_path = current.join("package.json");
                if manifest_path.is_file() {
                    let module_type = std::fs::read_to_string(&manifest_path)
                        .ok()
                        .and_then(|raw| serde_json::from_str::<PackageManifest>(&raw).ok())
                        .and_then(|manifest| manifest.module_type);
                    if module_type.as_deref() == Some("module") {
                        return NpmModuleFormat::Esm;
                    }
                    break;
                }
                dir = current.parent();
            }
            if source_is_esm(source) {
                NpmModuleFormat::Esm
            } else {
                NpmModuleFormat::Cjs
            }
        }
    }
}

/// Does this source actually use module syntax?
///
/// Two stages, because the cheap one is wrong on its own and the correct one is
/// too expensive to run over every file in a 1,771-file graph like
/// `lucide-react`:
///
/// 1. A word-boundary scan for `import` / `export`. 🪤 **A plain substring test
///    is useless here** — `exports.foo`, the single most common token in CJS,
///    contains `export`. The boundary check is what keeps the parse off the
///    overwhelming majority of CJS files.
/// 2. A real parse for the files that survive it. Only a top-level
///    `ModuleDecl` (or `import.meta`) is proof; a *dynamic* `import()` call is
///    legal in CJS and must not promote the file, which is precisely the
///    distinction a text scan cannot make and a parse makes for free.
///
/// An unparseable file is left as CJS: if it will not parse as a module it
/// cannot be one, and the CJS path passes source through verbatim, so a file
/// this cannot classify still has the better of the two chances.
fn source_is_esm(source: &str) -> bool {
    if !has_module_keyword(source) {
        return false;
    }

    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom("classify".to_string()).into(),
        source.to_string(),
    );
    let Ok(module) = Parser::new(
        Syntax::Es(EsSyntax {
            jsx: false,
            decorators: true,
            ..Default::default()
        }),
        StringInput::from(&*file),
        None,
    )
    .parse_module() else {
        return false;
    };

    if module
        .body
        .iter()
        .any(|item| matches!(item, ModuleItem::ModuleDecl(_)))
    {
        return true;
    }

    // `import.meta` is an expression, not a `ModuleDecl`, so the check above
    // misses it — and a file using it is unambiguously a module. This is the
    // `import.meta only valid in module code` class of failure.
    source.contains("import.meta")
}

/// `import` or `export` present as a whole word.
///
/// Deliberately not a regex: this runs once per file across every npm graph in
/// the project, and the rule is small enough to say directly.
fn has_module_keyword(source: &str) -> bool {
    const KEYWORDS: [&str; 2] = ["import", "export"];
    let bytes = source.as_bytes();
    let is_ident_byte =
        |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$';

    for keyword in KEYWORDS {
        let mut from = 0usize;
        while let Some(offset) = source[from..].find(keyword) {
            let start = from + offset;
            let end = start + keyword.len();
            let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
            // `exports` / `imported` must not count; `export{`, `export ` and
            // `export*` must.
            let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
            if before_ok && after_ok {
                return true;
            }
            from = end;
        }
    }
    false
}

/// A package the walker has located on disk.
#[derive(Debug, Clone)]
struct ResolvedPackage {
    name: String,
    version: String,
    dir: PathBuf,
}

/// Resolve a bare specifier to its entry file, starting the `node_modules`
/// walk from `search_dir`.
fn resolve_bare_entry(
    search_dir: &Path,
    specifier: &str,
) -> Result<(ResolvedPackage, PathBuf), NpmBundleError> {
    let (package_name, subpath) = split_bare_specifier(specifier);
    let package_dir = find_package_dir(search_dir, &package_name).ok_or_else(|| {
        NpmBundleError::PackageNotFound {
            package: package_name.clone(),
            searched_from: search_dir.to_path_buf(),
        }
    })?;
    let manifest = read_manifest(&package_name, &package_dir.join("package.json"))?;
    let package = ResolvedPackage {
        name: manifest.name.unwrap_or_else(|| package_name.clone()),
        version: manifest.version.unwrap_or_else(|| "0.0.0".to_string()),
        dir: package_dir.clone(),
    };

    let relative_target = if let Some(exports) = manifest.exports.as_ref() {
        resolve_exports_subpath(exports, &subpath).ok_or_else(|| {
            NpmBundleError::SubpathNotExported {
                package: package_name.clone(),
                subpath: subpath.clone(),
            }
        })?
    } else if subpath != "." {
        subpath.clone()
    } else {
        manifest
            .module
            .or(manifest.main)
            .unwrap_or_else(|| "./index.js".to_string())
    };

    let base = package_dir.join(relative_target.trim_start_matches("./"));
    let entry = probe_file(&base).ok_or_else(|| NpmBundleError::FileNotFound {
        specifier: specifier.to_string(),
        importer: format!("<entry of '{specifier}'>"),
        tried: base,
    })?;
    Ok((package, entry))
}

/// Resolve one raw specifier as written inside `importer_path` (a file that
/// belongs to `importer_package`).
fn resolve_from_file(
    importer_package: &ResolvedPackage,
    importer_path: &Path,
    raw: &str,
) -> Result<(ResolvedPackage, PathBuf), NpmBundleError> {
    if raw.starts_with('.') {
        let base = importer_path
            .parent()
            .unwrap_or(importer_path)
            .join(raw.replace('/', std::path::MAIN_SEPARATOR_STR));
        let resolved = probe_file(&base).ok_or_else(|| NpmBundleError::FileNotFound {
            specifier: raw.to_string(),
            importer: importer_path.display().to_string(),
            tried: base,
        })?;
        return Ok((importer_package.clone(), resolved));
    }
    // Bare specifier inside a package: another package (or a self-reference),
    // resolved by walking node_modules upward from the importing file.
    let search_dir = importer_path.parent().unwrap_or(importer_path);
    resolve_bare_entry(search_dir, raw)
}

/// Canonical record key for a file inside a package.
fn record_key(package: &ResolvedPackage, file: &Path) -> String {
    let relative = file
        .strip_prefix(&package.dir)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| file.to_string_lossy().replace('\\', "/"));
    format!("npm:{}@{}/{}", package.name, package.version, relative)
}

/// One import edge out of a file: the specifier as written, plus the names the
/// importer binds from it.
///
/// The names exist for one reason — checking an import against what a **host**
/// external actually provides — and are empty for CJS, where a `require` gives
/// no static name list at all.
#[derive(Debug, Clone)]
struct FileImport {
    specifier: String,
    /// Imported names; `default` for a default import, [`STAR_DEMAND`] for a
    /// namespace import, empty for a side-effect-only import or any `require`.
    names: Vec<String>,
}

/// Every import edge out of one file, in source order.
#[derive(Debug, Clone, Default)]
struct FileImports {
    entries: Vec<FileImport>,
}

/// Raw import/require specifiers found in one parsed file, with the names each
/// one binds.
///
/// 🔑 **One parse, one truth.** [`collect_specifiers`] is a projection of this,
/// so the specifier walk and the host-export check can never see different
/// import lists for the same file.
fn collect_imports(
    path: &Path,
    source: &str,
    format: NpmModuleFormat,
) -> Result<FileImports, NpmBundleError> {
    match format {
        NpmModuleFormat::Json => Ok(FileImports::default()),
        NpmModuleFormat::Esm => {
            let module =
                parse_npm_module(path, source).map_err(|message| NpmBundleError::Compile {
                    key: path.display().to_string(),
                    path: path.to_path_buf(),
                    message,
                })?;
            Ok(esm_imports(&module))
        }
        NpmModuleFormat::Cjs => {
            let program =
                parse_npm_program(path, source).map_err(|message| NpmBundleError::Compile {
                    key: path.display().to_string(),
                    path: path.to_path_buf(),
                    message,
                })?;
            let mut collector = RequireCollector::default();
            program.visit_with(&mut collector);
            Ok(FileImports {
                entries: collector
                    .specifiers
                    .into_iter()
                    .map(|specifier| FileImport {
                        specifier,
                        names: Vec::new(),
                    })
                    .collect(),
            })
        }
    }
}

/// Raw import/require specifiers found in one parsed file.
fn collect_specifiers(
    path: &Path,
    source: &str,
    format: NpmModuleFormat,
) -> Result<Vec<String>, NpmBundleError> {
    Ok(collect_imports(path, source, format)?
        .entries
        .into_iter()
        .map(|entry| entry.specifier)
        .collect())
}

/// Parse an npm ESM file (plain JS — no JSX/TS) into an swc module.
fn parse_npm_module(path: &Path, source: &str) -> Result<Module, String> {
    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom(format!("npm:{}", path.display())).into(),
        source.to_string(),
    );
    let mut parser = Parser::new(
        Syntax::Es(EsSyntax::default()),
        StringInput::from(&*file),
        None,
    );
    parser
        .parse_module()
        .map_err(|err| format!("parse error: {err:?}"))
}

/// Parse a CJS file as a full program (script first, module as a fallback for
/// files that mix `import`-less syntax Node still treats as CJS).
fn parse_npm_program(path: &Path, source: &str) -> Result<Program, String> {
    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom(format!("npm:{}", path.display())).into(),
        source.to_string(),
    );
    let mut parser = Parser::new(
        Syntax::Es(EsSyntax::default()),
        StringInput::from(&*file),
        None,
    );
    parser
        .parse_program()
        .map_err(|err| format!("parse error: {err:?}"))
}

/// Import edges of an ESM module, with the names each binds.
///
/// `export … from` and `export * from` are import edges too — they load the
/// target — but the names they *re-export* are not names this file binds, so
/// they carry an empty list rather than a name a host would be checked against.
fn esm_imports(module: &Module) -> FileImports {
    let mut entries = Vec::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        match decl {
            ModuleDecl::Import(import) => {
                let names = import
                    .specifiers
                    .iter()
                    .map(|specifier| match specifier {
                        ImportSpecifier::Default(_) => "default".to_string(),
                        ImportSpecifier::Namespace(_) => STAR_DEMAND.to_string(),
                        ImportSpecifier::Named(named) => named
                            .imported
                            .as_ref()
                            .map_or_else(|| named.local.sym.to_string(), export_name_text),
                    })
                    .collect();
                entries.push(FileImport {
                    specifier: import.src.value.to_string(),
                    names,
                });
            }
            ModuleDecl::ExportNamed(named) => {
                if let Some(src) = named.src.as_ref() {
                    entries.push(FileImport {
                        specifier: src.value.to_string(),
                        names: Vec::new(),
                    });
                }
            }
            ModuleDecl::ExportAll(all) => entries.push(FileImport {
                specifier: all.src.value.to_string(),
                names: Vec::new(),
            }),
            _ => {}
        }
    }
    FileImports { entries }
}

/// Import sources reachable from an ESM module: `import … from`, bare
/// side-effect imports, `export … from`, and `export * from`.
fn esm_specifiers(module: &Module) -> Vec<String> {
    let mut sources = Vec::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        match decl {
            ModuleDecl::Import(import) => sources.push(import.src.value.to_string()),
            ModuleDecl::ExportNamed(named) => {
                if let Some(src) = named.src.as_ref() {
                    sources.push(src.value.to_string());
                }
            }
            ModuleDecl::ExportAll(all) => sources.push(all.src.value.to_string()),
            _ => {}
        }
    }
    sources
}

// ── Tree shaking · the export graph ────────────────────────────────────
//
// The problem, concretely: `import { Check } from "lucide-react"` reaches an
// entry that is `export * from './icons'`, whose `./icons/index.js` is 854 lines
// of `export { default as Check } from './check'`. Walking the graph from the
// entry — which is what the untargeted bundler does, correctly, for the server —
// lowers all 854 icons to ship one. For a framework whose claim is minimal
// JavaScript, that is not a size regression, it is the claim being false.
//
// 🔑 **The shape of the fix is demand-driven construction, not post-hoc
// pruning.** Pruning after the walk means 854 files have already been read,
// parsed and lowered; the cost is paid before the saving is computed. Starting
// from *(entry, names the importer actually asked for)* and pulling only what
// answers that demand does the work once.
//
// 🔑 **Re-export edges are all this needs, and that is why it is tractable.**
// Resolving `Check` to `./icons/check.js` is a walk over `export … from`
// declarations — no scope analysis, no binding resolution, no side-effect
// inference inside a function body. The barrel *is* the index.
//
// ⚠️ **Deliberate boundary: no intra-file dead-code elimination.** Once a file is
// reached, every byte of it ships. Removing an unused local inside a reached
// module is a different discipline — full scope analysis — and the measurement
// says it is not where the bytes are. Shaking `lucide-react` for one icon:
//
// ```text
// whole 848 231 B → shaken 156 991 B, of which lucide's own code is 3 507 B
// ```
//
// **97.8% of what survives is dependencies** — `react` (98 kB across three
// files, 89 kB of it `react.development.js`), `prop-types` (34 kB), `react-is`
// (13 kB), `object-assign` (3 kB). Those go away by *externalising* the client
// runtime and by substituting `NODE_ENV`, not by pruning statements. Intra-file
// DCE would be a scope-analysis pass fighting for a few hundred bytes of the
// 3 507 while 153 kB sat beside it untouched.
//
// 🪤 **And this codebase has no minifier at all** — no `swc_ecma_minifier`, no
// terser, and `CodegenConfig::default()` means `minify: false`, so client JS
// ships pretty-printed. That is a larger and more general lever than DCE (it
// applies to every island, not only npm), and it is deliberately not smuggled in
// here. What this pass removes is whole files, which is where the 854 lives.

/// Where one exported name comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExportOrigin {
    /// Defined in this file (`export const x`, `export default …`). Reaching it
    /// makes the whole file live.
    Local,
    /// `export { a as b } from "./src"` — a direct name→file edge, and the case
    /// that makes barrel pruning exact.
    ReExport { source: String, imported: String },
}

/// What a module exports, and the `export *` sources it forwards to.
#[derive(Debug, Default, Clone)]
struct ExportMap {
    /// Named exports this file resolves itself.
    named: BTreeMap<String, ExportOrigin>,
    /// `export * from "./x"` sources, in source order. A name not found in
    /// `named` may be provided by one of these, and only reading them can say
    /// which — so they are followed lazily, on a miss.
    star: Vec<String>,
    /// Every specifier this file imports for its own use. Demanded wholesale
    /// once the file is live, because without intra-file DCE we cannot know
    /// which import a used export actually needs.
    imports: Vec<String>,
}

/// Read a module's export surface.
///
/// Only `ModuleDecl`s are inspected: this answers *"which file provides name
/// N"*, and no expression can change that answer.
fn export_map(module: &Module) -> ExportMap {
    let mut map = ExportMap::default();

    for item in &module.body {
        let ModuleItem::ModuleDecl(decl) = item else {
            continue;
        };
        match decl {
            ModuleDecl::Import(import) => map.imports.push(import.src.value.to_string()),
            ModuleDecl::ExportAll(all) => map.star.push(all.src.value.to_string()),
            ModuleDecl::ExportDefaultDecl(_) | ModuleDecl::ExportDefaultExpr(_) => {
                map.named.insert("default".to_string(), ExportOrigin::Local);
            }
            ModuleDecl::ExportDecl(export_decl) => {
                for name in declared_export_names(&export_decl.decl) {
                    map.named.insert(name, ExportOrigin::Local);
                }
            }
            ModuleDecl::ExportNamed(named) => {
                // With a source this is a re-export edge; without one it is a
                // local binding exported under a name.
                let source = named.src.as_ref().map(|src| src.value.to_string());
                if source.is_none() {
                    // `export { a, b }` — the bindings are local, so the file is
                    // its own provider and its imports are demanded with it.
                    for specifier in &named.specifiers {
                        if let ExportSpecifier::Named(entry) = specifier {
                            let exported = entry
                                .exported
                                .as_ref()
                                .map_or_else(|| export_name_text(&entry.orig), export_name_text);
                            map.named.insert(exported, ExportOrigin::Local);
                        }
                    }
                    continue;
                }
                let source = source.expect("checked above");
                for specifier in &named.specifiers {
                    match specifier {
                        ExportSpecifier::Named(entry) => {
                            let imported = export_name_text(&entry.orig);
                            let exported = entry
                                .exported
                                .as_ref()
                                .map_or_else(|| imported.clone(), export_name_text);
                            map.named.insert(
                                exported,
                                ExportOrigin::ReExport {
                                    source: source.clone(),
                                    imported,
                                },
                            );
                        }
                        // `export * as ns from "./x"` — the namespace object
                        // needs every export of the target, so the target is
                        // pulled whole rather than by name.
                        ExportSpecifier::Namespace(ns) => {
                            map.named.insert(
                                export_name_text(&ns.name),
                                ExportOrigin::ReExport {
                                    source: source.clone(),
                                    imported: STAR_DEMAND.to_string(),
                                },
                            );
                        }
                        ExportSpecifier::Default(_) => {
                            map.named.insert(
                                "default".to_string(),
                                ExportOrigin::ReExport {
                                    source: source.clone(),
                                    imported: "default".to_string(),
                                },
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    map
}

/// The sentinel demand meaning *every export of this module*.
///
/// Not a valid JavaScript identifier, so it can never collide with a real
/// export name — which is what lets one `BTreeSet<String>` carry both
/// "these names" and "all of them" without a second field to keep in sync.
pub const STAR_DEMAND: &str = "*";

/// The names bound by an `export <decl>`.
fn declared_export_names(decl: &Decl) -> Vec<String> {
    match decl {
        Decl::Fn(fn_decl) => vec![fn_decl.ident.sym.to_string()],
        Decl::Class(class_decl) => vec![class_decl.ident.sym.to_string()],
        Decl::Var(var_decl) => var_decl
            .decls
            .iter()
            .filter_map(|declarator| match &declarator.name {
                Pat::Ident(binding) => Some(binding.id.sym.to_string()),
                // A destructured export (`export const { a } = …`) binds names
                // this does not enumerate. Returning none for it is safe in the
                // direction that matters: the name simply will not be found by
                // demand, and the caller falls back to taking the file whole.
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `ModuleExportName` as the string it denotes.
fn export_name_text(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::Ident(ident) => ident.sym.to_string(),
        ModuleExportName::Str(str_name) => str_name.value.to_string(),
    }
}

/// One demanded export, resolved past every barrel to the record that defines it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedExport {
    /// Record key of the file that actually defines the binding.
    pub record_key: String,
    /// The name on that record. `default` for a default export; [`STAR_DEMAND`]
    /// when the importer needs the whole namespace object.
    pub export_name: String,
}

/// A tree-shaken bundle: only the files needed to answer a specific demand.
#[derive(Debug, Clone)]
pub struct ShakenNpmBundle {
    pub specifier: String,
    pub package_name: String,
    pub package_version: String,
    /// Demanded name → the record and property to bind it to. The importer
    /// binds against this directly, so **no barrel is emitted at all**.
    pub bindings: BTreeMap<String, ResolvedExport>,
    pub artifacts: Vec<NpmArtifact>,
    /// True when the package could not be shaken and was taken whole.
    pub taken_whole: bool,
}

/// What a bare specifier means to a **client** bundle when the host already
/// provides it, or refuses to.
///
/// 🔑 **This is the lever that carries most of Phase 2's payload.** Shaking
/// `lucide-react` for one icon left 156 991 B of which only 3 507 B was lucide's
/// own code; the rest was `react` (98 kB), `prop-types`, `react-is` and
/// `object-assign`, pulled in by the *package's* own `import … from 'react'`. A
/// project's react import already binds to the client runtime's globals; a
/// package's did not, so every React component library shipped a second React.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalTarget {
    /// The host publishes this module itself. The walk stops here: no file is
    /// read, nothing is emitted, and importers bind to `record_key`.
    Host {
        /// Record key the host registers the module under.
        record_key: String,
        /// Names the host's record actually provides. An ESM import naming
        /// anything outside this set is a **build error** — the same discipline
        /// as a demanded name nothing exports, and for the same reason: the
        /// alternative is an `undefined` binding discovered at first render.
        provides: BTreeSet<String>,
    },
    /// The host has no implementation, and stubbing one that throws would move
    /// a build-time fact to run time. Fails the bundle, naming why.
    Refused {
        /// Human-facing reason, surfaced verbatim in the build error.
        reason: String,
    },
}

/// How a demand-driven bundle differs from the server's whole-package one.
///
/// [`Default`] is exactly the pre-externalisation behaviour — no externals, no
/// defines — so the server path and every fixture test that wants the raw walk
/// can ask for it by name.
#[derive(Debug, Clone, Default)]
pub struct ShakeOptions {
    externals: BTreeMap<String, ExternalTarget>,
    defines: Defines,
}

impl ShakeOptions {
    /// Build an option set from an externals table and a define set.
    #[must_use]
    pub fn new(externals: BTreeMap<String, ExternalTarget>, defines: Defines) -> Self {
        Self { externals, defines }
    }

    /// The externals table, for callers that need to emit matching host records.
    #[must_use]
    pub fn externals(&self) -> &BTreeMap<String, ExternalTarget> {
        &self.externals
    }
}

/// Bundle `specifier` carrying only what `demand` needs.
///
/// # Why this is a second entry point and not a parameter on the first
///
/// [`bundle_npm_dependency`] serves the **server**, which registers a package
/// whole because it cannot know which export an action will reach. This serves
/// the **browser**, where the demand set is exactly the island's import list and
/// every unshaken byte is transferred to a user. Two different questions; making
/// one function answer both with a flag would put the server's correctness at
/// the mercy of a client-side optimisation.
///
/// # How a barrel disappears
///
/// A demanded name is resolved *through* re-export edges to the file that
/// defines it, and the importer is bound to that file's record. So
/// `import { Check } from "lucide-react"` emits a reference to
/// `…/icons/check.js` and the 854-line barrel is never emitted — which is also
/// why pruning the barrel's own body is unnecessary: nothing points at it.
///
/// # What `options` adds
///
/// * **Externals** — a specifier the host already provides is never walked, so
///   a package's `import 'react'` costs zero bytes instead of 98 kB.
/// * **Defines** — `process.env.NODE_ENV` folds to `"production"` before the
///   specifier scan, so a `NODE_ENV` fork contributes *one* arm to the graph
///   rather than both. See [`crate::bundler::defines`].
///
/// # When shaking is declined
///
/// Only a package declaring `"sideEffects": false` is shaken. Without that
/// declaration a file may matter for reasons no export graph can see (a polyfill
/// installing itself, a registry populating on import), and dropping it would
/// break the package silently at runtime — the worst failure this codebase has.
/// Such a package is taken whole and says so via [`ShakenNpmBundle::taken_whole`].
/// ⚠️ A whole-taken package still goes through the **server** bundler, so it is
/// neither externalised nor define-folded: those are properties of the shaken
/// walk, and pretending otherwise would emit a graph whose scan and lowering
/// disagreed.
///
/// # Errors
/// The resolution errors of [`bundle_npm_dependency`], plus a loud failure when
/// a demanded name is exported by nothing in the graph, when a package imports a
/// refused external, or when it imports a name the host external does not have.
pub fn bundle_npm_dependency_for_demand(
    search_dir: &Path,
    specifier: &str,
    demand: &BTreeSet<String>,
    options: &ShakeOptions,
) -> Result<ShakenNpmBundle, NpmBundleError> {
    let (entry_package, entry_path) = resolve_bare_entry(search_dir, specifier)?;

    // A star demand cannot be narrowed, and an unshakeable package must not be.
    let shakeable = package_declares_no_side_effects(&entry_package)
        && !demand.iter().any(|name| name == STAR_DEMAND);
    if !shakeable {
        let whole = bundle_npm_dependency(search_dir, specifier)?;
        let bindings = demand
            .iter()
            .map(|name| {
                (
                    name.clone(),
                    ResolvedExport {
                        record_key: whole.entry_key.clone(),
                        export_name: name.clone(),
                    },
                )
            })
            .collect();
        return Ok(ShakenNpmBundle {
            specifier: whole.specifier,
            package_name: whole.package_name,
            package_version: whole.package_version,
            bindings,
            artifacts: whole.artifacts,
            taken_whole: true,
        });
    }

    let mut walk = DemandWalk::new(specifier, options);
    let mut bindings = BTreeMap::new();
    for name in demand {
        let resolved = walk.resolve(&entry_package, &entry_path, name, 0)?;
        bindings.insert(name.clone(), resolved);
    }
    // Every file reached by resolution is live; a live file needs its own
    // imports whole, because without intra-file DCE we cannot tell which import
    // the used export depends on.
    walk.close_over_imports()?;

    Ok(ShakenNpmBundle {
        specifier: specifier.to_string(),
        package_name: entry_package.name.clone(),
        package_version: entry_package.version.clone(),
        bindings,
        artifacts: walk.into_artifacts()?,
        taken_whole: false,
    })
}

/// `"sideEffects": false` on the package's own manifest.
///
/// The array form (`"sideEffects": ["*.css"]`) is deliberately treated as *not*
/// declared: it means "these files do have effects", and honouring it properly
/// requires glob matching per file. Declining to shake is the safe reading of a
/// declaration we do not fully implement.
fn package_declares_no_side_effects(package: &ResolvedPackage) -> bool {
    let manifest_path = package.dir.join("package.json");
    let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    value.get("sideEffects") == Some(&serde_json::Value::Bool(false))
}

/// Where one import edge lands.
enum Edge {
    /// A host-provided module. Nothing is read, nothing is emitted.
    Host(String),
    /// A real file inside some package.
    File(ResolvedPackage, PathBuf),
}

/// One file, read, classified and define-folded exactly once.
///
/// 🔑 **The folded source is the only source.** The specifier walk and the
/// lowering both read this string, so they cannot disagree about which arm of a
/// `NODE_ENV` fork exists — the failure mode being a `require` that resolves
/// against a record nothing registered.
#[derive(Clone)]
struct PreparedFile {
    format: NpmModuleFormat,
    source: std::sync::Arc<str>,
    imports: std::sync::Arc<FileImports>,
}

/// The demand-driven walk: resolution state plus the live file set.
struct DemandWalk<'a> {
    specifier: String,
    options: &'a ShakeOptions,
    /// Files that must be emitted, in insertion order.
    live: Vec<(ResolvedPackage, PathBuf)>,
    live_keys: BTreeSet<PathBuf>,
    /// `(file, name)` pairs already resolved, so a re-export cycle terminates.
    resolving: BTreeSet<(PathBuf, String)>,
    /// Read-once cache. A file is touched up to three times (name resolution,
    /// import closure, lowering) and a barrel once *per demanded name* — twenty
    /// icons re-parsed lucide's 854-line index twenty times before this existed.
    prepared: HashMap<PathBuf, PreparedFile>,
    /// Export surfaces, cached for the same reason and on the same key.
    export_maps: HashMap<PathBuf, std::sync::Arc<ExportMap>>,
}

impl<'a> DemandWalk<'a> {
    fn new(specifier: &str, options: &'a ShakeOptions) -> Self {
        Self {
            specifier: specifier.to_string(),
            options,
            live: Vec::new(),
            live_keys: BTreeSet::new(),
            resolving: BTreeSet::new(),
            prepared: HashMap::new(),
            export_maps: HashMap::new(),
        }
    }

    fn mark_live(&mut self, package: &ResolvedPackage, path: &Path) {
        if self.live_keys.insert(path.to_path_buf()) {
            self.live.push((package.clone(), path.to_path_buf()));
        }
    }

    /// Read, classify and define-fold one file, memoised on its path.
    fn prepare(&mut self, path: &Path) -> Result<PreparedFile, NpmBundleError> {
        if let Some(hit) = self.prepared.get(path) {
            return Ok(hit.clone());
        }
        let raw = std::fs::read_to_string(path).map_err(|err| NpmBundleError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        // Classification reads the *original* text: define folding preserves
        // every `import`/`export` declaration, so it cannot change the answer,
        // and asking first means the fold knows which grammar to parse with.
        let format = classify_format(path, &raw);
        let source = fold_defines(
            &path.display().to_string(),
            &raw,
            format == NpmModuleFormat::Esm,
            &self.options.defines,
        )
        .unwrap_or(raw);
        let imports = collect_imports(path, &source, format)?;
        let prepared = PreparedFile {
            format,
            source: std::sync::Arc::from(source.as_str()),
            imports: std::sync::Arc::new(imports),
        };
        self.prepared.insert(path.to_path_buf(), prepared.clone());
        Ok(prepared)
    }

    /// The export surface of an ESM file, memoised.
    fn export_map_of(
        &mut self,
        package: &ResolvedPackage,
        path: &Path,
        source: &str,
    ) -> Result<std::sync::Arc<ExportMap>, NpmBundleError> {
        if let Some(hit) = self.export_maps.get(path) {
            return Ok(hit.clone());
        }
        let module = parse_npm_module(path, source).map_err(|message| NpmBundleError::Compile {
            key: record_key(package, path),
            path: path.to_path_buf(),
            message,
        })?;
        let map = std::sync::Arc::new(export_map(&module));
        self.export_maps.insert(path.to_path_buf(), map.clone());
        Ok(map)
    }

    /// Resolve one raw specifier written inside a package file, consulting the
    /// externals table first.
    fn edge(
        &self,
        package: &ResolvedPackage,
        path: &Path,
        raw: &str,
    ) -> Result<Edge, NpmBundleError> {
        match self.options.externals.get(raw) {
            Some(ExternalTarget::Host { record_key, .. }) => Ok(Edge::Host(record_key.clone())),
            Some(ExternalTarget::Refused { reason }) => Err(NpmBundleError::ExternalRefused {
                specifier: raw.to_string(),
                importer: path.to_path_buf(),
                reason: reason.clone(),
            }),
            None => {
                let (dep_package, dep_path) = resolve_from_file(package, path, raw)?;
                Ok(Edge::File(dep_package, dep_path))
            }
        }
    }

    /// Resolve one exported name to the record that defines it.
    fn resolve(
        &mut self,
        package: &ResolvedPackage,
        path: &Path,
        name: &str,
        depth: u32,
    ) -> Result<ResolvedExport, NpmBundleError> {
        // A barrel chain is short in practice (entry → index → leaf); the cap is
        // a backstop against a pathological or cyclic graph, not a real limit.
        const MAX_REEXPORT_DEPTH: u32 = 32;
        if depth > MAX_REEXPORT_DEPTH {
            return Err(NpmBundleError::Compile {
                key: self.specifier.clone(),
                path: path.to_path_buf(),
                message: format!("re-export chain for '{name}' exceeded {MAX_REEXPORT_DEPTH} hops"),
            });
        }

        let prepared = self.prepare(path)?;

        // Only ESM has a static export graph. A CJS or JSON file is opaque to
        // this analysis, so it is taken whole and the name is read off its
        // record at runtime — correct, just unshaken.
        if prepared.format != NpmModuleFormat::Esm {
            self.mark_live(package, path);
            return Ok(ResolvedExport {
                record_key: record_key(package, path),
                export_name: name.to_string(),
            });
        }

        if !self.resolving.insert((path.to_path_buf(), name.to_string())) {
            // Already being resolved higher in this chain — a cycle. Bind to
            // this file; the runtime linker's memoisation handles the rest.
            self.mark_live(package, path);
            return Ok(ResolvedExport {
                record_key: record_key(package, path),
                export_name: name.to_string(),
            });
        }

        let map = self.export_map_of(package, path, &prepared.source)?;

        if let Some(origin) = map.named.get(name) {
            return match origin.clone() {
                ExportOrigin::Local => {
                    self.mark_live(package, path);
                    Ok(ResolvedExport {
                        record_key: record_key(package, path),
                        export_name: name.to_string(),
                    })
                }
                ExportOrigin::ReExport {
                    source: from,
                    imported,
                } => match self.edge(package, path, &from)? {
                    // A re-export that lands on the host binds straight to the
                    // host's record — `export { forwardRef } from 'react'`.
                    Edge::Host(host_key) => Ok(ResolvedExport {
                        record_key: host_key,
                        export_name: imported,
                    }),
                    Edge::File(next_package, next_path) => {
                        if imported == STAR_DEMAND {
                            // A namespace re-export needs the target whole.
                            self.mark_live(&next_package, &next_path);
                            return Ok(ResolvedExport {
                                record_key: record_key(&next_package, &next_path),
                                export_name: STAR_DEMAND.to_string(),
                            });
                        }
                        self.resolve(&next_package, &next_path, &imported, depth + 1)
                    }
                },
            };
        }

        // Not named here — try each `export *` source in order, which is what
        // makes `export * from './icons'` resolve without reading all 854.
        for star_source in &map.star.clone() {
            // A host module has no readable export graph, so a name cannot be
            // *found* through it; skip rather than guess.
            let Ok(Edge::File(next_package, next_path)) = self.edge(package, path, star_source)
            else {
                continue;
            };
            if let Ok(resolved) = self.resolve(&next_package, &next_path, name, depth + 1) {
                return Ok(resolved);
            }
        }

        Err(NpmBundleError::Compile {
            key: record_key(package, path),
            path: path.to_path_buf(),
            message: format!(
                "'{}' does not export '{name}' — nothing in its module graph provides that binding",
                self.specifier
            ),
        })
    }

    /// A live file needs its own imports, and theirs, whole.
    fn close_over_imports(&mut self) -> Result<(), NpmBundleError> {
        let mut index = 0usize;
        while index < self.live.len() {
            let (package, path) = self.live[index].clone();
            index += 1;

            if self.live.len() > MAX_GRAPH_FILES {
                return Err(NpmBundleError::GraphTooLarge {
                    specifier: self.specifier.clone(),
                });
            }

            let prepared = self.prepare(&path)?;
            for entry in &prepared.imports.entries {
                match self.edge(&package, &path, &entry.specifier)? {
                    Edge::Host(_) => self.check_host_import(&path, entry)?,
                    Edge::File(dep_package, dep_path) => self.mark_live(&dep_package, &dep_path),
                }
            }
        }
        Ok(())
    }

    /// Every name a live file imports from a host module must be a name that
    /// host actually provides.
    ///
    /// 🔑 **Checked here, at build, rather than left to produce `undefined` at
    /// first render.** This is the same rule Phase 1 set for a demanded name
    /// nothing exports, applied to the other end of the edge.
    ///
    /// ⚠️ **Boundary: a namespace import is unverifiable.** `import * as React
    /// from 'react'` followed by `React.useSyncExternalStore(…)` is a member
    /// access on an object, and answering it would need whole-file member
    /// analysis rather than an import list. The same is true of CJS
    /// `require('react').x`. Both pass, and a missing member surfaces as an
    /// ordinary `undefined is not a function` at run time.
    fn check_host_import(
        &self,
        path: &Path,
        entry: &FileImport,
    ) -> Result<(), NpmBundleError> {
        let Some(ExternalTarget::Host { provides, .. }) =
            self.options.externals.get(&entry.specifier)
        else {
            return Ok(());
        };
        for name in &entry.names {
            if name == STAR_DEMAND || provides.contains(name) {
                continue;
            }
            return Err(NpmBundleError::ExternalExportMissing {
                specifier: entry.specifier.clone(),
                name: name.clone(),
                importer: path.to_path_buf(),
                provides: provides.iter().cloned().collect::<Vec<_>>().join(", "),
            });
        }
        Ok(())
    }

    /// Lower every live file to its registration artifact.
    fn into_artifacts(mut self) -> Result<Vec<NpmArtifact>, NpmBundleError> {
        let live = std::mem::take(&mut self.live);
        let mut artifacts = Vec::with_capacity(live.len());
        for (package, path) in &live {
            let key = record_key(package, path);
            let prepared = self.prepare(path)?;

            let mut resolve_map: BTreeMap<String, String> = BTreeMap::new();
            for entry in &prepared.imports.entries {
                if resolve_map.contains_key(&entry.specifier) {
                    continue;
                }
                let resolved = match self.edge(package, path, &entry.specifier)? {
                    Edge::Host(host_key) => host_key,
                    Edge::File(dep_package, dep_path) => record_key(&dep_package, &dep_path),
                };
                resolve_map.insert(entry.specifier.clone(), resolved);
            }

            // The lowering takes a `HashMap`; the walk keeps a `BTreeMap` so the
            // emitted resolve table is byte-stable across runs, which is what
            // lets the client chunk be content-hashed and cached.
            let resolve_map: HashMap<String, String> = resolve_map.into_iter().collect();
            let script =
                compile_npm_module_script(&key, &prepared.source, prepared.format, &resolve_map)
                    .map_err(|err| NpmBundleError::Compile {
                        key: key.clone(),
                        path: path.clone(),
                        message: err.to_string(),
                    })?;
            let source_hash = stable_source_hash(&prepared.source);
            artifacts.push(NpmArtifact {
                key,
                script,
                source_hash,
            });
        }
        Ok(artifacts)
    }
}

/// Collects string-literal `require("…")` call arguments anywhere in a CJS file.
#[derive(Default)]
struct RequireCollector {
    specifiers: Vec<String>,
}

impl Visit for RequireCollector {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Callee::Expr(callee) = &call.callee {
            if let Expr::Ident(ident) = callee.as_ref() {
                if ident.sym.as_ref() == "require" && call.args.len() == 1 {
                    if let Some(Expr::Lit(Lit::Str(specifier))) =
                        call.args.first().map(|arg| arg.expr.as_ref())
                    {
                        self.specifiers.push(specifier.value.to_string());
                    }
                }
            }
        }
        call.visit_children_with(self);
    }
}

/// Scan a project TSX/TS/JSX source for the **bare npm specifiers** it
/// imports (or re-exports from). Used at `CompiledProject::wrap` time to
/// discover which packages need bundling. Parse failures return an empty
/// list — discovery must never fail a build the component parser accepted.
#[must_use]
pub fn scan_bare_imports(source: &str) -> Vec<String> {
    let parse = |syntax: Syntax| -> Option<Module> {
        let source_map: Lrc<SourceMap> = Lrc::default();
        let file = source_map.new_source_file(
            FileName::Custom("scan".to_string()).into(),
            source.to_string(),
        );
        Parser::new(syntax, StringInput::from(&*file), None)
            .parse_module()
            .ok()
    };

    let module = parse(Syntax::Typescript(TsSyntax {
        tsx: true,
        decorators: true,
        ..Default::default()
    }))
    .or_else(|| {
        parse(Syntax::Es(EsSyntax {
            jsx: true,
            decorators: true,
            ..Default::default()
        }))
    });

    let Some(module) = module else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    esm_specifiers(&module)
        .into_iter()
        .filter(|specifier| is_bare_npm_specifier(specifier))
        .filter(|specifier| seen.insert(specifier.clone()))
        .collect()
}

/// Bundle one bare specifier: resolve its entry, walk the reachable graph,
/// and lower every file to a lazy-factory artifact plus a final alias
/// artifact for the bare specifier itself.
///
/// `search_dir` is where the upward `node_modules` walk starts — pass the
/// project root (or any directory inside it).
pub fn bundle_npm_dependency(
    search_dir: &Path,
    specifier: &str,
) -> Result<NpmDependencyBundle, NpmBundleError> {
    let (entry_package, entry_path) = resolve_bare_entry(search_dir, specifier)?;
    let entry_key = record_key(&entry_package, &entry_path);

    let mut artifacts = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut queue: Vec<(ResolvedPackage, PathBuf)> = vec![(entry_package.clone(), entry_path)];

    while let Some((package, path)) = queue.pop() {
        if !visited.insert(path.clone()) {
            continue;
        }
        if visited.len() > MAX_GRAPH_FILES {
            return Err(NpmBundleError::GraphTooLarge {
                specifier: specifier.to_string(),
            });
        }

        let key = record_key(&package, &path);
        let source = std::fs::read_to_string(&path).map_err(|err| NpmBundleError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?;
        let format = classify_format(&path, &source);

        // Resolve every raw specifier this file references to a record key,
        // queueing newly-discovered files.
        let mut resolve_map: BTreeMap<String, String> = BTreeMap::new();
        for raw in collect_specifiers(&path, &source, format)? {
            if resolve_map.contains_key(&raw) {
                continue;
            }
            let (dep_package, dep_path) = resolve_from_file(&package, &path, &raw)?;
            resolve_map.insert(raw.clone(), record_key(&dep_package, &dep_path));
            queue.push((dep_package, dep_path));
        }

        let resolve_map: HashMap<String, String> = resolve_map.into_iter().collect();
        let script =
            compile_npm_module_script(&key, &source, format, &resolve_map).map_err(|err| {
                NpmBundleError::Compile {
                    key: key.clone(),
                    path: path.clone(),
                    message: err.to_string(),
                }
            })?;
        let source_hash = stable_source_hash(&source);
        artifacts.push(NpmArtifact {
            key,
            script,
            source_hash,
        });
    }

    // Deterministic load order (the lazy factories don't need one, but stable
    // artifacts make hashing/caching and test assertions sane).
    artifacts.sort_by(|a, b| a.key.cmp(&b.key));

    // Alias: the bare specifier resolves to the entry record.
    let alias_script = format!(
        "globalThis.__ALBEDO_NPM_ALIASES[{}] = {};",
        serde_json::to_string(specifier).expect("specifier serializes"),
        serde_json::to_string(&entry_key).expect("key serializes"),
    );
    let alias_hash = stable_source_hash(&alias_script);
    artifacts.push(NpmArtifact {
        key: specifier.to_string(),
        script: alias_script,
        source_hash: alias_hash,
    });

    Ok(NpmDependencyBundle {
        specifier: specifier.to_string(),
        package_name: entry_package.name,
        package_version: entry_package.version,
        entry_key,
        artifacts,
    })
}

/// Bundle a set of bare specifiers. Artifacts for files shared between
/// specifiers (e.g. two `date-fns/…` subpaths) collapse at load time via
/// their identical keys and source hashes.
pub fn bundle_npm_dependencies(
    search_dir: &Path,
    specifiers: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<Vec<NpmDependencyBundle>, NpmBundleError> {
    let mut bundles = Vec::new();
    for specifier in specifiers {
        bundles.push(bundle_npm_dependency(search_dir, specifier.as_ref())?);
    }
    Ok(bundles)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;

    // ── format classification ──────────────────────────────────────────
    //
    // The bug these pin: entry resolution prefers the `"module"` field, which
    // is ESM by definition, while classification followed Node — extension,
    // then `"type"`, default CJS. Node never reads `"module"`, so Node never
    // has to reconcile them; this bundler does. An ESM file labelled CJS gets
    // wrapped verbatim in a `function(module, exports, …)` IIFE, putting
    // `export` inside a function body, and QuickJS answers *"unsupported
    // keyword: export"*. It was ~46 of 79 evaluation failures across the
    // measured corpus, and one such package in a project broke **every action
    // in that project**, because `preload_npm_bundles` loads them all.

    /// `clsx@1.2.1/dist/clsx.m.js`, byte for byte — the minimal real case, and
    /// the one that found this. Note the extension is `.js`: the `.m` belongs
    /// to the *basename*, so nothing about the path says ESM.
    const CLSX_MINIFIED_ESM: &str = r#"function r(e){var t,f,n="";if("string"==typeof e)n+=e;return n}export function clsx(){return ""}export default clsx;"#;

    #[test]
    fn a_minified_esm_file_named_dot_js_is_detected_as_esm() {
        assert!(
            source_is_esm(CLSX_MINIFIED_ESM),
            "`export` after a `}}` with no whitespace is still an export"
        );
    }

    #[test]
    fn a_cjs_file_using_exports_is_not_promoted() {
        // 🪤 The reason the keyword scan is word-boundary and not a substring
        // test: `exports` *contains* `export`, and it is the single most common
        // token in CommonJS. A substring test would parse every CJS file and
        // promote the ones whose parse happened to succeed.
        assert!(!source_is_esm(
            "exports.foo = 1; module.exports = { bar: 2 }; const imported = require('x');"
        ));
        assert!(!has_module_keyword("exports.a = 1; var imported = 2;"));
    }

    #[test]
    fn a_dynamic_import_alone_does_not_make_a_file_a_module() {
        // `import(...)` is legal in CommonJS. Only a top-level declaration (or
        // `import.meta`) is proof, which is exactly what the parse — and not
        // the text scan — can tell.
        assert!(!source_is_esm(
            "module.exports = async () => { const m = await import('node:fs'); return m; };"
        ));
    }

    #[test]
    fn import_meta_promotes_a_file_with_no_module_declarations() {
        // An expression, not a `ModuleDecl`, so the declaration check misses it
        // — this is the `import.meta only valid in module code` failure class.
        assert!(source_is_esm("const url = import.meta.url; module.exports = url;"));
    }

    #[test]
    fn every_export_form_the_corpus_produced_is_detected() {
        for source in [
            "export * from './a';",              // Unexpected token '*'
            "export { a as b } from './a';",     // Unexpected token '{'
            "export default function () {}",
            "const e = 1; export { e };",
            "import a from './a'; a();",
        ] {
            assert!(source_is_esm(source), "not detected as ESM: {source}");
        }
    }

    #[test]
    fn a_file_with_no_module_syntax_stays_cjs() {
        assert!(!source_is_esm("function a() { return 1; } a();"));
        // Unparseable source is left CJS: if it will not parse as a module it
        // cannot be one, and the CJS path passes source through verbatim.
        assert!(!source_is_esm("export export export"));
    }

    #[test]
    fn bare_specifier_detection() {
        assert!(is_bare_npm_specifier("zod"));
        assert!(is_bare_npm_specifier("date-fns/addDays"));
        assert!(is_bare_npm_specifier("@scope/pkg/sub"));
        assert!(!is_bare_npm_specifier("./local"));
        assert!(!is_bare_npm_specifier("../up"));
        assert!(!is_bare_npm_specifier("/abs"));
        assert!(!is_bare_npm_specifier("C:/abs/path"));
        assert!(!is_bare_npm_specifier("react"));
        assert!(!is_bare_npm_specifier("react-dom"));
        assert!(!is_bare_npm_specifier("albedo"));
        // The generated binding modules: types only, folded away by the
        // transpile, and no package by either name exists to resolve.
        assert!(!is_bare_npm_specifier("albedo/forge"));
        assert!(!is_bare_npm_specifier("albedo/sources"));
        assert!(!is_bare_npm_specifier("styles.css"));
        assert!(!is_bare_npm_specifier("https://cdn.example/x.js"));
    }

    #[test]
    fn split_handles_plain_and_scoped() {
        assert_eq!(
            split_bare_specifier("zod"),
            ("zod".to_string(), ".".to_string())
        );
        assert_eq!(
            split_bare_specifier("date-fns/addDays"),
            ("date-fns".to_string(), "./addDays".to_string())
        );
        assert_eq!(
            split_bare_specifier("@scope/pkg"),
            ("@scope/pkg".to_string(), ".".to_string())
        );
        assert_eq!(
            split_bare_specifier("@scope/pkg/deep/file"),
            ("@scope/pkg".to_string(), "./deep/file".to_string())
        );
    }

    #[test]
    fn exports_conditions_resolve_in_priority_order() {
        let exports = serde_json::json!({
            ".": { "types": "./index.d.ts", "import": "./index.mjs", "require": "./index.cjs" },
            "./sub": { "default": "./sub.js" }
        });
        assert_eq!(
            resolve_exports_subpath(&exports, ".").as_deref(),
            Some("./index.mjs")
        );
        assert_eq!(
            resolve_exports_subpath(&exports, "./sub").as_deref(),
            Some("./sub.js")
        );
        assert_eq!(resolve_exports_subpath(&exports, "./missing"), None);
    }

    #[test]
    fn exports_nested_conditions_and_string_form() {
        // date-fns shape: condition -> { types, default }.
        let nested = serde_json::json!({
            ".": { "require": { "default": "./index.cjs" }, "import": { "types": "./index.d.ts", "default": "./index.js" } }
        });
        assert_eq!(
            resolve_exports_subpath(&nested, ".").as_deref(),
            Some("./index.js")
        );

        let string_form = serde_json::json!("./only.js");
        assert_eq!(
            resolve_exports_subpath(&string_form, ".").as_deref(),
            Some("./only.js")
        );
        assert_eq!(resolve_exports_subpath(&string_form, "./sub"), None);
    }

    #[test]
    fn exports_wildcard_patterns_capture() {
        let exports = serde_json::json!({
            "./feature/*": "./lib/feature/*.js",
            "./*": "./lib/*.js"
        });
        assert_eq!(
            resolve_exports_subpath(&exports, "./feature/x").as_deref(),
            Some("./lib/feature/x.js"),
            "longest static prefix wins"
        );
        assert_eq!(
            resolve_exports_subpath(&exports, "./other").as_deref(),
            Some("./lib/other.js")
        );
    }

    #[test]
    fn collects_esm_and_cjs_specifiers() {
        let esm = r#"
            import a from "./a.js";
            import "./side.js";
            export { b } from "./b.js";
            export * from "./c.js";
            const x = 1;
        "#;
        let module = parse_npm_module(Path::new("x.js"), esm).unwrap();
        assert_eq!(
            esm_specifiers(&module),
            vec!["./a.js", "./side.js", "./b.js", "./c.js"]
        );

        let cjs = r#"
            'use strict';
            const a = require("./a.js");
            if (process.env.NODE_ENV !== 'production') { require("./dev.js"); }
            const dynamic = require(someVariable);
        "#;
        let program = parse_npm_program(Path::new("x.cjs"), cjs).unwrap();
        let mut collector = RequireCollector::default();
        program.visit_with(&mut collector);
        assert_eq!(collector.specifiers, vec!["./a.js", "./dev.js"]);
    }

    #[test]
    fn synthetic_package_bundles_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("tinylib");
        std::fs::create_dir_all(pkg.join("lib")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "tinylib", "version": "1.2.3", "type": "module",
                 "exports": { ".": { "import": "./lib/index.js" } } }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("lib").join("index.js"),
            r#"export { double } from "./math.js"; export const NAME = "tinylib";"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("lib").join("math.js"),
            "export function double(n) { return n * 2; }",
        )
        .unwrap();

        let bundle = bundle_npm_dependency(dir.path(), "tinylib").unwrap();
        assert_eq!(bundle.package_name, "tinylib");
        assert_eq!(bundle.package_version, "1.2.3");
        assert_eq!(bundle.entry_key, "npm:tinylib@1.2.3/lib/index.js");
        // Two file factories + one alias.
        assert_eq!(bundle.artifacts.len(), 3);
        assert!(bundle
            .artifacts
            .iter()
            .any(|a| a.key == "npm:tinylib@1.2.3/lib/math.js"));
        let alias = bundle.artifacts.last().unwrap();
        assert_eq!(alias.key, "tinylib");
        assert!(alias.script.contains("__ALBEDO_NPM_ALIASES"));
    }

    /// `react-remove-scroll-bar/constants`, reproduced exactly: a subpath that
    /// is a **directory containing nothing but a redirecting `package.json`**.
    ///
    /// The pre-`exports` way to publish a secondary entry point, and every Radix
    /// primitive depends on this one package — so while `probe_file` looked only
    /// for `constants.js` and `constants/index.js`, the entire shadcn layer was
    /// unresolvable.
    #[test]
    fn a_directory_subpath_resolves_through_its_own_package_json() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("scroll-bar");
        std::fs::create_dir_all(pkg.join("dist").join("es2015")).unwrap();
        std::fs::create_dir_all(pkg.join("constants")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "scroll-bar", "version": "2.3.8", "module": "dist/es2015/index.js" }"#,
        )
        .unwrap();
        // Nothing but the redirect — no `constants.js`, no `constants/index.js`.
        // The `../` in the target is the shape that makes this a real path
        // resolution and not a name lookup.
        std::fs::write(
            pkg.join("constants").join("package.json"),
            r#"{ "private": true, "main": "../dist/es5/constants.js",
                 "module": "../dist/es2015/constants.js" }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("dist").join("es2015").join("constants.js"),
            "export const zeroRightClassName = 'right-0';",
        )
        .unwrap();
        std::fs::write(
            pkg.join("dist").join("es2015").join("index.js"),
            "export const noop = 1;",
        )
        .unwrap();

        let bundle = bundle_npm_dependency(dir.path(), "scroll-bar/constants").unwrap();
        assert_eq!(
            bundle.entry_key, "npm:scroll-bar@2.3.8/dist/es2015/constants.js",
            "the directory's own manifest must redirect the probe, and `module` \
             must win over `main` the way `resolve_bare_entry` picks an entry"
        );
    }

    /// The redirect is tried *before* the index probe, because a directory may
    /// carry both and Node's LOAD_AS_DIRECTORY reads the manifest first.
    #[test]
    fn a_directory_manifest_wins_over_an_index_file_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("both");
        std::fs::create_dir_all(pkg.join("sub")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "both", "version": "1.0.0", "main": "sub/index.js" }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("sub").join("package.json"),
            r#"{ "main": "./real.js" }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("sub").join("real.js"), "module.exports = 'real';").unwrap();
        std::fs::write(pkg.join("sub").join("index.js"), "module.exports = 'index';").unwrap();

        let bundle = bundle_npm_dependency(dir.path(), "both/sub").unwrap();
        assert_eq!(bundle.entry_key, "npm:both@1.0.0/sub/real.js");
    }

    /// A manifest pointing at nothing must not veto the ordinary path — the
    /// folder can still be resolvable the normal way.
    #[test]
    fn a_broken_directory_manifest_falls_through_to_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("broken");
        std::fs::create_dir_all(pkg.join("sub")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "broken", "version": "1.0.0", "main": "sub/index.js" }"#,
        )
        .unwrap();
        std::fs::write(
            pkg.join("sub").join("package.json"),
            r#"{ "main": "./does-not-exist.js" }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("sub").join("index.js"), "module.exports = 'index';").unwrap();

        let bundle = bundle_npm_dependency(dir.path(), "broken/sub").unwrap();
        assert_eq!(bundle.entry_key, "npm:broken@1.0.0/sub/index.js");
    }

    /// A manifest that redirects to its own directory would recurse forever.
    /// The bound turns a malformed package into a refusal instead of a hang.
    #[test]
    fn a_self_referential_directory_manifest_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("loop");
        std::fs::create_dir_all(pkg.join("sub")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "loop", "version": "1.0.0", "main": "sub" }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("sub").join("package.json"), r#"{ "main": "." }"#).unwrap();

        let err = bundle_npm_dependency(dir.path(), "loop").unwrap_err();
        assert!(matches!(err, NpmBundleError::FileNotFound { .. }));
    }

    // ── tree shaking ───────────────────────────────────────────────────
    //
    // The corpus tests in `tests/npm_tree_shaking.rs` measure the real
    // packages; these pin the algorithm against fixtures so the rules hold
    // without a `node_modules` on disk.

    /// A package shaped like `lucide-react`: entry → star barrel → named
    /// barrel → leaf, with `sideEffects: false`.
    fn barrel_package(dir: &Path) -> PathBuf {
        let pkg = dir.join("node_modules").join("barrel");
        std::fs::create_dir_all(pkg.join("esm").join("icons")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "barrel", "version": "1.0.0", "module": "esm/index.js",
                 "sideEffects": false }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("esm").join("index.js"), "export * from './icons';").unwrap();
        std::fs::write(
            pkg.join("esm").join("icons").join("index.js"),
            "export { default as Alpha } from './alpha';\n\
             export { default as Beta } from './beta';\n\
             export { default as Gamma } from './gamma';",
        )
        .unwrap();
        for icon in ["alpha", "beta", "gamma"] {
            std::fs::write(
                pkg.join("esm").join("icons").join(format!("{icon}.js")),
                format!(
                    "import shared from '../shared';\nconst {icon} = shared('{icon}');\nexport default {icon};"
                ),
            )
            .unwrap();
        }
        std::fs::write(
            pkg.join("esm").join("shared.js"),
            "export default function shared(n) { return n; }",
        )
        .unwrap();
        pkg
    }

    fn demand(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    /// The headline rule: a demanded name is resolved *through* both barrels to
    /// the file that defines it, and the barrels are never emitted — which is
    /// also why their unused re-exports need no pruning, since nothing points
    /// at them.
    #[test]
    fn a_demanded_name_resolves_past_every_barrel() {
        let dir = tempfile::tempdir().unwrap();
        barrel_package(dir.path());

        let shaken =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Beta"]), &ShakeOptions::default()).unwrap();

        assert!(!shaken.taken_whole);
        let beta = shaken.bindings.get("Beta").expect("Beta resolved");
        assert!(beta.record_key.ends_with("esm/icons/beta.js"), "{beta:?}");
        assert_eq!(beta.export_name, "default");

        let keys: Vec<&str> = shaken.artifacts.iter().map(|a| a.key.as_str()).collect();
        assert!(
            !keys.iter().any(|k| k.ends_with("index.js")),
            "no barrel should be emitted: {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k.ends_with("alpha.js") || k.ends_with("gamma.js")),
            "unreached siblings must not ship: {keys:?}"
        );
        // The leaf and the helper its import demands, and nothing else.
        assert_eq!(shaken.artifacts.len(), 2, "{keys:?}");
    }

    /// A live file's own imports come with it, because without intra-file DCE we
    /// cannot know which import the used export depends on.
    #[test]
    fn a_live_file_pulls_its_own_imports() {
        let dir = tempfile::tempdir().unwrap();
        barrel_package(dir.path());

        let shaken =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Alpha"]), &ShakeOptions::default()).unwrap();
        assert!(shaken
            .artifacts
            .iter()
            .any(|a| a.key.ends_with("esm/shared.js")));
    }

    /// Two names share one graph — the helper is emitted once, not per name.
    #[test]
    fn two_demanded_names_share_their_common_dependency() {
        let dir = tempfile::tempdir().unwrap();
        barrel_package(dir.path());

        let one =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Alpha"]), &ShakeOptions::default()).unwrap();
        let two =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Alpha", "Beta"]), &ShakeOptions::default())
                .unwrap();

        assert_eq!(one.artifacts.len(), 2);
        assert_eq!(two.artifacts.len(), 3, "one extra leaf, one shared helper");
    }

    /// 🔒 Without `"sideEffects": false` the package is taken whole and says so.
    /// A file can matter for reasons no export graph can see — a polyfill
    /// installing itself, a registry populating on import — and dropping one
    /// breaks the package at runtime, silently, which is the worst outcome
    /// available.
    #[test]
    fn a_package_without_the_declaration_is_never_shaken() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = barrel_package(dir.path());
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "barrel", "version": "1.0.0", "module": "esm/index.js" }"#,
        )
        .unwrap();

        let shaken =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Beta"]), &ShakeOptions::default()).unwrap();
        assert!(shaken.taken_whole);
        assert!(shaken.artifacts.len() > 3, "the whole package ships");
        // The binding still resolves — through the entry record, as before.
        assert!(shaken.bindings.contains_key("Beta"));
    }

    /// The array form of `sideEffects` names files that *do* have effects.
    /// Honouring it needs glob matching per file; declining to shake is the
    /// safe reading of a declaration we do not fully implement.
    #[test]
    fn the_array_form_of_side_effects_declines_shaking() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = barrel_package(dir.path());
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "barrel", "version": "1.0.0", "module": "esm/index.js",
                 "sideEffects": ["*.css"] }"#,
        )
        .unwrap();

        let shaken =
            bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Beta"]), &ShakeOptions::default()).unwrap();
        assert!(shaken.taken_whole);
    }

    /// A name nothing exports is a build error, not a chunk whose binding is
    /// `undefined` when the island first renders.
    #[test]
    fn an_unexported_name_fails_at_build() {
        let dir = tempfile::tempdir().unwrap();
        barrel_package(dir.path());

        let err = bundle_npm_dependency_for_demand(dir.path(), "barrel", &demand(&["Delta"]), &ShakeOptions::default())
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Delta"), "must name the binding: {message}");
    }

    /// A re-export cycle terminates instead of recursing until the stack dies.
    #[test]
    fn a_re_export_cycle_terminates() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("loopy");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "loopy", "version": "1.0.0", "module": "a.js", "sideEffects": false }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("a.js"), "export * from './b';").unwrap();
        std::fs::write(pkg.join("b.js"), "export * from './a';").unwrap();

        // Either a clean "not exported" error or a bound record — never a hang
        // or a stack overflow.
        let _ = bundle_npm_dependency_for_demand(dir.path(), "loopy", &demand(&["Whatever"]), &ShakeOptions::default());
    }

    /// CommonJS has no static export graph, so a CJS entry is opaque to this
    /// analysis: it is taken whole and the name read off its record at runtime.
    #[test]
    fn a_commonjs_entry_is_taken_whole_but_still_binds() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("cjspkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "cjspkg", "version": "1.0.0", "main": "index.js", "sideEffects": false }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "module.exports = { hello: 1 };").unwrap();

        let shaken =
            bundle_npm_dependency_for_demand(dir.path(), "cjspkg", &demand(&["hello"]), &ShakeOptions::default()).unwrap();
        let hello = shaken.bindings.get("hello").expect("bound");
        assert!(hello.record_key.ends_with("index.js"));
        assert_eq!(hello.export_name, "hello");
    }

    #[test]
    fn missing_package_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let err = bundle_npm_dependency(dir.path(), "ghost-package").unwrap_err();
        assert!(matches!(err, NpmBundleError::PackageNotFound { .. }));
        assert!(err.to_string().contains("ghost-package"));
    }

    #[test]
    fn unexported_subpath_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("node_modules").join("sealed");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{ "name": "sealed", "version": "1.0.0", "type": "module",
                 "exports": { ".": "./index.js" } }"#,
        )
        .unwrap();
        std::fs::write(pkg.join("index.js"), "export const x = 1;").unwrap();

        let err = bundle_npm_dependency(dir.path(), "sealed/secret").unwrap_err();
        assert!(matches!(err, NpmBundleError::SubpathNotExported { .. }));
    }
}
