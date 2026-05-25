//! Phase N · CSS-modules scoping pass.
//!
//! Hashes the source path to a short suffix, rewrites every class
//! selector in the file from `.foo` to `.foo_<hash>`, and returns both
//! the rewritten CSS and the original→scoped class name map.
//!
//! The map is what JSX-side `styles.foo` references resolve against —
//! the dev/build pipeline injects the scoped CSS into the route's
//! `<style>` block and replaces `styles.foo` accesses at render time
//! with the scoped class name.
//!
//! Scope is intentionally narrow: this matches `.identifier` tokens in
//! selector context and ignores anything inside `{...}` rule bodies,
//! `@keyframes` names, attribute selectors, or quoted strings. That
//! covers the common-case scaffolds without pulling in a CSS parser.
//! Pseudo-classes (`.foo:hover`), combinators (`.foo > .bar`), and
//! comma-separated selector lists all work because the matcher fires
//! on every `.identifier` outside a `{...}` body.

use std::collections::BTreeMap;
use xxhash_rust::xxh3::xxh3_64;

const HASH_LEN: usize = 8;

/// Output of [`scope_module_css`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedCssModule {
    /// CSS with every class selector rewritten to `.<original>_<hash>`.
    pub scoped_css: String,
    /// Map from original class name (without leading `.`) to the
    /// scoped name. JSX references `styles.foo` resolve to
    /// `class_map["foo"]`.
    pub class_map: BTreeMap<String, String>,
    /// The hash suffix derived from `module_id`. Useful for tests and
    /// for downstream tooling that wants to assert deterministic
    /// scoping without re-hashing.
    pub hash_suffix: String,
}

/// Scope every class selector in `css_source` under a hash derived
/// from `module_id`.
///
/// `module_id` is typically the relative path of the `.module.css`
/// file. Two files with the same logical path always produce the same
/// hash; renaming the file changes every scoped class name, by design
/// — class names ride alongside content into the bundle, so any
/// rename invalidates the cached bundle anyway.
pub fn scope_module_css(module_id: &str, css_source: &str) -> ScopedCssModule {
    let hash = derive_hash_suffix(module_id);
    let mut class_map: BTreeMap<String, String> = BTreeMap::new();
    let mut scoped_css = String::with_capacity(css_source.len() + 32);

    let bytes = css_source.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut in_string: Option<u8> = None;

    while i < bytes.len() {
        let ch = bytes[i];

        if let Some(quote) = in_string {
            scoped_css.push(ch as char);
            if ch == quote && bytes.get(i.saturating_sub(1)).copied() != Some(b'\\') {
                in_string = None;
            }
            i += 1;
            continue;
        }
        if ch == b'"' || ch == b'\'' {
            scoped_css.push(ch as char);
            in_string = Some(ch);
            i += 1;
            continue;
        }

        // Skip CSS comments verbatim so anything inside `/* ... */`
        // does not get rewritten.
        if ch == b'/' && bytes.get(i + 1).copied() == Some(b'*') {
            let end = find_comment_end(bytes, i + 2);
            scoped_css.push_str(&css_source[i..end]);
            i = end;
            continue;
        }

        if ch == b'{' {
            depth += 1;
            scoped_css.push('{');
            i += 1;
            continue;
        }
        if ch == b'}' {
            depth -= 1;
            scoped_css.push('}');
            i += 1;
            continue;
        }

        // Outside any rule body, every `.identifier` is a class
        // selector. Don't rewrite property values — those live inside
        // `{...}` and are handled by the `depth > 0` early-out.
        if depth == 0 && ch == b'.' && is_ident_start(bytes.get(i + 1).copied()) {
            let (name, end) = read_identifier(bytes, i + 1);
            class_map.entry(name.clone()).or_insert_with(|| {
                format!("{name}_{hash}")
            });
            scoped_css.push('.');
            scoped_css.push_str(&name);
            scoped_css.push('_');
            scoped_css.push_str(&hash);
            i = end;
            continue;
        }

        scoped_css.push(ch as char);
        i += 1;
    }

    ScopedCssModule {
        scoped_css,
        class_map,
        hash_suffix: hash,
    }
}

/// Returns true if `path` is a CSS module by convention
/// (`*.module.css`). The bundler / dev path uses this to pick which
/// stylesheets get scoped vs. emitted raw.
pub fn is_css_module_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".module.css")
}

fn derive_hash_suffix(module_id: &str) -> String {
    let hash = xxh3_64(module_id.as_bytes());
    let hex = format!("{hash:016x}");
    hex[..HASH_LEN].to_string()
}

fn find_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            return i + 2;
        }
        i += 1;
    }
    bytes.len()
}

fn read_identifier(bytes: &[u8], start: usize) -> (String, usize) {
    let mut end = start;
    while end < bytes.len() && is_ident_continue(bytes[end]) {
        end += 1;
    }
    let name = String::from_utf8_lossy(&bytes[start..end]).to_string();
    (name, end)
}

fn is_ident_start(byte: Option<u8>) -> bool {
    matches!(byte, Some(b) if b.is_ascii_alphabetic() || b == b'_' || b == b'-')
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_class_is_scoped() {
        let scoped = scope_module_css("Counter.module.css", ".btn { color: red; }");
        let hash = &scoped.hash_suffix;
        assert_eq!(scoped.scoped_css, format!(".btn_{hash} {{ color: red; }}"));
        assert_eq!(scoped.class_map.get("btn"), Some(&format!("btn_{hash}")));
    }

    #[test]
    fn multiple_classes_share_one_hash() {
        let scoped = scope_module_css("X.module.css", ".btn {} .input {} .btn:hover {}");
        let hash = &scoped.hash_suffix;
        assert!(scoped.scoped_css.contains(&format!(".btn_{hash}")));
        assert!(scoped.scoped_css.contains(&format!(".input_{hash}")));
        assert!(scoped.scoped_css.contains(&format!(".btn_{hash}:hover")));
        assert_eq!(scoped.class_map.len(), 2);
    }

    #[test]
    fn different_files_produce_different_hashes() {
        let a = scope_module_css("A.module.css", ".btn {}");
        let b = scope_module_css("B.module.css", ".btn {}");
        assert_ne!(a.hash_suffix, b.hash_suffix);
        assert_ne!(
            a.class_map.get("btn").unwrap(),
            b.class_map.get("btn").unwrap()
        );
    }

    #[test]
    fn rule_body_content_is_left_alone() {
        let scoped = scope_module_css("X.module.css", ".card { background: .9; }");
        let hash = &scoped.hash_suffix;
        // `.9` inside `{...}` is a numeric value, not a class — must NOT be scoped.
        assert_eq!(
            scoped.scoped_css,
            format!(".card_{hash} {{ background: .9; }}")
        );
    }

    #[test]
    fn comments_and_strings_are_preserved_verbatim() {
        let scoped = scope_module_css(
            "X.module.css",
            "/* .ignored */ .real { content: \".str\"; }",
        );
        let hash = &scoped.hash_suffix;
        assert!(scoped.scoped_css.starts_with("/* .ignored */"));
        assert!(scoped.scoped_css.contains(&format!(".real_{hash}")));
        assert!(scoped.scoped_css.contains("\".str\""));
        assert!(!scoped.class_map.contains_key("ignored"));
        assert!(!scoped.class_map.contains_key("str"));
    }

    #[test]
    fn descendant_combinators_get_both_sides_scoped() {
        let scoped = scope_module_css("X.module.css", ".panel > .row { gap: 4px; }");
        let hash = &scoped.hash_suffix;
        assert_eq!(
            scoped.scoped_css,
            format!(".panel_{hash} > .row_{hash} {{ gap: 4px; }}")
        );
        assert!(scoped.class_map.contains_key("panel"));
        assert!(scoped.class_map.contains_key("row"));
    }

    #[test]
    fn is_css_module_path_recognises_extension() {
        assert!(is_css_module_path("Counter.module.css"));
        assert!(is_css_module_path("a/b/Counter.module.css"));
        assert!(!is_css_module_path("Counter.css"));
        assert!(!is_css_module_path("Counter.module.scss"));
    }

    #[test]
    fn hash_is_deterministic_across_runs() {
        let a = scope_module_css("path.module.css", ".x {}").hash_suffix;
        let b = scope_module_css("path.module.css", ".x {}").hash_suffix;
        assert_eq!(a, b);
    }
}
