//! Phase L · `<form action="action:NAME">` extractor.
//!
//! Surfaces every JSX `<form>` whose `action` attribute begins with
//! the sentinel prefix `"action:"`. The suffix is the action name the
//! server has (or will) register a handler under via
//! `register_form_action(name, ...)`.
//!
//! At render time the runtime emits an HTML `<form>` decorated with
//! `data-albedo-action="NAME"` so the client-side runtime can
//! intercept the submit event, serialize the FormData as a JSON
//! object, and POST an `ActionEnvelope` to `/_albedo/action`. The
//! envelope's `action_id` is `fnv1a_32(NAME)`; the payload is the
//! JSON bytes.
//!
//! This pass also surfaces the declared input / select / textarea
//! field names so the typed `register_form_action::<T>(...)`
//! shorthand can validate the form↔struct shape at registration time
//! (Stage 2) and so the renderer can emit `data-albedo-error` slots
//! for each declared field (Stage 1).

use swc_ecma_ast::{
    BlockStmtOrExpr, Decl, Expr, JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElement,
    JSXElementChild, JSXElementName, JSXExpr, Lit, Stmt,
};

/// Sentinel prefix the extractor and renderer both match on a
/// `<form>`'s `action` attribute to flag it as an Albedo form action
/// rather than a plain HTML form. Kept here so both sides share the
/// same literal.
pub const FORM_ACTION_PREFIX: &str = "action:";

// ─── Served-markup contract ──────────────────────────────────────────
//
// A form action is rendered by TWO independent renderers — the
// pure-Rust evaluator (`runtime::eval::core`, Tier-A) and the QuickJS
// `h()` shim (`runtime::quickjs_engine`, Tier-B/C) — and the token its
// CSRF input carries is filled in afterwards by a THIRD party (the
// server, post-render). Each of those used to spell the markup out for
// itself, and they drifted: the QuickJS path emitted no CSRF input at
// all, so a Tier-B form submitted with no token — and the gate, which
// keyed off the token being *present*, waved it straight through.
//
// This section is the single spelling. Renderers emit
// `FORM_ACTION_ATTR` + `CSRF_PLACEHOLDER_INPUT`; the server calls
// `fill_csrf_tokens`. The QuickJS shim receives these same constants
// injected as JS values rather than restating them across the language
// boundary. Nothing downstream re-types the literals, so there is no
// longer a pair of spellings that can disagree.

/// Attribute the renderers stamp in place of the `action="action:NAME"`
/// sentinel, carrying the bare action name. The client runtime
/// (`assets/albedo-link-forms.js`) keys its submit interception on it.
pub const FORM_ACTION_ATTR: &str = "data-albedo-action";

/// `name` of the hidden field carrying the per-session CSRF token.
/// Renderers emit it; the action dispatcher reads it back off the
/// submitted JSON payload.
pub const CSRF_FIELD_NAME: &str = "_csrf";

/// Marker attribute identifying a CSRF input the server still has to
/// fill. Present in both the placeholder and the filled output — it is
/// the anchor [`fill_csrf_tokens`] matches on.
pub const CSRF_MARKER_ATTR: &str = "data-albedo-csrf";

/// The hidden CSRF input every renderer emits as the first child of a
/// form-action `<form>`.
///
/// `value` is deliberately EMPTY here: rendering is not per-session
/// (Tier-A markup is baked at build time, and island markup is
/// precomputed once at boot), so the renderer has no session to mint a
/// token for. [`fill_csrf_tokens`] stamps the real token into every
/// placeholder at request time, once the session is known.
pub const CSRF_PLACEHOLDER_INPUT: &str =
    r#"<input type="hidden" name="_csrf" value="" data-albedo-csrf />"#;

/// The exact `value=""` + marker sequence [`fill_csrf_tokens`]
/// rewrites. A substring of [`CSRF_PLACEHOLDER_INPUT`] by construction
/// — `emitted_placeholder_contains_the_fill_anchor` fails the build if
/// that ever stops being true, which is the check that keeps emission
/// and fill from drifting apart.
const CSRF_EMPTY_VALUE_ANCHOR: &str = r#"value="" data-albedo-csrf"#;

/// Reads the action name out of a `<form>`'s `action` attribute value,
/// or `None` when it isn't a form-action sentinel (a plain HTML form,
/// which every renderer must pass through untouched).
#[must_use]
pub fn form_action_name(action_attr: &str) -> Option<&str> {
    action_attr.strip_prefix(FORM_ACTION_PREFIX)
}

/// Phase L · post-render CSRF fill.
///
/// Replaces the empty `value` of every [`CSRF_PLACEHOLDER_INPUT`] in
/// `html` with `token`. The server calls this once per rendered chunk,
/// after any island markup has been spliced in, so a form nested inside
/// an island is filled by the same pass as one in the shell.
///
/// A byte-for-byte literal replace is deliberate: the placeholder is a
/// constant this module owns, so its shape is not in question and an
/// HTML parser would be pure cost. Returns the input unchanged when no
/// marker is present — the common case (any page without a form).
#[must_use]
pub fn fill_csrf_tokens(html: &str, token: &str) -> String {
    if !html.contains(CSRF_EMPTY_VALUE_ANCHOR) {
        return html.to_string();
    }
    let filled = format!("value=\"{token}\" {CSRF_MARKER_ATTR}");
    html.replace(CSRF_EMPTY_VALUE_ANCHOR, &filled)
}

/// One `<form action="action:NAME">` surfaced by
/// [`extract_forms_in_function`].
#[derive(Debug, Clone)]
pub struct FormExtract {
    /// Position among all form-action elements in this component, in
    /// source-traversal order. Stable across recompilations of the
    /// same source.
    pub form_idx: usize,
    /// Raw action name following the `action:` prefix.
    pub action_name: String,
    /// Stable u32 id derived from `action_name` via FNV-1a-32 — equal
    /// to the `action_id` field of the `ActionEnvelope` the client
    /// will POST when this form is submitted. Pre-computed at
    /// extraction time so the renderer doesn't re-hash on every
    /// render.
    pub action_id: u32,
    /// Form HTTP method. Only POST is meaningfully supported; GET
    /// forms exist for completeness and bypass the action dispatcher.
    pub method: FormMethod,
    /// Declared form fields (input/select/textarea) in source order.
    pub fields: Vec<FormField>,
}

/// HTTP method declared on the form element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMethod {
    Get,
    Post,
}

/// One declared form field surfaced from the JSX subtree of a form.
#[derive(Debug, Clone)]
pub struct FormField {
    /// `name` attribute the field will submit under.
    pub name: String,
    /// Kind inferred from the tag + `type` attribute. The typed
    /// `register_form_action::<T>` adapter uses this to sanity-check
    /// the target struct's fields.
    pub kind: FormFieldKind,
    /// True when the JSX carries a `required` attribute (bare or
    /// `required={true}`). Server-side typed deserialization may also
    /// infer required-ness from the target struct's field
    /// optionality; this flag is the declared-in-JSX truth.
    pub required: bool,
}

/// Inferred kind of one form field. `Other` carries the originating
/// HTML tag for forward-compatibility with element types the typed
/// decoder hasn't grown a representation for yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldKind {
    Text,
    Number,
    Boolean,
    File,
    Other(&'static str),
}

/// FNV-1a-32 of the action name. Matches the family
/// `runtime::eval::component::fnv1a_32` exports; the server's
/// `register_form_action` uses the same hash so the wire `action_id`
/// is correct on both sides.
pub fn allocate_form_action_id(action_name: &str) -> u32 {
    fnv1a_32(action_name.as_bytes())
}

/// FNV-1a-32 of the field-error key, used for stable
/// `data-albedo-error` stamps. Server-side validation handlers
/// compute the same id when emitting `SetText` opcodes that target
/// the error span; client-side bakabox applies the patch against the
/// span the renderer stamped at the same id.
pub fn allocate_field_error_id(action_name: &str, field_name: &str) -> u32 {
    let key = format!("form-error:{action_name}:{field_name}");
    fnv1a_32(key.as_bytes())
}

/// Walks every JSX `<form>` in the function body and returns the
/// metadata for those whose `action` is a form-action sentinel.
///
/// Plain HTML forms (no `action:` prefix) are not surfaced; the
/// runtime emits them as-is, preserving the standard browser submit
/// behaviour.
pub fn extract_forms_in_function(stmts: &[Stmt]) -> Vec<FormExtract> {
    let mut sink = Vec::new();
    for stmt in stmts {
        visit_stmt_for_jsx(stmt, &mut sink);
    }
    sink
}

/// Statement-level recursion entry point. Structurally identical to
/// the other Phase-K/L extractors so the three can be fused into one
/// walker later.
fn visit_stmt_for_jsx(stmt: &Stmt, sink: &mut Vec<FormExtract>) {
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
        Stmt::Decl(Decl::Var(var)) => {
            for d in &var.decls {
                if let Some(init) = &d.init {
                    visit_expr_for_jsx(init, sink);
                }
            }
        }
        _ => {}
    }
}

/// Expression-level walker; descends into the subset of expressions
/// the Phase J renderer also descends into.
fn visit_expr_for_jsx(expr: &Expr, sink: &mut Vec<FormExtract>) {
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
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::Expr(e) => visit_expr_for_jsx(e, sink),
            BlockStmtOrExpr::BlockStmt(b) => {
                for s in &b.stmts {
                    visit_stmt_for_jsx(s, sink);
                }
            }
        },
        _ => {}
    }
}

/// Visit one element. If it's a `<form>` with an `action:NAME`
/// attribute, surface the form metadata and walk children to collect
/// the declared input fields. Non-form elements still recurse into
/// their children so nested form-actions (legal but unusual) are
/// caught.
fn visit_element(element: &JSXElement, sink: &mut Vec<FormExtract>) {
    if is_form_tag(&element.opening.name) {
        if let Some((action_name, method)) = read_form_action(&element.opening.attrs) {
            let mut fields = Vec::new();
            collect_form_fields_recursive(&element.children, &mut fields);
            let form_idx = sink.len();
            let action_id = allocate_form_action_id(&action_name);
            sink.push(FormExtract {
                form_idx,
                action_name,
                action_id,
                method,
                fields,
            });
            return;
        }
    }

    for child in &element.children {
        visit_child(child, sink);
    }
}

/// True for the bare HTML tag `<form>`. Member-expression / namespaced
/// forms are not matched.
fn is_form_tag(name: &JSXElementName) -> bool {
    matches!(name, JSXElementName::Ident(ident) if ident.sym.as_ref() == "form")
}

/// Returns `(action_name, method)` when the form's `action` attribute
/// starts with `action:`, else `None`.
///
/// The method defaults to POST (the only meaningful option for an
/// action form); an explicit `method="get"` overrides for forms that
/// prefer query-string submission.
fn read_form_action(attrs: &[JSXAttrOrSpread]) -> Option<(String, FormMethod)> {
    let mut action_name = None;
    let mut method = FormMethod::Post;

    for attr in attrs {
        let JSXAttrOrSpread::JSXAttr(attr) = attr else {
            continue;
        };
        let JSXAttrName::Ident(name_ident) = &attr.name else {
            continue;
        };
        let attr_name = name_ident.sym.as_ref();

        match attr_name {
            "action" => {
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                    let value = s.value.to_string();
                    if let Some(rest) = value.strip_prefix(FORM_ACTION_PREFIX) {
                        action_name = Some(rest.to_string());
                    }
                }
            }
            "method" => {
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                    let value = s.value.as_ref().to_ascii_lowercase();
                    if value == "get" {
                        method = FormMethod::Get;
                    }
                }
            }
            _ => {}
        }
    }

    action_name.map(|name| (name, method))
}

/// Walks the children of a `<form>` collecting every named input,
/// select, and textarea. Recurses through fragments and nested
/// elements so wrappers (`<fieldset>`, `<div>`, etc.) don't hide the
/// fields. JSX expression containers (`{cond && <input ... />}`) are
/// also descended.
fn collect_form_fields_recursive(children: &[JSXElementChild], out: &mut Vec<FormField>) {
    for child in children {
        match child {
            JSXElementChild::JSXElement(element) => {
                if let Some(field) = read_field_from_element(element) {
                    out.push(field);
                }
                collect_form_fields_recursive(&element.children, out);
            }
            JSXElementChild::JSXFragment(fragment) => {
                collect_form_fields_recursive(&fragment.children, out);
            }
            JSXElementChild::JSXExprContainer(container) => {
                if let JSXExpr::Expr(expr) = &container.expr {
                    collect_form_fields_from_expr(expr, out);
                }
            }
            _ => {}
        }
    }
}

/// Form-field collector for expression positions — symmetric with
/// `collect_form_fields_recursive`. Phase J's renderer evaluates the
/// same expression shapes, so fields they produce must be surfaced.
fn collect_form_fields_from_expr(expr: &Expr, out: &mut Vec<FormField>) {
    match expr {
        Expr::JSXElement(element) => {
            if let Some(field) = read_field_from_element(element) {
                out.push(field);
            }
            collect_form_fields_recursive(&element.children, out);
        }
        Expr::JSXFragment(fragment) => {
            collect_form_fields_recursive(&fragment.children, out);
        }
        Expr::Paren(p) => collect_form_fields_from_expr(&p.expr, out),
        Expr::Cond(c) => {
            collect_form_fields_from_expr(&c.cons, out);
            collect_form_fields_from_expr(&c.alt, out);
        }
        _ => {}
    }
}

/// Read a single `<input>` / `<select>` / `<textarea>` and surface
/// its declared `name`, kind, and `required` flag. Returns `None` for
/// elements that aren't fields or that omit `name` (an HTML form
/// field with no name is unsubmittable).
fn read_field_from_element(element: &JSXElement) -> Option<FormField> {
    let tag = match &element.opening.name {
        JSXElementName::Ident(ident) => ident.sym.as_ref(),
        _ => return None,
    };

    // For `<input>` we defer the kind decision until after we've seen
    // the `type` attribute; for `<select>` / `<textarea>` we can fix
    // the kind up front.
    let predetermined_kind = match tag {
        "input" => None,
        "select" => Some(FormFieldKind::Other("select")),
        "textarea" => Some(FormFieldKind::Text),
        _ => return None,
    };

    let mut name: Option<String> = None;
    let mut required = false;
    let mut input_type = "text".to_string();

    for attr in &element.opening.attrs {
        let JSXAttrOrSpread::JSXAttr(attr) = attr else {
            continue;
        };
        let JSXAttrName::Ident(name_ident) = &attr.name else {
            continue;
        };
        match name_ident.sym.as_ref() {
            "name" => {
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                    name = Some(s.value.to_string());
                }
            }
            "type" => {
                if let Some(JSXAttrValue::Lit(Lit::Str(s))) = &attr.value {
                    input_type = s.value.to_string().to_ascii_lowercase();
                }
            }
            "required" => {
                // Bare `required` and `required={true}` both mean
                // required; `required={false}` opts out explicitly.
                required = match &attr.value {
                    None => true,
                    Some(JSXAttrValue::Lit(Lit::Bool(b))) => b.value,
                    _ => true,
                };
            }
            _ => {}
        }
    }

    let name = name?;
    let final_kind = predetermined_kind.unwrap_or_else(|| match input_type.as_str() {
        "number" | "range" => FormFieldKind::Number,
        "checkbox" => FormFieldKind::Boolean,
        "file" => FormFieldKind::File,
        "text" | "email" | "password" | "tel" | "url" | "search" | "hidden" | "date" | "time" => {
            FormFieldKind::Text
        }
        _ => FormFieldKind::Other("input"),
    });

    Some(FormField {
        name,
        kind: final_kind,
        required,
    })
}

/// Generic JSX-child walker — symmetric with the event/link
/// extractors.
fn visit_child(child: &JSXElementChild, sink: &mut Vec<FormExtract>) {
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

/// FNV-1a-32 — vendored here so this module doesn't reach into the
/// runtime crate's eval helpers. Bytes match
/// `runtime::eval::component::fnv1a_32`; both produce identical
/// `action_id` and `data-albedo-error` ids for the same input.
fn fnv1a_32(data: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for byte in data {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// The structural invariant the whole contract rests on: the
    /// sequence `fill_csrf_tokens` searches for must actually occur in
    /// the markup the renderers emit. If someone reformats the
    /// placeholder (reorders the attributes, drops the space, switches
    /// quote style) the fill silently stops matching and every form
    /// ships `value=""` — which looks completely fine in the HTML and
    /// fails only at submit time. This test is what makes that a build
    /// failure instead.
    #[test]
    fn emitted_placeholder_contains_the_fill_anchor() {
        assert!(
            CSRF_PLACEHOLDER_INPUT.contains(CSRF_EMPTY_VALUE_ANCHOR),
            "the fill anchor must be a substring of the emitted placeholder",
        );
        assert!(CSRF_PLACEHOLDER_INPUT.contains(&format!("name=\"{CSRF_FIELD_NAME}\"")));
        assert!(CSRF_PLACEHOLDER_INPUT.contains(CSRF_MARKER_ATTR));
    }

    /// Emission → fill, composed end to end: the token a browser would
    /// actually submit. Asserting on the composition (rather than each
    /// half in isolation) is the point — the bug this contract exists
    /// to prevent lived precisely in the seam between the two.
    #[test]
    fn emit_then_fill_yields_a_submittable_token() {
        let filled = fill_csrf_tokens(CSRF_PLACEHOLDER_INPUT, "deadbeef");
        assert!(filled.contains("value=\"deadbeef\""));
        assert!(!filled.contains("value=\"\""), "no empty value may survive");
        assert!(
            filled.contains(CSRF_MARKER_ATTR),
            "the marker survives the fill",
        );
    }

    #[test]
    fn fill_is_a_noop_without_a_placeholder() {
        let plain = "<div>no forms here</div>";
        assert_eq!(fill_csrf_tokens(plain, "abc123"), plain);
    }

    #[test]
    fn fill_covers_every_form_on_the_page() {
        let page =
            format!("<form>{CSRF_PLACEHOLDER_INPUT}</form><form>{CSRF_PLACEHOLDER_INPUT}</form>");
        let filled = fill_csrf_tokens(&page, "tok");
        assert_eq!(filled.matches("value=\"tok\"").count(), 2);
        assert!(!filled.contains("value=\"\""));
    }

    #[test]
    fn form_action_name_reads_only_the_sentinel() {
        assert_eq!(
            form_action_name("action:sign_guestbook"),
            Some("sign_guestbook")
        );
        // A plain HTML form must pass through untouched on every renderer.
        assert_eq!(form_action_name("/submit"), None);
        assert_eq!(form_action_name(""), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use swc_common::{FileName, SourceMap};
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};

    fn parse_body(source: &str) -> Vec<Stmt> {
        let cm: Rc<SourceMap> = Rc::new(SourceMap::default());
        let fm = cm.new_source_file(
            FileName::Custom("t.jsx".into()).into(),
            source.to_string(),
        );
        let mut parser = Parser::new(
            Syntax::Es(EsSyntax {
                jsx: true,
                ..Default::default()
            }),
            StringInput::from(&*fm),
            None,
        );
        let module = parser.parse_module().expect("parse");
        for item in module.body {
            if let swc_ecma_ast::ModuleItem::Stmt(Stmt::Decl(Decl::Fn(fn_decl))) = item {
                if let Some(body) = fn_decl.function.body {
                    return body.stmts;
                }
            }
        }
        Vec::new()
    }

    #[test]
    fn extracts_form_action_with_fields() {
        let stmts = parse_body(
            r#"
            function Login() {
                return (
                    <form action="action:submit_login">
                        <input name="user" type="text" required />
                        <input name="pass" type="password" />
                        <button type="submit">Go</button>
                    </form>
                );
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 1);
        let f = &forms[0];
        assert_eq!(f.action_name, "submit_login");
        assert_eq!(f.method, FormMethod::Post);
        assert_eq!(f.action_id, allocate_form_action_id("submit_login"));
        assert_eq!(f.fields.len(), 2);
        assert_eq!(f.fields[0].name, "user");
        assert!(f.fields[0].required);
        assert_eq!(f.fields[0].kind, FormFieldKind::Text);
        assert_eq!(f.fields[1].name, "pass");
        assert!(!f.fields[1].required);
        assert_eq!(f.fields[1].kind, FormFieldKind::Text);
    }

    #[test]
    fn collects_fields_through_wrappers_and_fragments() {
        let stmts = parse_body(
            r#"
            function W() {
                return (
                    <form action="action:save">
                        <fieldset>
                            <input name="title" type="text" />
                            <>
                                <input name="qty" type="number" required />
                                <input name="published" type="checkbox" />
                            </>
                        </fieldset>
                    </form>
                );
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 1);
        let names: Vec<_> = forms[0].fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["title", "qty", "published"]);
        assert_eq!(forms[0].fields[1].kind, FormFieldKind::Number);
        assert_eq!(forms[0].fields[2].kind, FormFieldKind::Boolean);
    }

    #[test]
    fn ignores_plain_form_without_action_sentinel() {
        let stmts = parse_body(
            r#"
            function P() {
                return <form action="/raw"><input name="q" /></form>;
            }
        "#,
        );
        assert!(extract_forms_in_function(&stmts).is_empty());
    }

    #[test]
    fn allocates_stable_action_id() {
        let a = allocate_form_action_id("submit_login");
        let b = allocate_form_action_id("submit_login");
        assert_eq!(a, b);
        assert_ne!(a, allocate_form_action_id("submit_signup"));
    }

    #[test]
    fn field_error_id_is_namespaced_by_form_and_field() {
        let a = allocate_field_error_id("submit_login", "user");
        let b = allocate_field_error_id("submit_login", "pass");
        let c = allocate_field_error_id("submit_signup", "user");
        assert_ne!(a, b);
        assert_ne!(a, c);
        // Same inputs are deterministic.
        assert_eq!(a, allocate_field_error_id("submit_login", "user"));
    }

    #[test]
    fn picks_up_method_get_override() {
        let stmts = parse_body(
            r#"
            function S() {
                return <form action="action:query" method="get"><input name="q" /></form>;
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms[0].method, FormMethod::Get);
    }
}
