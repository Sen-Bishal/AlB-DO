use crate::effects::EffectProfile;
use crate::types::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use swc_common::SourceMap;
use swc_ecma_ast::*;
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_visit::{Visit, VisitWith};

pub struct ComponentParser {
    source_map: Rc<SourceMap>,
}

impl ComponentParser {
    pub fn new() -> Self {
        Self {
            source_map: Rc::new(SourceMap::default()),
        }
    }

    pub fn parse_file(&self, path: &Path) -> Result<Vec<ParsedComponent>> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| CompilerError::AnalysisFailed(format!("Failed to read file: {}", e)))?;

        self.parse_source(&content, path.to_str().unwrap_or("unknown"))
    }

    pub fn parse_source(&self, source: &str, filename: &str) -> Result<Vec<ParsedComponent>> {
        let source_file = self.source_map.new_source_file(
            swc_common::FileName::Custom(filename.to_string()).into(),
            source.to_string(),
        );

        let syntax = if filename.ends_with(".tsx") || filename.ends_with(".ts") {
            Syntax::Typescript(TsSyntax {
                tsx: filename.ends_with(".tsx"),
                decorators: true,
                ..Default::default()
            })
        } else {
            Syntax::Es(EsSyntax {
                jsx: true,
                decorators: true,
                ..Default::default()
            })
        };

        let input = StringInput::from(&*source_file);
        let mut parser = Parser::new(syntax, input, None);

        let module = parser
            .parse_module()
            .map_err(|e| CompilerError::AnalysisFailed(format!("Parse error: {:?}", e)))?;

        let source_hash = hash_source(source);
        let mut visitor = ComponentVisitor::new(filename.to_string(), source_hash);
        module.visit_with(&mut visitor);

        // A file that declared no component but exports something is a pure
        // data/util/lib module. Surface it as a module-only node so a component
        // importing it (by a name that matches no component) still links it on
        // the server. Files with zero exports (types-only, side-effect-only)
        // produce nothing, exactly as before.
        if visitor.components.is_empty() && visitor.has_export {
            visitor.components.push(ParsedComponent {
                name: module_only_node_name(filename),
                file_path: filename.to_string(),
                line_number: 0,
                imports: visitor.current_imports.clone(),
                import_sources: visitor.current_import_sources.clone(),
                estimated_size: 0,
                is_default_export: false,
                props: Vec::new(),
                effect_profile: EffectProfile::default(),
                is_interactive: false,
                is_client_interactive: false,
                state_escapes: false,
                // A module-only node is never rendered, so it has no render to
                // be request-scoped.
                reads_principal: false,
                source_hash,
                is_module_only: true,
            });
        }

        Ok(visitor.components)
    }
}

/// Stable, collision-tolerant name for a module-only node, derived from the
/// file stem (`../content/essays.ts` → `essays`). Only used as a graph/manifest
/// label — module linking keys on `module_path`, not this name.
fn module_only_node_name(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("module");
    format!("__module__{stem}")
}

impl Default for ComponentParser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ParsedComponent {
    pub name: String,
    pub file_path: String,
    pub line_number: usize,
    pub imports: Vec<String>,
    /// Module specifiers (the `from "..."` strings) of every `import` in the
    /// file, in source order. Distinct from `imports` (the bound *names*):
    /// dependency wiring resolves these by **path** so a component can depend
    /// on a non-component data/util module it imports by a name that doesn't
    /// match any component (`import { getIssue } from "../content/essays"`).
    /// See `ProjectScanner::build_compiler`.
    pub import_sources: Vec<String>,
    pub estimated_size: usize,
    pub is_default_export: bool,
    pub props: Vec<String>,
    pub effect_profile: EffectProfile,
    /// True when the component declares ANY `on*` JSX handler. Forces the
    /// component off Tier-A (it must hydrate at least enough to round-trip)
    /// and drives hydration timing. See `EffectCollector::visit_jsx_attr`.
    pub is_interactive: bool,
    /// True when at least one `on*` handler is provably client-satisfiable —
    /// its closure (transitively through local definitions) touches no server
    /// boundary (network io). This is the dataflow lever that promotes a
    /// hooks component to Tier-C (client island, zero round-trip) vs Tier-B
    /// (server round-trip). A `Counter` (onClick→setState) is client-satisfiable;
    /// a `LikeButton` (onClick→`fetch`) is not. See step 2 in the tier design.
    pub is_client_interactive: bool,
    /// Item 4.9 T1 · **the state-ownership signal.** True when the component's
    /// state is *proven* to leave the client — a `useSharedSlot` topic, a FORGE
    /// write, a server `action`, or a network boundary. False means "not proven
    /// to escape", **not** "proven local": see [`is_escape_call`] for why the
    /// unknown case must round toward Tier C.
    pub state_escapes: bool,
    /// AUTH § 3 · this component names `user`, so the render needs the
    /// request's principal. See [`scan_reads_principal`].
    pub reads_principal: bool,
    pub source_hash: u64,
    /// True when this node represents a file that declared NO component but
    /// DOES export something (a pure data/util/lib module). It is a graph +
    /// manifest + module-registry node purely so importers can link it on
    /// the server; it is never routed, tiered for render, or statically
    /// rendered. See `ProjectScanner::build_compiler` and the manifest
    /// builder's `sorted_children` skip.
    pub is_module_only: bool,
}

struct ComponentVisitor {
    file_path: String,
    source_hash: u64,
    components: Vec<ParsedComponent>,
    current_imports: Vec<String>,
    /// Parallel to `current_imports` but keyed by the module specifier
    /// (`from "..."`) rather than the bound name — see `import_sources`.
    current_import_sources: Vec<String>,
    /// The file exported at least one binding (named, default, or `export {}`).
    /// Gates synthesis of a module-only node for component-less files.
    has_export: bool,
}

impl ComponentVisitor {
    fn new(file_path: String, source_hash: u64) -> Self {
        Self {
            file_path,
            source_hash,
            components: Vec::new(),
            current_imports: Vec::new(),
            current_import_sources: Vec::new(),
            has_export: false,
        }
    }

    fn extract_component_name(&self, expr: &Expr) -> Option<String> {
        match expr {
            Expr::Ident(ident) => Some(ident.sym.to_string()),
            Expr::Arrow(_) => Some(format!("ArrowComponent_{}", self.components.len())),
            Expr::Fn(func) => func.ident.as_ref().map(|i| i.sym.to_string()),
            _ => None,
        }
    }

    fn analyze_function(&self, function: &Function) -> ComponentAnalysis {
        let mut collector = EffectCollector::default();
        if function.is_async {
            collector.profile.asynchronous = true;
        }
        let params: Vec<Pat> = function.params.iter().map(|p| p.pat.clone()).collect();
        let mut analysis = if let Some(body) = &function.body {
            collector.prime_local_defs(&body.stmts);
            body.visit_with(&mut collector);
            let mut analysis = collector.finish();
            analysis.reads_principal = scan_reads_principal(&body.stmts, &params);
            analysis
        } else {
            collector.finish()
        };
        analysis.reads_principal |= scan_reads_principal(&[], &params);
        analysis
    }

    fn analyze_arrow(&self, arrow: &ArrowExpr) -> ComponentAnalysis {
        let mut collector = EffectCollector::default();
        if arrow.is_async {
            collector.profile.asynchronous = true;
        }
        let mut body_stmts: Vec<Stmt> = Vec::new();
        match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => {
                collector.prime_local_defs(&block.stmts);
                block.visit_with(&mut collector);
                body_stmts = block.stmts.clone();
            }
            BlockStmtOrExpr::Expr(expr) => {
                expr.visit_with(&mut collector);
                // An expression-bodied arrow has no statements to walk, so wrap
                // it in one rather than duplicating the scan for the two shapes.
                body_stmts.push(Stmt::Expr(ExprStmt {
                    span: Default::default(),
                    expr: expr.clone(),
                }));
            }
        }
        let mut analysis = collector.finish();
        analysis.reads_principal = scan_reads_principal(&body_stmts, &arrow.params);
        analysis
    }

    fn analyze_expr(&self, expr: &Expr) -> ComponentAnalysis {
        match expr {
            Expr::Arrow(arrow) => self.analyze_arrow(arrow),
            Expr::Fn(function) => self.analyze_function(&function.function),
            _ => ComponentAnalysis::default(),
        }
    }

    fn push_component(
        &mut self,
        name: String,
        estimated_size: usize,
        is_default_export: bool,
        analysis: ComponentAnalysis,
    ) {
        self.components.push(ParsedComponent {
            name,
            file_path: self.file_path.clone(),
            line_number: 0,
            imports: self.current_imports.clone(),
            import_sources: self.current_import_sources.clone(),
            estimated_size,
            is_default_export,
            props: Vec::new(),
            effect_profile: analysis.profile,
            is_interactive: analysis.is_interactive,
            is_client_interactive: analysis.is_client_interactive,
            state_escapes: analysis.state_escapes,
            reads_principal: analysis.reads_principal,
            source_hash: self.source_hash,
            is_module_only: false,
        });
    }
}

impl Visit for ComponentVisitor {
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        // `import type { T } from "..."` is fully erased by the TS transform —
        // it creates no runtime module dependency, so it must contribute no
        // edge. A phantom edge from a type-only import can give an unrelated
        // route component a dependent (e.g. a `Foo` interface import colliding
        // with a component named `Foo`), which makes the route look non-root
        // and silently drops it from the manifest.
        if import.type_only {
            return;
        }

        // Collect only the runtime (non-type) binding names. Inline
        // `import { value, type T }` specifiers are erased per-specifier.
        let mut names = Vec::new();
        for spec in &import.specifiers {
            match spec {
                ImportSpecifier::Named(n) if n.is_type_only => continue,
                ImportSpecifier::Named(n) => names.push(n.local.sym.to_string()),
                ImportSpecifier::Default(d) => names.push(d.local.sym.to_string()),
                ImportSpecifier::Namespace(n) => names.push(n.local.sym.to_string()),
            }
        }

        // Record the module specifier only for a real runtime import: one that
        // binds runtime names, or a bare side-effect import (`import "./x"`,
        // no specifiers). An import whose every specifier was type-only is
        // erased and wires nothing.
        if !names.is_empty() || import.specifiers.is_empty() {
            self.current_import_sources
                .push(import.src.value.to_string());
        }
        self.current_imports.extend(names);
    }

    fn visit_export_decl(&mut self, export: &ExportDecl) {
        // `export const x` / `export function f` / `export class C` — mark the
        // file as exporting, then recurse so the inner fn/var decl still reaches
        // `visit_fn_decl` / `visit_var_decl` (an exported component is detected
        // exactly as a non-exported one is).
        self.has_export = true;
        export.visit_children_with(self);
    }

    fn visit_named_export(&mut self, export: &NamedExport) {
        // `export { a, b }` and re-exports `export { x } from "./y"` — declare no
        // component but make the file an importable module.
        self.has_export = true;
        export.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, func: &FnDecl) {
        let name = func.ident.sym.to_string();

        if name.chars().next().is_some_and(|c| c.is_uppercase()) {
            let estimated_size = name.len() * 50 + 200;
            let analysis = self.analyze_function(&func.function);
            self.push_component(name, estimated_size, false, analysis);
        }
    }

    fn visit_var_decl(&mut self, var: &VarDecl) {
        for decl in &var.decls {
            if let Some(name) = decl.name.as_ident().map(|i| i.sym.to_string()) {
                if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                    if let Some(init) = &decl.init {
                        let is_component = matches!(&**init, Expr::Arrow(_) | Expr::Fn(_));
                        if is_component {
                            let estimated_size = name.len() * 50 + 200;
                            let analysis = self.analyze_expr(init);
                            self.push_component(name, estimated_size, false, analysis);
                        }
                    }
                }
            }
        }
    }

    fn visit_export_default_decl(&mut self, export: &ExportDefaultDecl) {
        self.has_export = true;
        if let DefaultDecl::Fn(func) = &export.decl {
            if let Some(ident) = &func.ident {
                let name = ident.sym.to_string();
                let estimated_size = name.len() * 50 + 300;
                let analysis = self.analyze_function(&func.function);
                self.push_component(name, estimated_size, true, analysis);
            }
        }
    }

    fn visit_export_default_expr(&mut self, export: &ExportDefaultExpr) {
        self.has_export = true;
        if let Some(name) = self.extract_component_name(&export.expr) {
            let estimated_size = name.len() * 50 + 200;
            let analysis = self.analyze_expr(&export.expr);

            if let Some(comp) = self.components.iter_mut().find(|c| c.name == name) {
                comp.is_default_export = true;
                comp.effect_profile = comp.effect_profile.join(analysis.profile);
                comp.is_interactive = comp.is_interactive || analysis.is_interactive;
                comp.is_client_interactive =
                    comp.is_client_interactive || analysis.is_client_interactive;
                comp.state_escapes = comp.state_escapes || analysis.state_escapes;
            } else {
                self.push_component(name, estimated_size, true, analysis);
            }
        }
    }
}

/// Outcome of analyzing one component's defining closure.
#[derive(Default)]
struct ComponentAnalysis {
    profile: EffectProfile,
    /// Any `on*` handler present (keeps the component off Tier-A).
    is_interactive: bool,
    /// At least one handler is provably client-satisfiable (no server boundary).
    is_client_interactive: bool,
    /// This component's state is **proven** to escape the client — see
    /// [`is_escape_call`] and `Component::state_escapes`.
    state_escapes: bool,
    /// This component reads the request's principal — see
    /// [`scan_reads_principal`]. Like `state_escapes` this is a fact read off
    /// the AST rather than a heuristic, which is the distinction
    /// `TODO.md` P-c asks the decision path to stop losing.
    reads_principal: bool,
}

#[derive(Default)]
struct EffectCollector {
    profile: EffectProfile,
    /// Per-component map: local function/const name -> client-safe (its body
    /// reaches no server boundary, transitively via other locals). Primed from
    /// the component body before the main walk so handler references such as
    /// `onClick={inc}` can be resolved against it.
    local_safety: HashMap<String, bool>,
    /// Per-component map: local function/const name -> **escapes** (its body
    /// reaches an escape construct, transitively via other locals). Same
    /// fixpoint as `local_safety`, opposite polarity, so `onClick={handleAdd}`
    /// where `const handleAdd = () => append(…)` is seen as an escape. That
    /// indirection is the common idiom, not an edge case.
    local_escapes: HashMap<String, bool>,
    /// Saw at least one `on*` JSX handler prop.
    has_handler: bool,
    /// Saw at least one provably client-satisfiable `on*` handler.
    has_client_handler: bool,
    /// Saw at least one escape construct, at render scope **or inside a handler
    /// closure**.
    escapes: bool,
    /// Names bound locally in the component body. These **shadow** ALBEDO's
    /// ambient globals — see [`DefScan::found_escape`].
    shadowed: HashSet<String>,
}

impl EffectCollector {
    fn finish(self) -> ComponentAnalysis {
        ComponentAnalysis {
            profile: self.profile,
            is_interactive: self.has_handler,
            is_client_interactive: self.has_client_handler,
            state_escapes: self.escapes,
            // Filled by `scan_reads_principal` at the call site. It is a separate
            // walk on purpose: this collector deliberately does *not* descend
            // into `on*` handler closures (their effects run at interaction
            // time), but a `user` read anywhere in the component still means the
            // render needs the principal.
            reads_principal: false,
        }
    }

    /// Collect local `const NAME = closure` / `function NAME` definitions from
    /// the component body and classify each as client-safe (its body reaches no
    /// network/server boundary, transitively through other locals). This lets a
    /// handler reference like `onClick={inc}` resolve to `inc`'s analysis.
    fn prime_local_defs(&mut self, stmts: &[Stmt]) {
        // Shadowing is collected first: every scan below consults it, and a
        // name bound by `const { append } = useFieldArray(…)` must never be
        // read as ALBEDO's ambient FORGE write.
        collect_bound_names(stmts, &mut self.shadowed);

        // direct[name]        = body directly contains a server-boundary io call.
        // direct_escape[name] = body directly contains an escape construct.
        // calls[name]         = function names this def invokes (for transitivity).
        let mut direct: HashMap<String, bool> = HashMap::new();
        let mut direct_escape: HashMap<String, bool> = HashMap::new();
        let mut calls: HashMap<String, Vec<String>> = HashMap::new();

        for stmt in stmts {
            match stmt {
                Stmt::Decl(Decl::Fn(f)) => {
                    let mut scan = DefScan::default();
                    if let Some(body) = &f.function.body {
                        body.visit_with(&mut scan);
                    }
                    let name = f.ident.sym.to_string();
                    direct.insert(name.clone(), scan.found_io);
                    direct_escape.insert(name.clone(), scan.found_escape(&self.shadowed));
                    calls.insert(name, scan.called);
                }
                Stmt::Decl(Decl::Var(var)) => {
                    for decl in &var.decls {
                        if let Some(name) = decl.name.as_ident().map(|i| i.sym.to_string()) {
                            if let Some(init) = &decl.init {
                                if let Some(scan) = scan_closure(init) {
                                    direct.insert(name.clone(), scan.found_io);
                                    direct_escape
                                        .insert(name.clone(), scan.found_escape(&self.shadowed));
                                    calls.insert(name, scan.called);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        /// Least fixpoint over the local call graph: a def is tainted if it is
        /// directly tainted, or it calls a local def already known tainted.
        /// Shared by the two analyses so they cannot drift apart.
        fn propagate(
            direct: &HashMap<String, bool>,
            calls: &HashMap<String, Vec<String>>,
        ) -> HashSet<String> {
            let mut tainted: HashSet<String> = direct
                .iter()
                .filter(|(_, hit)| **hit)
                .map(|(name, _)| name.clone())
                .collect();
            loop {
                let mut changed = false;
                for (name, callees) in calls {
                    if tainted.contains(name) {
                        continue;
                    }
                    if callees.iter().any(|c| tainted.contains(c)) {
                        tainted.insert(name.clone());
                        changed = true;
                    }
                }
                if !changed {
                    break;
                }
            }
            tainted
        }

        let unsafe_defs = propagate(&direct, &calls);
        let escaping_defs = propagate(&direct_escape, &calls);

        self.local_safety = direct
            .keys()
            .map(|name| (name.clone(), !unsafe_defs.contains(name)))
            .collect();
        self.local_escapes = direct_escape
            .keys()
            .map(|name| (name.clone(), escaping_defs.contains(name)))
            .collect();
    }

    /// Does this `on*` handler value reach an escape construct?
    ///
    /// Deliberately **descends into the closure**, unlike the effect-profile
    /// walk in `visit_jsx_attr`, which must not: a `fetch` inside `onClick` is
    /// not a *render-time* io boundary, but an `append()` inside `onClick` is
    /// still state leaving the client. Escape is a property of the transition,
    /// not of when it runs.
    fn handler_escapes(&self, value: Option<&JSXAttrValue>) -> bool {
        let Some(JSXAttrValue::JSXExprContainer(container)) = value else {
            return false;
        };
        let JSXExpr::Expr(expr) = &container.expr else {
            return false;
        };

        // `onClick={handleAdd}` — resolve through the local fixpoint.
        if let Expr::Ident(ident) = &**expr {
            return self
                .local_escapes
                .get(ident.sym.as_ref())
                .copied()
                .unwrap_or(false);
        }

        let mut scan = DefScan::default();
        expr.visit_with(&mut scan);
        scan.found_escape(&self.shadowed)
            || scan
                .called
                .iter()
                .any(|name| self.local_escapes.get(name) == Some(&true))
    }

    /// Is this `on*` handler value provably client-satisfiable?
    fn handler_is_client_safe(&self, value: Option<&JSXAttrValue>) -> bool {
        match value {
            Some(JSXAttrValue::JSXExprContainer(container)) => match &container.expr {
                JSXExpr::Expr(expr) => self.expr_handler_client_safe(expr),
                JSXExpr::JSXEmptyExpr(_) => true,
            },
            // String-literal handler or boolean shorthand: no server boundary.
            _ => true,
        }
    }

    fn expr_handler_client_safe(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Arrow(arrow) => !arrow_hits_server_boundary(arrow, &self.local_safety),
            Expr::Fn(func) => func.function.body.as_ref().map_or(true, |body| {
                let mut scan = ServerBoundaryScan::new(&self.local_safety);
                body.visit_with(&mut scan);
                !scan.found
            }),
            Expr::Ident(id) => self
                .local_safety
                .get(id.sym.as_ref())
                .copied()
                .unwrap_or(true),
            Expr::Paren(paren) => self.expr_handler_client_safe(&paren.expr),
            // Member access, call result, etc.: not a *provable* server boundary.
            // Round toward Tier-C (a wrong Tier-C still works for a client-side
            // fetch; "use server" can override the unprovable long tail).
            _ => true,
        }
    }

    fn mark_call(&mut self, call_name: &str) {
        let name = call_name.trim();
        if is_hook_call(name) {
            self.profile.hooks = true;
        }
        if is_escape_call(name) && !self.shadowed.contains(name) {
            // Item 4.9 T1. Render-scope escapes — `useSharedSlot(…)` is the
            // load-bearing one: the state IS a broadcast topic, so it is shared
            // by construction and no analysis of *who else reads it* is needed.
            self.escapes = true;
        }
        if is_effect_hook_call(name) {
            // An effect hook requires client execution → fully hydrated Tier-C
            // island, never serve-wired or server-only Tier-B.
            self.profile.side_effects = true;
        }
        if is_io_call(name) {
            self.profile.io = true;
            self.profile.asynchronous = true;
        }
        if is_async_call(name) {
            self.profile.asynchronous = true;
        }
        if is_side_effect_call(name) {
            self.profile.side_effects = true;
        }
    }
}

impl Visit for EffectCollector {
    fn visit_await_expr(&mut self, await_expr: &AwaitExpr) {
        self.profile.asynchronous = true;
        await_expr.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Some(name) = callee_name(&call.callee) {
            self.mark_call(&name);
        }
        call.visit_children_with(self);
    }

    fn visit_jsx_attr(&mut self, attr: &JSXAttr) {
        // Handler detection: any `on[A-Z]…` prop (onClick, onSubmit, onChange…)
        // makes the component interactive. Server actions are authored as the
        // distinct `action="action:NAME"` attribute (not an `on*` prop) and so
        // are excluded by construction — they round-trip and stay Tier-B.
        if let JSXAttrName::Ident(ident) = &attr.name {
            if is_event_handler_prop(ident.sym.as_ref()) {
                self.has_handler = true;
                if self.handler_is_client_safe(attr.value.as_ref()) {
                    self.has_client_handler = true;
                }
                // Item 4.9 T1. Escape IS read out of the handler closure, even
                // though the effect profile deliberately is not — see
                // `handler_escapes`. `onClick={() => append(…)}` writes to
                // FORGE, and that is state leaving the client whenever it runs.
                if self.handler_escapes(attr.value.as_ref()) {
                    self.escapes = true;
                }
                // Do NOT descend into the handler closure: its effects run at
                // interaction time, not render time, so they must not pollute
                // the render-time effect profile (a `fetch` inside onClick is
                // not a render-time io boundary — it is classified above).
                return;
            }
        }
        attr.visit_children_with(self);
    }
}

fn is_event_handler_prop(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() > 2 && &bytes[..2] == b"on" && bytes[2].is_ascii_uppercase()
}

/// Does this call make the component's state **escape the client**?
///
/// Item 4.9 **T1**. This is the signal the tier ladder is actually about: state
/// that leaves one browser is state the *server* owns, so its updates travel as
/// opcodes over the wire and the component ships no code (Tier B). State no one
/// outside the tab can observe is client-owned, and an island is the honest
/// answer (Tier C).
///
/// 🔑 **Only positive evidence counts.** Absence of a match means *"not proven
/// to escape"*, never *"proven local"* — so an unmatched component keeps the
/// tier it has today. That direction is deliberate: **a wrong Tier C still
/// works** (an island is the general fallback), while a wrong Tier B ships a
/// binding for state the wire cannot drive. Unknown must round toward C, the
/// top of the lattice.
///
/// 🪤 This is the lesson of the signal it sits beside. `is_server_boundary_call`
/// is a 12-name list that fires on **2 of 536** interactive components in the
/// real-world corpus (`TIER_DISTRIBUTION.md`) — a bit that cannot say "no" is
/// not a lever. These names are different in kind: they are **ALBEDO's own
/// constructs**, so a match is a fact about the framework's semantics rather
/// than a guess about an ecosystem's naming conventions.
fn is_escape_call(name: &str) -> bool {
    const ESCAPES: &[&str] = &[
        // The state IS a broadcast topic — another client observes it.
        "useSharedSlot",
        "broadcast",
        // FORGE writes: the state escapes to persistence and outlives the tab.
        "append",
        "update",
        "remove",
        // A server action: the transition is computed on the server.
        "action",
    ];
    let name = name.trim();
    ESCAPES.contains(&name) || is_server_boundary_call(name)
}

/// A network/server boundary reachable from a handler closure forces Tier-B.
/// Subset of `is_io_call`: client-only storage (localStorage/sessionStorage)
/// is deliberately excluded — it is satisfiable in a Tier-C client island.
fn is_server_boundary_call(name: &str) -> bool {
    const SERVER_IO: &[&str] = &[
        "fetch",
        "axios",
        "axios.get",
        "axios.post",
        "fs.readFile",
        "fs.readFileSync",
        "fs.writeFile",
        "fs.writeFileSync",
        "http.get",
        "http.request",
        "https.get",
        "https.request",
    ];
    SERVER_IO.iter().any(|candidate| *candidate == name)
}

/// Scans a local definition's body for a direct server boundary and records the
/// function names it calls (so callers can propagate taint transitively).
#[derive(Default)]
struct DefScan {
    found_io: bool,
    called: Vec<String>,
}

impl DefScan {
    /// Item 4.9 T1. Did this body reach an escape construct that is **not
    /// shadowed by a local binding**?
    ///
    /// 🔴 The shadow check is not a nicety — it is what keeps the analysis
    /// sound. `append`/`update`/`remove` are ALBEDO's ambient FORGE writes, but
    /// they are also exactly what `react-hook-form` hands you:
    /// `const { fields, append, remove, update } = useFieldArray(…)`. That is
    /// **purely client-side form state**, and calling it an escape would ship a
    /// Tier-B binding for state the wire cannot drive — a broken component, in
    /// the one direction this analysis must never round toward.
    ///
    /// 🪤 Measured: without this, 6 of 1,398 real-world components were
    /// misclassified, **all of them `useFieldArray` destructures in cal.com**.
    /// A locally-bound name is a local, never the global.
    fn found_escape(&self, shadowed: &HashSet<String>) -> bool {
        self.called
            .iter()
            .any(|name| is_escape_call(name) && !shadowed.contains(name.trim()))
    }
}

impl Visit for DefScan {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Some(name) = callee_name(&call.callee) {
            if is_server_boundary_call(&name) {
                self.found_io = true;
            }
            self.called.push(name);
        }
        call.visit_children_with(self);
    }
}

/// Every identifier bound by a `const`/`let`/`var` in these statements,
/// including destructuring patterns — `const { append, remove } = …` binds both.
/// Used to shadow ALBEDO's ambient globals; see [`DefScan::found_escape`].
/// The one identifier that means "the request's signed-in principal".
///
/// Named here rather than spelled at each use site because three things have to
/// agree on it: this scan, the prop key the manifest records
/// (`dynamic_prop_keys_for_component`), and the key the server resolves
/// (`RequestContext::resolve`). Two spellings of one contract is the drift the
/// form-markup constants already had to be centralised to prevent.
pub const PRINCIPAL_IDENT: &str = "user";

/// Does this component read the request's principal?
///
/// ## Why this is a component fact and not a route declaration
///
/// AUTH § 4's claim is *derived, never authored*. A component that writes
/// `user.id` has already said it needs the principal; asking the author to also
/// record that somewhere is the second thing to keep in step, and the one that
/// gets forgotten. So the fact is read off the AST — the same shape as
/// `state_escapes`, and the same free-variable walk `item_expr_only_refs` does
/// for the keyed-list rung.
///
/// ## What counts
///
/// A **value-position** `user`. Member-property names (`row.user`) are
/// `IdentName`s inside `MemberProp` and JSX attribute names (`<Row user={x} />`)
/// are `JSXAttrName`s — neither is an `Expr::Ident`, so neither is visited. That
/// is exactly right: those name someone else's field, not our principal.
///
/// A `user` **bound locally** (`const user = rows[0]`) shadows the ambient one
/// and does not count, by the same rule that stops `const { append } = …` from
/// reading as a FORGE write.
///
/// 🔑 **Unknown rounds toward "reads it".** A false positive costs a build-time
/// bake — the component renders per request instead of once — which is slower
/// and correct. A false negative is the defect this function exists to fix: the
/// component is baked with no principal and its signed-in branch can never
/// render. The cheap error is the one to make.
fn scan_reads_principal(stmts: &[Stmt], params: &[Pat]) -> bool {
    // A `user` destructured from props is a *parameter* binding, so it must not
    // be collected as a shadow — it is precisely the case we are looking for.
    // Only body-local bindings shadow.
    let mut shadowed = HashSet::new();
    collect_bound_names(stmts, &mut shadowed);
    if shadowed.contains(PRINCIPAL_IDENT) {
        return false;
    }

    struct Scan {
        found: bool,
    }
    impl Visit for Scan {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Ident(ident) = expr {
                if ident.sym.as_ref() == PRINCIPAL_IDENT {
                    self.found = true;
                    return;
                }
            }
            expr.visit_children_with(self);
        }
    }

    let mut scan = Scan { found: false };
    // Destructuring `{ user }` out of the props parameter is a read on its own —
    // the component asked for it in its signature even if the body then only
    // passes it along.
    for param in params {
        if let Pat::Object(object) = param {
            for prop in &object.props {
                let named = match prop {
                    ObjectPatProp::Assign(assign) => assign.key.sym.as_ref() == PRINCIPAL_IDENT,
                    ObjectPatProp::KeyValue(kv) => matches!(
                        &kv.key,
                        PropName::Ident(ident) if ident.sym.as_ref() == PRINCIPAL_IDENT
                    ),
                    ObjectPatProp::Rest(_) => false,
                };
                if named {
                    return true;
                }
            }
        }
    }
    stmts.visit_with(&mut scan);
    scan.found
}

fn collect_bound_names(stmts: &[Stmt], out: &mut HashSet<String>) {
    fn from_pat(pat: &Pat, out: &mut HashSet<String>) {
        match pat {
            Pat::Ident(ident) => {
                out.insert(ident.id.sym.to_string());
            }
            Pat::Array(array) => {
                for element in array.elems.iter().flatten() {
                    from_pat(element, out);
                }
            }
            Pat::Object(object) => {
                for prop in &object.props {
                    match prop {
                        ObjectPatProp::KeyValue(kv) => from_pat(&kv.value, out),
                        ObjectPatProp::Assign(assign) => {
                            out.insert(assign.key.sym.to_string());
                        }
                        ObjectPatProp::Rest(rest) => from_pat(&rest.arg, out),
                    }
                }
            }
            Pat::Assign(assign) => from_pat(&assign.left, out),
            Pat::Rest(rest) => from_pat(&rest.arg, out),
            _ => {}
        }
    }

    for stmt in stmts {
        if let Stmt::Decl(Decl::Var(var)) = stmt {
            for decl in &var.decls {
                from_pat(&decl.name, out);
            }
        }
    }
}

/// Returns the [`DefScan`] for a closure expression, or `None` when the
/// initializer is not a function (so it is not a callable local handler).
fn scan_closure(expr: &Expr) -> Option<DefScan> {
    let mut scan = DefScan::default();
    match expr {
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => block.visit_with(&mut scan),
            BlockStmtOrExpr::Expr(inner) => inner.visit_with(&mut scan),
        },
        Expr::Fn(func) => {
            if let Some(body) = &func.function.body {
                body.visit_with(&mut scan);
            }
        }
        _ => return None,
    }
    Some(scan)
}

/// Walks a handler closure and flags whether it reaches a server boundary —
/// either a direct network io call or a call to a local def known to be unsafe.
struct ServerBoundaryScan<'a> {
    local_safety: &'a HashMap<String, bool>,
    found: bool,
}

impl<'a> ServerBoundaryScan<'a> {
    fn new(local_safety: &'a HashMap<String, bool>) -> Self {
        Self {
            local_safety,
            found: false,
        }
    }
}

impl Visit for ServerBoundaryScan<'_> {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Some(name) = callee_name(&call.callee) {
            if is_server_boundary_call(&name) || self.local_safety.get(&name) == Some(&false) {
                self.found = true;
            }
        }
        call.visit_children_with(self);
    }
}

fn arrow_hits_server_boundary(arrow: &ArrowExpr, local_safety: &HashMap<String, bool>) -> bool {
    let mut scan = ServerBoundaryScan::new(local_safety);
    match &*arrow.body {
        BlockStmtOrExpr::BlockStmt(block) => block.visit_with(&mut scan),
        BlockStmtOrExpr::Expr(inner) => inner.visit_with(&mut scan),
    }
    scan.found
}

fn hash_source(source: &str) -> u64 {
    // xxh3_64 — matches `stable_source_hash` in engine.rs and the file
    // content hash in `incremental.rs`. DefaultHasher must NOT be used here:
    // it is not stable across Rust versions or process restarts, which would
    // corrupt the incremental cache.
    xxhash_rust::xxh3::xxh3_64(source.as_bytes())
}

fn callee_name(callee: &Callee) -> Option<String> {
    match callee {
        Callee::Expr(expr) => expr_name(expr),
        Callee::Super(_) | Callee::Import(_) => None,
    }
}

fn expr_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(ident) => Some(ident.sym.to_string()),
        Expr::Member(member) => member_name(member),
        _ => None,
    }
}

fn member_name(member: &MemberExpr) -> Option<String> {
    let object = expr_name(&member.obj)?;
    let property = match &member.prop {
        MemberProp::Ident(ident) => ident.sym.to_string(),
        MemberProp::Computed(computed) => expr_name(&computed.expr)?,
        MemberProp::PrivateName(_) => return None,
    };
    Some(format!("{object}.{property}"))
}

fn is_hook_call(name: &str) -> bool {
    if !name.starts_with("use") || name.len() <= 3 {
        return false;
    }
    name.chars().nth(3).is_some_and(|ch| ch.is_uppercase())
}

/// The effect hooks — `useEffect`/`useLayoutEffect`/`useInsertionEffect` — are a
/// *client-lifecycle* requirement, not a passive hook: the effect body must run
/// in the browser (event listeners, DOM measurement, subscriptions, document
/// mutation). They cannot be expressed as declarative serve-wired bindings, so
/// they mark the component as side-effecting, which promotes it to a fully
/// hydrated Tier-C island (see `decide_tier_and_hydration`). Without this, an
/// effect-only component (no `on*` handler) would be read as a passive-hook
/// component and mis-tiered to Tier B (server-only), silently dropping the
/// effect on the serve path.
fn is_effect_hook_call(name: &str) -> bool {
    matches!(
        name.trim(),
        "useEffect" | "useLayoutEffect" | "useInsertionEffect"
    )
}

fn is_async_call(name: &str) -> bool {
    const ASYNC_CALLS: &[&str] = &[
        "fetch",
        "Promise.all",
        "Promise.race",
        "Promise.resolve",
        "setTimeout",
        "queueMicrotask",
    ];
    ASYNC_CALLS.iter().any(|candidate| *candidate == name)
}

fn is_io_call(name: &str) -> bool {
    const IO_CALLS: &[&str] = &[
        "fetch",
        "axios",
        "axios.get",
        "axios.post",
        "fs.readFile",
        "fs.readFileSync",
        "fs.writeFile",
        "fs.writeFileSync",
        "http.get",
        "http.request",
        "https.get",
        "https.request",
        "localStorage.getItem",
        "localStorage.setItem",
        "sessionStorage.getItem",
        "sessionStorage.setItem",
    ];
    IO_CALLS.iter().any(|candidate| *candidate == name)
}

fn is_side_effect_call(name: &str) -> bool {
    const SIDE_EFFECT_CALLS: &[&str] = &[
        "console.log",
        "console.info",
        "console.warn",
        "console.error",
        "document.write",
        "localStorage.setItem",
        "sessionStorage.setItem",
        "history.pushState",
        "window.location.assign",
    ];
    SIDE_EFFECT_CALLS.iter().any(|candidate| *candidate == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_import_source_paths_distinct_from_names() {
        let parser = ComponentParser::new();
        let source = r#"
            import EssayCard from "../components/EssayCard";
            import { getIssue, essays } from "../content/essays";
            export default function Issue() {
                return <main>{getIssue().title}</main>;
            }
        "#;

        let components = parser.parse_source(source, "src/routes/index.tsx").unwrap();
        let issue = components.iter().find(|c| c.name == "Issue").unwrap();
        // Names keep the legacy behaviour…
        assert!(issue.imports.contains(&"EssayCard".to_string()));
        assert!(issue.imports.contains(&"getIssue".to_string()));
        // …and the new field carries the *specifiers* for path resolution.
        assert!(issue
            .import_sources
            .contains(&"../components/EssayCard".to_string()));
        assert!(issue
            .import_sources
            .contains(&"../content/essays".to_string()));
        assert!(!issue.is_module_only);
    }

    #[test]
    fn data_module_with_no_component_becomes_module_only_node() {
        let parser = ComponentParser::new();
        let source = r#"
            export interface Essay { slug: string; title: string; }
            export const essays: Essay[] = [{ slug: "a", title: "A" }];
            export function getIssue() { return essays[0]; }
        "#;

        let components = parser.parse_source(source, "src/content/essays.ts").unwrap();
        assert_eq!(components.len(), 1, "one synthetic module-only node");
        assert!(components[0].is_module_only);
        assert_eq!(components[0].file_path, "src/content/essays.ts");
    }

    #[test]
    fn file_with_no_exports_and_no_component_produces_nothing() {
        let parser = ComponentParser::new();
        // Types-only / side-effect-free with no export → not a node.
        let source = "type Local = { x: number };\nconst unused: Local = { x: 1 };\n";
        let components = parser.parse_source(source, "src/internal.ts").unwrap();
        assert!(components.is_empty());
    }

    #[test]
    fn test_parse_simple_component() {
        let parser = ComponentParser::new();
        let source = r#"
            function Button() {
                return <button>Click</button>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "Button");
    }

    #[test]
    fn test_parse_arrow_component() {
        let parser = ComponentParser::new();
        let source = r#"
            const Header = () => {
                return <header>Title</header>;
            };
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "Header");
    }

    #[test]
    fn test_parse_with_imports() {
        let parser = ComponentParser::new();
        let source = r#"
            import React from 'react';
            import Button from './Button';
            
            function App() {
                return <div><Button /></div>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(components.len(), 1);
        assert!(components[0].imports.contains(&"Button".to_string()));
    }

    #[test]
    fn test_parse_detects_effects() {
        let parser = ComponentParser::new();
        let source = r#"
            export default async function App() {
                const [count] = useState(0);
                const response = await fetch('/api/data');
                console.log(response, count);
                return <main>{count}</main>;
            }
        "#;

        let components = parser.parse_source(source, "test.jsx").unwrap();
        let component = &components[0];
        assert!(component.effect_profile.hooks);
        assert!(component.effect_profile.asynchronous);
        assert!(component.effect_profile.io);
        assert!(component.effect_profile.side_effects);
    }

    #[test]
    fn test_jsx_onclick_marks_interactive() {
        let parser = ComponentParser::new();
        // Named like a non-interactive component on purpose: detection must be
        // driven by the onClick handler, not the component name.
        let source = r#"
            export default function Panel() {
                const [count, setCount] = useState(0);
                return <div onClick={() => setCount(count + 1)}>{count}</div>;
            }
        "#;
        let components = parser.parse_source(source, "test.tsx").unwrap();
        assert!(components[0].is_interactive);
        assert!(components[0].effect_profile.hooks);
    }

    #[test]
    fn test_no_handler_is_not_interactive() {
        let parser = ComponentParser::new();
        // Named "Button" — the old heuristic would have flagged it interactive.
        let source = r#"
            export default function Button() {
                return <button class="btn">Static</button>;
            }
        "#;
        let components = parser.parse_source(source, "test.tsx").unwrap();
        assert!(!components[0].is_interactive);
    }

    #[test]
    fn test_form_server_action_is_not_interactive() {
        let parser = ComponentParser::new();
        // `action="action:…"` is a server action (round-trips → Tier-B), not an
        // `on*` handler, so it must NOT mark the component interactive.
        let source = r#"
            export default function ContactForm() {
                return <form action="action:submit"><input name="email" /></form>;
            }
        "#;
        let components = parser.parse_source(source, "test.tsx").unwrap();
        assert!(!components[0].is_interactive);
    }

    #[test]
    fn test_parse_produces_stable_source_hash() {
        let parser = ComponentParser::new();
        let source = "export default function App(){return <main/>;}";
        let first = parser.parse_source(source, "test.jsx").unwrap();
        let second = parser.parse_source(source, "test.jsx").unwrap();
        assert_eq!(first[0].source_hash, second[0].source_hash);
    }
}
