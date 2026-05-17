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

use swc_ecma_ast::{
    BlockStmtOrExpr, Expr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElement,
    JSXElementChild, JSXExpr, Stmt,
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
