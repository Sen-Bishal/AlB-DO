//! SANDGATE · Gate 5 — **confinement, shipped and regression-pinned.**
//!
//! Gates 1–4 measured the pieces. This file measures the thing:
//! [`QuickJsEngine::confine`], which is what a request boundary actually calls.
//!
//! The distinction matters, because every earlier gate handed `rebuild_realm` a
//! closure that re-registered the modules *the test knew about*. Production has
//! no such caller. `confine()` replays the engine's own **ledger**
//! (`runtime::confinement`), so the property under test here is not "can a realm
//! be rebuilt" — gate 1 settled that — but **"can it be rebuilt by something
//! that was never told what was in it"**.
//!
//! | # | property |
//! |---|---|
//! | 5.0 | a confined engine renders byte-identically, with no recipe from the caller |
//! | 5.1 | the cross-request leak closes through `confine()` alone |
//! | 5.2 | module-level JS state stops surviving requests — a **behaviour change**, pinned |
//! | 5.3 | gate 1's zero-growth property survives the ledger |
//! | 5.4 | bytecode is actually on the replay path, and replays the same table |
//! | 5.5 | the dirty bit is false for an app with no npm, so nothing is confined for free |
//!
//! Run:
//! ```text
//! cargo test --test sandgate_gate5 -- --nocapture --test-threads=1
//! ```

#![cfg(feature = "forge")]

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmDependencyBundle, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine, RuntimeResult};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::path::Path;

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

fn package(name: &str, body: &str) -> Package {
    let dir = tempfile::tempdir().expect("tempdir");
    let pkg = dir.path().join("node_modules").join(name);
    write(
        &pkg.join("package.json"),
        &format!(r#"{{ "name": "{name}", "version": "1.0.0", "main": "./index.js" }}"#),
    );
    write(&pkg.join("index.js"), body);
    let options: ShakeOptions = dom_render_compiler::bundler::client_npm::server_shake_options();
    let bundle = bundle_npm_dependency(dir.path(), name, &options).expect("bundle");
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

const PLAIN: &str = "module.exports = { tag: 'plain' };";

// ─────────────────────────────────────────────────────────────────────────────
// 5.0 · the ledger replays a realm nobody described
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn a_confined_engine_renders_byte_identically_with_no_recipe_from_the_caller() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    register(&mut engine, &pkg).expect("register");

    let before = engine
        .render_component(&pkg.route, "{}")
        .expect("renders")
        .html;

    // 🔑 No closure, no artifact list, no module sources. Everything the engine
    // needs to reconstitute itself it already recorded.
    engine.confine().expect("confine");

    let after = engine
        .render_component(&pkg.route, "{}")
        .expect("renders after confinement")
        .html;

    assert_eq!(
        before, after,
        "a confined realm must render identically, or confinement is a correctness bug \
         wearing a security feature"
    );

    let stats = engine.confinement_stats();
    assert_eq!(stats.confinements, 1);
    assert!(
        stats.replayed_entries >= 2,
        "the ledger replayed {} entries — the package artifact and its route are the \
         floor. A confinement that replays nothing is confining nothing.",
        stats.replayed_entries
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.1 · the leak closes through `confine()` alone
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

const VICTIM: &str = r#"export default function V(props) {
    return <b data-secret={props.secret}>{props.secret}</b>;
}"#;

const READBACK: &str = r#"export default function R() {
    return <pre>{(globalThis.__stolen || []).join(" ~ ")}</pre>;
}"#;

const SECRET: &str = "alice-private-token";

#[test]
fn the_cross_request_leak_closes_through_confine_alone() {
    let mut engine = engine();
    let peeker = package("peeker", PEEKER);
    register(&mut engine, &peeker).expect("register");
    engine
        .render_component(&peeker.route, "{}")
        .expect("attacker renders");

    engine
        .load_module("routes/victim.tsx", VICTIM)
        .expect("victim");
    engine
        .render_component("routes/victim.tsx", &format!(r#"{{"secret":"{SECRET}"}}"#))
        .expect("victim renders");
    engine
        .load_module("routes/readback.tsx", READBACK)
        .expect("readback");

    let within = engine
        .render_component("routes/readback.tsx", "{}")
        .expect("readback")
        .html;
    assert!(
        within.contains(SECRET),
        "CONTROL FAILED — the leak was supposed to be open. Got: {within}"
    );

    engine.confine().expect("confine");

    let after = engine
        .render_component("routes/readback.tsx", "{}")
        .expect("readback")
        .html;
    assert!(
        !after.contains(SECRET),
        "🔴 request 1's props survived `confine()`. Got: {after}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.2 · the behaviour change, observed rather than discovered
// ─────────────────────────────────────────────────────────────────────────────

const COUNTER: &str = r#"
var calls = 0;
module.exports = { tag: 'counter', next: function () { calls = calls + 1; return calls; } };
"#;

const COUNTER_ROUTE: &str = r#"import { next } from "counter";
export default function C() { return <b>{String(next())}</b>; }"#;

/// 🔑 **Module-level JS state stops surviving requests, and that is a
/// behaviour change, not a bug fix.**
///
/// `const cache = new Map()` at a package's module scope is a legitimate,
/// extremely common pattern — memoised regexes, interned objects, warmed
/// lookup tables. It is *also* exactly how the cross-request leak above works.
/// The two are the same mechanism, so confinement cannot keep one and drop the
/// other.
///
/// This test exists so the consequence is a **fact in the suite** rather than
/// something a user rediscovers as "my package's cache keeps resetting".
#[test]
fn module_level_package_state_no_longer_survives_a_request_boundary() {
    let mut engine = engine();
    let counter = package("counter", COUNTER);
    for artifact in &counter.bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("artifact");
    }
    engine
        .load_module("routes/counter.tsx", COUNTER_ROUTE)
        .expect("route");

    let first = engine
        .render_component("routes/counter.tsx", "{}")
        .expect("renders")
        .html;
    let second = engine
        .render_component("routes/counter.tsx", "{}")
        .expect("renders")
        .html;
    assert!(
        first.contains('1') && second.contains('2'),
        "CONTROL — within one realm the package's counter accumulates. Got {first} / {second}"
    );

    engine.confine().expect("confine");

    let third = engine
        .render_component("routes/counter.tsx", "{}")
        .expect("renders")
        .html;
    assert!(
        third.contains('1'),
        "module-level state was expected to RESET across a confinement boundary. If this \
         now accumulates, confinement is not discarding the module record — which would \
         mean the leak in 5.1 is open again. Got: {third}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.3 · gate 1's zero-growth property, through the ledger
// ─────────────────────────────────────────────────────────────────────────────

/// Gate 1 measured `rebuild_realm` with a hand-written registration closure and
/// found 0.0 KB per rebuild — but only because the closure ran *inside* the
/// arena bracket. `confine()` replays a whole ledger in that same position, so
/// the property has to be re-measured, not assumed: a ledger replay is strictly
/// more allocation than gate 1's closure did.
#[test]
fn confinement_does_not_grow_the_persistent_arena() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    register(&mut engine, &pkg).expect("register");

    // 🪤 **The first version of this measured 754 B/cycle and I nearly shipped a
    // threshold that accepted it.** The engine runs its first
    // `ARENA_WARMUP_RENDERS` (8) renders in PERSISTENT mode on purpose — that is
    // how QuickJS's lazily-allocated global tables get interned somewhere the
    // request reset will not free them. Six warm-up renders left two of them
    // inside the measurement window, and a one-time step read as a slope.
    //
    // The fix is not a bigger tolerance. It is to measure two windows and
    // compare them: a step appears in the first and not the second, a leak
    // appears in both.
    for _ in 0..16 {
        engine.confine().expect("confine");
        engine.render_component(&pkg.route, "{}").expect("renders");
    }

    const WINDOW: usize = 40;
    let mut measure = |engine: &mut QuickJsEngine| -> f64 {
        let before = engine.arena_stats().persistent_used;
        for _ in 0..WINDOW {
            engine.confine().expect("confine");
            engine.render_component(&pkg.route, "{}").expect("renders");
        }
        let after = engine.arena_stats().persistent_used;
        after.saturating_sub(before) as f64 / WINDOW as f64
    };

    let first = measure(&mut engine);
    let second = measure(&mut engine);
    let stats = engine.arena_stats();

    println!(
        "  persistent growth per confine+render cycle: window 1 {first:.1} B ·          window 2 {second:.1} B · fallback allocs {} · system live {} B",
        stats.fallback_allocs, stats.system_live_bytes
    );

    // The steady-state window is the assertion. Gate 1 measured 0.0 KB per
    // rebuild for `rebuild_realm` with a hand-written closure; a ledger replay
    // is strictly more work, so a few bytes of jitter are expected and a
    // kilobyte-scale slope is the failure. At 754 B/cycle — the number the
    // warm-up window produced — the 16 MB region is gone in ~22 000 requests.
    assert!(
        second < 64.0,
        "🔴 confinement leaks {second:.1} B into the persistent region per cycle in          STEADY STATE (window 1 was {first:.1} B, so this is not warm-up). The 16 MB          region is exhausted after ~{:.0} confinements.",
        (16.0 * 1024.0 * 1024.0) / second.max(1.0)
    );
    assert_eq!(
        stats.fallback_allocs, 0,
        "the persistent region overflowed into the fallback allocator during the run"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.4 · bytecode is actually on the path
// ─────────────────────────────────────────────────────────────────────────────

/// Gate 3.1 measured a 6.8× speedup for bytecode replay in a throwaway harness.
/// This asserts the shipped path takes it — and, more importantly, that it
/// takes it *for the right entries*: npm artifacts yes, project modules no.
///
/// 🔑 The count is pinned to the ledger's own composition rather than to a
/// literal, because "bytecode_hits > 0" is the vacuous version of this
/// assertion — it passes with one artifact out of fifty-five compiled.
#[test]
fn npm_artifacts_replay_from_bytecode_and_project_modules_do_not() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    register(&mut engine, &pkg).expect("register");

    let npm_entries = pkg.bundle.artifacts.len() as u64;
    engine.confine().expect("confine");
    let stats = engine.confinement_stats();

    println!(
        "  replayed {} entries: {} from bytecode, {} from source ({} refusals)",
        stats.replayed_entries, stats.bytecode_hits, stats.source_replays, stats.bytecode_refusals
    );

    assert_eq!(
        stats.bytecode_refusals, 0,
        "an npm artifact refused to compile to bytecode; the fast path silently \
         degraded for it"
    );
    assert_eq!(
        stats.bytecode_hits, npm_entries,
        "every npm artifact must replay from bytecode — {npm_entries} were registered"
    );
    assert_eq!(
        stats.source_replays,
        stats.replayed_entries - npm_entries,
        "project modules must replay as SOURCE. Promoting them to strict module \
         source would change the meaning of application code (see \
         `runtime::confinement`'s strict-mode note)."
    );

    // Second confinement must reuse the compiled bytes rather than recompile.
    engine.confine().expect("confine again");
    let (script_bytes, bytecode_bytes) = engine.confinement_resident_bytes();
    println!("  resident: {script_bytes} B of replay script, {bytecode_bytes} B of bytecode");
    assert!(
        bytecode_bytes > 0,
        "bytecode was compiled and then not retained — every confinement is paying \
         the compile again"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.5 · nothing is confined for free
// ─────────────────────────────────────────────────────────────────────────────

const NO_NPM_ROUTE: &str = r#"export default function P() { return <b>hi</b>; }"#;

/// The dirty bit is what the pool gates on. An app with no npm must report
/// false, or every request on every project pays for a rebuild that protects
/// against nothing.
#[test]
fn an_app_with_no_npm_is_never_dirty() {
    let mut engine = engine();
    engine
        .load_module("routes/p.tsx", NO_NPM_ROUTE)
        .expect("route");
    engine.render_component("routes/p.tsx", "{}").expect("renders");

    assert!(
        !engine.holds_third_party_code(),
        "🔴 an app with no npm dependency reported dirty — the pool will confine \
         every request on every project for no benefit"
    );
    assert_eq!(engine.ledger_len(), 1, "one project module, one ledger entry");

    let pkg = package("plain", PLAIN);
    register(&mut engine, &pkg).expect("register");
    assert!(
        engine.holds_third_party_code(),
        "registering an npm artifact must latch the dirty bit"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.6 · a failed confinement must not cost the engine
// ─────────────────────────────────────────────────────────────────────────────

/// 🔴 **The failure mode this closes was worse than the attack it defends
/// against.**
///
/// The first version of `rebuild_realm` dropped the context and *then* built
/// the replacement. A replay that failed halfway therefore left the engine
/// initialised and **empty** — and a pooled engine in that state is not
/// removed from the idle stack, so it serves a missing-module error to every
/// subsequent request for the life of the process. A confinement is run on
/// every request; converting a transient allocation failure into a permanent
/// outage is not a trade worth making for a security boundary.
///
/// Rebuilding is now build-then-swap, so it is all-or-nothing.
#[test]
fn a_failed_rebuild_rolls_back_to_the_realm_that_was_working() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    register(&mut engine, &pkg).expect("register");
    let before = engine
        .render_component(&pkg.route, "{}")
        .expect("renders")
        .html;

    // A registration closure that fails partway: an empty module is refused by
    // the compiler, which is the cheapest way to make a replay fail for real
    // rather than by mocking one.
    let failed = engine.rebuild_realm(|e| {
        e.load_module("routes/ok.tsx", "export default function O() { return <i>ok</i>; }")?;
        e.load_module("routes/empty.tsx", "")
    });
    assert!(failed.is_err(), "the rebuild was supposed to fail");

    let after = engine
        .render_component(&pkg.route, "{}")
        .expect("🔴 the engine stopped serving after a failed rebuild — it is now \
                 initialised and empty, which is a permanent outage for a pooled engine")
        .html;
    assert_eq!(
        before, after,
        "the rolled-back realm must be the one that was working"
    );

    // And it is still confinable afterwards — the rollback left no wreckage.
    engine.confine().expect("confine after a failed rebuild");
    let recovered = engine
        .render_component(&pkg.route, "{}")
        .expect("renders")
        .html;
    assert_eq!(before, recovered);
}

// ─────────────────────────────────────────────────────────────────────────────
// 5.7 · registration is not execution
// ─────────────────────────────────────────────────────────────────────────────

const IDLE_ROUTE: &str = r#"export default function I() { return <b>no packages here</b>; }"#;

/// 📏 **Registration is not execution — and the narrow window in which that
/// helps.**
///
/// An npm artifact installs a lazy factory; nothing runs until something
/// imports it. A project that depends on a package no *project module* imports
/// at load therefore has a realm no package has touched, and confining it costs
/// the full replay — measured at 1.06 ms for the Radix corpus — to protect
/// against nothing.
///
/// ⚠️ **The window is much narrower than it looks**, for the reason spelled out
/// at the end of this test: the ledger replay itself re-links, so any project
/// whose routes import a package latches again the moment it is confined.
///
/// 🔑 The realm is asked, which is normally the wrong place. It is sound here
/// because the sealed holder offers only a **setter**: a package can latch the
/// flag and force extra rebuilds, and has no way to clear it. The failure
/// direction is the safe one.
#[test]
fn a_registered_but_never_imported_package_does_not_trigger_a_confinement() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    // Artifacts only — the route that imports it is deliberately not registered.
    for artifact in &pkg.bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("artifact");
    }
    engine
        .load_module("routes/idle.tsx", IDLE_ROUTE)
        .expect("route");
    engine
        .render_component("routes/idle.tsx", "{}")
        .expect("renders");

    assert!(
        engine.holds_third_party_code(),
        "CONTROL — npm IS registered on this engine"
    );
    assert!(
        !engine.third_party_code_ran(),
        "🔴 a package that was never imported reported as executed; every request \
         on every npm-having project pays for a rebuild it does not need"
    );

    // Now import it, and the latch must close.
    engine
        .load_module(&pkg.route, &pkg.route_src)
        .expect("importing route");
    engine
        .render_component(&pkg.route, "{}")
        .expect("renders");
    assert!(
        engine.third_party_code_ran(),
        "🔴 a package factory executed and the realm did not latch — confinement \
         would be skipped on exactly the requests that need it"
    );

    // 🔴 **AND HERE IS THE FINDING, which was written as `!ran` first and
    // failed.**
    //
    // A confinement does not leave the latch clear, because the replay
    // *re-links*: a project module links its imports eagerly at load, so
    // replaying `routes/plain.tsx` calls the linker, which runs the package's
    // factory — in the fresh realm, before the next request touches it.
    //
    // 🔑 That is gate 2's result showing up in the mechanism rather than in a
    // threat model: **confinement erases an attacker's accumulated data and
    // re-runs their code as part of putting the realm back.** It is also what
    // makes the "registration is not execution" optimisation above much
    // narrower than it first appears — it saves a rebuild only for a project
    // where *no project module imports npm at load*, and any project whose
    // routes import a package latches again immediately.
    //
    // Suppressing the latch during the replay would widen it and is **unsound**:
    // the package's body has genuinely run in the new realm at that point, and
    // an `h` wrapper it installed there would then accumulate across every
    // subsequent request — which is exactly the leak in 5.1.
    engine.confine().expect("confine");
    assert!(
        engine.third_party_code_ran(),
        "the ledger replay links project modules eagerly, so the package's factory \
         runs during the rebuild. If this ever reads false, linking became lazy — \
         which would be a real improvement, but check 5.1 still holds before \
         believing it."
    );
}

/// The React host records ship in the npm record format
/// (`albedo:host/react`), so a linker that marked on every record would latch
/// on essentially every render and make the flag useless.
#[test]
fn the_react_host_records_are_not_mistaken_for_third_party_code() {
    let mut engine = engine();
    let pkg = package("plain", PLAIN);
    for artifact in &pkg.bundle.artifacts {
        engine
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("artifact");
    }
    engine
        .load_module(
            "routes/hooks.tsx",
            "export default function H() { return <b>{String(typeof useState)}</b>; }",
        )
        .expect("route");
    engine
        .render_component("routes/hooks.tsx", "{}")
        .expect("renders");

    assert!(
        !engine.third_party_code_ran(),
        "🔴 rendering a route that touches only the React host shim latched the \
         third-party flag — the `albedo:host/` exclusion in the linker is gone, and \
         the optimisation it enables is dead"
    );
}
