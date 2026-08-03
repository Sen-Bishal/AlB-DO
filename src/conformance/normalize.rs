//! The *declared* differences between the two renderers.
//!
//! Every transformation in this file is a difference we have looked at and
//! decided is not a defect. That is the whole reason they live together in one
//! small module instead of being sprinkled through the comparison as ad-hoc
//! `replace` calls: the set of things we are willing to forgive has to be
//! readable in one sitting, and it has to be *finite*.
//!
//! Two rules keep this honest:
//!
//! 1. **No normalization may delete information.** Reordering attributes keeps
//!    every attribute; unwrapping an anchor keeps every child. A normalization
//!    that could drop a wrong value would turn the harness into a machine for
//!    agreeing with itself.
//! 2. **Each one is recorded per case.** [`super::Verdict::Equivalent`] carries
//!    the list of normalizations a case actually needed, so the report can
//!    distinguish "identical bytes" from "identical after we forgave two
//!    things" — and a case that slips from the first to the second is visible
//!    the day it happens.

use std::fmt;

/// A difference the harness is willing to forgive, and why it is not a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Normalization {
    /// Attribute *order* within a tag differs.
    ///
    /// Not semantic in HTML, and the divergence is structural rather than
    /// accidental: the pure-Rust renderer rewrites an attribute in the
    /// author's position (`action="action:x"` becomes `data-albedo-action` in
    /// place), while the QuickJS shim is bottom-up and can only append. The
    /// attribute *set* is compared exactly — a missing, extra, or wrong-valued
    /// attribute still diverges.
    AttributeOrder,

    /// A `display:contents` reactive anchor wrapper is present in one render
    /// and not the other.
    ///
    /// This one is a real structural difference, not a cosmetic one, and it is
    /// forgiven only because it is *intended*: hook-compile mode wraps a
    /// conditional or list region in a `<span style="display:contents">` so the
    /// client has a node to patch. Phase-J markup has no such regions. The
    /// wrapper's children are kept, so the markup inside a reactive region is
    /// still compared in full.
    ///
    /// ⚠️ Forgiving it here is **not** a claim that the two renders are
    /// interchangeable at serve time. Whether the frame's ids exist in the
    /// markup that is actually served is a separate question, asked separately
    /// by the addressability check.
    ReactiveAnchorWrapper,
}

impl fmt::Display for Normalization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Normalization::AttributeOrder => "attribute-order",
            Normalization::ReactiveAnchorWrapper => "reactive-anchor-wrapper",
        })
    }
}

/// The exact open tag the pure-Rust renderer emits for a reactive region.
/// Mirrors the two `format!`s in `runtime::eval::core` that produce it.
const ANCHOR_PREFIX: &str = "<span data-albedo-id=\"";
const ANCHOR_SUFFIX: &str = "\" style=\"display:contents\">";

/// Remove reactive anchor wrappers, keeping their children.
///
/// Scans for the exact open tag the renderer emits and unwraps to its matching
/// `</span>`, tracking nested `<span` depth so an anchor containing spans (the
/// common case — list rows are frequently spans) unwraps to the right close
/// tag rather than the first one.
pub fn strip_reactive_anchors(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    loop {
        let Some(start) = rest.find(ANCHOR_PREFIX) else {
            out.push_str(rest);
            return out;
        };
        // The id must be all digits and followed by exactly the anchor suffix,
        // or this is an ordinary span that merely shares a prefix.
        let after_prefix = &rest[start + ANCHOR_PREFIX.len()..];
        let Some(quote) = after_prefix.find('"') else {
            out.push_str(rest);
            return out;
        };
        let is_anchor = after_prefix[..quote].chars().all(|c| c.is_ascii_digit())
            && after_prefix[quote..].starts_with(ANCHOR_SUFFIX);
        if !is_anchor {
            let consumed = start + ANCHOR_PREFIX.len();
            out.push_str(&rest[..consumed]);
            rest = &rest[consumed..];
            continue;
        }

        out.push_str(&rest[..start]);
        let body_start = start + ANCHOR_PREFIX.len() + quote + ANCHOR_SUFFIX.len();
        let body = &rest[body_start..];

        match find_matching_span_close(body) {
            Some((inner_end, after_close)) => {
                // Recurse into the body so nested anchors unwrap too.
                out.push_str(&strip_reactive_anchors(&body[..inner_end]));
                rest = &body[after_close..];
            }
            None => {
                // Unbalanced markup: emit the remainder verbatim rather than
                // inventing structure. The comparison will fail, which is the
                // correct outcome for markup we cannot parse.
                out.push_str(body);
                return out;
            }
        }
    }
}

/// Given the text just after an anchor's open tag, find the byte range of its
/// children and the offset just past its matching `</span>`.
fn find_matching_span_close(body: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut idx = 0usize;
    let bytes = body.as_bytes();

    while idx < bytes.len() {
        if bytes[idx] != b'<' {
            idx += 1;
            continue;
        }
        let tail = &body[idx..];
        if tail.starts_with("</span>") {
            if depth == 0 {
                return Some((idx, idx + "</span>".len()));
            }
            depth -= 1;
            idx += "</span>".len();
            continue;
        }
        // An opening `<span` — but not a self-closed `<span … />`, which never
        // gets a close tag. Spans are not void elements, so the renderer only
        // emits the self-closed form for an empty one.
        if tail.starts_with("<span") {
            if let Some(gt) = tail.find('>') {
                if !tail[..gt].ends_with('/') {
                    depth += 1;
                }
                idx += gt + 1;
                continue;
            }
        }
        idx += 1;
    }
    None
}

/// Rewrite every open tag so its attributes appear in a canonical order.
///
/// Preserves the attribute set exactly — this sorts, it never drops or merges.
/// Text, comments and close tags pass through untouched.
pub fn canonicalize_attribute_order(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut idx = 0usize;

    while idx < bytes.len() {
        if bytes[idx] != b'<' {
            let next = html[idx..].find('<').map(|off| idx + off).unwrap_or(bytes.len());
            out.push_str(&html[idx..next]);
            idx = next;
            continue;
        }

        let tail = &html[idx..];

        // Comments pass through whole — the layout-children sentinel is one,
        // and a `>` inside a comment must not be read as a tag end.
        if tail.starts_with("<!--") {
            let end = tail.find("-->").map(|off| off + 3).unwrap_or(tail.len());
            out.push_str(&tail[..end]);
            idx += end;
            continue;
        }

        // Not a tag start (a bare `<` in text): copy and move on.
        let is_tag = tail[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        if !is_tag {
            out.push('<');
            idx += 1;
            continue;
        }

        let Some(tag_len) = tag_end(tail) else {
            out.push_str(tail);
            return out;
        };
        let tag = &tail[..tag_len];
        out.push_str(&sort_tag_attributes(tag));
        idx += tag_len;
    }

    out
}

/// Length of the tag starting at `tail[0] == '<'`, including the closing `>`,
/// respecting quoted attribute values (which may contain `>`).
fn tag_end(tail: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (offset, ch) in tail.char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => return Some(offset + ch.len_utf8()),
            None => {}
        }
    }
    None
}

/// Sort the attributes of one open tag. Close tags and tags with fewer than two
/// attributes are returned unchanged.
fn sort_tag_attributes(tag: &str) -> String {
    let inner = &tag[1..tag.len() - 1];
    if inner.starts_with('/') || inner.starts_with('!') {
        return tag.to_string();
    }

    // A trailing `/` marks a self-closed tag and is not an attribute.
    let (inner, self_closing) = match inner.strip_suffix('/') {
        Some(stripped) => (stripped, true),
        None => (inner, false),
    };

    let mut chars = inner.char_indices();
    let name_end = loop {
        match chars.next() {
            Some((offset, c)) if c.is_whitespace() => break offset,
            Some(_) => {}
            None => return tag.to_string(),
        }
    };
    let name = &inner[..name_end];

    let mut attrs = split_attributes(&inner[name_end..]);
    if attrs.len() < 2 {
        return tag.to_string();
    }
    attrs.sort();

    let mut out = String::with_capacity(tag.len());
    out.push('<');
    out.push_str(name);
    for attr in attrs {
        out.push(' ');
        out.push_str(&attr);
    }
    if self_closing {
        out.push_str(" /");
    }
    out.push('>');
    out
}

/// Split an attribute run into whole `name="value"` / `name` tokens, keeping
/// quoted values (which may contain spaces) intact.
fn split_attributes(run: &str) -> Vec<String> {
    let mut attrs = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in run.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                current.push(ch);
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                current.push(ch);
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    attrs.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        attrs.push(current);
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_a_reactive_anchor_and_keeps_its_children() {
        let html = "<ul><span data-albedo-id=\"12\" style=\"display:contents\">\
                    <li>a</li><li>b</li></span></ul>";
        assert_eq!(
            strip_reactive_anchors(html),
            "<ul><li>a</li><li>b</li></ul>"
        );
    }

    /// The reason `find_matching_span_close` counts depth: list rows are
    /// routinely spans, and closing on the first `</span>` would truncate the
    /// region and silently "agree" with a shorter render.
    #[test]
    fn a_nested_span_does_not_end_the_anchor_early() {
        let html = "<div><span data-albedo-id=\"7\" style=\"display:contents\">\
                    <span class=\"row\">a</span><span class=\"row\">b</span></span></div>";
        assert_eq!(
            strip_reactive_anchors(html),
            "<div><span class=\"row\">a</span><span class=\"row\">b</span></div>"
        );
    }

    #[test]
    fn an_ordinary_span_with_an_id_is_not_an_anchor() {
        let html = "<span data-albedo-id=\"9\">kept</span>";
        assert_eq!(strip_reactive_anchors(html), html);
    }

    #[test]
    fn nested_anchors_both_unwrap() {
        let html = "<span data-albedo-id=\"1\" style=\"display:contents\">\
                    <span data-albedo-id=\"2\" style=\"display:contents\">x</span></span>";
        assert_eq!(strip_reactive_anchors(html), "x");
    }

    #[test]
    fn attribute_order_is_canonical_but_the_attribute_set_is_untouched() {
        let a = "<form data-albedo-action=\"x\" data-albedo-id=\"1\">hi</form>";
        let b = "<form data-albedo-id=\"1\" data-albedo-action=\"x\">hi</form>";
        assert_eq!(
            canonicalize_attribute_order(a),
            canonicalize_attribute_order(b)
        );
    }

    /// The normalization must not be able to make a *wrong* attribute look
    /// right — otherwise it is a machine for agreeing with itself.
    #[test]
    fn a_differing_attribute_value_still_differs_after_canonicalization() {
        let a = "<div class=\"one\" id=\"x\">t</div>";
        let b = "<div id=\"x\" class=\"two\">t</div>";
        assert_ne!(
            canonicalize_attribute_order(a),
            canonicalize_attribute_order(b)
        );
    }

    #[test]
    fn a_missing_attribute_still_differs_after_canonicalization() {
        let a = "<div class=\"one\" id=\"x\">t</div>";
        let b = "<div id=\"x\">t</div>";
        assert_ne!(
            canonicalize_attribute_order(a),
            canonicalize_attribute_order(b)
        );
    }

    #[test]
    fn quoted_values_containing_spaces_and_angle_brackets_survive() {
        let html = "<div title=\"a > b, c\" class=\"x y\">t</div>";
        assert_eq!(
            canonicalize_attribute_order(html),
            "<div class=\"x y\" title=\"a > b, c\">t</div>"
        );
    }

    #[test]
    fn comments_pass_through_whole() {
        let html = "<div><!--__ALBEDO_LAYOUT_CHILDREN__--></div>";
        assert_eq!(canonicalize_attribute_order(html), html);
    }

    #[test]
    fn a_self_closed_void_element_keeps_its_slash() {
        let html = "<hr class=\"rule\" data-albedo-id=\"3\" />";
        assert_eq!(
            canonicalize_attribute_order(html),
            "<hr class=\"rule\" data-albedo-id=\"3\" />"
        );
    }

    #[test]
    fn a_boolean_attribute_sorts_alongside_valued_ones() {
        let a = "<input required name=\"x\" />";
        let b = "<input name=\"x\" required />";
        assert_eq!(
            canonicalize_attribute_order(a),
            canonicalize_attribute_order(b)
        );
    }
}
