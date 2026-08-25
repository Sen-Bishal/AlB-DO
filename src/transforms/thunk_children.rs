//! Defer a **component**'s children so its body runs before they do.
//!
//! ## The property this restores
//!
//! JSX lowers to `h(type, props, ...children)`, and JS evaluates arguments
//! before the call. So for `<Wrapper><Inner /></Wrapper>` the server ran
//! `Inner` **first** and `Wrapper` second — children before parents, the exact
//! reverse of React and of `assets/albedo-client.js`, whose `h` is lazy and
//! whose reconciler descends parent-first.
//!
//! That single ordering fact produced two independent, measured defects:
//!
//! * **`useId` diverged between the renderers.** Server `[outer=0, Wrapper=2,
//!   Inner=1]`, client `[outer=0, Wrapper=1, Inner=2]` — transposed, so the
//!   client silently rewrote every id the server had baked into the markup.
//!   Pinned by `use_id_agrees_when_children_are_passed_in`.
//! * **SSR context propagation was impossible.** A Provider cannot thread its
//!   value to consumers that were already finished HTML before it ran. Real
//!   Radix did not degrade to the context default — it *threw*
//!   (`` `DialogTrigger` must be used within `Dialog` ``), the island SSR
//!   failed, no `data-albedo-island` marker was emitted, and the component was
//!   absent from the page entirely.
//!
//! ## The transform
//!
//! Each child argument of a **component** call becomes `__albedo_t(() => …)`.
//! The runtime forces the thunk where the child is actually embedded, which is
//! after the parent's body has run and — critically — inside whatever context
//! Providers are on the stack at that moment.
//!
//! ## Why only component calls
//!
//! 🔑 **Host tags are left byte-identical, on purpose.** After JSX lowering a
//! host tag's type is a string literal (`h("div", …)`) and a component's is not
//! (`h(Wrapper, …)`, `h(h.Fragment, …)`, `h(Dialog.Root, …)`). A host tag has
//! no body to run first, so deferring its children could not change any
//! ordering — it would only add closures to the hottest path and put every
//! existing golden at risk for nothing.
//!
//! ## What this does NOT reach, and why it is still enough
//!
//! npm packages are compiled against the *automatic* runtime and arrive
//! pre-lowered, so this pass never sees their JSX — their children are ordinary
//! eager values. That is fine: a package's own JSX is evaluated **inside** its
//! component body, which is already after any Provider above it pushed. What
//! has to be deferred is the child a package receives from **app** code
//! (`<Dialog.Root><Dialog.Trigger /></Dialog.Root>`), and that call site is
//! exactly what this pass rewrites. Radix passes such children through opaquely
//! (`jsx(Provider, { children: props.children })`), so the thunk survives the
//! round trip.
//!
//! 🪤 **A thunk must be marked, never inferred.** `children` being a function is
//! a real React idiom (render props, `<Ctx.Consumer>{v => …}</Ctx.Consumer>`),
//! so forcing every function child would call user callbacks with no arguments.
//! `__albedo_t` tags what it wraps and the runtime forces only that.

use swc_common::DUMMY_SP;
use swc_ecma_ast::{
    ArrowExpr, BlockStmtOrExpr, CallExpr, Callee, Expr, ExprOrSpread, Ident, Lit, Module,
};
use swc_ecma_visit::{VisitMut, VisitMutWith};

/// The runtime helper that marks a deferred child. Defined in the QuickJS
/// prelude next to `h`.
const THUNK_HELPER: &str = "__albedo_t";

/// The JSX pragma this pass recognises. Must match `JsxOptions::pragma` in
/// `quickjs_engine`'s transpile — they are the two halves of one contract.
const PRAGMA: &str = "h";

/// Wrap every component call's children in `__albedo_t(() => …)`.
///
/// Runs **after** JSX lowering, so it matches on the emitted `h(…)` calls
/// rather than on JSX syntax — which means it sees exactly what the engine will
/// execute, including anything earlier passes rewrote.
pub fn thunk_component_children(module: &mut Module) {
    module.visit_mut_with(&mut ChildThunker);
}

struct ChildThunker;

impl VisitMut for ChildThunker {
    fn visit_mut_call_expr(&mut self, call: &mut CallExpr) {
        // Descend first: inner calls are rewritten in their own right, and a
        // child expression we are about to move inside an arrow must already be
        // in its final form.
        call.visit_mut_children_with(self);

        if !is_pragma_call(call) || call.args.len() <= 2 {
            return;
        }
        // A host tag's type is a string literal after lowering. Everything else
        // — an identifier, a member expression, `h.Fragment` — is a component.
        if matches!(call.args.first().map(|arg| &*arg.expr), Some(Expr::Lit(Lit::Str(_)))) {
            return;
        }

        for arg in call.args.iter_mut().skip(2) {
            *arg = thunked(arg);
        }
    }
}

/// `h(…)` or `h.Fragment(…)`? The pragma is a bare identifier, and the fragment
/// pragma is a member off it, so both reduce to "the leftmost object is `h`".
fn is_pragma_call(call: &CallExpr) -> bool {
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    match &**callee {
        Expr::Ident(ident) => ident.sym == *PRAGMA,
        Expr::Member(member) => matches!(&*member.obj, Expr::Ident(ident) if ident.sym == *PRAGMA),
        _ => false,
    }
}

/// `expr` → `__albedo_t(() => expr)`.
///
/// A spread child (`h(C, null, ...kids)`) cannot stay a spread inside an arrow,
/// so it becomes `__albedo_t(() => kids)` — a plain array argument. The runtime
/// flattens arrays through `__albedo_push_children` already, so the two forms
/// are indistinguishable downstream.
fn thunked(arg: &ExprOrSpread) -> ExprOrSpread {
    let body = arg.expr.clone();
    let arrow = Expr::Arrow(ArrowExpr {
        span: DUMMY_SP,
        params: Vec::new(),
        body: Box::new(BlockStmtOrExpr::Expr(body)),
        is_async: false,
        is_generator: false,
        type_params: None,
        return_type: None,
        ctxt: Default::default(),
    });
    ExprOrSpread {
        spread: None,
        expr: Box::new(Expr::Call(CallExpr {
            span: DUMMY_SP,
            callee: Callee::Expr(Box::new(Expr::Ident(Ident::new_no_ctxt(
                THUNK_HELPER.into(),
                DUMMY_SP,
            )))),
            args: vec![ExprOrSpread {
                spread: None,
                expr: Box::new(arrow),
            }],
            type_args: None,
            ctxt: Default::default(),
        })),
    }
}
