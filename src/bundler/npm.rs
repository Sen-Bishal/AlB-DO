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

use crate::runtime::engine::stable_source_hash;
use crate::runtime::quickjs_engine::{compile_npm_module_script, NpmModuleFormat};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use swc_common::{sync::Lrc, FileName, SourceMap};
use swc_ecma_ast::{CallExpr, Callee, Expr, Lit, Module, ModuleDecl, ModuleItem, Program};
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
    if matches!(s, "react" | "react-dom" | "albedo") || s.starts_with("react/") {
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

/// Probe `base` the way Node resolves a path-ish specifier: exact file, then
/// appended extensions, then directory index files.
fn probe_file(base: &Path) -> Option<PathBuf> {
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
        for index in ["index.js", "index.mjs", "index.cjs", "index.json"] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// How a resolved file should be lowered.
fn classify_format(path: &Path) -> NpmModuleFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("mjs") => NpmModuleFormat::Esm,
        Some("cjs") => NpmModuleFormat::Cjs,
        Some("json") => NpmModuleFormat::Json,
        _ => {
            // `.js` (or anything else): nearest package.json `"type"` decides,
            // defaulting to CJS exactly like Node.
            let mut dir = path.parent();
            while let Some(current) = dir {
                let manifest_path = current.join("package.json");
                if manifest_path.is_file() {
                    let module_type = std::fs::read_to_string(&manifest_path)
                        .ok()
                        .and_then(|raw| serde_json::from_str::<PackageManifest>(&raw).ok())
                        .and_then(|manifest| manifest.module_type);
                    return if module_type.as_deref() == Some("module") {
                        NpmModuleFormat::Esm
                    } else {
                        NpmModuleFormat::Cjs
                    };
                }
                dir = current.parent();
            }
            NpmModuleFormat::Cjs
        }
    }
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

/// Raw import/require specifiers found in one parsed file.
fn collect_specifiers(
    path: &Path,
    source: &str,
    format: NpmModuleFormat,
) -> Result<Vec<String>, NpmBundleError> {
    match format {
        NpmModuleFormat::Json => Ok(Vec::new()),
        NpmModuleFormat::Esm => {
            let module =
                parse_npm_module(path, source).map_err(|message| NpmBundleError::Compile {
                    key: path.display().to_string(),
                    path: path.to_path_buf(),
                    message,
                })?;
            Ok(esm_specifiers(&module))
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
            Ok(collector.specifiers)
        }
    }
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
        let format = classify_format(&path);

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
