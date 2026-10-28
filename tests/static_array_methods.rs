//! The static (Tier A) evaluator's array methods, checked against QuickJS.
//!
//! 🔴 Until 2026-09-29 the evaluator modelled `map` and `join` and nothing else:
//! every other array method — `find`, `filter`, `findIndex`, `flatMap` —
//! evaluated to `null`, which rendered as nothing, and a module constant that
//! failed to evaluate became `null` the same way. A docs layout built on
//! `pages.findIndex(...)` shipped an empty pager with the build green.
//!
//! Two properties are pinned here:
//! 1. Each supported method gives the markup QuickJS gives — the engine that
//!    runs real JavaScript is the reference, not this file's idea of it.
//! 2. An unsupported method, a function passed by name, and a read of a
//!    constant that failed to evaluate are ERRORS that name the call, never
//!    an empty render.

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

fn slots() -> SessionSlotView {
    SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()))
}

fn write(root: &Path, rel: &str, source: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn render_static(project: &CompiledProject, entry: &str) -> anyhow::Result<String> {
    render_entry_with_bindings(
        project,
        entry,
        &json!({}),
        &slots(),
        &RenderOptions { hook_compile: false },
    )
    .map(|output| output.html)
}

fn render_quickjs(project: &CompiledProject, entry: &str) -> String {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("engine init");
    project
        .render_entry_quickjs(&mut engine, entry, &json!({}), &slots())
        .expect("quickjs render succeeds")
        .html
}

/// Text content only: the two renderers differ in anchors and attribute
/// spelling, which is not what these tests are about.
fn text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

const DATA: &str = r#"
export const pages = [
  { href: "/a", title: "A", tags: ["x", "y"], n: 3 },
  { href: "/b", title: "B", tags: ["y"], n: 1 },
  { href: "/c", title: "C", tags: [], n: 2 },
];
export const sections = [{ pages: [1, 2] }, { pages: [3] }];
"#;

#[test]
fn supported_array_methods_render_what_quickjs_renders() {
    // QuickJS's test harness loads the entry alone, without its relative
    // imports, so the parity render inlines the data; the imported shape — the
    // one `src/docs.ts` has — is rendered by the static path below and must match.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.ts", DATA);
    write(dir.path(), "Probe.tsx", &format!("{DATA}{PROBE}"));
    write(dir.path(), "Imported.tsx", &format!("import {{ pages, sections }} from \"./data\";
{PROBE}"));

    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    let evaluated = text(&render_static(&project, "Probe.tsx").expect("static render succeeds"));
    let reference = text(&render_quickjs(&project, "Probe.tsx"));
    let imported = text(&render_static(&project, "Imported.tsx").expect("static render succeeds"));

    assert_eq!(evaluated, reference, "the static renderer diverged from QuickJS");
    assert_eq!(imported, evaluated, "an imported constant rendered differently from a local one");
    // And the reference is what it looks like — so an engine regression cannot
    // make both sides agree on nothing.
    for expected in EXPECTED {
        assert!(evaluated.contains(expected), "missing `{expected}` in: {evaluated}");
    }
}

const PROBE: &str = r#"
const flat = sections.flatMap((section) => section.pages);

export default function Probe() {
  const at = pages.findIndex((page) => page.href === "/b");
  const current = at >= 0 ? pages[at] : null;
  const block = pages.filter((page) => {
    if (page.n > 1) {
      return true;
    }
    return false;
  });
  return (
    <main>
      <p>at:{at} current:{current ? current.title : "none"}</p>
      <p>miss:{pages.findIndex((page) => page.href === "/zzz")}</p>
      <p>find:{pages.find((page) => page.n === 2).title}</p>
      <p>findLast:{pages.findLast((page) => page.tags.includes("y")).title}</p>
      <p>findLastIndex:{pages.findLastIndex((page) => page.n > 5)}</p>
      <p>filter:{block.map((page) => page.title).join("+")}</p>
      <p>some:{String(pages.some((page) => page.n > 2))} every:{String(pages.every((page) => page.n > 2))}</p>
      <p>includes:{String(["a", "b"].includes("b"))} indexOf:{[1, 2, 3].indexOf(3)} lastIndexOf:{[1, 2, 1].lastIndexOf(1)}</p>
      <p>at:{pages.at(-1).title} slice:{pages.slice(1).map((page) => page.title).join("")} slice2:{[1, 2, 3, 4].slice(-3, -1).join(",")}</p>
      <p>concat:{[1].concat([2, 3], 4).join(",")} flat:{[1, [2, [3]]].flat().length} flatMap:{flat.join(",")}</p>
      <p>reduce:{pages.reduce((sum, page) => sum + page.n, 0)} reduceRight:{["a", "b", "c"].reduceRight((acc, s) => acc + s)}</p>
      <p>sorted:{pages.toSorted((a, b) => a.n - b.n).map((page) => page.title).join("")} reversed:{pages.toReversed().map((page) => page.title).join("")}</p>
      <p>third:{pages.filter((page, i, all) => i === all.length - 1).map((page) => page.title).join("")}</p>
    </main>
  );
}
"#;

const EXPECTED: [&str; 13] = [
        "at:1 current:B",
        "miss:-1",
        "find:C",
        "findLast:B",
        "findLastIndex:-1",
        "filter:A+C",
        "some:true every:false",
        "includes:true indexOf:2 lastIndexOf:2",
        "at:C slice:BC slice2:2,3",
        "concat:1,2,3,4 flat:3 flatMap:1,2,3",
        "reduce:6 reduceRight:cba",
        "sorted:BCA reversed:CBA",
    "third:C",
];

fn refusal(component: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "data.ts", DATA);
    write(dir.path(), "Probe.tsx", component);
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    format!(
        "{:#}",
        render_static(&project, "Probe.tsx").expect_err("the static render must refuse, not render empty")
    )
}

#[test]
fn an_unsupported_array_method_is_refused_by_name() {
    let err = refusal(
        r#"export default function Probe() {
  return <p>{[3, 1].entries()}</p>;
}"#,
    );
    assert!(err.contains("`.entries()` on an array is not supported"), "{err}");
}

#[test]
fn a_mutating_array_method_is_refused_with_its_copying_form() {
    let err = refusal(
        r#"export default function Probe() {
  return <p>{[3, 1].sort().join(",")}</p>;
}"#,
    );
    assert!(err.contains("`.sort()` mutates the array"), "{err}");
    assert!(err.contains("toSorted"), "{err}");
}

#[test]
fn a_callback_passed_by_name_is_refused_for_map_and_the_rest() {
    let map = refusal(
        r#"import { pages } from "./data";
const title = (page) => page.title;
export default function Probe() {
  return <p>{pages.map(title).join(",")}</p>;
}"#,
    );
    assert!(map.contains("`.map()` needs an inline arrow"), "{map}");

    let filter = refusal(
        r#"import { pages } from "./data";
const big = (page) => page.n > 1;
export default function Probe() {
  return <p>{pages.filter(big).length}</p>;
}"#,
    );
    assert!(filter.contains("`.filter()` needs an inline arrow"), "{filter}");
}

/// The constant fails; READING it raises that failure, by name, in the module
/// that reads it and across an import — instead of rendering as nothing.
#[test]
fn reading_a_constant_that_failed_to_evaluate_raises_its_reason() {
    let local = refusal(
        r#"const pages = [1, 2].entries();
export default function Probe() {
  return <p>{pages}</p>;
}"#,
    );
    assert!(local.contains("`pages` could not be evaluated"), "{local}");
    assert!(local.contains("`.entries()` on an array is not supported"), "{local}");

    let imported = {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "data.ts", "export const broken = [1].entries();\n");
        write(
            dir.path(),
            "Probe.tsx",
            r#"import { broken } from "./data";
export default function Probe() {
  return <p>{ { broken }.broken }</p>;
}"#,
        );
        let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
        format!("{:#}", render_static(&project, "Probe.tsx").expect_err("refused"))
    };
    assert!(imported.contains("`broken` could not be evaluated"), "{imported}");
}

/// A constant that fails but is never read costs nothing — `export const join =
/// action(...)` style constants beside a component must not break its render.
#[test]
fn a_constant_that_failed_but_is_never_read_does_not_break_the_render() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Probe.tsx",
        r#"const unused = [1].entries();
export default function Probe() {
  return <p>fine</p>;
}"#,
    );
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    assert!(render_static(&project, "Probe.tsx").unwrap().contains("fine"));
}
