//! T-0033 · Q1a slice 2 — the emitted JS keeps every expression where the author
//! wrote it (Design 2, R3-1, R3-6, R4-4, Acceptance 2–3, Falsifiers 4–5).
//!
//! Each test rewrites a plain-JS fixture with [`rewrite_source`], strips its
//! imports, and runs it in QuickJS beside stub `__albedo_q_*` helpers that record
//! every call. The stubs return fresh, immutable handles, so a test sees exactly
//! which values were read, in what order, how often, and which slots were set.
//! Q1a defines no real `__albedo_q_*` (Design 8): these stubs exist only here.

use super::rewrite_source;
use super::tests::schemas;

const STUBS: &str = r#"
var log = [];
var reads = {};
function read(name, value) { reads[name] = (reads[name] || 0) + 1; log.push("read " + name); return value; }
function __albedo_q_new(id, slots, values) {
  log.push("new");
  return Object.freeze({ id: id, slots: slots, values: values, sets: Object.freeze([]) });
}
function __albedo_q_set(h, opt, slots, values) {
  log.push("set " + opt);
  return Object.freeze({ id: h.id, slots: h.slots, values: h.values, sets: Object.freeze(h.sets.concat([[opt, slots, values]])) });
}
function __albedo_q_run(h, slots, values) {
  log.push("run");
  return { handle: h, slots: slots, values: values };
}
"#;

fn rewrite(source: &str) -> String {
    rewrite_source("src/routes/probe.js", source, &schemas())
        .unwrap_or_else(|diagnostics| panic!("refused: {diagnostics:?}"))
}

/// Rewrite `source`, drop its import lines, and evaluate it followed by `tail`,
/// which yields a string (or a promise of one).
fn run(source: &str, tail: &str) -> String {
    let rewritten = rewrite(source);
    let body: Vec<&str> = rewritten
        .lines()
        .filter(|line| !line.trim_start().starts_with("import "))
        .collect();
    let script = format!("{STUBS}\n{}\n{tail}", body.join("\n"));
    let runtime = rquickjs::Runtime::new().expect("runtime");
    let context = rquickjs::Context::full(&runtime).expect("context");
    context.with(|ctx| {
        let value: rquickjs::Value = ctx
            .eval(script.as_str())
            .unwrap_or_else(|err| panic!("eval failed: {err:?}\n{script}"));
        let value = match value.as_promise() {
            Some(promise) => promise
                .finish::<rquickjs::Value>()
                .unwrap_or_else(|err| panic!("promise failed: {err:?}\n{script}")),
            None => value,
        };
        value
            .as_string()
            .and_then(|s| s.to_string().ok())
            .unwrap_or_else(|| panic!("tail did not yield a string\n{script}"))
    })
}

const IMPORTS: &str = "import { invoices } from \"albedo/forge\";\nimport { optional } from \"albedo/query\";\n";

#[test]
fn falsifier_4a_values_are_read_when_the_handle_is_made_not_when_it_runs() {
    let out = run(
        &format!(
            "{IMPORTS}function page() {{\n  let owner = \"a\";\n  let q = invoices.where({{ owner }});\n  owner = \"b\";\n  return q.limit(50);\n}}\n"
        ),
        "JSON.stringify(page().handle.values)",
    );
    assert_eq!(out, r#"["a"]"#);
}

#[test]
fn falsifier_4b_a_getter_guard_is_read_twice_and_inline_optional_once() {
    let out = run(
        &format!(
            "{IMPORTS}function guarded(o) {{\n  let q = invoices.where({{ status: \"open\" }});\n  if (o.x) q = q.where({{ owner: o.x }});\n  return q.limit(5);\n}}\nfunction inline(o) {{\n  return invoices.where({{ owner: optional(o.y) }}).limit(5);\n}}\n"
        ),
        "var o = { get x() { return read('x', 'ada'); }, get y() { return read('y', 'bo'); } };\n\
         guarded(o); inline(o);\n\
         JSON.stringify(reads)",
    );
    assert_eq!(out, r#"{"x":2,"y":1}"#);
}

#[test]
fn falsifier_5_each_guard_sets_the_slot_exactly_when_js_says_so() {
    let guards = ["X", "X != null", "X !== undefined", "X !== null"];
    for guard in guards {
        let source = format!(
            "{IMPORTS}function run(X) {{\n  let q = invoices.where({{ status: \"open\" }});\n  if ({guard}) q = q.where({{ owner: X }});\n  return q.limit(5);\n}}\nfunction truth(X) {{ return ({guard}) ? 1 : 0; }}\n"
        );
        let out = run(
            &source,
            "JSON.stringify([[run(null).handle.sets.length, truth(null)], [run(undefined).handle.sets.length, truth(undefined)]])",
        );
        let expected = match guard {
            "X" | "X != null" => "[[0,0],[0,0]]",
            "X !== undefined" => "[[1,1],[0,0]]",
            _ => "[[0,0],[1,1]]",
        };
        assert_eq!(out, expected, "guard `{guard}`");
    }
}

#[test]
fn an_await_between_making_and_running_keeps_the_handle() {
    let out = run(
        &format!(
            "{IMPORTS}async function page(f) {{\n  let q = invoices.where({{ status: f.s }});\n  await Promise.resolve();\n  f.s = \"changed\";\n  if (f.o) q = q.where({{ owner: f.o }});\n  const r = await q.limit(5);\n  return JSON.stringify([r.handle.values, r.handle.sets.length]);\n}}\n"
        ),
        "page({ s: \"open\", o: \"ada\" })",
    );
    assert_eq!(out, r#"[["open"],1]"#);
}

#[test]
fn a_throwing_argument_leaves_the_previous_handle_and_runs_nothing() {
    let out = run(
        &format!(
            "{IMPORTS}function boom() {{ throw new Error(\"boom\"); }}\nfunction page(flag) {{\n  let q = invoices.where({{ status: \"open\" }});\n  if (flag) q = q.where({{ owner: boom() }});\n  return q.limit(5);\n}}\n"
        ),
        "var threw = false; try { page(true); } catch (e) { threw = true; }\nJSON.stringify([threw, log])",
    );
    assert_eq!(out, r#"[true,["new"]]"#);
}

#[test]
fn two_concurrent_invocations_keep_their_own_handles() {
    let out = run(
        &format!(
            "{IMPORTS}async function page(owner, gate) {{\n  let q = invoices.where({{ owner }});\n  await gate;\n  if (owner === \"b\") q = q.where({{ status: \"x\" }});\n  const r = await q.limit(5);\n  return [r.handle.values, r.handle.sets.length];\n}}\n"
        ),
        "var release; var gate = new Promise(function (r) { release = r; });\n\
         var a = page(\"a\", gate); var b = page(\"b\", gate); release();\n\
         Promise.all([a, b]).then(function (rs) { return JSON.stringify(rs); })",
    );
    assert_eq!(out, r#"[[["a"],0],[["b"],1]]"#);
}

#[test]
fn r4_4_two_bounds_evaluate_in_source_order_and_bind_to_their_sides() {
    for written in ["{ lt: read(\"g\", 20), gt: read(\"f\", 10) }", "{ gt: read(\"f\", 10), lt: read(\"g\", 20) }"] {
        let source = format!(
            "{IMPORTS}function page() {{\n  return invoices.where({{ due: {written} }}).count();\n}}\n"
        );
        let out = run(
            &source,
            "var h = page().handle;\nvar bound = {}; h.slots.forEach(function (slot, i) { bound[slot] = h.values[i]; });\nJSON.stringify([log.filter(function (l) { return l.indexOf('read') === 0; }), bound])",
        );
        // Canonical params: the lower bound (`gt`) is p0, the upper (`lt`) p1,
        // whatever order they were written in; reads follow the source.
        let reads = if written.starts_with("{ lt") { r#"["read g","read f"]"# } else { r#"["read f","read g"]"# };
        assert_eq!(out, format!(r#"[{reads},{{"0":10,"1":20}}]"#), "{written}");
    }
}

#[test]
fn acceptance_2_two_sites_writing_keys_in_different_orders_share_an_id_and_map_their_own_slots() {
    let out = run(
        &format!(
            "{IMPORTS}function a() {{ return invoices.where({{ status: read(\"s1\", \"open\"), owner: read(\"o1\", \"ada\") }}).count(); }}\nfunction b() {{ return invoices.where({{ owner: read(\"o2\", \"bo\"), status: read(\"s2\", \"done\") }}).count(); }}\n"
        ),
        "function bind(h) { var out = {}; h.slots.forEach(function (slot, i) { out[slot] = h.values[i]; }); return out; }\n\
         var x = a().handle, y = b().handle;\n\
         JSON.stringify([x.id === y.id, bind(x), bind(y), log.filter(function (l) { return l.indexOf('read') === 0; })])",
    );
    // `owner` sorts before `status` canonically: p0 = owner, p1 = status.
    assert_eq!(
        out,
        r#"[true,{"0":"ada","1":"open"},{"0":"bo","1":"done"},["read s1","read o1","read o2","read s2"]]"#
    );
}

#[test]
fn r3_1_a_const_predicate_is_read_at_the_query_site_and_its_initializer_runs_once() {
    let out = run(
        &format!(
            "{IMPORTS}const open = {{ status: read(\"init\", \"open\") }};\nfunction page() {{ return invoices.where(open).count(); }}\n"
        ),
        "page(); page();\nJSON.stringify([reads, page().handle.values])",
    );
    assert_eq!(out, r#"[{"init":1},["open"]]"#);
}

#[test]
fn r3_1_an_initializer_that_throws_leaves_no_handle() {
    let out = run(
        &format!(
            "{IMPORTS}function page(f) {{\n  const pred = {{ owner: f.boom() }};\n  return invoices.where(pred).count();\n}}\n"
        ),
        "var threw = false; try { page({ boom: function () { throw new Error('x'); } }); } catch (e) { threw = true; }\nJSON.stringify([threw, log])",
    );
    assert_eq!(out, r#"[true,[]]"#);
}

#[test]
fn a_query_nested_in_another_querys_argument_is_rewritten_too() {
    let rewritten = rewrite(&format!(
        "{IMPORTS}async function page(id) {{\n  return await invoices.where({{ owner: (await invoices.get(id)).owner }}).count();\n}}\n"
    ));
    assert_eq!(rewritten.matches("__albedo_q_new(").count(), 2, "{rewritten}");
    assert!(!rewritten.contains("invoices.get("), "{rewritten}");
    assert!(!rewritten.contains("invoices.where("), "{rewritten}");
}

#[test]
fn a_module_with_a_refused_shape_does_not_rewrite() {
    let refused = rewrite_source(
        "src/routes/probe.js",
        "import { invoices } from \"albedo/forge\";\nconst x = invoices;\n",
        &schemas(),
    );
    assert!(refused.is_err());
}

#[test]
fn legacy_reads_are_left_byte_for_byte() {
    let source = "import { useSharedSlot } from \"albedo\";\nimport { messages } from \"albedo/forge\";\nexport default function C({ params }) {\n  const rows = useSharedSlot(messages.where({ room: params.id }));\n  return rows;\n}\n";
    assert_eq!(rewrite(source), source);
}
