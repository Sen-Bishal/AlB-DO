//! A2 · npm dependency bundling — synthetic-package gates.
//!
//! Builds tiny on-disk `node_modules` trees, bundles them through
//! `bundle_npm_dependency`, loads the artifacts into a real `QuickJsEngine`,
//! and renders components that import from them. Covers the module-system
//! surface real packages exercise: conditional `exports` maps, named/default/
//! namespace imports, `export … from` / `export * from` re-export chains,
//! exported classes, a CommonJS dependency consumed from ESM, a JSON module,
//! and an import cycle (which must resolve CJS-style, not recurse forever).

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmDependencyBundle};
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::eval::{
    render_entry_with_bindings, CompiledProject, RenderOptions, SessionSlotView,
};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::session::SessionId;
use dom_render_compiler::runtime::slot_store::SlotStore;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    engine
}

fn load_bundle(engine: &mut QuickJsEngine, bundle: &NpmDependencyBundle) {
    for artifact in &bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .unwrap_or_else(|err| panic!("loading artifact '{}': {err}", artifact.key));
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write fixture file");
}

/// ESM package: conditional exports, internal relative imports, re-export
/// chains (`export { x } from`, `export * from`), a class export, and a
/// default export — consumed by a component via named + default imports.
#[test]
fn esm_package_with_reexport_chain_renders_through_component() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join("mathkit");

    write(
        &pkg.join("package.json"),
        r#"{ "name": "mathkit", "version": "2.0.0", "type": "module",
             "exports": { ".": { "import": "./src/index.js", "require": "./dist/index.cjs" } } }"#,
    );
    write(
        &pkg.join("src").join("index.js"),
        r#"
            export { double } from "./ops.js";
            export * from "./constants.js";
            export { Accumulator } from "./acc.js";
            import { double as d } from "./ops.js";
            export default function describe(n) { return "twice " + n + " is " + d(n); }
        "#,
    );
    write(
        &pkg.join("src").join("ops.js"),
        "export function double(n) { return n * 2; }",
    );
    write(
        &pkg.join("src").join("constants.js"),
        "export const BASE = 21; export const NAME = \"mathkit\";",
    );
    write(
        &pkg.join("src").join("acc.js"),
        r#"
            export class Accumulator {
                constructor(start) { this.total = start; }
                add(n) { this.total += n; return this; }
            }
        "#,
    );

    let bundle = bundle_npm_dependency(dir.path(), "mathkit").expect("bundle mathkit");
    assert_eq!(bundle.package_name, "mathkit");
    assert_eq!(bundle.entry_key, "npm:mathkit@2.0.0/src/index.js");

    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import describe, { double, BASE, NAME, Accumulator } from "mathkit";
        export default function App() {
            const total = new Accumulator(BASE).add(double(10)).total;
            return <p data-lib={NAME}>{describe(BASE)} | total {total}</p>;
        }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("component loads with npm imports");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("component renders");
    assert_eq!(
        out.html,
        "<p data-lib=\"mathkit\">twice 21 is 42 | total 41</p>"
    );
}

/// A CommonJS dependency consumed from an ESM entry: `module.exports`
/// interop must expose both the default and copied named exports.
#[test]
fn cjs_dependency_interops_with_esm_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join("mixed");

    write(
        &pkg.join("package.json"),
        r#"{ "name": "mixed", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./index.js" } } }"#,
    );
    write(
        &pkg.join("index.js"),
        r#"
            import legacy from "./legacy.cjs";
            import { greet } from "./legacy.cjs";
            export const viaDefault = legacy.greet("default");
            export const viaNamed = greet("named");
        "#,
    );
    write(
        &pkg.join("legacy.cjs"),
        r#"
            'use strict';
            const pad = require("./pad.cjs");
            module.exports = { greet: function(tag) { return pad("hi-" + tag); } };
        "#,
    );
    write(
        &pkg.join("pad.cjs"),
        "module.exports = function pad(s) { return '[' + s + ']'; };",
    );

    let bundle = bundle_npm_dependency(dir.path(), "mixed").expect("bundle mixed");
    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { viaDefault, viaNamed } from "mixed";
        export default function App() {
            return <span>{viaDefault}/{viaNamed}</span>;
        }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<span>[hi-default]/[hi-named]</span>");
}

/// JSON modules: the parsed value is the default export, object keys are
/// named exports.
#[test]
fn json_module_exposes_default_and_named() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join("jsonpkg");

    write(
        &pkg.join("package.json"),
        r#"{ "name": "jsonpkg", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./index.js" } } }"#,
    );
    write(
        &pkg.join("index.js"),
        r#"
            import meta from "./meta.json";
            export const label = meta.label + "@" + meta.major;
        "#,
    );
    write(&pkg.join("meta.json"), r#"{ "label": "cfg", "major": 3 }"#);

    let bundle = bundle_npm_dependency(dir.path(), "jsonpkg").expect("bundle jsonpkg");
    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { label } from "jsonpkg";
        export default function App() { return <i>{label}</i>; }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<i>cfg@3</i>");
}

/// An import cycle must resolve like Node's CJS discipline: the record is
/// published before its factory runs, so the cycle observes a
/// partially-initialized record instead of recursing forever. As in CJS, a
/// back-reference accessed **at call time through a namespace import** works
/// (the record has been filled by then); a *destructured* back-reference would
/// snapshot `undefined` — the same trade CommonJS makes.
#[test]
fn import_cycle_resolves_lazily() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join("cyclic");

    write(
        &pkg.join("package.json"),
        r#"{ "name": "cyclic", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./a.js" } } }"#,
    );
    write(
        &pkg.join("a.js"),
        r#"
            import * as b from "./b.js";
            export function fromA() { return "A"; }
            export function combined() { return fromA() + b.fromB(); }
        "#,
    );
    write(
        &pkg.join("b.js"),
        r#"
            import * as a from "./a.js";
            export function fromB() { return "B+" + a.fromA(); }
        "#,
    );

    let bundle = bundle_npm_dependency(dir.path(), "cyclic").expect("bundle cyclic");
    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { combined } from "cyclic";
        export default function App() { return <b>{combined()}</b>; }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<b>AB+A</b>");
}

/// Subpath exports: `pkg/feature` resolves through the exports map
/// independently of the root entry, and both can load side by side.
#[test]
fn subpath_export_bundles_independently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join("featured");

    write(
        &pkg.join("package.json"),
        r#"{ "name": "featured", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./root.js" },
                          "./extra": { "import": "./extra.js" } } }"#,
    );
    write(&pkg.join("root.js"), "export const where = \"root\";");
    write(&pkg.join("extra.js"), "export const where = \"extra\";");

    let root = bundle_npm_dependency(dir.path(), "featured").expect("bundle root");
    let extra = bundle_npm_dependency(dir.path(), "featured/extra").expect("bundle subpath");
    assert_eq!(root.entry_key, "npm:featured@1.0.0/root.js");
    assert_eq!(extra.entry_key, "npm:featured@1.0.0/extra.js");

    let mut engine = engine();
    load_bundle(&mut engine, &root);
    load_bundle(&mut engine, &extra);

    let component = r#"
        import { where as a } from "featured";
        import { where as b } from "featured/extra";
        export default function App() { return <u>{a}:{b}</u>; }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<u>root:extra</u>");
}

/// A package importing another package: the bare specifier inside the first
/// package's file resolves through the node_modules walk-up, and both
/// packages' records land under their own versioned keys.
#[test]
fn transitive_package_dependency_resolves() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outer = dir.path().join("node_modules").join("outer");
    let inner = dir.path().join("node_modules").join("inner");

    write(
        &outer.join("package.json"),
        r#"{ "name": "outer", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./index.js" } } }"#,
    );
    write(
        &outer.join("index.js"),
        r#"
            import { core } from "inner";
            export const wrapped = "<<" + core + ">>";
        "#,
    );
    write(
        &inner.join("package.json"),
        r#"{ "name": "inner", "version": "3.1.4", "type": "module",
             "exports": { ".": { "import": "./index.js" } } }"#,
    );
    write(&inner.join("index.js"), "export const core = \"pith\";");

    let bundle = bundle_npm_dependency(dir.path(), "outer").expect("bundle outer");
    assert!(
        bundle
            .artifacts
            .iter()
            .any(|a| a.key == "npm:inner@3.1.4/index.js"),
        "inner package files keyed under their own package+version"
    );

    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { wrapped } from "outer";
        export default function App() { return <s>{wrapped}</s>; }
    "#;
    engine
        .load_module("routes/app.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/app.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<s>&lt;&lt;pith&gt;&gt;</s>");
}

/// Slice 3 — the full production path: `CompiledProject::wrap` discovers the
/// bare import, bundles it from the project's own `node_modules`, the QuickJS
/// render preloads it, and an action handler referencing the imported
/// function executes under QuickJS with the binding seeded.
#[test]
fn compiled_project_discovers_bundles_and_dispatches_with_npm_import() {
    let dir = tempfile::tempdir().expect("tempdir");

    // The npm dependency lives next to the component, like a real project.
    let pkg = dir.path().join("node_modules").join("numlib");
    write(
        &pkg.join("package.json"),
        r#"{ "name": "numlib", "version": "1.0.0", "type": "module",
             "exports": { ".": { "import": "./index.js" } } }"#,
    );
    write(
        &pkg.join("index.js"),
        "export function double(n) { return n * 2; }",
    );

    write(
        &dir.path().join("Component.tsx"),
        r#"
            import { useState } from "react";
            import { double } from "numlib";

            export default function Counter() {
              const [n, setN] = useState(3);
              return (
                <button onClick={() => setN(double(n))}>{n}</button>
              );
            }
        "#,
    );

    let project = CompiledProject::load_from_dir(dir.path()).expect("project compiles");

    // Discovery found exactly the npm import (node_modules was not walked as
    // project components, and "react" stayed a framework binding).
    let bundles = project.npm_bundles();
    assert_eq!(bundles.len(), 1, "one npm dependency discovered");
    assert_eq!(bundles[0].specifier, "numlib");

    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");

    // QuickJS render: the component module's top-level npm import links.
    let rendered = project
        .render_entry_quickjs(
            &mut engine,
            "Component.tsx",
            &Value::Object(Default::default()),
            &slots,
        )
        .expect("quickjs render with npm import");
    assert!(
        rendered.html.contains(">3</button>"),
        "initial render shows the useState initial, got: {}",
        rendered.html
    );

    // Discover the proxy id + slot from the pure-Rust bind render, exactly
    // as bakabox would.
    let opts = RenderOptions { hook_compile: true };
    let bind = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &opts,
    )
    .expect("bind render");
    let proxy_id = bind
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::BindEvent { proxy_id, .. } => Some(proxy_id.0),
            _ => None,
        })
        .expect("BindEvent emitted");
    let slot_id = bind
        .opcodes
        .iter()
        .find_map(|op| match op {
            Instruction::SetTextRef { slot_id, .. } => Some(*slot_id),
            _ => None,
        })
        .expect("SetTextRef emitted");

    // Dispatch: `setN(double(n))` with n=3 must write 6 — `double` resolves
    // through the seeded npm import binding.
    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };
    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("quickjs action with npm import binding");

    let raw = store.read(session, slot_id).expect("slot persisted");
    let decoded: Value = serde_json::from_slice(&raw).expect("slot decodes");
    assert_eq!(
        decoded.as_f64().expect("numeric") as i64,
        6,
        "double(3) must write 6 through the npm import"
    );
}

/// The A1 follow-up this work unlocks: a project component importing another
/// project component now links at load time (the legacy `__albedo_require`
/// became a global), so parent→child composition renders under QuickJS.
#[test]
fn project_child_component_import_now_links() {
    let mut engine = engine();

    engine
        .load_module(
            "components/Badge.tsx",
            r#"export default function Badge(props) { return <em>{props.label}</em>; }"#,
        )
        .expect("child loads");
    engine
        .load_module(
            "routes/page.tsx",
            r#"
                import Badge from "components/Badge.tsx";
                export default function Page() {
                    return <div>before <Badge label="mid" /> after</div>;
                }
            "#,
        )
        .expect("parent loads with a project-module import");

    let out = engine
        .render_component("routes/page.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<div>before <em>mid</em> after</div>");
}
