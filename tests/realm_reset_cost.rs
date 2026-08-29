//! What does confinement actually cost? — the number TODO 10.0 is missing.
//!
//! 10.0 rejects "a realm per request" on the grounds that it "kills the warm
//! arena/interning design". That reads QuickJS's ownership backwards: the
//! **arena, atom table and shape table belong to the `Runtime`**; the
//! **intrinsics and globals belong to the `Context`**. Dropping only the context
//! discards precisely the mutable surface a package can poison and keeps
//! precisely the expensive warm state.
//!
//! So the objection is not "it is impossible", it is "it is too slow" — and
//! nobody had measured it. This does.
//!
//! Reported, never asserted on a threshold: a timing assert on shared CI
//! hardware is a flake generator, and the decision this informs is a human one.
//! Run it and read it:
//!
//! ```text
//! cargo test --release --test realm_reset_cost -- --ignored --nocapture
//! ```
//!
//! ⚠️ **Release only.** A debug build measures rustc's inlining choices, not
//! QuickJS.

#![cfg(feature = "forge")]

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::time::{Duration, Instant};

const SPEC: &str = "Component.tsx";

/// A component with no npm at all — the floor.
const PLAIN: &str = r#"
export default function Component(props) {
  const items = [1, 2, 3, 4, 5];
  return (
    <div className="card">
      <h1>{props.title || "hello"}</h1>
      <ul>{items.map((n) => <li key={n}>row {n}</li>)}</ul>
    </div>
  );
}
"#;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn report(label: &str, samples: &[Duration]) {
    let mut sorted: Vec<f64> = samples.iter().copied().map(ms).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let total: f64 = sorted.iter().sum();
    let mean = total / sorted.len() as f64;
    let median = sorted[sorted.len() / 2];
    let p95 = sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)];
    println!(
        "  {label:<44} mean {mean:>8.3} ms   median {median:>8.3} ms   p95 {p95:>8.3} ms   n={}",
        sorted.len()
    );
}

fn warm_engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    engine
        .load_module_with_spec(SPEC, PLAIN, Some(SPEC))
        .expect("module loads");
    // Render once so the arena's warmup interning has happened.
    engine
        .render_component_with_host(SPEC, "{}", "")
        .expect("warm render");
    engine
}

/// The four costs, separated. The interesting one is C minus A.
#[ignore = "a benchmark; run explicitly with --release"]
#[test]
fn what_a_fresh_realm_costs() {
    const N: usize = 60;

    println!("\n=== 10.0 · the price of confinement (no npm) ===\n");

    // ---- A · today's model: reuse the realm, just render -------------------
    let mut engine = warm_engine();
    let mut render_only = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        render_only.push(t.elapsed());
    }
    report("A · render only (today)", &render_only);

    // ---- B · reset the realm, nothing else ---------------------------------
    let mut reset_only = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine.reset_realm().expect("reset");
        reset_only.push(t.elapsed());
        engine
            .load_module_with_spec(SPEC, PLAIN, Some(SPEC))
            .expect("reload");
    }
    report("B · reset_realm (context + prelude)", &reset_only);

    // ---- C · the real per-request cost under confinement -------------------
    let mut confined = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine.reset_realm().expect("reset");
        engine
            .load_module_with_spec(SPEC, PLAIN, Some(SPEC))
            .expect("reload");
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        confined.push(t.elapsed());
    }
    report("C · reset + reload + render (confined)", &confined);

    // ---- D · the reading 10.0 assumed: a whole new engine -------------------
    let mut cold = Vec::with_capacity(N.min(20));
    for _ in 0..N.min(20) {
        let t = Instant::now();
        let mut fresh = QuickJsEngine::new();
        fresh
            .init(&BootstrapPayload::default())
            .expect("engine init");
        fresh
            .load_module_with_spec(SPEC, PLAIN, Some(SPEC))
            .expect("module");
        fresh
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        cold.push(t.elapsed());
    }
    report("D · whole new engine (new Runtime too)", &cold);

    println!(
        "\n  → confinement overhead = C − A. D is what 'a realm per request'\n    \
         costs if you throw away the Runtime as well.\n"
    );
}

/// The same question with a real npm dependency tree loaded, because module
/// re-registration — not the prelude — is what should dominate.
#[ignore = "reads the external corpus at C:/Development/albedo-corpus; run with --release"]
#[test]
fn what_a_fresh_realm_costs_with_npm() {
    use dom_render_compiler::bundler::client_npm::server_shake_options;
    use dom_render_compiler::bundler::npm::bundle_npm_dependency;

    let root = std::path::Path::new("C:/Development/albedo-corpus/shadcn-taxonomy");
    if !root.join("node_modules/@radix-ui/react-dialog").is_dir() {
        println!("SKIPPED — corpus not installed");
        return;
    }

    const DIALOG: &str = r#"
    import * as D from "@radix-ui/react-dialog";
    export default function Component() {
      return (<D.Root><D.Trigger>Open</D.Trigger>
        <D.Portal><D.Content><D.Title>Hi</D.Title></D.Content></D.Portal></D.Root>);
    }
    "#;

    let bundle = bundle_npm_dependency(root, "@radix-ui/react-dialog", &server_shake_options())
        .expect("bundles");
    let chunks = bundle.artifacts.len();

    let install = |engine: &mut QuickJsEngine| {
        for artifact in &bundle.artifacts {
            engine
                .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
                .expect("artifact");
        }
        engine
            .load_module_with_spec(SPEC, DIALOG, Some(SPEC))
            .expect("component");
    };

    println!("\n=== 10.0 · the price of confinement (Radix Dialog, {chunks} chunks) ===\n");

    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    install(&mut engine);
    engine
        .render_component_with_host(SPEC, "{}", "")
        .expect("warm render");

    const N: usize = 30;

    let mut render_only = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        render_only.push(t.elapsed());
    }
    report("A · render only (today)", &render_only);

    let mut reset_only = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine.reset_realm().expect("reset");
        reset_only.push(t.elapsed());
        install(&mut engine);
    }
    report("B · reset_realm alone", &reset_only);

    let mut reload_only = Vec::with_capacity(N);
    for _ in 0..N {
        engine.reset_realm().expect("reset");
        let t = Instant::now();
        install(&mut engine);
        reload_only.push(t.elapsed());
    }
    report("B2 · re-register modules alone", &reload_only);

    let mut confined = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        engine.reset_realm().expect("reset");
        install(&mut engine);
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        confined.push(t.elapsed());
    }
    report("C · reset + reload + render (confined)", &confined);

    println!(
        "\n  → if B2 dominates, the fix is bytecode (`Module::write`/`Module::load`,\n    \
         both present in rquickjs 0.9) rather than abandoning confinement.\n"
    );
}
