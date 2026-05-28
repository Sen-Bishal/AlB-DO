//! Phase P · Stream C.1 — `action(<handler>)` extractor.
//!
//! Walks a parsed module's top-level constants and surfaces every
//! TS-side action declaration of the shape:
//!
//! ```tsx
//! import { action } from "albedo";
//!
//! export const post_chat_message = action(async ({ form, broadcast }) => {
//!   // ...
//! });
//! ```
//!
//! Each surfaced [`ActionDeclaration`] carries the user-supplied name,
//! the closure's params (reused via [`crate::runtime::eval::ParamBinding`]
//! so the existing param-binder dispatches them at invoke time), and
//! the body in the [`HandlerBody`] shape Phase K's [`eval_handler_body`]
//! already runs. The extractor itself is a pure metadata pass — no AST
//! mutation, no slot allocation. Naming + id assignment happens in
//! [`crate::runtime::compiled::CompiledProject::wrap`], which calls
//! this extractor once per module and registers each declaration as a
//! `ResolvedHandler` keyed by `FNV-1a-32(name)` (the same hash family
//! Phase L's `allocate_form_action_id` produces, so wire envelopes
//! round-trip between TS-side `action()` declarations and JSX
//! `<form action="action:NAME">` forms).
//!
//! The body itself is not interpreted until C.2 ships the `broadcast()`
//! builtin — for C.1 the extractor and the action registry land first
//! so the dispatch wiring is ready to receive C.2's interpreter
//! extensions without further schema churn.
//!
//! ## Import binding contract
//!
//! Only `action` symbols imported from `"albedo"` are recognised. A
//! user-defined function literally named `action` shadowing the
//! framework export is skipped — matches the
//! [`crate::transforms::shared_slots`] rule that pins identifiers to
//! their import source.
//!
//! ## Rejection rules
//!
//! - Missing handler argument → no body to invoke.
//! - Non-function handler argument → cannot be invoked as a closure.
//! - Duplicate action name within one module → `FNV-1a-32(name)` would
//!   collide on the wire and dispatch would silently route to whichever
//!   handler `wrap()` happened to insert last.

use crate::runtime::eval::expr::{param_from_pat, ParsedModule};
use crate::runtime::eval::{ImportBinding, ParamBinding};
use crate::transforms::events::HandlerBody;
use std::collections::HashMap;
use swc_ecma_ast::{ArrowExpr, BlockStmtOrExpr, CallExpr, Callee, Expr, Function};

/// One `export const NAME = action(<handler>)` declaration extracted
/// from a module's top-level constants.
#[derive(Debug, Clone)]
pub struct ActionDeclaration {
    /// User-supplied name from `export const NAME = action(...)`.
    /// `action_id = FNV-1a-32(name)` — same hash family as Phase L's
    /// [`crate::transforms::form::allocate_form_action_id`] so a
    /// `<form action="action:NAME">` JSX form and a TS-side
    /// `action()` declaration with the same name converge on the
    /// same wire id without per-project configuration.
    pub name: String,
    /// Position among action declarations in this module, in source
    /// order. Mirrors `hook_idx` / `handler_idx` conventions across
    /// the existing extractors.
    pub action_idx: usize,
    /// Param bindings on the handler closure. Empty for `() => ...`;
    /// one entry for `(arg) => ...`; multiple entries for the
    /// destructured form `({ form, broadcast }) => ...`. The
    /// dispatcher binds these from the `ActionEnvelope`'s payload
    /// at invoke time via [`crate::runtime::eval::bind_params`].
    pub params: Vec<ParamBinding>,
    /// Closure body, mirroring [`crate::transforms::events::HandlerExtract`]
    /// so the existing `eval_handler_body` interpreter runs the body
    /// unchanged once C.2 lands the `broadcast()` builtin.
    pub body: HandlerBody,
    /// True when the source declares the handler `async`. The
    /// interpreter treats async bodies as if they completed
    /// synchronously — `async` only matters when user code awaits
    /// actual I/O (database, HTTP), which happens above the
    /// interpreter via host calls. Stored so diagnostic tooling can
    /// surface async-vs-sync without re-parsing.
    pub is_async: bool,
}

/// Failure modes the extractor surfaces at compile time. Propagated
/// verbatim through [`crate::runtime::compiled::CompiledProject::wrap`]
/// so misuse fails the build instead of slipping into runtime.
#[derive(Debug, PartialEq, Eq)]
pub enum ActionExtractError {
    /// `action()` called with zero arguments.
    MissingHandlerArgument { name: String },
    /// `action(value)` where `value` isn't an arrow or function
    /// expression — e.g. a bare identifier reference.
    NonFunctionHandlerArgument { name: String },
    /// `function() {}` with no body — extremely unusual but the SWC
    /// AST allows it for ambient declarations, and we'd otherwise
    /// register a `ResolvedHandler` with no body to evaluate.
    MissingFunctionBody { name: String },
    /// Two `export const NAME = action(...)` declarations with the
    /// same `NAME` within one module. FNV-1a-32 would collide on the
    /// wire so the dispatcher would silently route to whichever the
    /// extractor saw last.
    DuplicateActionName { name: String, first_idx: usize, second_idx: usize },
}

impl std::fmt::Display for ActionExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHandlerArgument { name } => write!(
                f,
                "action declaration '{name}' is missing its handler argument; \
                 expected `action(<arrow>)`",
            ),
            Self::NonFunctionHandlerArgument { name } => write!(
                f,
                "action declaration '{name}' must be called with an arrow or \
                 function expression; bare identifiers and other values are \
                 not supported",
            ),
            Self::MissingFunctionBody { name } => write!(
                f,
                "action declaration '{name}' has no function body",
            ),
            Self::DuplicateActionName {
                name,
                first_idx,
                second_idx,
            } => write!(
                f,
                "action declaration '{name}' is declared twice in one module \
                 (positions {first_idx} and {second_idx}); duplicates would \
                 collide on the wire (action_id = FNV-1a-32(name))",
            ),
        }
    }
}

impl std::error::Error for ActionExtractError {}

/// Extract every `export const NAME = action(<arrow>)` from a parsed
/// module's top-level constants, in source order.
///
/// Returns one [`ActionDeclaration`] per match. Empty when the module
/// imports no `action` symbol from `"albedo"`, or when no top-level
/// const calls it.
pub fn extract_action_declarations(
    module: &ParsedModule,
) -> Result<Vec<ActionDeclaration>, ActionExtractError> {
    let mut out: Vec<ActionDeclaration> = Vec::new();
    let mut seen_names: HashMap<String, usize> = HashMap::new();

    for (name, init) in &module.module_constants {
        let Some((params, body, is_async)) =
            try_extract_action_body(name.as_str(), init, &module.imports)?
        else {
            continue;
        };

        if let Some(first_idx) = seen_names.get(name) {
            return Err(ActionExtractError::DuplicateActionName {
                name: name.clone(),
                first_idx: *first_idx,
                second_idx: out.len(),
            });
        }

        let action_idx = out.len();
        seen_names.insert(name.clone(), action_idx);
        out.push(ActionDeclaration {
            name: name.clone(),
            action_idx,
            params,
            body,
            is_async,
        });
    }

    Ok(out)
}

/// Inspect one `(name, init)` constant. Returns `Ok(None)` when the
/// init isn't a recognised `action(...)` call (so the constant is
/// just a regular user constant, not an action declaration).
fn try_extract_action_body(
    name: &str,
    init: &Expr,
    imports: &HashMap<String, ImportBinding>,
) -> Result<Option<(Vec<ParamBinding>, HandlerBody, bool)>, ActionExtractError> {
    let Expr::Call(call) = init else {
        return Ok(None);
    };
    if !is_action_call(call, imports) {
        return Ok(None);
    }

    let handler_arg = call.args.first().ok_or_else(|| {
        ActionExtractError::MissingHandlerArgument { name: name.to_string() }
    })?;
    if handler_arg.spread.is_some() {
        // Spread args (`action(...handler)`) cannot resolve to a
        // single arrow body at compile time.
        return Err(ActionExtractError::NonFunctionHandlerArgument {
            name: name.to_string(),
        });
    }

    let inner = unwrap_parens(&handler_arg.expr);
    match inner {
        Expr::Arrow(arrow) => Ok(Some(arrow_to_body(name, arrow)?)),
        Expr::Fn(fn_expr) => Ok(Some(function_to_body(name, &fn_expr.function)?)),
        _ => Err(ActionExtractError::NonFunctionHandlerArgument {
            name: name.to_string(),
        }),
    }
}

fn arrow_to_body(
    _name: &str,
    arrow: &ArrowExpr,
) -> Result<(Vec<ParamBinding>, HandlerBody, bool), ActionExtractError> {
    let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
    let body = match arrow.body.as_ref() {
        BlockStmtOrExpr::BlockStmt(block) => HandlerBody::Block(block.stmts.clone()),
        BlockStmtOrExpr::Expr(expr) => HandlerBody::Expr((**expr).clone()),
    };
    Ok((params, body, arrow.is_async))
}

fn function_to_body(
    name: &str,
    function: &Function,
) -> Result<(Vec<ParamBinding>, HandlerBody, bool), ActionExtractError> {
    let params: Vec<ParamBinding> = function
        .params
        .iter()
        .map(|p| param_from_pat(&p.pat))
        .collect();
    let block = function
        .body
        .as_ref()
        .ok_or_else(|| ActionExtractError::MissingFunctionBody {
            name: name.to_string(),
        })?;
    let body = HandlerBody::Block(block.stmts.clone());
    Ok((params, body, function.is_async))
}

fn is_action_call(call: &CallExpr, imports: &HashMap<String, ImportBinding>) -> bool {
    let Callee::Expr(callee_expr) = &call.callee else { return false };
    let Expr::Ident(ident) = callee_expr.as_ref() else { return false };
    let local = ident.sym.to_string();
    if local != "action" {
        return false;
    }
    imports
        .get(&local)
        .map(|b| b.source == "albedo" && b.export_name == "action")
        .unwrap_or(false)
}

/// Peel `(expr)` wrappers, plus TS-only `as`/`satisfies`/`as const`,
/// matching the convention `shared_slots.rs` established so the
/// extractor behaves the same way across TS authoring shapes.
fn unwrap_parens(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(p) => unwrap_parens(&p.expr),
        Expr::TsAs(ts_as) => unwrap_parens(&ts_as.expr),
        Expr::TsSatisfies(ts_sat) => unwrap_parens(&ts_sat.expr),
        Expr::TsConstAssertion(ts_const) => unwrap_parens(&ts_const.expr),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::expr::parse_module;
    use std::path::Path;

    fn parse(source: &str) -> ParsedModule {
        parse_module(source, Path::new("test_module.tsx")).expect("parse")
    }

    fn extract_or_panic(source: &str) -> Vec<ActionDeclaration> {
        let parsed = parse(source);
        // `parse_module` already runs the extractor and stores the
        // result on `ParsedModule.action_declarations`; this helper
        // re-runs it so the tests verify the extractor's output
        // directly rather than the parsed-module side-effect. Both
        // paths must agree.
        let direct = extract_action_declarations(&parsed).expect("extraction");
        assert_eq!(
            direct.len(),
            parsed.action_declarations.len(),
            "ParsedModule.action_declarations must match a fresh extractor run"
        );
        direct
    }

    /// `parse_module` itself propagates extraction errors (see
    /// `expr.rs::parse_module`), so a malformed action declaration
    /// fails at parse time. The error is wrapped in an `anyhow!`,
    /// so the test peels it back to recover the typed variant.
    fn extract_err(source: &str) -> ActionExtractError {
        // First confirm `parse_module` errors out.
        let parse_result = parse_module(source, Path::new("test_module.tsx"));
        assert!(
            parse_result.is_err(),
            "expected parse_module to surface action extractor error, but it returned Ok"
        );
        // Build a ParsedModule with the same `imports` + `module_constants`
        // shape so we can re-run the extractor in isolation and capture
        // the typed `ActionExtractError`. The cheapest correct way: walk
        // the AST ourselves the way `parse_module` does, minus the final
        // action extraction step.
        let bare = parse_bare_module(source);
        extract_action_declarations(&bare).expect_err("expected extraction error")
    }

    /// Build a `ParsedModule` without running the action extractor.
    /// Used by `extract_err` to recover the typed error variant for
    /// assertion — `parse_module` wraps the error in `anyhow!`.
    fn parse_bare_module(source: &str) -> ParsedModule {
        use swc_common::{FileName, SourceMap};
        use swc_ecma_ast::{
            Decl, ImportSpecifier, ModuleDecl, ModuleItem, Stmt, VarDecl, VarDeclarator,
        };
        use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};

        let cm = std::rc::Rc::new(SourceMap::default());
        let file = cm.new_source_file(
            FileName::Custom("test_module.tsx".to_string()).into(),
            source.to_string(),
        );
        let mut parser = Parser::new(
            Syntax::Typescript(TsSyntax {
                tsx: true,
                ..Default::default()
            }),
            StringInput::from(&*file),
            None,
        );
        let module = parser.parse_module().expect("swc parse");

        let mut bare = ParsedModule {
            imports: HashMap::new(),
            functions: HashMap::new(),
            default_export: None,
            module_constants: Vec::new(),
            action_declarations: Vec::new(),
        };

        for item in module.body {
            if let ModuleItem::ModuleDecl(ModuleDecl::Import(import_decl)) = &item {
                let source = import_decl.src.value.to_string();
                for specifier in &import_decl.specifiers {
                    if let ImportSpecifier::Named(named) = specifier {
                        let local = named.local.sym.to_string();
                        let export_name = named
                            .imported
                            .as_ref()
                            .and_then(|n| match n {
                                swc_ecma_ast::ModuleExportName::Ident(i) => {
                                    Some(i.sym.to_string())
                                }
                                _ => None,
                            })
                            .unwrap_or_else(|| local.clone());
                        bare.imports.insert(
                            local,
                            ImportBinding {
                                source: source.clone(),
                                export_name,
                            },
                        );
                    }
                }
            }
            let var_decl: Option<&VarDecl> = match &item {
                ModuleItem::ModuleDecl(ModuleDecl::ExportDecl(export_decl)) => {
                    if let Decl::Var(v) = &export_decl.decl {
                        Some(v.as_ref())
                    } else {
                        None
                    }
                }
                ModuleItem::Stmt(Stmt::Decl(Decl::Var(v))) => Some(v.as_ref()),
                _ => None,
            };
            if let Some(var_decl) = var_decl {
                for VarDeclarator { name, init, .. } in &var_decl.decls {
                    let swc_ecma_ast::Pat::Ident(binding) = name else { continue };
                    let Some(init) = init else { continue };
                    if matches!(
                        init.as_ref(),
                        swc_ecma_ast::Expr::Arrow(_) | swc_ecma_ast::Expr::Fn(_)
                    ) {
                        continue;
                    }
                    bare.module_constants
                        .push((binding.id.sym.to_string(), (**init).clone()));
                }
            }
        }
        bare
    }

    #[test]
    fn extracts_single_action_with_arrow_body() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const submit = action(({ form }) => {
                return { ok: true };
            });
            "#,
        );
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].name, "submit");
        assert_eq!(actions[0].action_idx, 0);
        assert!(!actions[0].is_async);
        assert!(matches!(actions[0].body, HandlerBody::Block(_)));
        assert_eq!(actions[0].params.len(), 1);
    }

    #[test]
    fn extracts_async_action_and_records_is_async_true() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const post = action(async ({ form, broadcast }) => {
                await broadcast("topic", x => x);
            });
            "#,
        );
        assert_eq!(actions.len(), 1);
        assert!(actions[0].is_async);
    }

    #[test]
    fn extracts_expression_bodied_arrow_as_handler_body_expr() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const ping = action(() => "pong");
            "#,
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].body, HandlerBody::Expr(_)));
        assert!(actions[0].params.is_empty());
    }

    #[test]
    fn extracts_function_expression_body() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const submit = action(function ({ form }) {
                return form;
            });
            "#,
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0].body, HandlerBody::Block(_)));
        assert_eq!(actions[0].params.len(), 1);
    }

    #[test]
    fn extracts_multiple_actions_in_source_order() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const first = action(() => 1);
            export const second = action(() => 2);
            export const third = action(() => 3);
            "#,
        );
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].name, "first");
        assert_eq!(actions[0].action_idx, 0);
        assert_eq!(actions[1].name, "second");
        assert_eq!(actions[1].action_idx, 1);
        assert_eq!(actions[2].name, "third");
        assert_eq!(actions[2].action_idx, 2);
    }

    #[test]
    fn ignores_action_from_a_different_import_source() {
        let actions = extract_or_panic(
            r#"
            import { action } from "some-other-lib";
            export const fake = action(() => 0);
            "#,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn ignores_action_with_no_import_binding() {
        let actions = extract_or_panic(
            r#"
            export const local = action(() => 0);
            "#,
        );
        assert!(actions.is_empty());
    }

    #[test]
    fn rejects_action_with_no_handler_argument() {
        let err = extract_err(
            r#"
            import { action } from "albedo";
            export const broken = action();
            "#,
        );
        assert!(matches!(err, ActionExtractError::MissingHandlerArgument { .. }));
    }

    #[test]
    fn rejects_action_with_non_function_argument() {
        let err = extract_err(
            r#"
            import { action } from "albedo";
            const handler = "not a function";
            export const broken = action(handler);
            "#,
        );
        assert!(matches!(
            err,
            ActionExtractError::NonFunctionHandlerArgument { .. }
        ));
    }

    #[test]
    fn rejects_duplicate_action_names_within_one_module() {
        let err = extract_err(
            r#"
            import { action } from "albedo";
            export const same = action(() => 1);
            export const same = action(() => 2);
            "#,
        );
        assert!(matches!(err, ActionExtractError::DuplicateActionName { .. }));
    }

    #[test]
    fn extraction_is_deterministic_across_runs() {
        let source = r#"
            import { action } from "albedo";
            export const alpha = action(() => "a");
            export const beta = action(async ({ form }) => form);
            export const gamma = action(function () { return 0; });
        "#;
        let first = extract_or_panic(source);
        let second = extract_or_panic(source);
        assert_eq!(first.len(), second.len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.action_idx, b.action_idx);
            assert_eq!(a.is_async, b.is_async);
            assert_eq!(a.params.len(), b.params.len());
        }
    }

    #[test]
    fn extracts_action_wrapped_in_paren_or_typescript_cast() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const wrapped = action((() => 1));
            export const cast = action((() => 2) as any);
            "#,
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].name, "wrapped");
        assert_eq!(actions[1].name, "cast");
    }

    #[test]
    fn destructured_params_preserve_field_keys() {
        let actions = extract_or_panic(
            r#"
            import { action } from "albedo";
            export const submit = action(({ form, broadcast }) => 0);
            "#,
        );
        let action = &actions[0];
        match &action.params[0] {
            ParamBinding::Object(fields) => {
                let names: Vec<&str> = fields.iter().map(|(k, _v)| k.as_str()).collect();
                assert_eq!(names, vec!["form", "broadcast"]);
            }
            other => panic!("expected destructured Object param, got {other:?}"),
        }
    }
}
