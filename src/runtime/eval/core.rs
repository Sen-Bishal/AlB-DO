use crate::ir::opcode::{Instruction, ProxyId, SlotId, StableId};
use crate::runtime::compiled::{allocate_proxy_id, CompiledComponent, CompiledProject};
use crate::runtime::slot_store::SessionSlotView;
use crate::transforms::events::HandlerBody;
// Aliased on import: the local `form_action_name` binding below holds the
// *detected* name for the element being rendered, so the parser that
// produces it needs a distinct name to stay readable.
use crate::transforms::form::{
    allocate_field_error_id, form_action_name as parse_form_action_sentinel,
    CSRF_PLACEHOLDER_INPUT, FORM_ACTION_ATTR,
};
use crate::types::ComponentId;
use anyhow::{anyhow, Result};
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::runtime::eval::component::{
    arg_num, classnames_collect, date_value_ms, escape_html, fnv1a_32, fnv1a_hash,
    import_candidates, is_classnames_source, is_component_module, is_component_tag, is_truthy,
    is_void_tag, json_int, json_num, lit_to_value, make_date_value, normalize_jsx_text,
    normalize_slashes, normalize_specifier, prop_name_to_string, render_attrs, to_number,
    value_to_string,
};

thread_local! {
    /// Per-render element counter. Reset by `render_entry` at the top of
    /// every render call so element ids are deterministic per render and
    /// independent across concurrent renders on different threads.
    ///
    /// Combined with `module_spec` it produces the FNV-1a-32 input that
    /// becomes `data-albedo-id` on every shell element bakabox should be
    /// able to address. Phase K's compiler can replace this with a
    /// content-hash strategy when HMR stability matters.
    static RENDER_ELEMENT_COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// Bakabox reads anchors from `data-albedo-id` (DEFAULT_ANCHOR_ATTRIBUTE
/// in `assets/albedo-runtime.js`). Keep these in sync.
pub const ALBEDO_ID_ATTR: &str = "data-albedo-id";

/// Phase P · Stream E.1 — sentinel emitted by the renderer when it
/// encounters the `<children />` JSX intrinsic. The manifest
/// builder's `wrap_in_layouts` pass post-render substitutes this
/// comment with the accumulated inner HTML, so nested
/// `routes/layout.tsx` files compose root → leaf without the
/// renderer ever holding the inner content in scope. The string is
/// chosen for vanishingly low collision risk with user-authored HTML
/// (deliberately ugly + double-underscore prefix matches the rest of
/// Albedo's internal markers).
pub const LAYOUT_CHILDREN_SENTINEL: &str = "<!--__ALBEDO_LAYOUT_CHILDREN__-->";

fn next_element_stable_id(module_spec: &str) -> u32 {
    RENDER_ELEMENT_COUNTER.with(|cell| {
        let counter = cell.get();
        cell.set(counter.wrapping_add(1));
        let key = format!("{module_spec}#{counter}");
        fnv1a_32(key.as_bytes())
    })
}

fn reset_element_counter() {
    RENDER_ELEMENT_COUNTER.with(|cell| cell.set(0));
}

/// A derived text/attribute binding collected during a Phase K render: a JSX
/// expression that reads ≥1 reactive slot but isn't a bare slot read
/// (`{count * 2}`, `{open ? 'on' : 'off'}`, `className={busy ? 'b' : ''}`). The
/// client recomputes `expr` from state whenever any `deps` slot changes and
/// re-applies it to `stable_id` — as text when `attr` is `None`, else as that
/// HTML attribute. Lowered to a JS thunk by `build_reactive_payload`.
#[derive(Debug, Clone)]
pub struct DerivedBindingRaw {
    pub stable_id: u32,
    pub attr: Option<String>,
    /// `(binding name, slot id)` for every reactive slot the expression reads.
    pub deps: Vec<(String, SlotId)>,
    pub expr: swc_ecma_ast::Expr,
}

thread_local! {
    /// Derived bindings collected during the current render. Reset at the top of
    /// every `render_entry_compiled*` call (like the element counter) and taken
    /// by `build_reactive_payload` right after the render returns. Renders that
    /// don't consume it just overwrite it next time — no leak, no wire impact.
    static DERIVED_OUT: RefCell<Vec<DerivedBindingRaw>> = const { RefCell::new(Vec::new()) };
}

fn reset_derived_bindings() {
    DERIVED_OUT.with(|cell| cell.borrow_mut().clear());
}

fn push_derived_binding(binding: DerivedBindingRaw) {
    DERIVED_OUT.with(|cell| cell.borrow_mut().push(binding));
}

/// Take the derived bindings collected by the most recent render on this thread.
pub fn take_phase_k_derived_bindings() -> Vec<DerivedBindingRaw> {
    DERIVED_OUT.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// A conditional subtree toggle collected during a Phase K render: a
/// `{cond && <static JSX>}` or `{cond ? <static A> : <static B>}` whose `cond`
/// is client-computable from reactive slots and whose branches are STATIC (no
/// inner slot reads, no `on*` handlers, no nested components). The client
/// recomputes `cond` from state whenever a dep slot changes and swaps the
/// wrapper element's `innerHTML` between the two pre-rendered branch HTMLs —
/// fine-grained structural reactivity with no component hydration. Lowered to a
/// derived binding (with `html: true`) by `build_reactive_payload`.
#[derive(Debug, Clone)]
pub struct ConditionalBindingRaw {
    /// `data-albedo-id` of the `display:contents` wrapper the renderer emitted.
    pub stable_id: u32,
    /// `(binding name, slot id)` for every reactive slot the test reads.
    pub deps: Vec<(String, SlotId)>,
    /// The boolean test expression (the `&&` left operand / ternary `test`).
    pub cond: swc_ecma_ast::Expr,
    /// HTML rendered when `cond` is truthy.
    pub html_true: String,
    /// HTML rendered when `cond` is falsy (empty string for `&&`).
    pub html_false: String,
}

/// A keyed-list (`.map()`) subtree collected during a Phase K render: an
/// `{ARRAY.map((item[, i]) => <static JSX>)}` whose `ARRAY` is client-computable
/// from reactive slots and whose per-item subtree is STATIC relative to the item
/// (host elements, no `on*` handlers, no nested components; `{expr}` holes that
/// reference only the item/index params and pure globals). The client recomputes
/// the whole list's HTML from state whenever a dep slot changes and swaps the
/// wrapper element's `innerHTML` — structural reactivity over a data-driven node
/// count with no component hydration. Lowered to a `html: true` derived binding
/// (a `.map(...).join('')` thunk) by `build_reactive_payload`.
///
/// First slice = coarse re-render: any change rebuilds all list DOM (correct for
/// static items, which carry no per-node state to lose). True keyed
/// reconciliation (preserve/reorder DOM identity, focus) is a documented
/// follow-up — the `key` prop is parsed past but not yet used.
#[derive(Debug, Clone)]
pub struct ListBindingRaw {
    /// `data-albedo-id` of the `display:contents` wrapper the renderer emitted.
    pub stable_id: u32,
    /// `(binding name, slot id)` for every reactive slot the array reads.
    pub deps: Vec<(String, SlotId)>,
    /// The array expression (with resolvable locals substituted), e.g. `items`.
    pub array: swc_ecma_ast::Expr,
    /// The arrow's first param name — the per-item binding (`item`).
    pub item_param: String,
    /// The arrow's optional second param name — the index binding (`i`).
    pub index_param: Option<String>,
    /// The arrow body: the per-item JSX element/fragment to template.
    pub item_body: swc_ecma_ast::Expr,
    /// The item's `key={…}` expression, when the root is a single host element
    /// carrying an explicit key. `Some` selects the **keyed reconcile lane**
    /// (`data-albedo-key` + `SetListRef` + `ReconcileList`); `None` keeps the
    /// coarse `.map().join('')` innerHTML tier — the correct tier for keyless /
    /// fragment-root lists, where index-keying would mis-reconcile a reorder.
    pub key_expr: Option<swc_ecma_ast::Expr>,
}

thread_local! {
    /// Conditional subtree toggles collected during the current render. Same
    /// lifecycle as `DERIVED_OUT`: reset at the top of each `render_entry_*`
    /// call, taken by `build_reactive_payload`.
    static CONDITIONAL_OUT: RefCell<Vec<ConditionalBindingRaw>> =
        const { RefCell::new(Vec::new()) };
    /// Keyed-list subtrees collected during the current render. Same lifecycle
    /// as `CONDITIONAL_OUT`.
    static LIST_OUT: RefCell<Vec<ListBindingRaw>> = const { RefCell::new(Vec::new()) };
    /// Set when the render hit a structural construct (a JSX conditional/list)
    /// that binding mode can't represent fine-grained — so the component must
    /// fall back to the A3 whole-component island rather than ship a stale or
    /// broken binding payload. Read by `build_reactive_payload`.
    static STRUCTURAL_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn reset_conditional_bindings() {
    CONDITIONAL_OUT.with(|cell| cell.borrow_mut().clear());
    LIST_OUT.with(|cell| cell.borrow_mut().clear());
    STRUCTURAL_FALLBACK.with(|cell| cell.set(false));
}

fn push_list_binding(binding: ListBindingRaw) {
    LIST_OUT.with(|cell| cell.borrow_mut().push(binding));
}

/// Take the keyed-list bindings collected by the most recent render.
pub fn take_phase_k_list_bindings() -> Vec<ListBindingRaw> {
    LIST_OUT.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn push_conditional_binding(binding: ConditionalBindingRaw) {
    CONDITIONAL_OUT.with(|cell| cell.borrow_mut().push(binding));
}

/// Take the conditional bindings collected by the most recent render.
pub fn take_phase_k_conditional_bindings() -> Vec<ConditionalBindingRaw> {
    CONDITIONAL_OUT.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn mark_structural_fallback() {
    STRUCTURAL_FALLBACK.with(|cell| cell.set(true));
}

/// Whether the most recent Phase K render saw a structural construct binding
/// mode can't represent — the signal for `build_reactive_payload` to fall back
/// to the A3 island path. Cleared at the start of each render.
pub fn phase_k_structural_fallback_required() -> bool {
    STRUCTURAL_FALLBACK.with(|cell| cell.get())
}

/// Analyse a JSX expression for a derived binding, substituting any resolvable
/// local (`useMemo` body / derived const) with its defining expression. Returns
/// `Some((resolved_expr, deps))` when, after substitution, the expression reads
/// ≥1 reactive slot AND every other variable identifier is a recognised pure
/// global — i.e. it is recomputable client-side from state alone. Returns `None`
/// when it reads no slot (static — leave it) or touches an unknown variable
/// (prop / unresolved local / ref / arbitrary call), conservatively skipping.
fn phase_k_collect_slot_deps(
    expr: &swc_ecma_ast::Expr,
) -> Option<(swc_ecma_ast::Expr, Vec<(String, SlotId)>)> {
    use swc_ecma_ast::Expr;
    use swc_ecma_visit::{Fold, FoldWith};

    const PURE_GLOBALS: &[&str] = &[
        "Math",
        "String",
        "Number",
        "Boolean",
        "JSON",
        "Array",
        "Object",
        "Date",
        "parseInt",
        "parseFloat",
        "isNaN",
        "isFinite",
        "undefined",
        "NaN",
        "Infinity",
    ];

    struct Resolver {
        deps: Vec<(String, SlotId)>,
        seen: HashSet<String>,
        unknown: bool,
        /// Resolution stack — guards against a local that (transitively)
        /// references itself.
        stack: Vec<String>,
    }

    impl Fold for Resolver {
        fn fold_expr(&mut self, expr: Expr) -> Expr {
            if let Expr::Ident(ident) = &expr {
                let name = ident.sym.to_string();
                if let Some(slot) = phase_k_slot_for_value(&name)
                    .or_else(|| phase_k_shared_slot_for_value(&name).map(|(slot, _)| slot))
                {
                    if self.seen.insert(name.clone()) {
                        self.deps.push((name, slot));
                    }
                    return expr;
                }
                if PURE_GLOBALS.contains(&name.as_str()) {
                    return expr;
                }
                if self.stack.iter().any(|n| n == &name) {
                    self.unknown = true;
                    return expr;
                }
                if let Some(def) = phase_k_resolve_local(&name) {
                    // Substitute the local with its (recursively resolved) def.
                    self.stack.push(name);
                    let folded = def.fold_with(self);
                    self.stack.pop();
                    return folded;
                }
                self.unknown = true;
                return expr;
            }
            expr.fold_children_with(self)
        }
    }

    let mut resolver = Resolver {
        deps: Vec::new(),
        seen: HashSet::new(),
        unknown: false,
        stack: Vec::new(),
    };
    let resolved = expr.clone().fold_with(&mut resolver);
    if resolver.unknown || resolver.deps.is_empty() {
        None
    } else {
        Some((resolved, resolver.deps))
    }
}

/// Strip enclosing parentheses so classifiers see the inner expression.
fn unwrap_paren(expr: &swc_ecma_ast::Expr) -> &swc_ecma_ast::Expr {
    let mut cur = expr;
    while let swc_ecma_ast::Expr::Paren(p) = cur {
        cur = &p.expr;
    }
    cur
}

/// True for a branch that renders to nothing (`null`, `undefined`, `false`) —
/// the trailing arm of a `cond && <X/>` (modelled as a ternary alt) or an
/// explicit `cond ? <X/> : null`.
fn is_empty_branch(expr: &swc_ecma_ast::Expr) -> bool {
    use swc_ecma_ast::{Expr, Lit};
    match unwrap_paren(expr) {
        Expr::Lit(Lit::Null(_)) => true,
        Expr::Ident(id) => id.sym.as_ref() == "undefined",
        Expr::Lit(Lit::Bool(b)) => !b.value,
        _ => false,
    }
}

/// One JSX-bearing conditional in child position the binding-mode renderer can
/// reason about structurally.
enum JsxConditional<'a> {
    /// `cond && <JSX>` — show the branch when `cond` is truthy.
    And {
        cond: &'a swc_ecma_ast::Expr,
        branch: &'a swc_ecma_ast::Expr,
    },
    /// `cond ? <JSX|null> : <JSX|null>` — at least one arm is JSX.
    Ternary {
        test: &'a swc_ecma_ast::Expr,
        cons: &'a swc_ecma_ast::Expr,
        alt: &'a swc_ecma_ast::Expr,
    },
}

/// True when an expression is a JSX element or fragment (the thing a branch
/// must be for this to be a *structural* conditional rather than a derived-text
/// one like `{cond && "label"}`, which the derived rung already handles).
fn is_jsx_expr(expr: &swc_ecma_ast::Expr) -> bool {
    matches!(
        unwrap_paren(expr),
        swc_ecma_ast::Expr::JSXElement(_) | swc_ecma_ast::Expr::JSXFragment(_)
    )
}

/// Recognise a child expression as a JSX-bearing conditional. Returns `None`
/// for non-conditionals and for conditionals with no JSX branch (those stay on
/// the derived-text path).
fn classify_jsx_conditional(expr: &swc_ecma_ast::Expr) -> Option<JsxConditional<'_>> {
    use swc_ecma_ast::{BinaryOp, Expr};
    match unwrap_paren(expr) {
        Expr::Bin(bin) if bin.op == BinaryOp::LogicalAnd && is_jsx_expr(&bin.right) => {
            Some(JsxConditional::And {
                cond: &bin.left,
                branch: &bin.right,
            })
        }
        Expr::Cond(cond) if is_jsx_expr(&cond.cons) || is_jsx_expr(&cond.alt) => {
            Some(JsxConditional::Ternary {
                test: &cond.test,
                cons: &cond.cons,
                alt: &cond.alt,
            })
        }
        _ => None,
    }
}

/// True when a JSX branch is *static* — safe to pre-render once and toggle
/// wholesale via `innerHTML`. Static means: host (lowercase) elements only (no
/// component refs, no `<Link>`/intrinsics), no `on*` handlers, no spread attrs,
/// attribute values that are string literals only, and children that are plain
/// text or nested static elements. No `{expr}` containers — so the subtree
/// reads no reactive slots and emits no bindings, which is exactly what makes a
/// whole-subtree swap correct (nothing inside needs re-binding) and crash-free
/// (it never dereferences possibly-null state, the `{user && <X name={user.x}/>}`
/// hazard). Anything else returns `false` → the component falls back to A3.
fn is_static_branch(expr: &swc_ecma_ast::Expr) -> bool {
    use swc_ecma_ast::Expr;
    match unwrap_paren(expr) {
        Expr::JSXElement(el) => is_static_jsx_element(el),
        Expr::JSXFragment(frag) => frag.children.iter().all(is_static_jsx_child),
        // An empty arm (`null`) is trivially static.
        e => is_empty_branch(e),
    }
}

fn is_static_jsx_element(el: &swc_ecma_ast::JSXElement) -> bool {
    use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElementName};
    // Host element only — a component ref needs hydration, not a static swap.
    let JSXElementName::Ident(tag) = &el.opening.name else {
        return false;
    };
    let name = tag.sym.as_ref();
    if !is_host_tag(name) {
        return false;
    }
    for attr in &el.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr else {
            return false; // spread → not static
        };
        let JSXAttrName::Ident(attr_name) = &jsx_attr.name else {
            return false;
        };
        let an = attr_name.sym.as_ref();
        if an.starts_with("on") && an.len() > 2 {
            return false; // event handler → needs binding
        }
        match &jsx_attr.value {
            None => {}                       // bare boolean attr
            Some(JSXAttrValue::Lit(_)) => {} // string literal
            _ => return false,               // `{expr}` value → reads state
        }
    }
    el.children.iter().all(is_static_jsx_child)
}

fn is_static_jsx_child(child: &swc_ecma_ast::JSXElementChild) -> bool {
    use swc_ecma_ast::JSXElementChild;
    match child {
        JSXElementChild::JSXText(_) => true,
        JSXElementChild::JSXElement(el) => is_static_jsx_element(el),
        JSXElementChild::JSXFragment(frag) => frag.children.iter().all(is_static_jsx_child),
        // `{expr}` children read state / emit bindings → not a static subtree.
        _ => false,
    }
}

/// True for a lowercase host tag that isn't a renderer intrinsic. `<Link>` and
/// `<children/>` get special rewrites elsewhere, so exclude them from the
/// static-swap path (they're not plain host markup).
fn is_host_tag(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && name != "children"
}

/// One keyed-list (`.map()`) call in child position the binding-mode renderer can
/// reason about. Borrows the array, the arrow's param names, and the per-item
/// JSX body. Returns `None` for anything that isn't `ARRAY.map(arrow)` with a
/// JSX-bearing expression-body arrow taking one or two identifier params.
struct JsxList<'a> {
    array: &'a swc_ecma_ast::Expr,
    item_param: String,
    index_param: Option<String>,
    body: &'a swc_ecma_ast::Expr,
}

fn classify_jsx_list(expr: &swc_ecma_ast::Expr) -> Option<JsxList<'_>> {
    use swc_ecma_ast::{BlockStmtOrExpr, Callee, Expr, MemberProp, Pat};
    let Expr::Call(call) = unwrap_paren(expr) else {
        return None;
    };
    // Callee must be a `.map` member access; the object is the array expression.
    let Callee::Expr(callee) = &call.callee else {
        return None;
    };
    let Expr::Member(member) = unwrap_paren(callee) else {
        return None;
    };
    let MemberProp::Ident(method) = &member.prop else {
        return None;
    };
    if method.sym.as_ref() != "map" {
        return None;
    }
    // Exactly one callback argument, an arrow with 1-2 identifier params.
    if call.args.len() != 1 || call.args[0].spread.is_some() {
        return None;
    }
    let Expr::Arrow(arrow) = unwrap_paren(&call.args[0].expr) else {
        return None;
    };
    if arrow.params.is_empty() || arrow.params.len() > 2 {
        return None;
    }
    let param_ident = |pat: &Pat| -> Option<String> {
        match pat {
            Pat::Ident(id) => Some(id.id.sym.to_string()),
            _ => None, // destructured params aren't handled in this slice
        }
    };
    let item_param = param_ident(&arrow.params[0])?;
    let index_param = match arrow.params.get(1) {
        Some(p) => Some(param_ident(p)?),
        None => None,
    };
    // Expression-body arrow returning JSX (the common `item => <li>…</li>`).
    let BlockStmtOrExpr::Expr(body) = &*arrow.body else {
        return None;
    };
    if !is_jsx_expr(body) {
        return None;
    }
    Some(JsxList {
        array: &member.obj,
        item_param,
        index_param,
        body,
    })
}

/// Extract a list item's `key={EXPR}` expression, when the item body is a
/// single host `JSXElement` carrying an explicit key. Returns `None` for a
/// keyless item or a fragment root — those stay on the coarse innerHTML tier
/// (see [`ListBindingRaw::key_expr`]). Bare `key="literal"` is skipped: a static
/// key can't distinguish rows, so it isn't a usable reconciliation identity.
fn extract_list_key_expr(body: &swc_ecma_ast::Expr) -> Option<swc_ecma_ast::Expr> {
    use swc_ecma_ast::{Expr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXExpr};
    let Expr::JSXElement(el) = unwrap_paren(body) else {
        return None;
    };
    for attr in &el.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr else {
            continue;
        };
        let JSXAttrName::Ident(name) = &jsx_attr.name else {
            continue;
        };
        if name.sym.as_ref() != "key" {
            continue;
        }
        if let Some(JSXAttrValue::JSXExprContainer(container)) = &jsx_attr.value {
            if let JSXExpr::Expr(expr) = &container.expr {
                return Some((**expr).clone());
            }
        }
    }
    None
}

/// True when a per-item JSX subtree is *static relative to the item*: host
/// elements only (no components, no `<Link>`), no `on*` handlers, no spreads,
/// and every `{expr}` hole (child or attribute value) references only the item
/// param, the index param, or a recognised pure global. Such a subtree can be
/// regenerated wholesale from live item data with a templated `innerHTML` swap;
/// anything richer (handlers, components, reads of OTHER component state) →
/// A3 fallback.
fn is_static_list_item(
    expr: &swc_ecma_ast::Expr,
    item_param: &str,
    index_param: Option<&str>,
) -> bool {
    use swc_ecma_ast::Expr;
    match unwrap_paren(expr) {
        Expr::JSXElement(el) => is_static_list_element(el, item_param, index_param),
        Expr::JSXFragment(frag) => frag
            .children
            .iter()
            .all(|c| is_static_list_child(c, item_param, index_param)),
        _ => false,
    }
}

fn is_static_list_element(
    el: &swc_ecma_ast::JSXElement,
    item_param: &str,
    index_param: Option<&str>,
) -> bool {
    use swc_ecma_ast::{JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElementName, JSXExpr};
    let JSXElementName::Ident(tag) = &el.opening.name else {
        return false;
    };
    if !is_host_tag(tag.sym.as_ref()) {
        return false;
    }
    for attr in &el.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr else {
            return false; // spread → not representable
        };
        let JSXAttrName::Ident(attr_name) = &jsx_attr.name else {
            return false;
        };
        let an = attr_name.sym.as_ref();
        if an.starts_with("on") && an.len() > 2 {
            return false; // event handler inside a list item → needs an island
        }
        match &jsx_attr.value {
            None => {}                       // bare boolean attr
            Some(JSXAttrValue::Lit(_)) => {} // string literal
            Some(JSXAttrValue::JSXExprContainer(c)) => {
                let JSXExpr::Expr(e) = &c.expr else {
                    return false;
                };
                if !item_expr_only_refs(e, item_param, index_param) {
                    return false;
                }
            }
            _ => return false, // JSX-valued attr etc.
        }
    }
    el.children
        .iter()
        .all(|c| is_static_list_child(c, item_param, index_param))
}

fn is_static_list_child(
    child: &swc_ecma_ast::JSXElementChild,
    item_param: &str,
    index_param: Option<&str>,
) -> bool {
    use swc_ecma_ast::{JSXElementChild, JSXExpr};
    match child {
        JSXElementChild::JSXText(_) => true,
        JSXElementChild::JSXElement(el) => is_static_list_element(el, item_param, index_param),
        JSXElementChild::JSXFragment(frag) => frag
            .children
            .iter()
            .all(|c| is_static_list_child(c, item_param, index_param)),
        JSXElementChild::JSXExprContainer(c) => match &c.expr {
            JSXExpr::Expr(e) => item_expr_only_refs(e, item_param, index_param),
            JSXExpr::JSXEmptyExpr(_) => true,
        },
        _ => false,
    }
}

/// True when every free identifier in `expr` (in value position) is the item
/// param, the index param, or a recognised pure global. Member-property names
/// (`item.name`) are not `Expr::Ident`s so they aren't visited — exactly what we
/// want, so `item.price * 2` and `Math.round(item.x)` pass while a read of an
/// outer slot/prop/ref fails. Nested JSX (a component or element with its own
/// state) also fails (its tag/handlers surface as non-item idents or are
/// rejected structurally upstream).
fn item_expr_only_refs(
    expr: &swc_ecma_ast::Expr,
    item_param: &str,
    index_param: Option<&str>,
) -> bool {
    use swc_ecma_visit::{Visit, VisitWith};

    const PURE_GLOBALS: &[&str] = &[
        "Math",
        "String",
        "Number",
        "Boolean",
        "JSON",
        "Array",
        "Object",
        "Date",
        "parseInt",
        "parseFloat",
        "isNaN",
        "isFinite",
        "undefined",
        "NaN",
        "Infinity",
    ];

    struct Check<'a> {
        item_param: &'a str,
        index_param: Option<&'a str>,
        ok: bool,
        has_jsx: bool,
    }
    impl Visit for Check<'_> {
        fn visit_expr(&mut self, e: &swc_ecma_ast::Expr) {
            use swc_ecma_ast::Expr;
            match e {
                // Value-position identifier — the only place a free variable can
                // appear. Member-property names (`item.name`) are `IdentName`s
                // inside `MemberProp`, never `Expr::Ident`, so they're skipped.
                Expr::Ident(id) => {
                    let name = id.sym.as_ref();
                    if !(name == self.item_param
                        || self.index_param == Some(name)
                        || PURE_GLOBALS.contains(&name))
                    {
                        self.ok = false;
                    }
                }
                // Nested JSX inside an item hole isn't templated in this slice.
                Expr::JSXElement(_) | Expr::JSXFragment(_) => self.has_jsx = true,
                other => other.visit_children_with(self),
            }
        }
    }

    let mut check = Check {
        item_param,
        index_param,
        ok: true,
        has_jsx: false,
    };
    expr.visit_with(&mut check);
    check.ok && !check.has_jsx
}

// ─────────────────────────────────────────────────────────────────────
// Phase K · hook-compile thread-local state
//
// `RENDER_K` is populated by `render_entry_compiled` and `eval_handler_body`
// for the duration of one render or handler dispatch. It threads:
//   * the session slot view (for slot reads and writes),
//   * the accumulator for binding opcodes (`BindEvent`, `SetTextRef`),
//   * a stack of containing element `data-albedo-id`s (so a slot read in JSX text-position knows
//     which element to subscribe), and
//   * a stack of per-component hook scopes (so identifier lookup resolves slot-bound names against
//     the right metadata).
// ─────────────────────────────────────────────────────────────────────

thread_local! {
    static RENDER_K: RefCell<Option<RenderKState>> = const { RefCell::new(None) };
}

struct RenderKState {
    slots: SessionSlotView,
    opcodes: Vec<Instruction>,
    element_stack: Vec<u32>,
    scopes: Vec<ComponentScope>,
    /// Render-scoped event intern table. Allocation order is "first
    /// appearance" of each unique event name, starting at id 1 (0 is
    /// reserved for an unset/sentinel id elsewhere in the substrate).
    /// `drain_phase_k_opcodes` prepends a single
    /// `InitInternTable { kind: Event, entries: ... }` opcode so
    /// bakabox can resolve the event_id every `BindEvent` references.
    event_intern: HashMap<String, u16>,
    event_intern_order: Vec<String>,
    /// Render-scoped attribute intern table, symmetric to the event one.
    /// Allocation order is first-appearance of each unique HTML attribute
    /// name (e.g. `class`), starting at id 1. `drain_phase_k_opcodes`
    /// prepends an `InitInternTable { kind: Attr, .. }` so bakabox can
    /// resolve the `attr_id` every `SetAttrRef` references.
    attr_intern: HashMap<String, u16>,
    attr_intern_order: Vec<String>,
}

#[derive(Clone)]
struct ComponentScope {
    module_spec: String,
    function_name: String,
    /// Map from value-binding name (`n`) → slot id holding its value.
    value_slots: HashMap<String, SlotId>,
    /// Map from setter-binding name (`setN`) → slot id whose value is
    /// overwritten when the setter is called.
    setter_slots: HashMap<String, SlotId>,
    /// Handler proxy_ids in source order. Indexed by `handlers_emitted`
    /// as the renderer encounters JSX `on*` attributes.
    proxy_ids: Vec<u32>,
    /// Cursor into `proxy_ids` advanced as handlers are emitted.
    handlers_emitted: usize,
    /// Initial-value expressions for each hook in source order. Used
    /// when a slot has not been written yet (first render) to derive
    /// the initial value via the existing Phase-J interpreter.
    initials: Vec<swc_ecma_ast::Expr>,
    /// Map from value-binding name to its position in `initials`. Used
    /// during useState destructure to look up the initial.
    hook_index_for_value: HashMap<String, usize>,
    /// Stage 2 — captured-prop slot ids per prop name. When set, the
    /// renderer writes the current value of each captured prop to its
    /// slot on every render of this component, and
    /// `eval_handler_body` seeds the handler env from these slots so
    /// the handler closure can reference the captured prop.
    capture_slots: HashMap<String, SlotId>,
    /// Phase O.2 — `useSharedSlot` bindings. Map from binding name to
    /// (broadcast slot id, topic key). The renderer reads the current
    /// value via the broadcast registry on first encounter, and
    /// `phase_k_detect_slot_text_read` reports the broadcast slot id
    /// so the emitted `SetTextRef` opcode lines up with future
    /// broadcast fan-outs targeting the same id.
    shared_slots: HashMap<String, (SlotId, String)>,
    /// Step 3 (derived bindings) — resolvable local defs (`useMemo` bodies /
    /// plain derived consts) keyed by name, so a JSX `{doubled}` can be
    /// substituted with its defining expression during derived-binding analysis.
    derived_locals: HashMap<String, swc_ecma_ast::Expr>,
}

fn phase_k_enabled() -> bool {
    RENDER_K.with(|cell| cell.borrow().is_some())
}

fn phase_k_push_scope(scope: ComponentScope) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scopes.push(scope);
        }
    });
}

fn phase_k_pop_scope() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scopes.pop();
        }
    });
}

fn phase_k_push_element(stable_id: u32) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.element_stack.push(stable_id);
        }
    });
}

fn phase_k_pop_element() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.element_stack.pop();
        }
    });
}

fn phase_k_top_element() -> Option<u32> {
    RENDER_K.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|state| state.element_stack.last().copied())
    })
}

fn phase_k_emit(op: Instruction) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.opcodes.push(op);
        }
    });
}

fn phase_k_slot_for_value(name: &str) -> Option<SlotId> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.value_slots.get(name).copied())
        })
    })
}

/// Resolve a JSX-referenced local (`useMemo` body / plain derived const) to its
/// defining expression in the current component scope. `None` when `name` isn't
/// a recognised derived local. See `ComponentScope::derived_locals`.
fn phase_k_resolve_local(name: &str) -> Option<swc_ecma_ast::Expr> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.derived_locals.get(name).cloned())
        })
    })
}

fn phase_k_slot_for_setter(name: &str) -> Option<SlotId> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.setter_slots.get(name).copied())
        })
    })
}

fn phase_k_next_proxy_id_for_event(event_name: &str) -> Option<u32> {
    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow.as_mut()?;
        let scope = state.scopes.last_mut()?;
        // The compile pass laid down proxy_ids in source-traversal
        // order; we advance one per emit and verify the recorded
        // event_name matches what the render is asking for. A
        // mismatch means the event order drifted between extraction
        // and render — fail loud rather than silently misroute.
        let idx = scope.handlers_emitted;
        let proxy_id = *scope.proxy_ids.get(idx)?;
        scope.handlers_emitted = idx + 1;
        // Belt-and-suspenders: re-derive the proxy_id from the
        // scope's identity + this event_name + idx, and assert it
        // matches. Mismatch is a programming error in the extractor.
        let derived = allocate_proxy_id(
            &scope.module_spec,
            &scope.function_name,
            event_name,
            idx,
        );
        debug_assert_eq!(
            proxy_id, derived,
            "phase K proxy_id drift: recorded {proxy_id} but derived {derived} for {}::{}::{event_name}#{idx}",
            scope.module_spec, scope.function_name,
        );
        Some(proxy_id)
    })
}

fn phase_k_read_slot_value(slot_id: SlotId) -> Option<Vec<u8>> {
    RENDER_K.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|state| state.slots.read(slot_id))
    })
}

fn phase_k_write_slot_value(slot_id: SlotId, bytes: Vec<u8>) {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow().as_ref() {
            state.slots.write(slot_id, bytes);
        }
    });
}

fn phase_k_current_hook_initial(value_name: &str) -> Option<swc_ecma_ast::Expr> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            let scope = state.scopes.last()?;
            let idx = scope.hook_index_for_value.get(value_name).copied()?;
            scope.initials.get(idx).cloned()
        })
    })
}

// ─────────────────────────────────────────────────────────────────────
// Phase L · form-action scope stack
//
// `FORM_ACTION_STACK` records the action name of every
// `<form action="action:NAME">` the renderer is currently inside, in
// nesting order. Descendant field elements (input / select /
// textarea) consult the top of the stack to decide whether to
// auto-emit a `data-albedo-error` span as their sibling — the span
// is what server-side validation patches target via `SetText`
// opcodes addressed by `allocate_field_error_id(action, field)`.
//
// Thread-local (single-threaded per render call) so concurrent
// renders on other threads cannot observe each other's form scopes.
// Modelled on the existing element / scope stacks above.
// ─────────────────────────────────────────────────────────────────────

thread_local! {
    static FORM_ACTION_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Push a `<form action="action:NAME">` scope. The renderer calls
/// this just before recursing into the form's children and pairs it
/// with [`pop_form_action_scope`] after.
fn push_form_action_scope(action_name: String) {
    FORM_ACTION_STACK.with(|stack| stack.borrow_mut().push(action_name));
}

/// Pop the most recent form-action scope. Idempotent on an empty
/// stack — defensive for early-return paths in eval_jsx_element that
/// might skip the push (none today, but kept safe by construction).
fn pop_form_action_scope() {
    FORM_ACTION_STACK.with(|stack| {
        let _ = stack.borrow_mut().pop();
    });
}

/// Returns the action name of the innermost form-action scope, or
/// `None` when the renderer is not inside one. Used by field
/// elements to decide whether to emit a sibling error span.
fn current_form_action_scope() -> Option<String> {
    FORM_ACTION_STACK.with(|stack| stack.borrow().last().cloned())
}

/// RAII installer/restorer for the Phase-K thread-local state. Even
/// on panic, the previous (typically `None`) state is reinstated so
/// concurrent renderers on the same thread don't observe leaked state.
struct PhaseKGuard {
    previous: Option<RenderKState>,
}

impl PhaseKGuard {
    fn install(slots: SessionSlotView) -> Self {
        let previous = RENDER_K.with(|cell| {
            cell.replace(Some(RenderKState {
                slots,
                opcodes: Vec::new(),
                element_stack: Vec::new(),
                scopes: Vec::new(),
                event_intern: HashMap::new(),
                event_intern_order: Vec::new(),
                attr_intern: HashMap::new(),
                attr_intern_order: Vec::new(),
            }))
        });
        Self { previous }
    }
}

impl Drop for PhaseKGuard {
    fn drop(&mut self) {
        RENDER_K.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

// `render_local` resolves the current component's scope via
// `current_phase_k_component`, which reads from a thread-local raw
// pointer to the active `CompiledProject` (installed below). The
// pointer is the right trade for keeping the Phase-J `render_*`
// signatures untouched; thread-local-by-pointer is safe because the
// borrow is single-threaded and the guard is dropped before the
// reference goes out of scope on the calling stack frame.
thread_local! {
    static PHASE_K_PROJECT: Cell<Option<*const CompiledProject>> = const { Cell::new(None) };
}

fn install_phase_k_project(project: &CompiledProject) -> PhaseKProjectGuard {
    let previous = PHASE_K_PROJECT.with(|cell| cell.replace(Some(project as *const _)));
    PhaseKProjectGuard { previous }
}

struct PhaseKProjectGuard {
    previous: Option<*const CompiledProject>,
}

impl Drop for PhaseKProjectGuard {
    fn drop(&mut self) {
        PHASE_K_PROJECT.with(|cell| cell.set(self.previous));
    }
}

fn current_phase_k_component(module_spec: &str, function_name: &str) -> Option<ComponentScope> {
    PHASE_K_PROJECT.with(|cell| {
        let ptr = cell.get()?;
        // Safety: the project reference is alive for the duration of
        // the render — `render_entry_compiled` holds `&CompiledProject`
        // on its stack frame while the eval runs, and the guard is
        // dropped before that frame returns. No concurrent mutation
        // is possible because access is thread-local.
        let project = unsafe { &*ptr };
        let meta = project.component_meta(module_spec, function_name)?;
        let shared_slots = meta
            .shared_slots
            .iter()
            .map(|binding| {
                (
                    binding.binding_name.clone(),
                    (
                        crate::runtime::broadcast::broadcast_slot_id(&binding.topic),
                        binding.topic.clone(),
                    ),
                )
            })
            .collect();
        Some(ComponentScope {
            module_spec: meta.module_spec.clone(),
            function_name: meta.function_name.clone(),
            value_slots: meta.value_slots.clone(),
            setter_slots: meta.setter_slots.clone(),
            proxy_ids: meta.proxy_ids.clone(),
            handlers_emitted: 0,
            initials: meta.hooks.iter().map(|h| h.initial.clone()).collect(),
            hook_index_for_value: meta
                .hooks
                .iter()
                .map(|h| (h.value_name.clone(), h.hook_idx))
                .collect(),
            capture_slots: meta.capture_slots.clone(),
            shared_slots,
            derived_locals: meta.derived_locals.clone(),
        })
    })
}

// ─────────────────────────────────────────────────────────────────────
// Phase O.2 · broadcast registry thread-local
//
// Mirrors `PHASE_K_PROJECT`: a raw pointer to the current
// `BroadcastRegistry` installed by `render_entry_compiled_with_broadcast`
// for the duration of one render. Required because `useSharedSlot`
// resolves to the topic's current value via the registry at render
// time, and the existing `render_entry_*` signatures predate the
// registry. Lifetime is bound by the install guard — the caller
// holds an `&BroadcastRegistry` while the guard exists.
// ─────────────────────────────────────────────────────────────────────

thread_local! {
    static PHASE_K_BROADCAST: Cell<Option<*const crate::runtime::broadcast::BroadcastRegistry>> =
        const { Cell::new(None) };
}

/// Phase P · Stream C.2 — peel `(expr)` and TS-only `as` / `satisfies`
/// wrappers off an updater closure argument so `broadcast(topic, (x =>
/// next))` and `broadcast(topic, (x => next) as any)` both reach the
/// inner Arrow/Fn unchanged. Mirrors `transforms::actions::unwrap_parens`
/// — the action authoring path lets the user wrap an updater in
/// `(...)` or a TS cast for the same reasons.
fn unwrap_updater_parens(expr: &swc_ecma_ast::Expr) -> &swc_ecma_ast::Expr {
    use swc_ecma_ast::Expr;
    match expr {
        Expr::Paren(p) => unwrap_updater_parens(&p.expr),
        Expr::TsAs(ts_as) => unwrap_updater_parens(&ts_as.expr),
        Expr::TsSatisfies(ts_sat) => unwrap_updater_parens(&ts_sat.expr),
        Expr::TsConstAssertion(ts_const) => unwrap_updater_parens(&ts_const.expr),
        other => other,
    }
}

// ── Phase P · Stream E.3 — CSS modules per-render thread-local ────
//
// CSS module class maps are owned by `CompiledProject` (see
// `CssModuleRegistry`). The renderer installs a pointer to that
// registry on the per-thread Phase K stack before walking a module,
// and `eval_member` intercepts `Ident("styles").className` when the
// ident resolves to a CSS-module import binding for the current
// `module_spec`. Returns the scoped class name as `Value::String`.
// Lifetime: the install guard outlives every borrow because the
// installer and the render call run on the same stack frame, same
// contract as `PHASE_K_BROADCAST`.
// ───────────────────────────────────────────────────────────────────

thread_local! {
    static PHASE_K_CSS_MODULES: Cell<
        Option<*const crate::runtime::compiled::CssModuleRegistry>,
    > = const { Cell::new(None) };
}

pub(crate) fn install_phase_k_css_modules(
    registry: &crate::runtime::compiled::CssModuleRegistry,
) -> PhaseKCssModulesGuard {
    let previous = PHASE_K_CSS_MODULES.with(|cell| cell.replace(Some(registry as *const _)));
    PhaseKCssModulesGuard { previous }
}

pub(crate) struct PhaseKCssModulesGuard {
    previous: Option<*const crate::runtime::compiled::CssModuleRegistry>,
}

impl Drop for PhaseKCssModulesGuard {
    fn drop(&mut self) {
        PHASE_K_CSS_MODULES.with(|cell| cell.set(self.previous));
    }
}

fn current_phase_k_css_modules() -> Option<&'static crate::runtime::compiled::CssModuleRegistry> {
    PHASE_K_CSS_MODULES.with(|cell| {
        let ptr = cell.get()?;
        // Safety: same contract as `current_phase_k_broadcast`. The
        // installer is on the same stack frame as the render call;
        // the guard restores the previous pointer on drop.
        Some(unsafe { &*ptr })
    })
}

thread_local! {
    static PHASE_K_ISLAND_SKIP: Cell<
        Option<*const std::collections::HashSet<String>>,
    > = const { Cell::new(None) };
}

/// Install the set of component names that are separate hydration islands
/// (Tier-C) for the duration of a static render. A child component whose name
/// is in this set is NOT inlined into its parent's HTML — the renderer emits
/// nothing for it, so the island appears exactly once at its placeholder
/// anchor. The manifest builder installs this around its Tier-A static pass;
/// every other render path leaves it unset and inlines as before. RAII guard
/// restores the previous installation on drop, mirroring the Phase K stack.
pub(crate) fn install_island_skip_set(set: &std::collections::HashSet<String>) -> IslandSkipGuard {
    let previous = PHASE_K_ISLAND_SKIP.with(|cell| cell.replace(Some(set as *const _)));
    IslandSkipGuard { previous }
}

pub(crate) struct IslandSkipGuard {
    previous: Option<*const std::collections::HashSet<String>>,
}

impl Drop for IslandSkipGuard {
    fn drop(&mut self) {
        PHASE_K_ISLAND_SKIP.with(|cell| cell.set(self.previous));
    }
}

fn island_skip_contains(name: &str) -> bool {
    PHASE_K_ISLAND_SKIP.with(|cell| match cell.get() {
        // Safety: same stack-frame contract as the other Phase K thread-locals.
        Some(ptr) => unsafe { &*ptr }.contains(name),
        None => false,
    })
}

thread_local! {
    static LAYOUT_ISLAND_PLACEHOLDERS: Cell<
        Option<*const std::collections::HashMap<String, String>>,
    > = const { Cell::new(None) };
}

/// Install the `component-name → placeholder-id` map of Tier-C islands mounted
/// in a layout, for the duration of that layout's static render. A route's own
/// islands emit nothing here (their placeholder is collected separately into the
/// `<children />` slot); but a layout island has no children slot to fall back
/// to, so while this map is installed the skipped island emits its real
/// `<div id="…" data-albedo-tier="c"></div>` placeholder INLINE at its authored
/// position (masthead, footer, …). The serve path then replaces that exact div
/// with the hydrated island, anchoring it where the layout put it. RAII guard
/// restores the previous installation on drop.
pub(crate) fn install_layout_island_placeholders(
    map: &std::collections::HashMap<String, String>,
) -> LayoutIslandPlaceholderGuard {
    let previous = LAYOUT_ISLAND_PLACEHOLDERS.with(|cell| cell.replace(Some(map as *const _)));
    LayoutIslandPlaceholderGuard { previous }
}

pub(crate) struct LayoutIslandPlaceholderGuard {
    previous: Option<*const std::collections::HashMap<String, String>>,
}

impl Drop for LayoutIslandPlaceholderGuard {
    fn drop(&mut self) {
        LAYOUT_ISLAND_PLACEHOLDERS.with(|cell| cell.set(self.previous));
    }
}

/// Return the placeholder id for a layout-mounted Tier-C island by name, when a
/// layout-island map is installed and contains it. `None` everywhere else — so
/// outside a layout render (or for an island not collected as a layout island)
/// the skip branch keeps its historical emit-nothing behavior.
fn layout_island_placeholder_for(name: &str) -> Option<String> {
    LAYOUT_ISLAND_PLACEHOLDERS.with(|cell| match cell.get() {
        // Safety: same stack-frame contract as the other Phase K thread-locals.
        Some(ptr) => unsafe { &*ptr }.get(name).cloned(),
        None => None,
    })
}

/// Phase O.2 / Phase P · Stream C.2 — install a broadcast registry
/// onto the per-thread Phase K stack. The returned guard restores the
/// previous installation on drop (Phase K's other thread-locals follow
/// the same RAII shape so nested renders / dispatches don't clobber
/// each other).
///
/// Visible at `pub(crate)` so [`crate::runtime::compiled::CompiledProject`]
/// can wrap action dispatch with this guard from outside the
/// `eval::core` module. Keeping it crate-private (rather than `pub`)
/// preserves the thread-local plumbing as an internal contract;
/// userland goes through `invoke_action_with_broadcast` instead.
pub(crate) fn install_phase_k_broadcast(
    broadcast: &crate::runtime::broadcast::BroadcastRegistry,
) -> PhaseKBroadcastGuard {
    let previous = PHASE_K_BROADCAST.with(|cell| cell.replace(Some(broadcast as *const _)));
    PhaseKBroadcastGuard { previous }
}

pub(crate) struct PhaseKBroadcastGuard {
    previous: Option<*const crate::runtime::broadcast::BroadcastRegistry>,
}

impl Drop for PhaseKBroadcastGuard {
    fn drop(&mut self) {
        PHASE_K_BROADCAST.with(|cell| cell.set(self.previous));
    }
}

/// Resolve the current broadcast registry, if any. Returned reference
/// is bound to the caller's lifetime — the guard above keeps the
/// pointer valid for the duration of the render call.
fn current_phase_k_broadcast() -> Option<&'static crate::runtime::broadcast::BroadcastRegistry> {
    PHASE_K_BROADCAST.with(|cell| {
        let ptr = cell.get()?;
        // Safety: see `current_phase_k_component`. Same lifetime
        // contract — guard outlives every borrow because the
        // installer is on the same stack frame as the render.
        Some(unsafe { &*ptr })
    })
}

/// `(slot_id, topic)` for a binding declared via `useSharedSlot` in
/// the current component scope, or `None` when the name is not a
/// shared-slot binding.
fn phase_k_shared_slot_for_value(name: &str) -> Option<(SlotId, String)> {
    RENDER_K.with(|cell| {
        cell.borrow().as_ref().and_then(|state| {
            state
                .scopes
                .last()
                .and_then(|scope| scope.shared_slots.get(name).cloned())
        })
    })
}

fn drain_phase_k_opcodes() -> Vec<Instruction> {
    use crate::ir::opcode::{InternEntry, InternTable, InternTableKind};

    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let Some(state) = borrow.as_mut() else {
            return Vec::new();
        };
        let body = std::mem::take(&mut state.opcodes);

        // Prepend the event intern table so bakabox can resolve every
        // `event_id` carried by a `BindEvent` opcode below. The control
        // stream conventionally ships intern tables ahead of the
        // referencing opcodes; we honour the same ordering here.
        let mut out: Vec<Instruction> = Vec::with_capacity(body.len() + 2);
        if !state.event_intern_order.is_empty() {
            let entries: Vec<InternEntry> = state
                .event_intern_order
                .iter()
                .enumerate()
                .map(|(idx, name)| InternEntry {
                    id: (idx as u16).saturating_add(1),
                    value: name.clone(),
                })
                .collect();
            out.push(Instruction::InitInternTable {
                table: InternTable {
                    kind: InternTableKind::Event,
                    entries,
                },
            });
        }
        // Attr intern table — symmetric to the event one above, so bakabox can
        // resolve the `attr_id` every `SetAttrRef` references.
        if !state.attr_intern_order.is_empty() {
            let entries: Vec<InternEntry> = state
                .attr_intern_order
                .iter()
                .enumerate()
                .map(|(idx, name)| InternEntry {
                    id: (idx as u16).saturating_add(1),
                    value: name.clone(),
                })
                .collect();
            out.push(Instruction::InitInternTable {
                table: InternTable {
                    kind: InternTableKind::Attr,
                    entries,
                },
            });
        }
        out.extend(body);
        out
    })
}

/// `useState(...)` from `react` AND the current Phase-K scope knows
/// about it. Used by `eval_var_decl_into_env` to decide whether to
/// route through the slot store or fall through to the Phase-J shim.
fn is_use_state_in_phase_k_scope(call: &swc_ecma_ast::CallExpr) -> bool {
    use swc_ecma_ast::*;
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let Expr::Ident(ident) = callee.as_ref() else {
        return false;
    };
    ident.sym.as_ref() == "useState" && phase_k_enabled()
}

/// Phase O.2 — `useSharedSlot(...)` from `albedo` AND Phase-K is
/// active. Symmetric to `is_use_state_in_phase_k_scope`; the
/// extractor already validates the topic-literal shape so the
/// renderer doesn't re-check it.
fn is_use_shared_slot_in_phase_k_scope(call: &swc_ecma_ast::CallExpr) -> bool {
    use swc_ecma_ast::*;
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let Expr::Ident(ident) = callee.as_ref() else {
        return false;
    };
    ident.sym.as_ref() == "useSharedSlot" && phase_k_enabled()
}

/// Drain pending dirty entries WITHOUT producing opcodes. Used after
/// a first-render initialisation write so the initial value doesn't
/// show up as a user-driven mutation in the response frame.
fn drain_initial_slot_writes() {
    RENDER_K.with(|cell| {
        if let Some(state) = cell.borrow().as_ref() {
            let _ = state.slots.drain_pending();
        }
    });
}

/// Stage 2 — write the current value of every captured prop into its
/// dedicated capture slot. Called at the top of `render_local` so a
/// handler that fires before the next render still sees the value
/// the prop had on the most recent render.
///
/// Writes are drained immediately because they're internal
/// bookkeeping — surfacing them as `SlotSet` opcodes would push
/// every captured prop down to bakabox on every render, even when
/// the prop didn't change. Bakabox only needs `SlotSet` for slots
/// it has subscribed via `SetTextRef` / `SetAttrRef`; capture slots
/// are never subscribed.
fn snapshot_captured_props_into_slots(scope: &ComponentScope, props: &Value) {
    if scope.capture_slots.is_empty() {
        return;
    }
    let Some(props_map) = props.as_object() else {
        return;
    };
    for (name, slot_id) in &scope.capture_slots {
        let Some(value) = props_map.get(name) else {
            continue;
        };
        if let Ok(bytes) = serde_json::to_vec(value) {
            phase_k_write_slot_value(*slot_id, bytes);
        }
    }
    drain_initial_slot_writes();
}

/// Detect whether the expression in a JSX text-position child is a
/// bare slot-bound identifier (e.g. `{n}` for `const [n, setN] =
/// useState(0)`, or `{messages}` for `const messages =
/// useSharedSlot("topic")`). Returns the SlotId when so, signalling
/// that the renderer should emit a `SetTextRef` binding for the
/// containing element so bakabox re-applies the value when a future
/// `SlotSet` for this id arrives — whether from per-session writes
/// (Phase H) or broadcast fan-out (Phase O.2). Phase K Stage 1 only
/// recognises the simple shape; member access (`state.value`),
/// arithmetic, and method calls are Phase J reads and don't subscribe
/// to slot changes.
fn phase_k_detect_slot_text_read(expr: &swc_ecma_ast::Expr) -> Option<SlotId> {
    use swc_ecma_ast::*;
    match expr {
        Expr::Ident(ident) => {
            let name = ident.sym.to_string();
            // Per-session useState slot first; shared-slot is the
            // fallback so a name collision between the two surfaces
            // resolves to whichever the user declared LAST — which is
            // the same conflict resolution JavaScript applies to
            // shadowed bindings, and is what the developer most
            // likely intended.
            if let Some(slot_id) = phase_k_slot_for_value(&name) {
                return Some(slot_id);
            }
            phase_k_shared_slot_for_value(&name).map(|(slot_id, _topic)| slot_id)
        }
        Expr::Paren(paren) => phase_k_detect_slot_text_read(&paren.expr),
        Expr::TsAs(node) => phase_k_detect_slot_text_read(&node.expr),
        Expr::TsNonNull(node) => phase_k_detect_slot_text_read(&node.expr),
        Expr::TsTypeAssertion(node) => phase_k_detect_slot_text_read(&node.expr),
        _ => None,
    }
}

/// Render-scoped event interner. Allocates ids in first-appearance
/// order starting at 1; id 0 is reserved as a sentinel. Bakabox
/// resolves event_id → name through the `InitInternTable` opcode the
/// drain step prepends, so the id only needs to be unique within one
/// render frame.
fn phase_k_event_id_for(event_name: &str) -> crate::ir::opcode::EventId {
    use crate::ir::opcode::EventId;
    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = match borrow.as_mut() {
            Some(state) => state,
            None => return EventId(0),
        };
        if let Some(id) = state.event_intern.get(event_name) {
            return EventId(*id);
        }
        // Next free id; +1 because 0 is reserved.
        let id = (state.event_intern_order.len() as u16).saturating_add(1);
        state.event_intern.insert(event_name.to_string(), id);
        state.event_intern_order.push(event_name.to_string());
        EventId(id)
    })
}

/// Intern an HTML attribute name to a render-scoped `AttrId`, recording it for
/// the `InitInternTable { kind: Attr }` `drain_phase_k_opcodes` prepends.
/// Symmetric to [`phase_k_event_id_for`].
fn phase_k_attr_id_for(attr_name: &str) -> crate::ir::opcode::AttrId {
    use crate::ir::opcode::AttrId;
    RENDER_K.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = match borrow.as_mut() {
            Some(state) => state,
            None => return AttrId(0),
        };
        if let Some(id) = state.attr_intern.get(attr_name) {
            return AttrId(*id);
        }
        let id = (state.attr_intern_order.len() as u16).saturating_add(1);
        state.attr_intern.insert(attr_name.to_string(), id);
        state.attr_intern_order.push(attr_name.to_string());
        AttrId(id)
    })
}
use crate::runtime::eval::expr::{
    apply_var_pat_to_env, bind_params, bind_params_positional, param_from_pat,
    parse_module as parse_module_impl, ParamBinding, ParsedModule,
};

/// `true` when any component of `path` is a `node_modules` directory. Those
/// trees belong to the npm bundler (`bundler::npm`), not the component walk.
fn path_is_in_node_modules(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "node_modules")
}

#[derive(Debug, Clone)]
pub struct ComponentProject {
    root: PathBuf,
    modules: HashMap<String, ParsedModule>,
    source_hashes: HashMap<String, u64>,
    /// Raw module source, keyed by specifier. Retained (rather than dropped
    /// after parse) so the A1 host-object bridge can feed a component's
    /// original TSX through the QuickJS engine's own transpile + load pipeline
    /// for a `render_entry_quickjs` render. Kept in lock-step with `modules`
    /// across `load_from_dir` and `patch`.
    sources: HashMap<String, String>,
    specifier_to_id: HashMap<String, ComponentId>,
    next_id: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PatchReport {
    pub reparsed: usize,
    pub skipped_unchanged: usize,
    pub deleted: usize,
    pub reparsed_ids: Vec<ComponentId>,
    pub reparsed_specifiers: Vec<String>,
    pub deleted_ids: Vec<ComponentId>,
    pub deleted_specifiers: Vec<String>,
}

impl ComponentProject {
    pub fn load_from_dir(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let mut modules = HashMap::new();
        let mut source_hashes = HashMap::new();
        let mut sources = HashMap::new();
        let mut specifier_to_id: HashMap<String, ComponentId> = HashMap::new();
        let mut next_id: u64 = 0;

        for entry in WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            // npm dependencies are bundled through `bundler::npm`, never
            // ingested as project components (a node_modules tree under the
            // project root would otherwise be walked wholesale).
            if path_is_in_node_modules(path) {
                continue;
            }

            if !is_component_module(path) {
                continue;
            }

            let relative = path
                .strip_prefix(&root)
                .map_err(|err| anyhow!("failed to compute module path: {err}"))?;
            let specifier = normalize_specifier(relative);
            let source = std::fs::read_to_string(path)
                .map_err(|err| anyhow!("failed to read '{}': {err}", path.display()))?;
            let parsed = parse_module_impl(&source, path)?;
            source_hashes.insert(specifier.clone(), fnv1a_hash(source.as_bytes()));
            specifier_to_id.insert(specifier.clone(), ComponentId::new(next_id));
            next_id += 1;
            sources.insert(specifier.clone(), source);
            modules.insert(specifier, parsed);
        }

        if modules.is_empty() {
            return Err(anyhow!("no components found under '{}'", root.display()));
        }

        Ok(Self {
            root,
            modules,
            source_hashes,
            sources,
            specifier_to_id,
            next_id,
        })
    }

    /// Raw TSX/JSX source for a module specifier, if known. Used by the A1
    /// host-object render bridge to load a component into the QuickJS engine.
    pub fn module_source(&self, specifier: &str) -> Option<&str> {
        let spec = normalize_slashes(specifier);
        self.sources.get(&spec).map(String::as_str)
    }

    /// Resolve a render `entry` to its `(module_spec, default_export_fn)`.
    /// Mirrors the resolution [`Self::render_entry`] does internally, exposed
    /// for the A1 host-object render bridge which needs the concrete module
    /// specifier (to load the source into the engine) and the default-export
    /// component name (to look up its compiled hook metadata).
    pub fn resolve_entry_component(&self, entry: &str) -> Option<(String, String)> {
        let module_spec = self.resolve_entry(entry)?;
        let function_name = self.modules.get(&module_spec)?.default_export.clone()?;
        Some((module_spec, function_name))
    }

    pub fn patch(
        &mut self,
        changed_paths: &[PathBuf],
        deleted_paths: &[PathBuf],
    ) -> Result<PatchReport> {
        let mut report = PatchReport::default();
        let mut parsed_updates = Vec::new();
        let mut staged_deletions = HashSet::new();
        let mut seen_changed = HashSet::new();

        for changed_path in changed_paths {
            let Some((specifier, absolute_path)) = self.module_specifier_for_path(changed_path)
            else {
                continue;
            };

            if !seen_changed.insert(specifier.clone()) {
                continue;
            }

            match std::fs::read_to_string(&absolute_path) {
                Ok(source) => {
                    let next_hash = fnv1a_hash(source.as_bytes());
                    if self.source_hashes.get(&specifier).copied() == Some(next_hash) {
                        report.skipped_unchanged += 1;
                        continue;
                    }

                    let parsed = parse_module_impl(&source, &absolute_path)?;
                    parsed_updates.push((specifier, parsed, next_hash, source));
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    staged_deletions.insert(specifier);
                }
                Err(err) => {
                    return Err(anyhow!(
                        "failed to read '{}' while patching: {err}",
                        absolute_path.display()
                    ));
                }
            }
        }

        for deleted_path in deleted_paths {
            let Some((specifier, _)) = self.module_specifier_for_path(deleted_path) else {
                continue;
            };
            staged_deletions.insert(specifier);
        }

        for (specifier, parsed, source_hash, source) in parsed_updates {
            self.modules.insert(specifier.clone(), parsed);
            self.source_hashes.insert(specifier.clone(), source_hash);
            self.sources.insert(specifier.clone(), source);
            let component_id = *self
                .specifier_to_id
                .entry(specifier.clone())
                .or_insert_with(|| {
                    let id = ComponentId::new(self.next_id);
                    self.next_id += 1;
                    id
                });
            report.reparsed_ids.push(component_id);
            report.reparsed_specifiers.push(specifier);
            report.reparsed += 1;
        }

        for specifier in staged_deletions {
            let component_id = self.specifier_to_id.get(&specifier).copied();
            let removed_module = self.modules.remove(&specifier).is_some();
            let removed_hash = self.source_hashes.remove(&specifier).is_some();
            self.sources.remove(&specifier);
            if removed_module || removed_hash {
                if let Some(component_id) = component_id {
                    report.deleted_ids.push(component_id);
                }
                report.deleted_specifiers.push(specifier);
                report.deleted += 1;
            }
        }

        Ok(report)
    }

    pub fn component_id_for_specifier(&self, specifier: &str) -> Option<ComponentId> {
        let spec = normalize_slashes(specifier);
        self.specifier_to_id.get(&spec).copied()
    }

    pub fn component_id_for_name(&self, name: &str) -> Option<ComponentId> {
        self.specifier_to_id
            .iter()
            .find(|(spec, _)| {
                Path::new(spec)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|stem| stem.eq_ignore_ascii_case(name))
                    .unwrap_or(false)
            })
            .map(|(_, &id)| id)
    }

    pub fn component_id_by_name(&self, name: &str) -> Option<ComponentId> {
        self.component_id_for_name(name)
    }

    pub fn render_entry(&self, entry: &str, props: &Value) -> Result<String> {
        // Each top-level render starts with a fresh element counter so the
        // `data-albedo-id` attributes the renderer stamps are stable per
        // render and don't leak across concurrent requests.
        reset_element_counter();
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;
        self.render_export(&entry, "default", props)
    }

    /// Exposes the parsed-module table so [`CompiledProject`] can run
    /// its Phase-K extractors over every function without re-parsing.
    #[must_use]
    /// Project source root used to resolve relative module imports.
    /// Exposed so [`crate::runtime::CompiledProject::wrap`] can read
    /// `.module.css` files from disk relative to each module's
    /// specifier (Phase P · Stream E.3).
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn modules(&self) -> &HashMap<String, ParsedModule> {
        &self.modules
    }

    /// Phase-K render entry: produces HTML plus the binding opcodes
    /// (`BindEvent`, `SetTextRef`) needed to hydrate the rendered
    /// shell against the session slot store. The opcodes ride the
    /// existing WT patches stream when the server ships them.
    ///
    /// Falls back to a Phase-J render when the compiled metadata for
    /// the entry component is empty (no hooks, no handlers).
    pub fn render_entry_compiled(
        &self,
        entry: &str,
        props: &Value,
        compiled: &CompiledProject,
        slots: &SessionSlotView,
    ) -> Result<(String, Vec<Instruction>)> {
        reset_element_counter();
        reset_derived_bindings();
        reset_conditional_bindings();
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;

        // Set up the Phase-K thread-local state. RAII guard restores
        // the previous (None) state even on panic so concurrent
        // renderers on the same thread don't see stale scope. The
        // project guard exposes the compiled metadata to `render_local`
        // via a thread-local pointer — see `current_phase_k_component`.
        let _slot_guard = PhaseKGuard::install(slots.clone());
        let _project_guard = install_phase_k_project(compiled);
        // Phase P · Stream E.3 — install the CSS-module class map
        // for the duration of the render so `eval_member` can
        // resolve `styles.foo` to the scoped class name.
        let _css_modules_guard = install_phase_k_css_modules(compiled.css_modules());

        let html = self.render_export(&entry, "default", props)?;
        let opcodes = drain_phase_k_opcodes();
        Ok((html, opcodes))
    }

    /// Phase O.2 · variant of [`Self::render_entry_compiled`] that
    /// makes a [`BroadcastRegistry`] available to `useSharedSlot`
    /// calls during render. Returns the same `(html, opcodes)` pair
    /// — opcodes include any `SetTextRef` bindings the renderer
    /// emitted for shared-slot text positions.
    ///
    /// The registry pointer is installed via a thread-local for the
    /// duration of this call and torn down before return; callers
    /// hand in a borrow and reclaim it untouched.
    pub fn render_entry_compiled_with_broadcast(
        &self,
        entry: &str,
        props: &Value,
        compiled: &CompiledProject,
        slots: &SessionSlotView,
        broadcast: &crate::runtime::broadcast::BroadcastRegistry,
    ) -> Result<(String, Vec<Instruction>)> {
        reset_element_counter();
        reset_derived_bindings();
        reset_conditional_bindings();
        let entry = self
            .resolve_entry(entry)
            .ok_or_else(|| anyhow!("entry '{}' not found in '{}'", entry, self.root.display()))?;

        let _slot_guard = PhaseKGuard::install(slots.clone());
        let _project_guard = install_phase_k_project(compiled);
        let _broadcast_guard = install_phase_k_broadcast(broadcast);
        // Phase P · Stream E.3 — same install as the non-broadcast
        // render path. CSS module bindings are per-component and
        // unrelated to broadcast scope, but render-time installs
        // are stacked on the same Phase K thread-local lifecycle.
        let _css_modules_guard = install_phase_k_css_modules(compiled.css_modules());

        let html = self.render_export(&entry, "default", props)?;
        let opcodes = drain_phase_k_opcodes();
        Ok((html, opcodes))
    }

    /// Re-execute a handler body server-side. The body is whatever
    /// `transforms::events::extract_handlers_in_function` surfaced —
    /// either a single expression (arrow body) or a block of
    /// statements. Setter calls inside the body translate to slot
    /// writes; identifier reads of slot-bound names translate to slot
    /// reads. Returns the explicit `Vec<Instruction>` from the body
    /// (the body itself rarely emits anything explicit — the SlotSet
    /// opcodes come from `SessionSlotView::drain_pending` afterwards).
    /// `form_payload` is the action envelope's decoded JSON payload — the
    /// submitted form fields — bound into the body's scope as `form`.
    ///
    /// Ambient, exactly like the `broadcast()` builtin: an author can write
    /// `form.author` directly, and destructuring (`action(({ form }) => …)`,
    /// the shape the extractor already preserves) resolves to the same binding
    /// because the body is evaluated in this scope either way.
    ///
    /// `None` (a click, an opaque payload) leaves `form` unbound, so a body that
    /// reads it fails loudly rather than reading `null.author`.
    pub fn eval_handler_body(
        &self,
        module_spec: &str,
        body: &HandlerBody,
        component: &CompiledComponent,
        slots: &SessionSlotView,
        form_payload: Option<&Value>,
    ) -> Result<Vec<Instruction>> {
        let _guard = PhaseKGuard::install(slots.clone());
        // Push the component's scope. We don't need any of the
        // pre-cache work because a handler only ever runs against one
        // component scope at a time.
        let shared_slots = component
            .shared_slots
            .iter()
            .map(|binding| {
                (
                    binding.binding_name.clone(),
                    (
                        crate::runtime::broadcast::broadcast_slot_id(&binding.topic),
                        binding.topic.clone(),
                    ),
                )
            })
            .collect();
        let scope = ComponentScope {
            module_spec: component.module_spec.clone(),
            function_name: component.function_name.clone(),
            value_slots: component.value_slots.clone(),
            setter_slots: component.setter_slots.clone(),
            proxy_ids: component.proxy_ids.clone(),
            handlers_emitted: 0,
            initials: component.hooks.iter().map(|h| h.initial.clone()).collect(),
            hook_index_for_value: component
                .hooks
                .iter()
                .map(|h| (h.value_name.clone(), h.hook_idx))
                .collect(),
            capture_slots: component.capture_slots.clone(),
            shared_slots,
            derived_locals: component.derived_locals.clone(),
        };
        phase_k_push_scope(scope);

        // Stage 3 · seed env with module-level constants first, so
        // a handler body that references one resolves correctly.
        // Slot values + captured props are seeded next, in that
        // order — later seeds shadow earlier ones, which matches JS
        // scoping (module const < component prop / state).
        let mut env: HashMap<String, Value> = HashMap::new();
        self.seed_env_with_module_constants(module_spec, &mut env);
        for (name, slot_id) in &component.value_slots {
            if let Some(bytes) = slots.read(*slot_id) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    env.insert(name.clone(), value);
                }
            } else if let Some(initial_expr) = component
                .hooks
                .iter()
                .find(|h| &h.value_name == name)
                .map(|h| h.initial.clone())
            {
                let value = self
                    .eval_expr(module_spec, &initial_expr, &env)
                    .unwrap_or(Value::Null);
                env.insert(name.clone(), value);
            }
        }

        // Stage 2 — seed env with captured prop snapshots. The render
        // path writes these on every render of the component; here we
        // read them back so the handler body's references to props
        // resolve correctly. Missing snapshots default to Null (the
        // prop was undefined at last render).
        for (name, slot_id) in &component.capture_slots {
            if let Some(bytes) = slots.read(*slot_id) {
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                    env.insert(name.clone(), value);
                }
            }
        }

        // Seeded LAST so it shadows a module constant or prop of the same name:
        // `form` is this request's data, and nothing in the component's scope
        // should be able to quietly stand in for it.
        if let Some(payload) = form_payload {
            env.insert("form".to_string(), payload.clone());
        }

        let result: Result<Vec<Instruction>> = match body {
            HandlerBody::Expr(expr) => {
                let _ = self.eval_expr(module_spec, expr, &env)?;
                Ok(Vec::new())
            }
            HandlerBody::Block(stmts) => {
                // Evaluate each statement; we only care about side
                // effects (slot writes via setter calls). Returns from
                // a handler are ignored in Phase K Stage 1.
                let mut local_env = env.clone();
                self.eval_body_stmts(module_spec, stmts, &mut local_env)
                    .map(|_| Vec::new())
            }
        };

        phase_k_pop_scope();
        result
    }

    fn resolve_entry(&self, entry: &str) -> Option<String> {
        let entry = normalize_slashes(entry);
        if self.modules.contains_key(&entry) {
            return Some(entry);
        }
        if Path::new(&entry).extension().is_none() {
            for ext in ["jsx", "tsx", "js", "ts"] {
                let candidate = format!("{entry}.{ext}");
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn module_specifier_for_path(&self, path: &Path) -> Option<(String, PathBuf)> {
        let absolute_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        let relative_path = absolute_path.strip_prefix(&self.root).ok()?;
        if path_is_in_node_modules(relative_path) {
            return None;
        }
        if !is_component_module(relative_path) {
            return None;
        }
        Some((normalize_specifier(relative_path), absolute_path))
    }

    /// Stage 3 · evaluate module-level constants in source order and
    /// seed them into `env`. Sequential evaluation against the
    /// accumulating env so a later const can read earlier ones (forward
    /// references in source order). Module constants whose init
    /// references something the Phase J interpreter doesn't model
    /// resolve to `Null` — a tracing warning surfaces the miss.
    fn seed_env_with_module_constants(&self, module_spec: &str, env: &mut HashMap<String, Value>) {
        let Some(module) = self.modules.get(module_spec) else {
            return;
        };
        if module.module_constants.is_empty() {
            return;
        }
        // Clone the const list so we can `eval_expr(&self, ...)`
        // without holding an immutable borrow of `self.modules`
        // across the call.
        let constants = module.module_constants.clone();
        for (name, expr) in constants {
            let value = self
                .eval_expr(module_spec, &expr, env)
                .unwrap_or(Value::Null);
            env.insert(name, value);
        }
    }

    fn render_export(&self, module_spec: &str, export_name: &str, props: &Value) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let local = if export_name == "default" {
            module
                .default_export
                .clone()
                .ok_or_else(|| anyhow!("module '{}' has no default export", module_spec))?
        } else {
            export_name.to_string()
        };
        self.render_local(module_spec, &local, props)
    }

    fn render_local(
        &self,
        module_spec: &str,
        function_name: &str,
        props: &Value,
    ) -> Result<String> {
        // Observer frame: opens a cascade-tracking scope for this component's
        // render. The guard publishes a `RenderInfo` on drop iff a process-wide
        // `RenderObserver` is installed — when none is, the whole scope
        // collapses to a single `OnceLock::get()` check.
        let _frame = crate::runtime::render_observer::enter_frame_guard(function_name, module_spec);

        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;
        let function = module.functions.get(function_name).ok_or_else(|| {
            anyhow!(
                "function '{}' missing in module '{}'",
                function_name,
                module_spec
            )
        })?;

        let mut env = HashMap::new();
        // Stage 3 · seed env with module-level constants BEFORE
        // binding props so a prop named the same as a const shadows
        // (rare; matches JS scoping). Done in both Phase J and
        // Phase K paths because module consts are universally useful
        // and previously rendered as Null via the unbound-ident
        // warning.
        self.seed_env_with_module_constants(module_spec, &mut env);
        bind_params(&function.params, props, &mut env);
        let stmts = function.body_stmts.clone();

        // Phase K: push this component's scope if hook-compile is
        // enabled and the compiled project has metadata for it. Pop
        // unconditionally on the way out so panics during eval don't
        // leak scope into a parent component's render.
        let pushed_phase_k_scope = if phase_k_enabled() {
            if let Some(scope) = current_phase_k_component(module_spec, function_name) {
                // Stage 2 · snapshot captured props to their dedicated
                // slots BEFORE evaluating the body, so a handler that
                // fires between renders reads the value the prop had
                // on the most recent render. We drain immediately so
                // the snapshot writes don't surface as user-driven
                // SlotSet opcodes in the response frame.
                snapshot_captured_props_into_slots(&scope, props);
                phase_k_push_scope(scope);
                true
            } else {
                false
            }
        } else {
            false
        };

        let result = self.eval_body_stmts(module_spec, &stmts, &mut env);

        if pushed_phase_k_scope {
            phase_k_pop_scope();
        }
        result
    }

    fn eval_expr(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match expr {
            Expr::JSXElement(element) => Ok(Value::String(self.eval_jsx_element(
                module_spec,
                element,
                env,
            )?)),
            Expr::JSXFragment(fragment) => Ok(Value::String(self.eval_jsx_fragment(
                module_spec,
                fragment,
                env,
            )?)),
            Expr::Lit(lit) => Ok(lit_to_value(lit)),
            Expr::Ident(ident) => {
                let name = ident.sym.to_string();
                if let Some(value) = env.get(&name) {
                    Ok(value.clone())
                } else {
                    // Static evaluator has no binding for this identifier.
                    // Phase K wires reactive bindings; until then make the
                    // miss findable in dev rather than letting it vanish.
                    tracing::debug!(
                        target: "albedo::eval",
                        ident = %name,
                        module = %module_spec,
                        "unbound identifier in JSX expression — evaluating to null",
                    );
                    Ok(Value::Null)
                }
            }
            Expr::Member(member) => self.eval_member(module_spec, member, env),
            Expr::Paren(paren) => self.eval_expr(module_spec, &paren.expr, env),
            Expr::Tpl(tpl) => self.eval_tpl(module_spec, tpl, env),
            Expr::Bin(bin) => self.eval_bin(module_spec, bin, env),
            Expr::Cond(cond) => self.eval_cond(module_spec, cond, env),
            Expr::Call(call) => self.eval_call_expr(module_spec, call, env),
            Expr::New(new_expr) => self.eval_new_expr(module_spec, new_expr, env),
            Expr::Array(arr) => self.eval_array_expr(module_spec, arr, env),
            Expr::Object(obj) => self.eval_object_expr(module_spec, obj, env),
            Expr::Unary(unary) => self.eval_unary(module_spec, unary, env),
            Expr::OptChain(opt) => self.eval_opt_chain(module_spec, opt, env),
            Expr::Seq(seq) => {
                let mut last = Value::Null;
                for expr in &seq.exprs {
                    last = self.eval_expr(module_spec, expr, env)?;
                }
                Ok(last)
            }
            // TypeScript escape hatches are runtime no-ops: unwrap to the
            // inner expression. SWC keeps these in the AST when JSX/TSX
            // sources contain `as`, `!`, `<X>e`, `satisfies`, `as const`,
            // or `f<T>` instantiation expressions.
            Expr::TsAs(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsNonNull(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsConstAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsTypeAssertion(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsSatisfies(node) => self.eval_expr(module_spec, &node.expr, env),
            Expr::TsInstantiation(node) => self.eval_expr(module_spec, &node.expr, env),
            other => {
                // Phase J keeps unhandled shapes returning Null for backwards
                // compatibility, but never silently — every drop emits a
                // tracing event that lets us extend the evaluator. Phase K's
                // SWC pass will compile most of these away into slot-store
                // opcodes, so this list should shrink, not grow.
                tracing::debug!(
                    target: "albedo::eval",
                    module = %module_spec,
                    expr_kind = std::any::type_name_of_val(other),
                    "unhandled JSX expression shape — evaluating to null",
                );
                Ok(Value::Null)
            }
        }
    }

    fn eval_opt_chain(
        &self,
        module_spec: &str,
        opt: &swc_ecma_ast::OptChainExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match &*opt.base {
            OptChainBase::Member(member) => {
                let obj = self.eval_expr(module_spec, &member.obj, env)?;
                if matches!(obj, Value::Null) {
                    return Ok(Value::Null);
                }
                self.eval_member_on(module_spec, &obj, &member.prop, env)
            }
            OptChainBase::Call(call) => {
                let callee = self.eval_expr(module_spec, &call.callee, env)?;
                if matches!(callee, Value::Null) {
                    return Ok(Value::Null);
                }
                // Callable-value support is Phase K; until then, treat
                // optional calls as null when reachable.
                Ok(Value::Null)
            }
        }
    }

    fn eval_new_expr(
        &self,
        module_spec: &str,
        new_expr: &swc_ecma_ast::NewExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Phase J only models `new Date(...)` because that's what ships in
        // the matrix. Other constructors fall through to Null with a trace.
        if let Expr::Ident(ident) = &*new_expr.callee {
            if ident.sym.as_ref() == "Date" {
                let args: Vec<Value> = match &new_expr.args {
                    Some(args) => args
                        .iter()
                        .map(|a| self.eval_expr(module_spec, &a.expr, env))
                        .collect::<Result<Vec<_>>>()?,
                    None => Vec::new(),
                };
                let ms = match args.first() {
                    None => 0.0, // Phase J: deterministic; no system clock.
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
                    Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
                    _ => 0.0,
                };
                return Ok(make_date_value(ms));
            }
        }
        tracing::debug!(
            target: "albedo::eval",
            module = %module_spec,
            "unhandled `new` constructor — evaluating to null",
        );
        Ok(Value::Null)
    }

    fn eval_member(
        &self,
        module_spec: &str,
        member: &swc_ecma_ast::MemberExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        // Phase P · Stream E.3 — `styles.foo` where `styles` is a
        // CSS-module import for the current module resolves to the
        // scoped class name via the per-project class map. We check
        // the obj BEFORE eval_expr so the lookup wins even though
        // `styles` isn't bound in env (CSS-module imports don't
        // surface as runtime values). Falls through to the regular
        // member path on any mismatch.
        if let (Expr::Ident(obj_ident), MemberProp::Ident(prop_ident)) =
            (&*member.obj, &member.prop)
        {
            let binding = obj_ident.sym.to_string();
            if !env.contains_key(&binding) {
                if let Some(registry) = current_phase_k_css_modules() {
                    let prop = prop_ident.sym.to_string();
                    if let Some(scoped) = registry.scoped_class_for(module_spec, &binding, &prop) {
                        return Ok(Value::String(scoped.to_string()));
                    }
                }
            }
        }

        let object = self.eval_expr(module_spec, &member.obj, env)?;
        self.eval_member_on(module_spec, &object, &member.prop, env)
    }

    /// Resolve a property access on an already-evaluated value. Factored
    /// out so `Expr::OptChain` and `Expr::Member` share the dispatch.
    fn eval_member_on(
        &self,
        module_spec: &str,
        object: &Value,
        prop: &swc_ecma_ast::MemberProp,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        // Computed access uses the runtime value verbatim — for arrays we
        // want a numeric index without stringifying through `value_to_string`,
        // which would render `1` as `"1"` and lose array-vs-object intent.
        match prop {
            MemberProp::Computed(computed) => {
                let key = self.eval_expr(module_spec, &computed.expr, env)?;
                if let (Value::Array(items), Some(idx)) = (object, key.as_f64()) {
                    if idx.is_finite() && idx >= 0.0 && idx == idx.trunc() {
                        return Ok(items.get(idx as usize).cloned().unwrap_or(Value::Null));
                    }
                }
                let prop_name = value_to_string(&key);
                self.lookup_named_prop(object, &prop_name)
            }
            MemberProp::Ident(ident) => {
                let prop_name = ident.sym.to_string();
                self.lookup_named_prop(object, &prop_name)
            }
            _ => Ok(Value::Null),
        }
    }

    fn lookup_named_prop(&self, object: &Value, prop_name: &str) -> Result<Value> {
        match object {
            Value::Object(map) => {
                // Date-tagged objects expose no JS-level properties; method
                // calls on them are handled in `eval_call_expr` via the
                // member callee path.
                Ok(map.get(prop_name).cloned().unwrap_or(Value::Null))
            }
            Value::Array(items) => match prop_name {
                "length" => Ok(json_int(items.len() as i64)),
                _ => {
                    // Numeric string indexing: `arr["0"]` matches JS semantics.
                    if let Ok(idx) = prop_name.parse::<usize>() {
                        return Ok(items.get(idx).cloned().unwrap_or(Value::Null));
                    }
                    Ok(Value::Null)
                }
            },
            Value::String(s) => match prop_name {
                "length" => Ok(json_int(s.chars().count() as i64)),
                _ => Ok(Value::Null),
            },
            _ => Ok(Value::Null),
        }
    }

    fn eval_tpl(
        &self,
        module_spec: &str,
        tpl: &swc_ecma_ast::Tpl,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let mut result = String::new();
        for (i, quasi) in tpl.quasis.iter().enumerate() {
            let text = quasi
                .cooked
                .as_ref()
                .map(|s| s.to_string())
                .unwrap_or_else(|| quasi.raw.to_string());
            result.push_str(&text);
            if i < tpl.exprs.len() {
                let val = self.eval_expr(module_spec, &tpl.exprs[i], env)?;
                result.push_str(&value_to_string(&val));
            }
        }
        Ok(Value::String(result))
    }

    fn eval_bin(
        &self,
        module_spec: &str,
        bin: &swc_ecma_ast::BinExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        match bin.op {
            BinaryOp::LogicalAnd => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if !is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::LogicalOr => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if is_truthy(&left) {
                    Ok(left)
                } else {
                    self.eval_expr(module_spec, &bin.right, env)
                }
            }
            BinaryOp::NullishCoalescing => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                if matches!(left, Value::Null) {
                    self.eval_expr(module_spec, &bin.right, env)
                } else {
                    Ok(left)
                }
            }
            BinaryOp::Add => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                // JS `+` semantics: string concat when EITHER side is a
                // string; numeric add (after `ToNumber` coercion)
                // otherwise. The previous shape gated numeric add on
                // BOTH sides being `Value::Number`, which made
                // `null + 1` fall to string concat — silently breaking
                // broadcast updaters like `(n) => n + 1` whose first
                // call sees `n = null` and produces "1" instead of 1.
                match (&left, &right) {
                    (Value::String(_), _) | (_, Value::String(_)) => Ok(Value::String(format!(
                        "{}{}",
                        value_to_string(&left),
                        value_to_string(&right)
                    ))),
                    _ => Ok(json_num(to_number(&left) + to_number(&right))),
                }
            }
            BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod | BinaryOp::Exp => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let value = match bin.op {
                    BinaryOp::Sub => l - r,
                    BinaryOp::Mul => l * r,
                    BinaryOp::Div => l / r,
                    BinaryOp::Mod => l % r,
                    BinaryOp::Exp => l.powf(r),
                    _ => unreachable!(),
                };
                Ok(json_num(value))
            }
            BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                let l = to_number(&left);
                let r = to_number(&right);
                let result = match bin.op {
                    BinaryOp::Lt => l < r,
                    BinaryOp::Gt => l > r,
                    BinaryOp::LtEq => l <= r,
                    BinaryOp::GtEq => l >= r,
                    _ => unreachable!(),
                };
                Ok(Value::Bool(result))
            }
            BinaryOp::EqEq | BinaryOp::EqEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) == value_to_string(&right),
                ))
            }
            BinaryOp::NotEq | BinaryOp::NotEqEq => {
                let left = self.eval_expr(module_spec, &bin.left, env)?;
                let right = self.eval_expr(module_spec, &bin.right, env)?;
                Ok(Value::Bool(
                    value_to_string(&left) != value_to_string(&right),
                ))
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_cond(
        &self,
        module_spec: &str,
        cond: &swc_ecma_ast::CondExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let test = self.eval_expr(module_spec, &cond.test, env)?;
        if is_truthy(&test) {
            self.eval_expr(module_spec, &cond.cons, env)
        } else {
            self.eval_expr(module_spec, &cond.alt, env)
        }
    }

    fn eval_unary(
        &self,
        module_spec: &str,
        unary: &swc_ecma_ast::UnaryExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let val = self.eval_expr(module_spec, &unary.arg, env)?;
        match unary.op {
            UnaryOp::Bang => Ok(Value::Bool(!is_truthy(&val))),
            UnaryOp::Minus => {
                if let Value::Number(n) = &val {
                    Ok(serde_json::Number::from_f64(-n.as_f64().unwrap_or(0.0))
                        .map(Value::Number)
                        .unwrap_or(Value::Null))
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_array_expr(
        &self,
        module_spec: &str,
        arr: &swc_ecma_ast::ArrayLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut out = Vec::with_capacity(arr.elems.len());
        for elem in &arr.elems {
            if let Some(ExprOrSpread { expr, spread: None }) = elem {
                out.push(self.eval_expr(module_spec, expr, env)?);
            }
        }
        Ok(Value::Array(out))
    }

    fn eval_object_expr(
        &self,
        module_spec: &str,
        obj: &swc_ecma_ast::ObjectLit,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        let mut map = serde_json::Map::new();
        for prop in &obj.props {
            if let PropOrSpread::Prop(prop_box) = prop {
                match prop_box.as_ref() {
                    Prop::KeyValue(kv) => {
                        if let Some(key) = prop_name_to_string(&kv.key) {
                            let val = self.eval_expr(module_spec, &kv.value, env)?;
                            map.insert(key, val);
                        }
                    }
                    Prop::Shorthand(ident) => {
                        let name = ident.sym.to_string();
                        let val = env.get(&name).cloned().unwrap_or(Value::Null);
                        map.insert(name, val);
                    }
                    _ => {}
                }
            }
        }
        Ok(Value::Object(map))
    }

    fn eval_call_expr(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        // --- Member-callee dispatch: obj.method(...args) -----------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Member(member) = callee_expr.as_ref() {
                if let MemberProp::Ident(prop_ident) = &member.prop {
                    let method = prop_ident.sym.to_string();

                    // Static-namespace dispatch (Math.x, Date.x, JSON.x, ...)
                    // is handled before evaluating `member.obj` because the
                    // namespace itself isn't a value we model — `Math.floor`
                    // would otherwise try to look up `Math` in env and miss.
                    if let Expr::Ident(ns_ident) = &*member.obj {
                        let ns_name = ns_ident.sym.to_string();
                        if !env.contains_key(&ns_name) {
                            if let Some(value) = self.eval_static_namespace_call(
                                module_spec,
                                &ns_name,
                                &method,
                                &call.args,
                                env,
                            )? {
                                return Ok(value);
                            }
                        }
                    }

                    // Instance-method dispatch.
                    let obj_val = self.eval_expr(module_spec, &member.obj, env)?;
                    if let Some(value) =
                        self.eval_instance_method(module_spec, &obj_val, &method, &call.args, env)?
                    {
                        return Ok(value);
                    }
                }
            }
        }

        // --- Bare-ident callee dispatch: f(...args) ----------------------
        if let Callee::Expr(callee_expr) = &call.callee {
            if let Expr::Ident(ident) = callee_expr.as_ref() {
                let fn_name = ident.sym.to_string();

                // Phase P · Stream C.2 — `broadcast(topic, updater)`
                // interpreter builtin. Lands above setter dispatch so
                // a TS action body that happens to declare a setter
                // named `broadcast` doesn't shadow the framework
                // builtin. v1 scope guard: `broadcast()` only resolves
                // when `PHASE_K_BROADCAST` is installed (i.e. inside
                // an action handler via `CompiledProject::invoke_action_with_broadcast`).
                // Render-time uses the thread-local read-only side via
                // `phase_k_shared_slot_for_value`; writes from render
                // callbacks aren't a supported surface today.
                if fn_name == "broadcast" {
                    if let Some(broadcast) = current_phase_k_broadcast() {
                        return self.eval_broadcast_call(module_spec, call, env, broadcast);
                    }
                    // v1 scope guard — surface a clean error rather
                    // than silently falling through to the import /
                    // user-function dispatch. A handler that reaches
                    // here is either running outside an action
                    // context (e.g. an onClick body that called
                    // broadcast directly) or the server forgot to
                    // route through `invoke_action_with_broadcast`.
                    return Err(anyhow!(
                        "broadcast() is only available inside action handlers \
                         dispatched via CompiledProject::invoke_action_with_broadcast; \
                         no broadcast registry is installed on the current call stack"
                    ));
                }

                // FORGE · `append(collection, record)` — the durable write
                // builtin. Sits beside `broadcast` deliberately: same shape
                // (framework builtin resolved above setter/user dispatch), same
                // scope guard.
                //
                // It RECORDS rather than writes. The substrate is async and this
                // evaluation is sync, so the intent is collected here and applied
                // by the async action adapter once the body returns — see
                // `crate::forge::write`. Returning Null keeps the handler body's
                // discarded value from surfacing a cast of the record.
                if fn_name == "append" {
                    return self.eval_forge_append(module_spec, call, env);
                }
                // FORGE · `update(collection, key, fields)` and
                // `delete(collection, key)` — same record-not-write discipline
                // as `append`, differing only in the intent recorded.
                if fn_name == "update" {
                    return self.eval_forge_update(module_spec, call, env);
                }
                // `remove`, not `delete`: `delete` is a JS reserved word (the
                // delete operator), so it can never parse as a call here.
                if fn_name == "remove" {
                    return self.eval_forge_delete(module_spec, call, env);
                }

                // Phase K · setter dispatch: when the current scope
                // has registered `fn_name` as a useState setter, the
                // call is a slot write — evaluate the arg, JSON-encode
                // it, and store. Returns Null so the handler body's
                // overall value (which is discarded) doesn't surface
                // a confusing string-cast of the written value.
                if let Some(slot_id) = phase_k_slot_for_setter(&fn_name) {
                    let arg_value = match call.args.first() {
                        Some(arg) if arg.spread.is_none() => {
                            self.eval_expr(module_spec, &arg.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    if let Ok(bytes) = serde_json::to_vec(&arg_value) {
                        phase_k_write_slot_value(slot_id, bytes);
                    }
                    return Ok(Value::Null);
                }

                let module = self.modules.get(module_spec);
                let import = module.and_then(|m| m.imports.get(&fn_name));

                // classnames / clsx — flatten args into a class string.
                let is_classnames = import
                    .map(|b| is_classnames_source(&b.source))
                    .unwrap_or(false);
                if is_classnames {
                    let mut classes = Vec::new();
                    for arg in &call.args {
                        if arg.spread.is_some() {
                            continue;
                        }
                        let val = self.eval_expr(module_spec, &arg.expr, env)?;
                        classnames_collect(&val, &mut classes);
                    }
                    return Ok(Value::String(classes.join(" ")));
                }

                // useState shim (Phase J): recognize the React import and
                // return `[initial, null]`. Phase K replaces this with real
                // slot-store reads/writes; until then this lets `{count}`
                // render its initial value instead of vanishing.
                let is_react_use_state = fn_name == "useState"
                    && import
                        .map(|b| b.source == "react" && b.export_name == "useState")
                        .unwrap_or(false);
                if is_react_use_state {
                    let initial = match call.args.first() {
                        Some(arg) if arg.spread.is_none() => {
                            self.eval_expr(module_spec, &arg.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(Value::Array(vec![initial, Value::Null]));
                }

                // JS-style coercions.
                if fn_name == "String" || fn_name == "Number" || fn_name == "Boolean" {
                    let arg = match call.args.first() {
                        Some(a) if a.spread.is_none() => {
                            self.eval_expr(module_spec, &a.expr, env)?
                        }
                        _ => Value::Null,
                    };
                    return Ok(match fn_name.as_str() {
                        "String" => Value::String(value_to_string(&arg)),
                        "Number" => json_num(to_number(&arg)),
                        "Boolean" => Value::Bool(is_truthy(&arg)),
                        _ => unreachable!(),
                    });
                }
            }
        }

        Ok(Value::Null)
    }

    fn eval_static_namespace_call(
        &self,
        module_spec: &str,
        ns: &str,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        let evaluated: Vec<Value> = args
            .iter()
            .filter(|a| a.spread.is_none())
            .map(|a| self.eval_expr(module_spec, &a.expr, env))
            .collect::<Result<Vec<_>>>()?;

        let result = match (ns, method) {
            // Math.* — covers everything that shows up in display logic.
            ("Math", "floor") => json_num(arg_num(&evaluated, 0).floor()),
            ("Math", "ceil") => json_num(arg_num(&evaluated, 0).ceil()),
            ("Math", "round") => json_num(arg_num(&evaluated, 0).round()),
            ("Math", "trunc") => json_num(arg_num(&evaluated, 0).trunc()),
            ("Math", "abs") => json_num(arg_num(&evaluated, 0).abs()),
            ("Math", "sqrt") => json_num(arg_num(&evaluated, 0).sqrt()),
            ("Math", "max") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::NEG_INFINITY, f64::max),
            ),
            ("Math", "min") => json_num(
                evaluated
                    .iter()
                    .map(to_number)
                    .fold(f64::INFINITY, f64::min),
            ),
            ("Math", "pow") => json_num(arg_num(&evaluated, 0).powf(arg_num(&evaluated, 1))),

            // Date statics — no system clock in Phase J (deterministic SSR).
            // `Date.now()` returns 0; user code that wants a real timestamp
            // should accept it as a prop. Phase K will surface a clock slot.
            ("Date", "now") => json_int(0),

            // JSON.* — useful in display-time templates for debug surfaces.
            ("JSON", "stringify") => match evaluated.first() {
                Some(value) => Value::String(serde_json::to_string(value).unwrap_or_default()),
                None => Value::Null,
            },

            // Object.keys / Object.values — used in admin/debug UIs.
            ("Object", "keys") => match evaluated.first() {
                Some(Value::Object(map)) => {
                    Value::Array(map.keys().cloned().map(Value::String).collect())
                }
                _ => Value::Array(Vec::new()),
            },
            ("Object", "values") => match evaluated.first() {
                Some(Value::Object(map)) => Value::Array(map.values().cloned().collect()),
                _ => Value::Array(Vec::new()),
            },

            _ => return Ok(None),
        };

        Ok(Some(result))
    }

    fn eval_instance_method(
        &self,
        module_spec: &str,
        receiver: &Value,
        method: &str,
        args: &[swc_ecma_ast::ExprOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Option<Value>> {
        // Date instance methods first — Date is encoded as a tagged object.
        if let Some(ms) = date_value_ms(receiver) {
            return Ok(Some(self.eval_date_method(method, ms)));
        }

        match receiver {
            Value::String(s) => {
                let result = match method {
                    "toUpperCase" => Some(Value::String(s.to_uppercase())),
                    "toLowerCase" => Some(Value::String(s.to_lowercase())),
                    "trim" => Some(Value::String(s.trim().to_string())),
                    "trimStart" | "trimLeft" => Some(Value::String(s.trim_start().to_string())),
                    "trimEnd" | "trimRight" => Some(Value::String(s.trim_end().to_string())),
                    "toString" => Some(Value::String(s.clone())),
                    _ => None,
                };
                Ok(result)
            }
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(0.0);
                let evaluated: Vec<Value> = args
                    .iter()
                    .filter(|a| a.spread.is_none())
                    .map(|a| self.eval_expr(module_spec, &a.expr, env))
                    .collect::<Result<Vec<_>>>()?;
                let result = match method {
                    "toFixed" => {
                        let digits = arg_num(&evaluated, 0).clamp(0.0, 100.0) as usize;
                        Some(Value::String(format!("{:.*}", digits, f)))
                    }
                    "toString" => {
                        let radix = if evaluated.is_empty() {
                            10.0
                        } else {
                            arg_num(&evaluated, 0)
                        };
                        if radix == 10.0 {
                            Some(Value::String(value_to_string(receiver)))
                        } else if (radix - radix.trunc()).abs() < f64::EPSILON
                            && (2.0..=36.0).contains(&radix)
                            && f.is_finite()
                            && f == f.trunc()
                        {
                            let int = f as i64;
                            let radix = radix as u32;
                            let mut digits = String::new();
                            let (sign, mut value) = if int < 0 {
                                ("-", (-(int as i128)) as u128)
                            } else {
                                ("", int as u128)
                            };
                            if value == 0 {
                                digits.push('0');
                            }
                            while value > 0 {
                                let d = (value % radix as u128) as u32;
                                let ch = std::char::from_digit(d, radix).unwrap_or('0');
                                digits.insert(0, ch);
                                value /= radix as u128;
                            }
                            Some(Value::String(format!("{sign}{digits}")))
                        } else {
                            Some(Value::String(value_to_string(receiver)))
                        }
                    }
                    _ => None,
                };
                Ok(result)
            }
            Value::Array(items) => match method {
                "map" => {
                    if let Some(swc_ecma_ast::ExprOrSpread {
                        expr: mapper,
                        spread: None,
                    }) = args.first()
                    {
                        let parts = items
                            .iter()
                            .enumerate()
                            .map(|(i, item)| {
                                self.eval_closure(module_spec, mapper, item, i, env)
                                    .map(|v| value_to_string(&v))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        return Ok(Some(Value::String(parts.join(""))));
                    }
                    Ok(Some(Value::Null))
                }
                "join" => {
                    let sep = match args.first() {
                        Some(a) if a.spread.is_none() => {
                            value_to_string(&self.eval_expr(module_spec, &a.expr, env)?)
                        }
                        _ => ",".to_string(),
                    };
                    let parts: Vec<String> = items.iter().map(value_to_string).collect();
                    Ok(Some(Value::String(parts.join(&sep))))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn eval_date_method(&self, method: &str, ms: f64) -> Value {
        match method {
            "getTime" | "valueOf" => json_num(ms),
            "toISOString" | "toJSON" | "toString" => {
                Value::String(value_to_string(&make_date_value(ms)))
            }
            _ => Value::Null,
        }
    }

    /// Phase P · Stream C.2 — evaluate `broadcast(topic, updater)`.
    ///
    /// Atomic read-modify-write on a broadcast topic, mirroring the
    /// React `setState(fn)` updater pattern:
    ///
    ///   1. Topic is registered if not yet present (idempotent — ad-hoc topics like
    ///      `broadcast(\`chat:${room}\`, ...)` don't need pre-registration).
    ///   2. Current value is read via `current_value()` and decoded as JSON. Empty / undecodable
    ///      bytes surface as `Value::Null` so the updater can initialise on first call.
    ///   3. The updater closure is evaluated with its single param bound to the current value.
    ///      Expression-bodied arrows return their tail; block-bodied closures pick up the `return`
    ///      statement. Block bodies without a `return` yield `Null`.
    ///   4. The result is JSON-encoded and pushed through `broadcast.write_topic`, fanning out a
    ///      `SlotSet` opcode to every subscribed session over the WT patches lane.
    ///
    /// Returns `Value::Null` so the action body's tail expression
    /// (which is discarded by the action dispatcher) doesn't surface
    /// a confusing string-cast of the written value.
    /// FORGE · `append(collection, record)` — record a durable append.
    ///
    /// Evaluates both arguments now (so the record reflects the values the body
    /// computed) but performs no I/O: the intent goes on the thread-local
    /// collector and the async action adapter applies it. See
    /// [`crate::forge::write`] for why the split exists.
    fn eval_forge_append(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let collection_arg = call
            .args
            .first()
            .ok_or_else(|| anyhow!("append() requires a collection name as its first argument"))?;
        let record_arg = call
            .args
            .get(1)
            .ok_or_else(|| anyhow!("append() requires a record object as its second argument"))?;
        if collection_arg.spread.is_some() || record_arg.spread.is_some() {
            return Err(anyhow!("append() does not accept spread arguments"));
        }

        let collection = match self.eval_expr(module_spec, &collection_arg.expr, env)? {
            Value::String(s) => s,
            other => {
                return Err(anyhow!(
                    "append() collection must evaluate to a string; got {other:?}"
                ))
            }
        };
        let record = match self.eval_expr(module_spec, &record_arg.expr, env)? {
            Value::Object(map) => map,
            other => {
                return Err(anyhow!(
                    "append() record must evaluate to an object; got {other:?}"
                ))
            }
        };

        let recorded = crate::forge::write::record_forge_write(crate::forge::ForgeWrite::Append {
            collection,
            record,
        });
        if !recorded {
            // Same scope guard as `broadcast()`: outside an action dispatch
            // there is nothing to apply the write, and silently discarding a
            // durable write is the worst possible outcome.
            return Err(anyhow!(
                "append() is only available inside action handlers dispatched with a \
                 FORGE write collector installed; no collector is on the current call stack"
            ));
        }

        Ok(Value::Null)
    }

    /// FORGE · `update(collection, key, fields)` — record a durable update of
    /// the row identified by `key`. Mirrors [`Self::eval_forge_append`]'s
    /// record-not-write discipline and scope guard.
    fn eval_forge_update(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let collection =
            self.forge_string_arg(module_spec, call, env, 0, "update", "collection")?;
        let key = self.forge_scalar_key_arg(module_spec, call, env, 1, "update")?;
        let fields_arg = call
            .args
            .get(2)
            .ok_or_else(|| anyhow!("update() requires a fields object as its third argument"))?;
        if fields_arg.spread.is_some() {
            return Err(anyhow!("update() does not accept spread arguments"));
        }
        let fields = match self.eval_expr(module_spec, &fields_arg.expr, env)? {
            Value::Object(map) => map,
            other => {
                return Err(anyhow!(
                    "update() fields must evaluate to an object; got {other:?}"
                ))
            }
        };

        let recorded = crate::forge::write::record_forge_write(crate::forge::ForgeWrite::Update {
            collection,
            key,
            fields,
        });
        if !recorded {
            return Err(anyhow!(
                "update() is only available inside action handlers dispatched with a \
                 FORGE write collector installed; no collector is on the current call stack"
            ));
        }
        Ok(Value::Null)
    }

    /// FORGE · `delete(collection, key)` — record a durable delete of the row
    /// identified by `key`.
    fn eval_forge_delete(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
    ) -> Result<Value> {
        let collection =
            self.forge_string_arg(module_spec, call, env, 0, "remove", "collection")?;
        let key = self.forge_scalar_key_arg(module_spec, call, env, 1, "remove")?;

        let recorded = crate::forge::write::record_forge_write(crate::forge::ForgeWrite::Delete {
            collection,
            key,
        });
        if !recorded {
            return Err(anyhow!(
                "remove() is only available inside action handlers dispatched with a \
                 FORGE write collector installed; no collector is on the current call stack"
            ));
        }
        Ok(Value::Null)
    }

    /// Evaluate a positional argument to a non-spread string. Shared by the
    /// FORGE builtins for their `collection` argument.
    fn forge_string_arg(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
        index: usize,
        builtin: &str,
        what: &str,
    ) -> Result<String> {
        let arg = call
            .args
            .get(index)
            .ok_or_else(|| anyhow!("{builtin}() requires a {what} as argument {}", index + 1))?;
        if arg.spread.is_some() {
            return Err(anyhow!("{builtin}() does not accept spread arguments"));
        }
        match self.eval_expr(module_spec, &arg.expr, env)? {
            Value::String(s) => Ok(s),
            other => Err(anyhow!(
                "{builtin}() {what} must evaluate to a string; got {other:?}"
            )),
        }
    }

    /// Evaluate a positional argument to a scalar row key (string/number/bool).
    /// Rejects objects, arrays, and null — a key must identify exactly one row,
    /// and the SQL builder would refuse a non-scalar anyway; catching it here
    /// gives a builtin-named error instead of a substrate one.
    fn forge_scalar_key_arg(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
        index: usize,
        builtin: &str,
    ) -> Result<Value> {
        let arg = call
            .args
            .get(index)
            .ok_or_else(|| anyhow!("{builtin}() requires a key as argument {}", index + 1))?;
        if arg.spread.is_some() {
            return Err(anyhow!("{builtin}() does not accept spread arguments"));
        }
        match self.eval_expr(module_spec, &arg.expr, env)? {
            key @ (Value::String(_) | Value::Number(_) | Value::Bool(_)) => Ok(key),
            other => Err(anyhow!(
                "{builtin}() key must be a string, number, or boolean; got {other:?}"
            )),
        }
    }

    fn eval_broadcast_call(
        &self,
        module_spec: &str,
        call: &swc_ecma_ast::CallExpr,
        env: &HashMap<String, Value>,
        broadcast: &crate::runtime::broadcast::BroadcastRegistry,
    ) -> Result<Value> {
        let topic_arg = call.args.first().ok_or_else(|| {
            anyhow!("broadcast() requires a topic argument as its first parameter")
        })?;
        if topic_arg.spread.is_some() {
            return Err(anyhow!(
                "broadcast() does not accept a spread topic argument"
            ));
        }
        let topic_value = self.eval_expr(module_spec, &topic_arg.expr, env)?;
        let topic = match topic_value {
            Value::String(s) => s,
            other => {
                return Err(anyhow!(
                    "broadcast() topic must evaluate to a string; got {other:?}"
                ))
            }
        };

        // Ensure topic exists. Idempotent — `topic()` returns the
        // existing entry on a second call. Default-seed with `null`
        // so the updater's first invocation sees a sensible value
        // rather than empty bytes.
        let topic_arc = broadcast
            .get(&topic)
            .unwrap_or_else(|| broadcast.topic(topic.clone(), b"null".to_vec()));
        let current_bytes = topic_arc.current_value();
        let current_value: Value = if current_bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&current_bytes).unwrap_or(Value::Null)
        };

        let updater_arg = call.args.get(1).ok_or_else(|| {
            anyhow!(
                "broadcast('{}', updater) requires an updater closure as its second parameter",
                topic
            )
        })?;
        if updater_arg.spread.is_some() {
            return Err(anyhow!(
                "broadcast() does not accept a spread updater argument"
            ));
        }

        let next_value =
            self.invoke_updater(module_spec, &updater_arg.expr, &current_value, env)?;

        let encoded = serde_json::to_vec(&next_value).map_err(|err| {
            anyhow!("broadcast('{topic}') failed to encode updater result: {err}")
        })?;
        broadcast
            .write_topic(&topic, encoded)
            .map_err(|err| anyhow!("broadcast('{topic}') write failed: {err}"))?;

        Ok(Value::Null)
    }

    /// Phase P · Stream C.2 — invoke an updater closure with one
    /// positional arg. Supports arrow and function expressions, with
    /// or without paren wrappers / TS casts. Block-bodied closures
    /// follow the first `return` statement; expression-bodied arrows
    /// return their tail expression. Anything else yields `Null` so
    /// the broadcast pipeline still completes without a Rust panic.
    fn invoke_updater(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        arg: &Value,
        parent_env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        let unwrapped = unwrap_updater_parens(expr);
        match unwrapped {
            Expr::Arrow(arrow) => {
                let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
                let mut env = parent_env.clone();
                let args = Value::Array(vec![arg.clone()]);
                bind_params_positional(&params, &args, &mut env);
                match &*arrow.body {
                    BlockStmtOrExpr::Expr(body_expr) => {
                        self.eval_expr(module_spec, body_expr, &env)
                    }
                    BlockStmtOrExpr::BlockStmt(block) => {
                        self.eval_updater_block(module_spec, &block.stmts, &mut env)
                    }
                }
            }
            Expr::Fn(fn_expr) => {
                let params: Vec<ParamBinding> = fn_expr
                    .function
                    .params
                    .iter()
                    .map(|p| param_from_pat(&p.pat))
                    .collect();
                let mut env = parent_env.clone();
                let args = Value::Array(vec![arg.clone()]);
                bind_params_positional(&params, &args, &mut env);
                match &fn_expr.function.body {
                    Some(block) => self.eval_updater_block(module_spec, &block.stmts, &mut env),
                    None => Ok(Value::Null),
                }
            }
            _ => Err(anyhow!(
                "broadcast() updater must be an arrow or function expression"
            )),
        }
    }

    /// Walk a block-bodied updater and return the first `return`'s
    /// value (or `Value::Null` if no return is reached). Sibling to
    /// `eval_body_stmts` but returns `Value` instead of coercing to
    /// `String` — broadcast writes need the structured value back so
    /// it can be JSON-encoded.
    fn eval_updater_block(
        &self,
        module_spec: &str,
        stmts: &[swc_ecma_ast::Stmt],
        env: &mut HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;
        for stmt in stmts {
            match stmt {
                Stmt::Return(ret) => {
                    return match &ret.arg {
                        Some(expr) => self.eval_expr(module_spec, expr, env),
                        None => Ok(Value::Null),
                    };
                }
                Stmt::Decl(Decl::Var(var)) => {
                    self.eval_var_decl_into_env(module_spec, var, env);
                }
                _ => {}
            }
        }
        Ok(Value::Null)
    }

    fn eval_closure(
        &self,
        module_spec: &str,
        expr: &swc_ecma_ast::Expr,
        arg: &Value,
        index: usize,
        parent_env: &HashMap<String, Value>,
    ) -> Result<Value> {
        use swc_ecma_ast::*;

        match expr {
            Expr::Arrow(arrow) => {
                let params: Vec<ParamBinding> = arrow.params.iter().map(param_from_pat).collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                match &*arrow.body {
                    BlockStmtOrExpr::BlockStmt(block) => self
                        .eval_body_stmts(module_spec, &block.stmts, &mut env)
                        .map(Value::String),
                    BlockStmtOrExpr::Expr(body_expr) => {
                        self.eval_expr(module_spec, body_expr, &env)
                    }
                }
            }
            Expr::Fn(fn_expr) => {
                let params: Vec<ParamBinding> = fn_expr
                    .function
                    .params
                    .iter()
                    .map(|p| param_from_pat(&p.pat))
                    .collect();
                let mut env = parent_env.clone();
                let index_val = serde_json::Number::from_f64(index as f64)
                    .map(Value::Number)
                    .unwrap_or(Value::Null);
                let args = Value::Array(vec![arg.clone(), index_val]);
                bind_params_positional(&params, &args, &mut env);
                if let Some(body) = &fn_expr.function.body {
                    self.eval_body_stmts(module_spec, &body.stmts, &mut env)
                        .map(Value::String)
                } else {
                    Ok(Value::Null)
                }
            }
            _ => Ok(Value::Null),
        }
    }

    fn eval_body_stmts(
        &self,
        module_spec: &str,
        stmts: &[swc_ecma_ast::Stmt],
        env: &mut HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

        for stmt in stmts {
            match stmt {
                Stmt::Return(ret) => {
                    let value = if let Some(expr) = &ret.arg {
                        self.eval_expr(module_spec, expr, env)?
                    } else {
                        Value::Null
                    };
                    return Ok(value_to_string(&value));
                }
                Stmt::Decl(Decl::Var(var)) => {
                    self.eval_var_decl_into_env(module_spec, var, env);
                }
                // A bare expression statement is evaluated for its side
                // effects. This is the path block-bodied handlers take —
                // `() => { setCount(count + 1); }` is an `ExprStmt` wrapping
                // the setter call, and the slot write happens *inside*
                // `eval_expr`. Before, this hit the catch-all and was silently
                // dropped, so block-bodied handlers ran but did nothing. The
                // return value is discarded (statement position).
                Stmt::Expr(expr_stmt) => {
                    let _ = self.eval_expr(module_spec, &expr_stmt.expr, env)?;
                }
                // `if (cond) { ... } else { ... }`: evaluate the guard, recurse
                // into the taken branch with the same environment (function
                // scope — handlers and Tier-A render bodies are small and don't
                // rely on block-scoped shadowing). A `return` reached inside the
                // branch surfaces as a non-empty string and short-circuits, the
                // same convention the top-level loop uses.
                Stmt::If(if_stmt) => {
                    let test = self.eval_expr(module_spec, &if_stmt.test, env)?;
                    if is_truthy(&test) {
                        let returned = self.eval_body_stmts(
                            module_spec,
                            std::slice::from_ref(&if_stmt.cons),
                            env,
                        )?;
                        if !returned.is_empty() {
                            return Ok(returned);
                        }
                    } else if let Some(alt) = &if_stmt.alt {
                        let returned =
                            self.eval_body_stmts(module_spec, std::slice::from_ref(alt), env)?;
                        if !returned.is_empty() {
                            return Ok(returned);
                        }
                    }
                }
                // A nested block (`{ ... }`) is evaluated inline.
                Stmt::Block(block) => {
                    let returned = self.eval_body_stmts(module_spec, &block.stmts, env)?;
                    if !returned.is_empty() {
                        return Ok(returned);
                    }
                }
                // An empty statement (`;`) is a no-op, not unsupported.
                Stmt::Empty(_) => {}
                // Everything else (`for` / `while` / `do` / `for-in` / `for-of`
                // / `try` / `switch` / `throw` / labelled / `break` / `continue`
                // / function & class declarations / async constructs) is *not*
                // something this pure-Rust evaluator models. Silent-wrong is the
                // core enemy: rather than drop it and emit partial output, fail
                // loudly so the caller surfaces it. Components that need these
                // constructs are Tier-B/C and belong on the QuickJS engine.
                other => {
                    return Err(anyhow!(
                        "unsupported statement `{}` in pure-Rust evaluator for module '{}'; \
                         this construct must run on the QuickJS engine (Tier B/C)",
                        statement_kind(other),
                        module_spec
                    ));
                }
            }
        }
        Ok(String::new())
    }

    fn eval_var_decl_into_env(
        &self,
        module_spec: &str,
        var: &swc_ecma_ast::VarDecl,
        env: &mut HashMap<String, Value>,
    ) {
        use swc_ecma_ast::*;

        for decl in &var.decls {
            // Phase O.2 hook-compile path: `const name = useSharedSlot("topic")`
            // binds `name` to the broadcast topic's current value at
            // render time. The extractor already verified the binding
            // shape and topic-literal contract; the renderer just
            // looks up the value via the broadcast thread-local. When
            // the registry isn't installed (a render outside the
            // broadcast surface) the binding falls through to a
            // `null` value — same shape as a slot that hasn't been
            // written yet.
            if let Pat::Ident(binding) = &decl.name {
                if let Some(init) = &decl.init {
                    if let Expr::Call(call) = init.as_ref() {
                        if is_use_shared_slot_in_phase_k_scope(call) {
                            let name = binding.id.sym.to_string();
                            if let Some((_slot_id, topic)) = phase_k_shared_slot_for_value(&name) {
                                let value = current_phase_k_broadcast()
                                    .and_then(|reg| reg.get(&topic))
                                    .map(|t| t.current_value())
                                    .filter(|bytes| !bytes.is_empty())
                                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                                    .unwrap_or(Value::Null);
                                env.insert(name, value);
                                continue;
                            }
                        }
                    }
                }
            }

            // Phase K hook-compile path: when `const [name, setter] =
            // useState(initial)` is recognised AND the current scope
            // has metadata for `name`, bind `name` to the current slot
            // value (initialising the slot from `initial` on first
            // access). The setter binding stays as Null in env — the
            // call site is intercepted by `eval_call_expr`.
            if let Pat::Array(array) = &decl.name {
                if let Some(init) = &decl.init {
                    if let Expr::Call(call) = init.as_ref() {
                        if is_use_state_in_phase_k_scope(call) {
                            let value_name = array
                                .elems
                                .first()
                                .and_then(|opt| opt.as_ref())
                                .and_then(|p| match p {
                                    Pat::Ident(ident) => Some(ident.id.sym.to_string()),
                                    _ => None,
                                });
                            let setter_name =
                                array.elems.get(1).and_then(|opt| opt.as_ref()).and_then(
                                    |p| match p {
                                        Pat::Ident(ident) => Some(ident.id.sym.to_string()),
                                        _ => None,
                                    },
                                );
                            if let Some(name) = value_name {
                                if let Some(slot_id) = phase_k_slot_for_value(&name) {
                                    let value = match phase_k_read_slot_value(slot_id) {
                                        Some(bytes) => serde_json::from_slice::<Value>(&bytes)
                                            .unwrap_or(Value::Null),
                                        None => {
                                            // First read for this slot — seed it from
                                            // the initial expression so the state
                                            // persists across re-renders.
                                            let initial_expr = phase_k_current_hook_initial(&name);
                                            let initial_value = initial_expr
                                                .as_ref()
                                                .map(|expr| {
                                                    self.eval_expr(module_spec, expr, env)
                                                        .unwrap_or(Value::Null)
                                                })
                                                .unwrap_or(Value::Null);
                                            if let Ok(bytes) = serde_json::to_vec(&initial_value) {
                                                phase_k_write_slot_value(slot_id, bytes);
                                                // Drain immediately — first-render
                                                // initialisations are not user-visible
                                                // mutations, they shouldn't show up
                                                // in the response opcode frame as a
                                                // SlotSet. Drop the pending entries
                                                // by draining and ignoring.
                                                drain_initial_slot_writes();
                                            }
                                            initial_value
                                        }
                                    };
                                    env.insert(name.clone(), value);
                                    if let Some(setter) = setter_name {
                                        env.insert(setter, Value::Null);
                                    }
                                    continue;
                                }
                            }
                        }
                    }
                }
            }

            // Phase J fallback: existing behaviour.
            let value = if let Some(init) = &decl.init {
                self.eval_expr(module_spec, init, env)
                    .unwrap_or(Value::Null)
            } else {
                Value::Null
            };
            apply_var_pat_to_env(&decl.name, value, env);
        }
    }

    fn eval_jsx_fragment(
        &self,
        module_spec: &str,
        fragment: &swc_ecma_ast::JSXFragment,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        self.render_children(module_spec, &fragment.children, env, false)
    }

    fn eval_jsx_element(
        &self,
        module_spec: &str,
        element: &swc_ecma_ast::JSXElement,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::*;

        let original_tag = match &element.opening.name {
            JSXElementName::Ident(ident) => ident.sym.to_string(),
            _ => return Err(anyhow!("unsupported JSX tag in module '{}'", module_spec)),
        };

        // Phase P · Stream E.1 — `<children />` is the layout-wrap
        // intrinsic. The renderer emits a fixed sentinel comment;
        // the manifest builder's `wrap_in_layouts` substitutes the
        // sentinel with the accumulated inner HTML after rendering
        // each layout independently. Lowercase tag so JSX treats it
        // as a host element (not a component reference) — we
        // intercept before the host-element path runs so no
        // `<children></children>` HTML ever lands in the output.
        // Attrs and nested children on `<children />` are ignored;
        // the intrinsic is a content sink, not a wrapper.
        if original_tag == "children" {
            return Ok(LAYOUT_CHILDREN_SENTINEL.to_string());
        }

        // Phase L · `<Link href="...">` desugars to an `<a href="..."
        // data-albedo-link>` host element. Rewriting the tag here
        // (rather than routing through `render_component_ref`) keeps
        // the entire path — id stamping, BindEvent emission, children
        // rendering — on the host-element track so `<Link>` enjoys
        // the same wire contract as any other lowercase tag. The
        // client-side runtime hooks `data-albedo-link` to intercept
        // clicks and request the route over WebTransport instead of
        // doing a full browser navigation.
        let (tag, link_rewrite) = if original_tag == "Link" {
            ("a".to_string(), true)
        } else {
            (original_tag.clone(), false)
        };

        if is_component_tag(&tag) {
            // Build-time tier split: a Tier-C child is a standalone hydration
            // island, not part of its Tier-A parent's static HTML. When the
            // manifest builder has installed the island set, emit nothing here
            // so the component renders only once — at its placeholder anchor.
            // EXCEPTION: while a layout-island map is installed (layout render),
            // emit the island's real placeholder div INLINE so it anchors at its
            // authored position in the layout — a layout has no `<children />`
            // slot for the separate placeholder collection to target.
            if island_skip_contains(&tag) {
                if let Some(placeholder_id) = layout_island_placeholder_for(&tag) {
                    // Emit RAW (no escaping): the serve path string-replaces this
                    // exact div, and placeholder ids are always `__c_<slug>_<id>`
                    // (no markup-significant chars), so escaping would only risk
                    // a mismatch.
                    return Ok(format!(
                        "<div id=\"{placeholder_id}\" data-albedo-tier=\"c\"></div>"
                    ));
                }
                return Ok(String::new());
            }

            let mut props = Map::new();
            for (name, value) in self.read_attrs(module_spec, &element.opening.attrs, env)? {
                if !name.starts_with("on") {
                    props.insert(name, value);
                }
            }

            let children = self.read_children_as_values(module_spec, &element.children, env)?;
            if !children.is_empty() {
                if children.len() == 1 {
                    props.insert("children".to_string(), children[0].clone());
                } else {
                    props.insert("children".to_string(), Value::Array(children));
                }
            }

            return self.render_component_ref(module_spec, &tag, &Value::Object(props));
        }

        let mut attrs = self.read_attrs(module_spec, &element.opening.attrs, env)?;

        // Phase L · attach the `<Link>` marker attribute to the
        // resulting `<a>` host element. `Value::Bool(true)` renders
        // as a bare HTML attribute (`data-albedo-link`) via
        // `render_attrs`, mirroring how the existing `required` etc.
        // flag attributes ship.
        if link_rewrite {
            attrs.push(("data-albedo-link".to_string(), Value::Bool(true)));
        }

        // Phase L · `<form action="action:NAME">` rewrite. The
        // renderer strips the sentinel `action="action:..."` and
        // substitutes `data-albedo-action="NAME"` so the client-side
        // runtime can intercept the submit. The action name is also
        // pushed onto the form-scope stack so descendant fields
        // (input / select / textarea) auto-emit a sibling
        // `data-albedo-error` span — addressable from server-side
        // validation patches via `allocate_field_error_id`.
        let form_action_name: Option<String> = if tag == "form" {
            let detected = attrs.iter().find_map(|(name, value)| {
                if name != "action" {
                    return None;
                }
                if let Value::String(raw) = value {
                    parse_form_action_sentinel(raw).map(str::to_string)
                } else {
                    None
                }
            });
            if let Some(action_name) = &detected {
                // Drop the sentinel `action` attribute and replace it
                // with `data-albedo-action` carrying the bare name.
                // Slot count stays balanced (1 in, 1 out) so the
                // rendered HTML still looks like a normal form
                // element with one form-action hook attached.
                attrs.retain(|(n, _)| n != "action");
                attrs.push((
                    FORM_ACTION_ATTR.to_string(),
                    Value::String(action_name.clone()),
                ));
            }
            detected
        } else {
            None
        };

        // Shell-stamp every host (lowercase-tag) element with a stable
        // `data-albedo-id`. Bakabox's `seedNodesFromDocument` looks for
        // exactly this attribute (DEFAULT_ANCHOR_ATTRIBUTE) at boot, so
        // this is the single contract that makes any future Tier-B/C
        // patch addressable. The id is derived BEFORE children render so
        // counter ordering is pre-order and matches client-side traversal.
        //
        // We don't override an explicit user-supplied `data-albedo-id`,
        // which lets test harnesses or static fragments pin a known id.
        let stable_id = match attrs
            .iter()
            .find(|(name, _)| name == ALBEDO_ID_ATTR)
            .and_then(|(_, value)| value.as_str())
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(existing) => existing,
            None => {
                let id = next_element_stable_id(module_spec);
                attrs.push((ALBEDO_ID_ATTR.to_string(), Value::String(id.to_string())));
                id
            }
        };

        // Phase K · emit BindEvent for every JSX `on*` handler attached
        // to this element. The proxy_ids were allocated at compile
        // time in source order; the per-scope cursor (`handlers_emitted`)
        // advances one per emit. event_id is the host-level event name
        // — the wire opcode carries the same lowercase string bakabox
        // already maps via `addEventListener`.
        if phase_k_enabled() {
            for attr in &element.opening.attrs {
                if let JSXAttrOrSpread::JSXAttr(jsx_attr) = attr {
                    if let JSXAttrName::Ident(name_ident) = &jsx_attr.name {
                        let name = name_ident.sym.to_string();
                        if name.starts_with("on") && name.len() > 2 {
                            let event_name = name[2..].to_ascii_lowercase();
                            if let Some(proxy_id) = phase_k_next_proxy_id_for_event(&event_name) {
                                phase_k_emit(Instruction::BindEvent {
                                    stable_id: StableId(stable_id),
                                    event_id: phase_k_event_id_for(&event_name),
                                    proxy_id: ProxyId(proxy_id),
                                });
                            }
                        } else if crate::runtime::eval::component::is_reserved_jsx_prop(&name) {
                            // A reserved prop is not an HTML attribute, so
                            // `render_attrs` never emits one. Binding it anyway
                            // would have the client *add* an attribute the
                            // server never rendered — the same leak, arriving
                            // from the CSR side instead.
                        } else if let Some(JSXAttrValue::JSXExprContainer(container)) =
                            &jsx_attr.value
                        {
                            // Phase K · attribute binding: when an attr value is a
                            // bare slot read (`className={cls}`, `value={text}`),
                            // bind it so a future SlotSet re-applies the attribute.
                            // Uses the HTML attribute name render_attrs emits
                            // (`className`→`class`) so the client patches the same
                            // attribute the server rendered. Derived expressions
                            // (ternary, concat) are not bare reads — left unbound.
                            if let JSXExpr::Expr(expr) = &container.expr {
                                let html_name = if name == "className" {
                                    "class"
                                } else {
                                    name.as_str()
                                };
                                if let Some(slot_id) = phase_k_detect_slot_text_read(expr) {
                                    phase_k_emit(Instruction::SetAttrRef {
                                        stable_id: StableId(stable_id),
                                        attr_id: phase_k_attr_id_for(html_name),
                                        slot_id,
                                    });
                                } else if let Some((resolved, deps)) =
                                    phase_k_collect_slot_deps(expr)
                                {
                                    // Derived attribute: `className={busy ? 'b' : ''}`.
                                    push_derived_binding(DerivedBindingRaw {
                                        stable_id,
                                        attr: Some(html_name.to_string()),
                                        deps,
                                        expr: resolved,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Push the element onto the scope-stack so a slot read inside
        // this element's children knows which `stable_id` to subscribe.
        phase_k_push_element(stable_id);

        // Phase L · enter the form-action scope before rendering
        // children so nested fields can find it via
        // `current_form_action_scope`. Paired with a pop after the
        // recursive children render returns.
        if let Some(action_name) = &form_action_name {
            push_form_action_scope(action_name.clone());
        }

        let attrs_html = render_attrs(&attrs);
        let children_html = self.render_children(module_spec, &element.children, env, false)?;
        let void_tag = is_void_tag(&tag);

        if form_action_name.is_some() {
            pop_form_action_scope();
        }

        phase_k_pop_element();

        // Phase L · for form-action forms, inject the hidden CSRF input
        // as the first child of the form body. The placeholder and the
        // per-session fill that later replaces its `value` are both
        // owned by `transforms::form` — the QuickJS shim emits this
        // same constant, so Tier-A and Tier-B forms are identical here.
        // Empty string for every non-form element so the format strings
        // below stay branch-free.
        let body_prefix: &'static str = if form_action_name.is_some() {
            CSRF_PLACEHOLDER_INPUT
        } else {
            ""
        };

        // Phase L · sibling `data-albedo-error` span for every named
        // field rendered inside a form-action form. The stable id is
        // `allocate_field_error_id(action, field_name)` so server-side
        // validation patches can target it via `SetText`. Field-tag
        // matching is conservative (input / select / textarea) and
        // the field must carry a `name` attribute — anything else is
        // unsubmittable and shouldn't pretend to have an error sink.
        let error_span_suffix: String = if let Some(action) = current_form_action_scope() {
            if matches!(tag.as_str(), "input" | "select" | "textarea") {
                attrs
                    .iter()
                    .find(|(n, _)| n == "name")
                    .and_then(|(_, v)| v.as_str().map(str::to_string))
                    .map(|field| {
                        let id = allocate_field_error_id(&action, &field);
                        format!(
                            "<span data-albedo-id=\"{id}\" data-albedo-error=\"{field}\"></span>"
                        )
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let element_html = if void_tag && children_html.is_empty() {
            if attrs_html.is_empty() {
                format!("<{tag} />")
            } else {
                format!("<{tag} {attrs_html} />")
            }
        } else if attrs_html.is_empty() {
            format!("<{tag}>{body_prefix}{children_html}</{tag}>")
        } else {
            format!("<{tag} {attrs_html}>{body_prefix}{children_html}</{tag}>")
        };

        Ok(format!("{element_html}{error_span_suffix}"))
    }

    fn render_component_ref(
        &self,
        module_spec: &str,
        component: &str,
        props: &Value,
    ) -> Result<String> {
        let module = self
            .modules
            .get(module_spec)
            .ok_or_else(|| anyhow!("module '{}' not loaded", module_spec))?;

        if let Some(import_binding) = module.imports.get(component) {
            if import_binding.source == "react" {
                return Ok(String::new());
            }
            let target = self
                .resolve_import(module_spec, &import_binding.source)
                .ok_or_else(|| {
                    anyhow!(
                        "could not resolve import '{}' from '{}'",
                        import_binding.source,
                        module_spec
                    )
                })?;
            return self.render_export(&target, &import_binding.export_name, props);
        }

        self.render_local(module_spec, component, props)
    }

    fn read_attrs(
        &self,
        module_spec: &str,
        attrs: &[swc_ecma_ast::JSXAttrOrSpread],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<(String, Value)>> {
        use swc_ecma_ast::*;
        let mut out = Vec::new();
        for attr in attrs {
            match attr {
                JSXAttrOrSpread::SpreadElement(_) => {
                    return Err(anyhow!("spread attributes are not supported"));
                }
                JSXAttrOrSpread::JSXAttr(attr) => {
                    let name = match &attr.name {
                        JSXAttrName::Ident(ident) => ident.sym.to_string(),
                        _ => return Err(anyhow!("unsupported JSX attribute name")),
                    };
                    let value = match &attr.value {
                        None => Value::Bool(true),
                        Some(JSXAttrValue::Lit(lit)) => lit_to_value(lit),
                        Some(JSXAttrValue::JSXExprContainer(container)) => match &container.expr {
                            JSXExpr::Expr(expr) => self.eval_expr(module_spec, expr, env)?,
                            JSXExpr::JSXEmptyExpr(_) => Value::Null,
                        },
                        _ => Value::Null,
                    };
                    out.push((name, value));
                }
            }
        }
        Ok(out)
    }

    fn read_children_as_values(
        &self,
        module_spec: &str,
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
    ) -> Result<Vec<Value>> {
        use swc_ecma_ast::*;
        let mut out = Vec::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        out.push(Value::String(normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        let value = self.eval_expr(module_spec, expr, env)?;
                        if !matches!(value, Value::Null | Value::Bool(false)) {
                            out.push(value);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    out.push(Value::String(self.eval_jsx_element(
                        module_spec,
                        element,
                        env,
                    )?));
                }
                JSXElementChild::JSXFragment(fragment) => {
                    out.push(Value::String(self.eval_jsx_fragment(
                        module_spec,
                        fragment,
                        env,
                    )?));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Render a JSX-bearing conditional (`{cond && <X/>}` / `{cond ? <A/> :
    /// <B/>}`) in binding mode. Appends the SSR HTML for the branch active
    /// under the initial state to `html`. When the conditional is eligible
    /// (client-computable `cond` + static branches) it also wraps that HTML in
    /// a `display:contents` element and records a [`ConditionalBindingRaw`] so
    /// the client can toggle it locally. When it isn't, it marks the render for
    /// the A3 island fallback and just renders the active branch (the payload
    /// is then discarded, so correctness comes from the fallback).
    fn render_jsx_conditional(
        &self,
        module_spec: &str,
        kind: &JsxConditional<'_>,
        env: &HashMap<String, Value>,
        html: &mut String,
    ) -> Result<()> {
        // Normalise both shapes to (test, true-branch, optional false-branch).
        // `And` has no false branch — it renders to nothing when falsy.
        let (cond, true_branch, false_branch): (
            &swc_ecma_ast::Expr,
            &swc_ecma_ast::Expr,
            Option<&swc_ecma_ast::Expr>,
        ) = match kind {
            JsxConditional::And { cond, branch } => (cond, branch, None),
            JsxConditional::Ternary { test, cons, alt } => (test, cons, Some(alt)),
        };

        // Is the test computable from reactive slots alone? `None` means either
        // a constant test (can't change client-side — a static render is then
        // correct) or one touching props/refs (also can't change via local
        // state). Either way, no binding is needed.
        let deps = phase_k_collect_slot_deps(cond);

        // Are both branches safe to pre-render and toggle wholesale?
        let branches_static =
            is_static_branch(true_branch) && false_branch.map(is_static_branch).unwrap_or(true);

        let active_truthy = is_truthy(&self.eval_expr(module_spec, cond, env)?);

        // Render the branch React would show for the initial state — used for
        // the SSR first paint either way.
        let active_branch = if active_truthy {
            Some(true_branch)
        } else {
            false_branch
        };

        // Eligible only when the test is slot-reactive AND both branches are
        // static. A slot-reactive test with a non-static branch is the case we
        // must NOT ship statically (it would go stale) → fall back to A3.
        if !(deps.is_some() && branches_static) {
            if deps.is_some() {
                // Slot-reactive but not representable fine-grained → A3 island.
                mark_structural_fallback();
            }
            if let Some(active) = active_branch {
                html.push_str(&self.render_branch_html(module_spec, active, env)?);
            }
            return Ok(());
        }

        // `phase_k_collect_slot_deps` returns the test with any resolvable
        // locals (useMemo / derived const) substituted in, plus its slot deps —
        // the same shape the derived rung lowers, so a `{ready && …}` where
        // `ready` is a `useMemo` recomputes correctly client-side.
        let (resolved_cond, deps) = deps.expect("eligible implies deps present");

        // Pre-render both branches (both are static, so both are crash-free).
        let html_true = self.render_branch_html(module_spec, true_branch, env)?;
        let html_false = match false_branch {
            Some(alt) => self.render_branch_html(module_spec, alt, env)?,
            None => String::new(),
        };

        // A `display:contents` wrapper carries the stable id without adding a
        // layout box, so toggling its innerHTML preserves the surrounding flow.
        let wrapper_id = next_element_stable_id(module_spec);
        let active_html = if active_truthy {
            &html_true
        } else {
            &html_false
        };
        html.push_str(&format!(
            "<span {ALBEDO_ID_ATTR}=\"{wrapper_id}\" style=\"display:contents\">{active_html}</span>"
        ));

        push_conditional_binding(ConditionalBindingRaw {
            stable_id: wrapper_id,
            deps,
            cond: resolved_cond,
            html_true,
            html_false,
        });
        Ok(())
    }

    /// Render one conditional branch expression to HTML. JSX elements/fragments
    /// render normally; an empty arm (`null`/`undefined`/`false`) renders to "".
    fn render_branch_html(
        &self,
        module_spec: &str,
        branch: &swc_ecma_ast::Expr,
        env: &HashMap<String, Value>,
    ) -> Result<String> {
        use swc_ecma_ast::Expr;
        match unwrap_paren(branch) {
            Expr::JSXElement(el) => self.eval_jsx_element(module_spec, el, env),
            Expr::JSXFragment(frag) => self.eval_jsx_fragment(module_spec, frag, env),
            other if is_empty_branch(other) => Ok(String::new()),
            // Any other expression (e.g. a string in one arm of a ternary whose
            // other arm is JSX) renders as its evaluated text.
            other => {
                let value = self.eval_expr(module_spec, other, env)?;
                if matches!(value, Value::Null | Value::Bool(false)) {
                    Ok(String::new())
                } else {
                    Ok(escape_html(&value_to_string(&value)))
                }
            }
        }
    }

    /// Binding mode · keyed-lists rung. Render `{ARRAY.map((item[,i]) => <JSX>)}`.
    /// When `ARRAY` is slot-reactive AND the item subtree is static-relative-to-
    /// item, emit a `display:contents` wrapper and record a [`ListBindingRaw`] so
    /// the client regenerates the list's `innerHTML` from live state. Otherwise
    /// mark the render for the A3 island fallback and just render the current
    /// list (the binding payload is then discarded; correctness comes from the
    /// fallback).
    fn render_jsx_list(
        &self,
        module_spec: &str,
        list: &JsxList<'_>,
        env: &HashMap<String, Value>,
        html: &mut String,
    ) -> Result<()> {
        // Is the array computable from reactive slots alone? `None` → it's a
        // prop/constant/unresolved (can't change via local state), so a static
        // render is correct and no binding is needed.
        let deps = phase_k_collect_slot_deps(list.array);
        let item_static =
            is_static_list_item(list.body, &list.item_param, list.index_param.as_deref());
        // An explicit `key={…}` on a single host-element item selects the keyed
        // reconcile lane; else the coarse innerHTML tier renders unchanged.
        let key_expr = extract_list_key_expr(list.body);

        // SSR first paint: evaluate the array and render each item with the item
        // (and index) param bound. This is the markup shipped for no-JS / before
        // the client boots; on boot the driver recomputes and reconciles it. On
        // the keyed lane, each row's `key={…}` is stamped as `data-albedo-key` by
        // `render_attrs` as the row renders, so the client sink seeds its row map
        // from the SSR rows rather than duplicating them — no separate stamp here.
        let array_value = self.eval_expr(module_spec, list.array, env)?;
        let mut inner = String::new();
        if let Value::Array(items) = &array_value {
            for (i, item) in items.iter().enumerate() {
                let mut child_env = env.clone();
                child_env.insert(list.item_param.clone(), item.clone());
                if let Some(index_param) = &list.index_param {
                    child_env.insert(index_param.clone(), Value::from(i as i64));
                }
                inner.push_str(&self.render_branch_html(module_spec, list.body, &child_env)?);
            }
        }

        // Eligible only when the array is slot-reactive AND items are static.
        if !(deps.is_some() && item_static) {
            if deps.is_some() {
                // Slot-reactive list we can't represent fine-grained → A3 island.
                mark_structural_fallback();
            }
            html.push_str(&inner);
            return Ok(());
        }

        let (resolved_array, deps) = deps.expect("eligible implies deps present");
        let wrapper_id = next_element_stable_id(module_spec);
        html.push_str(&format!(
            "<span {ALBEDO_ID_ATTR}=\"{wrapper_id}\" style=\"display:contents\">{inner}</span>"
        ));
        push_list_binding(ListBindingRaw {
            stable_id: wrapper_id,
            deps,
            array: resolved_array,
            item_param: list.item_param.clone(),
            index_param: list.index_param.clone(),
            item_body: list.body.clone(),
            key_expr,
        });
        Ok(())
    }

    fn render_children(
        &self,
        module_spec: &str,
        children: &[swc_ecma_ast::JSXElementChild],
        env: &HashMap<String, Value>,
        escape_expr_children: bool,
    ) -> Result<String> {
        use swc_ecma_ast::*;
        let mut html = String::new();
        for child in children {
            match child {
                JSXElementChild::JSXText(text) => {
                    if let Some(normalized) = normalize_jsx_text(text.value.as_ref()) {
                        html.push_str(&escape_html(&normalized));
                    }
                }
                JSXElementChild::JSXExprContainer(container) => match &container.expr {
                    JSXExpr::Expr(expr) => {
                        // Binding mode · conditionals rung. A `{cond && <JSX>}`
                        // or `{cond ? <A/> : <B/>}` is structural — state can
                        // change WHICH nodes exist, not just their text/attrs.
                        // When `cond` is client-computable and the branches are
                        // static, render it as a `display:contents` wrapper the
                        // client toggles via `innerHTML` (handled here); else
                        // mark the component for the A3 island fallback. Either
                        // way we skip the text/derived/eval path below.
                        if phase_k_enabled() {
                            if let Some(kind) = classify_jsx_conditional(expr) {
                                self.render_jsx_conditional(module_spec, &kind, env, &mut html)?;
                                continue;
                            }
                            // Binding mode · keyed-lists rung. `{arr.map(item =>
                            // <JSX>)}` is structural — state changes WHICH nodes
                            // exist. When the array is slot-reactive and items are
                            // static, render it as a `display:contents` wrapper the
                            // client regenerates via `innerHTML`; else fall back.
                            if let Some(list) = classify_jsx_list(expr) {
                                self.render_jsx_list(module_spec, &list, env, &mut html)?;
                                continue;
                            }
                        }
                        // Phase K: when the child expression is a bare
                        // slot-bound identifier, the rendered text node
                        // becomes a reactive binding site. Emit
                        // SetTextRef targeting the containing element
                        // (top of element_stack) so bakabox subscribes
                        // it to the slot store and re-applies on
                        // future SlotSet opcodes.
                        if let Some(slot_id) = phase_k_detect_slot_text_read(expr) {
                            if let Some(stable_id) = phase_k_top_element() {
                                phase_k_emit(Instruction::SetTextRef {
                                    stable_id: StableId(stable_id),
                                    slot_id,
                                });
                            }
                        } else if phase_k_enabled() {
                            // Derived text: `{count * 2}`, `{open ? 'a' : 'b'}` —
                            // reads slots but isn't a bare read. Record it so the
                            // client recomputes the value from state on change.
                            if let Some(stable_id) = phase_k_top_element() {
                                if let Some((resolved, deps)) = phase_k_collect_slot_deps(expr) {
                                    push_derived_binding(DerivedBindingRaw {
                                        stable_id,
                                        attr: None,
                                        deps,
                                        expr: resolved,
                                    });
                                }
                            }
                        }
                        let value = self.eval_expr(module_spec, expr, env)?;
                        if matches!(value, Value::Null | Value::Bool(false)) {
                            continue;
                        }
                        let text = value_to_string(&value);
                        if escape_expr_children {
                            html.push_str(&escape_html(&text));
                        } else {
                            html.push_str(&text);
                        }
                    }
                    JSXExpr::JSXEmptyExpr(_) => {}
                },
                JSXElementChild::JSXElement(element) => {
                    html.push_str(&self.eval_jsx_element(module_spec, element, env)?);
                }
                JSXElementChild::JSXFragment(fragment) => {
                    html.push_str(&self.eval_jsx_fragment(module_spec, fragment, env)?);
                }
                _ => {}
            }
        }
        Ok(html)
    }

    fn resolve_import(&self, current_module: &str, source: &str) -> Option<String> {
        if !source.starts_with('.') {
            return None;
        }

        let current_dir = Path::new(current_module)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let base = normalize_specifier(current_dir.join(source));
        for candidate in import_candidates(&base) {
            if self.modules.contains_key(&candidate) {
                return Some(candidate);
            }
        }

        if let Some(stripped) = source.strip_prefix("./components/") {
            let alt = normalize_specifier(PathBuf::from(stripped));
            for candidate in import_candidates(&alt) {
                if self.modules.contains_key(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }
}

/// Human-readable label for a statement the pure-Rust evaluator does not model,
/// used in the loud-error message so the dev knows exactly which construct hit
/// the Tier-B/C boundary.
fn statement_kind(stmt: &swc_ecma_ast::Stmt) -> &'static str {
    use swc_ecma_ast::{Decl, Stmt};
    match stmt {
        Stmt::For(_) => "for",
        Stmt::ForIn(_) => "for-in",
        Stmt::ForOf(_) => "for-of",
        Stmt::While(_) => "while",
        Stmt::DoWhile(_) => "do-while",
        Stmt::Try(_) => "try/catch",
        Stmt::Switch(_) => "switch",
        Stmt::Throw(_) => "throw",
        Stmt::Break(_) => "break",
        Stmt::Continue(_) => "continue",
        Stmt::Labeled(_) => "labelled statement",
        Stmt::With(_) => "with",
        Stmt::Decl(Decl::Fn(_)) => "function declaration",
        Stmt::Decl(Decl::Class(_)) => "class declaration",
        Stmt::Decl(_) => "declaration",
        Stmt::Debugger(_) => "debugger",
        _ => "statement",
    }
}

pub fn render_from_components_dir(
    components_root: impl AsRef<Path>,
    entry_module: &str,
    props: &Value,
) -> Result<String> {
    let project = ComponentProject::load_from_dir(components_root)?;
    project.render_entry(entry_module, props)
}
