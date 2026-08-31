//! SANDGATE · Gate 2.1 — *does confinement actually close the leak?*
//!
//! Gate 1 proved `rebuild_realm` survives (sound, ~0 KB persistent growth per
//! rebuild). It did **not** prove the primitive fixes anything. This file runs
//! each scenario from `tests/quickjs_realm_isolation.rs` a second time, with a
//! realm rebuild inserted where a server's request boundary would be, and
//! records which ones close.
//!
//! ## Why this is a new file rather than an inversion of the old one
//!
//! `quickjs_realm_isolation.rs` describes **production**, and production does
//! not rebuild the realm — `EnginePool::with_engine` returns the worker to the
//! idle stack untouched. Inverting those assertions now would assert a property
//! the shipped server does not have. They stay true until the boundary is
//! wired; this file measures the primitive on its own.
//!
//! ## 🔴 Gate 2.1 as written is wrong: only TWO of the four can move
//!
//! The checklist says *"the four tests must FAIL"*. Two of them assert facts
//! that hold in **any** realm, fresh or poisoned, because they never cross a
//! request boundary at all:
//!
//! * `the_global_parameter_is_a_redundant_alias` — `globalThis` is an intrinsic.
//!   A brand-new context has one too.
//! * `the_realms_intrinsics_and_host_globals_are_all_writable` — a *fresh* realm
//!   is exactly as writable as a used one. Recycling is not freezing.
//!
//! Confinement is a **temporal** boundary: it bounds how long a poisoning
//! lasts, not whether one is possible. The two tests it can move are the two
//! that read *across* renders. See [`a_rebuild_does_not_freeze_anything`] and
//! [`a_rebuild_does_not_take_globalthis_away`], which assert the unchanged
//! outcome deliberately so the limit is pinned rather than assumed.
//!
//! Run:
//! ```text
//! cargo test --test sandgate_gate2 -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "forge")]

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmDependencyBundle, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine, RuntimeResult};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::{HandlerEffect, HandlerInvocation};
use std::path::Path;

const VICTIM: &str = r#"export default function V(props) {
    return <b data-secret={props.secret}>{props.secret}</b>;
}"#;

const READBACK: &str = r#"export default function R() {
    return <pre>{(globalThis.__stolen || []).join(" ~ ")}</pre>;
}"#;

const SECRET: &str = "alice-private-token";

fn server_options() -> ShakeOptions {
    dom_render_compiler::bundler::client_npm::server_shake_options()
}

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("init");
    engine
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// A bundled one-file package, kept alive so it can be registered **again**
/// after a rebuild — which is what `EnginePool::install_npm_bundles` does to
/// every engine, and what a request boundary would have to redo.
struct Package {
    _dir: tempfile::TempDir,
    bundle: NpmDependencyBundle,
    route: String,
    route_src: String,
}

fn package(name: &str, body: &str) -> Package {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join(name);
    write(
        &pkg.join("package.json"),
        &format!(r#"{{ "name": "{name}", "version": "1.0.0", "main": "./index.js" }}"#),
    );
    write(&pkg.join("index.js"), body);
    let bundle = bundle_npm_dependency(dir.path(), name, &server_options()).expect("bundle");
    Package {
        _dir: dir,
        bundle,
        route: format!("routes/{name}.tsx"),
        route_src: format!(
            r#"import {{ tag }} from "{name}";
               export default function A() {{ return <i>{{tag}}</i>; }}"#
        ),
    }
}

/// Register the package's artifacts and its importing route. Fallible so it can
/// run **inside** `rebuild_realm`'s closure, which is the only place a
/// re-registration is arena-correct (SANDGATE G1: bracketing the context alone
/// still leaked 16.7 KB per rebuild).
fn register(engine: &mut QuickJsEngine, pkg: &Package) -> RuntimeResult<()> {
    for artifact in &pkg.bundle.artifacts {
        engine.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)?;
    }
    engine.load_module(&pkg.route, &pkg.route_src)
}

/// Run the package body by rendering the component that imports it.
fn run_package(engine: &mut QuickJsEngine, pkg: &Package) -> String {
    engine
        .render_component(&pkg.route, "{}")
        .expect("attacker renders")
        .html
}

// ─────────────────────────────────────────────────────────────────────────────
// The two that confinement CAN close — both read across a request boundary.
// ─────────────────────────────────────────────────────────────────────────────

const PEEKER: &str = r#"
if (!globalThis.__stolen) {
  globalThis.__stolen = [];
  var realH = globalThis.h;
  globalThis.h = function () {
    try { globalThis.__stolen.push(JSON.stringify(arguments[1])); } catch (e) {}
    return realH.apply(this, arguments);
  };
}
module.exports = { tag: 'installed' };
"#;

/// Counterpart to `a_package_can_read_a_later_renders_props`.
///
/// The control half matters as much as the assertion: **inside one request the
/// leak stays open, and that is correct.** A package runs with the requesting
/// principal's authority, so seeing that request's props is not an escalation.
/// What confinement has to buy is that request 2 cannot read request 1.
#[test]
fn a_rebuild_closes_the_cross_request_props_leak() {
    let mut engine = engine();
    let peeker = package("peeker", PEEKER);

    // ── request 1 ──────────────────────────────────────────────────────────
    register(&mut engine, &peeker).expect("register");
    run_package(&mut engine, &peeker); // installs the wrapper on `h`
    engine
        .load_module("routes/victim.tsx", VICTIM)
        .expect("victim");
    engine
        .render_component("routes/victim.tsx", &format!(r#"{{"secret":"{SECRET}"}}"#))
        .expect("victim renders");
    engine
        .load_module("routes/readback.tsx", READBACK)
        .expect("readback");

    let within_request = engine
        .render_component("routes/readback.tsx", "{}")
        .expect("readback renders")
        .html;
    assert!(
        within_request.contains(SECRET),
        "CONTROL FAILED — the leak was supposed to be open before the rebuild. \
         Without this the assertion below proves nothing. Got: {within_request}"
    );

    // ── the request boundary: discard the realm, re-register everything ────
    engine
        .rebuild_realm(|e| {
            register(e, &peeker)?;
            e.load_module("routes/victim.tsx", VICTIM)?;
            e.load_module("routes/readback.tsx", READBACK)
        })
        .expect("rebuild_realm");

    // ── request 2 ──────────────────────────────────────────────────────────
    let after = engine
        .render_component("routes/readback.tsx", "{}")
        .expect("readback renders")
        .html;
    assert!(
        !after.contains(SECRET),
        "🔴 CONFINEMENT DOES NOT HOLD: request 1's props survived a realm rebuild. \
         Got: {after}"
    );
}

const FORGER: &str = r#"
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
"#;

fn forged_write(engine: &mut QuickJsEngine) -> bool {
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
    outcome.effects.iter().any(|effect| {
        matches!(effect, HandlerEffect::ForgeAppend { collection, .. } if collection == "albedo_users")
    })
}

/// Is the attacker's `JSON.stringify` patch installed in the realm right now?
///
/// ⚠️ **This observable replaced `forged_write` in the two forge tests, and the
/// reason is the whole of SANDGATE-B.** They used to ask *"did a forged effect
/// reach the server"*, because that is what the patch bought. It buys nothing
/// now — the effect channel no longer routes through the realm's `JSON`
/// (`runtime::confinement::build_sealed_intrinsics_script`) — so a forged write
/// is unobservable through the effects, and both tests would have passed for a
/// reason with nothing to do with confinement. Asking about the **patch** keeps
/// them measuring what they were written to measure.
fn patch_is_live(engine: &mut QuickJsEngine) -> bool {
    let spec = "routes/__probe.tsx";
    let src = "export default function P() { return <pre>{String(JSON.stringify([1]).indexOf('albedo_users') !== -1)}</pre>; }";
    engine.load_module(spec, src).expect("probe loads");
    engine
        .render_component(spec, "{}")
        .expect("probe renders")
        .html
        .contains("true")
}

/// Register only the package's artifacts — **not** the route that imports it.
///
/// An npm artifact registers a CJS factory into `__ALBEDO_MODULES`; the body
/// does not run until something `require`s it. That distinction is the whole
/// difference between the next two tests, so it gets its own helper rather than
/// living as a flag.
fn register_artifacts_only(engine: &mut QuickJsEngine, pkg: &Package) -> RuntimeResult<()> {
    for artifact in &pkg.bundle.artifacts {
        engine.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)?;
    }
    Ok(())
}

/// A victim route that **imports the package** — i.e. the package is one of its
/// dependencies, which is the only reason a package is ever installed.
const VICTIM_IMPORTING: &str = r#"import { tag } from "forger";
export default function V(props) {
    return <b data-tag={tag} data-secret={props.secret}>{props.secret}</b>;
}"#;

/// Counterpart to `a_package_can_forge_a_durable_write_into_someone_elses_handler`,
/// **half one**: the patch itself does not survive the rebuild.
///
/// Nothing re-imports the package in request 2, so its body never runs and
/// `JSON.stringify` is the fresh intrinsic. This is the property confinement is
/// supposed to buy — and it does buy it.
#[test]
fn a_rebuild_clears_the_forge_when_the_package_is_not_reimported() {
    let mut engine = engine();
    let forger = package("forger", FORGER);

    register(&mut engine, &forger).expect("register");
    run_package(&mut engine, &forger);

    assert!(
        patch_is_live(&mut engine),
        "CONTROL FAILED — the patch was supposed to be installed before the rebuild."
    );

    // The boundary a server would run: npm artifacts back on the engine, and
    // only the modules this request needs. The attacker's route is not one.
    engine
        .rebuild_realm(|e| {
            register_artifacts_only(e, &forger)?;
            e.load_module("routes/victim.tsx", VICTIM)
        })
        .expect("rebuild_realm");

    assert!(
        !patch_is_live(&mut engine),
        "🔴 the patched `JSON.stringify` survived a realm rebuild even though \
         nothing re-imported the package."
    );
}

/// **Half two — and this is the finding.**
///
/// 🔴 Confinement does nothing here. A package is installed *because something
/// imports it*, so at a request boundary the victim route pulls it back in and
/// the poison is reinstalled — by the re-registration itself — before the
/// handler runs. The forge does not need to survive the rebuild; it only needs
/// to be re-applied inside the request it attacks.
///
/// 🔑 The distinction the gate actually draws is **not** "before vs after a
/// rebuild". It is **stateful vs stateless**: confinement erases an attacker's
/// accumulated *data* (see [`a_rebuild_closes_the_cross_request_props_leak`],
/// where `__stolen` comes back empty) and erases nothing about their *code*,
/// which re-runs on import every single request.
///
/// So this stays open, and closing it is SANDGATE-B — the effect stream needs
/// provenance, because `bridge::lower_effect` still dispatches on `kind` alone.
#[test]
fn a_rebuild_does_not_help_when_the_victim_imports_the_package() {
    let mut engine = engine();
    let forger = package("forger", FORGER);

    register(&mut engine, &forger).expect("register");
    run_package(&mut engine, &forger);
    assert!(
        patch_is_live(&mut engine),
        "CONTROL FAILED — the patch was supposed to be installed before the rebuild."
    );

    engine
        .rebuild_realm(|e| {
            register_artifacts_only(e, &forger)?;
            e.load_module("routes/victim.tsx", VICTIM_IMPORTING)
        })
        .expect("rebuild_realm");
    engine
        .render_component("routes/victim.tsx", &format!(r#"{{"secret":"{SECRET}"}}"#))
        .expect("victim renders");

    assert!(
        patch_is_live(&mut engine),
        "the patch stopped being re-applied when the victim imports the package —          if that is a real change rather than a load-order accident, say why here."
    );

    // ── the two-layer result, pinned in one place ─────────────────────────
    //
    // Confinement did NOT help: the patch is live again, put back by the
    // victim's own import, inside the request it attacks. What changed is that
    // the patch no longer reaches anything — SANDGATE-B moved the effect
    // channel off the realm's `JSON` entirely (see `tests/sandgate_gate4.rs`).
    // Both facts sit together so nobody later reads "gate 2 row 3 closed" as
    // "confinement closed it".
    assert!(
        !forged_write(&mut engine),
        "🔴 SANDGATE-B regressed: a re-applied `JSON.stringify` patch forged a          durable write again."
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The two that confinement CANNOT close. Asserted as unchanged, on purpose.
// ─────────────────────────────────────────────────────────────────────────────

const MUTATOR: &str = r#"
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
"#;

/// 🔑 **The limit of the whole approach, pinned as a test.**
///
/// A rebuilt realm is exactly as writable as the one it replaced. Confinement
/// bounds *how long* a poisoning lasts; it does not prevent one. Anything that
/// needs un-poisonable intrinsics needs freezing, which is a different
/// mechanism and is not in SANDGATE-A.
///
/// If this ever starts failing, someone added freezing — invert it then.
#[test]
fn a_rebuild_does_not_freeze_anything() {
    let mut engine = engine();
    let mutator = package("mutator", MUTATOR);

    engine
        .rebuild_realm(|e| register(e, &mutator))
        .expect("rebuild_realm");

    let html = run_package(&mut engine, &mutator);
    for surface in ["arrayPrototype", "jsonStringify", "hostH", "moduleTable"] {
        assert!(
            html.contains(&format!(r#""{surface}":true"#)),
            "{surface} is no longer writable in a rebuilt realm — freezing landed \
             somewhere. Invert this test. Got: {html}"
        );
    }
}

const REACH: &str = r#"
var viaParam = (typeof global !== 'undefined');
var viaIntrinsic = (typeof globalThis !== 'undefined');
var viaFunction = false;
try { viaFunction = (Function('return this')() === globalThis); } catch (e) {}
module.exports = { tag: JSON.stringify({ viaParam: viaParam, viaIntrinsic: viaIntrinsic, viaFunction: viaFunction }) };
"#;

/// The realm is still reachable three ways after a rebuild — as it must be, in
/// any conforming JS realm. Recorded so gate 2.1's "all four must fail" is not
/// mistaken for an achievable target.
#[test]
fn a_rebuild_does_not_take_globalthis_away() {
    let mut engine = engine();
    let reach = package("reach", REACH);

    engine
        .rebuild_realm(|e| register(e, &reach))
        .expect("rebuild_realm");

    let html = run_package(&mut engine, &reach);
    assert!(html.contains(r#""viaIntrinsic":true"#), "{html}");
    assert!(html.contains(r#""viaFunction":true"#), "{html}");
}
