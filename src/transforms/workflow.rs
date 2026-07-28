//! APERTURE · A2 — the workflow lowering for handler bodies.
//!
//! `development-plan/APERTURE.md` § 5.5: **there is no ALBEDO dialect.** The
//! author writes exactly what a vendor's docs, Stack Overflow or an assistant
//! hands them:
//!
//! ```js
//! const res  = await fetch("https://api.stripe.com/v1/charges", { method: "POST", body })
//! const paid = await res.json()
//! ```
//!
//! That is a requirement rather than a courtesy. A `fetch()` returning a value
//! directly, without `await`, would read as uncanny to every engineer who has
//! used the web platform and would break every example on the internet — and
//! the on-ramp `TODO.md` items 0/1 bought would be spent again.
//!
//! Two folds make that source run on an engine with no event loop:
//!
//! 1. [`strip_await`] — `await` is a **compiler lowering, not a runtime
//!    feature**. A handler body is spliced into an ordinary function, so an
//!    `await` in it is a `SyntaxError` before it is anything else. The engine's
//!    `fetch` returns its value directly (it is answered from the journal), so
//!    removing the marker is exact rather than approximate. No `AsyncRuntime`,
//!    no job queue, no event loop; the marker never survives into emitted JS.
//!
//! 2. [`guard_userland_catches`] — § 11 R3. The suspend sentinel travels as a
//!    throw, and a userland `try { … } catch { … }` around a call would eat it
//!    and run the body on garbage. React Suspense has this exact bug class. So
//!    every userland `catch` gets `if (__albedo_is_suspend(<param>)) throw
//!    <param>;` prepended, recursing into nested functions.
//!
//! **Neither fold is exhaustive on its own, and that is by design.** The AST
//! cannot see inside an npm bundle, so a `fetch` in a callback handed to a
//! bundled library's `try/catch` escapes fold 2 — which is why the runtime also
//! carries an independent flag the epilogue checks (`bridge.rs`). A swallowed
//! sentinel degrades to *suspend anyway*, never to *commit the effects of a body
//! that never got its data*.

use swc_ecma_ast::{CatchClause, Expr, Ident, Pat, Stmt};
use swc_ecma_visit::{VisitMut, VisitMutWith};
use swc_common::DUMMY_SP;

/// Name of the runtime predicate the catch guard calls. Defined once in
/// `bridge.rs`'s handler prelude; named here so the two cannot drift.
pub const IS_SUSPEND_FN: &str = "__albedo_is_suspend";

/// Binding introduced for a `catch { }` with no parameter, so the guard has
/// something to test. Optional catch binding is ES2019 and authors use it.
const SYNTHETIC_CATCH_PARAM: &str = "__albedo_caught";

/// Remove every `await` in a handler body.
///
/// Sound because the operand of an `await` in a handler body is never a promise
/// this engine could settle: `fetch` answers from the journal, and `res.json()`
/// parses a string that already arrived. `await` on a non-promise is the
/// identity in JS anyway — the difference is only *when*, and a body with no
/// event loop has no other work to interleave.
pub fn strip_await(stmts: &mut Vec<Stmt>) {
    let mut folder = AwaitStripper;
    for stmt in stmts.iter_mut() {
        stmt.visit_mut_with(&mut folder);
    }
}

/// [`strip_await`] for an expression-bodied handler.
pub fn strip_await_expr(expr: &mut Expr) {
    expr.visit_mut_with(&mut AwaitStripper);
}

struct AwaitStripper;

impl VisitMut for AwaitStripper {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        expr.visit_mut_children_with(self);
        if let Expr::Await(await_expr) = expr {
            *expr = (*await_expr.arg).clone();
        }
    }
}

/// Prepend the suspend re-throw to every `catch` in a handler body.
pub fn guard_userland_catches(stmts: &mut Vec<Stmt>) {
    let mut folder = CatchGuard;
    for stmt in stmts.iter_mut() {
        stmt.visit_mut_with(&mut folder);
    }
}

/// [`guard_userland_catches`] for an expression-bodied handler (a `catch` can
/// still appear inside a nested function expression).
pub fn guard_userland_catches_expr(expr: &mut Expr) {
    expr.visit_mut_with(&mut CatchGuard);
}

struct CatchGuard;

impl VisitMut for CatchGuard {
    fn visit_mut_catch_clause(&mut self, clause: &mut CatchClause) {
        clause.visit_mut_children_with(self);

        let param_name = match &clause.param {
            Some(Pat::Ident(ident)) => ident.id.sym.to_string(),
            // A destructuring catch param (`catch ({ message })`) cannot name the
            // error itself, and `catch {}` has no binding at all. Both get a
            // synthetic one: the guard needs a value to test, and rewriting the
            // author's pattern would change what their handler sees.
            _ => {
                clause.param = Some(Pat::Ident(
                    Ident::new_no_ctxt(SYNTHETIC_CATCH_PARAM.into(), DUMMY_SP).into(),
                ));
                SYNTHETIC_CATCH_PARAM.to_string()
            }
        };

        clause
            .body
            .stmts
            .insert(0, suspend_rethrow(&param_name));
    }
}

/// `if (__albedo_is_suspend(<param>)) throw <param>;`
fn suspend_rethrow(param: &str) -> Stmt {
    use swc_ecma_ast::{CallExpr, Callee, ExprOrSpread, IfStmt, ThrowStmt};

    let error = || Box::new(Expr::Ident(Ident::new_no_ctxt(param.into(), DUMMY_SP)));
    Stmt::If(IfStmt {
        span: DUMMY_SP,
        test: Box::new(Expr::Call(CallExpr {
            span: DUMMY_SP,
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
                IS_SUSPEND_FN.into(),
                DUMMY_SP,
            )))),
            args: vec![ExprOrSpread {
                spread: None,
                expr: error(),
            }],
            type_args: None,
            ctxt: Default::default(),
        })),
        cons: Box::new(Stmt::Throw(ThrowStmt {
            span: DUMMY_SP,
            arg: error(),
        })),
        alt: None,
    })
}

/// A `return` inside a `finally` **discards an in-flight throw**, which would
/// swallow the suspend sentinel in a way no re-throw guard can reach: the
/// `finally` wins over the `catch` by JavaScript's own rules.
///
/// So it is a build error in a body that calls out — a loud message about a
/// construct nobody writes deliberately, rather than a silent correctness hole.
/// § 11 R3.
pub fn find_return_in_finally(stmts: &[Stmt]) -> bool {
    struct Finder {
        in_finally: bool,
        found: bool,
    }
    impl swc_ecma_visit::Visit for Finder {
        fn visit_try_stmt(&mut self, node: &swc_ecma_ast::TryStmt) {
            use swc_ecma_visit::VisitWith;
            node.block.visit_with(self);
            if let Some(handler) = &node.handler {
                handler.visit_with(self);
            }
            if let Some(finalizer) = &node.finalizer {
                let outer = self.in_finally;
                self.in_finally = true;
                finalizer.visit_with(self);
                self.in_finally = outer;
            }
        }
        fn visit_return_stmt(&mut self, _node: &swc_ecma_ast::ReturnStmt) {
            if self.in_finally {
                self.found = true;
            }
        }
        // A `return` inside a function DECLARED in a finally block returns from
        // that function, not from the finally — it discards nothing.
        fn visit_function(&mut self, _node: &swc_ecma_ast::Function) {}
        fn visit_arrow_expr(&mut self, _node: &swc_ecma_ast::ArrowExpr) {}
    }

    use swc_ecma_visit::VisitWith;
    let mut finder = Finder {
        in_finally: false,
        found: false,
    };
    for stmt in stmts {
        stmt.visit_with(&mut finder);
    }
    finder.found
}

/// Whether a body mentions `fetch` as a call at all — the gate for the checks
/// that only matter to a body that calls out.
#[must_use]
pub fn body_calls_fetch(stmts: &[Stmt]) -> bool {
    struct Finder(bool);
    impl swc_ecma_visit::Visit for Finder {
        fn visit_call_expr(&mut self, node: &swc_ecma_ast::CallExpr) {
            use swc_ecma_visit::VisitWith;
            if let swc_ecma_ast::Callee::Expr(callee) = &node.callee {
                if matches!(&**callee, Expr::Ident(id) if id.sym.as_ref() == "fetch") {
                    self.0 = true;
                }
            }
            node.visit_children_with(self);
        }
    }
    use swc_ecma_visit::VisitWith;
    let mut finder = Finder(false);
    for stmt in stmts {
        stmt.visit_with(&mut finder);
    }
    finder.0
}

/// Everything a handler body needs before it can run under the engine.
///
/// One entry point rather than three calls at the call site, because the three
/// belong together: the `await` strip is what makes the source parse, and the
/// catch guard is what makes the resulting throw survive the author's own error
/// handling. Missing either one produces a body that *looks* fine.
pub fn lower_handler_body(stmts: &mut Vec<Stmt>) -> Result<(), WorkflowLoweringError> {
    if body_calls_fetch(stmts) && find_return_in_finally(stmts) {
        return Err(WorkflowLoweringError::ReturnInFinally);
    }
    strip_await(stmts);
    guard_userland_catches(stmts);
    Ok(())
}

/// Why a body cannot be lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowLoweringError {
    /// `return` inside a `finally` in a body that calls out.
    ReturnInFinally,
}

impl std::fmt::Display for WorkflowLoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowLoweringError::ReturnInFinally => write!(
                f,
                "a `return` inside `finally` discards an in-flight throw, which would silently \
                 swallow an outbound call's suspension and run the rest of this action on \
                 incomplete data. Move the return out of the `finally` block."
            ),
        }
    }
}

impl std::error::Error for WorkflowLoweringError {}

#[cfg(test)]
mod tests {
    use super::*;
    use swc_common::{sync::Lrc, FileName, SourceMap};
    use swc_ecma_codegen::{text_writer::JsWriter, Config, Emitter};
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};

    fn parse(src: &str) -> Vec<Stmt> {
        let cm: Lrc<SourceMap> = Default::default();
        let file = cm.new_source_file(Lrc::new(FileName::Custom("body.js".into())), src.to_string());
        let mut parser = Parser::new(
            Syntax::Es(EsSyntax::default()),
            StringInput::from(&*file),
            None,
        );
        parser
            .parse_script()
            .expect("body parses")
            .body
    }

    fn emit(stmts: &[Stmt]) -> String {
        use swc_ecma_ast::{Program, Script};
        let cm: Lrc<SourceMap> = Default::default();
        let mut buf = Vec::new();
        {
            let mut emitter = Emitter {
                cfg: Config::default(),
                cm: cm.clone(),
                comments: None,
                wr: JsWriter::new(cm, "\n", &mut buf, None),
            };
            emitter
                .emit_program(&Program::Script(Script {
                    span: DUMMY_SP,
                    body: stmts.to_vec(),
                    shebang: None,
                }))
                .expect("emits");
        }
        String::from_utf8(buf).expect("utf8")
    }

    fn lowered(src: &str) -> String {
        let mut stmts = parse(src);
        lower_handler_body(&mut stmts).expect("lowers");
        emit(&stmts)
    }

    /// The whole point of § 5.5: vendor sample code runs verbatim.
    #[test]
    fn the_standard_fetch_idiom_lowers_to_plain_calls() {
        let out = lowered(
            "const res = await fetch('https://api.test/charges', { method: 'POST' }); \
             const paid = await res.json(); append('orders', { id: paid.id });",
        );
        assert!(!out.contains("await"), "no await survives; got {out}");
        assert!(out.contains("fetch('https://api.test/charges'"), "got {out}");
        assert!(out.contains("res.json()"), "got {out}");
    }

    #[test]
    fn await_is_stripped_inside_nested_functions_too() {
        let out = lowered("const f = async () => { return await fetch('/a'); }; f();");
        assert!(!out.contains("await"), "got {out}");
    }

    /// § 11 R3 — the guard is the FIRST statement of the catch, so it runs
    /// before any userland handling that might return, log or swallow.
    #[test]
    fn every_userland_catch_rethrows_the_suspension_first() {
        let out = lowered("try { fetch('/a'); } catch (e) { console.log(e); }");
        let guard = "if (__albedo_is_suspend(e)) throw e;";
        assert!(out.contains(guard), "got {out}");
        let guard_at = out.find(guard).unwrap();
        let userland_at = out.find("console.log(e)").unwrap();
        assert!(guard_at < userland_at, "the guard must come first; got {out}");
    }

    /// `catch {}` and `catch ({ message })` cannot name the error, so the guard
    /// would have nothing to test. A synthetic binding is added rather than
    /// rewriting the author's pattern, which would change what their handler
    /// sees.
    #[test]
    fn a_catch_with_no_usable_binding_gets_a_synthetic_one() {
        let bare = lowered("try { fetch('/a'); } catch { }");
        assert!(bare.contains("__albedo_caught"), "got {bare}");

        let destructured = lowered("try { fetch('/a'); } catch ({ message }) { log(message); }");
        assert!(destructured.contains("__albedo_caught"), "got {destructured}");
    }

    #[test]
    fn nested_catches_are_all_guarded() {
        let out = lowered(
            "try { try { fetch('/a'); } catch (inner) { x(); } } catch (outer) { y(); }",
        );
        assert!(out.contains("__albedo_is_suspend(inner)"), "got {out}");
        assert!(out.contains("__albedo_is_suspend(outer)"), "got {out}");
    }

    /// A `finally` that returns beats a `catch` that re-throws, by JavaScript's
    /// own rules — so no guard can save it and it has to be a build error.
    #[test]
    fn a_return_inside_finally_is_refused_when_the_body_calls_out() {
        let mut stmts = parse("try { fetch('/a'); } finally { return 1; }");
        assert_eq!(
            lower_handler_body(&mut stmts),
            Err(WorkflowLoweringError::ReturnInFinally)
        );
    }

    /// …and is left alone otherwise. The construct is legal JavaScript; it is
    /// only unsafe in a body that can suspend, so refusing it everywhere would
    /// be a framework breaking code it has no stake in.
    #[test]
    fn a_return_inside_finally_is_fine_in_a_body_that_never_calls_out() {
        let mut stmts = parse("try { setCount(1); } finally { return 1; }");
        assert!(lower_handler_body(&mut stmts).is_ok());
    }

    /// A `return` inside a function *declared* in a finally returns from that
    /// function — it discards nothing, and refusing it would be a false
    /// positive on ordinary code.
    #[test]
    fn a_return_inside_a_function_declared_in_finally_is_not_the_hazard() {
        let stmts = parse("try { fetch('/a'); } finally { const f = () => { return 1; }; f(); }");
        assert!(!find_return_in_finally(&stmts));
    }
}
