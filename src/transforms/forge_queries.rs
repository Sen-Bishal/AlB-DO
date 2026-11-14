//! T-0033 · Q1a — the one classification pass over every `albedo/forge` and
//! `albedo/query` reference (Design 8).
//!
//! Every **value** reference to a collection imported from `albedo/forge`, or
//! to a helper imported from `albedo/query`, in a compiled runtime module is
//! exactly one of:
//!
//! - **legacy** — `useSharedSlot(collection)` or
//!   `useSharedSlot(collection.where({ col: v }))` with nothing chained
//!   (Design 9). Left to the legacy extractor, byte for byte.
//! - **a new-style query** — a chain (`where* → orderBy* → limit | first |
//!   count | groupBy`, or `get(key)` alone) that lowers to a schema-checked
//!   [`QueryDescriptor`] and SQL per presence mask. Q1a lowers it and then
//!   **refuses** it by name: running it arrives with Q1b.
//! - **a diagnostic** naming `file:line:column` in the original source and the
//!   rule it broke (`const x = guestbook`, `guestbook.length`, a helper outside
//!   a chain, a namespace import, a re-export, …).
//!
//! Identity is the binding, not the spelling: the module runs through SWC's
//! resolver first, so a parameter or local that shadows a collection import is
//! ordinary JS, and an aliased import resolves to its exported name. The pass
//! runs on the raw AST **before** type stripping, and never looks inside type
//! positions, so `import type`, inline `type` specifiers and `typeof x` in a
//! type are ignored.
//!
//! [`enforce_project`] is the early refusal boundary: build, dev and serve call
//! it (through `run_prod_build_with_budget`) before any manifest optimisation or
//! render, and server boot calls it before loading the compiled project, so this
//! diagnostic is always the first error — ahead of the `albedo/forge` tripwire,
//! the legacy extractor's message and any render failure.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use swc_common::{sync::Lrc, Globals, Mark, SourceMap, Span, Spanned, GLOBALS};
use swc_ecma_ast::{
    ArrowExpr, AssignExpr, AssignOp, AssignTarget, CallExpr, Callee, Constructor, Decl,
    DoWhileStmt, ExportNamedSpecifier, Expr, ExprOrSpread, ForInStmt, ForOfStmt, ForStmt,
    Function, GetterProp, Id, Ident, ImportSpecifier, JSXElementName, JSXObject,
    JSXOpeningElement, Lit, MemberProp, Module, ModuleDecl, NamedExport,
    ModuleExportName, ModuleItem, NewExpr, ObjectLit, Pat, Prop, PropName, PropOrSpread,
    ReturnStmt, SetterProp, SimpleAssignTarget, Stmt, TsInterfaceDecl, TsType, TsTypeAliasDecl,
    UpdateExpr, VarDecl, VarDeclKind, WhileStmt,
};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMutWith, VisitWith};

use crate::forge::declare::CollectionDecl;
use crate::forge::query::coerce::Provenance;
use crate::forge::query::index::{IndexColumn, IndexSpec};
use crate::forge::query::{
    lower, Aggregate, Canonical, CollectionSchema, Direction, Lowered, Mode, OrderTerm,
    QueryDescriptor, RangeBound, RowBound, Shape, Term, TermOp,
};

/// Collections, by their declared names. Types-only at run time.
pub const FORGE_MODULE: &str = "albedo/forge";
/// Query helpers (`or`, `optional`, `count`, …). A separate module because a
/// collection may itself be named `count` (Design 1).
pub const QUERY_MODULE: &str = "albedo/query";
/// The framework module `useSharedSlot` comes from.
const ALBEDO_MODULE: &str = "albedo";

const HELPERS: &[&str] = &["or", "optional", "count", "sum", "avg", "min", "max"];
const QUERY_METHODS: &str = "where, orderBy, limit, first, count, groupBy, get";
/// Operator keys that look like text search (Acceptance 1).
const SEARCH_KEYS: &[&str] = &[
    "contains",
    "like",
    "ilike",
    "search",
    "match",
    "startsWith",
    "endsWith",
];

/// Where something was written: the module path as displayed, and 1-based
/// line and column in the **original** source (for a Markdown page, the
/// generated module's — labelled as such).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// Module path relative to the project, `/`-separated.
    pub module: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, in characters.
    pub column: usize,
    /// The coordinates are in generated code (a Markdown page's module).
    pub generated: bool,
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.module, self.line, self.column)?;
        if self.generated {
            f.write_str(" (generated coordinates)")?;
        }
        Ok(())
    }
}

/// A rule broken at a position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Where the rule was broken.
    pub at: Position,
    /// What was wrong, and what to write instead.
    pub message: String,
}

/// One new-style query: where it was written and what it lowered to.
#[derive(Debug, Clone)]
pub struct LoweredQuery {
    /// Where the chain's root collection identifier is written.
    pub at: Position,
    /// The collection it reads.
    pub collection: String,
    /// The canonical descriptor and its id.
    pub canonical: Canonical,
    /// SQL and derived indexes.
    pub lowered: Lowered,
    /// Named predicates it reads from other app modules, as (home module,
    /// export name). Each is checked app-wide before the query stands (R3-1).
    pub imported: Vec<(PathBuf, String)>,
    /// Where each canonical parameter's value comes from at **this** call site
    /// (Design 4, R3-6): kept outside the descriptor's identity, so two sites
    /// sharing one descriptor can coerce the same text differently.
    pub provenance: Vec<Provenance>,
}

/// What the pass found in one module, or in a whole project.
#[derive(Debug, Clone, Default)]
pub struct Classification {
    /// Every new-style query, lowered.
    pub queries: Vec<LoweredQuery>,
    /// Every broken rule.
    pub diagnostics: Vec<Diagnostic>,
    /// Legacy `useSharedSlot` reads, left to the legacy extractor.
    pub legacy: usize,
}

impl Classification {
    fn extend(&mut self, other: Classification) {
        self.queries.extend(other.queries);
        self.diagnostics.extend(other.diagnostics);
        self.legacy += other.legacy;
    }

    /// The build-stopping report, or `None` when nothing is refused.
    ///
    /// Diagnostics come first (a broken rule), then every lowered query, each
    /// refused by name: Q1a compiles and lowers new-style reads but has nothing
    /// that runs them.
    #[must_use]
    pub fn refusal(&self) -> Option<String> {
        if self.diagnostics.is_empty() && self.queries.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        let mut diagnostics = self.diagnostics.clone();
        diagnostics.sort_by(|a, b| a.at.cmp(&b.at));
        for diagnostic in &diagnostics {
            lines.push(format!("  {} — {}", diagnostic.at, diagnostic.message));
        }
        let mut queries: Vec<&LoweredQuery> = self.queries.iter().collect();
        queries.sort_by(|a, b| a.at.cmp(&b.at));
        for query in queries {
            let statements = query.lowered.statements.len();
            lines.push(format!(
                "  {} — query {} on `{}` is compiled and lowered ({} statement{}), but nothing \
                 runs new-style queries yet: that arrives with Q1b. Until then this read can't \
                 be served; read the collection through useSharedSlot(...) instead.",
                query.at,
                query.canonical.id,
                query.collection,
                statements,
                if statements == 1 { "" } else { "s" },
            ));
        }
        Some(format!(
            "FORGE reads refused before anything rendered ({}):\n{}",
            lines.len(),
            lines.join("\n")
        ))
    }
}

/// Classify every compiled runtime module under `root` — the same set
/// `ComponentProject::load_from_dir` loads (components, actions, middleware,
/// jobs, helpers; not declarations, tests or `node_modules`), minus generated
/// `.albedo` output. Paths are displayed relative to `display_base` when they
/// are under it, else relative to `root`.
///
/// # Errors
/// A module that can't be read. A module that doesn't parse is left to the
/// loaders, which already report parse errors with their own context.
pub fn classify_project(
    root: &Path,
    display_base: &Path,
    decls: &BTreeMap<String, CollectionDecl>,
) -> Result<Classification, String> {
    let schemas: BTreeMap<String, CollectionSchema> = decls
        .iter()
        .map(|(name, decl)| (name.clone(), CollectionSchema::from_decl(name, decl)))
        .collect();
    let mut paths = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name();
            name != "node_modules" && name != ".albedo"
        })
    {
        let entry = entry.map_err(|err| format!("failed to scan query sources: {err}"))?;
        let path = entry.path();
        if path.is_file() && crate::runtime::eval::component::is_component_module(path) {
            paths.push(path.to_path_buf());
        }
    }
    paths.sort();
    let shown = |path: &Path| -> String {
        path.strip_prefix(display_base)
            .or_else(|_| path.strip_prefix(root))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    };

    let homes: RefCell<HashMap<PathBuf, Option<Rc<HomeExports>>>> = RefCell::new(HashMap::new());
    let mut accounted: HashMap<PathBuf, HashSet<SpanKey>> = HashMap::new();
    let mut out = Classification::default();
    for path in &paths {
        let source = crate::markdown::read_module_source(path)
            .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
        // Import specifiers can contain JS escapes or line continuations;
        // only the parsed string value can rule out a framework import.
        let generated = crate::markdown::is_markdown_page(path);
        let loader = |specifier: &str, export: &str| -> Option<Loaded> {
            let target = resolve_relative(path, specifier)?;
            let home_path = canonical(&target);
            let home = homes
                .borrow_mut()
                .entry(home_path.clone())
                .or_insert_with(|| load_home(&target, &shown(&target)).map(Rc::new))
                .clone()?;
            let obj = home.predicates.get(export)?.clone();
            Some(Loaded {
                obj,
                home: home_path,
                shown: home.shown.clone(),
                export: export.to_string(),
                source_map: home.source_map.clone(),
            })
        };
        let output = run_pass(&shown(path), &source, generated, &schemas, &loader);
        accounted.insert(canonical(path), output.accounted);
        out.extend(output.classification);
    }
    check_imported_predicates(&mut out, &paths, &shown, &accounted)?;
    Ok(out)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `./x` or `../x` from `importer` → the app module it names (as written, then
/// with each script extension, then as a directory's `index`).
fn resolve_relative(importer: &Path, specifier: &str) -> Option<PathBuf> {
    if !(specifier.starts_with("./") || specifier.starts_with("../")) {
        return None;
    }
    // Lexically, so `src/routes/../lib/x` is shown as `src/lib/x`.
    let mut base = PathBuf::new();
    for part in importer.parent()?.join(specifier).components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                base.pop();
            }
            other => base.push(other),
        }
    }
    const EXTENSIONS: [&str; 4] = ["ts", "tsx", "js", "jsx"];
    std::iter::once(base.clone())
        .chain(EXTENSIONS.iter().map(|ext| PathBuf::from(format!("{}.{ext}", base.display()))))
        .chain(EXTENSIONS.iter().map(|ext| base.join(format!("index.{ext}"))))
        .find(|candidate| {
            candidate.is_file() && crate::runtime::eval::component::is_component_module(candidate)
        })
}

/// A module's exported `const` object literals, by export name.
struct HomeExports {
    shown: String,
    source_map: Lrc<SourceMap>,
    predicates: HashMap<String, ObjectLit>,
}

fn load_home(path: &Path, shown: &str) -> Option<HomeExports> {
    let source = crate::markdown::read_module_source(path).ok()?;
    let syntax = crate::runtime::quickjs_engine::syntax_for_specifier(shown);
    let (module, source_map) =
        crate::runtime::quickjs_engine::parse_module_with_fallback(shown, &source, syntax).ok()?;
    let mut local: HashMap<String, ObjectLit> = HashMap::new();
    let mut predicates = HashMap::new();
    for item in &module.body {
        let (var, exported) = match item {
            ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))) => (var, false),
            ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => match &export.decl {
                Decl::Var(var) => (var, true),
                _ => continue,
            },
            _ => continue,
        };
        if var.kind != VarDeclKind::Const {
            continue;
        }
        for decl in &var.decls {
            let (Pat::Ident(binding), Some(init)) = (&decl.name, &decl.init) else {
                continue;
            };
            if let Expr::Object(obj) = unwrap(init) {
                let name = binding.id.sym.to_string();
                if exported {
                    predicates.insert(name.clone(), obj.clone());
                }
                local.insert(name, obj.clone());
            }
        }
    }
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) = item else {
            continue;
        };
        if export.src.is_some() || export.type_only {
            continue;
        }
        for spec in &export.specifiers {
            let swc_ecma_ast::ExportSpecifier::Named(named) = spec else {
                continue;
            };
            if named.is_type_only {
                continue;
            }
            let ModuleExportName::Ident(orig) = &named.orig else {
                continue;
            };
            let name = match &named.exported {
                Some(ModuleExportName::Ident(e)) => e.sym.to_string(),
                Some(ModuleExportName::Str(e)) => e.value.to_string(),
                None => orig.sym.to_string(),
            };
            if let Some(obj) = local.get(orig.sym.as_ref()) {
                predicates.insert(name, obj.clone());
            }
        }
    }
    Some(HomeExports {
        shown: shown.to_string(),
        source_map,
        predicates,
    })
}

/// R3-1, app-wide: an imported named predicate some query read may have no
/// other value use — in its home module, in any module that imports it, and no
/// re-export. Each violation is a diagnostic at the use; every query that read
/// that predicate is dropped.
fn check_imported_predicates(
    out: &mut Classification,
    paths: &[PathBuf],
    shown: &dyn Fn(&Path) -> String,
    accounted: &HashMap<PathBuf, HashSet<SpanKey>>,
) -> Result<(), String> {
    let used: BTreeSet<(PathBuf, String)> = out
        .queries
        .iter()
        .flat_map(|q| q.imported.iter().cloned())
        .collect();
    if used.is_empty() {
        return Ok(());
    }
    let mut violated: HashSet<(PathBuf, String)> = HashSet::new();
    for path in paths {
        let source = crate::markdown::read_module_source(path)
            .map_err(|err| format!("failed to read '{}': {err}", path.display()))?;
        let here = canonical(path);
        // Re-exports and namespace accesses need not spell the export name.
        // Parse each module rather than using source-text shortcuts: comments,
        // whitespace and computed properties do not change the binding rule.
        let wanted: Vec<&(PathBuf, String)> = used.iter().collect();
        let none = HashSet::new();
        let accounted = accounted.get(&here).unwrap_or(&none);
        for diagnostic in imported_strays(path, &shown(path), &source, &here, &wanted, accounted) {
            violated.insert(diagnostic.0);
            if !out.diagnostics.iter().any(|d| d.at == diagnostic.1.at) {
                out.diagnostics.push(diagnostic.1);
            }
        }
    }
    out.queries
        .retain(|q| !q.imported.iter().any(|read| violated.contains(read)));
    Ok(())
}

/// In one module, every use of a `wanted` predicate that isn't a query argument.
fn imported_strays(
    path: &Path,
    shown: &str,
    source: &str,
    here: &Path,
    wanted: &[&(PathBuf, String)],
    accounted: &HashSet<SpanKey>,
) -> Vec<((PathBuf, String), Diagnostic)> {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let syntax = crate::runtime::quickjs_engine::syntax_for_specifier(shown);
        let Ok((mut module, source_map)) =
            crate::runtime::quickjs_engine::parse_module_with_fallback(shown, source, syntax)
        else {
            return Vec::new();
        };
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        let mut census = Census::default();
        module.visit_with(&mut census);
        let at = |span: Span| {
            let loc = source_map.lookup_char_pos(span.lo);
            Position {
                module: shown.to_string(),
                line: loc.line,
                column: loc.col.0 + 1,
                generated: false,
            }
        };
        // (predicate, binding here, spans that are its declaration or export)
        let mut bindings: Vec<(&(PathBuf, String), Id, HashSet<SpanKey>)> = Vec::new();
        let mut found = Vec::new();
        for item in &module.body {
            match item {
                ModuleItem::ModuleDecl(ModuleDecl::Import(import)) if !import.type_only => {
                    let Some(target) = resolve_relative(path, import.src.value.as_ref()) else {
                        continue;
                    };
                    let target = canonical(&target);
                    for spec in &import.specifiers {
                        if let ImportSpecifier::Namespace(namespace) = spec {
                            for read in wanted.iter().filter(|(home, _)| *home == target) {
                                let mut uses = NamespaceUses {
                                    binding: namespace.local.to_id(),
                                    export: &read.1,
                                    found: Vec::new(),
                                };
                                module.visit_with(&mut uses);
                                if let Some(span) = uses.found.into_iter().min_by_key(|s| s.lo) {
                                    found.push((
                                        (*read).clone(),
                                        Diagnostic {
                                            at: at(span),
                                            message: format!(
                                                "`{}` is a predicate a query reads, so accessing it \
                                                 through a namespace outside a query is refused",
                                                read.1
                                            ),
                                        },
                                    ));
                                }
                            }
                            continue;
                        }
                        let ImportSpecifier::Named(named) = spec else {
                            continue;
                        };
                        if named.is_type_only {
                            continue;
                        }
                        let exported = match &named.imported {
                            Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                            Some(ModuleExportName::Str(s)) => s.value.to_string(),
                            None => named.local.sym.to_string(),
                        };
                        if let Some(read) = wanted.iter().find(|(h, e)| *h == target && *e == exported) {
                            bindings.push((read, named.local.to_id(), HashSet::from([key(named.local.span)])));
                        }
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) => {
                    if export.type_only {
                        continue;
                    }
                    let Some(src) = &export.src else {
                        continue;
                    };
                    let Some(target) = resolve_relative(path, src.value.as_ref()) else {
                        continue;
                    };
                    let target = canonical(&target);
                    for spec in &export.specifiers {
                        if let swc_ecma_ast::ExportSpecifier::Namespace(namespace) = spec {
                            for read in wanted.iter().filter(|(home, _)| *home == target) {
                                found.push((
                                    (*read).clone(),
                                    Diagnostic {
                                        at: at(namespace.span),
                                        message: format!(
                                            "namespace re-exporting `{}` is refused: it is a \
                                             predicate a query reads",
                                            read.1
                                        ),
                                    },
                                ));
                            }
                            continue;
                        }
                        let swc_ecma_ast::ExportSpecifier::Named(named) = spec else {
                            continue;
                        };
                        if named.is_type_only {
                            continue;
                        }
                        let (name, span) = match &named.orig {
                            ModuleExportName::Ident(orig) => (orig.sym.as_ref(), orig.span),
                            ModuleExportName::Str(orig) => (orig.value.as_ref(), orig.span),
                        };
                        if let Some(read) = wanted
                            .iter()
                            .find(|(h, e)| *h == target && e.as_str() == name)
                        {
                            found.push((
                                (*read).clone(),
                                Diagnostic {
                                    at: at(span),
                                    message: format!(
                                        "re-exporting `{}` is refused: it is a predicate a query \
                                         reads, so import it from its home module where it's used",
                                        name
                                    ),
                                },
                            ));
                        }
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportAll(export)) => {
                    if export.type_only {
                        continue;
                    }
                    let Some(target) = resolve_relative(path, export.src.value.as_ref()) else {
                        continue;
                    };
                    let target = canonical(&target);
                    for read in wanted.iter().filter(|(h, _)| *h == target) {
                        found.push((
                            (*read).clone(),
                            Diagnostic {
                                at: at(export.span),
                                message: format!(
                                    "`export *` re-exports `{}`, a predicate a query reads: \
                                     import it from its home module where it's used",
                                    read.1
                                ),
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
        // The home module: the exported binding itself.
        for read in wanted.iter().filter(|(h, _)| h.as_path() == here) {
            // `export { local as name }` → `local`; otherwise the export is its
            // own binding name.
            let mut own: HashSet<SpanKey> = HashSet::new();
            let mut local_name = read.1.clone();
            for item in &module.body {
                let ModuleItem::ModuleDecl(ModuleDecl::ExportNamed(export)) = item else {
                    continue;
                };
                if export.src.is_some() || export.type_only {
                    continue;
                }
                for spec in &export.specifiers {
                    let swc_ecma_ast::ExportSpecifier::Named(named) = spec else {
                        continue;
                    };
                    if named.is_type_only {
                        continue;
                    }
                    let ModuleExportName::Ident(orig) = &named.orig else {
                        continue;
                    };
                    own.insert(key(orig.span));
                    let name = match &named.exported {
                        Some(ModuleExportName::Ident(e)) => e.sym.to_string(),
                        Some(ModuleExportName::Str(e)) => e.value.to_string(),
                        None => orig.sym.to_string(),
                    };
                    if name == read.1 {
                        local_name = orig.sym.to_string();
                    }
                }
            }
            let mut binding: Option<Id> = None;
            for item in &module.body {
                let var = match item {
                    ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))) => var,
                    ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export)) => match &export.decl {
                        Decl::Var(var) => var,
                        _ => continue,
                    },
                    _ => continue,
                };
                for decl in &var.decls {
                    if let Pat::Ident(b) = &decl.name {
                        if b.id.sym.as_ref() == local_name {
                            binding = Some(b.id.to_id());
                            own.insert(key(b.id.span));
                        }
                    }
                }
            }
            if let Some(id) = binding {
                bindings.push((read, id, own));
            }
        }
        for (read, id, own) in bindings {
            let stray = census
                .refs
                .get(&id)
                .into_iter()
                .flatten()
                .filter(|r| !own.contains(&key(r.span)) && !accounted.contains(&key(r.span)))
                .min_by_key(|r| r.span.lo)
                .copied();
            if let Some(stray) = stray {
                found.push((
                    read.clone(),
                    Diagnostic {
                        at: at(stray.span),
                        message: format!(
                            "`{}` is a predicate a query reads when it runs, so it may only be \
                             passed to `.where()` or `or()`, in every module: here it is {}",
                            read.1,
                            stray_reason(stray.role)
                        ),
                    },
                ));
            }
        }
        found
    })
}

/// Namespace aliases must not hide a predicate's mutation or escape. Access to
/// a different, statically named export is unrelated; a computed name or an
/// escaped namespace can reach the predicate and cannot be proved safe.
struct NamespaceUses<'a> {
    binding: Id,
    export: &'a str,
    found: Vec<Span>,
}

impl Visit for NamespaceUses<'_> {
    fn visit_jsx_member_expr(&mut self, member: &swc_ecma_ast::JSXMemberExpr) {
        if matches!(&member.obj, JSXObject::Ident(ident) if ident.to_id() == self.binding)
            && member.prop.sym.as_ref() == self.export
        {
            self.found.push(member.span);
        }
        member.visit_children_with(self);
    }

    fn visit_named_export(&mut self, export: &NamedExport) {
        if !export.type_only {
            export.visit_children_with(self);
        }
    }

    fn visit_export_named_specifier(&mut self, spec: &ExportNamedSpecifier) {
        if !spec.is_type_only {
            if let ModuleExportName::Ident(ident) = &spec.orig {
                if ident.to_id() == self.binding {
                    self.found.push(ident.span);
                }
            }
        }
    }

    fn visit_prop(&mut self, prop: &Prop) {
        if let Prop::Shorthand(ident) = prop {
            self.visit_expr(&Expr::Ident(ident.clone()));
        } else {
            prop.visit_children_with(self);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Ident(ident) = unwrap(expr) {
            if ident.to_id() == self.binding {
                self.found.push(ident.span);
                return;
            }
        }
        expr.visit_children_with(self);
    }

    fn visit_member_expr(&mut self, member: &swc_ecma_ast::MemberExpr) {
        if matches!(unwrap(&member.obj), Expr::Ident(ident) if ident.to_id() == self.binding) {
            let name = match &member.prop {
                MemberProp::Ident(ident) => Some(ident.sym.to_string()),
                MemberProp::Computed(computed) => string_literal(&computed.expr),
                MemberProp::PrivateName(_) => None,
            };
            if name.as_deref().map_or(true, |name| name == self.export) {
                self.found.push(member.span);
            }
            // The computed name may itself contain a predicate access.
            member.prop.visit_with(self);
        } else {
            member.visit_children_with(self);
        }
    }
}

fn stray_reason(role: Role) -> &'static str {
    match role {
        Role::CallArg => "passed to a function",
        Role::MemberWrite => "written to outside a query",
        Role::Assign => "reassigned outside a query",
        Role::Returned => "returned outside a query",
        Role::Read => "used outside a query",
    }
}

/// The early refusal boundary: [`classify_project`], then fail with
/// [`Classification::refusal`] when anything is refused.
///
/// With `report_to`, the query report ([`query_report`]) is written there when
/// any new-style query was found, and any stale one removed when none was — so
/// an app with no new-style query never has the file (Acceptance 7).
///
/// # Errors
/// The refusal report, or a module that can't be read.
pub fn enforce_project(
    root: &Path,
    display_base: &Path,
    decls: &BTreeMap<String, CollectionDecl>,
    report_to: Option<&Path>,
) -> Result<Classification, String> {
    let classification = classify_project(root, display_base, decls)?;
    let mut written = None;
    if let Some(path) = report_to {
        match query_report(&classification, decls) {
            Some(report) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(path, report).is_ok() {
                    written = Some(path);
                }
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    match classification.refusal() {
        Some(mut refusal) => {
            if let Some(path) = written {
                refusal.push_str(&format!(
                    "\n  Each query's SQL, parameters and derived indexes: {}",
                    path.display()
                ));
            }
            Err(refusal)
        }
        None => Ok(classification),
    }
}

/// The legacy `useSharedSlot` reads of a module, by call position (R5-7).
///
/// A call is a legacy site when it is `useSharedSlot(<collection>)`, or
/// `useSharedSlot(<collection>.where({ <one key>: <value> }))`, with the hook and
/// the collection both the module's own imports **by binding**. The module must
/// already have been through SWC's resolver, so a parameter or local that shadows
/// the name is not a site. The key is the call's absolute start offset within the
/// module's own source map: every consumer reads the same parse, so the offset
/// means the same call to each of them.
///
/// The classifier refuses every non-legacy shape before any consumer runs; a site
/// here is therefore only a reference the boundary has already accepted, or one
/// a consumer rejects by its own message when the boundary did not run.
#[must_use]
pub fn legacy_shared_slot_sites(resolved: &Module) -> HashSet<u32> {
    legacy_shared_slot_index(resolved).sites
}

/// What a legacy partition read's key is, **by binding** (Design 4): the value
/// of `<collection>.where({ <column>: <value> })` traced to the entry
/// component's first parameter, carrying the field read off it. This is the
/// classifier's own provenance, handed to the legacy extractor so the two
/// cannot disagree about `{ params: route }` → `route.id`, a whole `props` →
/// `props.params.id`, or `{ user: actor }` → `actor.id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyKey {
    /// A route param, by name: `params.id`, `route.id`, `props.params.id`.
    Param(String),
    /// A field of the principal: `user.id`, `actor.id`, `props.user.id`. Only
    /// `id` is a valid partition key; the extractor refuses any other field
    /// with its own message.
    Principal(String),
}

/// [`legacy_shared_slot_sites`] plus, for each legacy partition read, its key
/// by binding — keyed by the **value expression's** start offset, so a consumer
/// looks it up from the expression it is already holding.
#[derive(Debug, Clone, Default)]
pub struct LegacySiteIndex {
    pub sites: HashSet<u32>,
    pub keys: HashMap<u32, LegacyKey>,
}

/// [`legacy_shared_slot_sites`], with each partition read's key by binding. The
/// module must already have been through SWC's resolver.
#[must_use]
pub fn legacy_shared_slot_index(resolved: &Module) -> LegacySiteIndex {
    let mut collections: HashSet<Id> = HashSet::new();
    let mut hooks: HashSet<Id> = HashSet::new();
    for item in &resolved.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        if import.type_only {
            continue;
        }
        let source = import.src.value.as_ref();
        for spec in &import.specifiers {
            let ImportSpecifier::Named(named) = spec else {
                continue;
            };
            if named.is_type_only {
                continue;
            }
            let exported = match &named.imported {
                Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                Some(ModuleExportName::Str(s)) => s.value.to_string(),
                None => named.local.sym.to_string(),
            };
            match source {
                FORGE_MODULE => {
                    collections.insert(named.local.to_id());
                }
                ALBEDO_MODULE if exported == "useSharedSlot" => {
                    hooks.insert(named.local.to_id());
                }
                _ => {}
            }
        }
    }
    let mut finder = LegacySites {
        collections,
        hooks,
        props: PropBindings::from_module(resolved),
        index: LegacySiteIndex::default(),
    };
    resolved.visit_with(&mut finder);
    finder.index
}

/// [`legacy_shared_slot_sites`] for a module that has not been resolved: resolves
/// a copy first. The module itself is left untouched, so its positions are the
/// same as the caller's.
#[must_use]
pub fn legacy_shared_slot_sites_unresolved(module: &Module) -> HashSet<u32> {
    legacy_shared_slot_index_unresolved(module).sites
}

/// [`legacy_shared_slot_index`] for a module that has not been resolved.
#[must_use]
pub fn legacy_shared_slot_index_unresolved(module: &Module) -> LegacySiteIndex {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let mut resolved = module.clone();
        resolved.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        legacy_shared_slot_index(&resolved)
    })
}

struct LegacySites {
    collections: HashSet<Id>,
    hooks: HashSet<Id>,
    props: PropBindings,
    index: LegacySiteIndex,
}

impl Visit for LegacySites {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Callee::Expr(callee) = &call.callee {
            if let Expr::Ident(hook) = unwrap(callee) {
                if self.hooks.contains(&hook.to_id()) {
                    if let Some(first) = call.args.first() {
                        if first.spread.is_none() && self.legacy_arg(&first.expr) {
                            self.index.sites.insert(call.span.lo.0);
                            self.record_key(&first.expr);
                        }
                    }
                }
            }
        }
        call.visit_children_with(self);
    }
}

impl LegacySites {
    /// For `<collection>.where({ <column>: <value> })`, record `<value>`'s key by
    /// binding. A value that is not a route param or the principal records
    /// nothing; the extractor then judges it by its own rules.
    fn record_key(&mut self, arg: &Expr) {
        let Expr::Call(call) = unwrap(arg) else {
            return;
        };
        let [only] = call.args.as_slice() else {
            return;
        };
        let Expr::Object(obj) = unwrap(&only.expr) else {
            return;
        };
        let [PropOrSpread::Prop(prop)] = obj.props.as_slice() else {
            return;
        };
        let Prop::KeyValue(kv) = &**prop else {
            return;
        };
        if let Some(key) = self.bound_key(&kv.value) {
            self.index.keys.insert(kv.value.span().lo.0, key);
        }
    }

    /// `<route>.<field>`, `<principal>.<field>`, or `<props>.params|user.<field>`,
    /// where each root is the entry component's parameter **by binding**.
    fn bound_key(&self, value: &Expr) -> Option<LegacyKey> {
        let Expr::Member(outer) = unwrap(value) else {
            return None;
        };
        let MemberProp::Ident(field) = &outer.prop else {
            return None;
        };
        let field = field.sym.to_string();
        match unwrap(&outer.obj) {
            Expr::Ident(root) => {
                let id = root.to_id();
                if self.props.route.contains(&id) {
                    Some(LegacyKey::Param(field))
                } else if self.props.principal.contains(&id) {
                    Some(LegacyKey::Principal(field))
                } else {
                    None
                }
            }
            Expr::Member(inner) => {
                let Expr::Ident(root) = unwrap(&inner.obj) else {
                    return None;
                };
                let MemberProp::Ident(via) = &inner.prop else {
                    return None;
                };
                if !self.props.whole.contains(&root.to_id()) {
                    return None;
                }
                match via.sym.as_ref() {
                    "params" => Some(LegacyKey::Param(field)),
                    "user" => Some(LegacyKey::Principal(field)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// `<collection>` or `<collection>.where({ <one key>: <value> })`.
    fn legacy_arg(&self, expr: &Expr) -> bool {
        match unwrap(expr) {
            Expr::Ident(ident) => self.collections.contains(&ident.to_id()),
            Expr::Call(call) => {
                let Callee::Expr(callee) = &call.callee else {
                    return false;
                };
                let Expr::Member(member) = unwrap(callee) else {
                    return false;
                };
                let MemberProp::Ident(method) = &member.prop else {
                    return false;
                };
                if method.sym.as_ref() != "where" {
                    return false;
                }
                let Expr::Ident(root) = unwrap(&member.obj) else {
                    return false;
                };
                if !self.collections.contains(&root.to_id()) {
                    return false;
                }
                // Any one argument: the shape (one column, a literal key, …) is
                // the extractor's to validate with its own messages, and the
                // classifier has already refused everything it would not accept.
                let [arg] = call.args.as_slice() else {
                    return false;
                };
                arg.spread.is_none()
            }
            _ => false,
        }
    }
}

/// Most derived indexes on one table before the report warns (Design 11).
pub const DERIVED_INDEX_WARNING: usize = 6;

/// The build's query report (Design 1, 11): every lowered query with its SQL per
/// presence mask and parameter plan, which reads are unbounded, which may
/// examine a whole table, and the derived indexes after dedup — against each
/// other and against the legacy partition indexes — with each index's column
/// list. `None` when the app has no new-style query, so no file exists.
///
/// It is written by the early boundary, not by `preflight`: preflight runs
/// after manifest optimisation, which the boundary stops before whenever a
/// new-style query exists.
#[must_use]
pub fn query_report(
    classification: &Classification,
    decls: &BTreeMap<String, CollectionDecl>,
) -> Option<String> {
    use std::fmt::Write as _;
    if classification.queries.is_empty() {
        return None;
    }
    let schemas: BTreeMap<String, CollectionSchema> = decls
        .iter()
        .map(|(name, decl)| (name.clone(), CollectionSchema::from_decl(name, decl)))
        .collect();
    let mut queries: Vec<&LoweredQuery> = classification.queries.iter().collect();
    queries.sort_by(|a, b| a.at.cmp(&b.at));

    let mut out = String::new();
    let _ = writeln!(
        out,
        "# FORGE query report — {} new-style quer{} (Q1a lowers them; Q1b runs them)\n",
        queries.len(),
        if queries.len() == 1 { "y" } else { "ies" }
    );
    let mut wanted_by_table: BTreeMap<String, Vec<IndexSpec>> = BTreeMap::new();
    let mut unbounded = Vec::new();
    let mut whole_table = Vec::new();
    for query in &queries {
        let Some(schema) = schemas.get(&query.collection) else {
            continue;
        };
        let d = &query.canonical.descriptor;
        let _ = writeln!(out, "## {} — {} on `{}`", query.at, query.canonical.id, query.collection);
        let _ = writeln!(out, "descriptor: {}", query.canonical.json);
        for statement in &query.lowered.statements {
            let _ = writeln!(out, "mask {:03b}: {}", statement.mask, statement.sql);
            let _ = writeln!(out, "  binds: {:?}", statement.binds);
        }
        for param in &query.lowered.params {
            let _ = writeln!(
                out,
                "  ?slot {}: `{}` ({}) as {:?}",
                param.slot,
                param.column,
                crate::forge::query::descriptor::type_name(param.ty),
                param.role
            );
        }
        let wanted = crate::forge::query::index::wanted(d, schema);
        if d.shape == Shape::Rows && d.bound == RowBound::None {
            unbounded.push(query.at.to_string());
        }
        let constrained = !d.terms.is_empty() || !d.or_groups.is_empty();
        let get = matches!(d.shape, Shape::Get { .. });
        if !get && !constrained && (d.bound == RowBound::None || wanted.is_empty()) {
            whole_table.push(query.at.to_string());
        }
        wanted_by_table.entry(schema.table.clone()).or_default().extend(wanted);
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "## Derived indexes (deduplicated; Q1b applies them)");
    for (table, wanted) in wanted_by_table {
        let legacy: Vec<IndexSpec> = schemas
            .values()
            .filter(|s| s.table == table)
            .filter_map(legacy_partition_index)
            .collect();
        let kept = crate::forge::query::index::dedup(wanted, &legacy);
        for index in &kept {
            let _ = writeln!(out, "{}  -- ({})", index.ddl(), index.column_spec());
        }
        if kept.len() > DERIVED_INDEX_WARNING {
            let _ = writeln!(
                out,
                "warning: {} derived indexes on `{table}` (more than {DERIVED_INDEX_WARNING}): every write maintains each one",
                kept.len()
            );
        }
    }
    let _ = writeln!(out, "\n## Unbounded reads (no limit/first): {}", unbounded.len());
    for at in &unbounded {
        let _ = writeln!(out, "  {at}");
    }
    let _ = writeln!(
        out,
        "\n## May examine a whole table (no term constrains it): {}",
        whole_table.len()
    );
    for at in &whole_table {
        let _ = writeln!(out, "  {at}");
    }
    let _ = writeln!(
        out,
        "\nNote: `limit` bounds rows returned, not rows examined or sorted. Plans were measured \
         on libsql without ANALYZE; with every derived index present, the planner can pick \
         another query's index and sort (see T-0033 plans-libsql.txt)."
    );
    Some(out)
}

/// The legacy `(partition, order…, key)` index a partitioned collection
/// already has (`forge::declare`), so a query it serves derives nothing.
fn legacy_partition_index(schema: &CollectionSchema) -> Option<IndexSpec> {
    let partition = schema.partition_by.as_ref()?;
    let mut columns = vec![IndexColumn {
        column: partition.clone(),
        dir: Direction::Asc,
    }];
    for term in &schema.default_order {
        if &term.column != partition && !columns.iter().any(|c| c.column == term.column) {
            columns.push(IndexColumn {
                column: term.column.clone(),
                dir: term.dir,
            });
        }
    }
    if !columns.iter().any(|c| c.column == schema.key) {
        columns.push(IndexColumn {
            column: schema.key.clone(),
            dir: Direction::Asc,
        });
    }
    Some(IndexSpec {
        table: schema.table.clone(),
        columns,
    })
}

/// Classify one module. `module` is the path shown in diagnostics.
#[must_use]
pub fn classify_source(
    module: &str,
    source: &str,
    generated: bool,
    schemas: &BTreeMap<String, CollectionSchema>,
) -> Classification {
    run_pass(module, source, generated, schemas, &|_, _| None).classification
}

/// The module with every new-style query rewritten to the handle calls the Q1b
/// runtime defines (Design 2), and everything else byte for byte:
///
/// - an inline chain → `__albedo_q_run(__albedo_q_new("<id>", [slots], [values]), [], [])`
/// - `let q = <chain>` → `let q = __albedo_q_new("<id>", [slots], [values])`
/// - `if (…) q = q.where({ c: v })` → `if (…) q = __albedo_q_set(q, <optional slot>, [slots], [values])`
/// - the one use → `__albedo_q_run(q, [slots], [values])`
///
/// `values` are the authored expressions in source order, each written once
/// where the author wrote it (a const predicate's are read from the object at
/// the query site, R3-1); `slots` maps each to its canonical parameter. Handles
/// are values: `set` returns a new one, so a throwing argument leaves the
/// previous handle as it was, and two invocations never share one.
///
/// Q1a refuses every new-style query before anything runs, and defines no
/// `__albedo_q_*` (Design 8): this is the shape Q1b executes, proven here
/// against stub helpers.
///
/// # Errors
/// The module's diagnostics, when any rule is broken.
pub fn rewrite_source(
    module: &str,
    source: &str,
    schemas: &BTreeMap<String, CollectionSchema>,
) -> Result<String, Vec<Diagnostic>> {
    let PassOutput {
        classification,
        mut edits,
        ..
    } = run_pass(module, source, false, schemas, &|_, _| None);
    if !classification.diagnostics.is_empty() {
        return Err(classification.diagnostics);
    }
    edits.sort_by(|a, b| a.lo.cmp(&b.lo).then(b.hi.cmp(&a.hi)));
    Ok(render(source, &edits, 0, source.len()))
}

/// A named predicate a loader found one hop away.
struct Loaded {
    obj: ObjectLit,
    home: PathBuf,
    shown: String,
    export: String,
    source_map: Lrc<SourceMap>,
}

/// An imported named predicate, as one importing module sees it.
struct ForeignPred {
    loaded: Loaded,
    /// The importing module's own binding identifier, which isn't a use.
    local: Span,
}

impl ForeignPred {
    /// A span inside the predicate, in its home module's coordinates.
    fn position(&self, span: Span) -> String {
        let loc = self.loaded.source_map.lookup_char_pos(span.lo);
        format!("{}:{}:{}", self.loaded.shown, loc.line, loc.col.0 + 1)
    }
}

struct PassOutput {
    classification: Classification,
    edits: Vec<Edit>,
    /// Named-predicate references that are `.where()` / `or()` arguments.
    accounted: HashSet<SpanKey>,
}

/// `import { x } from "./y"` → (binding, specifier, export name, binding span).
fn relative_imports(module: &Module) -> Vec<(Id, String, String, Span)> {
    let mut out = Vec::new();
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        let source = import.src.value.as_ref();
        if import.type_only || !(source.starts_with("./") || source.starts_with("../")) {
            continue;
        }
        for spec in &import.specifiers {
            let ImportSpecifier::Named(named) = spec else {
                continue;
            };
            if named.is_type_only {
                continue;
            }
            let exported = match &named.imported {
                Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                Some(ModuleExportName::Str(s)) => s.value.to_string(),
                None => named.local.sym.to_string(),
            };
            out.push((named.local.to_id(), source.to_string(), exported, named.local.span));
        }
    }
    out
}

fn run_pass(
    module: &str,
    source: &str,
    generated: bool,
    schemas: &BTreeMap<String, CollectionSchema>,
    loader: &dyn Fn(&str, &str) -> Option<Loaded>,
) -> PassOutput {
    let globals = Globals::new();
    GLOBALS.set(&globals, || {
        let syntax = crate::runtime::quickjs_engine::syntax_for_specifier(module);
        let Ok((mut parsed, source_map)) =
            crate::runtime::quickjs_engine::parse_module_with_fallback(module, source, syntax)
        else {
            return PassOutput {
                classification: Classification::default(),
                edits: Vec::new(),
                accounted: HashSet::new(),
            };
        };
        let unresolved_mark = Mark::new();
        let top_level_mark = Mark::new();
        parsed.visit_mut_with(&mut resolver(unresolved_mark, top_level_mark, true));
        let file_start = source_map.files().first().map_or(0, |file| file.start_pos.0);

        let mut census = Census::default();
        parsed.visit_with(&mut census);
        let mut consts = Consts::default();
        parsed.visit_with(&mut consts);
        let props = PropBindings::from_module(&parsed);
        let mut foreign: HashMap<Id, ForeignPred> = HashMap::new();
        for (id, specifier, export, local) in relative_imports(&parsed) {
            if let Some(loaded) = loader(&specifier, &export) {
                foreign.insert(id, ForeignPred { loaded, local });
            }
        }

        let mut pass = Pass {
            module,
            source,
            file_start,
            generated,
            source_map,
            schemas,
            census: &census.refs,
            consts: &consts,
            foreign: &foreign,
            props: &props,
            collections: HashMap::new(),
            helpers: HashMap::new(),
            use_shared_slot: HashSet::new(),
            claimed: HashSet::new(),
            accounted: HashSet::new(),
            preds_used: HashMap::new(),
            pending: Vec::new(),
            out: Classification::default(),
        };
        pass.collect_imports(&parsed);
        if pass.collections.is_empty() && pass.helpers.is_empty() && pass.out.diagnostics.is_empty()
        {
            return PassOutput {
                classification: pass.out,
                edits: Vec::new(),
                accounted: HashSet::new(),
            };
        }
        parsed.visit_with(&mut pass);
        pass.settle()
    })
}

// ── Text edits ───────────────────────────────────────────────────────────────

/// Replace `source[lo..hi]` with `pieces`. A [`Piece::Hole`] is a range of the
/// original source, rendered with any edits inside it applied — so a query
/// written inside another query's argument is rewritten too.
struct Edit {
    lo: usize,
    hi: usize,
    pieces: Vec<Piece>,
}

enum Piece {
    Text(String),
    Hole(usize, usize),
}

/// `source[lo..hi]` with every outermost edit inside it applied. `edits` is
/// sorted by `lo`, outer edits before the edits they contain.
fn render(source: &str, edits: &[Edit], lo: usize, hi: usize) -> String {
    let mut out = String::new();
    let mut cursor = lo;
    for edit in edits {
        if edit.lo < cursor || edit.hi > hi {
            continue;
        }
        out.push_str(&source[cursor..edit.lo]);
        for piece in &edit.pieces {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Hole(a, b) => out.push_str(&render(source, edits, *a, *b)),
            }
        }
        cursor = edit.hi;
    }
    out.push_str(&source[cursor..hi]);
    out
}

// ── Census: every value reference, by binding ────────────────────────────────

type SpanKey = (u32, u32);

fn key(span: Span) -> SpanKey {
    (span.lo.0, span.hi.0)
}

/// How a reference is used, from its immediate parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Read,
    Assign,
    MemberWrite,
    CallArg,
    Returned,
}

#[derive(Debug, Clone, Copy)]
struct Ref {
    span: Span,
    fn_depth: u32,
    loop_depth: u32,
    role: Role,
}

/// Every identifier in a value position, keyed by its resolved binding, with
/// how deep in functions and loops it sits and how its parent uses it. Type
/// positions are skipped: they are types, not uses (Design 8).
#[derive(Default)]
struct Census {
    refs: HashMap<Id, Vec<Ref>>,
    marks: HashMap<SpanKey, Role>,
    fn_depth: u32,
    loop_depth: u32,
}

impl Census {
    fn mark(&mut self, expr: &Expr, role: Role) {
        if let Expr::Ident(ident) = unwrap(expr) {
            self.marks.insert(key(ident.span), role);
        }
    }

    fn in_function(&mut self, visit: impl FnOnce(&mut Self)) {
        self.fn_depth += 1;
        visit(self);
        self.fn_depth -= 1;
    }

    fn in_loop(&mut self, visit: impl FnOnce(&mut Self)) {
        self.loop_depth += 1;
        visit(self);
        self.loop_depth -= 1;
    }
}

impl Visit for Census {
    fn visit_ident(&mut self, ident: &Ident) {
        let role = self.marks.get(&key(ident.span)).copied().unwrap_or(Role::Read);
        self.refs.entry(ident.to_id()).or_default().push(Ref {
            span: ident.span,
            fn_depth: self.fn_depth,
            loop_depth: self.loop_depth,
            role,
        });
    }

    fn visit_function(&mut self, node: &Function) {
        self.in_function(|v| node.visit_children_with(v));
    }
    fn visit_arrow_expr(&mut self, node: &ArrowExpr) {
        self.in_function(|v| node.visit_children_with(v));
    }
    fn visit_getter_prop(&mut self, node: &GetterProp) {
        self.in_function(|v| node.visit_children_with(v));
    }
    fn visit_setter_prop(&mut self, node: &SetterProp) {
        self.in_function(|v| node.visit_children_with(v));
    }
    fn visit_constructor(&mut self, node: &Constructor) {
        self.in_function(|v| node.visit_children_with(v));
    }

    fn visit_for_stmt(&mut self, node: &ForStmt) {
        self.in_loop(|v| node.visit_children_with(v));
    }
    fn visit_for_in_stmt(&mut self, node: &ForInStmt) {
        self.in_loop(|v| node.visit_children_with(v));
    }
    fn visit_for_of_stmt(&mut self, node: &ForOfStmt) {
        self.in_loop(|v| node.visit_children_with(v));
    }
    fn visit_while_stmt(&mut self, node: &WhileStmt) {
        self.in_loop(|v| node.visit_children_with(v));
    }
    fn visit_do_while_stmt(&mut self, node: &DoWhileStmt) {
        self.in_loop(|v| node.visit_children_with(v));
    }

    fn visit_call_expr(&mut self, node: &CallExpr) {
        for arg in &node.args {
            self.mark(&arg.expr, Role::CallArg);
        }
        node.visit_children_with(self);
    }
    fn visit_new_expr(&mut self, node: &NewExpr) {
        for arg in node.args.iter().flatten() {
            self.mark(&arg.expr, Role::CallArg);
        }
        node.visit_children_with(self);
    }
    fn visit_return_stmt(&mut self, node: &ReturnStmt) {
        if let Some(arg) = &node.arg {
            self.mark(arg, Role::Returned);
        }
        node.visit_children_with(self);
    }
    fn visit_assign_expr(&mut self, node: &AssignExpr) {
        match &node.left {
            AssignTarget::Simple(SimpleAssignTarget::Ident(target)) => {
                self.marks.insert(key(target.id.span), Role::Assign);
            }
            AssignTarget::Simple(SimpleAssignTarget::Member(member)) => {
                self.mark(&member.obj, Role::MemberWrite);
            }
            _ => {}
        }
        node.visit_children_with(self);
    }
    fn visit_update_expr(&mut self, node: &UpdateExpr) {
        match unwrap(&node.arg) {
            Expr::Member(member) => self.mark(&member.obj, Role::MemberWrite),
            other => self.mark(other, Role::Assign),
        }
        node.visit_children_with(self);
    }

    fn visit_ts_type(&mut self, _: &TsType) {}
    fn visit_ts_interface_decl(&mut self, _: &TsInterfaceDecl) {}
    fn visit_ts_type_alias_decl(&mut self, _: &TsTypeAliasDecl) {}

    fn visit_named_export(&mut self, node: &NamedExport) {
        if !node.type_only {
            node.visit_children_with(self);
        }
    }

    fn visit_export_named_specifier(&mut self, node: &ExportNamedSpecifier) {
        if !node.is_type_only {
            node.visit_children_with(self);
        }
    }
}

/// `const` bindings whose initializer is an object literal (a candidate named
/// predicate, R3-1) or a number literal (a candidate `limit`).
#[derive(Default)]
struct Consts {
    predicates: HashMap<Id, ObjectLit>,
    numbers: HashMap<Id, f64>,
    /// The binding's own identifier, which isn't a use.
    declared_at: HashMap<Id, Span>,
}

impl Visit for Consts {
    fn visit_var_decl(&mut self, node: &VarDecl) {
        if node.kind == VarDeclKind::Const {
            for decl in &node.decls {
                let (Pat::Ident(binding), Some(init)) = (&decl.name, &decl.init) else {
                    continue;
                };
                let id = binding.id.to_id();
                match unwrap(init) {
                    Expr::Object(obj) => {
                        self.predicates.insert(id.clone(), obj.clone());
                        self.declared_at.insert(id, binding.id.span);
                    }
                    Expr::Lit(Lit::Num(number)) => {
                        self.numbers.insert(id, number.value);
                    }
                    _ => {}
                }
            }
        }
        node.visit_children_with(self);
    }
}

// ── The pass ─────────────────────────────────────────────────────────────────

/// The bindings a value's provenance is judged by (Design 4: by binding, not
/// spelling): `params` / `user` destructured from a function's first parameter
/// (`function Page({ params, user })`, `{ params: p }`), and a first parameter
/// read whole (`props`, for `props.params`). A local that happens to be named
/// `params` is none of these, and is code.
#[derive(Default)]
struct PropBindings {
    route: HashSet<Id>,
    principal: HashSet<Id>,
    whole: HashSet<Id>,
}

impl PropBindings {
    fn first_param(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(binding) => {
                self.whole.insert(binding.id.to_id());
            }
            Pat::Object(object) => {
                for prop in &object.props {
                    let (key, binding) = match prop {
                        swc_ecma_ast::ObjectPatProp::Assign(assign) => {
                            (assign.key.id.sym.to_string(), Some(assign.key.id.to_id()))
                        }
                        swc_ecma_ast::ObjectPatProp::KeyValue(kv) => (
                            prop_key(&kv.key).unwrap_or_default(),
                            match &*kv.value {
                                Pat::Ident(b) => Some(b.id.to_id()),
                                _ => None,
                            },
                        ),
                        swc_ecma_ast::ObjectPatProp::Rest(_) => continue,
                    };
                    match (key.as_str(), binding) {
                        ("params", Some(id)) => {
                            self.route.insert(id);
                        }
                        ("user", Some(id)) => {
                            self.principal.insert(id);
                        }
                        _ => {}
                    }
                }
            }
            Pat::Assign(assign) => self.first_param(&assign.left),
            _ => {}
        }
    }
}

impl PropBindings {
    /// The first parameter of every **top-level** entry function in the module:
    /// a declaration, a default export, or an arrow / function expression bound
    /// at the top level. Nested helpers are not entry points, so their
    /// parameters are ordinary code (review finding 2).
    fn from_module(module: &Module) -> Self {
        let mut out = Self::default();
        for item in &module.body {
            match item {
                ModuleItem::Stmt(Stmt::Decl(decl))
                | ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(swc_ecma_ast::ExportDecl {
                    decl,
                    ..
                })) => out.from_decl(decl),
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultDecl(default)) => {
                    if let swc_ecma_ast::DefaultDecl::Fn(function) = &default.decl {
                        if let Some(first) = function.function.params.first() {
                            out.first_param(&first.pat);
                        }
                    }
                }
                ModuleItem::ModuleDecl(ModuleDecl::ExportDefaultExpr(default)) => {
                    out.from_expr(&default.expr);
                }
                _ => {}
            }
        }
        out
    }

    fn from_decl(&mut self, decl: &Decl) {
        match decl {
            Decl::Fn(function) => {
                if let Some(first) = function.function.params.first() {
                    self.first_param(&first.pat);
                }
            }
            Decl::Var(var) => {
                for declarator in &var.decls {
                    if let Some(init) = &declarator.init {
                        self.from_expr(init);
                    }
                }
            }
            _ => {}
        }
    }

    fn from_expr(&mut self, expr: &Expr) {
        match unwrap(expr) {
            Expr::Arrow(arrow) => {
                if let Some(first) = arrow.params.first() {
                    self.first_param(first);
                }
            }
            Expr::Fn(function) => {
                if let Some(first) = function.function.params.first() {
                    self.first_param(&first.pat);
                }
            }
            _ => {}
        }
    }
}

impl Pass<'_> {
    fn visit_assignment_pat(&mut self, pat: &Pat) {
        match pat {
            Pat::Ident(ident) => self.visit_expr(&Expr::Ident(ident.id.clone())),
            Pat::Array(array) => {
                for element in array.elems.iter().flatten() {
                    self.visit_assignment_pat(element);
                }
            }
            Pat::Object(object) => {
                for prop in &object.props {
                    match prop {
                        swc_ecma_ast::ObjectPatProp::KeyValue(kv) => {
                            kv.key.visit_with(self);
                            self.visit_assignment_pat(&kv.value);
                        }
                        swc_ecma_ast::ObjectPatProp::Assign(assign) => {
                            self.visit_expr(&Expr::Ident(assign.key.id.clone()));
                            if let Some(default) = &assign.value {
                                default.visit_with(self);
                            }
                        }
                        swc_ecma_ast::ObjectPatProp::Rest(rest) => {
                            self.visit_assignment_pat(&rest.arg);
                        }
                    }
                }
            }
            Pat::Assign(assign) => {
                self.visit_assignment_pat(&assign.left);
                assign.right.visit_with(self);
            }
            Pat::Rest(rest) => self.visit_assignment_pat(&rest.arg),
            Pat::Expr(expr) => expr.visit_with(self),
            Pat::Invalid(_) => {}
        }
    }

    /// Where a value comes from, by its root binding: `params.id` (or
    /// `props.params.id`) is a route param, `user.id` the principal, anything
    /// else code.
    fn provenance(&self, value: &Expr) -> Provenance {
        let mut cursor = unwrap(value);
        let mut first_prop: Option<String> = None;
        loop {
            match cursor {
                Expr::Member(member) => {
                    first_prop = match &member.prop {
                        MemberProp::Ident(name) => Some(name.sym.to_string()),
                        _ => None,
                    };
                    cursor = unwrap(&member.obj);
                }
                Expr::Ident(root) => {
                    let id = root.to_id();
                    return if self.props.route.contains(&id) {
                        Provenance::RouteParam
                    } else if self.props.principal.contains(&id) {
                        Provenance::Principal
                    } else if self.props.whole.contains(&id) {
                        match first_prop.as_deref() {
                            Some("params") => Provenance::RouteParam,
                            Some("user") => Provenance::Principal,
                            _ => Provenance::Code,
                        }
                    } else {
                        Provenance::Code
                    };
                }
                _ => return Provenance::Code,
            }
        }
    }
}

struct Pass<'a> {
    module: &'a str,
    source: &'a str,
    file_start: u32,
    generated: bool,
    source_map: Lrc<SourceMap>,
    schemas: &'a BTreeMap<String, CollectionSchema>,
    census: &'a HashMap<Id, Vec<Ref>>,
    consts: &'a Consts,
    /// Named predicates imported one hop from other app modules.
    foreign: &'a HashMap<Id, ForeignPred>,
    /// The `params` / `user` bindings a component receives (Design 4).
    props: &'a PropBindings,
    /// Value bindings of `albedo/forge` imports → the collection's name.
    collections: HashMap<Id, String>,
    /// Value bindings of `albedo/query` imports → the helper's name.
    helpers: HashMap<Id, String>,
    /// Bindings of `useSharedSlot` imported from `albedo`.
    use_shared_slot: HashSet<Id>,
    /// Expressions a handle owns (its chain, its folds, its run): classified
    /// with the handle, skipped by the general walk.
    claimed: HashSet<SpanKey>,
    /// References to a named predicate that are `.where()` / `or()` arguments.
    accounted: HashSet<SpanKey>,
    /// Named predicates some query read, and where first.
    preds_used: HashMap<Id, Span>,
    /// Lowered queries, held until every named predicate they read is checked.
    pending: Vec<Pending>,
    out: Classification,
}

struct Pending {
    query: LoweredQuery,
    edits: Vec<Edit>,
    preds: Vec<Id>,
}

impl Pass<'_> {
    /// A named predicate's own binding identifier (a local `const`, or the
    /// import of a foreign one), which isn't a use.
    fn declared_at(&self, id: &Id) -> Option<Span> {
        self.consts
            .declared_at
            .get(id)
            .copied()
            .or_else(|| self.foreign.get(id).map(|f| f.local))
    }
}

/// One `.method(args)` step of a chain.
#[derive(Clone)]
struct Step<'e> {
    name: String,
    args: &'e [ExprOrSpread],
    span: Span,
}

/// A refusal carrying the span it is about.
struct Refuse {
    span: Span,
    message: String,
}

fn refuse(span: Span, message: impl Into<String>) -> Refuse {
    Refuse {
        span,
        message: message.into(),
    }
}

/// Strip parentheses and TypeScript-only wrappers, which change nothing at run
/// time (the pass runs before type stripping).
fn unwrap(mut expr: &Expr) -> &Expr {
    loop {
        expr = match expr {
            Expr::Paren(inner) => &inner.expr,
            Expr::TsAs(inner) => &inner.expr,
            Expr::TsSatisfies(inner) => &inner.expr,
            Expr::TsNonNull(inner) => &inner.expr,
            Expr::TsConstAssertion(inner) => &inner.expr,
            Expr::TsTypeAssertion(inner) => &inner.expr,
            Expr::TsInstantiation(inner) => &inner.expr,
            _ => return expr,
        };
    }
}

fn prop_key(key: &PropName) -> Option<String> {
    match key {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(text) => Some(text.value.to_string()),
        _ => None,
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match unwrap(expr) {
        Expr::Lit(Lit::Str(text)) => Some(text.value.to_string()),
        Expr::Tpl(tpl) if tpl.exprs.is_empty() && tpl.quasis.len() == 1 => tpl.quasis[0]
            .cooked
            .as_ref()
            .map(|cooked| cooked.to_string()),
        _ => None,
    }
}

/// `root.m1(a).m2(b)…` with `root_ok(root)` → the root and its steps in source
/// order. `None` for anything else (a computed member, a method referenced
/// without a call, a bare identifier).
fn chain_by<'e>(
    expr: &'e Expr,
    root_ok: &dyn Fn(&Ident) -> bool,
) -> Option<(&'e Ident, Vec<Step<'e>>)> {
    let mut steps = Vec::new();
    let mut cursor = unwrap(expr);
    loop {
        match cursor {
            Expr::Call(call) => {
                let Callee::Expr(callee) = &call.callee else {
                    return None;
                };
                let Expr::Member(member) = unwrap(callee) else {
                    return None;
                };
                let MemberProp::Ident(name) = &member.prop else {
                    return None;
                };
                steps.push(Step {
                    name: name.sym.to_string(),
                    args: &call.args,
                    span: call.span,
                });
                cursor = unwrap(&member.obj);
            }
            Expr::Ident(root) if !steps.is_empty() && root_ok(root) => {
                steps.reverse();
                return Some((root, steps));
            }
            _ => return None,
        }
    }
}

/// A statement in a block, or an item at the top of a module.
#[derive(Clone, Copy)]
enum Node<'s> {
    Stmt(&'s Stmt),
    Item(&'s ModuleItem),
}

impl<'s> Node<'s> {
    fn stmt(self) -> Option<&'s Stmt> {
        match self {
            Node::Stmt(stmt) | Node::Item(ModuleItem::Stmt(stmt)) => Some(stmt),
            Node::Item(_) => None,
        }
    }

    fn var_decl(self) -> Option<&'s VarDecl> {
        match self {
            Node::Stmt(Stmt::Decl(Decl::Var(var)))
            | Node::Item(ModuleItem::Stmt(Stmt::Decl(Decl::Var(var)))) => Some(var),
            Node::Item(ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export))) => match &export.decl {
                Decl::Var(var) => Some(var),
                _ => None,
            },
            _ => None,
        }
    }

    fn span(self) -> Span {
        match self {
            Node::Stmt(stmt) => stmt.span(),
            Node::Item(item) => item.span(),
        }
    }
}

/// `q = <rhs>` as the whole consequent of an `if` (bare, or the only statement
/// of a block) → the target identifier and the right-hand side.
fn fold_assignment<'s>(cons: &'s Stmt, handle: &Id) -> Option<(&'s Ident, &'s Expr)> {
    let stmt = match cons {
        Stmt::Block(block) if block.stmts.len() == 1 => &block.stmts[0],
        Stmt::Block(_) => return None,
        other => other,
    };
    let Stmt::Expr(expr) = stmt else {
        return None;
    };
    let Expr::Assign(assign) = unwrap(&expr.expr) else {
        return None;
    };
    if assign.op != AssignOp::Assign {
        return None;
    }
    let AssignTarget::Simple(SimpleAssignTarget::Ident(target)) = &assign.left else {
        return None;
    };
    (target.id.to_id() == *handle).then_some((&target.id, &*assign.right))
}

/// The one use of a handle, found by the reference's span: a chain rooted at it
/// (`q.orderBy(…).limit(n)`), or the handle awaited bare (`await q`). Cloned, so
/// it can be classified after the walk that found it.
struct FinalUse {
    span: Span,
    chain: Option<Expr>,
}

struct FindFinal {
    reference: Span,
    found: Option<FinalUse>,
}

impl Visit for FindFinal {
    fn visit_expr(&mut self, expr: &Expr) {
        if self.found.is_some() {
            return;
        }
        let bare = unwrap(expr);
        if let Expr::Await(awaited) = bare {
            if let Expr::Ident(ident) = unwrap(&awaited.arg) {
                if ident.span == self.reference {
                    self.found = Some(FinalUse {
                        span: ident.span,
                        chain: None,
                    });
                    return;
                }
            }
        }
        let reference = self.reference;
        if chain_by(bare, &|root| root.span == reference).is_some() {
            self.found = Some(FinalUse {
                span: bare.span(),
                chain: Some(bare.clone()),
            });
            return;
        }
        expr.visit_children_with(self);
    }
}

/// Which part of a query a run of steps is, for emitting it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SegKind {
    /// A whole chain written in one expression.
    Inline,
    /// `let q = <chain>`.
    Base,
    /// `if (…) q = q.where(…)`.
    Fold,
    /// The handle's one use.
    Final,
}

struct Segment<'e> {
    kind: SegKind,
    steps: Vec<Step<'e>>,
    /// The expression this segment's emitted call replaces.
    span: Span,
}

impl Pass<'_> {
    fn position(&self, span: Span) -> Position {
        let loc = self.source_map.lookup_char_pos(span.lo);
        Position {
            module: self.module.to_string(),
            line: loc.line,
            column: loc.col.0 + 1,
            generated: self.generated,
        }
    }

    fn offset(&self, pos: swc_common::BytePos) -> usize {
        (pos.0 - self.file_start) as usize
    }

    fn text(&self, span: Span) -> &str {
        &self.source[self.offset(span.lo)..self.offset(span.hi)]
    }

    fn diagnose(&mut self, span: Span, message: impl Into<String>) {
        let at = self.position(span);
        self.out.diagnostics.push(Diagnostic {
            at,
            message: message.into(),
        });
    }

    fn collect_imports(&mut self, module: &Module) {
        for item in &module.body {
            let ModuleItem::ModuleDecl(decl) = item else {
                continue;
            };
            match decl {
                ModuleDecl::Import(import) => {
                    let source = import.src.value.as_ref();
                    if import.type_only
                        || !matches!(source, FORGE_MODULE | QUERY_MODULE | ALBEDO_MODULE)
                    {
                        continue;
                    }
                    for spec in &import.specifiers {
                        match spec {
                            ImportSpecifier::Named(named) => {
                                if named.is_type_only {
                                    continue;
                                }
                                let exported = match &named.imported {
                                    Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                                    Some(ModuleExportName::Str(s)) => s.value.to_string(),
                                    None => named.local.sym.to_string(),
                                };
                                let id = named.local.to_id();
                                match source {
                                    FORGE_MODULE => {
                                        self.collections.insert(id, exported);
                                    }
                                    QUERY_MODULE => {
                                        if HELPERS.contains(&exported.as_str()) {
                                            self.helpers.insert(id, exported);
                                        } else {
                                            self.diagnose(
                                                named.span,
                                                format!(
                                                    "`albedo/query` has no `{exported}`; it exports {}",
                                                    HELPERS.join(", ")
                                                ),
                                            );
                                        }
                                    }
                                    _ => {
                                        if exported == "useSharedSlot" {
                                            self.use_shared_slot.insert(id);
                                        }
                                    }
                                }
                            }
                            ImportSpecifier::Namespace(ns) if source != ALBEDO_MODULE => {
                                self.diagnose(
                                    ns.span,
                                    format!(
                                        "`import * as {} from \"{source}\"` hides which names are \
                                         read; import each one by name",
                                        ns.local.sym
                                    ),
                                );
                            }
                            ImportSpecifier::Default(default) if source != ALBEDO_MODULE => {
                                self.diagnose(
                                    default.span,
                                    format!(
                                        "\"{source}\" has no default export; import names in braces"
                                    ),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                ModuleDecl::ExportNamed(export) => {
                    if let Some(src) = &export.src {
                        let source = src.value.as_ref();
                        let has_values = export.specifiers.iter().any(|spec| {
                            !matches!(spec, swc_ecma_ast::ExportSpecifier::Named(named) if named.is_type_only)
                        });
                        if !export.type_only && has_values && matches!(source, FORGE_MODULE | QUERY_MODULE) {
                            self.diagnose(
                                export.span,
                                format!(
                                    "re-exporting from \"{source}\" is refused: import it where \
                                     it's read"
                                ),
                            );
                        }
                    }
                }
                ModuleDecl::ExportAll(export) => {
                    let source = export.src.value.as_ref();
                    if !export.type_only && matches!(source, FORGE_MODULE | QUERY_MODULE) {
                        self.diagnose(
                            export.span,
                            format!(
                                "re-exporting from \"{source}\" is refused: import it where it's \
                                 read"
                            ),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn collection_of(&self, ident: &Ident) -> Option<&String> {
        self.collections.get(&ident.to_id())
    }

    fn helper_of(&self, expr: &Expr) -> Option<&str> {
        match unwrap(expr) {
            Expr::Ident(ident) => self.helpers.get(&ident.to_id()).map(String::as_str),
            _ => None,
        }
    }

    fn helper_call<'e>(&self, expr: &'e Expr, name: &str) -> Option<&'e CallExpr> {
        let Expr::Call(call) = unwrap(expr) else {
            return None;
        };
        let Callee::Expr(callee) = &call.callee else {
            return None;
        };
        (self.helper_of(callee) == Some(name)).then_some(call)
    }

    /// A chain rooted at a collection binding.
    fn chain<'e>(&self, expr: &'e Expr) -> Option<(&'e Ident, Vec<Step<'e>>)> {
        chain_by(expr, &|root| self.collection_of(root).is_some())
    }

    fn claim(&mut self, span: Span) {
        self.claimed.insert(key(span));
    }

    /// Visit the value expressions inside a chain's arguments (so a collection
    /// or helper misused *inside* an argument is still caught), without
    /// revisiting the chain itself.
    fn visit_values(&mut self, values: &[Value<'_>]) {
        for value in values {
            match value {
                Value::Expr(expr) => expr.visit_with(self),
                Value::Ident(ident) => self.visit_expr(&Expr::Ident((*ident).clone())),
            }
        }
    }

    /// Every `let/const/var q = <chain>` among `nodes`, with the statements that
    /// follow it (Design 2).
    fn scan_block(&mut self, nodes: &[Node<'_>]) {
        for (index, node) in nodes.iter().enumerate() {
            let Some(var) = node.var_decl() else {
                continue;
            };
            for decl in &var.decls {
                let (Pat::Ident(binding), Some(init)) = (&decl.name, &decl.init) else {
                    continue;
                };
                if let Some((root, steps)) = self.chain(init) {
                    self.classify_handle(&binding.id, root, steps, init, &nodes[index + 1..]);
                }
            }
        }
    }

    /// A handle: its chain, the folds after it, and its one use — or, when any
    /// reference to it breaks the shape, one diagnostic naming that reference.
    fn classify_handle(
        &mut self,
        handle: &Ident,
        root: &Ident,
        base: Vec<Step<'_>>,
        init: &Expr,
        rest: &[Node<'_>],
    ) {
        let id = handle.to_id();
        let name = handle.sym.to_string();
        let base_span = unwrap(init).span();
        self.claim(base_span);

        let all = self.census.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        let (fn_depth, loop_depth) = all
            .iter()
            .find(|r| r.span == handle.span)
            .map_or((0, 0), |r| (r.fn_depth, r.loop_depth));
        let refs: Vec<Ref> = all.iter().filter(|r| r.span != handle.span).copied().collect();

        let mut accounted: HashSet<SpanKey> = HashSet::new();
        let mut folds: Vec<(Span, Vec<Step<'_>>)> = Vec::new();
        let mut final_use: Option<FinalUse> = None;
        let mut refused: Option<Refuse> = None;

        for node in rest {
            let node_span = node.span();
            let inside: Vec<Ref> = refs
                .iter()
                .filter(|r| node_span.contains(r.span))
                .copied()
                .collect();
            if inside.is_empty() || final_use.is_some() {
                continue;
            }
            if let Some(Stmt::If(if_stmt)) = node.stmt() {
                if let Some((target, rhs)) = fold_assignment(&if_stmt.cons, &id) {
                    if let Some((fold_root, steps)) = chain_by(rhs, &|r| r.to_id() == id) {
                        if let Some(alt) = &if_stmt.alt {
                            refused = Some(refuse(
                                alt.span(),
                                format!(
                                    "`{name}` is narrowed in an `else`: a fold is `if (…) {name} = \
                                     {name}.where({{ column: value }})` with no else — write two \
                                     queries, or use optional()"
                                ),
                            ));
                            break;
                        }
                        let one_column = steps.len() == 1
                            && steps[0].name == "where"
                            && steps[0].args.len() == 1
                            && matches!(unwrap(&steps[0].args[0].expr), Expr::Object(obj) if obj.props.len() == 1);
                        if !one_column {
                            refused = Some(refuse(
                                unwrap(rhs).span(),
                                format!(
                                    "a fold narrows one column per `if`: `if (…) {name} = \
                                     {name}.where({{ column: value }})`"
                                ),
                            ));
                            break;
                        }
                        accounted.insert(key(target.span));
                        accounted.insert(key(fold_root.span));
                        folds.push((unwrap(rhs).span(), steps));
                        continue;
                    }
                }
            }
            if let [only] = inside.as_slice() {
                if only.role == Role::Read
                    && only.fn_depth == fn_depth
                    && only.loop_depth == loop_depth
                {
                    let mut find = FindFinal {
                        reference: only.span,
                        found: None,
                    };
                    match node {
                        Node::Stmt(stmt) => stmt.visit_with(&mut find),
                        Node::Item(item) => item.visit_with(&mut find),
                    }
                    if let Some(found) = find.found {
                        accounted.insert(key(only.span));
                        final_use = Some(found);
                    }
                }
            }
        }

        if refused.is_none() {
            let final_lo = final_use.as_ref().map(|f| f.span.lo);
            if let Some(r) = refs
                .iter()
                .filter(|r| !accounted.contains(&key(r.span)))
                .min_by_key(|r| r.span.lo)
            {
                let reason = if r.fn_depth > fn_depth {
                    "is captured by a closure"
                } else if r.loop_depth > loop_depth {
                    "is used inside a loop"
                } else if r.role == Role::Assign {
                    "is reassigned here"
                } else if r.role == Role::CallArg {
                    "is passed to a function"
                } else if r.role == Role::Returned {
                    "is returned"
                } else if final_lo.is_some_and(|lo| r.span.lo > lo) {
                    "is used a second time"
                } else {
                    "is used outside the accepted shape"
                };
                refused = Some(refuse(
                    r.span,
                    format!(
                        "`{name}` holds a query and {reason}. A query handle is `let {name} = \
                         <chain>`, narrowed only by `if (…) {name} = {name}.where({{ column: value \
                         }})` in the same block, then run once (`await {name}…`); otherwise write \
                         the chain inline, use optional(), or wait for raw SQL (Q1b)"
                    ),
                ));
            }
        }

        for (span, _) in &folds {
            self.claim(*span);
        }
        if let Some(found) = &final_use {
            self.claim(found.span);
        }
        if let Some(refused) = refused {
            self.diagnose(refused.span, refused.message);
            return;
        }

        let mut segments = vec![Segment {
            kind: SegKind::Base,
            steps: base,
            span: base_span,
        }];
        for (span, steps) in folds {
            segments.push(Segment {
                kind: SegKind::Fold,
                steps,
                span,
            });
        }
        let final_chain = final_use.as_ref().and_then(|f| f.chain.clone());
        if let Some(found) = &final_use {
            let steps = final_chain
                .as_ref()
                .and_then(|chain| chain_by(chain, &|r| r.to_id() == id))
                .map(|(_, steps)| steps)
                .unwrap_or_default();
            segments.push(Segment {
                kind: SegKind::Final,
                steps,
                span: found.span,
            });
        }
        self.classify_segments(root, &segments, Mode::Request, Some(&name));
    }

    fn classify_segments(
        &mut self,
        root: &Ident,
        segments: &[Segment<'_>],
        mode: Mode,
        handle: Option<&str>,
    ) {
        let collection = self.collection_of(root).cloned().unwrap_or_default();
        let at = self.position(root.span);
        let Some(schema) = self.schemas.get(&collection).cloned() else {
            let declared = self.schemas.keys().cloned().collect::<Vec<_>>().join(", ");
            self.diagnose(
                root.span,
                format!(
                    "`{collection}` isn't a declared collection (declared: {})",
                    if declared.is_empty() { "none" } else { &declared }
                ),
            );
            return;
        };
        let mut build = Build {
            pass: self,
            descriptor: QueryDescriptor {
                root: collection.clone(),
                terms: Vec::new(),
                or_groups: Vec::new(),
                order: Vec::new(),
                bound: RowBound::None,
                shape: Shape::Rows,
                includes: Vec::new(),
                relation_terms: Vec::new(),
                mode,
            },
            next_param: 0,
            parameter_overflow: None,
            next_optional: 0,
            values: Vec::new(),
            sources: Vec::new(),
            path: None,
            fold_optional: None,
            phase: Phase::Where,
            started: false,
            got_key: false,
            preds: Vec::new(),
            provenance: Vec::new(),
        };
        let mut infos: Vec<(std::ops::Range<u16>, Option<u8>)> = Vec::new();
        let mut built = Ok(());
        for segment in segments {
            let start = build.next_param;
            let optional = (segment.kind == SegKind::Fold).then(|| {
                let slot = build.next_optional;
                build.next_optional = build.next_optional.saturating_add(1);
                slot
            });
            build.fold_optional = optional;
            built = build.steps(&collection, &segment.steps);
            if let Some(span) = build.parameter_overflow {
                built = Err(refuse(
                    span,
                    "this query has too many parameters; the limit is 65535",
                ));
            }
            build.fold_optional = None;
            infos.push((start..build.next_param, optional));
            if built.is_err() {
                break;
            }
        }
        let values = std::mem::take(&mut build.values);
        let sources = std::mem::take(&mut build.sources);
        let preds = std::mem::take(&mut build.preds);
        let authored_provenance = std::mem::take(&mut build.provenance);
        let descriptor = build.descriptor;
        for (pred, span) in &preds {
            self.accounted.insert(key(*span));
            self.preds_used.entry(pred.clone()).or_insert(*span);
        }
        self.visit_values(&values);
        if let Err(refused) = built {
            self.diagnose(refused.span, refused.message);
            return;
        }
        if mode == Mode::Live {
            self.diagnose(
                root.span,
                format!(
                    "a live query (`useSharedSlot` over a `{collection}` chain) arrives with Q3; \
                     today useSharedSlot takes `{collection}` or `{collection}.where({{ <partition \
                     column>: params.x | user.id }})` with nothing chained"
                ),
            );
            return;
        }
        let canonical = match descriptor.finish(&schema) {
            Ok(canonical) => canonical,
            Err(error) => {
                self.diagnose(root.span, error.to_string());
                return;
            }
        };
        let edits = segments
            .iter()
            .zip(&infos)
            .map(|(segment, (params, optional))| {
                self.emit(segment, params.clone(), *optional, &sources, &canonical, handle)
            })
            .collect();
        let lowered = lower(&canonical.descriptor, &schema);
        let mut provenance = vec![Provenance::Code; authored_provenance.len()];
        for (authored, from) in authored_provenance.iter().enumerate() {
            if let Some(canonical) = canonical.slot_map.get(authored) {
                if let Some(slot) = provenance.get_mut(*canonical as usize) {
                    *slot = *from;
                }
            }
        }
        let imported = preds
            .iter()
            .filter_map(|(id, _)| self.foreign.get(id))
            .map(|f| (f.loaded.home.clone(), f.loaded.export.clone()))
            .collect();
        self.pending.push(Pending {
            query: LoweredQuery {
                at,
                collection,
                canonical,
                lowered,
                imported,
                provenance,
            },
            edits,
            preds: preds.into_iter().map(|(id, _)| id).collect(),
        });
    }

    /// The call one segment becomes (see [`rewrite_source`]).
    fn emit(
        &self,
        segment: &Segment<'_>,
        params: std::ops::Range<u16>,
        optional: Option<u8>,
        sources: &[Source],
        canonical: &Canonical,
        handle: Option<&str>,
    ) -> Edit {
        let slots = params
            .clone()
            .map(|p| canonical.slot_map[p as usize].to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let mut values = Vec::new();
        for (i, p) in params.enumerate() {
            if i > 0 {
                values.push(Piece::Text(", ".into()));
            }
            values.push(match &sources[p as usize] {
                Source::Span(span) => Piece::Hole(self.offset(span.lo), self.offset(span.hi)),
                Source::Path { base, keys } => {
                    let mut text = self.text(*base).to_string();
                    for k in keys {
                        if is_identifier(k) {
                            text.push('.');
                            text.push_str(k);
                        } else {
                            text.push_str(&format!("[{}]", serde_json::Value::String(k.clone())));
                        }
                    }
                    Piece::Text(text)
                }
            });
        }
        let id = serde_json::Value::String(canonical.id.clone()).to_string();
        let handle = handle.unwrap_or_default();
        let (open, close) = match segment.kind {
            SegKind::Inline => (
                format!("__albedo_q_run(__albedo_q_new({id}, [{slots}], ["),
                "]), [], [])".to_string(),
            ),
            SegKind::Base => (format!("__albedo_q_new({id}, [{slots}], ["), "])".to_string()),
            SegKind::Fold => {
                let slot = optional.map_or(0, |o| canonical.optional_map[o as usize]);
                (
                    format!("__albedo_q_set({handle}, {slot}, [{slots}], ["),
                    "])".to_string(),
                )
            }
            SegKind::Final => (format!("__albedo_q_run({handle}, [{slots}], ["), "])".to_string()),
        };
        let mut pieces = vec![Piece::Text(open)];
        pieces.extend(values);
        pieces.push(Piece::Text(close));
        Edit {
            lo: self.offset(segment.span.lo),
            hi: self.offset(segment.span.hi),
            pieces,
        }
    }

    /// Design 9's legacy line, exactly: `<collection>.where({ <partition column>:
    /// params.x | user.id })` with nothing chained. The column must be the
    /// collection's declared `partition_by`, the value a route param or the
    /// principal by binding, and the object one plain `key: value` pair — an
    /// operator object, a second column, a spread or an unpartitioned collection
    /// is a new-style live read (review finding 1).
    fn is_exact_partition_read(&self, root: &Ident, steps: &[Step<'_>]) -> bool {
        let [step] = steps else {
            return false;
        };
        if step.name != "where" {
            return false;
        }
        let [arg] = step.args else {
            return false;
        };
        let Some(collection) = self.collection_of(root) else {
            return false;
        };
        let Some(partition) = self
            .schemas
            .get(collection)
            .and_then(|schema| schema.partition_by.as_deref())
        else {
            return false;
        };
        let Expr::Object(obj) = unwrap(&arg.expr) else {
            return false;
        };
        let [PropOrSpread::Prop(prop)] = obj.props.as_slice() else {
            return false;
        };
        let (column, from) = match &**prop {
            Prop::KeyValue(kv) => (prop_key(&kv.key), self.provenance(&kv.value)),
            Prop::Shorthand(ident) => (
                Some(ident.sym.to_string()),
                self.provenance(&Expr::Ident(ident.clone())),
            ),
            _ => return false,
        };
        column.as_deref() == Some(partition)
            && matches!(from, Provenance::RouteParam | Provenance::Principal)
    }

    /// `useSharedSlot(arg)`: the legacy forms stay legacy (Design 9); any other
    /// chain over a collection is a new-style live query.
    fn classify_use_shared_slot(&mut self, call: &CallExpr) -> bool {
        let Callee::Expr(callee) = &call.callee else {
            return false;
        };
        let Expr::Ident(name) = unwrap(callee) else {
            return false;
        };
        if !self.use_shared_slot.contains(&name.to_id()) {
            return false;
        }
        let Some(first) = call.args.first() else {
            return false;
        };
        let arg = unwrap(&first.expr);
        if first.spread.is_some()
            && (matches!(arg, Expr::Ident(ident) if self.collection_of(ident).is_some())
                || self.chain(arg).is_some())
        {
            self.diagnose(first.expr.span(), "useSharedSlot takes one source, not a spread collection");
            for arg in &call.args {
                arg.expr.visit_with(self);
            }
            return true;
        }
        let rest: Vec<Value<'_>> = call.args.iter().skip(1).map(|a| Value::Expr(&a.expr)).collect();
        if let Expr::Ident(ident) = arg {
            if self.collection_of(ident).is_some() {
                self.out.legacy += 1;
                self.visit_values(&rest);
                return true;
            }
        }
        let Some((root, steps)) = self.chain(arg) else {
            return false;
        };
        let legacy = self.is_exact_partition_read(root, &steps);
        if legacy {
            // The legacy extractor validates the partition column and the key's
            // derivation, with its own messages; only its argument values are
            // walked here.
            self.out.legacy += 1;
            if let Expr::Object(obj) = unwrap(&steps[0].args[0].expr) {
                obj.visit_with(self);
            }
        } else {
            let span = arg.span();
            self.classify_segments(
                root,
                &[Segment {
                    kind: SegKind::Inline,
                    steps,
                    span,
                }],
                Mode::Live,
                None,
            );
        }
        self.visit_values(&rest);
        true
    }

    /// After the walk: a named predicate any query read must have no other use
    /// anywhere in the module (R3-1); queries reading a refused one are dropped.
    fn settle(mut self) -> PassOutput {
        let mut used: Vec<(Id, Span)> = self.preds_used.drain().collect();
        used.sort_by_key(|(_, span)| span.lo);
        let mut refused: HashSet<Id> = HashSet::new();
        for (id, first) in used {
            let declared = self.declared_at(&id);
            let Some(stray) = self
                .census
                .get(&id)
                .into_iter()
                .flatten()
                .filter(|r| Some(r.span) != declared && !self.accounted.contains(&key(r.span)))
                .min_by_key(|r| r.span.lo)
                .copied()
            else {
                continue;
            };
            let reason = stray_reason(stray.role);
            let line = self.position(first).line;
            self.diagnose(
                stray.span,
                format!(
                    "`{}` is a predicate a query reads when it runs (line {line}), so it may only be \
                     passed to `.where()` or `or()`: here it is {reason}",
                    id.0
                ),
            );
            refused.insert(id);
        }
        let mut edits = Vec::new();
        for pending in std::mem::take(&mut self.pending) {
            if pending.preds.iter().any(|p| refused.contains(p)) {
                continue;
            }
            self.out.queries.push(pending.query);
            edits.extend(pending.edits);
        }
        PassOutput {
            classification: self.out,
            edits,
            accounted: self.accounted,
        }
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

impl Visit for Pass<'_> {
    fn visit_assign_target(&mut self, target: &AssignTarget) {
        match target {
            AssignTarget::Simple(SimpleAssignTarget::Ident(ident)) => {
                self.visit_expr(&Expr::Ident(ident.id.clone()));
            }
            AssignTarget::Pat(swc_ecma_ast::AssignTargetPat::Array(array)) => {
                // Assignment patterns use binding identifiers in the AST, but
                // these are value writes to existing bindings, not declarations.
                self.visit_assignment_pat(&Pat::Array(array.clone()));
            }
            AssignTarget::Pat(swc_ecma_ast::AssignTargetPat::Object(object)) => {
                self.visit_assignment_pat(&Pat::Object(object.clone()));
            }
            _ => target.visit_children_with(self),
        }
    }

    fn visit_for_head(&mut self, head: &swc_ecma_ast::ForHead) {
        if let swc_ecma_ast::ForHead::Pat(pat) = head {
            self.visit_assignment_pat(pat);
        } else {
            head.visit_children_with(self);
        }
    }

    fn visit_jsx_opening_element(&mut self, node: &JSXOpeningElement) {
        let root = match &node.name {
            JSXElementName::Ident(ident)
                if !ident.sym.starts_with(|c: char| c.is_ascii_lowercase()) => Some(ident),
            JSXElementName::JSXMemberExpr(member) => {
                let mut object = &member.obj;
                loop {
                    match object {
                        JSXObject::Ident(ident) => break Some(ident),
                        JSXObject::JSXMemberExpr(member) => object = &member.obj,
                    }
                }
            }
            _ => None,
        };
        if let Some(root) = root {
            self.visit_expr(&Expr::Ident(root.clone()));
        }
        node.visit_children_with(self);
    }

    fn visit_stmts(&mut self, stmts: &[Stmt]) {
        let nodes: Vec<Node<'_>> = stmts.iter().map(Node::Stmt).collect();
        self.scan_block(&nodes);
        for stmt in stmts {
            stmt.visit_with(self);
        }
    }

    fn visit_module_items(&mut self, items: &[ModuleItem]) {
        let nodes: Vec<Node<'_>> = items.iter().map(Node::Item).collect();
        self.scan_block(&nodes);
        for item in items {
            item.visit_with(self);
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if self.claimed.contains(&key(unwrap(expr).span())) {
            return;
        }
        if let Expr::Call(call) = unwrap(expr) {
            if self.classify_use_shared_slot(call) {
                return;
            }
        }
        if let Some((root, steps)) = self.chain(expr) {
            let span = unwrap(expr).span();
            self.classify_segments(
                root,
                &[Segment {
                    kind: SegKind::Inline,
                    steps,
                    span,
                }],
                Mode::Request,
                None,
            );
            return;
        }
        if let Expr::Ident(ident) = unwrap(expr) {
            if let Some(collection) = self.collection_of(ident).cloned() {
                self.diagnose(
                    ident.span,
                    format!(
                        "`{collection}` is a collection: it has no value of its own. Read it \
                         through a query chain (`{collection}.where({{ … }})`, \
                         `{collection}.get(id)`) or useSharedSlot({collection})"
                    ),
                );
                return;
            }
            if let Some(helper) = self.helpers.get(&ident.to_id()).cloned() {
                self.diagnose(
                    ident.span,
                    format!(
                        "`{helper}` from \"albedo/query\" only means something inside a query \
                         chain's arguments"
                    ),
                );
                return;
            }
        }
        expr.visit_children_with(self);
    }

    fn visit_prop(&mut self, prop: &Prop) {
        if let Prop::Shorthand(ident) = prop {
            if self.collection_of(ident).is_some() || self.helpers.contains_key(&ident.to_id()) {
                self.visit_expr(&Expr::Ident(ident.clone()));
                return;
            }
        }
        prop.visit_children_with(self);
    }

    fn visit_export_named_specifier(&mut self, spec: &ExportNamedSpecifier) {
        if spec.is_type_only {
            return;
        }
        if let ModuleExportName::Ident(orig) = &spec.orig {
            if self.collection_of(orig).is_some() || self.helpers.contains_key(&orig.to_id()) {
                self.diagnose(
                    orig.span,
                    format!(
                        "re-exporting `{}` is refused: import it from its framework module \
                         where it's read",
                        orig.sym
                    ),
                );
            }
        }
    }

    fn visit_named_export(&mut self, node: &NamedExport) {
        if !node.type_only {
            node.visit_children_with(self);
        }
    }
}

// ── Building one descriptor ──────────────────────────────────────────────────

/// An argument value to walk for nested misuse.
enum Value<'e> {
    Expr(&'e Expr),
    /// A shorthand property's identifier (`{ owner }`).
    Ident(&'e Ident),
}

/// Where a parameter's value comes from in the emitted JS.
enum Source {
    /// The expression the author wrote, evaluated where they wrote it.
    Span(Span),
    /// A named predicate's value, read from the object at the query site
    /// (`open.owner`, `open.due.lt` — R3-1).
    Path { base: Span, keys: Vec<String> },
}

/// Builds one descriptor from a chain's segments, numbering parameters and
/// optional slots in source order. Value expressions are collected, not
/// judged: they are walked by the pass afterwards for nested misuse.
struct Build<'p, 'a, 'e> {
    pass: &'p Pass<'a>,
    descriptor: QueryDescriptor,
    next_param: u16,
    parameter_overflow: Option<Span>,
    next_optional: u8,
    values: Vec<Value<'e>>,
    sources: Vec<Source>,
    /// While reading a named predicate: its identifier at the query site and the
    /// key path to the value being read.
    path: Option<(Span, Vec<String>)>,
    /// While reading a fold's `where`: the optional slot its term takes.
    fold_optional: Option<u8>,
    phase: Phase,
    started: bool,
    got_key: bool,
    /// Named predicates read, with the reference that read them.
    preds: Vec<(Id, Span)>,
    /// Provenance per authored parameter.
    provenance: Vec<Provenance>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
enum Phase {
    Where,
    Order,
    Done,
}

impl<'a: 'e, 'e> Build<'_, 'a, 'e> {
    fn param(&mut self, value: &'e Expr) -> u16 {
        let param = self.next_param;
        if param == u16::MAX {
            self.parameter_overflow.get_or_insert(value.span());
            return param;
        }
        self.next_param += 1;
        self.provenance.push(self.pass.provenance(value));
        match &self.path {
            Some((base, keys)) => self.sources.push(Source::Path {
                base: *base,
                keys: keys.clone(),
            }),
            None => {
                self.values.push(Value::Expr(value));
                self.sources.push(Source::Span(value.span()));
            }
        }
        param
    }

    fn param_ident(&mut self, ident: &'e Ident) -> u16 {
        let param = self.next_param;
        if param == u16::MAX {
            self.parameter_overflow.get_or_insert(ident.span);
            return param;
        }
        self.next_param += 1;
        self.provenance
            .push(self.pass.provenance(&Expr::Ident(ident.clone())));
        match &self.path {
            Some((base, keys)) => self.sources.push(Source::Path {
                base: *base,
                keys: keys.clone(),
            }),
            None => {
                self.values.push(Value::Ident(ident));
                self.sources.push(Source::Span(ident.span));
            }
        }
        param
    }

    /// Run `read` with `key` appended to the named-predicate path, if reading one.
    fn keyed<T>(&mut self, key: &str, read: impl FnOnce(&mut Self) -> T) -> T {
        if let Some((_, keys)) = self.path.as_mut() {
            keys.push(key.to_string());
        }
        let out = read(self);
        if let Some((_, keys)) = self.path.as_mut() {
            keys.pop();
        }
        out
    }

    fn steps(&mut self, collection: &str, steps: &[Step<'e>]) -> Result<(), Refuse> {
        for step in steps {
            if let Some(arg) = step.args.iter().find(|arg| arg.spread.is_some()) {
                return Err(refuse(arg.expr.span(), "query methods take explicit arguments, not a spread"));
            }
            if self.got_key {
                return Err(refuse(
                    step.span,
                    format!("`{collection}.get(key)` stands alone; nothing chains after it"),
                ));
            }
            if step.name == "get" {
                if self.started {
                    return Err(refuse(
                        step.span,
                        format!("`get(key)` stands alone: `{collection}.get(id)`"),
                    ));
                }
                let [arg] = step.args else {
                    return Err(refuse(step.span, "`get(key)` takes exactly one argument"));
                };
                if arg.spread.is_some() {
                    return Err(refuse(step.span, "`get(...)` takes a key, not a spread"));
                }
                let param = self.param(&arg.expr);
                self.descriptor.shape = Shape::Get { param };
                self.got_key = true;
                self.started = true;
                continue;
            }
            let wanted = match step.name.as_str() {
                "where" => Phase::Where,
                "orderBy" => Phase::Order,
                "limit" | "first" | "count" | "groupBy" => Phase::Done,
                other => {
                    return Err(refuse(
                        step.span,
                        format!(
                            "`{collection}.{other}` isn't a query method (the chain is {QUERY_METHODS})"
                        ),
                    ))
                }
            };
            if self.phase == Phase::Done || wanted < self.phase {
                return Err(refuse(
                    step.span,
                    format!(
                        "`.{}` is out of order: a chain is where* → orderBy* → one of limit, \
                         first, count, groupBy",
                        step.name
                    ),
                ));
            }
            self.phase = wanted;
            self.started = true;
            match step.name.as_str() {
                "where" => self.where_step(step)?,
                "orderBy" => self.order_step(step)?,
                "limit" => self.limit_step(step)?,
                "first" => {
                    no_args(step)?;
                    self.descriptor.bound = RowBound::First;
                }
                "count" => {
                    no_args(step)?;
                    self.descriptor.shape = Shape::Count;
                }
                _ => self.group_step(step)?,
            }
        }
        Ok(())
    }

    fn where_step(&mut self, step: &Step<'e>) -> Result<(), Refuse> {
        let [arg] = step.args else {
            return Err(refuse(
                step.span,
                "`.where(…)` takes one predicate object, or or(…)",
            ));
        };
        if arg.spread.is_some() {
            return Err(refuse(step.span, "`.where(...)` takes a predicate, not a spread"));
        }
        let expr: &'e Expr = &arg.expr;
        if let Some(call) = self.pass.helper_call(expr, "or") {
            let mut branches = Vec::new();
            for branch in &call.args {
                if branch.spread.is_some() {
                    return Err(refuse(branch.expr.span(), "or(…) takes objects, not a spread"));
                }
                branches.push(self.predicate_arg(&branch.expr, false)?);
            }
            self.descriptor.or_groups.push(branches);
            return Ok(());
        }
        let terms = self.predicate_arg(expr, true)?;
        self.descriptor.terms.extend(terms);
        Ok(())
    }

    /// A `.where()` or `or()` argument: an object literal, or an identifier bound
    /// by `const` to one (a named predicate, R3-1).
    fn predicate_arg(&mut self, expr: &'e Expr, optional_allowed: bool) -> Result<Vec<Term>, Refuse> {
        match unwrap(expr) {
            Expr::Object(obj) => self.predicate(obj, optional_allowed),
            Expr::Ident(ident) => {
                let consts: &'a Consts = self.pass.consts;
                let foreign: &'a HashMap<Id, ForeignPred> = self.pass.foreign;
                if let Some(imported) = foreign.get(&ident.to_id()) {
                    self.preds.push((ident.to_id(), ident.span));
                    let outer = self.path.replace((ident.span, Vec::new()));
                    let terms = self.predicate(&imported.loaded.obj, optional_allowed);
                    self.path = outer;
                    // A rule broken inside it is named at this import's use, with
                    // where in its home module it was written.
                    return terms.map_err(|refused| {
                        refuse(
                            ident.span,
                            format!(
                                "in `{}` (from {}): {}",
                                ident.sym,
                                imported.position(refused.span),
                                refused.message
                            ),
                        )
                    });
                }
                let Some(obj) = consts.predicates.get(&ident.to_id()) else {
                    return Err(refuse(
                        ident.span,
                        format!(
                            "`{}` is passed to `.where` by name, but it isn't a `const` bound to an \
                             object literal; write the predicate here, or name it with `const`",
                            ident.sym
                        ),
                    ));
                };
                self.preds.push((ident.to_id(), ident.span));
                let outer = self.path.replace((ident.span, Vec::new()));
                let terms = self.predicate(obj, optional_allowed);
                self.path = outer;
                terms
            }
            other => Err(refuse(
                other.span(),
                if optional_allowed {
                    "`.where(…)` takes a predicate object literal, or or(…)"
                } else {
                    "each or(…) branch is a predicate object literal"
                },
            )),
        }
    }

    fn predicate(&mut self, obj: &'e ObjectLit, optional_allowed: bool) -> Result<Vec<Term>, Refuse> {
        let mut terms = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for prop in &obj.props {
            let PropOrSpread::Prop(prop) = prop else {
                return Err(refuse(obj.span, "a predicate can't contain a spread: name each column"));
            };
            let (column, key_span) = match &**prop {
                Prop::KeyValue(kv) => {
                    let Some(column) = prop_key(&kv.key) else {
                        return Err(refuse(
                            kv.key.span(),
                            "a predicate key is a column name, not a computed or numeric key",
                        ));
                    };
                    (column, kv.key.span())
                }
                Prop::Shorthand(ident) => (ident.sym.to_string(), ident.span),
                _ => {
                    return Err(refuse(
                        prop.span(),
                        "a predicate holds `column: value` pairs only — no getters, setters or methods",
                    ))
                }
            };
            if column == "__proto__" {
                return Err(refuse(key_span, "`__proto__` isn't a column"));
            }
            if !seen.insert(column.clone()) {
                return Err(refuse(
                    key_span,
                    format!("`{column}` appears twice in one predicate (JS keeps only the last)"),
                ));
            }
            if column.contains('.') {
                return Err(refuse(
                    key_span,
                    format!("`{column}` reads through a reference: references arrive with Q1r"),
                ));
            }
            let term = self.keyed(&column.clone(), |build| match &**prop {
                Prop::Shorthand(ident) => Ok(Term {
                    column,
                    op: TermOp::Eq {
                        param: build.param_ident(ident),
                    },
                    optional_slot: build.fold_optional,
                }),
                Prop::KeyValue(kv) => build.term(column, &kv.value, optional_allowed),
                _ => unreachable!("refused above"),
            })?;
            terms.push(term);
        }
        Ok(terms)
    }

    fn term(&mut self, column: String, value: &'e Expr, optional_allowed: bool) -> Result<Term, Refuse> {
        if let Some(call) = self.pass.helper_call(value, "optional") {
            if let Some((base, _)) = &self.path {
                return Err(refuse(
                    call.span,
                    format!(
                        "optional() can't appear in a named predicate (`{}`): it is read whole; \
                         write optional(…) inline in `.where({{ … }})`",
                        self.pass.text(*base)
                    ),
                ));
            }
            if self.fold_optional.is_some() {
                return Err(refuse(
                    call.span,
                    "optional() inside an `if` fold is refused: the `if` already makes this column \
                     optional",
                ));
            }
            if !optional_allowed {
                return Err(refuse(
                    call.span,
                    "optional() inside or(…) is refused in Q1a: whether it drops the branch or \
                     makes it true is not decided",
                ));
            }
            let [arg] = call.args.as_slice() else {
                return Err(refuse(call.span, "optional(x) takes exactly one value"));
            };
            if arg.spread.is_some() {
                return Err(refuse(arg.expr.span(), "optional(x) takes one value, not a spread"));
            }
            let param = self.param(&arg.expr);
            let slot = self.next_optional;
            self.next_optional = self.next_optional.saturating_add(1);
            return Ok(Term {
                column,
                op: TermOp::Eq { param },
                optional_slot: Some(slot),
            });
        }
        let op = match unwrap(value) {
            Expr::Lit(Lit::Null(_)) => TermOp::IsNull,
            Expr::Object(ops) => self.operators(&column, ops)?,
            _ => TermOp::Eq {
                param: self.param(value),
            },
        };
        Ok(Term {
            column,
            op,
            optional_slot: self.fold_optional,
        })
    }

    fn operators(&mut self, column: &str, ops: &'e ObjectLit) -> Result<TermOp, Refuse> {
        let mut entries: Vec<(String, &'e Expr, Span)> = Vec::new();
        for prop in &ops.props {
            let PropOrSpread::Prop(prop) = prop else {
                return Err(refuse(ops.span, "an operator object can't contain a spread"));
            };
            let Prop::KeyValue(kv) = &**prop else {
                return Err(refuse(prop.span(), "an operator object holds `op: value` pairs only"));
            };
            let Some(key) = prop_key(&kv.key) else {
                return Err(refuse(kv.key.span(), "an operator is a name, not a computed key"));
            };
            if entries.iter().any(|(k, _, _)| *k == key) {
                return Err(refuse(kv.key.span(), format!("`{key}` appears twice")));
            }
            entries.push((key, &kv.value, kv.key.span()));
        }
        if let Some((key, _, span)) = entries.iter().find(|(k, _, _)| SEARCH_KEYS.contains(&k.as_str())) {
            return Err(refuse(
                *span,
                format!("`{column}: {{ {key}: … }}` is a text search: search arrives with Q2"),
            ));
        }
        if let Some((key, _, span)) = entries
            .iter()
            .find(|(k, _, _)| !matches!(k.as_str(), "not" | "in" | "lt" | "lte" | "gt" | "gte"))
        {
            return Err(refuse(
                *span,
                format!(
                    "`{column}: {{ {key}: … }}` filters through a nested object: references arrive \
                     with Q1r (operators here are not, in, lt, lte, gt, gte)"
                ),
            ));
        }
        if entries.is_empty() {
            return Err(refuse(ops.span, format!("`{column}: {{}}` tests nothing")));
        }
        if let Some((_, value, span)) = entries.iter().find(|(k, _, _)| k == "not") {
            if entries.len() != 1 || !matches!(unwrap(value), Expr::Lit(Lit::Null(_))) {
                return Err(refuse(*span, "`not` takes only `null`, alone: `{ not: null }`"));
            }
            return Ok(TermOp::NotNull);
        }
        if let Some((_, value, span)) = entries.iter().find(|(k, _, _)| k == "in") {
            if entries.len() != 1 {
                return Err(refuse(*span, "`in` stands alone in its operator object"));
            }
            let value: &'e Expr = value;
            let param = self.keyed("in", |build| build.param(value));
            return Ok(TermOp::In { param });
        }
        let mut lo: Option<RangeBound> = None;
        let mut hi: Option<RangeBound> = None;
        for (key, value, span) in entries {
            if matches!(unwrap(value), Expr::Lit(Lit::Null(_))) {
                return Err(refuse(span, format!("`{key}: null` can't bound a range")));
            }
            let lower = key.starts_with('g');
            if if lower { lo.is_some() } else { hi.is_some() } {
                return Err(refuse(
                    span,
                    format!("`{column}` has two {} bounds", if lower { "lower" } else { "upper" }),
                ));
            }
            let param = self.keyed(&key, |build| build.param(value));
            let bound = Some(RangeBound {
                param,
                inclusive: key.ends_with('e'),
            });
            if lower {
                lo = bound;
            } else {
                hi = bound;
            }
        }
        Ok(TermOp::Range { lo, hi })
    }

    fn order_step(&mut self, step: &Step<'e>) -> Result<(), Refuse> {
        let (column, dir) = match step.args {
            [column] => (column, None),
            [column, dir] => (column, Some(dir)),
            _ => {
                return Err(refuse(
                    step.span,
                    "`.orderBy(column, \"asc\" | \"desc\")` takes a column and an optional direction",
                ))
            }
        };
        let Some(column) = string_literal(&column.expr) else {
            return Err(refuse(column.expr.span(), "`.orderBy` names its column as a string literal"));
        };
        let dir = match dir.map(|d| string_literal(&d.expr)) {
            None => Direction::Asc,
            Some(Some(d)) if d == "asc" => Direction::Asc,
            Some(Some(d)) if d == "desc" => Direction::Desc,
            Some(_) => {
                return Err(refuse(step.span, "`.orderBy`'s direction is \"asc\" or \"desc\""));
            }
        };
        self.descriptor.order.push(OrderTerm { column, dir });
        Ok(())
    }

    fn limit_step(&mut self, step: &Step<'e>) -> Result<(), Refuse> {
        let [arg] = step.args else {
            return Err(refuse(step.span, "`.limit(n)` takes one integer"));
        };
        let value = match unwrap(&arg.expr) {
            Expr::Lit(Lit::Num(number)) => Some(number.value),
            Expr::Ident(ident) => self.pass.consts.numbers.get(&ident.to_id()).copied(),
            _ => None,
        };
        let Some(value) = value else {
            return Err(refuse(
                arg.expr.span(),
                "`.limit(n)` takes an integer literal 1..=1000, or a `const` bound to one (a bound \
                 is part of the query's shape)",
            ));
        };
        if value.fract() != 0.0 || value < 1.0 {
            return Err(refuse(arg.expr.span(), "`.limit(n)` takes an integer 1..=1000"));
        }
        self.descriptor.bound = RowBound::Limit {
            n: value.min(f64::from(u32::MAX)) as u32,
        };
        Ok(())
    }

    fn group_step(&mut self, step: &Step<'e>) -> Result<(), Refuse> {
        let [column, aggs] = step.args else {
            return Err(refuse(
                step.span,
                "`.groupBy(column, { alias: count() | sum(\"col\") | … })` takes two arguments",
            ));
        };
        let Some(column) = string_literal(&column.expr) else {
            return Err(refuse(column.expr.span(), "`.groupBy` names its column as a string literal"));
        };
        let Expr::Object(obj) = unwrap(&aggs.expr) else {
            return Err(refuse(aggs.expr.span(), "`.groupBy`'s aggregates are an object literal"));
        };
        let mut out = BTreeMap::new();
        for prop in &obj.props {
            let PropOrSpread::Prop(prop) = prop else {
                return Err(refuse(obj.span, "aggregates can't contain a spread"));
            };
            let Prop::KeyValue(kv) = &**prop else {
                return Err(refuse(prop.span(), "aggregates are `alias: count()` pairs"));
            };
            let Some(alias) = prop_key(&kv.key) else {
                return Err(refuse(kv.key.span(), "an aggregate alias is a name"));
            };
            let aggregate = self.aggregate(&kv.value)?;
            if out.insert(alias.clone(), aggregate).is_some() {
                return Err(refuse(kv.key.span(), format!("the alias `{alias}` is used twice")));
            }
        }
        self.descriptor.shape = Shape::Group { column, aggs: out };
        Ok(())
    }

    fn aggregate(&mut self, value: &Expr) -> Result<Aggregate, Refuse> {
        let Expr::Call(call) = unwrap(value) else {
            return Err(refuse(value.span(), "an aggregate is count(), sum(\"col\"), avg, min or max"));
        };
        if let Some(arg) = call.args.iter().find(|arg| arg.spread.is_some()) {
            return Err(refuse(arg.expr.span(), "aggregate helpers take explicit arguments, not a spread"));
        }
        let Callee::Expr(callee) = &call.callee else {
            return Err(refuse(call.span, "an aggregate is a helper from \"albedo/query\""));
        };
        let Some(helper) = self.pass.helper_of(callee) else {
            return Err(refuse(call.span, "an aggregate is a helper from \"albedo/query\""));
        };
        let column = || -> Result<String, Refuse> {
            match call.args.as_slice() {
                [arg] => string_literal(&arg.expr)
                    .ok_or_else(|| refuse(call.span, format!("{helper}(\"column\") names a column"))),
                _ => Err(refuse(call.span, format!("{helper}(\"column\") takes one column"))),
            }
        };
        Ok(match helper {
            "count" => {
                if !call.args.is_empty() {
                    return Err(refuse(call.span, "count() counts rows and takes no column"));
                }
                Aggregate::Count
            }
            "sum" => Aggregate::Sum { column: column()? },
            "avg" => Aggregate::Avg { column: column()? },
            "min" => Aggregate::Min { column: column()? },
            "max" => Aggregate::Max { column: column()? },
            other => {
                return Err(refuse(call.span, format!("`{other}` isn't an aggregate")));
            }
        })
    }
}

fn no_args(step: &Step<'_>) -> Result<(), Refuse> {
    if step.args.is_empty() {
        Ok(())
    } else {
        Err(refuse(step.span, format!("`.{}()` takes no arguments", step.name)))
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fold_tests;

#[cfg(test)]
mod emit_tests;

#[cfg(test)]
mod project_tests;

#[cfg(test)]
mod report_tests;

#[cfg(test)]
mod provenance_tests;

#[cfg(test)]
mod review_tests;
