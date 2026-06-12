//! A2 · npm dependency bundling — real-package gates (zod, date-fns).
//!
//! These run against an actual `node_modules` tree at
//! `target/npm-fixture/node_modules` (create it with
//! `cd target/npm-fixture && npm install --no-save zod date-fns`). When the
//! tree is absent — e.g. on CI without node — each test **skips loudly**
//! rather than failing, so the synthetic gates in `npm_bundle.rs` remain the
//! always-on coverage and these are the ground-truth check against the two
//! packages Gate 2 names.

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
use std::path::PathBuf;
use std::sync::Arc;

fn fixture_root() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("npm-fixture");
    if root.join("node_modules").is_dir() {
        Some(root)
    } else {
        eprintln!(
            "SKIP: real-package fixture missing at {} (run `npm install --no-save zod date-fns` there)",
            root.display()
        );
        None
    }
}

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

/// `import { z } from "zod"` — the Gate 1 verification line. A component
/// builds a schema, parses a valid value, and renders a `safeParse` failure
/// message for an invalid one.
#[test]
fn zod_schema_validates_inside_component_render() {
    let Some(root) = fixture_root() else { return };

    let bundle = bundle_npm_dependency(&root, "zod").expect("bundle zod");
    assert_eq!(bundle.package_name, "zod");
    eprintln!(
        "zod {} bundled: {} artifacts, entry {}",
        bundle.package_version,
        bundle.artifacts.len(),
        bundle.entry_key
    );

    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { z } from "zod";
        const User = z.object({ name: z.string(), age: z.number().int().min(0) });
        export default function App(props) {
            const ok = User.parse({ name: props.name, age: props.age });
            const bad = User.safeParse({ name: 42, age: -1 });
            return <div data-valid={String(bad.success)}>{ok.name} is {ok.age}</div>;
        }
    "#;
    engine
        .load_module("routes/zod_app.tsx", component)
        .expect("component with zod import loads");
    let out = engine
        .render_component("routes/zod_app.tsx", r#"{"name":"ada","age":36}"#)
        .expect("component with zod renders");
    assert_eq!(out.html, "<div data-valid=\"false\">ada is 36</div>");
}

/// A thrown `ZodError` from an invalid `parse` surfaces as a loud render
/// error, not a silent fallback.
#[test]
fn zod_parse_failure_is_loud() {
    let Some(root) = fixture_root() else { return };

    let bundle = bundle_npm_dependency(&root, "zod").expect("bundle zod");
    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { z } from "zod";
        export default function App() {
            const n = z.number().parse("not a number");
            return <span>{n}</span>;
        }
    "#;
    engine
        .load_module("routes/boom.tsx", component)
        .expect("loads");
    let err = engine
        .render_component("routes/boom.tsx", "{}")
        .expect_err("invalid parse must throw loudly");
    let message = err.to_string();
    assert!(
        message.contains("expected number") || message.contains("invalid_type"),
        "error should carry zod's message, got: {message}"
    );
}

/// Root `date-fns` import: format/addDays through the full re-export index
/// (the heaviest graph here — hundreds of reachable files).
#[test]
fn date_fns_formats_through_root_import() {
    let Some(root) = fixture_root() else { return };

    let bundle = bundle_npm_dependency(&root, "date-fns").expect("bundle date-fns");
    eprintln!(
        "date-fns {} bundled: {} artifacts",
        bundle.package_version,
        bundle.artifacts.len()
    );

    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { addDays, format } from "date-fns";
        export default function App() {
            const due = addDays(new Date(2026, 5, 11), 30);
            return <time>{format(due, "yyyy-MM-dd")}</time>;
        }
    "#;
    engine
        .load_module("routes/dates.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/dates.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<time>2026-07-11</time>");
}

/// The whole production loop with real zod: `CompiledProject::wrap` discovers
/// and bundles the dependency, the QuickJS render preloads it, and an action
/// handler validates through `z` (seeded as an npm import binding). This is
/// the Gate 1 verification line — "TSX with `import { z } from "zod"` and an
/// action handler" — running end-to-end.
#[test]
fn compiled_project_action_handler_validates_with_zod() {
    let Some(root) = fixture_root() else { return };

    // Symlink-free: point the project at the real node_modules by nesting the
    // project dir inside the fixture root (the resolver walks upward).
    let project_dir = root.join("proj-zod-action");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    std::fs::write(
        project_dir.join("Component.tsx"),
        r#"
            import { useState } from "react";
            import { z } from "zod";

            export default function Quantity() {
              const [qty, setQty] = useState(1);
              return (
                <button onClick={() => {
                  const next = z.number().int().min(1).max(99).parse(qty + 1);
                  setQty(next);
                }}>{qty}</button>
              );
            }
        "#,
    )
    .expect("write component");

    let project = CompiledProject::load_from_dir(&project_dir).expect("project compiles");
    assert!(
        project.npm_bundles().iter().any(|b| b.specifier == "zod"),
        "zod must be discovered and bundled at wrap time"
    );

    let store = Arc::new(SlotStore::new());
    let session = SessionId::random();
    let slots = SessionSlotView::new(session, store.clone());
    let mut engine = engine();

    let rendered = project
        .render_entry_quickjs(
            &mut engine,
            "Component.tsx",
            &Value::Object(Default::default()),
            &slots,
        )
        .expect("quickjs render with zod import");
    assert!(rendered.html.contains(">1</button>"), "initial qty renders");

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

    let envelope = ActionEnvelope {
        action_id: proxy_id,
        event_kind: 0,
        payload: Vec::new(),
    };
    project
        .invoke_action_quickjs(&mut engine, &envelope, &slots)
        .expect("zod-validating handler dispatches");

    let raw = store.read(session, slot_id).expect("slot persisted");
    let decoded: Value = serde_json::from_slice(&raw).expect("slot decodes");
    assert_eq!(
        decoded.as_f64().expect("numeric") as i64,
        2,
        "z.parse(qty + 1) must validate and write 2"
    );

    std::fs::remove_dir_all(&project_dir).ok();
}

/// Subpath import (`date-fns/addDays`) — resolves through the exports map's
/// per-function entries and pulls a much smaller graph than the root.
#[test]
fn date_fns_subpath_import_resolves() {
    let Some(root) = fixture_root() else { return };

    let bundle = bundle_npm_dependency(&root, "date-fns/addDays").expect("bundle subpath");
    eprintln!(
        "date-fns/addDays bundled: {} artifacts",
        bundle.artifacts.len()
    );

    let mut engine = engine();
    load_bundle(&mut engine, &bundle);

    let component = r#"
        import { addDays } from "date-fns/addDays";
        export default function App() {
            const d = addDays(new Date(2026, 0, 31), 1);
            return <span>{d.getMonth() + 1}/{d.getDate()}</span>;
        }
    "#;
    engine
        .load_module("routes/sub.tsx", component)
        .expect("loads");
    let out = engine
        .render_component("routes/sub.tsx", "{}")
        .expect("renders");
    assert_eq!(out.html, "<span>2/1</span>");
}
