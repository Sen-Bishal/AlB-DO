//! Phase P · Stream G — Cold start time for the production server.
//!
//! Measures the **elapsed time from `boot_production_server` entry
//! to a ready `AlbedoServer` instance**: `RendererRuntime::from_artifacts_dir`
//! (manifest + module source loading + QuickJS engine warm-up + cache
//! priming) plus `CompiledProject::load_from_dir` (parse every
//! `.tsx`, Phase K metadata extraction) plus the
//! `AlbedoServerBuilder::build` finalize pass.
//!
//! This is the "time-to-first-byte after `albedo serve` starts"
//! number. ALBEDO's single-binary architecture avoids a Node.js
//! boot + `next build` warm-up — the entire serve path is one
//! Rust process loading bincode + JSON manifests.
//!
//! Reference: `next start` for a 10-route Next.js app typically
//! takes 1–3 seconds to ready depending on bundle size. ALBEDO's
//! cold start is dominated by SWC parse + QuickJS warm-up; the
//! number below is the load-cost on the user's machine.
//!
//! Cold start is genuinely one-shot work — `sample_size(10)` keeps
//! Criterion's iteration count tractable while still producing
//! meaningful confidence intervals.
//!
//! Reproduce with:
//!   cargo bench --bench parity_cold_start

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use std::time::Duration;

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("layouts")
}

/// Boot a `CompiledProject` from disk — the heavier of the two
/// passes `boot_production_server` performs. Reads + parses every
/// `.tsx` under the fixture's source root, runs Phase K metadata
/// extraction across every function, builds the CSS-module registry.
fn measure_compiled_project_load() {
    let project = dom_render_compiler::runtime::eval::CompiledProject::load_from_dir(
        fixture_root(),
    )
    .expect("load compiled project");
    black_box(project);
}

fn bench_cold_start(c: &mut Criterion) {
    let mut group = c.benchmark_group("cold_start");
    // Cold start is expensive (disk reads + parse + Phase K
    // extraction); 10 samples is enough for stable means without
    // making the bench run for minutes. Measurement time matches.
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));

    group.bench_function("compiled_project_load_from_dir", |b| {
        b.iter(measure_compiled_project_load);
    });

    group.finish();
}

criterion_group!(benches, bench_cold_start);
criterion_main!(benches);
