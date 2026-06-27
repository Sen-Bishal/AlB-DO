//! Gate 2 · B — the static layer of the Head/metadata API.
//!
//! Lowers a route's `export const metadata = { ... }` object literal into
//! the authoring-surface-agnostic [`RouteMetadata`] the shell renders.
//! This is build-time and literal-only by design: the object is plain
//! data, so a focused AST walker (no evaluator) is enough. Anything
//! non-literal — a computed expression, a function call, a spread — is
//! skipped here and belongs to the dynamic `generateMetadata()` layer.
//!
//! The field vocabulary mirrors Next.js's `Metadata` object so a Next dev
//! authors metadata unchanged: `title`, `description`, `keywords`,
//! `openGraph`, `twitter`, `robots`, and an `other` escape hatch.

use serde_json::{Map, Number, Value};
use swc_ecma_ast::{Expr, Lit, Prop, PropName, PropOrSpread, UnaryOp};

use crate::manifest::schema::{MetaTag, RouteMetadata};

/// Slice 2 — hoist JSX-rendered `<title>` / `<meta>` tags out of the
/// rendered body HTML into a [`RouteMetadata`], returning the body with
/// those tags removed. Mirrors React 19's automatic head-tag hoisting: a
/// `<title>`/`<meta>` authored inside a component lifts into the document
/// `<head>` rather than rendering in the body.
///
/// The renderer escapes tag text and attribute values, so extracted
/// values are un-escaped back to raw strings here — they re-enter the
/// shared `RouteMetadata` model and the shell re-escapes them on emit, so
/// there is exactly one round of escaping (no double-encoding).
pub fn hoist_head_tags_from_body(body: &str) -> (String, RouteMetadata) {
    let mut out = String::with_capacity(body.len());
    let mut meta = RouteMetadata::default();
    let mut rest = body;

    while !rest.is_empty() {
        if tag_starts(rest, "title") {
            if let Some((consumed, inner)) = take_title(rest) {
                if let Some(text) = inner {
                    meta.title = Some(text);
                }
                rest = &rest[consumed..];
                continue;
            }
        }
        if tag_starts(rest, "meta") {
            if let Some((consumed, attrs)) = take_meta(rest) {
                apply_hoisted_meta(&mut meta, &attrs);
                rest = &rest[consumed..];
                continue;
            }
        }
        let len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&rest[..len]);
        rest = &rest[len..];
    }

    (out, meta)
}

/// True when `rest` opens the element `<name` with a real tag boundary
/// after the name (whitespace, `>`, or `/`) — so `<meta` matches but
/// `<metadata` does not.
fn tag_starts(rest: &str, name: &str) -> bool {
    let open_len = 1 + name.len();
    if rest.len() < open_len || !rest.starts_with('<') {
        return false;
    }
    if !rest[1..open_len].eq_ignore_ascii_case(name) {
        return false;
    }
    match rest[open_len..].chars().next() {
        Some(c) => c.is_whitespace() || c == '>' || c == '/',
        None => false,
    }
}

/// Consume a `<title ...>inner</title>`. Returns the consumed byte length
/// and the un-escaped inner text. `None` (don't hoist) when the open or
/// close tag is malformed/unterminated.
fn take_title(rest: &str) -> Option<(usize, Option<String>)> {
    let open_end = find_tag_end(rest)?;
    let after_open = open_end + 1;
    // Self-closing `<title/>` carries no text.
    if rest[..after_open].trim_end().ends_with("/>") {
        return Some((after_open, None));
    }
    let lower = rest.to_ascii_lowercase();
    let close_rel = lower[after_open..].find("</title>")?;
    let inner = &rest[after_open..after_open + close_rel];
    let consumed = after_open + close_rel + "</title>".len();
    let text = unescape_html(inner.trim());
    Some((consumed, (!text.is_empty()).then_some(text)))
}

/// Consume a `<meta ...>` (or `<meta ... />`). Returns the consumed byte
/// length and the parsed attributes.
fn take_meta(rest: &str) -> Option<(usize, Vec<(String, String)>)> {
    let tag_end = find_tag_end(rest)?;
    // Inside is everything after `<meta` up to the closing `>`,
    // dropping a trailing self-closing slash.
    let inside = rest[5..tag_end].trim().trim_end_matches('/');
    Some((tag_end + 1, parse_attrs(inside)))
}

/// Index of the `>` that closes the tag starting at byte 0, skipping
/// quoted attribute regions so a `>` inside a value isn't mistaken for
/// the tag end.
fn find_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return Some(i),
                _ => {}
            },
        }
    }
    None
}

fn parse_attrs(mut rest: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    rest = rest.trim_start();
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let name = rest[..name_end].trim().to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        if name.is_empty() {
            let len = rest.chars().next().map(char::len_utf8).unwrap_or(1);
            rest = &rest[len..];
            continue;
        }
        if let Some(after_eq) = rest.strip_prefix('=') {
            let (value, consumed) = read_attr_value(after_eq.trim_start());
            rest = &after_eq.trim_start()[consumed..];
            attrs.push((name, unescape_html(&value)));
        } else {
            attrs.push((name, String::new()));
        }
        rest = rest.trim_start();
    }
    attrs
}

fn read_attr_value(s: &str) -> (String, usize) {
    let consumed_prefix = s.len() - s.trim_start().len();
    let s_trimmed = &s[consumed_prefix..];
    for quote in ['"', '\''] {
        if let Some(stripped) = s_trimmed.strip_prefix(quote) {
            return match stripped.find(quote) {
                Some(end) => (stripped[..end].to_string(), consumed_prefix + 1 + end + 1),
                None => (stripped.to_string(), s.len()),
            };
        }
    }
    let end = s_trimmed
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(s_trimmed.len());
    (s_trimmed[..end].to_string(), consumed_prefix + end)
}

/// Route a hoisted `<meta>`'s attributes into the [`RouteMetadata`].
/// `name="description"` rides the structured `description` field (so it
/// dedupes with and overrides the static one); everything else becomes a
/// `MetaTag` keyed by `name` / `property` / `http-equiv`.
fn apply_hoisted_meta(meta: &mut RouteMetadata, attrs: &[(String, String)]) {
    let get = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let Some(content) = get("content") else {
        return;
    };
    if let Some(name) = get("name") {
        if name.eq_ignore_ascii_case("description") {
            meta.description = Some(content);
        } else {
            meta.meta.push(MetaTag {
                attr: "name".to_string(),
                key: name,
                content,
            });
        }
    } else if let Some(property) = get("property") {
        meta.meta.push(MetaTag {
            attr: "property".to_string(),
            key: property,
            content,
        });
    } else if let Some(http_equiv) = get("http-equiv") {
        meta.meta.push(MetaTag {
            attr: "http-equiv".to_string(),
            key: http_equiv,
            content,
        });
    }
}

/// Inverse of the shell's `escape_html`. `&amp;` is resolved last so an
/// escaped entity like `&amp;lt;` round-trips to the literal `&lt;`
/// rather than collapsing to `<`.
fn unescape_html(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Slice 3 — marker placed in a route shell's `<head>` when the route exports
/// `generateMetadata`. The serve path runs `generateMetadata` per request,
/// merges the result over the static base, and replaces this marker with
/// [`render_head_metadata`] of the resolved metadata. Routes without dynamic
/// metadata never carry it (their static head is rendered at build time).
pub const DYNAMIC_HEAD_MARKER: &str = "<!--__ALBEDO_HEAD_META__-->";

/// Render a route's resolved `<head>` metadata block — `<title>`, the
/// `description` meta, and every author `<meta>` tag — to HTML. Shared by the
/// build-time shell (static metadata) and the serve-time dynamic injection so
/// both escape identically and apply the same `ALBEDO {route}` title fallback.
/// `lower_metadata_object` is the companion that turns a `generateMetadata`
/// return value into the [`RouteMetadata`] this renders.
#[must_use]
pub fn render_head_metadata(route: &str, metadata: &RouteMetadata) -> String {
    let mut out = String::new();
    match &metadata.title {
        Some(title) => out.push_str(&format!("<title>{}</title>", escape_html(title))),
        None => out.push_str(&format!("<title>ALBEDO {}</title>", escape_html(route))),
    }
    if let Some(description) = &metadata.description {
        out.push_str(&format!(
            "<meta name=\"description\" content=\"{}\">",
            escape_html(description)
        ));
    }
    for tag in &metadata.meta {
        out.push_str(&format!(
            "<meta {}=\"{}\" content=\"{}\">",
            escape_html(&tag.attr),
            escape_html(&tag.key),
            escape_html(&tag.content)
        ));
    }
    out
}

/// Lower a `generateMetadata()` return value (the Next.js `Metadata` object,
/// arrived as JSON) into a [`RouteMetadata`]. A non-object value yields the
/// default (empty) metadata. The public entry for the dynamic (slice 3) layer;
/// internally identical to the static const path's object lowering.
#[must_use]
pub fn lower_metadata_object(value: &Value) -> RouteMetadata {
    match value {
        Value::Object(map) => metadata_from_json(map),
        _ => RouteMetadata::default(),
    }
}

/// HTML-escape for the head block. Matches the builder's `escape_html` exactly
/// so build-time and serve-time emission are byte-identical.
fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Lower a `export const metadata = { ... }` initializer into a
/// [`RouteMetadata`]. A non-object (or empty) initializer yields the
/// default (empty) metadata, which preserves the shell's historical
/// `<head>` exactly.
pub fn metadata_from_const_expr(expr: &Expr) -> RouteMetadata {
    match expr_to_json(expr) {
        Some(Value::Object(map)) => metadata_from_json(&map),
        _ => RouteMetadata::default(),
    }
}

/// Walk the literal subset of a JS expression into JSON. Returns `None`
/// for anything non-literal so callers can skip it cleanly.
fn expr_to_json(expr: &Expr) -> Option<Value> {
    match expr {
        Expr::Lit(lit) => lit_to_json(lit),
        // A no-substitution template literal (`` `text` ``) is a string.
        Expr::Tpl(tpl) if tpl.exprs.is_empty() && tpl.quasis.len() == 1 => {
            let quasi = &tpl.quasis[0];
            let text = quasi
                .cooked
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_else(|| quasi.raw.to_string());
            Some(Value::String(text))
        }
        Expr::Array(arr) => {
            let mut out = Vec::new();
            for elem in arr.elems.iter().flatten() {
                if elem.spread.is_some() {
                    continue;
                }
                if let Some(value) = expr_to_json(&elem.expr) {
                    out.push(value);
                }
            }
            Some(Value::Array(out))
        }
        Expr::Object(obj) => {
            let mut map = Map::new();
            for prop in &obj.props {
                let PropOrSpread::Prop(prop) = prop else {
                    continue;
                };
                if let Prop::KeyValue(kv) = &**prop {
                    let Some(key) = prop_name_to_key(&kv.key) else {
                        continue;
                    };
                    if let Some(value) = expr_to_json(&kv.value) {
                        map.insert(key, value);
                    }
                }
            }
            Some(Value::Object(map))
        }
        Expr::Paren(paren) => expr_to_json(&paren.expr),
        // `-1` etc. — a unary minus over a numeric literal.
        Expr::Unary(unary) if matches!(unary.op, UnaryOp::Minus) => match expr_to_json(&unary.arg)? {
            Value::Number(n) => n
                .as_f64()
                .and_then(|f| Number::from_f64(-f))
                .map(Value::Number),
            _ => None,
        },
        _ => None,
    }
}

fn lit_to_json(lit: &Lit) -> Option<Value> {
    match lit {
        Lit::Str(s) => Some(Value::String(s.value.to_string())),
        Lit::Num(n) => Number::from_f64(n.value).map(Value::Number),
        Lit::Bool(b) => Some(Value::Bool(b.value)),
        Lit::Null(_) => Some(Value::Null),
        _ => None,
    }
}

fn prop_name_to_key(name: &PropName) -> Option<String> {
    match name {
        PropName::Ident(ident) => Some(ident.sym.to_string()),
        PropName::Str(s) => Some(s.value.to_string()),
        _ => None,
    }
}

/// Map the resolved JSON metadata object onto the Next.js field
/// vocabulary. Unknown keys are ignored rather than erroring — a metadata
/// object can carry more than we render today without breaking the build.
fn metadata_from_json(map: &Map<String, Value>) -> RouteMetadata {
    let mut out = RouteMetadata::default();

    if let Some(title) = map.get("title").and_then(title_to_string) {
        out.title = Some(title);
    }
    if let Some(description) = map.get("description").and_then(Value::as_str) {
        out.description = Some(description.to_string());
    }
    if let Some(keywords) = map.get("keywords").and_then(keywords_to_string) {
        out.push_name("keywords", keywords);
    }

    // Simple top-level name= passthroughs.
    for (field, meta_name) in [
        ("applicationName", "application-name"),
        ("generator", "generator"),
        ("creator", "creator"),
        ("publisher", "publisher"),
        ("category", "category"),
        ("robots", "robots"),
        ("themeColor", "theme-color"),
    ] {
        if let Some(value) = map.get(field).and_then(Value::as_str) {
            out.push_name(meta_name, value.to_string());
        }
    }

    if let Some(Value::Object(og)) = map.get("openGraph") {
        push_open_graph(&mut out, og);
    }
    if let Some(Value::Object(twitter)) = map.get("twitter") {
        push_twitter(&mut out, twitter);
    }

    // `other: { key: value }` — a verbatim escape hatch for tags we don't
    // model. Emitted as `<meta name="key" content="value">`.
    if let Some(Value::Object(other)) = map.get("other") {
        for (key, value) in other {
            if let Some(content) = scalar_to_string(value) {
                out.push_name(key, content);
            }
        }
    }

    out
}

/// `title` is a string, or an object `{ default | absolute }`.
fn title_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => map
            .get("absolute")
            .or_else(|| map.get("default"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// `keywords` is a string or an array of strings → a comma-joined value.
fn keywords_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

fn push_open_graph(out: &mut RouteMetadata, og: &Map<String, Value>) {
    for (field, property) in [
        ("title", "og:title"),
        ("description", "og:description"),
        ("url", "og:url"),
        ("siteName", "og:site_name"),
        ("type", "og:type"),
        ("locale", "og:locale"),
    ] {
        if let Some(value) = og.get(field).and_then(Value::as_str) {
            out.push_property(property, value.to_string());
        }
    }
    // `images` is a string, an object `{ url }`, or an array of either.
    for url in collect_image_urls(og.get("images")) {
        out.push_property("og:image", url);
    }
}

fn push_twitter(out: &mut RouteMetadata, twitter: &Map<String, Value>) {
    for (field, name) in [
        ("card", "twitter:card"),
        ("site", "twitter:site"),
        ("creator", "twitter:creator"),
        ("title", "twitter:title"),
        ("description", "twitter:description"),
    ] {
        if let Some(value) = twitter.get(field).and_then(Value::as_str) {
            out.push_name(name, value.to_string());
        }
    }
    for url in collect_image_urls(twitter.get("images").or_else(|| twitter.get("image"))) {
        out.push_name("twitter:image", url);
    }
}

/// Resolve an `images` field (string | { url } | array of either) into a
/// flat list of URLs.
fn collect_image_urls(value: Option<&Value>) -> Vec<String> {
    fn one(value: &Value) -> Option<String> {
        match value {
            Value::String(s) => Some(s.clone()),
            Value::Object(map) => map.get("url").and_then(Value::as_str).map(str::to_string),
            _ => None,
        }
    }
    match value {
        Some(Value::Array(items)) => items.iter().filter_map(one).collect(),
        Some(other) => one(other).into_iter().collect(),
        None => Vec::new(),
    }
}

fn scalar_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let joined = items
                .iter()
                .filter_map(scalar_to_string)
                .collect::<Vec<_>>()
                .join(", ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

impl RouteMetadata {
    fn push_name(&mut self, key: impl Into<String>, content: impl Into<String>) {
        self.meta.push(MetaTag {
            attr: "name".to_string(),
            key: key.into(),
            content: content.into(),
        });
    }

    fn push_property(&mut self, key: impl Into<String>, content: impl Into<String>) {
        self.meta.push(MetaTag {
            attr: "property".to_string(),
            key: key.into(),
            content: content.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::eval::expr::parse_module;
    use std::path::Path;

    fn extract(source: &str) -> RouteMetadata {
        let module = parse_module(source, Path::new("routes/page.tsx")).expect("module parses");
        module
            .module_constants
            .iter()
            .find(|(name, _)| name == "metadata")
            .map(|(_, expr)| metadata_from_const_expr(expr))
            .unwrap_or_default()
    }

    fn tag<'a>(meta: &'a RouteMetadata, attr: &str, key: &str) -> Option<&'a str> {
        meta.meta
            .iter()
            .find(|t| t.attr == attr && t.key == key)
            .map(|t| t.content.as_str())
    }

    #[test]
    fn lowers_the_full_metadata_object() {
        let source = r#"
            export const metadata = {
              title: "Home — ALBEDO",
              description: "The fastest way to ship.",
              keywords: ["albedo", "ssr", "rust"],
              robots: "index,follow",
              openGraph: {
                title: "Home OG",
                description: "OG description",
                url: "https://albedo.dev",
                siteName: "ALBEDO",
                type: "website",
                images: "https://albedo.dev/og.png",
              },
              twitter: {
                card: "summary_large_image",
                title: "Home on Twitter",
              },
              other: { "fediverse:creator": "@albedo@example.com" },
            };
            export default function Page() { return <main>home</main>; }
        "#;
        let meta = extract(source);

        assert_eq!(meta.title.as_deref(), Some("Home — ALBEDO"));
        assert_eq!(meta.description.as_deref(), Some("The fastest way to ship."));
        assert_eq!(tag(&meta, "name", "keywords"), Some("albedo, ssr, rust"));
        assert_eq!(tag(&meta, "name", "robots"), Some("index,follow"));
        assert_eq!(tag(&meta, "property", "og:title"), Some("Home OG"));
        assert_eq!(tag(&meta, "property", "og:url"), Some("https://albedo.dev"));
        assert_eq!(tag(&meta, "property", "og:site_name"), Some("ALBEDO"));
        assert_eq!(tag(&meta, "property", "og:image"), Some("https://albedo.dev/og.png"));
        assert_eq!(tag(&meta, "name", "twitter:card"), Some("summary_large_image"));
        assert_eq!(tag(&meta, "name", "twitter:title"), Some("Home on Twitter"));
        assert_eq!(tag(&meta, "name", "fediverse:creator"), Some("@albedo@example.com"));
    }

    #[test]
    fn title_object_and_image_array_resolve() {
        let source = r#"
            export const metadata = {
              title: { absolute: "Absolute Title", template: "%s | ALBEDO" },
              openGraph: { images: [{ url: "a.png" }, "b.png"] },
            };
            export default function Page() { return <main>x</main>; }
        "#;
        let meta = extract(source);
        assert_eq!(meta.title.as_deref(), Some("Absolute Title"));
        let images: Vec<&str> = meta
            .meta
            .iter()
            .filter(|t| t.key == "og:image")
            .map(|t| t.content.as_str())
            .collect();
        assert_eq!(images, vec!["a.png", "b.png"]);
    }

    #[test]
    fn no_metadata_export_yields_empty() {
        let source = "export default function Page() { return <main>x</main>; }";
        assert!(extract(source).is_empty());
    }

    #[test]
    fn hoists_title_and_meta_from_body() {
        let body = "<main><title>Hi &amp; Bye</title><p>x</p>\
                    <meta name=\"description\" content=\"a desc\">\
                    <meta property=\"og:title\" content=\"OG\"></main>";
        let (stripped, meta) = hoist_head_tags_from_body(body);

        assert_eq!(meta.title.as_deref(), Some("Hi & Bye"));
        assert_eq!(meta.description.as_deref(), Some("a desc"));
        assert_eq!(tag(&meta, "property", "og:title"), Some("OG"));
        assert!(!stripped.contains("<title"), "title stripped: {stripped}");
        assert!(!stripped.contains("<meta"), "meta stripped: {stripped}");
        assert!(stripped.contains("<p>x</p>"));
        assert!(stripped.starts_with("<main>") && stripped.ends_with("</main>"));
    }

    #[test]
    fn hoist_ignores_partial_tag_names() {
        let body = "<metadata>not a meta</metadata><titlebar>x</titlebar><div>keep</div>";
        let (stripped, meta) = hoist_head_tags_from_body(body);
        assert!(meta.is_empty());
        assert_eq!(stripped, body, "non-head tags must pass through verbatim");
    }

    #[test]
    fn hoist_handles_self_closing_and_quoted_gt() {
        // A `>` inside a quoted attribute value must not end the tag early;
        // a self-closing `<title/>` carries no text.
        let body = "<meta name=\"x\" content=\"a > b\" /><title/>";
        let (stripped, meta) = hoist_head_tags_from_body(body);
        assert_eq!(tag(&meta, "name", "x"), Some("a > b"));
        assert_eq!(meta.title, None);
        assert_eq!(stripped, "");
    }

    #[test]
    fn non_literal_metadata_is_skipped_not_panicked() {
        // A computed metadata object isn't lowerable at build time; the
        // static layer must degrade to empty (the dynamic layer's job),
        // never panic.
        let source = r#"
            const base = "ALBEDO";
            export const metadata = { title: base };
            export default function Page() { return <main>x</main>; }
        "#;
        let meta = extract(source);
        // `base` is an identifier, not a literal → title unresolved.
        assert_eq!(meta.title, None);
    }

    #[test]
    fn lower_metadata_object_maps_a_dynamic_result() {
        // The shape a `generateMetadata()` return value arrives as (JSON).
        let value = serde_json::json!({
            "title": "Dynamic",
            "description": "per request",
            "openGraph": { "title": "OG", "type": "website" }
        });
        let meta = lower_metadata_object(&value);
        assert_eq!(meta.title.as_deref(), Some("Dynamic"));
        assert_eq!(meta.description.as_deref(), Some("per request"));
        assert_eq!(tag(&meta, "property", "og:title"), Some("OG"));
        assert_eq!(tag(&meta, "property", "og:type"), Some("website"));

        // A non-object result (a route that returns nothing useful) is empty,
        // not a panic.
        assert!(lower_metadata_object(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn render_head_metadata_emits_block_and_falls_back() {
        // Dynamic title wins and escapes; description + author meta render.
        let mut meta = RouteMetadata {
            title: Some("A & B".to_string()),
            description: Some("d".to_string()),
            meta: Vec::new(),
        };
        meta.push_property("og:title", "OG".to_string());
        let html = render_head_metadata("/post", &meta);
        assert!(html.contains("<title>A &amp; B</title>"), "{html}");
        assert!(html.contains("<meta name=\"description\" content=\"d\">"), "{html}");
        assert!(html.contains("<meta property=\"og:title\" content=\"OG\">"), "{html}");

        // Empty metadata falls back to the `ALBEDO {route}` title — identical to
        // the build-time shell, so a dynamic route whose eval fails degrades
        // cleanly.
        let fallback = render_head_metadata("/post", &RouteMetadata::default());
        assert_eq!(fallback, "<title>ALBEDO /post</title>");
    }
}
