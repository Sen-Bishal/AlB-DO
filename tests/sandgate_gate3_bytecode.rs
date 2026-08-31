//! SANDGATE · Gate 3.1 — *does bytecode collapse B2?*
//!
//! Gate 1 priced confinement and found the bill is not architecture: **B2, the
//! re-registration of 55 Radix chunks, is 4.874 ms — 71% of a 6.832 ms confined
//! request — and it is spent parsing JavaScript.** SANDGATE § 3 names the lever
//! as bytecode (`Module::write` / `Module::load`, both present in rquickjs 0.9)
//! and says to run this **before building any pool machinery**, because a large
//! enough win means no background pool is needed at all.
//!
//! ## What this measures, and what it deliberately does not
//!
//! This is a question about **QuickJS**, not about our engine wiring, so it runs
//! against a bare `Runtime`/`Context` rather than `QuickJsEngine`. Registering a
//! module is either:
//!
//! * **today** — `ctx.eval(script)`: tokenize, parse, emit bytecode, build closures.
//! * **bytecode** — `Module::load(bytes)` + `eval()`: `JS_ReadObject` and run.
//!
//! Both are timed into a **fresh `Context` on the same warm `Runtime`**, which is
//! exactly the shape `rebuild_realm` produces, so the numbers are comparable to
//! B2 rather than to a synthetic loop.
//!
//! 🪤 **The obstacle SANDGATE § 3 flagged is real and is measured here rather
//! than assumed.** Our artifacts register as *scripts* (`__ALBEDO_MODULES[k] = …`),
//! and `JS_EVAL_FLAG_COMPILE_ONLY` — the only route to bytecode — is reachable
//! only through `Module::declare`, which also forces `JS_EVAL_TYPE_MODULE` and
//! `JS_EVAL_FLAG_STRICT`. So the first thing this reports is **how many of the
//! real chunks survive being compiled as strict module source at all**. A chunk
//! that will not compile is not a chunk bytecode can help, and the headline
//! comparison is run on the surviving subset so it stays apples-to-apples.
//!
//! Run:
//! ```text
//! cargo test --release --test sandgate_gate3_bytecode -- --ignored --nocapture
//! ```
//!
//! ⚠️ **Release only.** A debug build measures rustc's inlining, not QuickJS.

// AUDITED EXCEPTION — `Module::load` is `unsafe` because QuickJS will happily
// read malformed bytecode into a live realm. Here the bytes come from
// `Module::write` on this same process's `Runtime`, microseconds earlier, and
// never leave this test binary; no untrusted input can reach the call. There is
// no safe route to `JS_ReadObject`, and measuring the bytecode path is the whole
// point of gate 3.1 — see the `// SAFETY:` argument at the site.
#![cfg(feature = "forge")]
#![allow(unsafe_code)]

use dom_render_compiler::bundler::client_npm::server_shake_options;
use dom_render_compiler::bundler::npm::bundle_npm_dependency;
use rquickjs::{Context, Module, Runtime};
use std::time::{Duration, Instant};

const N: usize = 30;

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn report(label: &str, samples: &[Duration]) {
    let mut sorted: Vec<f64> = samples.iter().copied().map(ms).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let median = sorted[sorted.len() / 2];
    println!(
        "  {label:<44} mean {mean:>7.3} ms   median {median:>7.3} ms   min {:>7.3} ms",
        sorted[0]
    );
    // Returned via the printed line only: this test reports, it does not assert
    // on a threshold. A timing assert on this machine is a flake generator, and
    // the decision it informs (pool or no pool) is a human one.
}

/// Install **the production linker**, not a hand-rolled stand-in.
///
/// `npm_record_linker_script()` is the same source `QuickJsEngine` evaluates, so
/// it defines `__ALBEDO_NPM_FACTORIES`, `__ALBEDO_NPM_ALIASES`, `__ALBEDO_MODULES`
/// and `__albedo_require_record` exactly as production does. An artifact
/// registers a factory (or an alias) into those tables; the factory body does not
/// run until a `require`, so this is all a chunk needs to register.
///
/// 🪤 This started life as a two-line stub naming `__ALBEDO_MODULES`, which is
/// the *record cache*, not the factory table. Every script threw on its first
/// statement, the timed loop swallowed it with `let _ =`, and the equivalence
/// check compared two empty tables and passed — a 6× "speedup" measuring
/// parse-then-throw against load-then-reject. **Guessing the prelude is what
/// produced the wrong number; calling the real one is the fix**, and the
/// assertions below are non-vacuous so the same mistake fails loudly next time.
fn install_linker(ctx: &Context) {
    let linker = dom_render_compiler::runtime::quickjs_engine::npm_record_linker_script();
    ctx.with(|ctx| {
        ctx.eval::<(), _>(linker.as_str())
            .expect("npm record linker installs");
    });
}

#[ignore = "reads the external corpus at C:/Development/albedo-corpus; run with --release"]
#[test]
fn does_bytecode_collapse_module_reregistration() {
    let root = std::path::Path::new("C:/Development/albedo-corpus/shadcn-taxonomy");
    if !root.join("node_modules/@radix-ui/react-dialog").is_dir() {
        println!("SKIPPED — corpus not installed");
        return;
    }

    let bundle = bundle_npm_dependency(root, "@radix-ui/react-dialog", &server_shake_options())
        .expect("bundles");
    let scripts: Vec<(String, String)> = bundle
        .artifacts
        .iter()
        .map(|a| (a.key.clone(), a.script.clone()))
        .collect();
    let source_bytes: usize = scripts.iter().map(|(_, s)| s.len()).sum();

    println!(
        "\n=== SANDGATE 3.1 · does bytecode collapse B2? ===\n\n  \
         corpus: @radix-ui/react-dialog — {} chunks, {:.1} KB of source\n",
        scripts.len(),
        source_bytes as f64 / 1024.0
    );

    let rt = Runtime::new().expect("runtime");

    // ── phase 1 · can these compile to bytecode at all? ────────────────────
    //
    // Done in a throwaway context so a half-registered module table cannot
    // colour the timings below.
    let mut compiled: Vec<(String, Vec<u8>)> = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    {
        let ctx = Context::full(&rt).expect("context");
        install_linker(&ctx);
        ctx.with(|ctx| {
            for (key, script) in &scripts {
                match Module::declare(ctx.clone(), key.as_str(), script.as_str())
                    .and_then(|m| m.write(false))
                {
                    Ok(bytes) => compiled.push((key.clone(), bytes)),
                    Err(err) => refused.push((key.clone(), err.to_string())),
                }
            }
        });
    }

    let bytecode_bytes: usize = compiled.iter().map(|(_, b)| b.len()).sum();
    let compiled_source_bytes: usize = scripts
        .iter()
        .filter(|(k, _)| compiled.iter().any(|(ck, _)| ck == k))
        .map(|(_, s)| s.len())
        .sum();

    println!(
        "  compiles as strict module source : {} / {}",
        compiled.len(),
        scripts.len()
    );
    if !refused.is_empty() {
        println!("  🔴 refused ({}):", refused.len());
        for (key, err) in refused.iter().take(8) {
            let first_line = err.lines().next().unwrap_or("").trim();
            println!("      {key} — {first_line}");
        }
        if refused.len() > 8 {
            println!("      … and {} more", refused.len() - 8);
        }
    }
    println!(
        "  bytecode size                    : {:.1} KB for {:.1} KB of source ({:.2}×)\n",
        bytecode_bytes as f64 / 1024.0,
        compiled_source_bytes as f64 / 1024.0,
        bytecode_bytes as f64 / compiled_source_bytes.max(1) as f64
    );

    if compiled.is_empty() {
        println!(
            "  ⇒ VERDICT: bytecode is unreachable for this corpus without changing how\n     \
             artifacts are registered. Gate 3.1 answered NO on the current shape.\n"
        );
        return;
    }

    let compilable: Vec<&(String, String)> = scripts
        .iter()
        .filter(|(k, _)| compiled.iter().any(|(ck, _)| ck == k))
        .collect();

    // ── phase 1b · EQUIVALENCE, before any timing is believed ─────────────
    //
    // 🔑 A module that loads fast and registers nothing is fast and useless, and
    // it would not error — `Module::declare` forces `JS_EVAL_TYPE_MODULE`, which
    // is strict mode with `this === undefined` at top level, so a CJS artifact
    // that leans on either would go quietly wrong rather than throw. Compare the
    // module tables the two paths actually produce before reporting a speedup.
    let table_after = |run: &dyn Fn(&rquickjs::Ctx)| -> Vec<String> {
        let ctx = Context::full(&rt).expect("context");
        install_linker(&ctx);
        ctx.with(|ctx| {
            run(&ctx);
            // Both tables: an artifact registers exactly one entry, and a
            // subpath artifact registers an *alias* rather than a factory.
            // Counting only factories reports 54 of 55 and looks like a loss.
            let mut keys: Vec<String> = ctx
                .eval::<Vec<String>, _>(
                    "Object.keys(globalThis.__ALBEDO_NPM_FACTORIES).map(function (k) { return 'factory:' + k; })
                       .concat(Object.keys(globalThis.__ALBEDO_NPM_ALIASES).map(function (k) { return 'alias:' + k; }))",
                )
                .expect("module tables readable");
            keys.sort();
            keys
        })
    };

    let eval_table = table_after(&|ctx| {
        for (key, script) in &compilable {
            ctx.eval::<(), _>(script.as_str())
                .unwrap_or_else(|e| panic!("script path failed on {key}: {e}"));
        }
    });
    let bytecode_table = table_after(&|ctx| {
        for (_, bytes) in &compiled {
            // SAFETY: as at the timed site below — bytes produced by
            // `Module::write` in this process moments ago, never untrusted.
            let module =
                unsafe { Module::load(ctx.clone(), bytes.as_slice()) }.expect("bytecode loads");
            let _ = module.eval().expect("bytecode evaluates");
        }
    });

    println!(
        "  registers the same module table  : {}  (eval {} keys · bytecode {} keys)",
        if eval_table == bytecode_table {
            "✅ yes"
        } else {
            "🔴 NO — the speedup below is fiction"
        },
        eval_table.len(),
        bytecode_table.len()
    );
    if eval_table != bytecode_table {
        let missing: Vec<&String> = eval_table
            .iter()
            .filter(|k| !bytecode_table.contains(k))
            .collect();
        let extra: Vec<&String> = bytecode_table
            .iter()
            .filter(|k| !eval_table.contains(k))
            .collect();
        println!("      missing under bytecode: {missing:?}");
        println!("      extra under bytecode  : {extra:?}");
    }
    // 🔑 NON-VACUOUS, deliberately, and checked FIRST. `eval_table ==
    // bytecode_table` is satisfied by two empty tables — which is exactly how an
    // earlier version of this file passed while timing parse-then-throw against
    // load-then-reject. Pin the count against the corpus before believing the
    // equality.
    assert_eq!(
        eval_table.len(),
        scripts.len(),
        "\n🔴 the script path registered {} entries for {} artifacts — the corpus is \
         not being exercised and every timing below measures the wrong work.\n",
        eval_table.len(),
        scripts.len()
    );
    assert_eq!(
        eval_table, bytecode_table,
        "\n🔴 the bytecode path does not reproduce the script path's module table. \
         Any timing below is measuring the wrong work.\n"
    );
    println!();

    // ── phase 2 · today's path, whole corpus (should reproduce B2) ─────────
    let mut eval_all = Vec::with_capacity(N);
    for _ in 0..N {
        let ctx = Context::full(&rt).expect("context");
        install_linker(&ctx);
        let t = Instant::now();
        ctx.with(|ctx| {
            for (_, script) in &scripts {
                ctx.eval::<(), _>(script.as_str()).expect("script registers");
            }
        });
        eval_all.push(t.elapsed());
    }
    report("eval — all chunks (today, = B2)", &eval_all);

    // ── phase 3 · today's path, compilable subset ─────────────────────────
    let mut eval_subset = Vec::with_capacity(N);
    for _ in 0..N {
        let ctx = Context::full(&rt).expect("context");
        install_linker(&ctx);
        let t = Instant::now();
        ctx.with(|ctx| {
            for (_, script) in &compilable {
                ctx.eval::<(), _>(script.as_str()).expect("script registers");
            }
        });
        eval_subset.push(t.elapsed());
    }
    report("eval — compilable subset", &eval_subset);

    // ── phase 4 · bytecode path, same subset ──────────────────────────────
    let mut load_subset = Vec::with_capacity(N);
    for _ in 0..N {
        let ctx = Context::full(&rt).expect("context");
        install_linker(&ctx);
        let t = Instant::now();
        ctx.with(|ctx| {
            for (_, bytes) in &compiled {
                // SAFETY: `bytes` came from `Module::write` on this same
                // process's `Runtime` a few microseconds ago, so it is
                // well-formed QuickJS bytecode of matching endianness and
                // version. Nothing untrusted reaches this call.
                let module = unsafe { Module::load(ctx.clone(), bytes.as_slice()) }
                    .expect("bytecode loads");
                let _ = module.eval().expect("bytecode evaluates");
            }
        });
        load_subset.push(t.elapsed());
    }
    report("bytecode — same subset", &load_subset);

    let eval_mean = eval_subset.iter().map(|d| ms(*d)).sum::<f64>() / N as f64;
    let load_mean = load_subset.iter().map(|d| ms(*d)).sum::<f64>() / N as f64;
    let all_mean = eval_all.iter().map(|d| ms(*d)).sum::<f64>() / N as f64;
    let saved = eval_mean - load_mean;

    println!(
        "\n  speedup on the subset            : {:.2}×  ({:.3} ms → {:.3} ms)",
        eval_mean / load_mean.max(f64::MIN_POSITIVE),
        eval_mean,
        load_mean
    );
    println!(
        "  best case against the whole B2   : {:.3} ms → {:.3} ms  ({:.0}% of B2 removed)",
        all_mean,
        all_mean - saved,
        (saved / all_mean.max(f64::MIN_POSITIVE)) * 100.0
    );
    println!(
        "\n  reading: a confined request was 6.832 ms with B2 at 4.874 ms. Subtract the\n  \
         saving above from B2 to get the confined request bytecode would buy, then ask\n  \
         whether THAT number still needs a background pool.\n"
    );
}
