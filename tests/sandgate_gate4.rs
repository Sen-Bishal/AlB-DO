//! SANDGATE · Gate 4 — **SANDGATE-B, the effect channel's integrity.**
//!
//! Gate 2 closed two of its three live rows and left the third open, and the
//! open one was the one that mattered:
//!
//! > a package patches `JSON.stringify`; the victim route imports the package,
//! > so the patch is re-applied *inside* the request it attacks, before the
//! > handler runs; the handler's epilogue serialises the effect list through
//! > exactly that function, and the server executes what comes back.
//!
//! Confinement cannot close it. Confinement erases an attacker's accumulated
//! **data**; the attacker's **code** re-runs on import every request. So gate 2
//! promoted SANDGATE-B from optional to necessary, and this file is the gate on
//! it.
//!
//! ## What is being asserted
//!
//! | # | property | mechanism |
//! |---|---|---|
//! | 1 | a patched `JSON.stringify` can no longer inject an effect | pristine `stringify` + null-prototype records + concatenated assembly |
//! | 2 | a `toJSON` hook refuses the whole pass, loudly | the integrity probe, in an envelope built by concatenation so it cannot report on itself |
//! | 3 | the sealed holder cannot be replaced, redefined or deleted | `writable:false, configurable:false` + `Object.freeze` |
//! | 4 | a package's factory cannot reach the effect builtins at all | they are `const`s inside the handler IIFE, never globals |
//! | 5 | provenance does identify the module whose body is running | the linker's `enterModule`/`exitModule` bracket |
//!
//! 🔑 **Row 4 is the finding, and it reframes what SANDGATE-B is for.** The
//! direct-call vector — a package simply calling `append` — was never open,
//! because the builtins are lexically scoped to the handler and are not on
//! `globalThis`. Every real forging path went through **serialisation**, which
//! is why rows 1–3 are the fix and row 5 is a diagnostic rather than a defence.
//! Building a capability check on provenance would have been machinery guarding
//! a door that has no handle on the outside.
//!
//! Run:
//! ```text
//! cargo test --test sandgate_gate4 -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "forge")]

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmDependencyBundle, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine, RuntimeResult};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::{HandlerEffect, HandlerInvocation};
use std::path::Path;

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

struct Package {
    _dir: tempfile::TempDir,
    bundle: NpmDependencyBundle,
    route: String,
    route_src: String,
}

/// Bundle a one-file package plus a route that imports it — the only shape in
/// which a package is ever installed.
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

fn register(engine: &mut QuickJsEngine, pkg: &Package) -> RuntimeResult<()> {
    for artifact in &pkg.bundle.artifacts {
        engine.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)?;
    }
    engine.load_module(&pkg.route, &pkg.route_src)
}

/// Render the route that imports the package, which is what runs its body.
fn run_package(engine: &mut QuickJsEngine, pkg: &Package) -> String {
    engine
        .render_component(&pkg.route, "{}")
        .expect("attacker renders")
        .html
}


/// Read a JS expression out of the realm the way gate 2 does — by rendering it
/// through a throwaway route — rather than by adding an `eval` door to
/// `QuickJsEngine` that only tests would ever walk through.
fn probe(engine: &mut QuickJsEngine, expr: &str) -> String {
    let spec = "routes/__probe.tsx";
    let src = format!("export default function P() {{ return <pre>{{String({expr})}}</pre>; }}");
    engine.load_module(spec, &src).expect("probe module loads");
    let html = engine
        .render_component(spec, "{}")
        .expect("probe renders")
        .html;
    html.trim_start_matches("<pre>")
        .trim_end_matches("</pre>")
        .to_string()
}

fn probe_bool(engine: &mut QuickJsEngine, expr: &str) -> bool {
    probe(engine, expr) == "true"
}

fn invocation<'a>(env: &'a serde_json::Map<String, serde_json::Value>) -> HandlerInvocation<'a> {
    HandlerInvocation {
        body: "1 + 1",
        is_block: false,
        env,
        raw_bindings: &[],
        setters: &[],
        event_json: None,
        broadcast_current: &[],
        journal: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 1 · the forge, closed
// ─────────────────────────────────────────────────────────────────────────────

/// The exact package body from gate 2's open row.
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

/// 🔴 **Gate 2, row 3 — inverted.** This assertion was `assert!(forged)` there.
///
/// No rebuild is involved and none is needed: the fix is not temporal. The
/// package's patch is installed and live for the whole of this handler run; it
/// simply has nothing on the path to intercept any more.
#[test]
fn a_patched_json_stringify_can_no_longer_inject_an_effect() {
    let mut engine = engine();
    let forger = package("forger", FORGER);
    register(&mut engine, &forger).expect("register");
    run_package(&mut engine, &forger);

    let env = serde_json::Map::new();
    let outcome = engine
        .eval_handler("routes/forger.tsx", &invocation(&env))
        .expect("handler runs even though the realm is poisoned");

    let forged = outcome.effects.iter().any(|effect| {
        matches!(effect, HandlerEffect::ForgeAppend { collection, .. } if collection == "albedo_users")
    });
    assert!(
        !forged,
        "🔴 SANDGATE-B DOES NOT HOLD — a patched `JSON.stringify` still forged a \
         durable write into someone else's handler. Effects: {:?}",
        outcome.effects
    );
    assert!(
        outcome.effects.is_empty(),
        "the handler body produced no effects of its own, so the list must be empty; \
         anything here came from the attacker. Got: {:?}",
        outcome.effects
    );
}

/// The control: the patch really is installed and really does still fire on an
/// array. Without this the test above passes just as well against a package
/// that never ran.
#[test]
fn the_control_holds_the_patch_is_live_during_the_handler() {
    let mut engine = engine();
    let forger = package("forger", FORGER);
    register(&mut engine, &forger).expect("register");
    run_package(&mut engine, &forger);

    let patched = probe_bool(
        &mut engine,
        "JSON.stringify([1]).indexOf('albedo_users') !== -1",
    );
    assert!(
        patched,
        "CONTROL FAILED — the package's patch is not installed, so the assertion \
         in `a_patched_json_stringify_can_no_longer_inject_an_effect` proves nothing"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 · the payload hook, refused
// ─────────────────────────────────────────────────────────────────────────────

const TOJSON_HOOK: &str = r#"
Object.prototype.toJSON = function () { return { hijacked: true }; };
module.exports = { tag: 'tojson' };
"#;

/// The residual the sealed holder alone cannot close: `stringify` consults
/// `toJSON` on any **object** it encodes, and an effect's *payload* is an
/// object. Rather than defend a value the application owns, the pass is refused
/// — no application plants `toJSON` on `Object.prototype`.
#[test]
fn a_tojson_hook_on_object_prototype_refuses_the_handler_loudly() {
    let mut engine = engine();
    let hook = package("tojson", TOJSON_HOOK);
    register(&mut engine, &hook).expect("register");
    run_package(&mut engine, &hook);

    let env = serde_json::Map::new();
    let err = engine
        .eval_handler("routes/tojson.tsx", &invocation(&env))
        .expect_err("a poisoned realm must refuse, not silently produce effects");
    let message = err.to_string();
    assert!(
        message.contains("Object.prototype.toJSON"),
        "the refusal must name the hook it found, or it is unactionable. Got: {message}"
    );
    assert!(
        message.contains("SANDGATE-B"),
        "the refusal must point at the doctrine that explains it. Got: {message}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3 · the holder itself
// ─────────────────────────────────────────────────────────────────────────────

const HOLDER_ATTACKER: &str = r#"
var out = {};
// 🪤 The first version of this compared the holder's captured `stringify`
// against `JSON.stringify` while the latter was still pristine — they are the
// same function by design, so it reported a breach on every run. The patch has
// to come FIRST, and the question is whether the holder's copy followed it.
var pristine = JSON.stringify;
JSON.stringify = function () { return '[]'; };
out.followedThePatch = (globalThis.__albedo_sealed.stringify !== pristine);
out.assigned = (function () {
  try { globalThis.__albedo_sealed = { stringify: function () { return '[]'; } }; } catch (e) {}
  return globalThis.__albedo_sealed.stringify !== pristine;
})();
out.redefined = (function () {
  try {
    Object.defineProperty(globalThis, '__albedo_sealed', { value: {}, configurable: true });
    return true;
  } catch (e) { return false; }
})();
out.deleted = (function () {
  try { return delete globalThis.__albedo_sealed; } catch (e) { return false; }
})();
out.mutated = (function () {
  var real = globalThis.__albedo_sealed.stringify;
  try { globalThis.__albedo_sealed.stringify = function () { return '[]'; }; } catch (e) {}
  return globalThis.__albedo_sealed.stringify !== real;
})();
globalThis.__attack_report = out;
module.exports = { tag: 'holder' };
"#;

/// Every route a package has to take the sealed intrinsics away from us.
///
/// `assigned` is the interesting one: it patches `JSON.stringify` first, then
/// checks whether the holder's captured copy followed. It must not — the whole
/// design rests on the capture happening before any package can run.
#[test]
fn the_sealed_holder_cannot_be_replaced_redefined_deleted_or_mutated() {
    let mut engine = engine();
    let attacker = package("holder", HOLDER_ATTACKER);
    register(&mut engine, &attacker).expect("register");
    run_package(&mut engine, &attacker);

    for (field, why) in [
        (
            "followedThePatch",
            "the holder's `stringify` tracked a later patch — the capture is not pristine",
        ),
        ("assigned", "assignment replaced the holder"),
        ("redefined", "defineProperty replaced the holder"),
        ("deleted", "delete removed the holder"),
        ("mutated", "the frozen holder's `stringify` was swapped"),
    ] {
        let breached = probe_bool(
            &mut engine,
            &format!("globalThis.__attack_report.{field} === true"),
        );
        assert!(!breached, "🔴 SANDGATE-B holder breached: {why}");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4 · THE FINDING — the effect builtins were never reachable
// ─────────────────────────────────────────────────────────────────────────────

const REACHER: &str = r#"
var names = ['append', 'update', 'remove', '__albedo_emit', '__albedo_rec', '__albedo_S'];
var found = [];
for (var i = 0; i < names.length; i++) {
  if (typeof globalThis[names[i]] !== 'undefined') { found.push(names[i]); }
}
globalThis.__reachable_builtins = found.join(',');
// `broadcast` IS a global — the render path installs an inert stub, because a
// render is read-only and a `broadcast(...)` reached during one must not write.
// Call it with something a handler would have recorded, so the test below can
// check that nothing was.
try { globalThis.broadcast('albedo_users', { role: 'admin' }); } catch (e) {}
module.exports = { tag: 'reacher' };
"#;

/// 🔑 **Why provenance is a diagnostic here and not a defence.**
///
/// The effect builtins are `const` declarations inside the per-request handler
/// IIFE. They are not properties of `globalThis`, so a package's factory body —
/// which runs in global scope — has no name to reach them by, and no reference
/// to close over. The only way a package obtains one is if the application
/// hands it over explicitly, which is the application delegating its own
/// authority and not an escalation.
///
/// That is why the forging attacks all went through *serialisation* rather than
/// through a call, and why rows 1–3 above are the fix.
#[test]
fn a_packages_factory_cannot_reach_any_effect_builtin() {
    let mut engine = engine();
    let reacher = package("reacher", REACHER);
    register(&mut engine, &reacher).expect("register");
    run_package(&mut engine, &reacher);

    let found = probe(&mut engine, "globalThis.__reachable_builtins");
    assert_eq!(
        found, "",
        "🔴 an effect builtin is reachable from package code as a global: {found}. \
         The lexical-scope argument in this file's docs no longer holds and \
         provenance has to become a real capability check."
    );
    // 🪤 `broadcast` is the exception the first draft of this test tripped over:
    // it *is* a global, because the render path needs a name for it. What
    // matters is that the global one is INERT — the effect-recording `broadcast`
    // is the handler IIFE's `const`, and the two are different functions with
    // the same name. Reachability is not authority; assert the authority.
    let env = serde_json::Map::new();
    let outcome = engine
        .eval_handler("routes/reacher.tsx", &invocation(&env))
        .expect("handler runs");
    assert!(
        outcome.effects.is_empty(),
        "the package called `globalThis.broadcast(...)` during its factory body and it reached the effect channel. The render-path stub is no longer inert. Got: {:?}",
        outcome.effects
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5 · provenance, proven where it exists
// ─────────────────────────────────────────────────────────────────────────────

const WITNESS: &str = r#"
globalThis.__origin_during_factory = globalThis.__albedo_sealed.currentOrigin();
globalThis.__depth_during_factory = globalThis.__albedo_sealed.originDepth();
module.exports = { tag: 'witness' };
"#;

/// The linker's `enterModule`/`exitModule` bracket does identify the running
/// module — and unwinds afterwards, so a later effect is not mis-attributed to
/// whichever package happened to load last.
#[test]
fn provenance_names_the_module_whose_body_is_running_and_unwinds_after() {
    let mut engine = engine();
    let witness = package("witness", WITNESS);
    register(&mut engine, &witness).expect("register");
    run_package(&mut engine, &witness);

    let during = probe(&mut engine, "globalThis.__origin_during_factory");
    assert!(
        during.contains("witness"),
        "provenance must name the module executing. Got: {during}"
    );

    let depth_after = probe(&mut engine, "globalThis.__albedo_sealed.originDepth()");
    assert_eq!(
        depth_after, "0",
        "the provenance stack must unwind, or every later effect is attributed to \
         the last package that loaded"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 6 · the probe must not be answerable by the thing it probes
// ─────────────────────────────────────────────────────────────────────────────

const LYING_HOOK: &str = r#"
// A `toJSON` that hides from anyone who reads it off the prototype itself, and
// appears for `JSON.stringify`, whose lookup uses the *value* as the receiver.
Object.defineProperty(Object.prototype, 'toJSON', {
  configurable: true,
  get: function () {
    if (this === Object.prototype) { return undefined; }
    return function () { return { hijacked: true }; };
  }
});
module.exports = { tag: 'liar' };
"#;

/// 🔴 **Found in review, not by the first version of this gate.**
///
/// The integrity probe originally read `Object.prototype.toJSON` as a plain
/// property. A plain read invokes an accessor with `Object.prototype` as the
/// receiver, so a getter that branches on `this` can answer *"clean"* to the
/// probe and hand a hijacking function to `JSON.stringify`, whose own lookup
/// uses the value being serialised as the receiver.
///
/// The probe now reads a **descriptor**, which is present either way and cannot
/// be made receiver-dependent.
#[test]
fn a_receiver_dependent_tojson_getter_cannot_hide_from_the_integrity_probe() {
    let mut engine = engine();
    let liar = package("liar", LYING_HOOK);
    register(&mut engine, &liar).expect("register");
    run_package(&mut engine, &liar);

    // The control: the getter really does lie to a plain read.
    let lies = probe_bool(&mut engine, "Object.prototype.toJSON === undefined");
    assert!(
        lies,
        "CONTROL FAILED — the getter is supposed to read as `undefined` off the \
         prototype, which is what made the old probe answerable"
    );

    let env = serde_json::Map::new();
    let err = engine
        .eval_handler("routes/liar.tsx", &invocation(&env))
        .expect_err("a realm carrying a hidden toJSON hook must refuse the pass");
    assert!(
        err.to_string().contains("Object.prototype.toJSON"),
        "🔴 the integrity probe was fooled by a receiver-dependent getter. Got: {err}"
    );
}
