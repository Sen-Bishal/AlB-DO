//! SANDGATE · what a confined request actually costs, on the **shipped** path.
//!
//! Gate 3.1 measured bytecode against `ctx.eval` in a throwaway harness and
//! *projected* a confined Radix request at ≈2.68 ms. A projection is not a
//! measurement: it applied a ratio measured on module registration to a total
//! that also contains the prelude rebuild, and it did not go through
//! [`QuickJsEngine::confine`] — which adds a ledger walk, a hash-table rebuild,
//! and a `BootstrapPayload` clone that the harness never paid for.
//!
//! This measures the thing the server calls, and attributes the total so the
//! next optimisation is aimed rather than guessed.
//!
//! ## Measured (dev rig, release, 2026-08-30)
//!
//! | | before the prelude went to bytecode | after |
//! |---|---|---|
//! | prelude rebuild | 1.631 ms | **0.373 ms** |
//! | confined request, no npm | 1.673 ms | **0.415 ms** |
//! | confined request, Radix (55 artifacts) | 2.320 ms | **1.110 ms** |
//! | npm's marginal cost | 0.635 ms | 0.685 ms |
//!
//! Gate 3.1 *projected* 2.68 ms and took the "no background pool needed"
//! decision on it. The shipped path is 1.11 ms, so the decision holds with
//! room. The attribution inverted on the way — the prelude was 72% of a
//! confinement and is now 35% of a smaller one — which is why the assertion at
//! the bottom of this file is on the total and not on the ratio.
//!
//! ```text
//! cargo test --release --features forge --test sandgate_confine_cost -- --ignored --nocapture --test-threads=1
//! ```
//!
//! 🪤 **Two false numbers came out of gate 3.1 before it was trusted, both from
//! the same root: timing work that had silently failed.** Every phase here
//! therefore asserts its own output before its timing is printed — a render
//! that throws is fast and meaningless.

#![cfg(feature = "forge")]

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, ShakeOptions};
use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The corpus root is a *project* inside `albedo-corpus`, not the corpus
/// directory itself — `node_modules` lives per project. Gate 3.1's harness used
/// the parent and silently took its "corpus not installed" branch.
const CORPUS: &str = "C:/Development/albedo-corpus/shadcn-taxonomy";
const REPS: usize = 60;

fn engine() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine.init(&BootstrapPayload::default()).expect("init");
    engine
}

fn mean_ms(samples: &[f64]) -> f64 {
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Median, because a single GC pause in 60 reps moves a mean by more than any
/// optimisation on this page would.
fn median_ms(samples: &mut Vec<f64>) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    samples[samples.len() / 2]
}

fn time<F: FnMut()>(reps: usize, mut body: F) -> (f64, f64) {
    let mut samples = Vec::with_capacity(reps);
    for _ in 0..reps {
        let start = Instant::now();
        body();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mean = mean_ms(&samples);
    (mean, median_ms(&mut samples))
}

const ROUTE_NO_NPM: &str = r#"export default function P(props) {
    return <div class="card"><b>{props.title}</b><span>{props.body}</span></div>;
}"#;

/// The same route, but importing the package — so the replay pays for eager
/// linking the way a real route does.
const ROUTE_WITH_NPM: &str = r#"import * as Dialog from "@radix-ui/react-dialog";
export default function P(props) {
    return <div class="card" data-has={typeof Dialog.Root === "undefined" ? "no" : "dialog"}>
        <b>{props.title}</b><span>{props.body}</span>
    </div>;
}"#;

#[test]
#[ignore = "timing; run explicitly in release"]
fn a_confined_request_costs_what_gate_3_1_projected() {
    // ── phase 1 · no npm — the floor ──────────────────────────────────────
    let mut plain = engine();
    plain
        .load_module("routes/p.tsx", ROUTE_NO_NPM)
        .expect("route");
    let props = r#"{"title":"t","body":"b"}"#;
    let baseline = plain
        .render_component("routes/p.tsx", props)
        .expect("renders")
        .html;
    assert!(
        baseline.contains("card"),
        "the control render produced nothing to time"
    );
    // Warm past the arena's persistent window before timing anything.
    for _ in 0..16 {
        plain.confine().expect("confine");
        plain.render_component("routes/p.tsx", props).expect("render");
    }

    let (render_mean, render_median) = time(REPS, || {
        let out = plain.render_component("routes/p.tsx", props).expect("render");
        assert!(!out.html.is_empty());
    });
    let (confine_mean, confine_median) = time(REPS, || plain.confine().expect("confine"));

    println!("\n=== SANDGATE · the shipped confined request ===\n");
    println!("  NO NPM");
    println!("    A · render only          : {render_mean:.3} ms (median {render_median:.3})");
    println!("    B · confine only         : {confine_mean:.3} ms (median {confine_median:.3})");
    println!(
        "    C · confined request     : {:.3} ms",
        render_mean + confine_mean
    );

    // ── phase 2 · with a real package graph ───────────────────────────────
    let root = PathBuf::from(CORPUS);
    if !root.join("node_modules/@radix-ui/react-dialog").is_dir() {
        println!("\n  RADIX — SKIPPED, corpus not installed at {CORPUS}\n");
        return;
    }

    let options: ShakeOptions = dom_render_compiler::bundler::client_npm::server_shake_options();
    let bundle = bundle_npm_dependency(Path::new(CORPUS), "@radix-ui/react-dialog", &options)
        .expect("bundles");
    let source_bytes: usize = bundle.artifacts.iter().map(|a| a.script.len()).sum();

    let mut npm = engine();
    for artifact in &bundle.artifacts {
        npm.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("artifact registers");
    }
    // 🔴 The route must genuinely IMPORT the package. A route that does not is
    // the cheap case, and measuring it would understate every number below:
    // project modules link their imports eagerly at load, so a replay of an
    // importing route also re-runs the package's factory chain, which a
    // non-importing route never pays for.
    npm.load_module("routes/p.tsx", ROUTE_WITH_NPM)
        .expect("route");
    let imported = npm
        .render_component("routes/p.tsx", props)
        .expect("renders")
        .html;
    assert!(
        imported.contains("dialog"),
        "the timed route did not actually reach the package: {imported}"
    );
    assert!(
        npm.holds_third_party_code(),
        "the dirty bit did not latch — this phase would be measuring the no-npm path again"
    );

    for _ in 0..16 {
        npm.confine().expect("confine");
        npm.render_component("routes/p.tsx", props).expect("render");
    }
    let stats = npm.confinement_stats();
    assert!(
        stats.bytecode_hits > 0,
        "no entry replayed from bytecode — this is timing the slow path"
    );
    assert_eq!(
        stats.bytecode_refusals, 0,
        "{} artifacts refused to compile; the number below is a mixture",
        stats.bytecode_refusals
    );

    let (npm_render_mean, _) = time(REPS, || {
        npm.render_component("routes/p.tsx", props).expect("render");
    });
    let (npm_confine_mean, npm_confine_median) = time(REPS, || npm.confine().expect("confine"));
    let (script_bytes, bytecode_bytes) = npm.confinement_resident_bytes();

    println!(
        "\n  RADIX — {} artifacts, {:.1} KB of registration script",
        bundle.artifacts.len(),
        source_bytes as f64 / 1024.0
    );
    println!("    A · render only          : {npm_render_mean:.3} ms");
    println!(
        "    B · confine only         : {npm_confine_mean:.3} ms (median {npm_confine_median:.3})"
    );
    println!(
        "    C · confined request     : {:.3} ms",
        npm_render_mean + npm_confine_mean
    );
    println!(
        "    npm's marginal cost      : {:.3} ms  (confine {npm_confine_mean:.3} − {confine_mean:.3})",
        npm_confine_mean - confine_mean
    );
    println!(
        "\n  RESIDENT · {:.1} KB replay script + {:.1} KB bytecode ({:.2}× the script)",
        script_bytes as f64 / 1024.0,
        bytecode_bytes as f64 / 1024.0,
        bytecode_bytes as f64 / script_bytes.max(1) as f64
    );
    println!(
        "  ATTRIBUTION · of {npm_confine_mean:.3} ms of confinement, the prelude rebuild is \
         {confine_mean:.3} ms ({:.0}%) and the {} module replays are {:.3} ms ({:.0}%)",
        confine_mean / npm_confine_mean * 100.0,
        stats.replayed_entries / stats.confinements.max(1),
        npm_confine_mean - confine_mean,
        (npm_confine_mean - confine_mean) / npm_confine_mean * 100.0
    );
    println!();

    // ── phase 3 · how the cost SCALES ─────────────────────────────────────
    //
    // 🔑 The number that decides whether confinement is affordable for a real
    // app, and the one a single-package measurement cannot show: **a
    // confinement replays the whole project's npm surface, not the part the
    // route uses.** Cost is linear in what the app depends on, and a route that
    // imports nothing pays the same as one that imports everything.
    //
    // 🪤 The first version of this compared a half-bundle engine with NO route
    // against the full engine WITH an importing route, and reported the sum of
    // two different terms as a per-artifact slope. Each measurement below now
    // varies exactly one thing.
    let registration_only = |count: usize| -> f64 {
        let mut e = engine();
        for artifact in bundle.artifacts.iter().take(count) {
            e.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
                .expect("artifact registers");
        }
        for _ in 0..16 {
            e.confine().expect("confine");
        }
        time(REPS, || e.confine().expect("confine")).0
    };

    let half = bundle.artifacts.len() / 2;
    let small = registration_only(half);
    let full = registration_only(bundle.artifacts.len());
    let per_artifact_us = (full - small) * 1000.0 / (bundle.artifacts.len() - half) as f64;

    // The second term, isolated: replaying a project module that imports the
    // package re-runs its factory chain, because a module links its imports
    // eagerly at load. `npm_confine_mean` carries it; `full` does not.
    let eager_link_ms = npm_confine_mean - full;

    println!(
        "  SCALING · registration only: {half} artifacts {small:.3} ms → {} artifacts          {full:.3} ms  ⇒  {per_artifact_us:.1} µs per artifact",
        bundle.artifacts.len()
    );
    println!(
        "    eager re-linking on top   : {eager_link_ms:.3} ms — what a route that actually \
         IMPORTS the package adds, by re-running its factory chain in the fresh realm"
    );
    println!(
        "    extrapolated per request  : ≈{:.1} ms at 500 artifacts, ≈{:.1} ms at 2000 \
         — for the app's WHOLE npm surface, regardless of what the route imports",
        confine_mean + 500.0 * per_artifact_us / 1000.0,
        confine_mean + 2000.0 * per_artifact_us / 1000.0
    );
    println!(
        "\n  ⇒ the next optimisation is LAZY artifact hydration: register the alias table \
         eagerly and hydrate a factory on first require, so a request pays for the \
         packages its route reaches instead of every package the project has.\n"
    );

    // ── phase 4 · the collector, relaxed for the WHOLE engine ─────────────
    //
    // 🔴 Kept as the control for the refutation it produced. Raising the
    // threshold and leaving it raised helps the rebuild and wrecks the render,
    // because the `Context` is discarded per request but the **heap belongs to
    // the `Runtime`** and survives — so render garbage accumulates in it. The
    // shipped version raises it only for the duration of a rebuild
    // (`REBUILD_GC_THRESHOLD`), which is what the numbers below are measured
    // against.
    let mut relaxed = engine();
    for artifact in &bundle.artifacts {
        relaxed
            .load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
            .expect("artifact registers");
    }
    relaxed
        .load_module("routes/p.tsx", ROUTE_WITH_NPM)
        .expect("route");
    relaxed.set_gc_threshold(64 * 1024 * 1024);
    for _ in 0..16 {
        relaxed.confine().expect("confine");
        relaxed.render_component("routes/p.tsx", props).expect("render");
    }
    let (relaxed_confine, _) = time(REPS, || relaxed.confine().expect("confine"));
    let (relaxed_render, _) = time(REPS, || {
        relaxed.render_component("routes/p.tsx", props).expect("render");
    });

    println!(
        "
  GC · shipped (suspended for the rebuild only) : confine {npm_confine_mean:.3} ms ·          render {npm_render_mean:.3} ms · request {:.3} ms",
        npm_render_mean + npm_confine_mean
    );
    println!(
        "     · relaxed for the WHOLE engine (REFUTED)    : confine {relaxed_confine:.3} ms ·          render {relaxed_render:.3} ms · request {:.3} ms",
        relaxed_render + relaxed_confine
    );
    println!(
        "     ⇒ leaving it relaxed costs {:+.1}% on the render and {:+.1}% on the request.
",
        (relaxed_render - npm_render_mean) / npm_render_mean * 100.0,
        ((relaxed_render + relaxed_confine) - (npm_render_mean + npm_confine_mean))
            / (npm_render_mean + npm_confine_mean)
            * 100.0
    );

    // ── the gate ──────────────────────────────────────────────────────────
    //
    // 🪤 **This assertion started life as "the prelude must dominate the module
    // replay", and optimising the prelude nearly made it fire.** It was a
    // proxy for the thing actually being defended — gate 3.1's conclusion that
    // *a background pool is not a prerequisite* — and the proxy inverted the
    // moment the prelude went onto the bytecode path (1.63 ms → 0.37 ms, so
    // module replay went from 28% of a confinement to 65% of a smaller one).
    // An assertion that fails because the code got faster is measuring the
    // wrong thing.
    //
    // The gate is now on the total, which is what the conclusion rests on.
    // Absolute milliseconds are machine-specific, so the bound is deliberately
    // loose: gate 3.1 took the "no pool needed" decision on a *projected*
    // 2.68 ms, and 5 ms is roughly where the complexity of a background pool
    // would start to earn itself back. If this fires, re-take that decision —
    // do not raise the number.
    const CEILING_MS: f64 = 5.0;
    let total = npm_render_mean + npm_confine_mean;
    assert!(
        total < CEILING_MS,
        "🔴 a confined Radix request now costs {total:.3} ms. Gate 3.1's \
         'a background pool is NOT a prerequisite' was decided at a projected \
         2.68 ms; at this cost that decision has to be re-taken rather than the \
         threshold raised."
    );

    // Bytecode carries the module half. If it silently stopped applying, the
    // total above would still pass on a fast machine while every request paid
    // 6.8× for module registration — so the mechanism is asserted separately
    // from the cost.
    assert_eq!(
        stats.bytecode_hits / stats.confinements.max(1),
        bundle.artifacts.len() as u64,
        "not every npm artifact replayed from bytecode"
    );
}
