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

/// How a **boolean-valued** JSX prop becomes an HTML attribute.
///
/// 🔑 **HTML has two unrelated kinds of attribute that both take `true` in
/// JSX.** Conflating them is not a formatting difference — it changes what the
/// attribute *means*:
///
/// | kind | `true` | `false` | example |
/// |---|---|---|---|
/// | boolean attribute | present, bare | **absent** | `disabled`, `checked`, `hidden` |
/// | enumerated attribute | `="true"` | `="false"` | `aria-expanded`, `aria-hidden` |
///
/// For a real boolean attribute the *presence* of the name is the whole signal
/// and the value is ignored, so `disabled="false"` is still disabled. For an
/// enumerated attribute the value **is** the signal, and its value space is the
/// two literal strings `"true"` and `"false"` — a bare `aria-expanded` is the
/// empty string, which is in neither. Assistive technology reads that as *no
/// value supplied*, i.e. not expanded.
///
/// ⚠️ **Both halves were wrong for aria before this existed.** All three
/// renderers emitted `true` bare and *dropped* `false` entirely, so
/// `aria-expanded={true}` shipped inert and `aria-hidden={false}` shipped
/// nothing at all — and "not hidden" is a claim, not the absence of one: it is
/// what stops an ancestor's `aria-hidden="true"` from being inherited over a
/// subtree that must stay reachable. Radix wires **every** compound component's
/// accessibility through this shape (`aria-expanded`, `aria-selected`,
/// `aria-checked`, `aria-pressed`, `aria-hidden`), always as booleans, so the
/// entire shadcn/UI layer server-rendered with dead aria state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BooleanAttributeForm {
    /// A real HTML boolean attribute: emit the bare name for `true`, emit
    /// nothing for `false`.
    Bare,
    /// An enumerated attribute: emit `="true"` / `="false"`. Never dropped.
    Enumerated,
}

/// The enumerated-boolean attributes that are **not** matched by the `aria-`
/// prefix rule in [`boolean_attribute_form`].
///
/// Compared ASCII-case-insensitively, so one entry covers both the JSX spelling
/// and the HTML one (`contentEditable` and `contenteditable` are the same
/// attribute; HTML attribute names are case-insensitive and `setAttribute`
/// lowercases them on an HTML element).
///
/// ## Boundary
///
/// This is React's `BOOLEANISH_STRING` set and nothing more — the attributes
/// where a JSX author writes a boolean and the HTML spec wants the *word*. It is
/// deliberately not "every enumerated attribute in HTML": `translate` takes
/// `yes`/`no` and `autocomplete` takes `on`/`off`, so a boolean there is a
/// different mistake with a different fix, and neither is what a component
/// library emits.
///
/// 🚫 **`data-*` is deliberately absent, and this is a knowing divergence from
/// React**, which stringifies `data-foo={true}` to `data-foo="true"`. Two
/// reasons: this codebase emits its own `data-albedo-link` marker *as a boolean
/// prop* and `tests/hydration_integration_tests.rs` asserts it stays bare; and
/// Radix's own convention for its `data-*` state markers is the bare form
/// (`data-disabled`), which is what a `[data-disabled]` selector and
/// `hasAttribute` both key on. Unlike aria, nothing reads a `data-*` attribute's
/// value as a tri-state, so there is no defect here to fix — only a spelling to
/// keep stable. The falsifier: an app that writes `data-x={true}` and reads
/// `el.dataset.x` gets `""` where React would give it `"true"`.
pub const ENUMERATED_BOOLEAN_ATTRIBUTES: &[&str] = &[
    // ── HTML ────────────────────────────────────────────────────────────
    "contenteditable",
    "draggable",
    "spellcheck",
    // ── SVG ─────────────────────────────────────────────────────────────
    "autoreverse",
    "externalresourcesrequired",
    "focusable",
    "preservealpha",
];

/// The `aria-` prefix, matched case-insensitively. Every ARIA state and property
/// that takes a boolean is enumerated, without exception, so the prefix is the
/// rule rather than a list of the sixty-odd names — a list would rot the moment
/// ARIA grows an attribute, and it would rot *silently*, into inert markup.
const ARIA_PREFIX: &str = "aria-";

/// Which of the two forms a boolean takes for `attribute_name`.
///
/// Takes the **already-renamed** attribute name — the output of
/// [`jsx_attribute_name`], not the JSX prop — so a prop whose spelling changes
/// on the way out is judged by what actually lands in the markup.
#[must_use]
pub fn boolean_attribute_form(attribute_name: &str) -> BooleanAttributeForm {
    if attribute_name
        .get(..ARIA_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(ARIA_PREFIX))
    {
        return BooleanAttributeForm::Enumerated;
    }
    for enumerated in ENUMERATED_BOOLEAN_ATTRIBUTES {
        if attribute_name.eq_ignore_ascii_case(enumerated) {
            return BooleanAttributeForm::Enumerated;
        }
    }
    BooleanAttributeForm::Bare
}

/// The QuickJS-side lookup, generated from [`JSX_ATTRIBUTE_RENAMES`].
///
/// Installs `globalThis.__albedo_attr_name`, which the `h` shim's attribute loop
/// calls, and `globalThis.__albedo_attr_bool_enumerated`, which it consults
/// before spelling a boolean. Generated rather than written into the prelude's
/// raw string so the two server renderers cannot disagree — which is the whole
/// contract the `h` shim's own comment states: *"the ` />` spelling is the
/// pure-Rust renderer's, so the two agree byte-for-byte rather than merely
/// parsing to the same tree."*
#[must_use]
pub fn build_jsx_attribute_table_script() -> String {
    let mut entries = String::new();
    for (prop, attribute) in JSX_ATTRIBUTE_RENAMES {
        entries.push_str(&format!("  '{prop}': '{attribute}',\n"));
    }
    let mut enumerated = String::new();
    for attribute in ENUMERATED_BOOLEAN_ATTRIBUTES {
        enumerated.push_str(&format!("  '{attribute}': true,\n"));
    }
    format!(
        "(function() {{\n\
         if (globalThis.__albedo_attr_name) {{ return; }}\n\
         var table = {{\n{entries}}};\n\
         var own = Object.prototype.hasOwnProperty;\n\
         globalThis.__albedo_attr_name = function(name) {{\n\
         \x20 return own.call(table, name) ? table[name] : name;\n\
         }};\n\
         var enumeratedBooleans = {{\n{enumerated}}};\n\
         globalThis.__albedo_attr_bool_enumerated = function(name) {{\n\
         \x20 var lower = String(name).toLowerCase();\n\
         \x20 return lower.slice(0, {aria_len}) === '{ARIA_PREFIX}'\n\
         \x20   || own.call(enumeratedBooleans, lower);\n\
         }};\n\
         }})();\n",
        aria_len = ARIA_PREFIX.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

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

    /// The defect this whole distinction exists for: `aria-expanded={true}` was
    /// a bare attribute, which is the empty string, which is *not expanded*.
    #[test]
    fn aria_booleans_are_enumerated_not_bare() {
        for name in [
            "aria-expanded",
            "aria-hidden",
            "aria-selected",
            "aria-checked",
            "aria-pressed",
            "aria-disabled",
            // Case-insensitive: an author may write `ARIA-Expanded`, and HTML
            // attribute names do not care.
            "ARIA-Expanded",
            // The prefix is a rule, not a list — an ARIA attribute nobody has
            // enumerated here still lands on the right side.
            "aria-some-future-state",
        ] {
            assert_eq!(
                boolean_attribute_form(name),
                BooleanAttributeForm::Enumerated,
                "{name} must carry the literal word"
            );
        }
    }

    #[test]
    fn real_html_boolean_attributes_stay_bare() {
        // 🚫 The two `data-*` entries are knowingly bare, unlike React: this
        // codebase emits `data-albedo-link` as a boolean prop and asserts it
        // stays bare, and Radix's own `data-*` state markers are bare too.
        for name in [
            "disabled",
            "checked",
            "hidden",
            "required",
            "readonly",
            "selected",
            "multiple",
            "open",
            "inert",
            "autofocus",
            "class",
            "id",
            "style",
            "stroke-width",
            "data-albedo-link",
            "data-state",
        ] {
            assert_eq!(
                boolean_attribute_form(name),
                BooleanAttributeForm::Bare,
                "{name} signals by presence, so a value would be noise"
            );
        }
    }

    /// The non-`aria-` enumerated attributes, in both the JSX spelling and the
    /// HTML one — `jsx_attribute_name` does not rename `contentEditable`, so the
    /// camelCase form is what reaches the decision.
    #[test]
    fn booleanish_string_attributes_are_enumerated_in_either_case() {
        for name in [
            "contentEditable",
            "contenteditable",
            "draggable",
            "spellCheck",
            "spellcheck",
            "focusable",
            "preserveAlpha",
        ] {
            assert_eq!(
                boolean_attribute_form(name),
                BooleanAttributeForm::Enumerated,
                "{name} takes the word `true`/`false`, not a bare name"
            );
        }
    }

    /// `get(..5)` on a short or multi-byte name must not panic — the prefix probe
    /// runs on every attribute of every element the server renders.
    #[test]
    fn the_prefix_probe_survives_short_and_multibyte_names() {
        for name in ["", "a", "ari", "aria", "日本語です", "aria", "é-x"] {
            let _ = boolean_attribute_form(name);
        }
        assert_eq!(boolean_attribute_form("aria"), BooleanAttributeForm::Bare);
    }

    #[test]
    fn the_enumerated_list_is_lowercase_and_free_of_the_aria_prefix() {
        for attribute in ENUMERATED_BOOLEAN_ATTRIBUTES {
            assert_eq!(
                *attribute,
                attribute.to_ascii_lowercase(),
                "{attribute} must be listed lowercase — lookups lowercase first"
            );
            assert!(
                !attribute.starts_with(ARIA_PREFIX),
                "{attribute} is already covered by the prefix rule; listing it \
                 twice is a line that can rot"
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

    /// The same argument as above, for the same reason, about the other table.
    ///
    /// Drift here is worse than drift in the rename table, because it is
    /// *invisible*: the client would keep the attribute name and only change its
    /// value, so hydration adopts the node and then quietly rewrites the aria
    /// state the server got right — or leaves the one it got wrong.
    #[test]
    fn the_client_runtime_enumerated_boolean_table_matches_this_one() {
        let client = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/albedo-client.js"
        ));
        let start = client
            .find("var ENUMERATED_BOOLEAN_ATTRIBUTES = {")
            .expect("albedo-client.js must declare ENUMERATED_BOOLEAN_ATTRIBUTES");
        let body = &client[start..];
        let end = body.find("};").expect("the table must be closed");
        let body = &body[..end];

        let mut found: BTreeSet<String> = BTreeSet::new();
        for line in body.lines().skip(1) {
            let line = line.trim().trim_end_matches(',');
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let Some((name, _)) = line.split_once(':') else {
                continue;
            };
            found.insert(
                name.trim()
                    .trim_matches('\'')
                    .trim_matches('"')
                    .to_ascii_lowercase(),
            );
        }

        let expected: BTreeSet<String> = ENUMERATED_BOOLEAN_ATTRIBUTES
            .iter()
            .map(|name| (*name).to_string())
            .collect();

        assert_eq!(
            found, expected,
            "assets/albedo-client.js's enumerated-boolean table has drifted from \
             ENUMERATED_BOOLEAN_ATTRIBUTES — the client and the server would \
             spell the same boolean prop differently on the same node"
        );
    }

    /// The `aria-` half is code in all three renderers rather than data, so the
    /// only thing that can be asserted from here is that the client still
    /// *contains* the rule. Its behaviour is proven by running the real client
    /// runtime (`tests/client_hydration.rs`).
    #[test]
    fn the_client_runtime_carries_the_aria_prefix_rule() {
        let client = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/albedo-client.js"
        ));
        assert!(
            client.contains(&format!("var ARIA_PREFIX = '{ARIA_PREFIX}'")),
            "albedo-client.js must key the prefix rule on the same `{ARIA_PREFIX}` \
             the server does"
        );
    }

    #[test]
    fn the_generated_script_carries_every_enumerated_boolean() {
        let script = build_jsx_attribute_table_script();
        for attribute in ENUMERATED_BOOLEAN_ATTRIBUTES {
            assert!(
                script.contains(&format!("'{attribute}': true")),
                "{attribute} missing from the generated enumerated-boolean table"
            );
        }
        assert!(
            script.contains(&format!("=== '{ARIA_PREFIX}'")),
            "the generated table must carry the aria prefix rule, not just the list"
        );
    }
}
