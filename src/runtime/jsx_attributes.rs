//! The single JSX-prop → HTML-attribute rename table, shared by every renderer.
//!
//! ## Why this is one table and not three
//!
//! Three renderers emit attributes from the same JSX, and **two of them are
//! required to agree byte-for-byte**, not merely to parse to the same tree:
//!
//! | renderer | site |
//! |---|---|
//! | pure-Rust (Tier A/B static) | [`crate::runtime::eval::component`] |
//! | QuickJS `h` (Tier B per-request, Tier C island SSR) | the `h` shim's attribute loop |
//! | `assets/albedo-client.js` (hydration + client updates) | `applyProp` |
//!
//! Hydration *adopts* the server's DOM on the strength of that agreement. A
//! renderer that spells one attribute differently does not produce a cosmetic
//! difference — it produces a re-mount, or a stray attribute on an adopted node.
//! This codebase has already paid for three independent implementations of one
//! rule once (the paint rules); this is the same shape, so the rule lives in one
//! place and the other two are *derived* from it:
//!
//! * the pure-Rust renderer calls [`jsx_attribute_name`] directly;
//! * the QuickJS prelude is **generated** from [`JSX_ATTRIBUTE_RENAMES`] by
//!   [`build_jsx_attribute_table_script`];
//! * `albedo-client.js` is hand-written JavaScript that cannot be generated, so
//!   a test asserts its table is set-equal to this one. That test is the only
//!   thing standing between the three, and it is deliberately loud.
//!
//! ## The SVG half, and why it needs no context
//!
//! 🔑 **The mapping is keyed on the attribute name alone.** `strokeWidth` is
//! meaningless on a `<div>` and `fillRule` is meaningless outside SVG, so no
//! renderer has to know whether it is inside an `<svg>` subtree to decide the
//! spelling. That matters because the QuickJS `h` is **eager and depth-first** —
//! a child is stringified before its parent exists, so a context flag could not
//! work there even if we wanted one. Element *creation* is still
//! context-sensitive (`createElementNS` on the client), but that is a separate
//! question with a separate answer.
//!
//! ⚠️ **This was already broken before npm entered the picture.** `<svg
//! strokeWidth="2">` authored directly in a Tier-A component shipped the
//! browser-inert `strokewidth`, because the pure-Rust renamer fell through to
//! the identity case. Every icon drawn by hand in a component has been rendering
//! without its stroke widths.
//!
//! ## Boundary
//!
//! The SVG entries cover the **presentation attributes** real content uses —
//! every icon set, every chart library — not the full SVG 1.1 surface, which is
//! two hundred filter-primitive attributes nothing here has ever emitted. Adding
//! one is a single line in one table.
//!
//! Attributes whose SVG spelling is already camelCase (`viewBox`,
//! `preserveAspectRatio`, `gradientUnits`, …) are **deliberately absent**: they
//! map to themselves. The server writes them verbatim into HTML text, where the
//! parser's own SVG adjustment table restores the case inside an `<svg>`; the
//! client writes them with `setAttribute` on a namespaced element, which is
//! case-preserving. An identity entry would be a line that can rot.

/// Every JSX prop whose emitted attribute name differs from the prop name.
///
/// Sorted by group, then alphabetically inside it, so a reader can find an entry
/// and a reviewer can see a duplicate.
pub const JSX_ATTRIBUTE_RENAMES: &[(&str, &str)] = &[
    // ── HTML ────────────────────────────────────────────────────────────
    // JSX cannot spell `class` or `for` (both reserved words), so React-shaped
    // code writes `className`/`htmlFor` and every renderer must undo it.
    // Without the `htmlFor` rename a `<label>` is silently DISCONNECTED from its
    // control: clicking does nothing and a screen reader announces an
    // unlabelled input.
    ("className", "class"),
    ("htmlFor", "for"),
    // React's uncontrolled form-control props. Without these a pre-filled
    // `<input defaultValue={x}>` ships the inert `defaultvalue` and renders
    // blank.
    ("defaultChecked", "checked"),
    ("defaultValue", "value"),
    // ── SVG presentation attributes ─────────────────────────────────────
    ("alignmentBaseline", "alignment-baseline"),
    ("baselineShift", "baseline-shift"),
    ("clipPath", "clip-path"),
    ("clipRule", "clip-rule"),
    ("colorInterpolation", "color-interpolation"),
    ("colorInterpolationFilters", "color-interpolation-filters"),
    ("dominantBaseline", "dominant-baseline"),
    ("fillOpacity", "fill-opacity"),
    ("fillRule", "fill-rule"),
    ("floodColor", "flood-color"),
    ("floodOpacity", "flood-opacity"),
    ("fontFamily", "font-family"),
    ("fontSize", "font-size"),
    ("fontSizeAdjust", "font-size-adjust"),
    ("fontStretch", "font-stretch"),
    ("fontStyle", "font-style"),
    ("fontVariant", "font-variant"),
    ("fontWeight", "font-weight"),
    ("imageRendering", "image-rendering"),
    ("letterSpacing", "letter-spacing"),
    ("lightingColor", "lighting-color"),
    ("markerEnd", "marker-end"),
    ("markerMid", "marker-mid"),
    ("markerStart", "marker-start"),
    ("paintOrder", "paint-order"),
    ("pointerEvents", "pointer-events"),
    ("shapeRendering", "shape-rendering"),
    ("stopColor", "stop-color"),
    ("stopOpacity", "stop-opacity"),
    ("strokeDasharray", "stroke-dasharray"),
    ("strokeDashoffset", "stroke-dashoffset"),
    ("strokeLinecap", "stroke-linecap"),
    ("strokeLinejoin", "stroke-linejoin"),
    ("strokeMiterlimit", "stroke-miterlimit"),
    ("strokeOpacity", "stroke-opacity"),
    ("strokeWidth", "stroke-width"),
    ("textAnchor", "text-anchor"),
    ("textDecoration", "text-decoration"),
    ("textRendering", "text-rendering"),
    ("unicodeBidi", "unicode-bidi"),
    ("vectorEffect", "vector-effect"),
    ("wordSpacing", "word-spacing"),
    ("writingMode", "writing-mode"),
];

/// The attribute name to emit for a JSX prop. Identity for anything unlisted.
#[must_use]
pub fn jsx_attribute_name(name: &str) -> &str {
    // A linear scan over ~45 entries, on a path that already allocates a String
    // per attribute value. A `phf` map would be measurably faster and would add
    // a dependency to save nanoseconds beside a heap allocation; if this ever
    // shows up in a profile, the fix is to stop allocating, not to index this.
    for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
        if *prop == name {
            return attribute;
        }
    }
    name
}

/// The QuickJS-side lookup, generated from [`JSX_ATTRIBUTE_RENAMES`].
///
/// Installs `globalThis.__albedo_attr_name`, which the `h` shim's attribute loop
/// calls. Generated rather than written into the prelude's raw string so the two
/// server renderers cannot disagree — which is the whole contract the `h` shim's
/// own comment states: *"the ` />` spelling is the pure-Rust renderer's, so the
/// two agree byte-for-byte rather than merely parsing to the same tree."*
#[must_use]
pub fn build_jsx_attribute_table_script() -> String {
    let mut entries = String::new();
    for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
        entries.push_str(&format!("  '{prop}': '{attribute}',\n"));
    }
    format!(
        "(function() {{\n\
         if (globalThis.__albedo_attr_name) {{ return; }}\n\
         var table = {{\n{entries}}};\n\
         var own = Object.prototype.hasOwnProperty;\n\
         globalThis.__albedo_attr_name = function(name) {{\n\
         \x20 return own.call(table, name) ? table[name] : name;\n\
         }};\n\
         }})();\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn html_renames_are_preserved() {
        assert_eq!(jsx_attribute_name("className"), "class");
        assert_eq!(jsx_attribute_name("htmlFor"), "for");
        assert_eq!(jsx_attribute_name("defaultValue"), "value");
        assert_eq!(jsx_attribute_name("defaultChecked"), "checked");
    }

    #[test]
    fn svg_presentation_attributes_hyphenate() {
        assert_eq!(jsx_attribute_name("strokeWidth"), "stroke-width");
        assert_eq!(jsx_attribute_name("strokeLinecap"), "stroke-linecap");
        assert_eq!(jsx_attribute_name("fillRule"), "fill-rule");
    }

    /// Already-camelCase SVG attributes map to themselves and must not be
    /// listed — an identity entry is a line that can rot.
    #[test]
    fn camel_case_svg_attributes_are_identities() {
        assert_eq!(jsx_attribute_name("viewBox"), "viewBox");
        assert_eq!(jsx_attribute_name("preserveAspectRatio"), "preserveAspectRatio");
        assert_eq!(jsx_attribute_name("gradientUnits"), "gradientUnits");
    }

    #[test]
    fn unlisted_props_pass_through() {
        assert_eq!(jsx_attribute_name("id"), "id");
        assert_eq!(jsx_attribute_name("data-albedo-id"), "data-albedo-id");
        assert_eq!(jsx_attribute_name("aria-label"), "aria-label");
    }

    #[test]
    fn the_table_has_no_duplicate_props() {
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
            assert!(
                seen.insert(prop, attribute).is_none(),
                "duplicate entry for {prop}"
            );
        }
    }

    #[test]
    fn the_generated_script_carries_every_entry() {
        let script = build_jsx_attribute_table_script();
        for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
            assert!(
                script.contains(&format!("'{prop}': '{attribute}'")),
                "{prop} missing from the generated table"
            );
        }
    }

    /// 🔑 **The third copy cannot be generated, so it is asserted.**
    /// `assets/albedo-client.js` is hand-written JavaScript served to the
    /// browser; if its table drifts from this one, hydration stops adopting the
    /// server's DOM and starts replacing it — silently, and only for the
    /// attributes that differ.
    #[test]
    fn the_client_runtime_table_matches_this_one() {
        let client = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/albedo-client.js"
        ));
        let start = client
            .find("var JSX_ATTRIBUTE_RENAMES = {")
            .expect("albedo-client.js must declare JSX_ATTRIBUTE_RENAMES");
        let body = &client[start..];
        let end = body.find("};").expect("the table must be closed");
        let body = &body[..end];

        let mut found: BTreeMap<String, String> = BTreeMap::new();
        for line in body.lines().skip(1) {
            let line = line.trim().trim_end_matches(',');
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let Some((prop, attribute)) = line.split_once(':') else {
                continue;
            };
            let unquote = |s: &str| s.trim().trim_matches('\'').trim_matches('"').to_string();
            found.insert(unquote(prop), unquote(attribute));
        }

        let expected: BTreeMap<String, String> = JSX_ATTRIBUTE_RENAMES
            .iter()
            .map(|(prop, attribute)| ((*prop).to_string(), (*attribute).to_string()))
            .collect();

        assert_eq!(
            found, expected,
            "assets/albedo-client.js's rename table has drifted from \
             JSX_ATTRIBUTE_RENAMES — the client would emit different attribute \
             names than the server, and hydration would replace adopted DOM \
             instead of keeping it"
        );
    }
}
