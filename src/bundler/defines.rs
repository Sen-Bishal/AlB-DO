//! Tier C · compile-time `define` substitution and constant-branch folding.
//!
//! ## The finding this exists for
//!
//! Shaking `lucide-react` for one icon left **156 991 B of which 3 507 B was
//! lucide's own code**. A large slice of the remainder was there because the
//! graph walk takes *both* arms of the `NODE_ENV` fork every npm package
//! publishes:
//!
//! ```js
//! // react/index.js
//! if (process.env.NODE_ENV === 'production') {
//!   module.exports = require('./cjs/react.production.min.js');
//! } else {
//!   module.exports = require('./cjs/react.development.js');
//! }
//! ```
//!
//! Both `require`s are string literals, so the specifier scan finds both, so
//! **`react.development.js` (89 kB) shipped to the browser** beside the
//! production build it can never reach. `prop-types` is the same shape and
//! drags `react-is` (13 kB) plus the 20 kB checking factory in behind it.
//!
//! ## Why this is a define pass and not "dead code elimination"
//!
//! 🔑 **The substitution is the whole mechanism; the folding is bookkeeping.**
//! Nothing here reasons about which bindings are used — it replaces a *known
//! constant* with its value and then evaluates the comparisons that constant
//! makes decidable. That is the `define` every bundler ships (esbuild's
//! `--define`, webpack's `DefinePlugin`), and it is categorically not the
//! intra-file DCE that Tier C's Phase 1 declared out of scope: DCE needs scope
//! analysis to prove a binding unreachable, and this needs none.
//!
//! ⚠️ **The scan and the emit must agree.** If the specifier walk pruned the
//! dev arm but the emitted code kept it, the emitted `require('./cjs/…dev.js')`
//! would resolve against a record that was never registered and throw
//! `MODULE_MISSING` at first render. So this returns *source*, and both the walk
//! and the lowering consume that same folded source — they cannot disagree,
//! because there is only one string.
//!
//! ## Hoisting, and why the naive rule was not good enough
//!
//! `var` and function declarations are visible to the whole enclosing function
//! whether or not their branch ran, so a dead branch cannot simply be deleted.
//! The first version of this pass kept any branch containing either — and that
//! rule immediately failed on the exact file it was written for:
//!
//! ```js
//! // prop-types/index.js
//! if (process.env.NODE_ENV !== 'production') {
//!   var ReactIs = require('react-is');           // ← a hoisted `var`
//!   var throwOnDirectAccess = true;
//!   module.exports = require('./factoryWithTypeCheckers')(ReactIs.isElement, …);
//! } else {
//!   module.exports = require('./factoryWithThrowingShims')();
//! }
//! ```
//!
//! Two `var`s pinned 33 kB of development-only code — `react-is` (13 kB) and the
//! checking factory (24 kB) — into every client bundle.
//!
//! 🔑 **The exact rule, not a conservative one: replace the dead branch with the
//! bare declarations it hoisted.** `if (false) { var a = 1; }` leaves `a`
//! declared and `undefined`, which is *precisely* what `var a;` means. So the
//! body goes and `var a;` stays, and nothing observable changes — no scope
//! analysis, no reference counting, no guess.
//!
//! ⚠️ **A function declaration still pins its branch.** Unlike `var`, a hoisted
//! function is *initialized* at hoist time in sloppy mode and block-scoped in
//! strict mode, so there is no single replacement that is right in both. When a
//! branch is pinned the test is still folded to a literal — the byte is not
//! saved, but the runtime decision is free and, more importantly, **the
//! specifier scan still sees both arms**, which is the honest outcome rather
//! than a silent mismatch.

use std::collections::BTreeMap;
use swc_common::comments::SingleThreadedComments;
use swc_common::util::take::Take;
use swc_common::{sync::Lrc, FileName, SourceMap, DUMMY_SP};
use swc_ecma_ast::{
    BinExpr, BinaryOp, BindingIdent, BlockStmt, Bool, CondExpr, Decl, EmptyStmt, Expr, IfStmt, Lit,
    MemberProp, Pat, Program, Stmt, Str, UnaryOp, VarDecl, VarDeclKind, VarDeclarator,
};
use swc_ecma_codegen::{text_writer::JsWriter, Config as CodegenConfig, Emitter};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// The one substitution Tier C ships: an npm package's `NODE_ENV` fork resolves
/// to the production arm, because the browser bundle *is* the production build
/// and the client runtime's `process` stub says so too (see
/// `build_browser_npm_runtime_script`). The two must name the same value; a
/// bundle folded to `production` running against a stub reporting `development`
/// would take a branch whose dependency was never bundled.
pub const NODE_ENV_PATH: &str = "process.env.NODE_ENV";

/// The value [`NODE_ENV_PATH`] folds to. Shared with the browser runtime stub so
/// the two cannot drift.
pub const NODE_ENV_VALUE: &str = "production";

/// Dotted member paths whose value is known at build time.
///
/// Only string values, deliberately: every real-world instance of this idiom
/// compares a string, and admitting arbitrary expressions would turn a
/// substitution table into an evaluator.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Defines {
    entries: BTreeMap<String, String>,
}

impl Defines {
    /// The client-bundle define set.
    #[must_use]
    pub fn browser() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(NODE_ENV_PATH.to_string(), NODE_ENV_VALUE.to_string());
        Self { entries }
    }

    /// `true` when nothing is defined, so callers can skip the pass entirely.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The value bound to a dotted path, if any.
    fn lookup(&self, path: &str) -> Option<&str> {
        self.entries.get(path).map(String::as_str)
    }

    /// A cheap pre-filter: the leading identifier of every defined path. A file
    /// mentioning none of them cannot be changed by this pass, and skipping the
    /// parse for it is what keeps the cost off the ~99% of a package's files
    /// that have no `process.env` in them at all.
    fn mentions_any(&self, source: &str) -> bool {
        self.entries
            .keys()
            .any(|path| source.contains(path.split('.').next().unwrap_or(path)))
    }
}

/// Substitute `defines` into `source` and fold what that makes constant.
///
/// Returns `None` when the pass changed nothing — including the common case of
/// a file that never mentions a defined path — so the caller keeps the original
/// bytes and pays no re-print.
///
/// `is_module` selects ESM vs script parsing, matching how the caller already
/// classified the file; a parse failure returns `None` (the file is passed
/// through untouched and fails later, loudly, in the lowering that owns that
/// error).
#[must_use]
pub fn fold_defines(
    label: &str,
    source: &str,
    is_module: bool,
    defines: &Defines,
) -> Option<String> {
    if defines.is_empty() || !defines.mentions_any(source) {
        return None;
    }

    let source_map: Lrc<SourceMap> = Lrc::default();
    let comments = SingleThreadedComments::default();
    let file = source_map.new_source_file(
        FileName::Custom(label.to_string()).into(),
        source.to_string(),
    );
    let mut parser = Parser::new(
        Syntax::Es(EsSyntax::default()),
        StringInput::from(&*file),
        Some(&comments),
    );
    let mut program = if is_module {
        Program::Module(parser.parse_module().ok()?)
    } else {
        Program::Script(parser.parse_script().ok()?)
    };

    let mut folder = DefineFolder {
        defines,
        changed: false,
    };
    program.visit_mut_with(&mut folder);
    if !folder.changed {
        return None;
    }

    // Comments are carried through the re-print. Dropping them would shave
    // bytes off exactly the files this pass touches, but those bytes include
    // npm packages' licence headers, and silently stripping a licence to save
    // 400 bytes is not a trade this pass gets to make on the user's behalf.
    let mut buffer = Vec::new();
    {
        let writer = JsWriter::new(source_map.clone(), "\n", &mut buffer, None);
        let mut emitter = Emitter {
            cfg: CodegenConfig::default(),
            cm: source_map,
            comments: Some(&comments),
            wr: writer,
        };
        match &program {
            Program::Module(module) => emitter.emit_module(module).ok()?,
            Program::Script(script) => emitter.emit_script(script).ok()?,
        }
    }
    String::from_utf8(buffer).ok()
}

struct DefineFolder<'a> {
    defines: &'a Defines,
    changed: bool,
}

impl VisitMut for DefineFolder<'_> {
    fn visit_mut_expr(&mut self, expr: &mut Expr) {
        // Children first: an inner `process.env.NODE_ENV` must already be a
        // literal before the comparison containing it is asked whether it is
        // decidable.
        expr.visit_mut_children_with(self);

        if matches!(expr, Expr::Member(_)) {
            if let Some(value) = dotted_path(expr).and_then(|path| {
                self.defines.lookup(path.as_str()).map(str::to_string)
            }) {
                *expr = Expr::Lit(Lit::Str(Str {
                    span: DUMMY_SP,
                    value: value.into(),
                    raw: None,
                }));
                self.changed = true;
                return;
            }
        }

        match expr {
            Expr::Bin(bin) => {
                if let Some(result) = fold_comparison(bin) {
                    *expr = bool_lit(result);
                    self.changed = true;
                }
            }
            Expr::Unary(unary) if unary.op == UnaryOp::Bang => {
                if let Some(value) = as_bool(&unary.arg) {
                    *expr = bool_lit(!value);
                    self.changed = true;
                }
            }
            Expr::Cond(CondExpr {
                test, cons, alt, ..
            }) => {
                if let Some(value) = as_bool(test) {
                    let taken = if value { cons.take() } else { alt.take() };
                    *expr = *taken;
                    self.changed = true;
                }
            }
            _ => {}
        }
    }

    fn visit_mut_stmt(&mut self, stmt: &mut Stmt) {
        stmt.visit_mut_children_with(self);

        let Stmt::If(IfStmt {
            test, cons, alt, ..
        }) = stmt
        else {
            return;
        };
        let Some(value) = as_bool(test) else {
            return;
        };
        // The hoisting rule (see the module docs). Asked *before* anything is
        // moved out of the node — `Take::take` swaps a dummy in, so a check
        // after the fact would leave `if (false) ;` behind and silently delete
        // the live branch.
        let hoisted = match if value { alt.as_deref() } else { Some(&**cons) } {
            None => Vec::new(),
            Some(dead) => match hoisted_var_names(dead) {
                Some(names) => names,
                // A function declaration pins its branch. The test above is
                // already a literal, so the runtime decision is free; only the
                // bytes stay.
                None => return,
            },
        };

        let taken = if value { Some(cons.take()) } else { alt.take() };
        let kept = match taken {
            Some(branch) => *branch,
            None => Stmt::Empty(EmptyStmt { span: DUMMY_SP }),
        };
        *stmt = if hoisted.is_empty() {
            kept
        } else {
            // `var` is function-scoped, so wrapping both statements in a block
            // leaves the declarations exactly where they already were.
            Stmt::Block(BlockStmt {
                span: DUMMY_SP,
                ctxt: Default::default(),
                stmts: vec![undefined_var_decl(&hoisted), kept],
            })
        };
        self.changed = true;
    }
}

/// `a.b.c` as `"a.b.c"`, for a chain of plain identifiers (or `a["b"]`, which
/// means the same thing). Anything else — a call, a computed non-literal index —
/// is not a define path and returns `None`.
fn dotted_path(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(ident) => Some(ident.sym.to_string()),
        Expr::Member(member) => {
            let object = dotted_path(&member.obj)?;
            let property = match &member.prop {
                MemberProp::Ident(ident) => ident.sym.to_string(),
                MemberProp::Computed(computed) => match computed.expr.as_ref() {
                    Expr::Lit(Lit::Str(literal)) => literal.value.to_string(),
                    _ => return None,
                },
                MemberProp::PrivateName(_) => return None,
            };
            Some(format!("{object}.{property}"))
        }
        _ => None,
    }
}

/// Evaluate a comparison both of whose sides are literals this pass understands.
///
/// `===`/`!==` only when the two literals have the same type; `==`/`!=` is
/// answered the same way, which is sound because the only literal kinds here
/// are strings and booleans and loose equality agrees with strict equality on
/// same-typed operands. A mixed-type comparison declines rather than guessing at
/// coercion.
fn fold_comparison(bin: &BinExpr) -> Option<bool> {
    let negated = match bin.op {
        BinaryOp::EqEqEq | BinaryOp::EqEq => false,
        BinaryOp::NotEqEq | BinaryOp::NotEq => true,
        _ => return None,
    };
    let equal = match (as_literal(&bin.left)?, as_literal(&bin.right)?) {
        (LiteralValue::Str(left), LiteralValue::Str(right)) => left == right,
        (LiteralValue::Bool(left), LiteralValue::Bool(right)) => left == right,
        _ => return None,
    };
    Some(equal != negated)
}

enum LiteralValue {
    Str(String),
    Bool(bool),
}

fn as_literal(expr: &Expr) -> Option<LiteralValue> {
    match expr {
        Expr::Lit(Lit::Str(literal)) => Some(LiteralValue::Str(literal.value.to_string())),
        Expr::Lit(Lit::Bool(literal)) => Some(LiteralValue::Bool(literal.value)),
        Expr::Paren(paren) => as_literal(&paren.expr),
        _ => None,
    }
}

fn as_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Lit(Lit::Bool(literal)) => Some(literal.value),
        Expr::Paren(paren) => as_bool(&paren.expr),
        _ => None,
    }
}

fn bool_lit(value: bool) -> Expr {
    Expr::Lit(Lit::Bool(Bool {
        span: DUMMY_SP,
        value,
    }))
}

/// The `var` names a statement hoists out of its own block, or `None` when a
/// function declaration makes the statement undroppable.
///
/// `let`/`const`/`class` are block-scoped and cannot be observed from outside
/// the branch, so they are not collected. Function and class *bodies* are not
/// entered — a `var` inside a nested function belongs to that function, not to
/// the branch being dropped.
fn hoisted_var_names(stmt: &Stmt) -> Option<Vec<String>> {
    let mut scan = HoistScan {
        vars: Vec::new(),
        pinned: false,
    };
    stmt.visit_with(&mut scan);
    if scan.pinned {
        return None;
    }
    scan.vars.dedup();
    Some(scan.vars)
}

/// `var a, b;` — the declarations without their initializers, which is exactly
/// what a `var` in a branch that did not execute means.
fn undefined_var_decl(names: &[String]) -> Stmt {
    Stmt::Decl(Decl::Var(Box::new(VarDecl {
        span: DUMMY_SP,
        ctxt: Default::default(),
        kind: VarDeclKind::Var,
        declare: false,
        decls: names
            .iter()
            .map(|name| VarDeclarator {
                span: DUMMY_SP,
                name: Pat::Ident(BindingIdent {
                    id: swc_ecma_ast::Ident::new_no_ctxt(name.as_str().into(), DUMMY_SP),
                    type_ann: None,
                }),
                init: None,
                definite: false,
            })
            .collect(),
    })))
}

struct HoistScan {
    vars: Vec<String>,
    pinned: bool,
}

impl Visit for HoistScan {
    // `visit_var_decl` rather than `visit_decl`, because a `for (var i = 0; …)`
    // head is a `VarDecl` that is not a `Decl` — and its `i` hoists just the
    // same.
    fn visit_var_decl(&mut self, var: &VarDecl) {
        if var.kind == VarDeclKind::Var {
            for declarator in &var.decls {
                collect_pattern_idents(&declarator.name, &mut self.vars);
            }
        }
        var.visit_children_with(self);
    }

    fn visit_fn_decl(&mut self, _: &swc_ecma_ast::FnDecl) {
        self.pinned = true;
    }

    // A nested function's own declarations are its own; do not descend.
    fn visit_function(&mut self, _: &swc_ecma_ast::Function) {}
    fn visit_arrow_expr(&mut self, _: &swc_ecma_ast::ArrowExpr) {}
    fn visit_class(&mut self, _: &swc_ecma_ast::Class) {}
}

/// Every identifier a binding pattern introduces, destructuring included.
fn collect_pattern_idents(pattern: &Pat, out: &mut Vec<String>) {
    match pattern {
        Pat::Ident(binding) => out.push(binding.id.sym.to_string()),
        Pat::Array(array) => {
            for element in array.elems.iter().flatten() {
                collect_pattern_idents(element, out);
            }
        }
        Pat::Object(object) => {
            for property in &object.props {
                match property {
                    swc_ecma_ast::ObjectPatProp::KeyValue(entry) => {
                        collect_pattern_idents(&entry.value, out);
                    }
                    swc_ecma_ast::ObjectPatProp::Assign(entry) => {
                        out.push(entry.key.sym.to_string());
                    }
                    swc_ecma_ast::ObjectPatProp::Rest(entry) => {
                        collect_pattern_idents(&entry.arg, out);
                    }
                }
            }
        }
        Pat::Rest(rest) => collect_pattern_idents(&rest.arg, out),
        Pat::Assign(assign) => collect_pattern_idents(&assign.left, out),
        Pat::Expr(_) | Pat::Invalid(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser_fold(source: &str) -> Option<String> {
        fold_defines("test.js", source, false, &Defines::browser())
    }

    #[test]
    fn a_file_without_the_define_is_left_alone() {
        assert!(browser_fold("module.exports = require('./a');").is_none());
    }

    #[test]
    fn the_production_arm_survives_and_the_dev_arm_goes() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV === 'production') {\n  module.exports = require('./prod');\n} else {\n  module.exports = require('./dev');\n}",
        )
        .expect("the fork folds");
        assert!(folded.contains("./prod"), "kept the production arm: {folded}");
        assert!(
            !folded.contains("./dev"),
            "the development arm must not survive: {folded}"
        );
    }

    #[test]
    fn the_negated_form_folds_the_other_way() {
        // `prop-types`'s exact shape: the dev arm is the `if`, not the `else`.
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') {\n  module.exports = require('./checkers');\n} else {\n  module.exports = require('./shims');\n}",
        )
        .expect("the fork folds");
        assert!(folded.contains("./shims"), "{folded}");
        assert!(!folded.contains("./checkers"), "{folded}");
    }

    #[test]
    fn a_ternary_folds_too() {
        let folded =
            browser_fold("var mode = process.env.NODE_ENV === 'production' ? 'p' : 'd';")
                .expect("the ternary folds");
        assert!(folded.contains("'p'") || folded.contains("\"p\""), "{folded}");
        assert!(!folded.contains("'d'") && !folded.contains("\"d\""), "{folded}");
    }

    #[test]
    fn an_if_with_no_else_disappears_when_its_test_is_false() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') { console.warn('dev only'); }",
        )
        .expect("folds");
        assert!(!folded.contains("dev only"), "{folded}");
    }

    /// The hoisting rule: the *declaration* survives, its initializer does not.
    /// `if (false) { var a = 1; }` leaves `a` declared and `undefined`, and
    /// `var a;` says exactly that.
    #[test]
    fn a_dead_branch_var_survives_as_a_bare_declaration() {
        let source =
            "if (process.env.NODE_ENV !== 'production') { var warned = require('./noisy'); }\nconsole.log(warned);";
        let folded = browser_fold(source).expect("folds");
        assert!(
            folded.contains("var warned"),
            "the hoisted binding must still exist: {folded}"
        );
        assert!(
            !folded.contains("./noisy"),
            "but its initializer — and the dependency it names — must not: {folded}"
        );
    }

    /// `prop-types/index.js`, reproduced exactly. Two hoisted `var`s used to pin
    /// 33 kB of development-only code (`react-is` plus the checking factory)
    /// into every client bundle.
    #[test]
    fn the_prop_types_shape_drops_its_development_arm() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') {\n             var ReactIs = require('react-is');\n             var throwOnDirectAccess = true;\n             module.exports = require('./factoryWithTypeCheckers')(ReactIs.isElement, throwOnDirectAccess);\n             } else {\n             module.exports = require('./factoryWithThrowingShims')();\n             }",
        )
        .expect("folds");
        assert!(folded.contains("./factoryWithThrowingShims"), "{folded}");
        assert!(!folded.contains("react-is"), "{folded}");
        assert!(!folded.contains("factoryWithTypeCheckers"), "{folded}");
        assert!(folded.contains("var ReactIs"), "the bindings stay: {folded}");
    }

    /// A function declaration is the one thing that still pins its branch:
    /// hoisted-and-initialized in sloppy mode, block-scoped in strict mode, and
    /// no single replacement is right in both.
    #[test]
    fn a_dead_branch_declaring_a_function_is_kept_whole() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') { function warn() { return require('./noisy'); } }",
        )
        .expect("the test still folds to a literal");
        assert!(folded.contains("./noisy"), "{folded}");
        assert!(folded.contains("false"), "the test is still constant: {folded}");
    }

    /// Destructured `var`s hoist every name they bind.
    #[test]
    fn a_destructured_dead_var_hoists_all_of_its_names() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') { var { a, b: c } = require('./noisy'); }",
        )
        .expect("folds");
        assert!(folded.contains('a') && folded.contains('c'), "{folded}");
        assert!(!folded.contains("./noisy"), "{folded}");
    }

    #[test]
    fn a_dead_branch_declaring_let_is_dropped() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') { let scoped = 1; console.log(scoped); }",
        )
        .expect("folds");
        assert!(!folded.contains("scoped"), "{folded}");
    }

    /// A `var` inside a nested function belongs to that function, so the branch
    /// around it is still droppable.
    #[test]
    fn a_var_inside_a_nested_function_does_not_pin_the_branch() {
        let folded = browser_fold(
            "if (process.env.NODE_ENV !== 'production') { (function () { var inner = 1; return inner; })(); }",
        )
        .expect("folds");
        assert!(!folded.contains("inner"), "{folded}");
    }

    #[test]
    fn esm_sources_fold_as_modules() {
        let folded = fold_defines(
            "test.mjs",
            "import x from './x';\nexport const flag = process.env.NODE_ENV === 'production';",
            true,
            &Defines::browser(),
        )
        .expect("folds");
        assert!(folded.contains("import"), "{folded}");
        assert!(folded.contains("true"), "{folded}");
    }

    #[test]
    fn a_mixed_type_comparison_is_declined_rather_than_guessed() {
        // `process.env.NODE_ENV == 0` is a coercion question this pass does not
        // answer; folding it would require modelling `ToNumber`.
        let folded = browser_fold("var x = process.env.NODE_ENV == 0;").expect("substitutes");
        assert!(
            folded.contains("production") && folded.contains('0'),
            "the comparison must remain: {folded}"
        );
    }
}
