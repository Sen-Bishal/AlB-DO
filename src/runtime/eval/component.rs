use serde_json::Value;
use std::path::Path;

pub fn is_component_module(path: &Path) -> bool {
    // Phase P · post-P wire-through — skip ambient TypeScript
    // declaration files (`*.d.ts`, `*.d.tsx`). They carry
    // `declare function` shapes with no body that the SWC parse
    // path would reject as "missing function body", and they're
    // declaration-only anyway — no runtime content for the
    // renderer to walk.
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".d.ts") || name.ends_with(".d.tsx") {
        return false;
    }
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("jsx" | "tsx" | "js" | "ts")
    )
}

pub fn fnv1a_hash(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// FNV-1a-32: matches `stable_id_for_placeholder` in albedo-server's
/// `render::tier_b`. The compiler crate can't depend on the server crate,
/// so this is the source of truth for shell-stamped `data-albedo-id`s
/// emitted by the static evaluator. Both functions must produce the same
/// bytes for the same input — anchor IDs cross the WT boundary as u32s.
pub fn fnv1a_32(data: &[u8]) -> u32 {
    const FNV_OFFSET: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET;
    for byte in data {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

pub fn normalize_specifier(path: impl AsRef<Path>) -> String {
    let mut parts = Vec::new();
    for component in path.as_ref().components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            std::path::Component::Normal(segment) => {
                parts.push(segment.to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    normalize_slashes(&parts.join("/"))
}

pub fn normalize_slashes(value: &str) -> String {
    value.replace('\\', "/")
}

/// Extensions a specifier may already carry and be considered fully resolved.
/// Mirrors `is_component_module` — a module is only ever keyed under one of
/// these, so anything else is not an extension as far as resolution goes.
const MODULE_EXTENSIONS: [&str; 4] = ["jsx", "tsx", "js", "ts"];

pub fn import_candidates(base: &str) -> Vec<String> {
    let mut out = Vec::new();
    // The test is "does it END IN a module extension", not "does
    // `Path::extension()` return anything". A dot in the FILENAME is not an
    // extension: `./WelcomeScreen.Center` yields `Some("Center")`, and reading
    // that as "already resolved" made every dotted component name
    // (`Foo.Bar.tsx`, `Button.styles.ts`) unimportable — the module is keyed
    // under `WelcomeScreen.Center.tsx`, a candidate we then never tried.
    let extension = std::path::Path::new(base)
        .extension()
        .and_then(|ext| ext.to_str());
    if matches!(extension, Some(ext) if MODULE_EXTENSIONS.contains(&ext)) {
        out.push(base.to_string());
        return out;
    }
    // A non-module extension (`./data.json`, `./theme.css`) still resolves to
    // itself first if the map holds it verbatim, then falls through to the
    // dotted-filename reading below.
    if extension.is_some() {
        out.push(base.to_string());
    }
    for ext in MODULE_EXTENSIONS {
        out.push(format!("{base}.{ext}"));
    }
    for ext in MODULE_EXTENSIONS {
        out.push(format!("{base}/index.{ext}"));
    }
    out
}

pub fn normalize_jsx_text(value: &str) -> Option<String> {
    // React JSX whitespace rules (paraphrased):
    //   * Interior runs of whitespace collapse to a single space.
    //   * A leading/trailing whitespace run is REMOVED when it
    //     contains a newline (source indentation adjacent to a tag)
    //     but PRESERVED as a single space when it does not — so
    //     `Built at the <span>speed</span>` keeps the space before
    //     `<span>` even when the text node opens with a newline+indent,
    //     while `\n  x \n` (indented on its own line) collapses to `x`.
    //   * A pure-whitespace node collapses to nothing if it spans a
    //     newline, but to a single significant space otherwise (the
    //     space in `{a} {b}` on one line).
    let inner = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if inner.is_empty() {
        if value.is_empty() || value.contains('\n') {
            return None;
        }
        return Some(" ".to_string());
    }
    // Inspect only the boundary whitespace runs, not the whole string,
    // so a newline buried in interior indentation can't strip a
    // same-line space adjacent to an inline element.
    let leading_has_newline = value
        .chars()
        .take_while(|c| c.is_whitespace())
        .any(|c| c == '\n');
    let trailing_has_newline = value
        .chars()
        .rev()
        .take_while(|c| c.is_whitespace())
        .any(|c| c == '\n');
    let mut result = String::new();
    if value.starts_with(|c: char| c.is_whitespace()) && !leading_has_newline {
        result.push(' ');
    }
    result.push_str(&inner);
    if value.ends_with(|c: char| c.is_whitespace()) && !trailing_has_newline {
        result.push(' ');
    }
    Some(result)
}

pub fn is_component_tag(tag: &str) -> bool {
    tag.chars()
        .next()
        .map(|c| c.is_ascii_uppercase())
        .unwrap_or(false)
}

/// The key that tags an evaluator value as ALREADY-RENDERED MARKUP.
///
/// The evaluator's `Value` is `serde_json::Value` — a foreign type with no room
/// for a new variant — so "this string is HTML, not text" rides the same way a
/// `Date` does: a one-key object. QuickJS draws the same distinction with the
/// `AlbedoHtml` wrapper in `quickjs_engine`'s `h()` shim, and the two renderers
/// must agree on *which* children get escaped or they emit different bytes for
/// the same component.
///
/// ## Why the key carries a per-process nonce
///
/// `AlbedoHtml` is unforgeable in JS: a value decoded from JSON is never
/// `instanceof AlbedoHtml`. A fixed key like `__albedo_html__` would NOT be
/// unforgeable here — props, FORGE rows and fetched JSON are all
/// attacker-influenced `Value`s, and any one of them could carry that key and
/// so be spliced into the page as raw markup. That would replace the escaping
/// hole this marker exists to close with a narrower one, which is not a fix.
/// A nonce chosen at process start cannot be written by anything that entered
/// as data, so the marker means what it says: *this renderer produced it*.
///
/// It never reaches output — [`value_to_string`] unwraps it, and
/// [`unwrap_html_markers`] is applied at the two boundaries that serialise a
/// `Value` back out (island props, `JSON.stringify`) — so the nondeterminism
/// stays inside one process's memory and out of every artifact.
#[must_use]
pub fn albedo_html_tag() -> &'static str {
    static TAG: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TAG.get_or_init(|| format!("__albedo_html_{:016x}__", rand::random::<u64>()))
}

/// Tag `html` as markup that is already safe to embed verbatim.
#[must_use]
pub fn make_html_value(html: String) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(albedo_html_tag().to_string(), Value::String(html));
    Value::Object(map)
}

/// The markup inside an already-rendered value, or `None` for plain data.
///
/// Plain data is everything a component author can put in an expression child
/// that is not the output of a render: strings, numbers, dates, objects, and
/// anything that arrived as props. All of it must be escaped.
#[must_use]
pub fn as_rendered_html(value: &Value) -> Option<&str> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    object.get(albedo_html_tag())?.as_str()
}

/// Replace every markup marker in `value` with its bare string.
///
/// For the boundaries that serialise a `Value` back out of the renderer, where
/// the marker's shape would otherwise leak into a payload. Restores exactly what
/// those boundaries saw before the marker existed: a `String` of HTML.
#[must_use]
pub fn unwrap_html_markers(value: &Value) -> Value {
    if let Some(html) = as_rendered_html(value) {
        return Value::String(html.to_string());
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(unwrap_html_markers).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), unwrap_html_markers(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Append one JSX expression child to `out`, escaping it unless it is markup
/// this renderer produced.
///
/// The Rust half of QuickJS's `__albedo_push_children`, and it must stay the
/// same rule: arrays flatten, `null`/`false` render nothing, an already-rendered
/// value passes through verbatim, and **everything else is escaped**. Anything
/// that reached here as data — props, a route param, a FORGE column, a fetched
/// field — is in that last case.
pub fn push_expression_child(value: &Value, out: &mut String) {
    if let Some(html) = as_rendered_html(value) {
        out.push_str(html);
        return;
    }
    match value {
        // `{null}`, `{undefined}` and `{cond && ...}` with a false `cond`
        // render nothing. `true` is deliberately not in this set: JS prints it.
        Value::Null | Value::Bool(false) => {}
        // A mapped list is an array of children, each with its own nature —
        // `xs.map(x => <li/>)` is markup, `xs.map(x => x.name)` is text — so the
        // decision is made per element rather than for the array.
        Value::Array(items) => {
            for item in items {
                push_expression_child(item, out);
            }
        }
        other => out.push_str(&escape_html(&value_to_string(other))),
    }
}

pub fn escape_attr(value: &str) -> String {
    escape_html(value).replace('"', "&quot;")
}

/// JSX props that are framework-level and must never reach the HTML.
///
/// * `key` — React's reconciliation identity. It addresses an element within a
///   sibling list; it is not data about the element, and `key` is not a valid
///   HTML attribute. React strips it rather than emitting it, and so must we.
/// * `ref` — an escape hatch to the host node. Also not an HTML attribute.
/// * `children` — the element's content, rendered between the tags.
///
/// This is the single Rust-side definition; every HTML-emitting path consults it
/// (`render_attrs` here, the list templater in `runtime::compiled`). The QuickJS
/// shim carries its own copy because it lives on the other side of a language
/// boundary — `runtime::quickjs_engine`'s `__albedo_is_reserved_prop` must be
/// kept in lockstep with this list, or SSR and CSR emit different attributes for
/// the same component.
#[must_use]
pub fn is_reserved_jsx_prop(name: &str) -> bool {
    matches!(name, "key" | "ref" | "children")
}

pub fn render_attrs(attrs: &[(String, Value)]) -> String {
    let mut out = Vec::new();
    for (name, value) in attrs {
        if name.starts_with("on") {
            continue;
        }
        if name == "key" {
            // React's `key` is not a raw HTML attribute, but it IS the delta
            // sink's reconciliation identity — stamp it as `data-albedo-key` so a
            // keyed list's server-rendered rows can be key-reconciled by the
            // client. This is the single SSR stamp point (both the Tier-C local
            // lane and the Tier-B/broadcast lane render host elements through
            // here); the QuickJS `h` shim mirrors it. `ref`/`children` still carry
            // no identity and stay stripped below.
            let text = value_to_string(value);
            if !text.is_empty() {
                out.push(format!("data-albedo-key=\"{}\"", escape_attr(&text)));
            }
            continue;
        }
        if is_reserved_jsx_prop(name) {
            continue;
        }
        // JSX prop → HTML attribute rename, from the ONE table every renderer
        // reads (`runtime::jsx_attributes`). It used to be a `match` here, a
        // ternary chain in the QuickJS `h` shim, and a third pair of `if`s in
        // `albedo-client.js` — three independent implementations of a rule that
        // hydration requires to agree byte-for-byte. The SVG half was missing
        // from all three, which is why `<svg strokeWidth="2">` has been shipping
        // the browser-inert `strokewidth` since Tier A existed.
        let attr_name = crate::runtime::jsx_attributes::jsx_attribute_name(name.as_str());
        // `style` takes an object in JSX and CSS text in HTML. Without this the
        // object fell through to `value_to_string`, which JSON-encodes it, and a
        // `<div style={{height:"1px"}}>` shipped a `style` attribute holding
        // `{"height":"1px"}` — not a style the browser applies, and not what the
        // QuickJS shim produced from the same source either.
        if attr_name == "style" {
            if let Value::Object(map) = value {
                let css = style_object_to_css(map.iter().map(|(k, v)| (k.as_str(), v)));
                if !css.is_empty() {
                    out.push(format!("style=\"{}\"", escape_attr(&css)));
                }
                continue;
            }
        }
        match value {
            Value::Null => {}
            Value::Bool(false) => {}
            Value::Bool(true) => out.push(attr_name.to_string()),
            _ => {
                let text = value_to_string(value);
                if !text.is_empty() {
                    out.push(format!("{attr_name}=\"{}\"", escape_attr(&text)));
                }
            }
        }
    }
    out.join(" ")
}

/// The HTML void elements — the tags that take no closing tag.
///
/// This is the single spelling of the set. The pure-Rust renderer reads it
/// through [`is_void_tag`], the list templater in `runtime::compiled` reads it
/// directly, and the QuickJS `h()` shim receives it as *data* on
/// `__ALBEDO_MARKUP_CONTRACT` rather than restating it in JS. Three copies of
/// this list is three chances for one renderer to close a tag the other leaves
/// open — which is precisely the drift the conformance harness exists to catch.
pub const HTML_VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

pub fn is_void_tag(tag: &str) -> bool {
    HTML_VOID_ELEMENTS.contains(&tag)
}

/// CSS properties React leaves unitless when handed a bare number.
///
/// Lifted from React's `isUnitlessNumber`. Every property *not* here gets `px`
/// appended to a non-zero numeric value, which is the rule that turns
/// `style={{ width: 10 }}` into `width:10px` but leaves `style={{ flex: 1 }}`
/// as `flex:1`.
///
/// Base spellings only — [`css_unitless_properties`] derives the vendor-prefixed
/// variants the way React does, so `WebkitLineClamp` is covered without being
/// written out.
pub const CSS_UNITLESS_BASE_PROPERTIES: &[&str] = &[
    "animationIterationCount",
    "aspectRatio",
    "borderImageOutset",
    "borderImageSlice",
    "borderImageWidth",
    "boxFlex",
    "boxFlexGroup",
    "boxOrdinalGroup",
    "columnCount",
    "columns",
    "flex",
    "flexGrow",
    "flexPositive",
    "flexShrink",
    "flexNegative",
    "flexOrder",
    "gridArea",
    "gridRow",
    "gridRowEnd",
    "gridRowSpan",
    "gridRowStart",
    "gridColumn",
    "gridColumnEnd",
    "gridColumnSpan",
    "gridColumnStart",
    "fontWeight",
    "lineClamp",
    "lineHeight",
    "opacity",
    "order",
    "orphans",
    "tabSize",
    "widows",
    "zIndex",
    "zoom",
    // SVG-related, unitless for the same reason.
    "fillOpacity",
    "floodOpacity",
    "stopOpacity",
    "strokeDasharray",
    "strokeDashoffset",
    "strokeMiterlimit",
    "strokeOpacity",
    "strokeWidth",
];

/// Vendor prefixes React generates a unitless variant for.
const CSS_VENDOR_PREFIXES: &[&str] = &["Webkit", "ms", "Moz", "O"];

/// The full unitless set — base names plus their vendor-prefixed spellings.
///
/// This is the single spelling of the set, in exactly the sense
/// [`HTML_VOID_ELEMENTS`] is one: the pure-Rust renderer reads it through
/// [`is_unitless_style_property`], and the QuickJS `h()` shim receives it as
/// *data* on `__ALBEDO_MARKUP_CONTRACT` rather than restating it in JS. Two
/// copies would be two chances for one renderer to emit `flex:1` where the
/// other emits `flex:1px` — a divergence a browser would render differently,
/// not merely spell differently.
pub fn css_unitless_properties() -> &'static std::collections::HashSet<String> {
    static SET: std::sync::OnceLock<std::collections::HashSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let mut set = std::collections::HashSet::new();
        for base in CSS_UNITLESS_BASE_PROPERTIES {
            set.insert((*base).to_string());
            let mut capitalized = base.chars();
            let capitalized = match capitalized.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + capitalized.as_str(),
                None => continue,
            };
            for prefix in CSS_VENDOR_PREFIXES {
                set.insert(format!("{prefix}{capitalized}"));
            }
        }
        set
    })
}

pub fn is_unitless_style_property(property: &str) -> bool {
    css_unitless_properties().contains(property)
}

/// React's `hyphenateStyleName`: `backgroundColor` → `background-color`.
///
/// A leading uppercase letter yields a leading dash, which is what makes
/// `WebkitTransform` come out as `-webkit-transform`. The Microsoft spelling is
/// the one exception React special-cases — `msTransform` hyphenates to
/// `ms-transform`, but CSS wants `-ms-transform`.
pub fn hyphenate_style_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for ch in name.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    match out.strip_prefix("ms-") {
        Some(rest) => format!("-ms-{rest}"),
        None => out,
    }
}

/// One `style` declaration's value, or `None` when React would omit the
/// declaration entirely.
///
/// `null`, booleans and the empty string are dropped rather than emitted as an
/// empty declaration. A non-zero number on a property that takes a length gets
/// `px`; zero never does, matching React's `value !== 0` guard.
pub fn style_value_to_css(property: &str, value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bool(_) => None,
        Value::Number(number) => {
            let text = format_number_for_output(number);
            if text == "0" || property.starts_with("--") || is_unitless_style_property(property) {
                Some(text)
            } else {
                Some(format!("{text}px"))
            }
        }
        other => {
            let text = value_to_string(other);
            let trimmed = text.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
    }
}

/// A JSX `style` object lowered to CSS text, React's way.
///
/// Declarations are emitted in the order the iterator yields them, joined with
/// `;` and no trailing separator. Custom properties (`--brand`) keep their name
/// verbatim; everything else is hyphenated.
///
/// **Order is the caller's responsibility and it matters.** CSS is
/// order-sensitive — `{ margin: 0, marginTop: 4 }` and its reverse mean
/// different things — so a caller holding the authored order must preserve it.
/// See `read_attrs`, which lowers object *literals* straight from the AST for
/// exactly this reason.
pub fn style_object_to_css<'a>(entries: impl Iterator<Item = (&'a str, &'a Value)>) -> String {
    let mut out = String::new();
    for (name, value) in entries {
        let Some(rendered) = style_value_to_css(name, value) else {
            continue;
        };
        let property = if name.starts_with("--") {
            name.to_string()
        } else {
            hyphenate_style_name(name)
        };
        if !out.is_empty() {
            out.push(';');
        }
        out.push_str(&property);
        out.push(':');
        out.push_str(&rendered);
    }
    out
}

pub fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

pub fn classnames_collect(val: &Value, out: &mut Vec<String>) {
    match val {
        Value::String(s) if !s.is_empty() => {
            out.push(s.clone());
        }
        Value::Array(arr) => {
            for item in arr {
                classnames_collect(item, out);
            }
        }
        Value::Object(map) => {
            for (key, flag) in map {
                if is_truthy(flag) {
                    out.push(key.clone());
                }
            }
        }
        _ => {}
    }
}

pub fn is_classnames_source(source: &str) -> bool {
    matches!(source, "classnames" | "clsx")
        || source.ends_with("/classnames")
        || source.ends_with("/clsx")
}

pub fn lit_to_value(lit: &swc_ecma_ast::Lit) -> Value {
    match lit {
        swc_ecma_ast::Lit::Str(str_lit) => Value::String(str_lit.value.to_string()),
        swc_ecma_ast::Lit::Bool(bool_lit) => Value::Bool(bool_lit.value),
        swc_ecma_ast::Lit::Num(num) => serde_json::Number::from_f64(num.value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        swc_ecma_ast::Lit::Null(_) => Value::Null,
        _ => Value::Null,
    }
}

pub fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => format_number_for_output(number),
        Value::String(string) => string.clone(),
        Value::Array(values) => values.iter().map(value_to_string).collect(),
        Value::Object(object) => {
            // Already-rendered markup prints as itself. `value_to_string` is the
            // one funnel every text-ish consumer goes through, so unwrapping
            // here keeps the marker's shape from ever reaching a page.
            if let Some(html) = as_rendered_html(value) {
                return html.to_string();
            }
            // Date objects (encoded as { __albedo_date__: ms }) print as the
            // ISO string, mirroring JS's `String(new Date())` shape closely
            // enough for templates that interpolate them directly. Anything
            // else falls through to JSON for visibility.
            if let Some(ms) = object
                .get("__albedo_date__")
                .and_then(|v| v.as_f64())
            {
                return format_date_iso(ms);
            }
            serde_json::to_string(object).unwrap_or_default()
        }
    }
}

/// Format a JSON number the way JS's `String(n)` does: integers without a
/// trailing `.0`, floats with the standard ECMAScript-ish representation.
/// `serde_json::Number::from_f64(42.0).to_string()` yields "42.0", which
/// silently drifts from JS semantics — fix it once at the print site.
pub fn format_number_for_output(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    if let Some(f) = n.as_f64() {
        if f.is_finite() && f == f.trunc() && f.abs() < 1e16 {
            return format!("{}", f as i64);
        }
        return n.to_string();
    }
    n.to_string()
}

/// Encode a Date instance as a tagged JSON object so it survives through
/// the evaluator's `Value` substrate without needing a parallel type.
pub fn make_date_value(ms: f64) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(
        "__albedo_date__".to_string(),
        serde_json::Number::from_f64(ms)
            .map(Value::Number)
            .unwrap_or(Value::Null),
    );
    Value::Object(map)
}

pub fn date_value_ms(value: &Value) -> Option<f64> {
    value
        .as_object()
        .and_then(|m| m.get("__albedo_date__"))
        .and_then(|v| v.as_f64())
}

/// Coerce a runtime `Value` to an f64 the way JS's arithmetic operators
/// would. NaN-on-failure is left as 0.0 because the static evaluator
/// never surfaces NaN to HTML — Phase K's reactive path can take over.
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(true) => 1.0,
        Value::Bool(false) => 0.0,
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        Value::Array(_) | Value::Object(_) => 0.0,
    }
}

pub fn json_num(value: f64) -> Value {
    // Prefer the integer form when the value is exactly representable
    // as i64. `serde_json::Number::from_f64(1.0)` would encode as
    // `"1.0"` over the wire — which lands on the client as the literal
    // text "1.0" in a counter span, instead of the expected "1". This
    // matches JS's `String(1)` → "1" semantics rather than the f64
    // round-tripping a serde would otherwise impose.
    if value.is_finite() && value == value.trunc() && value.abs() < 1e16 {
        return Value::Number(serde_json::Number::from(value as i64));
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn json_int(value: i64) -> Value {
    Value::Number(serde_json::Number::from(value))
}

pub fn arg_num(args: &[Value], index: usize) -> f64 {
    args.get(index).map(to_number).unwrap_or(0.0)
}

fn format_date_iso(ms: f64) -> String {
    let total_ms = ms as i64;
    let mut secs = total_ms.div_euclid(1000);
    let mut millis = total_ms.rem_euclid(1000) as u32;
    if millis >= 1000 {
        secs += 1;
        millis -= 1000;
    }
    let (y, mo, d, h, mi, s) = epoch_seconds_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, millis
    )
}

fn epoch_seconds_to_ymd_hms(mut secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let day_secs: i64 = 86_400;
    let mut days = secs.div_euclid(day_secs);
    secs = secs.rem_euclid(day_secs);
    let hour = (secs / 3600) as u32;
    let minute = ((secs % 3600) / 60) as u32;
    let second = (secs % 60) as u32;

    // Civil-from-days (Howard Hinnant), works for the full proleptic Gregorian
    // range including pre-1970 negative inputs.
    days += 719_468;
    let era = if days >= 0 { days / 146_097 } else { (days - 146_096) / 146_097 };
    let doe = (days - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hour, minute, second)
}

pub fn prop_name_to_string(name: &swc_ecma_ast::PropName) -> Option<String> {
    match name {
        swc_ecma_ast::PropName::Ident(ident) => Some(ident.sym.to_string()),
        swc_ecma_ast::PropName::Str(str_lit) => Some(str_lit.value.to_string()),
        swc_ecma_ast::PropName::Num(num) => Some(num.value.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr(name: &str, value: &str) -> (String, Value) {
        (name.to_string(), Value::String(value.to_string()))
    }

    /// `key` must never reach the browser as a raw `key="1"` attribute — but it
    /// IS the delta sink's reconciliation identity, so it is stamped as
    /// `data-albedo-key` (the single SSR stamp point; the QuickJS `h` shim
    /// mirrors it). This is what lets a keyed list's rows be key-reconciled.
    #[test]
    fn render_attrs_stamps_key_as_data_albedo_key() {
        let html = render_attrs(&[attr("className", "entry"), attr("key", "1")]);
        assert_eq!(html, "class=\"entry\" data-albedo-key=\"1\"");
    }

    #[test]
    fn render_attrs_drops_ref_and_children() {
        let html = render_attrs(&[
            attr("ref", "anchor"),
            attr("children", "inner"),
            attr("id", "real"),
        ]);
        assert_eq!(html, "id=\"real\"");
    }

    /// The guard must remove *only* reserved props — a real attribute whose name
    /// merely contains them (`data-key`, `keygen`) is still an attribute.
    #[test]
    fn render_attrs_keeps_attributes_that_only_resemble_reserved_props() {
        let html = render_attrs(&[
            attr("data-key", "k"),
            attr("keygen", "g"),
            attr("aria-keyshortcuts", "s"),
        ]);
        assert_eq!(html, "data-key=\"k\" keygen=\"g\" aria-keyshortcuts=\"s\"");
    }

    /// React's uncontrolled form-control props are not HTML attributes — the DOM
    /// spells them `value` / `checked`. Shipping `defaultValue` verbatim let the
    /// browser lowercase it to the inert `defaultvalue`, so a pre-filled
    /// `<input defaultValue={x}>` (the natural edit-in-a-row shape) rendered
    /// BLANK. Both SSR renderers translate them; the QuickJS `h` shim mirrors
    /// this so Tier-A and Tier-B stay byte-for-byte identical.
    #[test]
    fn render_attrs_translates_uncontrolled_form_props() {
        let html = render_attrs(&[attr("name", "score"), attr("defaultValue", "200")]);
        assert_eq!(html, "name=\"score\" value=\"200\"");

        // `defaultChecked` is boolean: true → bare `checked`, false → omitted,
        // riding the same path `checked` itself would.
        let checked = render_attrs(&[("defaultChecked".to_string(), Value::Bool(true))]);
        assert_eq!(checked, "checked");
        let unchecked = render_attrs(&[("defaultChecked".to_string(), Value::Bool(false))]);
        assert_eq!(unchecked, "");
    }

    /// A dot in the filename is not an extension. `Path::extension()` says
    /// `Foo.Bar` carries extension `Bar`, and treating that as "already
    /// resolved" meant the resolver never tried `Foo.Bar.tsx` — the spec the
    /// module is actually keyed under — so any component file with a dot in
    /// its name (`Foo.Bar.tsx`, `Button.styles.ts`) was unimportable.
    #[test]
    fn import_candidates_extends_a_dotted_filename() {
        let candidates = import_candidates("dir/Foo.Bar");
        assert!(
            candidates.contains(&"dir/Foo.Bar.tsx".to_string()),
            "expected the dotted name to be extended, got {candidates:?}"
        );
        // The `index.*` fallbacks still apply to it.
        assert!(candidates.contains(&"dir/Foo.Bar/index.ts".to_string()));
    }

    /// A specifier that already ends in a module extension is resolved — it
    /// must not sprout `Foo.tsx.jsx` candidates.
    #[test]
    fn import_candidates_leaves_a_module_extension_alone() {
        for spec in ["dir/Foo.tsx", "dir/Foo.jsx", "dir/Foo.js", "dir/Foo.ts"] {
            assert_eq!(import_candidates(spec), vec![spec.to_string()]);
        }
    }

    /// A non-module extension is neither: it resolves to itself first, but
    /// still gets extended, because `theme.css` and `Foo.Bar` are the same
    /// shape and only the module map can tell them apart.
    #[test]
    fn import_candidates_tries_a_non_module_extension_verbatim_first() {
        let candidates = import_candidates("dir/theme.css");
        assert_eq!(candidates.first().unwrap(), "dir/theme.css");
        assert!(candidates.contains(&"dir/theme.css.tsx".to_string()));
    }

    #[test]
    fn import_candidates_extends_a_bare_specifier() {
        assert_eq!(
            import_candidates("dir/Foo"),
            vec![
                "dir/Foo.jsx",
                "dir/Foo.tsx",
                "dir/Foo.js",
                "dir/Foo.ts",
                "dir/Foo/index.jsx",
                "dir/Foo/index.tsx",
                "dir/Foo/index.js",
                "dir/Foo/index.ts",
            ]
        );
    }

    /// `backgroundColor` → `background-color`, and the two prefix spellings
    /// that are not just "lowercase it".
    #[test]
    fn hyphenate_style_name_matches_reacts_spelling() {
        assert_eq!(hyphenate_style_name("backgroundColor"), "background-color");
        assert_eq!(hyphenate_style_name("height"), "height");
        assert_eq!(hyphenate_style_name("WebkitTransform"), "-webkit-transform");
        assert_eq!(hyphenate_style_name("msFlexOrder"), "-ms-flex-order");
    }

    /// The `px` rule and its three exemptions: zero, the unitless set, and
    /// custom properties. A wrong answer here is browser-visible — `flex:1px`
    /// is discarded outright.
    #[test]
    fn numeric_style_values_take_px_unless_the_property_is_exempt() {
        let n = |v: f64| Value::Number(serde_json::Number::from_f64(v).unwrap());
        assert_eq!(style_value_to_css("width", &n(10.0)).unwrap(), "10px");
        assert_eq!(style_value_to_css("marginTop", &n(0.0)).unwrap(), "0");
        assert_eq!(style_value_to_css("flexGrow", &n(2.0)).unwrap(), "2");
        assert_eq!(style_value_to_css("lineHeight", &n(1.5)).unwrap(), "1.5");
        assert_eq!(style_value_to_css("--brand", &n(3.0)).unwrap(), "3");
        // Vendor-prefixed spellings inherit the base property's exemption.
        assert!(is_unitless_style_property("WebkitLineClamp"));
        assert!(is_unitless_style_property("msFlexOrder"));
        assert!(!is_unitless_style_property("width"));
    }

    /// React omits the declaration entirely for these rather than emitting an
    /// empty value.
    #[test]
    fn empty_style_values_drop_their_declaration() {
        for value in [Value::Null, Value::Bool(false), Value::Bool(true)] {
            assert_eq!(style_value_to_css("color", &value), None);
        }
        assert_eq!(style_value_to_css("padding", &Value::String(String::new())), None);
    }

    /// The whole rule end to end, on the excalidraw shape that found the bug.
    /// Order here is the authored order, not the alphabetical one a
    /// `serde_json::Map` would hand back.
    #[test]
    fn style_object_renders_as_css_text_in_the_order_given() {
        let entries = [
            ("height".to_string(), Value::String("1px".into())),
            (
                "backgroundColor".to_string(),
                Value::String("var(--default-border-color)".into()),
            ),
            ("margin".to_string(), Value::String("6px 0".into())),
            ("flex".to_string(), Value::String("0 0 auto".into())),
        ];
        assert_eq!(
            style_object_to_css(entries.iter().map(|(k, v)| (k.as_str(), v))),
            "height:1px;background-color:var(--default-border-color);margin:6px 0;flex:0 0 auto"
        );
    }

    /// The attribute path, which is what actually reaches the page. Before this
    /// the object fell through to `value_to_string` and shipped as JSON.
    #[test]
    fn render_attrs_lowers_a_style_object_instead_of_json_encoding_it() {
        let style = serde_json::json!({ "height": "1px", "color": "red" });
        let html = render_attrs(&[("style".to_string(), style)]);
        // A `serde_json::Map` is a `BTreeMap`, so this path is alphabetical —
        // the documented residual. What matters is that it is CSS, not JSON.
        assert_eq!(html, "style=\"color:red;height:1px\"");
        assert!(!html.contains('{'), "a style object must never ship as JSON: {html}");
    }

    #[test]
    fn is_reserved_jsx_prop_covers_exactly_the_framework_props() {
        assert!(is_reserved_jsx_prop("key"));
        assert!(is_reserved_jsx_prop("ref"));
        assert!(is_reserved_jsx_prop("children"));
        assert!(!is_reserved_jsx_prop("class"));
        assert!(!is_reserved_jsx_prop("data-key"));
        assert!(!is_reserved_jsx_prop("Key"), "matching must be case-exact");
    }
}
