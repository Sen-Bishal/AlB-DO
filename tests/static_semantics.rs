//! The static (Tier A) evaluator's operators, string methods and calls,
//! checked against QuickJS.
//!
//! 🔴 Until 2026-10-07:
//! - `===`, `==`, `!==` and `!=` compared the *string forms* of their operands,
//!   so `1 === "1"` was true and `[1, 2] === "12"` was true;
//! - `<` and friends compared `to_number` of each side, which is `0` for any
//!   non-numeric string, so `"b" > "a"` was false;
//! - every other binary operator (`|`, `&`, `<<`, `in`, …) and every unary one
//!   but `!` and numeric `-` evaluated to `null`;
//! - six string methods were modelled and the rest — `includes`, `split`,
//!   `replace`, `slice`, `padStart` — were `null`;
//! - a call to a function declared in the module was `null`;
//! - `if (!x) return null;` did not stop a render: an empty return read as no
//!   return, and the body ran on past its own guard.
//!
//! Every one rendered as nothing, or as the wrong thing, with the build green.
//! Two properties are pinned here, as in `static_array_methods.rs`:
//! 1. Each supported construct renders what QuickJS renders.
//! 2. Each unsupported one is an ERROR that names it, never an empty render.

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

/// Text content, one line per `<p>`: the two renderers differ in anchors and
/// attribute spelling, which is not what these tests are about.
fn lines(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    let mut tag = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            // Text between paragraphs belongs to none of them.
            '<' if tag == "/p" || tag.starts_with("main") => {
                in_tag = true;
                tag.clear();
                line.clear();
            }
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' => {
                in_tag = false;
                if tag.starts_with("/p") {
                    let text = std::mem::take(&mut line);
                    if !text.trim().is_empty() {
                        out.push(text.trim().to_string());
                    }
                }
            }
            c if in_tag => tag.push(c),
            c => line.push(c),
        }
    }
    out
}

/// Every supported construct, each on its own labelled line. QuickJS's harness
/// loads the entry alone, so everything it needs is declared in it.
const PROBE: &str = r#"
const items = [{ n: 3, t: "a" }, { n: 1, t: "b" }];
const tags = ["x", "y"];
const word = "Hello, World";
const emoji = "a😀b";
const none = null;

function slugify(s) {
  return s.toLowerCase().split(" ").join("-");
}
const shout = (s) => s.toUpperCase() + "!";
function classify(n) {
  if (n > 2) return "big";
  if (n === 0) return "zero";
  return "small";
}

function Gate({ open }) {
  if (!open) return null;
  return <b>shown</b>;
}

export default function Probe() {
  return (
    <main>
      <p>strict:{[1 === "1", 1 !== "1", "1" === "1", null === null, [1, 2] === "1,2"].join(",")}</p>
      <p>loose:{[1 == "1", 0 == "", null == 0, null == false, true == 1, [1, 2] == "1,2", 2 != "2"].join(",")}</p>
      <p>order:{["b" > "a", "10" < "9", 10 < 9, "abc" >= 0, "abc" < 0, 2 <= 2, "B" < "a"].join(",")}</p>
      <p>bits:{[5 | 2, 5 & 4, 5 ^ 1, 1 << 4, -16 >> 2, -1 >>> 28, ~5, ~~3.7].join(",")}</p>
      <p>unary:{[-"5", +"42", +true, -true, typeof 1, typeof "s", typeof items, typeof missingGlobal, typeof items[9], typeof items[0].zz, typeof Math, typeof slugify, typeof none?.x].join(",")}</p>
      <p>in:{["t" in items[0], "zz" in items[0], 0 in tags, 5 in tags].join(",")}</p>
      <p>concat:{`${tags}`}|{"v" + tags}|{[1] + [2]}|{1 + 2 + "3"}</p>
      <p>math:{[Math.round(-2.5), Math.round(2.5), Math.round(0.49999999999999994), Math.max(...[3, 9, 4]), Math.min(4, ...tags.map((t) => t.length)), Math.sign(-3)].join(",")}</p>
      <p>fixed:{[(2.5).toFixed(0), (0.125).toFixed(2), (1.005).toFixed(2), (-2.5).toFixed(0), (1.45).toFixed(1), (10).toFixed(2)].join(",")}</p>
      <p>search:{[word.includes("World"), word.includes("o", 9), word.startsWith("Hell"), word.startsWith("World", 7), word.endsWith("d"), word.endsWith("Hello", 5), word.indexOf("o"), word.indexOf("o", 5), word.lastIndexOf("o"), word.lastIndexOf("o", 5), word.indexOf("zz")].join(",")}</p>
      <p>cut:{[word.slice(-5), word.slice(0, 5), word.slice(3, 1), word.substring(7, 0), word.substr(-5, 3), word.at(-1), word.charAt(1), word.charAt(99), word[4]].join("|")}</p>
      <p>split:{[word.split(", ").join("/"), "a,b,,c".split(",").length, "abc".split("").join("/"), word.split(" ", 1).length, "".split(",").length, word.split().length].join("|")}</p>
      <p>replace:{[word.replace("o", "0"), word.replaceAll("o", "0"), "a-b".replace("-", "$&$&"), "x".replace("x", "$$"), "ab".replace("b", "[$`|$']"), "ab".replace("b", (m, i) => m.toUpperCase() + i), "abc".replaceAll("", "-"), "a1".replace("1", "$1")].join("|")}</p>
      <p>pad:{["7".padStart(3, "0"), "ab".padEnd(5, "xy"), "ab".padStart(1), "ab".repeat(3), " pad ".trim(), word.concat("!", 1), "ÉCOLE".toLowerCase()].join("|")}</p>
      <p>units:{[emoji.length, emoji.slice(1, 3), emoji[0], emoji.indexOf("b")].join("|")}</p>
      <p>spread:{[[...tags, "z"].join(""), { ...items[0], t: "q" }.t, { ...items[0] }.n, [..."hi"].length, [1, , 3].length, { ["k" + 1]: "v" }.k1].join("|")}</p>
      <p>objects:{[Object.entries({ a: 1 }).map((e) => e[0] + e[1]).join(""), Object.keys(tags).join(""), Array.from({ length: 3 }, (_, i) => i * 2).join(","), Array.isArray(tags), Number.isInteger(2)].join("|")}</p>
      <p>optional:{items?.map((it) => it.t).join("")}|{none?.map((x) => x) ?? "nil"}</p>
      <p>helpers:{[slugify("Hello Big World"), shout("hey"), classify(3), classify(0), classify(1)].join("|")}</p>
      <p>parse:{[parseInt("42px"), parseInt("0x1F"), parseInt("101", 2), parseFloat("3.5e2kg"), parseFloat(".5"), encodeURIComponent("a b&c/é"), Number("12"), (255).toString(16)].join("|")}</p>
      <p>gate:[<Gate open={false} />][<Gate open={true} />]</p>
      <p>blockmap:{items.map((it) => {
        if (it.n < 2) return null;
        return <i>{it.t}</i>;
      })}</p>
    </main>
  );
}
"#;

/// What QuickJS renders, written out — so an engine regression cannot make
/// both sides agree on nothing.
const EXPECTED: [&str; 22] = [
    "strict:false,true,true,true,false",
    "loose:true,true,false,false,true,true,false",
    "order:true,true,false,false,false,true,true",
    "bits:7,4,4,16,-4,15,-6,3",
    "unary:-5,42,1,-1,number,string,object,undefined,undefined,undefined,object,function,undefined",
    "in:true,false,true,false",
    "concat:x,y|vx,y|12|33",
    "math:-2,3,0,9,1,-1",
    "fixed:3,0.13,1.00,-3,1.4,10.00",
    "search:true,false,true,true,true,true,4,8,8,4,-1",
    "cut:World|Hello||Hello, |Wor|d|e||o",
    "split:Hello/World|4|a/b/c|1|1|1",
    "replace:Hell0, World|Hell0, W0rld|a--b|$|a[a|]|aB1|-a-b-c-|a$1",
    "pad:007|abxyx|ab|ababab|pad|Hello, World!1|école",
    "units:4|😀|a|3",
    "spread:xyz|q|3|2|3|v",
    "objects:a1|01|0,2,4|true|true",
    "optional:ab|nil",
    "helpers:hello-big-world|HEY!|big|zero|small",
    "parse:42|31|5|350|0.5|a%20b%26c%2F%C3%A9|12|ff",
    // The early-return guard: `Gate` stops at `return null`, and a block-bodied
    // map callback stops at its own — before the fix these were `[leaked]`-
    // shaped: the body ran on past the guard.
    "gate:[][shown]",
    "blockmap:a",
];

#[test]
fn operators_string_methods_and_calls_render_what_quickjs_renders() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Probe.tsx", PROBE);
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");

    let evaluated = lines(&render_static(&project, "Probe.tsx").expect("static render succeeds"));
    let reference = lines(&render_quickjs(&project, "Probe.tsx"));

    // Line by line, so a divergence names the construct rather than dumping
    // two pages of text.
    for (ours, theirs) in evaluated.iter().zip(&reference) {
        assert_eq!(ours, theirs, "the static renderer diverged from QuickJS");
    }
    assert_eq!(evaluated.len(), reference.len(), "{evaluated:#?}\n{reference:#?}");
    for expected in EXPECTED {
        assert!(evaluated.iter().any(|line| line == expected), "missing `{expected}` in {evaluated:#?}");
    }
}

fn refusal(body: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Probe.tsx", body);
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    format!(
        "{:#}",
        render_static(&project, "Probe.tsx").expect_err("the static render must refuse, not render empty")
    )
}

/// Each construct the evaluator does not model is an error that names it.
#[test]
fn unmodelled_constructs_are_refused_by_name() {
    let cases: &[(&str, &str)] = &[
        (r#"<p>{"Hello".normalize()}</p>"#, "`.normalize()` on a string"),
        (r#"<p>{"Hello".replace(/l/g, "L")}</p>"#, "regular expression `/l/g`"),
        (r#"<p>{"b".localeCompare("a")}</p>"#, "`.localeCompare()`"),
        (r#"<p>{"a".match("a")}</p>"#, "`.match()`, which takes a regular expression"),
        (r#"<p>{String([] instanceof Array)}</p>"#, "`instanceof`"),
        (r#"<p>{Math.random()}</p>"#, "`Math.random()`"),
        (r#"<p>{Math.hypot(3, 4)}</p>"#, "`Math.hypot()`"),
        (r#"<p>{(1234).toLocaleString()}</p>"#, "`.toLocaleString()` on a number"),
        (r#"<p>{1 / 0}</p>"#, "produces Infinity"),
        (r#"<p>{"abc" * 2}</p>"#, "produces NaN"),
        (r#"<p>{format(1)}</p>"#, "calling `format()`"),
        (r#"<p>{new Map().size}</p>"#, "`new Map()`"),
        (r#"<p>{[1][0].foo()}</p>"#, "`.foo()` on a number"),
        (r#"<p>{({ a: 1 }).foo()}</p>"#, "calling `.foo()` on an object"),
        (r#"<p>{"x".repeat(-1)}</p>"#, "RangeError"),
        (r#"<p>{[...5].length}</p>"#, "not iterable"),
        (r#"<p>{"a".split(null).length}</p>"#, "null-or-undefined argument"),
    ];
    for (jsx, expected) in cases {
        let err = refusal(&format!("export default function Probe() {{\n  return {jsx};\n}}\n"));
        assert!(err.contains(expected), "`{jsx}` should be refused naming {expected:?}, got: {err}");
    }
}

/// `typeof` answers where the evaluator knows — an undeclared name, a missing
/// key — and refuses a value that is null or undefined, because it holds both
/// as one value and `typeof` is exactly the question of which.
#[test]
fn typeof_of_a_null_or_undefined_value_is_refused_not_guessed() {
    let err = refusal(
        r#"const none = null;
export default function Probe() {
  return <p>{typeof none}</p>;
}"#,
    );
    assert!(err.contains("`typeof` of a value that is null or undefined"), "{err}");
}

/// A refusal inside a LOCAL constant used to be swallowed into `null`; it is
/// now raised when the local is read, as a module constant's is.
#[test]
fn a_refusal_inside_a_local_constant_is_raised_where_it_is_read() {
    let err = refusal(
        r#"export default function Probe() {
  const label = "Hello".normalize();
  return <p>{label}</p>;
}"#,
    );
    assert!(err.contains("`label` could not be evaluated"), "{err}");
    assert!(err.contains("`.normalize()` on a string"), "{err}");

    // …and one that is never read costs nothing.
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "Probe.tsx",
        r#"export default function Probe() {
  const unused = "Hello".normalize();
  return <p>fine</p>;
}"#,
    );
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    assert!(render_static(&project, "Probe.tsx").unwrap().contains("fine"));
}

/// A helper that fails names both itself and what stopped it.
#[test]
fn a_failing_helper_names_the_call_and_the_cause() {
    let err = refusal(
        r#"function label(s) {
  return s.normalize();
}
export default function Probe() {
  return <p>{label("x")}</p>;
}"#,
    );
    assert!(err.contains("`.normalize()` on a string"), "{err}");
    assert!(err.contains("in `label()`"), "{err}");
}

/// A function calling itself forever stops with a message, not a stack
/// overflow that takes the build with it.
#[test]
fn unbounded_recursion_in_a_helper_is_stopped() {
    let err = refusal(
        r#"function forever(n) {
  return forever(n + 1);
}
export default function Probe() {
  return <p>{forever(0)}</p>;
}"#,
    );
    assert!(err.contains("nested more than 32 calls deep"), "{err}");
}

/// A helper imported from another project module is called like a local one.
#[test]
fn a_helper_imported_from_a_project_module_is_called() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "format.ts",
        "export function money(n) {\n  return \"$\" + n.toFixed(2);\n}\nexport default (s) => s.trim();\n",
    );
    write(
        dir.path(),
        "Probe.tsx",
        r#"import trim, { money } from "./format";
export default function Probe() {
  return <p>{money(2.5)}|{trim("  x  ")}</p>;
}"#,
    );
    let project = CompiledProject::load_from_dir(dir.path()).expect("compiles");
    let html = render_static(&project, "Probe.tsx").expect("renders");
    assert!(html.contains("$2.50|x"), "{html}");
}
