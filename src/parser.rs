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

        Ok(visitor.components)
    }
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
    pub source_hash: u64,
}

struct ComponentVisitor {
    file_path: String,
    source_hash: u64,
    components: Vec<ParsedComponent>,
    current_imports: Vec<String>,
}

impl ComponentVisitor {
    fn new(file_path: String, source_hash: u64) -> Self {
        Self {
            file_path,
            source_hash,
            components: Vec::new(),
            current_imports: Vec::new(),
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
        if let Some(body) = &function.body {
            collector.prime_local_defs(&body.stmts);
            body.visit_with(&mut collector);
        }
        collector.finish()
    }

    fn analyze_arrow(&self, arrow: &ArrowExpr) -> ComponentAnalysis {
        let mut collector = EffectCollector::default();
        if arrow.is_async {
            collector.profile.asynchronous = true;
        }
        match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => {
                collector.prime_local_defs(&block.stmts);
                block.visit_with(&mut collector);
            }
            BlockStmtOrExpr::Expr(expr) => expr.visit_with(&mut collector),
        }
        collector.finish()
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
            estimated_size,
            is_default_export,
            props: Vec::new(),
            effect_profile: analysis.profile,
            is_interactive: analysis.is_interactive,
            is_client_interactive: analysis.is_client_interactive,
            source_hash: self.source_hash,
        });
    }
}

impl Visit for ComponentVisitor {
    fn visit_import_decl(&mut self, import: &ImportDecl) {
        for spec in &import.specifiers {
            let name = match spec {
                ImportSpecifier::Named(n) => n.local.sym.to_string(),
                ImportSpecifier::Default(d) => d.local.sym.to_string(),
                ImportSpecifier::Namespace(n) => n.local.sym.to_string(),
            };
            self.current_imports.push(name);
        }
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
        if let Some(name) = self.extract_component_name(&export.expr) {
            let estimated_size = name.len() * 50 + 200;
            let analysis = self.analyze_expr(&export.expr);

            if let Some(comp) = self.components.iter_mut().find(|c| c.name == name) {
                comp.is_default_export = true;
                comp.effect_profile = comp.effect_profile.join(analysis.profile);
                comp.is_interactive = comp.is_interactive || analysis.is_interactive;
                comp.is_client_interactive =
                    comp.is_client_interactive || analysis.is_client_interactive;
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
}

#[derive(Default)]
struct EffectCollector {
    profile: EffectProfile,
    /// Per-component map: local function/const name -> client-safe (its body
    /// reaches no server boundary, transitively via other locals). Primed from
    /// the component body before the main walk so handler references such as
    /// `onClick={inc}` can be resolved against it.
    local_safety: HashMap<String, bool>,
    /// Saw at least one `on*` JSX handler prop.
    has_handler: bool,
    /// Saw at least one provably client-satisfiable `on*` handler.
    has_client_handler: bool,
}

impl EffectCollector {
    fn finish(self) -> ComponentAnalysis {
        ComponentAnalysis {
            profile: self.profile,
            is_interactive: self.has_handler,
            is_client_interactive: self.has_client_handler,
        }
    }

    /// Collect local `const NAME = closure` / `function NAME` definitions from
    /// the component body and classify each as client-safe (its body reaches no
    /// network/server boundary, transitively through other locals). This lets a
    /// handler reference like `onClick={inc}` resolve to `inc`'s analysis.
    fn prime_local_defs(&mut self, stmts: &[Stmt]) {
        // direct[name] = body directly contains a server-boundary io call.
        // calls[name]  = function names this def invokes (for transitivity).
        let mut direct: HashMap<String, bool> = HashMap::new();
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
                    calls.insert(name, scan.called);
                }
                Stmt::Decl(Decl::Var(var)) => {
                    for decl in &var.decls {
                        if let Some(name) = decl.name.as_ident().map(|i| i.sym.to_string()) {
                            if let Some(init) = &decl.init {
                                if let Some((found_io, called)) = scan_closure(init) {
                                    direct.insert(name.clone(), found_io);
                                    calls.insert(name, called);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Fixpoint: a def is unsafe if it directly hits a boundary, or it calls
        // a local def already known to be unsafe.
        let mut unsafe_defs: HashSet<String> = direct
            .iter()
            .filter(|(_, io)| **io)
            .map(|(name, _)| name.clone())
            .collect();
        loop {
            let mut changed = false;
            for (name, callees) in &calls {
                if unsafe_defs.contains(name) {
                    continue;
                }
                if callees.iter().any(|c| unsafe_defs.contains(c)) {
                    unsafe_defs.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        self.local_safety = direct
            .keys()
            .map(|name| (name.clone(), !unsafe_defs.contains(name)))
            .collect();
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

/// Returns `(direct_io, called_names)` for a closure expression, or `None` when
/// the initializer is not a function (so it is not a callable local handler).
fn scan_closure(expr: &Expr) -> Option<(bool, Vec<String>)> {
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
    Some((scan.found_io, scan.called))
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
