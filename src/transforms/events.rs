//! Phase K · JSX event-handler extractor.
//!
//! Walks the JSX inside a component body and surfaces every `on*`
//! attribute whose value is a JSX expression container holding a
//! function expression. Each surfaced site becomes one
//! [`HandlerExtract`].
//!
//! The extractor is structural — it returns the handler in source
//! order with the AST of the function body intact, so the renderer
//! can:
//!
//!   1. Compute a deterministic `proxy_id` from `(module, function,
//!      handler_idx)` and emit a `BindEvent` opcode for the
//!      containing element.
//!
//!   2. Register a server-side dispatcher that re-executes the same
//!      AST against the session slot store when bakabox POSTs to
//!      `/_albedo/action/<proxy_id>`.
//!
//! Phase K Stage 1 supports the canonical inline-arrow pattern:
//!
//! ```tsx
//! <button onClick={() => setN(n + 1)}>...</button>
//! ```
//!
//! The body can be either a single expression (arrow with
//! `BlockStmtOrExpr::Expr`) or a block of statements containing
//! `setN(...)` calls; both are evaluated server-side via the existing
//! Phase J interpreter.

use std::collections::HashSet;
use swc_ecma_ast::{
    BlockStmtOrExpr, Callee, Decl, Expr, ExprStmt, JSXAttrName, JSXAttrOrSpread, JSXAttrValue,
    JSXElement, JSXElementChild, JSXExpr, MemberProp, Pat, Stmt,
};

/// One JSX `on*` handler extracted from a component body.
#[derive(Debug, Clone)]
pub struct HandlerExtract {
    /// Position among all handlers in this component in source order.
    pub handler_idx: usize,
    /// The DOM event name (lowercased, with the `on` prefix
    /// stripped). `onClick` → `"click"`, `onSubmit` → `"submit"`.
    pub event_name: String,
    /// The handler closure body as an AST. Either the expression form
    /// (single-expression arrow) or the block form (lifted into one
    /// statement vector). Server-side dispatch evaluates this via the
    /// shared Phase J interpreter, with setter identifiers bound to
    /// slot-write actions.
    pub body: HandlerBody,
}

#[derive(Debug, Clone)]
pub enum HandlerBody {
    /// Single-expression arrow: `() => setN(n + 1)`.
    Expr(Expr),
    /// Block-bodied arrow or function expression.
    Block(Vec<Stmt>),
}

/// Walk every JSX element in a component function body and extract
/// the `on*` handlers attached to host (lowercase-tag) elements. The
/// returned vector is in source-traversal order; handler_idx is the
/// vector index.
pub fn extract_handlers_in_function(stmts: &[Stmt]) -> Vec<HandlerExtract> {
    let mut sink = Vec::new();
    for stmt in stmts {
        visit_stmt_for_jsx(stmt, &mut sink);
    }
    sink
}

fn visit_stmt_for_jsx(stmt: &Stmt, sink: &mut Vec<HandlerExtract>) {
    match stmt {
        Stmt::Return(ret) => {
            if let Some(arg) = &ret.arg {
                visit_expr_for_jsx(arg, sink);
            }
        }
        Stmt::Expr(es) => visit_expr_for_jsx(&es.expr, sink),
        Stmt::Block(block) => {
            for s in &block.stmts {
                visit_stmt_for_jsx(s, sink);
            }
        }
        Stmt::Decl(swc_ecma_ast::Decl::Var(var)) => {
            for d in &var.decls {
                if let Some(init) = &d.init {
                    visit_expr_for_jsx(init, sink);
                }
            }
        }
        _ => {}
    }
}

fn visit_expr_for_jsx(expr: &Expr, sink: &mut Vec<HandlerExtract>) {
    match expr {
        Expr::JSXElement(element) => visit_element(element, sink),
        Expr::JSXFragment(fragment) => {
            for child in &fragment.children {
                visit_child(child, sink);
            }
        }
        Expr::Paren(paren) => visit_expr_for_jsx(&paren.expr, sink),
        Expr::Cond(c) => {
            visit_expr_for_jsx(&c.cons, sink);
            visit_expr_for_jsx(&c.alt, sink);
        }
        _ => {}
    }
}

fn visit_element(element: &JSXElement, sink: &mut Vec<HandlerExtract>) {
    for attr in &element.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(attr) = attr else { continue };
        let JSXAttrName::Ident(name_ident) = &attr.name else { continue };
        let name = name_ident.sym.to_string();
        if !name.starts_with("on") || name.len() <= 2 {
            continue;
        }
        let event_name = name[2..].to_ascii_lowercase();
        let Some(JSXAttrValue::JSXExprContainer(container)) = &attr.value else { continue };
        let JSXExpr::Expr(handler_expr) = &container.expr else { continue };
        let body = match handler_expr.as_ref() {
            Expr::Arrow(arrow) => match &*arrow.body {
                BlockStmtOrExpr::BlockStmt(block) => HandlerBody::Block(block.stmts.clone()),
                BlockStmtOrExpr::Expr(expr) => HandlerBody::Expr((**expr).clone()),
            },
            Expr::Fn(fn_expr) => match fn_expr.function.body.as_ref() {
                Some(block) => HandlerBody::Block(block.stmts.clone()),
                None => continue,
            },
            // Bare identifiers (`onClick={handleClick}`) are deferred
            // to Phase K Stage 2 — they need handler-resolution against
            // the surrounding scope.
            _ => continue,
        };
        let handler_idx = sink.len();
        sink.push(HandlerExtract { handler_idx, event_name, body });
    }
    for child in &element.children {
        visit_child(child, sink);
    }
}

fn visit_child(child: &JSXElementChild, sink: &mut Vec<HandlerExtract>) {
    match child {
        JSXElementChild::JSXElement(element) => visit_element(element, sink),
        JSXElementChild::JSXFragment(fragment) => {
            for c in &fragment.children {
                visit_child(c, sink);
            }
        }
        JSXElementChild::JSXExprContainer(container) => {
            if let JSXExpr::Expr(expr) = &container.expr {
                visit_expr_for_jsx(expr, sink);
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────
// Stage 2 · free-variable collector for handler bodies
//
// `collect_free_idents_in_handler_body` walks a [`HandlerBody`] and
// surfaces every identifier reference that is NOT introduced by a
// binding inside the body itself. The result is the set the
// CompiledProject filters against (slot reads, setter calls, captured
// props, captured module constants) to decide how each name should
// resolve at handler-invoke time.
//
// Stage 2 scope: only counts free identifiers that appear as direct
// references — member access (`obj.prop`) contributes only `obj`, not
// `prop`. Nested arrows/functions push local bindings onto a small
// scope stack so their params don't leak into the parent's free set.
// Var decls inside blocks also introduce locals.
//
// This is deliberately a small surface — for Stage 2 / 3 we only
// need the **names** the handler refers to, not their types or
// dataflow. The runtime resolves each at invoke time.
// ─────────────────────────────────────────────────────────────────────

/// Collect every free identifier referenced by a handler body.
pub fn collect_free_idents_in_handler_body(body: &HandlerBody) -> HashSet<String> {
    let mut scope = ScopeStack::new();
    let mut sink: HashSet<String> = HashSet::new();
    match body {
        HandlerBody::Expr(expr) => collect_in_expr(expr, &mut scope, &mut sink),
        HandlerBody::Block(stmts) => {
            for stmt in stmts {
                collect_in_stmt(stmt, &mut scope, &mut sink);
            }
        }
    }
    sink
}

#[derive(Default)]
struct ScopeStack {
    frames: Vec<HashSet<String>>,
}

impl ScopeStack {
    fn new() -> Self {
        Self::default()
    }
    fn push(&mut self) {
        self.frames.push(HashSet::new());
    }
    fn pop(&mut self) {
        self.frames.pop();
    }
    fn bind(&mut self, name: String) {
        if let Some(frame) = self.frames.last_mut() {
            frame.insert(name);
        }
    }
    fn contains(&self, name: &str) -> bool {
        self.frames.iter().any(|frame| frame.contains(name))
    }
}

fn collect_in_stmt(stmt: &Stmt, scope: &mut ScopeStack, sink: &mut HashSet<String>) {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for decl in &var.decls {
                if let Some(init) = &decl.init {
                    collect_in_expr(init, scope, sink);
                }
                bind_pat(&decl.name, scope);
            }
        }
        Stmt::Expr(ExprStmt { expr, .. }) => collect_in_expr(expr, scope, sink),
        Stmt::Return(ret) => {
            if let Some(arg) = &ret.arg {
                collect_in_expr(arg, scope, sink);
            }
        }
        Stmt::Block(block) => {
            scope.push();
            for s in &block.stmts {
                collect_in_stmt(s, scope, sink);
            }
            scope.pop();
        }
        Stmt::If(node) => {
            collect_in_expr(&node.test, scope, sink);
            collect_in_stmt(&node.cons, scope, sink);
            if let Some(alt) = &node.alt {
                collect_in_stmt(alt, scope, sink);
            }
        }
        _ => {}
    }
}

fn collect_in_expr(expr: &Expr, scope: &mut ScopeStack, sink: &mut HashSet<String>) {
    match expr {
        Expr::Ident(ident) => {
            let name = ident.sym.to_string();
            if !scope.contains(&name) {
                sink.insert(name);
            }
        }
        Expr::Call(call) => {
            if let Callee::Expr(callee) = &call.callee {
                collect_in_expr(callee, scope, sink);
            }
            for arg in &call.args {
                collect_in_expr(&arg.expr, scope, sink);
            }
        }
        Expr::Member(member) => {
            // Only the object side contributes a free name; the
            // property side is a static identifier in JS object space.
            collect_in_expr(&member.obj, scope, sink);
            if let MemberProp::Computed(computed) = &member.prop {
                collect_in_expr(&computed.expr, scope, sink);
            }
        }
        Expr::Bin(bin) => {
            collect_in_expr(&bin.left, scope, sink);
            collect_in_expr(&bin.right, scope, sink);
        }
        Expr::Unary(unary) => collect_in_expr(&unary.arg, scope, sink),
        Expr::Cond(cond) => {
            collect_in_expr(&cond.test, scope, sink);
            collect_in_expr(&cond.cons, scope, sink);
            collect_in_expr(&cond.alt, scope, sink);
        }
        Expr::Paren(paren) => collect_in_expr(&paren.expr, scope, sink),
        Expr::Tpl(tpl) => {
            for e in &tpl.exprs {
                collect_in_expr(e, scope, sink);
            }
        }
        Expr::Array(array) => {
            for elem in array.elems.iter().flatten() {
                collect_in_expr(&elem.expr, scope, sink);
            }
        }
        Expr::Object(object) => {
            for prop in &object.props {
                if let swc_ecma_ast::PropOrSpread::Prop(p) = prop {
                    if let swc_ecma_ast::Prop::KeyValue(kv) = p.as_ref() {
                        collect_in_expr(&kv.value, scope, sink);
                    }
                }
            }
        }
        Expr::Arrow(arrow) => {
            scope.push();
            for param in &arrow.params {
                bind_pat(param, scope);
            }
            match &*arrow.body {
                BlockStmtOrExpr::Expr(e) => collect_in_expr(e, scope, sink),
                BlockStmtOrExpr::BlockStmt(b) => {
                    for s in &b.stmts {
                        collect_in_stmt(s, scope, sink);
                    }
                }
            }
            scope.pop();
        }
        Expr::Fn(fn_expr) => {
            scope.push();
            for param in &fn_expr.function.params {
                bind_pat(&param.pat, scope);
            }
            if let Some(body) = &fn_expr.function.body {
                for s in &body.stmts {
                    collect_in_stmt(s, scope, sink);
                }
            }
            scope.pop();
        }
        Expr::New(new_expr) => {
            collect_in_expr(&new_expr.callee, scope, sink);
            if let Some(args) = &new_expr.args {
                for arg in args {
                    collect_in_expr(&arg.expr, scope, sink);
                }
            }
        }
        Expr::OptChain(opt) => match &*opt.base {
            swc_ecma_ast::OptChainBase::Member(m) => {
                collect_in_expr(&m.obj, scope, sink);
            }
            swc_ecma_ast::OptChainBase::Call(c) => {
                collect_in_expr(&c.callee, scope, sink);
                for arg in &c.args {
                    collect_in_expr(&arg.expr, scope, sink);
                }
            }
        },
        Expr::TsAs(node) => collect_in_expr(&node.expr, scope, sink),
        Expr::TsNonNull(node) => collect_in_expr(&node.expr, scope, sink),
        Expr::TsConstAssertion(node) => collect_in_expr(&node.expr, scope, sink),
        Expr::TsTypeAssertion(node) => collect_in_expr(&node.expr, scope, sink),
        _ => {}
    }
}

fn bind_pat(pat: &Pat, scope: &mut ScopeStack) {
    match pat {
        Pat::Ident(ident) => scope.bind(ident.id.sym.to_string()),
        Pat::Array(array) => {
            for elem in array.elems.iter().flatten() {
                bind_pat(elem, scope);
            }
        }
        Pat::Object(object) => {
            for prop in &object.props {
                match prop {
                    swc_ecma_ast::ObjectPatProp::Assign(assign) => {
                        scope.bind(assign.key.sym.to_string());
                    }
                    swc_ecma_ast::ObjectPatProp::KeyValue(kv) => {
                        bind_pat(&kv.value, scope);
                    }
                    swc_ecma_ast::ObjectPatProp::Rest(rest) => {
                        bind_pat(&rest.arg, scope);
                    }
                }
            }
        }
        _ => {}
    }
}
