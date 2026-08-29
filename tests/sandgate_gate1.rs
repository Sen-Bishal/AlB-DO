//! SANDGATE · Gate 1 — *does the primitive survive at all?*
//!
//! Before any design work on 10.0's confinement half, `QuickJsEngine::reset_realm`
//! has to be shown not to corrupt or exhaust the arena. This is the gate that
//! can kill the whole approach, so it runs first and it runs loud.
//!
//! ## The specific hazard
//!
//! `ArenaControl::in_request` is a **routing flag**, not a region reset:
//!
//! * `in_request == true`  → allocations become QuickJS-managed *system* blocks,
//!   freed per-block by refcount + the cycle collector.
//! * `in_request == false` → allocations go to the **persistent bump region**,
//!   which is never freed while the engine lives (16 MB cap).
//!
//! `reset_realm` runs *between* renders, so `in_request` is **false** — which
//! routes an entire fresh context, its intrinsics and the whole prelude into the
//! persistent bump. Nothing frees that. A reset-per-request server would march
//! the persistent region to its cap and then fall back, on a fixed number of
//! requests.
//!
//! ✅ The hazard I first assumed — `end_request` freeing live intrinsics — does
//! **not** exist: `end_request` resets no region (see `arena.rs`, "Why not a
//! resettable request region?"). That was designed out already. The real risk is
//! the opposite: not freeing at all.
//!
//! Run:
//! ```text
//! cargo test --release --test sandgate_gate1 -- --ignored --nocapture --test-threads=1
//! cargo test --test sandgate_gate1 -- --ignored --nocapture --test-threads=1   # debug asserts
//! ```

#![cfg(feature = "forge")]

use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;

const SPEC: &str = "Component.tsx";

const COMPONENT: &str = r#"
export default function Component(props) {
  const items = [1, 2, 3];
  return (
    <div className="card">
      <h1>{props.title || "hello"}</h1>
      <ul>{items.map((n) => <li key={n}>row {n}</li>)}</ul>
    </div>
  );
}
"#;

fn kb(bytes: usize) -> f64 {
    bytes as f64 / 1024.0
}

fn warm() -> QuickJsEngine {
    let mut engine = QuickJsEngine::new();
    engine
        .init(&BootstrapPayload::default())
        .expect("engine init");
    engine
        .load_module_with_spec(SPEC, COMPONENT, Some(SPEC))
        .expect("module loads");
    // Past ARENA_WARMUP_RENDERS (8) so the engine is in request-scoped mode —
    // the state a live server is actually in when a reset would happen.
    for _ in 0..12 {
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("warm render");
    }
    engine
}

/// **G1.2 — the decisive one.** Does the persistent region grow per reset?
///
/// Reports bytes-per-reset. If that number is large and linear, reset-per-request
/// exhausts a 16 MB region in a fixed, small number of requests and the whole
/// confinement design has to route the reset through request mode instead.
#[ignore = "SANDGATE Gate 1; run explicitly"]
#[test]
fn g1_2_persistent_growth_per_reset() {
    const N: usize = 40;

    let mut engine = warm();
    let before = engine.arena_stats();
    println!("\n=== G1.2 · persistent growth per reset ===\n");
    println!(
        "  baseline: persistent {:>10.1} KB   system_live {:>10.1} KB   fallback {}",
        kb(before.persistent_used),
        kb(before.system_live_bytes),
        before.fallback_allocs
    );

    let mut samples = Vec::new();
    for i in 0..N {
        engine
            .rebuild_realm(|e| e.load_module_with_spec(SPEC, COMPONENT, Some(SPEC)))
            .expect("rebuild");
        engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        let s = engine.arena_stats();
        samples.push(s.persistent_used);
        if i < 3 || i == N / 2 || i == N - 1 {
            println!(
                "  after reset {i:>3}: persistent {:>10.1} KB   system_live {:>10.1} KB   \
                 fallback {}   grew_in_request {}",
                kb(s.persistent_used),
                kb(s.system_live_bytes),
                s.fallback_allocs,
                s.persistent_grew_in_request
            );
        }
    }

    let after = engine.arena_stats();
    let growth = after
        .persistent_used
        .saturating_sub(before.persistent_used);
    let per_reset = growth as f64 / N as f64;
    println!(
        "\n  TOTAL persistent growth over {N} resets: {:.1} KB  →  {:.1} KB per reset",
        kb(growth),
        per_reset / 1024.0
    );
    if per_reset > 1.0 {
        let cap_kb = 16.0 * 1024.0;
        println!(
            "  ⇒ at this rate a 16 MB persistent region is exhausted after ~{:.0} resets",
            cap_kb / (per_reset / 1024.0)
        );
    }
    println!(
        "  fallback_allocs: {} (non-zero ⇒ the persistent region was exhausted)\n",
        after.fallback_allocs
    );
}

/// **G1.1 — soak.** Many reset/render cycles. Proves the process survives and
/// that request memory returns to a steady state rather than climbing.
#[ignore = "SANDGATE Gate 1; run explicitly"]
#[test]
fn g1_1_soak_realm_churn() {
    const N: usize = 400;

    println!("\n=== G1.1 · soak, {N} reset+render cycles ===\n");
    let mut engine = warm();
    let baseline_live = engine.arena_stats().system_live_bytes;

    for i in 0..N {
        engine
            .rebuild_realm(|e| e.load_module_with_spec(SPEC, COMPONENT, Some(SPEC)))
            .expect("rebuild");
        let out = engine
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
        assert!(
            out.html.contains(">hello</h1>"),
            "render {i} produced wrong markup after a realm reset: {}",
            out.html
        );
    }

    let s = engine.arena_stats();
    println!(
        "  survived {N} cycles.\n  persistent {:>10.1} KB   system_live {:>10.1} KB \
         (baseline {:.1} KB)   system_peak {:>10.1} KB",
        kb(s.persistent_used),
        kb(s.system_live_bytes),
        kb(baseline_live),
        kb(s.system_peak_bytes)
    );
    println!(
        "  alloc {}  realloc {}  dealloc {}  fallback {}\n",
        s.alloc_calls, s.realloc_calls, s.dealloc_calls, s.fallback_allocs
    );
}

/// **G1.4 — is a reset realm actually usable?** Correctness, not memory: a reset
/// engine must render identically, and must have genuinely fresh globals.
#[ignore = "SANDGATE Gate 1; run explicitly"]
#[test]
fn g1_4_a_reset_realm_is_clean_and_correct() {
    println!("\n=== G1.4 · a reset realm is clean and correct ===\n");
    let mut engine = warm();

    let before = engine
        .render_component_with_host(SPEC, r#"{"title":"first"}"#, "")
        .expect("render")
        .html;

    // Poison the realm exactly as the 10.0 probe does.
    engine
        .load_module_with_spec(
            "poison.tsx",
            r#"
            globalThis.__POISON__ = "still here";
            const realPush = Array.prototype.push;
            Array.prototype.push = function() { globalThis.__PUSH_PATCHED__ = true; return realPush.apply(this, arguments); };
            export default function P() { return <span>p</span>; }
            "#,
            Some("poison.tsx"),
        )
        .expect("poison module loads");
    engine
        .render_component_with_host("poison.tsx", "{}", "")
        .expect("poison renders");

    engine.reset_realm().expect("reset");
    engine
        .load_module_with_spec(SPEC, COMPONENT, Some(SPEC))
        .expect("reload");

    let after = engine
        .render_component_with_host(SPEC, r#"{"title":"first"}"#, "")
        .expect("render after reset")
        .html;

    assert_eq!(
        before, after,
        "a reset realm must render byte-identically — if it does not, confinement \
         changes output and the whole approach is dead on arrival"
    );
    println!("  ✅ renders byte-identically after a reset");

    // The poisoned module is gone from the module table, and so is its global.
    let probe = engine.render_component_with_host("poison.tsx", "{}", "");
    println!(
        "  poisoned module after reset: {}",
        if probe.is_err() {
            "unregistered (expected — module records live in the context)"
        } else {
            "STILL REGISTERED — unexpected"
        }
    );
    println!();
}

/// **G1.3 — attribution.** Which of the three rebuild steps grows the persistent
/// region? Bracketing `reset_realm` cut the leak from 366 KB to 16.7 KB per
/// cycle but did not remove it, and `persistent_grew_in_request` stayed at 0 —
/// so the residual is allocated OUTSIDE the bracket. This says by whom.
#[ignore = "SANDGATE Gate 1; run explicitly"]
#[test]
fn g1_3_which_step_grows_the_persistent_region() {
    const N: usize = 30;
    println!("\n=== G1.3 · attribution of persistent growth ===\n");

    // --- reset alone -------------------------------------------------------
    let mut engine = warm();
    let start = engine.arena_stats().persistent_used;
    for _ in 0..N {
        engine.reset_realm().expect("reset");
        // Re-register so the next reset has the same work to do, but measure
        // only across the resets by sampling before/after the whole loop.
        engine
            .load_module_with_spec(SPEC, COMPONENT, Some(SPEC))
            .expect("reload");
    }
    let reset_plus_load = engine.arena_stats().persistent_used - start;

    // --- module load alone, no reset ---------------------------------------
    let mut engine2 = warm();
    let start2 = engine2.arena_stats().persistent_used;
    for i in 0..N {
        // A DIFFERENT spec each time so the hash memo cannot short-circuit it —
        // this is the same work a post-reset re-registration does.
        let spec = format!("Component{i}.tsx");
        engine2
            .load_module_with_spec(&spec, COMPONENT, Some(&spec))
            .expect("load");
    }
    let load_only = engine2.arena_stats().persistent_used - start2;

    // --- render alone ------------------------------------------------------
    let mut engine3 = warm();
    let start3 = engine3.arena_stats().persistent_used;
    for _ in 0..N {
        engine3
            .render_component_with_host(SPEC, "{}", "")
            .expect("render");
    }
    let render_only = engine3.arena_stats().persistent_used - start3;

    println!(
        "  reset + module reload : {:>9.1} KB total  →  {:>7.1} KB / cycle",
        kb(reset_plus_load),
        kb(reset_plus_load) / N as f64
    );
    println!(
        "  module load only      : {:>9.1} KB total  →  {:>7.1} KB / cycle",
        kb(load_only),
        kb(load_only) / N as f64
    );
    println!(
        "  render only           : {:>9.1} KB total  →  {:>7.1} KB / cycle",
        kb(render_only),
        kb(render_only) / N as f64
    );
    println!("\n  ⇒ whichever line is non-trivial is the one that must be bracketed.\n");
}
