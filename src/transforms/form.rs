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

/// Path prefix of the endpoint a form-action `<form>` actually posts to.
///
/// ## Why a real URL exists at all now
///
/// The sentinel used to be *replaced* by [`FORM_ACTION_ATTR`] and nothing else,
/// so the served `<form>` had **no `action` attribute**. A browser resolves that
/// to the current URL, and a page route answers `405` to a POST — which meant the
/// only working write path in the framework ran through
/// `assets/albedo-link-forms.js`. "Zero JS" described the render and not a single
/// mutation.
///
/// Emitting the endpoint restores the browser's own submit as the baseline: with
/// JavaScript the interceptor still calls `preventDefault()` and posts a bincode
/// envelope for an in-place patch; without it the browser posts the form to this
/// URL and gets `303 See Other` back to the page it came from. Same action, same
/// handler, same CSRF gate.
///
/// ## Why the *name* and not the `action_id`
///
/// The id is `fnv1a_32(name)` and is what the registry is keyed by, so putting it
/// in the URL would work. The name is used instead because the server can then
/// route, gate and *price* the request from the request line alone —
/// `AUTH.md` § "P2's shape": our `action_id` rides inside a bincode envelope, so
/// today the server cannot know what an action is until it has buffered the whole
/// body, which is why SHUTTER charges a flat `Write` before it knows what it is
/// charging for and why streaming uploads are foreclosed. A readable segment also
/// makes a failing submit greppable, which a `u32` never is.
pub const ACTION_ENDPOINT_PREFIX: &str = "/_albedo/action/";

/// `name` of the hidden field naming the page a no-JS submit returns to.
///
/// Mirrored in `albedo_server::forms`, which re-exports this constant rather
/// than restating it — the CSRF input already demonstrated what two spellings of
/// one markup contract costs.
pub const RETURN_FIELD_NAME: &str = "_albedo_return";

/// Marker attribute identifying a return-path input the server still has to
/// fill. The anchor [`fill_return_paths`] matches on.
pub const RETURN_MARKER_ATTR: &str = "data-albedo-return";

/// The hidden return-path input every renderer emits as a child of a
/// form-action `<form>`, beside the CSRF input.
///
/// Empty for the same reason the CSRF placeholder is: Tier-A markup is baked at
/// build time and island markup is precomputed once at boot, so the renderer has
/// no request whose path it could stamp. [`fill_return_paths`] fills it once the
/// request is known.
///
/// 🔑 **This is not a redirect the client gets to choose.** The value is
/// re-parsed server-side by `albedo_server::forms::ReturnPath`, which accepts
/// only a rooted same-origin path — so an edited field can at worst send the
/// submitter to a different page of the same app. A login form that trusted this
/// value would be the classic credential-phishing chain, which is why the
/// validation lives at the reader and not here.
pub const RETURN_PLACEHOLDER_INPUT: &str =
    r#"<input type="hidden" name="_albedo_return" value="" data-albedo-return />"#;

/// The exact `value=""` + marker sequence [`fill_return_paths`] rewrites.
/// A substring of [`RETURN_PLACEHOLDER_INPUT`] by construction — asserted by
/// `emitted_placeholder_contains_the_fill_anchor`.
const RETURN_EMPTY_VALUE_ANCHOR: &str = r#"value="" data-albedo-return"#;

/// APERTURE A3 · the field carrying this render's **intent token**.
pub const INTENT_FIELD_NAME: &str = "_albedo_intent";

/// Marker attribute identifying an intent input the server still has to fill.
pub const INTENT_MARKER_ATTR: &str = "data-albedo-intent";

/// The hidden intent input every renderer emits beside the CSRF one.
///
/// # What the token means, and why it is per *render*
///
/// It answers the one question no server can answer for itself: **is this
/// submit the same intention as the last one, or a new one?** Two deliberate
/// clicks and one retry produce byte-identical requests, so the distinction has
/// to come from the client — which is why every API that offers idempotency
/// (Stripe's included) takes the key from the caller.
///
/// Per render is exactly the right granularity, and it falls out for free:
///
/// * a no-JS resubmit (`F5` → *resend?*) replays this same hidden field, so the
///   workflow **resumes** rather than starting a second one;
/// * a second deliberate submit arrives from a fresh page render with a fresh
///   token, so it is correctly a **new** intention;
/// * the client runtime reuses it across its own network retries.
///
/// The author writes nothing. That is the difference between this and every
/// idempotency-key API a person has had to hold in their head.
///
/// Empty for the same reason the CSRF placeholder is — Tier-A markup is baked
/// at build time and island markup is precomputed once at boot, so the renderer
/// has no request to mint against. [`fill_intent_tokens`] fills it.
pub const INTENT_PLACEHOLDER_INPUT: &str =
    r#"<input type="hidden" name="_albedo_intent" value="" data-albedo-intent />"#;

/// The exact `value=""` + marker sequence [`fill_intent_tokens`] rewrites.
const INTENT_EMPTY_VALUE_ANCHOR: &str = r#"value="" data-albedo-intent"#;

/// Every hidden input, in the order a renderer emits them.
///
/// A `concat!` and therefore a restatement of the constants above, which is
/// the drift this module exists to prevent — so `the_fused_prefix_is_exactly_its_parts`
/// fails the build the moment it stops being their concatenation. Const string
/// concatenation of two `const`s is not expressible in stable Rust without a
/// macro crate, and a test that cannot pass silently is cheaper than the
/// dependency.
pub const FORM_HIDDEN_INPUTS: &str = concat!(
    r#"<input type="hidden" name="_csrf" value="" data-albedo-csrf />"#,
    r#"<input type="hidden" name="_albedo_return" value="" data-albedo-return />"#,
    r#"<input type="hidden" name="_albedo_intent" value="" data-albedo-intent />"#,
);

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

/// Whether a **plain** `<form>` — one carrying no `action:` sentinel — still
/// needs the hidden inputs [`FORM_HIDDEN_INPUTS`] supplies.
///
/// ## Why this exists
///
/// The sentinel was the only thing that earned a CSRF token, which quietly made
/// one kind of form **unauthorable**: the first-party sign-in endpoints
/// (`/_albedo/auth/password/login` and friends) are real URLs, not action names,
/// so a `<form action="/_albedo/auth/password/login" method="POST">` rendered
/// with no token and `run_auth_route` answered every submit with `403 This form
/// is stale`. There was no spelling of a working login form. `AUTH.md`
/// § "P2's shape" asks for the general seam rather than a login special case,
/// and this is it: the rule is about *where the form posts*, not about which
/// feature asked.
///
/// ## The rule, and why each half of it
///
/// - **POST only.** A GET form puts its fields in the URL, so a token emitted
///   there would land in the history, the `Referer` and the access log — the
///   leak the token exists to prevent. A GET form is also not a mutation, so
///   there is nothing to forge.
/// - **Same-origin only**, meaning an absent `action` (the browser resolves it
///   to the current URL) or one that is a **rooted path**. 🔑 An absolute or
///   protocol-relative `action` is refused *specifically* because emitting the
///   token there would hand this session's CSRF token to a third-party origin on
///   the next submit — a token-disclosure bug introduced by the very mechanism
///   meant to stop forgery. `//evil.example` is a URL, not a path, which is why
///   the `//` case is rejected rather than treated as rooted.
///
/// Both renderers ask this one function, for the reason the "served-markup
/// contract" section above exists: the CSRF input had two spellings once
/// already, and the Tier-B copy emitted nothing.
#[must_use]
pub fn plain_form_needs_hidden_inputs(action: Option<&str>, method: Option<&str>) -> bool {
    if !method.is_some_and(|m| m.trim().eq_ignore_ascii_case("post")) {
        return false;
    }
    match action.map(str::trim) {
        None | Some("") => true,
        Some(path) => path.starts_with('/') && !path.starts_with("//"),
    }
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
pub fn fill_intent_tokens(html: &str, token: &str) -> String {
    if !html.contains(INTENT_EMPTY_VALUE_ANCHOR) {
        return html.to_string();
    }
    let filled = format!("value=\"{token}\" {INTENT_MARKER_ATTR}");
    html.replace(INTENT_EMPTY_VALUE_ANCHOR, &filled)
}

/// Stamp the session's CSRF token into every placeholder.
pub fn fill_csrf_tokens(html: &str, token: &str) -> String {
    if !html.contains(CSRF_EMPTY_VALUE_ANCHOR) {
        return html.to_string();
    }
    let filled = format!("value=\"{token}\" {CSRF_MARKER_ATTR}");
    html.replace(CSRF_EMPTY_VALUE_ANCHOR, &filled)
}

/// The URL a form carrying `action:NAME` posts to, or `None` when `name`
/// cannot be a URL path segment.
///
/// ## Why `None` rather than an escaped name
///
/// Percent-encoding the segment would work and would be wrong: the name also has
/// to survive being read back out of a route match, so an encoded form means two
/// spellings of the same action and a decode step that can disagree with the
/// encode step. Refusing is cheaper and has no failure mode.
///
/// The alphabet is the identifier characters every real action name already uses
/// (`sign_guestbook`, `submit-login`). A name outside it keeps exactly the
/// behaviour it had before this function existed — [`FORM_ACTION_ATTR`] and a
/// JS-only submit.
///
/// 🪤 **`.` is not in the alphabet, and that is the point.** It is a perfectly
/// reasonable character in an action name (`todos.add`), and admitting it also
/// admits `..` — a path segment with a meaning the router assigns rather than we
/// do. The rule that has no traversal question is worth more than the naming
/// style it costs.
///
/// 🔲 **This should be a build error, not a silent narrowing.** An action name
/// with a space in it produces a form that works with JavaScript and silently
/// does nothing without it, which is precisely the class of difference this whole
/// change exists to remove. Making it one needs a fallible channel out of
/// [`extract_forms_in_function`], which today returns a plain `Vec`.
#[must_use]
pub fn action_endpoint(action_name: &str) -> Option<String> {
    if !is_url_safe_action_name(action_name) {
        return None;
    }
    Some(format!("{ACTION_ENDPOINT_PREFIX}{action_name}"))
}

/// Whether an action name can appear verbatim as a URL path segment.
#[must_use]
pub fn is_url_safe_action_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Post-render return-path fill, the sibling of [`fill_csrf_tokens`].
///
/// Stamps the request's own path into every [`RETURN_PLACEHOLDER_INPUT`] on the
/// page so a browser submit lands back where it started. Runs in the same pass as
/// the CSRF fill, because a form that got one and not the other is a form that
/// works and then redirects to `/`.
///
/// `path` is HTML-escaped: it comes from the request line and routinely carries
/// `&` (query strings do), which unescaped would truncate the attribute at the
/// first parameter — and a `"` would end it entirely.
#[must_use]
pub fn fill_return_paths(html: &str, path: &str) -> String {
    if !html.contains(RETURN_EMPTY_VALUE_ANCHOR) {
        return html.to_string();
    }
    let filled = format!(
        "value=\"{}\" {RETURN_MARKER_ATTR}",
        escape_attribute_value(path)
    );
    html.replace(RETURN_EMPTY_VALUE_ANCHOR, &filled)
}

/// Minimal HTML attribute-value escape for a double-quoted attribute.
///
/// Four characters, in the order that matters: `&` first, or the ampersands
/// introduced by the other three get escaped a second time.
fn escape_attribute_value(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
    /// True when this form is reached by descending through a list
    /// `.map(...)`/`.flatMap(...)` callback — i.e. it is rendered once per row.
    ///
    /// Its per-field `data-albedo-error` ids are
    /// [`allocate_field_error_id`]`(action, field)`, a constant per
    /// `(action, field)` pair, so a repeated form stamps the **same**
    /// `data-albedo-id` on every row — a duplicate-id violation that misroutes a
    /// validation `SetText` (and, seeded, breaks keyed reconciliation). The
    /// compiled manifest therefore suppresses the error-span seed *and* the
    /// submit projection for any action that is ever `in_list`; inline per-field
    /// validation is simply not offered for a form that repeats. See
    /// `runtime::compiled`'s `list_repeated_actions`.
    pub in_list: bool,
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
        visit_stmt_for_jsx(stmt, &mut sink, false);
    }
    sink
}

/// Statement-level recursion entry point. Structurally identical to
/// the other Phase-K/L extractors so the three can be fused into one
/// walker later.
///
/// `in_list` is `true` once the walk has descended through a list
/// `.map(...)` callback; every form found from there is `in_list`.
fn visit_stmt_for_jsx(stmt: &Stmt, sink: &mut Vec<FormExtract>, in_list: bool) {
    match stmt {
        Stmt::Return(ret) => {
            if let Some(arg) = &ret.arg {
                visit_expr_for_jsx(arg, sink, in_list);
            }
        }
        Stmt::Expr(es) => visit_expr_for_jsx(&es.expr, sink, in_list),
        Stmt::Block(block) => {
            for s in &block.stmts {
                visit_stmt_for_jsx(s, sink, in_list);
            }
        }
        Stmt::Decl(Decl::Var(var)) => {
            for d in &var.decls {
                if let Some(init) = &d.init {
                    visit_expr_for_jsx(init, sink, in_list);
                }
            }
        }
        _ => {}
    }
}

/// Expression-level walker; descends into the subset of expressions
/// the Phase J renderer also descends into.
fn visit_expr_for_jsx(expr: &Expr, sink: &mut Vec<FormExtract>, in_list: bool) {
    match expr {
        Expr::JSXElement(element) => visit_element(element, sink, in_list),
        Expr::JSXFragment(fragment) => {
            for child in &fragment.children {
                visit_child(child, sink, in_list);
            }
        }
        Expr::Paren(paren) => visit_expr_for_jsx(&paren.expr, sink, in_list),
        Expr::Cond(c) => {
            visit_expr_for_jsx(&c.cons, sink, in_list);
            visit_expr_for_jsx(&c.alt, sink, in_list);
        }
        Expr::Arrow(arrow) => match &*arrow.body {
            BlockStmtOrExpr::Expr(e) => visit_expr_for_jsx(e, sink, in_list),
            BlockStmtOrExpr::BlockStmt(b) => {
                for s in &b.stmts {
                    visit_stmt_for_jsx(s, sink, in_list);
                }
            }
        },
        // A list `.map(cb)` / `.flatMap(cb)` renders `cb` once per row, so any
        // form inside the callback repeats. Descend into the callback arguments
        // with `in_list = true`; the JSX inside is caught by the arms above. This
        // is also the only place `.map()`-nested forms become visible to the
        // extractor at all — without it they were absent from `FormActionIds`
        // (so a tokenless submit was mis-phrased "action" not "form action") as
        // well as from the error-span manifest.
        Expr::Call(call) => {
            if is_list_map_call(call) {
                for arg in &call.args {
                    visit_expr_for_jsx(&arg.expr, sink, true);
                }
            }
        }
        _ => {}
    }
}

/// True for `obj.map(...)` / `obj.flatMap(...)` — the member calls that render a
/// JSX element per item. Keyed on the method name only (not the receiver), so a
/// chained `items.filter(p).map(row => …)` is still recognised.
fn is_list_map_call(call: &swc_ecma_ast::CallExpr) -> bool {
    use swc_ecma_ast::{Callee, MemberProp};
    let Callee::Expr(callee) = &call.callee else {
        return false;
    };
    let Expr::Member(member) = callee.as_ref() else {
        return false;
    };
    let MemberProp::Ident(method) = &member.prop else {
        return false;
    };
    matches!(method.sym.as_ref(), "map" | "flatMap")
}

/// Visit one element. If it's a `<form>` with an `action:NAME`
/// attribute, surface the form metadata and walk children to collect
/// the declared input fields. Non-form elements still recurse into
/// their children so nested form-actions (legal but unusual) are
/// caught.
fn visit_element(element: &JSXElement, sink: &mut Vec<FormExtract>, in_list: bool) {
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
                in_list,
            });
            return;
        }
    }

    for child in &element.children {
        visit_child(child, sink, in_list);
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
fn visit_child(child: &JSXElementChild, sink: &mut Vec<FormExtract>, in_list: bool) {
    match child {
        JSXElementChild::JSXElement(element) => visit_element(element, sink, in_list),
        JSXElementChild::JSXFragment(fragment) => {
            for c in &fragment.children {
                visit_child(c, sink, in_list);
            }
        }
        JSXElementChild::JSXExprContainer(container) => {
            if let JSXExpr::Expr(expr) = &container.expr {
                visit_expr_for_jsx(expr, sink, in_list);
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

        // Same invariant for the return-path input, which arrived later and
        // would otherwise have been the one nobody checked.
        assert!(RETURN_PLACEHOLDER_INPUT.contains(RETURN_EMPTY_VALUE_ANCHOR));
        assert!(RETURN_PLACEHOLDER_INPUT.contains(&format!("name=\"{RETURN_FIELD_NAME}\"")));
        assert!(RETURN_PLACEHOLDER_INPUT.contains(RETURN_MARKER_ATTR));
    }

    /// The rule that makes a sign-in form authorable: a same-origin POST form
    /// earns the hidden inputs whether or not it carries an `action:` sentinel.
    #[test]
    fn a_same_origin_post_form_earns_the_hidden_inputs() {
        assert!(plain_form_needs_hidden_inputs(
            Some("/_albedo/auth/password/login"),
            Some("POST")
        ));
        // Case and surrounding space are the author's, not the rule's.
        assert!(plain_form_needs_hidden_inputs(Some("/submit"), Some("post")));
        assert!(plain_form_needs_hidden_inputs(Some(" /submit "), Some(" Post ")));
        // No action at all resolves to the current URL, which is same-origin.
        assert!(plain_form_needs_hidden_inputs(None, Some("post")));
        assert!(plain_form_needs_hidden_inputs(Some(""), Some("post")));
    }

    /// 🔑 The half that is a *disclosure* rule rather than a forgery one. A
    /// token emitted into a form that posts off-origin is handed to that origin
    /// on submit, so the mechanism meant to stop CSRF would be leaking the
    /// secret it depends on. A GET form leaks it a different way — into the URL,
    /// the history and the access log.
    #[test]
    fn a_token_is_never_emitted_where_it_would_leak() {
        // Off-origin, in each spelling that is not a rooted path.
        for action in [
            "https://evil.example/collect",
            "http://evil.example/collect",
            "//evil.example/collect",
            "javascript:void(0)",
            "relative/path",
        ] {
            assert!(
                !plain_form_needs_hidden_inputs(Some(action), Some("post")),
                "{action} must not receive a CSRF token",
            );
        }
        // GET, however same-origin.
        assert!(!plain_form_needs_hidden_inputs(Some("/search"), Some("get")));
        assert!(!plain_form_needs_hidden_inputs(Some("/search"), None));
    }

    /// [`FORM_HIDDEN_INPUTS`] restates the placeholder literals because stable
    /// Rust cannot concatenate `const`s. This is the check that makes the
    /// restatement safe: edit any placeholder without editing the fused constant
    /// and the build stops here rather than shipping a form whose hidden inputs
    /// no fill can see.
    ///
    /// ✅ It did exactly that when APERTURE A3 added `_albedo_intent` — which is
    /// the whole reason to keep a test whose only job is to restate a `concat!`.
    #[test]
    fn the_fused_prefix_is_exactly_its_parts() {
        assert_eq!(
            FORM_HIDDEN_INPUTS,
            format!(
                "{CSRF_PLACEHOLDER_INPUT}{RETURN_PLACEHOLDER_INPUT}{INTENT_PLACEHOLDER_INPUT}"
            )
        );
    }

    /// The fills run over the same HTML in the same pass, so none may match
    /// another's anchor. They differ only in their marker attribute, which is
    /// exactly the kind of near-collision worth pinning — and the reason the
    /// final assertion is *"nothing stayed empty"* rather than three positive
    /// checks: a fill that quietly matched a sibling's anchor would satisfy its
    /// own check and leave the sibling blank.
    #[test]
    fn the_placeholder_fills_do_not_match_each_others_anchors() {
        let all = format!(
            "<form>{CSRF_PLACEHOLDER_INPUT}{RETURN_PLACEHOLDER_INPUT}{INTENT_PLACEHOLDER_INPUT}</form>"
        );
        let filled = fill_intent_tokens(
            &fill_return_paths(&fill_csrf_tokens(&all, "tok"), "/mine"),
            "i-1",
        );
        assert!(filled.contains(&format!("name=\"{CSRF_FIELD_NAME}\" value=\"tok\"")));
        assert!(filled.contains(&format!("name=\"{RETURN_FIELD_NAME}\" value=\"/mine\"")));
        assert!(filled.contains(&format!("name=\"{INTENT_FIELD_NAME}\" value=\"i-1\"")));
        assert!(!filled.contains("value=\"\""), "nothing may stay empty: {filled}");
    }

    /// A query string is the common case and it is full of ampersands. Left
    /// unescaped the attribute truncates at the first one, so the submit returns
    /// to a path missing half its parameters — a bug that only shows up on pages
    /// with more than one query parameter.
    #[test]
    fn a_query_string_survives_the_fill_intact() {
        let filled = fill_return_paths(RETURN_PLACEHOLDER_INPUT, "/search?q=a&page=2");
        assert!(
            filled.contains("value=\"/search?q=a&amp;page=2\""),
            "{filled}"
        );
    }

    /// A path that could close the attribute must not be able to.
    #[test]
    fn a_quote_in_the_path_cannot_escape_the_attribute() {
        let filled = fill_return_paths(RETURN_PLACEHOLDER_INPUT, "/x\"><script>alert(1)</script>");
        assert!(!filled.contains("<script>"), "{filled}");
        assert!(filled.contains("&quot;"), "{filled}");
    }

    #[test]
    fn the_return_fill_is_a_noop_without_a_placeholder() {
        let plain = "<div>no forms here</div>";
        assert_eq!(fill_return_paths(plain, "/mine"), plain);
    }

    /// The endpoint is what makes a submit work with no JavaScript at all, so
    /// its shape is a contract and not an implementation detail.
    #[test]
    fn the_action_endpoint_is_the_prefix_plus_the_name() {
        assert_eq!(
            action_endpoint("sign_guestbook").as_deref(),
            Some("/_albedo/action/sign_guestbook")
        );
        assert_eq!(
            action_endpoint("submit-login").as_deref(),
            Some("/_albedo/action/submit-login")
        );
    }

    /// A name that cannot be a path segment gets no URL rather than a broken
    /// one. Path traversal is in here deliberately: `..` must never become a
    /// route.
    #[test]
    fn an_unusable_action_name_yields_no_endpoint() {
        for name in [
            "sign guestbook",
            "a/b",
            "..",
            ".",
            "todos.add",
            "a?b",
            "a#b",
            "a%2Fb",
            "",
        ] {
            assert_eq!(action_endpoint(name), None, "`{name}` must not build a URL");
        }
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

    #[test]
    fn a_top_level_form_is_not_in_list() {
        let stmts = parse_body(
            r#"
            function S() {
                return <form action="action:save"><input name="title" /></form>;
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 1);
        assert!(!forms[0].in_list, "a form outside any .map() must not be in_list");
    }

    /// The form-in-list-row case. Before this, `.map()` was not descended at
    /// all, so a per-row form was invisible to the extractor — absent from
    /// `FormActionIds` (mis-phrasing a tokenless submit) and from the error-span
    /// manifest. Now it is surfaced AND flagged `in_list` so its unaddressable
    /// per-row error spans are suppressed downstream.
    #[test]
    fn a_form_inside_a_map_callback_is_surfaced_and_flagged_in_list() {
        let stmts = parse_body(
            r#"
            function List() {
                return (
                    <ul>
                        {rows.map((row) => (
                            <li key={row.id}>
                                <form action="action:set_score">
                                    <input name="id" type="hidden" />
                                    <input name="score" />
                                </form>
                            </li>
                        ))}
                    </ul>
                );
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 1, "the .map()-nested form must be surfaced");
        assert_eq!(forms[0].action_name, "set_score");
        assert!(forms[0].in_list, "a form inside .map() must be in_list");
        let names: Vec<_> = forms[0].fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["id", "score"], "its fields are still collected");
    }

    /// A chained `items.filter(p).map(cb)` still counts — the check keys on the
    /// method name, not the receiver.
    #[test]
    fn a_chained_filter_map_is_still_in_list() {
        let stmts = parse_body(
            r#"
            function L() {
                return <ul>{xs.filter(x => x.on).map(x => <form action="action:go"><input name="id"/></form>)}</ul>;
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 1);
        assert!(forms[0].in_list);
    }

    /// Both instances of one action surface; the top-level is not `in_list`, the
    /// per-row one is. `CompiledProject` collapses this to "the action is
    /// list-repeated" and suppresses spans for both.
    #[test]
    fn an_action_used_top_level_and_in_a_row_surfaces_both() {
        let stmts = parse_body(
            r#"
            function Board() {
                return (
                    <div>
                        <ul>{rows.map(r => <li key={r.id}><form action="action:edit"><input name="v"/></form></li>)}</ul>
                        <form action="action:edit"><input name="v" /></form>
                    </div>
                );
            }
        "#,
        );
        let forms = extract_forms_in_function(&stmts);
        assert_eq!(forms.len(), 2);
        let in_list: Vec<bool> = forms.iter().map(|f| f.in_list).collect();
        assert!(
            in_list.contains(&true) && in_list.contains(&false),
            "one instance is in a row, one is top-level: {in_list:?}"
        );
    }
}
