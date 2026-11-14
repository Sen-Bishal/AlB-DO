//! T-0033 · Q1a slice 2 — the Design 2 fold, const shapes (R3-1), shorthand
//! predicates and const limits: what each lowers to, and what is refused where.

use super::tests::classify;
use super::Classification;
use crate::forge::query::RowBound;

fn fold(body: &str) -> Classification {
    classify(&format!(
        "import {{ invoices, guestbook }} from \"albedo/forge\";\n\
         import {{ optional }} from \"albedo/query\";\n\
         export async function load(f, o, list) {{\n{body}\n}}\n"
    ))
}

fn messages(c: &Classification) -> Vec<String> {
    c.diagnostics
        .iter()
        .map(|d| format!("{} — {}", d.at, d.message))
        .collect()
}

#[test]
fn a_fold_lowers_to_one_descriptor_with_one_optional_slot_per_if() {
    let c = fold(
        "  let q = invoices.where({ status: \"open\" });\n\
         \x20 if (f.owner) q = q.where({ owner: f.owner });\n\
         \x20 if (f.due != null) { q = q.where({ due: { lt: f.due } }); }\n\
         \x20 const rows = await q.orderBy(\"due\", \"desc\").limit(50);\n\
         \x20 return rows;",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(c.queries.len(), 1, "one descriptor for the whole fold");
    let q = &c.queries[0];
    assert_eq!(
        q.at.to_string(),
        "src/routes/probe.tsx:4:11",
        "named at the base chain's collection"
    );
    let d = &q.canonical.descriptor;
    let optional: Vec<&str> = d
        .terms
        .iter()
        .filter(|t| t.optional_slot.is_some())
        .map(|t| t.column.as_str())
        .collect();
    assert_eq!(optional.len(), 2, "{:?}", d.terms);
    assert!(optional.contains(&"owner") && optional.contains(&"due"));
    assert!(d
        .terms
        .iter()
        .any(|t| t.column == "status" && t.optional_slot.is_none()));
    assert_eq!(q.lowered.statements.len(), 4, "2 optional slots → 4 presence masks");
}

#[test]
fn a_handle_awaited_bare_or_never_used_is_still_one_query() {
    let c = fold(
        "  let q = invoices.where({ status: \"open\" });\n\
         \x20 if (f.x) q = q.where({ owner: f.x });\n\
         \x20 return await q;",
    );
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(c.queries.len(), 1);
    let c = fold("  const unused = invoices.where({ status: \"open\" }).count();");
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(c.queries.len(), 1);
}

#[test]
fn fold_shapes_outside_design_2_are_refused_naming_the_reference() {
    let base = "  let q = invoices.where({ status: \"open\" });\n";
    let cases: &[(&str, &str, u32)] = &[
        (
            "  if (f.x) q = q.where({ owner: f.x }); else q = q.where({ owner: \"me\" });\n  return await q;",
            "else",
            5,
        ),
        (
            "  for (const x of list) q = q.where({ owner: x });\n  return await q;",
            "inside a loop",
            5,
        ),
        (
            "  const a = await q.limit(5);\n  const b = await q.count();\n  return [a, b];",
            "a second time",
            6,
        ),
        ("  return send(q);", "passed to a function", 5),
        ("  return q;", "returned", 5),
        ("  return () => q.limit(5);", "captured by a closure", 5),
        (
            "  if (f.x) q = guestbook.where({ author: \"a\" });\n  return await q;",
            "reassigned",
            5,
        ),
        (
            "  function g() { q = q.where({ owner: \"me\" }); }\n  g();\n  return await q;",
            "captured by a closure",
            5,
        ),
        (
            "  if (f.x) q = q.where({ owner: f.x, due: 1 });\n  return await q;",
            "one column",
            5,
        ),
        (
            "  if (f.x) { q = q.where({ owner: f.x }); log(); }\n  return await q;",
            "reassigned",
            5,
        ),
        (
            "  const alias = q;\n  return await alias;",
            "outside the accepted shape",
            5,
        ),
        (
            "  if (f.a) q = q.where({ owner: optional(f.a) });\n  return await q;",
            "optional()",
            5,
        ),
    ];
    for (rest, expected, line) in cases {
        let body = format!("{base}{rest}");
        let c = fold(&body);
        // The refused handle lowers nothing. (A chain *assigned* to it, such as
        // `guestbook.where(…)`, is a query of its own and may lower.)
        assert!(
            c.queries.iter().all(|q| q.at.line != 4),
            "{body}\nthe refused handle lowered {:?}",
            c.queries
        );
        let messages = messages(&c);
        assert_eq!(messages.len(), 1, "{body}\n{messages:?}");
        assert!(
            messages[0].contains(expected)
                && messages[0].starts_with(&format!("src/routes/probe.tsx:{line}:")),
            "{body}\nexpected `{expected}` at line {line}, got {messages:?}"
        );
    }
}

#[test]
fn more_than_three_optional_slots_is_refused_by_the_cap() {
    let c = fold(
        "  let q = invoices.where({ status: \"open\" });\n\
         \x20 if (f.a) q = q.where({ owner: f.a });\n\
         \x20 if (f.b) q = q.where({ due: f.b });\n\
         \x20 if (f.c) q = q.where({ amount: f.c });\n\
         \x20 if (f.d) q = q.where({ status: f.d });\n\
         \x20 return await q;",
    );
    assert!(c.queries.is_empty());
    assert!(
        c.diagnostics.iter().any(|d| d.message.contains("at most 3")),
        "{:?}",
        messages(&c)
    );
}

#[test]
fn a_shorthand_predicate_names_its_column() {
    let c = fold("  const owner = f.owner;\n  return await invoices.where({ owner }).count();");
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(c.queries[0].canonical.descriptor.terms[0].column, "owner");
    // A collection in shorthand is still the collection, not a value.
    let c = fold("  return await invoices.where({ guestbook }).count();");
    assert!(
        c.diagnostics
            .iter()
            .any(|d| d.message.contains("`guestbook` is a collection")),
        "{:?}",
        messages(&c)
    );
}

#[test]
fn a_limit_may_be_a_const_bound_to_an_integer_literal() {
    let c = fold("  const PAGE = 20;\n  return await invoices.where({ status: \"open\" }).limit(PAGE);");
    assert!(c.diagnostics.is_empty(), "{:?}", messages(&c));
    assert_eq!(
        c.queries[0].canonical.descriptor.bound,
        RowBound::Limit { n: 20 }
    );
    for (decl, expected) in [
        ("let PAGE = 20;", "integer literal"),
        ("const PAGE = 2000;", "out of range"),
        ("const PAGE = f.n;", "integer literal"),
    ] {
        let c = fold(&format!(
            "  {decl}\n  return await invoices.where({{ status: \"open\" }}).limit(PAGE);"
        ));
        assert!(c.queries.is_empty(), "{decl}");
        assert!(
            c.diagnostics.iter().any(|d| d.message.contains(expected)),
            "{decl}: {:?}",
            messages(&c)
        );
    }
}

fn consts(prelude: &str, body: &str) -> Classification {
    classify(&format!(
        "import {{ invoices }} from \"albedo/forge\";\n\
         import {{ or, optional }} from \"albedo/query\";\n\
         {prelude}\n\
         export async function load(f) {{\n{body}\n}}\n"
    ))
}

#[test]
fn a_const_predicate_lowers_like_its_literal() {
    let literal = consts(
        "",
        "  return await invoices.where({ status: \"open\", due: { lt: f.before } }).count();",
    );
    let named = consts(
        "const open = { status: \"open\", due: { lt: Date.now() } };",
        "  return await invoices.where(open).count();",
    );
    assert!(named.diagnostics.is_empty(), "{:?}", messages(&named));
    assert_eq!(
        named.queries[0].canonical.id, literal.queries[0].canonical.id,
        "same shape, same id"
    );
    let in_or = consts(
        "const open = { status: \"open\" };",
        "  return await invoices.where(or(open, { owner: f.me })).count();",
    );
    assert!(in_or.diagnostics.is_empty(), "{:?}", messages(&in_or));
    assert_eq!(in_or.queries.len(), 1);
}

#[test]
fn a_const_predicate_used_any_other_way_is_refused_naming_that_use() {
    let read = "  return await invoices.where(open).count();";
    let cases: &[(&str, &str, u32)] = &[
        ("const open = { status: \"open\" };\nconst alias = open;", "outside a query", 4),
        (
            "const open = { status: \"open\" };\nObject.assign(open, { status: \"x\" });",
            "passed to a function",
            4,
        ),
        ("const open = { status: \"open\" };\nopen.status = \"x\";", "outside a query", 4),
        ("let open = { status: \"open\" };", "by name", 5),
        ("const open = { ...base };", "spread", 3),
        ("const open = { owner: optional(1) };", "optional()", 3),
    ];
    for (prelude, expected, line) in cases {
        let c = consts(prelude, read);
        assert!(c.queries.is_empty(), "{prelude}\nlowered {:?}", c.queries);
        let messages = messages(&c);
        assert!(
            messages.iter().any(|m| m.contains(expected)
                && m.starts_with(&format!("src/routes/probe.tsx:{line}:"))),
            "{prelude}\nexpected `{expected}` at line {line}, got {messages:?}"
        );
    }
}

