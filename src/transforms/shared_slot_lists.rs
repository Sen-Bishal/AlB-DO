//! B2 · shared-slot list anchor marker — a Tier-B (QuickJS) transpile pre-pass.
//!
//! For `{IDENT.map(...)}` where `IDENT` is a `useSharedSlot("topic")` binding,
//! this stamps the enclosing JSX element with `data-albedo-list-slot="topic"`.
//! That turns the list's container into a keyed-list **anchor** the client sink
//! can find at boot and register against the topic's wire slot — the seam a
//! FORGE topic write fans a `SlotDelta` into (S4), reconciling rows without a
//! reload. It is the framework-emitted version of the hand-written
//! `data-forge="…"` hint a user might otherwise reach for.
//!
//! Runs BEFORE the JSX→`h` transform, while the tree is still JSX, so the
//! stamped attribute rides through the ordinary attribute path in the `h` shim.
//!
//! Self-contained: it reads the module's own `albedo` import and its
//! `const x = useSharedSlot("t")` declarations, so the transpile caller doesn't
//! have to thread the component-level shared-slot analysis in. It intentionally
//! marks only the *direct* container of a shared-slot `.map()` — the common
//! `<ul>{items.map(...)}</ul>` shape, whose keyed `<li>` children are the rows.

use crate::forge::delta::RenderedRows;
use std::collections::HashMap;
use swc_common::{sync::Lrc, FileName, SourceMap, DUMMY_SP};
use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_ast::{
    BlockStmtOrExpr, CallExpr, Callee, Expr, Ident, IdentName, ImportSpecifier, JSXAttr,
    JSXAttrName, JSXAttrOrSpread, JSXAttrValue, JSXElement, JSXElementChild, JSXExpr, Lit,
    MemberProp, Module, ModuleDecl, ModuleExportName, ModuleItem, Pat, Str, VarDeclarator,
};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

/// Attribute a shared-slot list container is stamped with. The value is the
/// **topic**; the client derives the wire slot id with the same FNV-1a hash the
/// broadcast registry uses (`broadcast::{topic}`), so the id never has to travel
/// on the wire.
pub const LIST_SLOT_ATTR: &str = "data-albedo-list-slot";

/// Attribute carrying a row's reconciliation identity, stamped from the
/// author's `key={…}` by the two SSR renderers
/// (`runtime::eval::component::render_attrs` and the QuickJS `h` shim).
///
/// Named here, beside the anchor attribute, because the pair is one contract:
/// the anchor says "this element is a keyed list bound to topic T" and the key
/// says "this child is row K of it". [`extract_keyed_rows`] is the reader of
/// that contract, the client's `_seedRowsByKey` its mirror.
pub const ROW_KEY_ATTR: &str = "data-albedo-key";

/// Read a rendered page's keyed rows back out of it: every direct child of the
/// `topic`'s list anchor that carries a [`ROW_KEY_ATTR`], as
/// `key → the child's outer HTML`.
///
/// This is the inverse of what this module stamps, and it exists because a
/// `SlotDelta`'s payload must be *exactly* the markup SSR produces for that row
/// — not a re-derivation of it. Rendering the collection and reading the rows
/// back guarantees that by construction: there is one row template, it runs in
/// one place, and both the reload path and the delta path get their bytes from
/// it. Any scheme that re-rendered a row separately would be a second
/// implementation of the same template, and the two would only have to disagree
/// once to strand a row on the page.
///
/// Returns `None` when the anchor isn't present or the markup around it doesn't
/// parse as expected — never a partial read, since a missing row would surface
/// as a delta that silently drops it.
///
/// # Scanning assumptions
///
/// The input is **our own** generated markup, not the open web, which is what
/// makes a tag-level scan (rather than a DOM parse) the right tool: tags are
/// balanced, attribute values are always double-quoted by
/// `render_attrs`/the `h` shim, and raw-text elements (`<script>`, `<style>`)
/// never appear as list rows. The scan still handles quoted `>` inside
/// attribute values, void elements, self-closing tags, and comments, because
/// those *do* occur inside rows.
#[must_use]
pub fn extract_keyed_rows(html: &str, topic: &str) -> Option<RenderedRows> {
    let mut rows = RenderedRows::new();
    let mut cursor = find_anchor(html, topic)?;
    // Depth within the anchor: 0 means "between rows", 1+ means "inside one".
    let mut depth = 0usize;
    let mut row_start = 0usize;

    while let Some(tag) = next_tag(html, cursor) {
        cursor = tag.end;
        match tag.kind {
            TagKind::Open { void_or_self_closing } => {
                if depth == 0 {
                    if void_or_self_closing {
                        collect_row(html, tag.start, tag.end, &mut rows);
                    } else {
                        row_start = tag.start;
                        depth = 1;
                    }
                } else if !void_or_self_closing {
                    depth += 1;
                }
            }
            TagKind::Close => {
                if depth == 0 {
                    // The anchor's own closing tag: every row is accounted for.
                    return Some(rows);
                }
                depth -= 1;
                if depth == 0 {
                    collect_row(html, row_start, tag.end, &mut rows);
                }
            }
            TagKind::Ignorable => {}
        }
    }

    // Ran off the end without closing the anchor — the markup is not what this
    // reader was promised, so it reports nothing rather than a truncated list.
    None
}

/// Record one direct child of the anchor, if it carries a row key.
/// Unkeyed children (a `<li>` the author wrote outside the `.map()`, whitespace
/// wrappers) are structural, not rows, and are skipped.
fn collect_row(html: &str, start: usize, end: usize, rows: &mut RenderedRows) {
    let outer = &html[start..end];
    if let Some(key) = attr_value(outer, ROW_KEY_ATTR) {
        rows.insert(key, outer.to_string());
    }
}

/// Byte offset just past the anchor's opening tag for `topic` — where its
/// children begin.
fn find_anchor(html: &str, topic: &str) -> Option<usize> {
    let needle = format!("{LIST_SLOT_ATTR}=\"{}\"", escape_attr_value(topic));
    let hit = html.find(&needle)?;
    // Walk back to the `<` that opened this tag. The attribute value cannot
    // contain `<` (it is escaped), so the first one found is the tag's.
    let start = html[..hit].rfind('<')?;
    let tag = next_tag(html, start)?;
    // The tag we found by scanning must be the one holding the attribute; if a
    // stray `<` in text put us on a different tag, refuse rather than guess.
    (tag.start == start && tag.end > hit).then_some(tag.end)
}

/// Value of `name` in an element's opening tag, HTML-unescaped so it compares
/// equal to what `Element.getAttribute` hands the client sink.
fn attr_value(outer_html: &str, name: &str) -> Option<String> {
    let opening_end = outer_html.find('>')?;
    let opening = &outer_html[..=opening_end];
    let needle = format!("{name}=\"");
    let at = opening.find(&needle)? + needle.len();
    let end = at + opening[at..].find('"')?;
    Some(unescape_attr_value(&opening[at..end]))
}

/// `escape_attr`'s rule, for building the needle we search with. Kept minimal
/// and local: a topic is a compile-time literal, so this only ever matters if
/// one grows a character the renderer would have escaped.
fn escape_attr_value(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Inverse of the two renderers' attribute escaping. `&amp;` is unescaped last
/// so an encoded `&amp;lt;` does not decode into a `<`.
fn unescape_attr_value(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

#[derive(Debug, Clone, Copy)]
enum TagKind {
    Open { void_or_self_closing: bool },
    Close,
    /// Comment, doctype, or processing instruction — no effect on nesting.
    Ignorable,
}

#[derive(Debug, Clone, Copy)]
struct Tag {
    start: usize,
    end: usize,
    kind: TagKind,
}

/// The next tag at or after `from`, as a half-open byte range and a kind.
/// Text between tags is skipped.
fn next_tag(html: &str, from: usize) -> Option<Tag> {
    let start = from + html.get(from..)?.find('<')?;
    let rest = &html[start..];

    if rest.starts_with("<!--") {
        let end = rest.find("-->").map(|at| start + at + 3)?;
        return Some(Tag { start, end, kind: TagKind::Ignorable });
    }
    if rest.starts_with("<!") || rest.starts_with("<?") {
        let end = start + rest.find('>')? + 1;
        return Some(Tag { start, end, kind: TagKind::Ignorable });
    }

    // Find the `>` that ends this tag, skipping any inside a quoted value.
    let mut quote: Option<char> = None;
    let mut end = None;
    for (offset, ch) in rest.char_indices().skip(1) {
        match (quote, ch) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => {
                end = Some(start + offset + 1);
                break;
            }
            (None, _) => {}
        }
    }
    let end = end?;

    if rest.starts_with("</") {
        return Some(Tag { start, end, kind: TagKind::Close });
    }
    let name: String = rest[1..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .flat_map(char::to_lowercase)
        .collect();
    let self_closing = html[start..end].trim_end_matches('>').trim_end().ends_with('/');
    Some(Tag {
        start,
        end,
        kind: TagKind::Open {
            void_or_self_closing: self_closing || crate::runtime::eval::component::is_void_tag(&name),
        },
    })
}

/// Stamp every shared-slot list container in `module` with [`LIST_SLOT_ATTR`].
/// A no-op when the module imports no `albedo` `useSharedSlot`, or binds none of
/// its results to a `.map()` inside JSX.
pub fn mark_shared_slot_lists(module: &mut Module) {
    let idents = collect_shared_slot_idents(module);
    if idents.is_empty() {
        return;
    }
    module.visit_mut_with(&mut ListAnchorMarker { idents });
}

/// `local_binding -> topic` for every `const X = useSharedSlot("T")` whose
/// `useSharedSlot` resolves to the `albedo` import.
fn collect_shared_slot_idents(module: &Module) -> HashMap<String, String> {
    let Some(local_name) = use_shared_slot_local_name(module) else {
        return HashMap::new();
    };
    let mut collector = SlotDeclCollector { local_name, out: HashMap::new() };
    module.visit_with(&mut collector);
    collector.out
}

/// The local identifier `useSharedSlot` is imported as from `"albedo"`, if any.
/// Mirrors `transforms::shared_slots`' import-binding rule so a user function
/// merely named `useSharedSlot` from elsewhere is not mistaken for the hook.
fn use_shared_slot_local_name(module: &Module) -> Option<String> {
    for item in &module.body {
        let ModuleItem::ModuleDecl(ModuleDecl::Import(import)) = item else {
            continue;
        };
        if import.src.value.as_ref() != "albedo" {
            continue;
        }
        for spec in &import.specifiers {
            let ImportSpecifier::Named(named) = spec else {
                continue;
            };
            let imported = match &named.imported {
                Some(ModuleExportName::Ident(i)) => i.sym.to_string(),
                Some(ModuleExportName::Str(s)) => s.value.to_string(),
                None => named.local.sym.to_string(),
            };
            if imported == "useSharedSlot" {
                return Some(named.local.sym.to_string());
            }
        }
    }
    None
}

struct SlotDeclCollector {
    local_name: String,
    out: HashMap<String, String>,
}

impl Visit for SlotDeclCollector {
    fn visit_var_declarator(&mut self, decl: &VarDeclarator) {
        if let (Pat::Ident(name), Some(init)) = (&decl.name, &decl.init) {
            if let Some(topic) = shared_slot_topic(init, &self.local_name) {
                self.out.insert(name.id.sym.to_string(), topic);
            }
        }
        decl.visit_children_with(self);
    }
}

/// `Some(topic)` when `expr` is `<local_name>("topic")` with a string-literal arg.
fn shared_slot_topic(expr: &Expr, local_name: &str) -> Option<String> {
    let Expr::Call(CallExpr { callee: Callee::Expr(callee), args, .. }) = expr else {
        return None;
    };
    let Expr::Ident(id) = callee.as_ref() else {
        return None;
    };
    if id.sym.as_ref() != local_name {
        return None;
    }
    let Expr::Lit(Lit::Str(s)) = args.first()?.expr.as_ref() else {
        return None;
    };
    Some(s.value.to_string())
}

struct ListAnchorMarker {
    idents: HashMap<String, String>,
}

impl VisitMut for ListAnchorMarker {
    fn visit_mut_jsx_element(&mut self, el: &mut JSXElement) {
        el.visit_mut_children_with(self);

        let topic = el.children.iter().find_map(|child| {
            let JSXElementChild::JSXExprContainer(container) = child else {
                return None;
            };
            let JSXExpr::Expr(expr) = &container.expr else {
                return None;
            };
            map_over_shared_slot(expr, &self.idents)
        });

        if let Some(topic) = topic {
            if !has_attr(el, LIST_SLOT_ATTR) {
                el.opening.attrs.push(list_slot_attr(&topic));
            }
        }
    }
}

/// `Some(topic)` when `expr` is `IDENT.map(...)` and `IDENT` is a shared-slot binding.
fn map_over_shared_slot(expr: &Expr, idents: &HashMap<String, String>) -> Option<String> {
    let Expr::Call(CallExpr { callee: Callee::Expr(callee), .. }) = expr else {
        return None;
    };
    let Expr::Member(member) = callee.as_ref() else {
        return None;
    };
    let MemberProp::Ident(method) = &member.prop else {
        return None;
    };
    if method.sym.as_ref() != "map" {
        return None;
    }
    let Expr::Ident(obj) = member.obj.as_ref() else {
        return None;
    };
    idents.get(obj.sym.as_ref()).cloned()
}

/// How a shared-slot row template depends on its collection — the compile-time
/// classification that decides whether a single-record write can be answered by
/// rendering **one row** (`O(1)`) or must re-render the whole view (`O(|view|)`).
///
/// The fast path renders one row by feeding the *same* row template a singleton
/// collection `[record]` and reading the row back out — not a second renderer
/// (see [`extract_keyed_rows`]'s contract note), just the one renderer over a
/// one-element input. That is byte-identical to the whole-view render's slice of
/// the row **iff** the row's markup does not depend on anything the singleton
/// changes: the record's index, the collection's length, or the array itself.
/// This classification is exactly that proof, and it is deliberately
/// conservative — [`RowProjection::WholeView`] is the answer on any doubt,
/// because a false `PerRecord` strands a wrong row on the page permanently while
/// a false `WholeView` only forgoes the optimization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowProjection {
    /// `map(row => …)` — the row's markup is a function of its record alone. A
    /// single-record write always renders just that row.
    PerRecord,
    /// `map((row, i) => …)` — the row reads its index but not the collection's
    /// length or the array. A single-row render is correct only where the index
    /// is unchanged (a tail append or an in-place update); other writes fall back.
    PositionStable,
    /// Length-dependent, reads the collection/array directly, a shape the
    /// classifier cannot inspect (a named `.map(renderRow)` callback, a spread),
    /// or otherwise unprovable. Always re-renders the whole view.
    WholeView,
}

impl RowProjection {
    /// The more conservative of two classifications (`WholeView` wins, then
    /// `PositionStable`). Used when one topic is mapped at more than one site —
    /// across `.map()` calls in one module, or across the modules of one render
    /// plan.
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        use RowProjection::{PerRecord, PositionStable, WholeView};
        match (self, other) {
            (WholeView, _) | (_, WholeView) => WholeView,
            (PositionStable, _) | (_, PositionStable) => PositionStable,
            (PerRecord, PerRecord) => PerRecord,
        }
    }
}

/// `topic → row-projection class` for every shared-slot `.map()` in the module.
///
/// Runs the same shared-slot binding resolution as [`mark_shared_slot_lists`],
/// then classifies each `IDENT.map(callback)` it finds. A topic mapped at more
/// than one site collapses to the most conservative class (its rows are
/// ambiguous to the projector anyway).
#[must_use]
pub fn classify_shared_slot_lists(module: &Module) -> HashMap<String, RowProjection> {
    let idents = collect_shared_slot_idents(module);
    if idents.is_empty() {
        return HashMap::new();
    }
    let mut classifier = RowProjectionClassifier { idents, out: HashMap::new() };
    module.visit_with(&mut classifier);
    classifier.out
}

/// Classify a module from its source text, choosing the parser syntax by the
/// specifier's extension. The server-side render-plan builder calls this per
/// module so the [`crate::forge::RowProjector`] can report each collection's
/// class without the caller depending on swc.
///
/// A source that fails to parse yields an empty map — every collection then
/// defaults to [`RowProjection::WholeView`], the always-correct whole-view
/// render. Classification never *causes* a wrong render; at worst it forgoes the
/// single-row fast path.
#[must_use]
pub fn classify_shared_slot_lists_source(
    specifier: &str,
    source: &str,
) -> HashMap<String, RowProjection> {
    use std::path::Path;
    let syntax = match Path::new(specifier).extension().and_then(|e| e.to_str()) {
        Some("ts") => Syntax::Typescript(TsSyntax::default()),
        Some("tsx") => Syntax::Typescript(TsSyntax { tsx: true, ..Default::default() }),
        _ => Syntax::Es(EsSyntax { jsx: true, ..Default::default() }),
    };
    let cm: Lrc<SourceMap> = Default::default();
    let fm = cm.new_source_file(FileName::Custom(specifier.to_string()).into(), source.to_string());
    let mut parser = Parser::new(syntax, StringInput::from(&*fm), None);
    match parser.parse_module() {
        Ok(module) => classify_shared_slot_lists(&module),
        Err(_) => HashMap::new(),
    }
}

struct RowProjectionClassifier {
    /// `local binding -> topic`, as in [`ListAnchorMarker`].
    idents: HashMap<String, String>,
    /// `topic -> class`.
    out: HashMap<String, RowProjection>,
}

impl Visit for RowProjectionClassifier {
    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Some((topic, binding)) = map_call_binding(call, &self.idents) {
            let class = classify_row_projection(call, &binding);
            self.out
                .entry(topic)
                .and_modify(|existing| *existing = existing.min(class))
                .or_insert(class);
        }
        call.visit_children_with(self);
    }
}

/// `Some((topic, binding_ident))` when `call` is `IDENT.map(...)` and `IDENT` is
/// a shared-slot binding. The binding name is returned alongside the topic
/// because the classifier needs it: a row body that references the array
/// identifier depends on the whole view.
fn map_call_binding(
    call: &CallExpr,
    idents: &HashMap<String, String>,
) -> Option<(String, String)> {
    let Callee::Expr(callee) = &call.callee else {
        return None;
    };
    let Expr::Member(member) = callee.as_ref() else {
        return None;
    };
    let MemberProp::Ident(method) = &member.prop else {
        return None;
    };
    if method.sym.as_ref() != "map" {
        return None;
    }
    let Expr::Ident(obj) = member.obj.as_ref() else {
        return None;
    };
    let topic = idents.get(obj.sym.as_ref())?.clone();
    Some((topic, obj.sym.to_string()))
}

/// Classify one shared-slot `.map(callback)` call. `collection_binding` is the
/// array identifier the map is called on; a body that names it depends on the
/// whole collection.
fn classify_row_projection(call: &CallExpr, collection_binding: &str) -> RowProjection {
    let Some(first) = call.args.first() else {
        return RowProjection::WholeView;
    };
    // `.map(...spread)` — the callback isn't a single inspectable expression.
    if first.spread.is_some() {
        return RowProjection::WholeView;
    }

    // Pull the callback's params and collect every identifier its body uses.
    let (params, body_idents): (Vec<Pat>, IdentCollector) = match first.expr.as_ref() {
        Expr::Arrow(arrow) => {
            let mut idents = IdentCollector::default();
            match arrow.body.as_ref() {
                BlockStmtOrExpr::BlockStmt(block) => block.visit_with(&mut idents),
                BlockStmtOrExpr::Expr(expr) => expr.visit_with(&mut idents),
            }
            (arrow.params.clone(), idents)
        }
        Expr::Fn(func) => {
            let mut idents = IdentCollector::default();
            if let Some(body) = &func.function.body {
                body.visit_with(&mut idents);
            }
            let pats = func.function.params.iter().map(|p| p.pat.clone()).collect();
            (pats, idents)
        }
        // A named callback (`.map(renderRow)`), a member expr, anything we can't
        // read the body of: cannot prove PerRecord.
        _ => return RowProjection::WholeView,
    };

    // Referencing the array identifier itself is a whole-view dependency
    // (`entries.length`, `entries.find(…)`, a nested map, …).
    if body_idents.uses(collection_binding) {
        return RowProjection::WholeView;
    }

    // The 3rd map param IS the array; the 2nd is the index. A param we can't name
    // (a destructured index, a rest param) is one we can't prove unused — treat
    // its presence conservatively.
    if let Some(third) = params.get(2) {
        match pat_ident_name(third) {
            Some(name) if body_idents.uses(&name) => return RowProjection::WholeView,
            None => return RowProjection::WholeView,
            _ => {}
        }
    }
    if let Some(second) = params.get(1) {
        match pat_ident_name(second) {
            Some(name) if body_idents.uses(&name) => return RowProjection::PositionStable,
            None => return RowProjection::WholeView,
            _ => {}
        }
    }

    RowProjection::PerRecord
}

/// The name a param binds, when it is a plain identifier. Destructuring (`{id}`),
/// rest (`...xs`), and defaults return `None` — the caller decides how to treat a
/// param it cannot name.
fn pat_ident_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(binding) => Some(binding.id.sym.to_string()),
        _ => None,
    }
}

/// Collects every identifier used in expression position within a callback body,
/// so the classifier can ask whether a given name (the array binding, the index
/// param) is referenced. Member-property names (`.length`) are `IdentName`, not
/// `Ident`, so they are not collected — which is what we want: the danger is a
/// reference to the array *value* (`entries` in `entries.length`), and that obj
/// is a plain `Ident` this does collect.
#[derive(Default)]
struct IdentCollector {
    used: std::collections::HashSet<String>,
}

impl IdentCollector {
    fn uses(&self, name: &str) -> bool {
        self.used.contains(name)
    }
}

impl Visit for IdentCollector {
    fn visit_ident(&mut self, ident: &Ident) {
        self.used.insert(ident.sym.to_string());
    }
}

fn has_attr(el: &JSXElement, name: &str) -> bool {
    el.opening.attrs.iter().any(|attr| {
        matches!(
            attr,
            JSXAttrOrSpread::JSXAttr(JSXAttr { name: JSXAttrName::Ident(ident), .. })
                if ident.sym.as_ref() == name
        )
    })
}

fn list_slot_attr(topic: &str) -> JSXAttrOrSpread {
    JSXAttrOrSpread::JSXAttr(JSXAttr {
        span: DUMMY_SP,
        name: JSXAttrName::Ident(IdentName { span: DUMMY_SP, sym: LIST_SLOT_ATTR.into() }),
        value: Some(JSXAttrValue::Lit(Lit::Str(Str {
            span: DUMMY_SP,
            value: topic.into(),
            raw: None,
        }))),
    })
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// Parse a TSX fragment to a raw swc [`Module`] — the input the transpile
    /// pre-pass classifier runs on, before resolver/type-strip.
    fn parse_tsx(source: &str) -> Module {
        let cm: Lrc<SourceMap> = Default::default();
        let fm = cm.new_source_file(FileName::Custom("test.tsx".into()).into(), source.to_string());
        let mut parser = Parser::new(
            Syntax::Typescript(TsSyntax { tsx: true, ..Default::default() }),
            StringInput::from(&*fm),
            None,
        );
        parser.parse_module().expect("tsx parses")
    }

    /// Classify a `guestbook` shared-slot list whose `.map(...)` is `map_expr`.
    fn classify(map_expr: &str) -> RowProjection {
        let source = format!(
            r#"
            import {{ useSharedSlot }} from "albedo";
            export default function Component() {{
                const entries = useSharedSlot("guestbook");
                return <ul>{{{map_expr}}}</ul>;
            }}
            "#
        );
        let module = parse_tsx(&source);
        *classify_shared_slot_lists(&module)
            .get("guestbook")
            .expect("guestbook was classified")
    }

    #[test]
    fn a_record_only_row_is_per_record() {
        assert_eq!(
            classify("entries.map(entry => <li key={entry.id}>{entry.message}</li>)"),
            RowProjection::PerRecord,
        );
    }

    #[test]
    fn a_destructured_record_is_per_record() {
        assert_eq!(
            classify("entries.map(({ id, message }) => <li key={id}>{message}</li>)"),
            RowProjection::PerRecord,
        );
    }

    #[test]
    fn an_unused_index_param_stays_per_record() {
        // The classifier keys off *usage*, not the arity of the callback: a
        // second param that never appears in the body cannot change a row.
        assert_eq!(
            classify("entries.map((entry, i) => <li key={entry.id}>{entry.message}</li>)"),
            RowProjection::PerRecord,
        );
    }

    #[test]
    fn a_used_index_is_position_stable() {
        assert_eq!(
            classify("entries.map((entry, i) => <li key={entry.id}>{i}. {entry.message}</li>)"),
            RowProjection::PositionStable,
        );
    }

    #[test]
    fn reading_the_collection_length_is_whole_view() {
        assert_eq!(
            classify("entries.map(entry => <li key={entry.id}>{entries.length}</li>)"),
            RowProjection::WholeView,
        );
    }

    #[test]
    fn using_the_third_array_param_is_whole_view() {
        assert_eq!(
            classify("entries.map((entry, i, all) => <li key={entry.id}>{all.length}</li>)"),
            RowProjection::WholeView,
        );
    }

    #[test]
    fn a_named_callback_cannot_be_proven_and_is_whole_view() {
        assert_eq!(
            classify("entries.map(renderRow)"),
            RowProjection::WholeView,
        );
    }

    #[test]
    fn the_most_conservative_class_wins_across_sites() {
        // One binding mapped twice: once record-only, once index-using. The
        // topic collapses to the more conservative (PositionStable).
        let source = r#"
            import { useSharedSlot } from "albedo";
            export default function Component() {
                const entries = useSharedSlot("guestbook");
                return <div>
                    <ul>{entries.map(entry => <li key={entry.id}>{entry.message}</li>)}</ul>
                    <ol>{entries.map((entry, i) => <li key={entry.id}>{i}</li>)}</ol>
                </div>;
            }
        "#;
        let module = parse_tsx(source);
        assert_eq!(
            *classify_shared_slot_lists(&module).get("guestbook").unwrap(),
            RowProjection::PositionStable,
        );
    }

    #[test]
    fn a_module_with_no_shared_slot_map_classifies_nothing() {
        let module = parse_tsx(
            r#"
            export default function Component() {
                return <ul><li>static</li></ul>;
            }
            "#,
        );
        assert!(classify_shared_slot_lists(&module).is_empty());
    }
}

#[cfg(test)]
mod extract_tests {
    use super::*;

    /// The shape the guestbook actually renders: a stamped `<ul>` whose keyed
    /// `<li>` children are the rows.
    #[test]
    fn reads_back_the_rows_it_stamped() {
        let html = "<main><h1>Guestbook</h1>\
            <ul data-albedo-list-slot=\"guestbook\" class=\"entries\">\
            <li data-albedo-key=\"1\">ada</li>\
            <li data-albedo-key=\"2\">alan</li>\
            </ul></main>";
        let rows = extract_keyed_rows(html, "guestbook").unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows["1"], "<li data-albedo-key=\"1\">ada</li>");
        assert_eq!(rows["2"], "<li data-albedo-key=\"2\">alan</li>");
    }

    /// A row is an element tree, not a text run. Its whole subtree — including
    /// nested elements of the SAME tag — has to come back as one payload, or
    /// the client inserts a fragment of a row.
    #[test]
    fn a_row_carries_its_whole_subtree_including_same_tag_nesting() {
        let html = "<ul data-albedo-list-slot=\"t\">\
            <li data-albedo-key=\"1\"><ul><li>nested</li></ul><span>x</span></li>\
            <li data-albedo-key=\"2\">flat</li>\
            </ul>";
        let rows = extract_keyed_rows(html, "t").unwrap();

        assert_eq!(rows.len(), 2, "the nested <li> must not be mistaken for a row");
        assert_eq!(
            rows["1"],
            "<li data-albedo-key=\"1\"><ul><li>nested</li></ul><span>x</span></li>"
        );
    }

    /// A `>` inside an attribute value does not end a tag. Getting this wrong
    /// truncates the row at the first `>` in user data.
    #[test]
    fn a_quoted_angle_bracket_does_not_end_a_tag() {
        let html = "<ul data-albedo-list-slot=\"t\">\
            <li data-albedo-key=\"1\" title=\"a > b\">ada</li></ul>";
        let rows = extract_keyed_rows(html, "t").unwrap();
        assert_eq!(rows["1"], "<li data-albedo-key=\"1\" title=\"a > b\">ada</li>");
    }

    /// Void and self-closing elements have no closing tag; treating them as
    /// open would swallow every following row into one.
    #[test]
    fn void_and_self_closing_children_do_not_swallow_their_siblings() {
        let html = "<div data-albedo-list-slot=\"t\">\
            <img data-albedo-key=\"1\" src=\"a.png\">\
            <hr>\
            <p data-albedo-key=\"2\">after<br>break</p>\
            </div>";
        let rows = extract_keyed_rows(html, "t").unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows["1"], "<img data-albedo-key=\"1\" src=\"a.png\">");
        assert_eq!(rows["2"], "<p data-albedo-key=\"2\">after<br>break</p>");
    }

    #[test]
    fn comments_and_text_between_rows_are_not_rows() {
        let html = "<ul data-albedo-list-slot=\"t\">\n  <!-- rows -->\n  \
            <li data-albedo-key=\"1\">ada</li>\n  <li>unkeyed heading</li>\n</ul>";
        let rows = extract_keyed_rows(html, "t").unwrap();
        assert_eq!(rows.len(), 1, "only keyed children are rows");
        assert!(rows.contains_key("1"));
    }

    /// The key must come back the way `getAttribute` gives it to the client,
    /// or the delta names a row the sink can never match.
    #[test]
    fn a_key_is_unescaped_to_match_what_the_client_reads() {
        let html = "<ul data-albedo-list-slot=\"t\">\
            <li data-albedo-key=\"a&amp;b &lt;c&gt; &quot;d&quot;\">x</li></ul>";
        let rows = extract_keyed_rows(html, "t").unwrap();
        assert!(rows.contains_key("a&b <c> \"d\""));
    }

    #[test]
    fn only_the_named_topics_anchor_is_read() {
        let html = "<ul data-albedo-list-slot=\"other\"><li data-albedo-key=\"9\">no</li></ul>\
            <ul data-albedo-list-slot=\"t\"><li data-albedo-key=\"1\">yes</li></ul>";
        let rows = extract_keyed_rows(html, "t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows["1"], "<li data-albedo-key=\"1\">yes</li>");
    }

    /// Real markup, captured from the live forge-lab guestbook render: the
    /// anchor carries the author's own `data-forge` attribute alongside the
    /// stamped one, and each row wraps two `<span>`s. Guards the reader against
    /// the shape it actually meets, not the shape a test would invent.
    #[test]
    fn reads_the_markup_the_guestbook_really_renders() {
        let html = "<section class=\"sheet\"><ul class=\"entries\" data-forge=\"guestbook\"             data-albedo-list-slot=\"guestbook\">            <li class=\"entry\" data-albedo-key=\"1\">            <span class=\"entry-author\">ada</span>            <span class=\"entry-message\">first light</span></li>            <li class=\"entry\" data-albedo-key=\"2\">            <span class=\"entry-author\">alan</span>            <span class=\"entry-message\">the machine stirs</span></li>            </ul></section>";
        let rows = extract_keyed_rows(html, "guestbook").unwrap();

        assert_eq!(rows.len(), 2);
        assert!(rows["2"].starts_with("<li class=\"entry\" data-albedo-key=\"2\">"));
        assert!(rows["2"].ends_with("</li>"));
        assert!(rows["2"].contains("the machine stirs"));
    }

    #[test]
    fn a_missing_anchor_reads_nothing_rather_than_something() {
        assert!(extract_keyed_rows("<ul><li data-albedo-key=\"1\">ada</li></ul>", "t").is_none());
    }

    /// Truncated markup must not produce a plausible-looking partial list —
    /// that would fan out a delta that silently drops rows.
    #[test]
    fn an_unclosed_anchor_refuses_to_report_a_partial_list() {
        let html = "<ul data-albedo-list-slot=\"t\"><li data-albedo-key=\"1\">ada</li>";
        assert!(extract_keyed_rows(html, "t").is_none());
    }

    #[test]
    fn an_empty_list_is_an_empty_map_not_a_failure() {
        let rows = extract_keyed_rows("<ul data-albedo-list-slot=\"t\"></ul>", "t").unwrap();
        assert!(rows.is_empty());
    }
}
