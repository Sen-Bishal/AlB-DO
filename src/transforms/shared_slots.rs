//! Phase O.2 · `useSharedSlot` extractor.
//!
//! Walks a parsed component body and surfaces every call to
//! `useSharedSlot<T>("topic")`. Returns one [`SharedSlotBinding`]
//! per call in source order, mirroring the structural contract the
//! Phase K [`crate::transforms::hooks`] extractor established.
//!
//! ## Surface
//!
//! ```tsx
//! const messages = useSharedSlot<Message[]>("chat:room-42");
//! ```
//!
//! Unlike `useState`, the hook returns a **single read binding** — no
//! setter pair. Writes to a shared slot are authored server-side via
//! [`crate::runtime::BroadcastRegistry::write_topic`] from an action
//! handler. This matches the framework's broader pattern: events
//! travel client→server, writes happen server-side, the bakabox
//! client just consumes `SlotSet` opcodes off the WT patches lane.
//!
//! ## Rejection rules
//!
//! - `useSharedSlot` inside a conditional / loop body → would change
//!   the binding count across renders; bakabox cannot align slots
//!   stably so we refuse at compile time.
//! - Missing argument → no topic to subscribe to.
//! - Non-string-literal argument → the topic is what derives the
//!   wire-level slot id at compile time; a dynamic expression makes
//!   the slot id non-deterministic across builds.
//! - Non-identifier destructure pattern → no name to bind the read
//!   value to. (`const { foo } = useSharedSlot(...)` is unsupported;
//!   only `const x = useSharedSlot(...)` matches.)
//!
//! ## Import binding contract
//!
//! Only `useSharedSlot` symbols imported from `albedo` (or the
//! ambient global type surface emitted by Phase M.3) are recognised.
//! A user-defined function literally named `useSharedSlot` shadowing
//! the framework export is skipped — this matches the
//! `extract_use_state_hooks` rule that pins identifiers to their
//! `react` import.

use crate::runtime::eval::{ComponentFunction, ImportBinding};
use std::collections::HashMap;
use swc_ecma_ast::{
    BlockStmtOrExpr, CallExpr, Callee, Decl, Expr, ExprStmt, ForStmt, IfStmt, Lit, Pat, Stmt,
    VarDeclarator,
};

/// One `useSharedSlot` call extracted from a component body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedSlotBinding {
    /// Position among all `useSharedSlot` calls in the component body,
    /// in source order. Stable across recompilations of the same
    /// source. Use this to pair extractor output with later passes.
    pub hook_idx: usize,
    /// The local name the read binding is assigned to.
    pub binding_name: String,
    /// The literal topic key the user passed. The wire slot id is
    /// derived from this via [`crate::runtime::broadcast_slot_id`] —
    /// `"broadcast::{topic}"` hashed FNV-1a-32. The extractor does
    /// not compute the slot id; that's the broadcast registry's
    /// responsibility so the same hash function ships everywhere.
    pub topic: String,
}

/// Failure modes refused at compile time. Surfaced verbatim through
/// [`crate::runtime::compiled::CompiledProject::wrap`] so misuse
/// stops the build instead of slipping into runtime.
#[derive(Debug, PartialEq, Eq)]
pub enum SharedSlotExtractError {
    HookInsideConditional { hook_idx_so_far: usize, location: String },
    MissingTopicArgument { binding_name: Option<String> },
    NonStringLiteralTopic { binding_name: Option<String> },
    UnsupportedDestructurePattern,
}

impl std::fmt::Display for SharedSlotExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HookInsideConditional { hook_idx_so_far, location } => write!(
                f,
                "useSharedSlot invoked inside a conditional ({location}); hooks must run \
                 unconditionally (would have been call #{hook_idx_so_far} in source order)",
            ),
            Self::MissingTopicArgument { binding_name } => write!(
                f,
                "useSharedSlot{} requires a string-literal topic argument",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::NonStringLiteralTopic { binding_name } => write!(
                f,
                "useSharedSlot{} topic must be a string literal so the wire slot id is \
                 deterministic at build time",
                binding_name
                    .as_deref()
                    .map(|name| format!(" assigned to '{name}'"))
                    .unwrap_or_default(),
            ),
            Self::UnsupportedDestructurePattern => f.write_str(
                "useSharedSlot must bind to a single identifier (e.g. `const x = useSharedSlot(\"topic\")`)",
            ),
        }
    }
}

impl std::error::Error for SharedSlotExtractError {}

/// Extract every `useSharedSlot` call from a component body in
/// source-traversal order.
///
/// `imports` is the function's containing module's import map; only
/// identifiers that resolve back to an `albedo` import named
/// `useSharedSlot` are recognised. Anything else is treated as user
/// code and ignored.
pub fn extract_shared_slot_hooks(
    function: &ComponentFunction,
    imports: &HashMap<String, ImportBinding>,
) -> Result<Vec<SharedSlotBinding>, SharedSlotExtractError> {
    let mut out = Vec::new();
    for stmt in &function.body_stmts {
        visit_stmt_top_level(stmt, imports, &mut out)?;
    }
    Ok(out)
}

fn visit_stmt_top_level(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    out: &mut Vec<SharedSlotBinding>,
) -> Result<(), SharedSlotExtractError> {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => {
            for decl in &var.decls {
                try_extract_from_var_declarator(decl, imports, out)?;
            }
            Ok(())
        }
        Stmt::If(IfStmt { cons, alt, .. }) => {
            check_no_shared_slot_calls_in_stmt(cons, imports, out.len())?;
            if let Some(alt) = alt {
                check_no_shared_slot_calls_in_stmt(alt, imports, out.len())?;
            }
            Ok(())
        }
        Stmt::For(ForStmt { body, .. }) => {
            check_no_shared_slot_calls_in_stmt(body, imports, out.len())
        }
        Stmt::While(node) => check_no_shared_slot_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::DoWhile(node) => check_no_shared_slot_calls_in_stmt(&node.body, imports, out.len()),
        Stmt::Try(node) => {
            for inner in &node.block.stmts {
                check_no_shared_slot_calls_in_stmt(inner, imports, out.len())?;
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
    out: &mut Vec<SharedSlotBinding>,
) -> Result<(), SharedSlotExtractError> {
    let Some(init) = &decl.init else { return Ok(()) };
    let Expr::Call(call) = init.as_ref() else { return Ok(()) };
    if !is_use_shared_slot_call(call, imports) {
        return Ok(());
    }

    let binding_name = match &decl.name {
        Pat::Ident(ident) => Some(ident.id.sym.to_string()),
        _ => None,
    };

    let topic = extract_string_topic(call, &binding_name)?;
    let binding_name = binding_name.ok_or(SharedSlotExtractError::UnsupportedDestructurePattern)?;

    out.push(SharedSlotBinding {
        hook_idx: out.len(),
        binding_name,
        topic,
    });
    Ok(())
}

fn extract_string_topic(
    call: &CallExpr,
    binding_name: &Option<String>,
) -> Result<String, SharedSlotExtractError> {
    let first = call.args.first().ok_or_else(|| {
        SharedSlotExtractError::MissingTopicArgument { binding_name: binding_name.clone() }
    })?;
    let expr = unwrap_parens(&first.expr);
    let Expr::Lit(Lit::Str(s)) = expr else {
        return Err(SharedSlotExtractError::NonStringLiteralTopic {
            binding_name: binding_name.clone(),
        });
    };
    Ok(s.value.to_string())
}

/// Peel `(expr)` wrappers so `useSharedSlot(("topic"))` still
/// extracts cleanly. SWC also surfaces `TsAs` / `TsSatisfies` wrappers
/// when the user writes `useSharedSlot("t" as const)`; those pass
/// through too.
fn unwrap_parens(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(p) => unwrap_parens(&p.expr),
        Expr::TsAs(ts_as) => unwrap_parens(&ts_as.expr),
        Expr::TsSatisfies(ts_sat) => unwrap_parens(&ts_sat.expr),
        Expr::TsConstAssertion(ts_const) => unwrap_parens(&ts_const.expr),
        other => other,
    }
}

fn is_use_shared_slot_call(call: &CallExpr, imports: &HashMap<String, ImportBinding>) -> bool {
    let Callee::Expr(callee) = &call.callee else { return false };
    let Expr::Ident(ident) = callee.as_ref() else { return false };
    let name = ident.sym.to_string();
    if name != "useSharedSlot" {
        return false;
    }
    imports
        .get(&name)
        .map(|b| b.source == "albedo" && b.export_name == "useSharedSlot")
        .unwrap_or(false)
}

fn check_no_shared_slot_calls_in_stmt(
    stmt: &Stmt,
    imports: &HashMap<String, ImportBinding>,
    hook_idx_so_far: usize,
) -> Result<(), SharedSlotExtractError> {
    let location = match stmt {
        Stmt::If(_) => "inside if-statement body",
        Stmt::For(_) => "inside for-loop body",
        Stmt::While(_) => "inside while-loop body",
        Stmt::DoWhile(_) => "inside do-while-loop body",
        Stmt::Block(_) => "inside nested block",
        _ => "inside conditional path",
    };
    if stmt_contains_shared_slot_call(stmt, imports) {
        return Err(SharedSlotExtractError::HookInsideConditional {
            hook_idx_so_far,
            location: location.to_string(),
        });
    }
    Ok(())
}

fn stmt_contains_shared_slot_call(stmt: &Stmt, imports: &HashMap<String, ImportBinding>) -> bool {
    match stmt {
        Stmt::Decl(Decl::Var(var)) => var.decls.iter().any(|d| {
            d.init
                .as_ref()
                .map(|e| expr_contains_shared_slot_call(e, imports))
                .unwrap_or(false)
        }),
        Stmt::Expr(ExprStmt { expr, .. }) => expr_contains_shared_slot_call(expr, imports),
        Stmt::Block(block) => block
            .stmts
            .iter()
            .any(|s| stmt_contains_shared_slot_call(s, imports)),
        Stmt::If(IfStmt { cons, alt, .. }) => {
            stmt_contains_shared_slot_call(cons, imports)
                || alt
                    .as_ref()
                    .map(|a| stmt_contains_shared_slot_call(a, imports))
                    .unwrap_or(false)
        }
        Stmt::For(ForStmt { body, .. }) => stmt_contains_shared_slot_call(body, imports),
        Stmt::While(node) => stmt_contains_shared_slot_call(&node.body, imports),
        Stmt::DoWhile(node) => stmt_contains_shared_slot_call(&node.body, imports),
        Stmt::Return(node) => node
            .arg
            .as_ref()
            .map(|e| expr_contains_shared_slot_call(e, imports))
            .unwrap_or(false),
        _ => false,
    }
}

fn expr_contains_shared_slot_call(expr: &Expr, imports: &HashMap<String, ImportBinding>) -> bool {
    match expr {
        Expr::Call(call) => {
            if is_use_shared_slot_call(call, imports) {
                return true;
            }
            call.args
                .iter()
                .any(|a| expr_contains_shared_slot_call(&a.expr, imports))
        }
        Expr::Bin(b) => {
            expr_contains_shared_slot_call(&b.left, imports)
                || expr_contains_shared_slot_call(&b.right, imports)
        }
        Expr::Cond(c) => {
            expr_contains_shared_slot_call(&c.test, imports)
                || expr_contains_shared_slot_call(&c.cons, imports)
                || expr_contains_shared_slot_call(&c.alt, imports)
        }
        Expr::Paren(p) => expr_contains_shared_slot_call(&p.expr, imports),
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::BlockStmt(block) => block
                .stmts
                .iter()
                .any(|s| stmt_contains_shared_slot_call(s, imports)),
            BlockStmtOrExpr::Expr(e) => expr_contains_shared_slot_call(e, imports),
        },
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::expr::parse_module;
    use crate::runtime::eval::ParsedModule;
    use std::path::Path;

    /// Compile a TSX fragment to a `ParsedModule` and return the
    /// component function named `Component`.
    fn parse(source: &str) -> ParsedModule {
        parse_module(source, Path::new("test_module.tsx")).expect("parse")
    }

    fn function_named<'a>(module: &'a ParsedModule, name: &str) -> &'a ComponentFunction {
        module.functions.get(name).expect("function present")
    }

    fn extract_or_panic(source: &str) -> Vec<SharedSlotBinding> {
        let parsed = parse(source);
        let function = function_named(&parsed, "Component");
        extract_shared_slot_hooks(function, &parsed.imports).expect("extraction")
    }

    fn extract_err(source: &str) -> SharedSlotExtractError {
        let parsed = parse(source);
        let function = function_named(&parsed, "Component");
        extract_shared_slot_hooks(function, &parsed.imports).expect_err("expected extraction error")
    }

    #[test]
    fn extracts_single_call_with_topic_and_binding_name() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const messages = useSharedSlot("chat:room-42");
                return <ul>{messages}</ul>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].hook_idx, 0);
        assert_eq!(bindings[0].binding_name, "messages");
        assert_eq!(bindings[0].topic, "chat:room-42");
    }

    #[test]
    fn extracts_multiple_calls_in_source_order() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const a = useSharedSlot("topic-a");
                const b = useSharedSlot("topic-b");
                return <div>{a}{b}</div>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].topic, "topic-a");
        assert_eq!(bindings[0].hook_idx, 0);
        assert_eq!(bindings[1].topic, "topic-b");
        assert_eq!(bindings[1].hook_idx, 1);
    }

    #[test]
    fn ignores_calls_to_unrelated_use_shared_slot_imports() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "some-other-lib";
            export default function Component() {
                const x = useSharedSlot("nope");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(bindings.is_empty());
    }

    #[test]
    fn ignores_calls_without_an_import_binding() {
        let bindings = extract_or_panic(
            r#"
            export default function Component() {
                const x = useSharedSlot("local");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(bindings.is_empty());
    }

    #[test]
    fn rejects_call_inside_if_body() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                if (true) {
                    const x = useSharedSlot("conditional");
                }
                return <span/>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::HookInsideConditional { .. }));
    }

    #[test]
    fn rejects_call_inside_for_body() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                for (let i = 0; i < 3; i++) {
                    const x = useSharedSlot("loop");
                }
                return <span/>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::HookInsideConditional { .. }));
    }

    #[test]
    fn rejects_missing_topic_argument() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const x = useSharedSlot();
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::MissingTopicArgument { .. }));
    }

    #[test]
    fn rejects_non_string_literal_topic() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const t = "dynamic";
                const x = useSharedSlot(t);
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::NonStringLiteralTopic { .. }));
    }

    #[test]
    fn accepts_topic_wrapped_in_typescript_as_const_or_satisfies() {
        let bindings = extract_or_panic(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const a = useSharedSlot("a" as const);
                const b = useSharedSlot(("b"));
                return <div>{a}{b}</div>;
            }
            "#,
        );
        assert_eq!(bindings.len(), 2);
        assert_eq!(bindings[0].topic, "a");
        assert_eq!(bindings[1].topic, "b");
    }

    #[test]
    fn rejects_destructure_pattern() {
        let err = extract_err(
            r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const [x] = useSharedSlot("topic");
                return <span>{x}</span>;
            }
            "#,
        );
        assert!(matches!(err, SharedSlotExtractError::UnsupportedDestructurePattern));
    }

    #[test]
    fn extraction_is_deterministic_across_runs() {
        let source = r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const messages = useSharedSlot("chat:42");
                const cursors = useSharedSlot("cursors:doc-1");
                return <div>{messages}{cursors}</div>;
            }
        "#;
        let first = extract_or_panic(source);
        let second = extract_or_panic(source);
        assert_eq!(first, second);
    }
}
