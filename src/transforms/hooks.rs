//! Phase K · hook extractor.
//!
//! Walks a parsed [`ComponentFunction`] body and surfaces every call to
//! `useState(...)` whose identifier traces back to a `react` import.
//! Returns one [`HookBinding`] per call in source order. The order is
//! the contract — `hook_idx` ties a binding to its slot allocation in
//! [`crate::runtime::compiled::HookSchema`].
//!
//! Phase K Stage 1 supports the canonical pattern:
//!
//! ```tsx
//! const [name, setName] = useState(initial);
//! ```
//!
//! Stages 2 (closures over props) and 3 (module constants) widen the
//! `initial` expression handling; this extractor surfaces the
//! expression verbatim and lets the renderer evaluate it via the
//! existing Phase-J interpreter.
//!
//! Misuse is rejected at extraction time (a `HookExtractError`):
//!   * `useState` inside a conditional branch → would change hook
//!     count across renders (React's Rules of Hooks).
//!   * Non-array, non-binding destructure pattern (e.g. `const x =
//!     useState(0)`) — supported as a single-binding form returning
//!     the tuple, but loses setter access; surfaced as a warning.
//!
//! The extractor is intentionally a pure function over the AST; it
//! does NOT allocate `SlotId`s. That happens in `compiled.rs` so the
//! allocator can be deterministic across compilations and reusable
//! when only some components have changed (Phase J incremental).

use crate::runtime::eval::{ComponentFunction, ImportBinding};
use std::collections::HashMap;
use swc_ecma_ast::{
    BlockStmtOrExpr, CallExpr, Callee, Decl, Expr, ExprStmt, ForStmt, IfStmt, Pat, Stmt,
    VarDeclarator,
};

/// One `useState` call extracted from a component body.
#[derive(Debug, Clone)]
pub struct HookBinding {
    /// Position of this hook among all `useState` calls in the
    /// component body, in source order. Stable across recompilations
    /// of the same source so the slot allocator can re-key.
    pub hook_idx: usize,
    /// The local name the read binding is destructured into
    /// (`const [name, _] = useState(...)` ⇒ `"name"`).
    pub value_name: String,
    /// The local name the setter is destructured into, if present.
    /// `None` when the pattern is `const x = useState(...)`.
    pub setter_name: Option<String>,
    /// The unevaluated initial-value expression. The renderer
    /// evaluates this lazily via the existing Phase J interpreter so
    /// it inherits the full expression-shape matrix (literals,
    /// `new Date()`, computed values, etc.).
    pub initial: Expr,
}

/// Failure modes the extractor refuses to compile.
#[derive(Debug)]
pub enum HookExtractError {
    /// `useState` appeared inside an `if`/`for`/`while` body — would
    /// change hook count between renders.
    HookInsideConditional { hook_idx_so_far: usize, location: String },
}

impl std::fmt::Display for HookExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HookExtractError::HookInsideConditional { hook_idx_so_far, location } => write!(
                f,
                "useState invoked inside a conditional ({location}); hooks must run unconditionally \
                 (would have been hook #{hook_idx_so_far} in source order)",
            ),
        }
    }
}

impl std::error::Error for HookExtractError {}

/// Extract every `useState` call from a component body in source order.
///
/// `imports` is the function's containing module's import map; the
/// extractor only recognises `useState` identifiers that resolve back
/// to a `react` import — anything else is treated as user code and
/// ignored.
pub fn extract_use_state_hooks(
    function: &ComponentFunction,
    imports: &HashMap<String, ImportBinding>,
) -> Result<Vec<HookBinding>, HookExtractError> {
    let mut out = Vec::new();
    for stmt in &function.body_stmts {
        visit_stmt_top_level(stmt, imports, &mut out)?;
    }
    Ok(out)
}

fn visit_stmt_top_level(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    out: &mut Vec<HookBinding>,
) -> Result<(), HookExtractError> {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for decl in &var.decls {
                try_extract_from_var_declarator(decl, imports, out);
            }
            Ok(())
        }
        // Hooks inside conditionals violate React's Rules of Hooks and
        // also our slot allocation contract — slot ids are positional,
        // so a hook that may or may not run would corrupt every
        // following allocation.
        Stmt::If(IfStmt { cons, alt, .. }) => {
            check_no_hook_calls_in_stmt(cons, imports, out.len())?;
            if let Some(alt) = alt {
                check_no_hook_calls_in_stmt(alt, imports, out.len())?;
            }
            Ok(())
        }
        Stmt::For(ForStmt { body, .. }) => {
            check_no_hook_calls_in_stmt(body, imports, out.len())
        }
        Stmt::While(node) => check_no_hook_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::DoWhile(node) => check_no_hook_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::Try(node) => {
            for inner in &node.block.stmts {
                check_no_hook_calls_in_stmt(inner, imports, out.len())?;
            }
            Ok(())
        }
        Stmt::Block(block) => {
            for inner in &block.stmts {
                visit_stmt_top_level(inner, imports, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn try_extract_from_var_declarator(
    decl: &VarDeclarator,
    imports: &HashMap<String, ImportBinding>,
    out: &mut Vec<HookBinding>,
) {
    let Some(init) = &decl.init else { return };
    let Expr::Call(call) = init.as_ref() else { return };
    if !is_use_state_call(call, imports) {
        return;
    }
    let initial = call
        .args
        .first()
        .map(|spread| (*spread.expr).clone())
        .unwrap_or_else(|| Expr::Lit(swc_ecma_ast::Lit::Null(swc_ecma_ast::Null {
            span: Default::default(),
        })));
    let (value_name, setter_name) = destructure_names(&decl.name);
    let Some(value_name) = value_name else { return };

    out.push(HookBinding {
        hook_idx: out.len(),
        value_name,
        setter_name,
        initial,
    });
}

fn destructure_names(pat: &Pat) -> (Option<String>, Option<String>) {
    match pat {
        // `const [value, setter] = useState(...)`
        Pat::Array(array) => {
            let first = array
                .elems
                .first()
                .and_then(|opt| opt.as_ref())
                .and_then(pat_to_ident_name);
            let second = array
                .elems
                .get(1)
                .and_then(|opt| opt.as_ref())
                .and_then(pat_to_ident_name);
            (first, second)
        }
        // `const x = useState(...)` — single-binding form (rare; kept
        // for grace, no setter accessible).
        Pat::Ident(ident) => (Some(ident.id.sym.to_string()), None),
        _ => (None, None),
    }
}

fn pat_to_ident_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(ident) => Some(ident.id.sym.to_string()),
        _ => None,
    }
}

fn is_use_state_call(call: &CallExpr, imports: &HashMap<String, ImportBinding>) -> bool {
    let Callee::Expr(callee) = &call.callee else { return false };
    let Expr::Ident(ident) = callee.as_ref() else { return false };
    let name = ident.sym.to_string();
    if name != "useState" {
        return false;
    }
    imports
        .get(&name)
        .map(|b| b.source == "react" && b.export_name == "useState")
        .unwrap_or(false)
}

fn check_no_hook_calls_in_stmt(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    hook_idx_so_far: usize,
) -> Result<(), HookExtractError> {
    let location = match stmt {
        Stmt::If(_) => "inside if-statement body",
        Stmt::For(_) => "inside for-loop body",
        Stmt::While(_) => "inside while-loop body",
        Stmt::DoWhile(_) => "inside do-while-loop body",
        Stmt::Block(_) => "inside nested block",
        _ => "inside conditional path",
    };
    if stmt_contains_hook_call(stmt, imports) {
        return Err(HookExtractError::HookInsideConditional {
            hook_idx_so_far,
            location: location.to_string(),
        });
    }
    Ok(())
}

fn stmt_contains_hook_call(stmt: &Stmt, imports: &HashMap<String, ImportBinding>) -> bool {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => var.decls.iter().any(|d| {
            d.init
                .as_ref()
                .map(|e| expr_contains_hook_call(e, imports))
                .unwrap_or(false)
        }),
        Stmt::Expr(ExprStmt { expr, .. }) => expr_contains_hook_call(expr, imports),
        Stmt::Block(block) => block.stmts.iter().any(|s| stmt_contains_hook_call(s, imports)),
        Stmt::If(IfStmt { cons, alt, .. }) => {
            stmt_contains_hook_call(cons, imports)
                || alt.as_ref().map(|a| stmt_contains_hook_call(a, imports)).unwrap_or(false)
        }
        Stmt::For(ForStmt { body, .. }) => stmt_contains_hook_call(body, imports),
        Stmt::While(node) => stmt_contains_hook_call(&node.body, imports),
        Stmt::DoWhile(node) => stmt_contains_hook_call(&node.body, imports),
        Stmt::Return(node) => node
            .arg
            .as_ref()
            .map(|e| expr_contains_hook_call(e, imports))
            .unwrap_or(false),
        _ => false,
    }
}

fn expr_contains_hook_call(expr: &Expr, imports: &HashMap<String, ImportBinding>) -> bool {
    match expr {
        Expr::Call(call) => {
            if is_use_state_call(call, imports) {
                return true;
            }
            call.args.iter().any(|a| expr_contains_hook_call(&a.expr, imports))
        }
        Expr::Bin(b) => expr_contains_hook_call(&b.left, imports) || expr_contains_hook_call(&b.right, imports),
        Expr::Cond(c) => {
            expr_contains_hook_call(&c.test, imports)
                || expr_contains_hook_call(&c.cons, imports)
                || expr_contains_hook_call(&c.alt, imports)
        }
        Expr::Paren(p) => expr_contains_hook_call(&p.expr, imports),
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => {
                block.stmts.iter().any(|s| stmt_contains_hook_call(s, imports))
            }
            BlockStmtOrExpr::Expr(e) => expr_contains_hook_call(e, imports),
        },
        _ => false,
    }
}
