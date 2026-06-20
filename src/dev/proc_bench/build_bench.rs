//! Workstream C · build-time clean-vs-incremental bench.
//!
//! Pins the wall-clock difference between a cold `albedo build` (no
//! cache, every component re-analysed) and a warm incremental build (the
//! `IncrementalCache` from [`crate::incremental`] hits, unchanged
//! components skip re-analysis). That delta is the headline number for
//! the incremental-build claim — measured, not asserted.
//!
//! The orchestration is written against a [`BuildWorkload`] trait so the
//! sequencing + stats are unit-tested with a fake (no real `albedo
//! build` subprocess), while the CLI fills it with
//! [`CommandBuildWorkload`] — a real `albedo build` invocation plus the
//! artifact paths to wipe for a clean run.

use crate::dev::serve_bench::LatencyStats;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// One buildable project under test. `clean` removes all build outputs
/// + incremental cache so the next `build` is genuinely cold; `build`
/// runs one build and errors if it fails (a failed build's timing is
/// meaningless and must abort the run).
pub trait BuildWorkload {
    /// Remove build artifacts + the incremental cache.
    fn clean(&self) -> std::io::Result<()>;
    /// Run a single build to completion. `Err` aborts the bench.
    fn build(&self) -> Result<(), String>;
}

#[derive(Debug, thiserror::Error)]
pub enum BuildBenchError {
    #[error("clean and incremental sample counts must both be >= 1")]
    NoSamples,
    #[error("failed to clean build artifacts: {0}")]
    Clean(std::io::Error),
    #[error("build failed during the bench: {0}")]
    Build(String),
}

#[derive(Debug, Clone)]
pub struct BuildBenchConfig {
    /// Cold builds to time (each preceded by a `clean`).
    pub clean_samples: u32,
    /// Warm incremental builds to time (cache left intact between them).
    pub incremental_samples: u32,
}

impl Default for BuildBenchConfig {
    fn default() -> Self {
        Self {
            clean_samples: 3,
            incremental_samples: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildBenchReport {
    pub version: String,
    /// Cold-build wall-clock distribution (ms).
    pub clean: LatencyStats,
    /// Warm incremental-build wall-clock distribution (ms).
    pub incremental: LatencyStats,
    /// `clean.p50 / incremental.p50` — how much the cache buys. `0.0`
    /// when the incremental p50 is zero (degenerate / sub-resolution).
    pub speedup: f64,
}

impl BuildBenchReport {
    pub const VERSION: &'static str = "build-bench/1";

    fn compute_speedup(clean: &LatencyStats, incremental: &LatencyStats) -> f64 {
        if incremental.p50_ms > 0.0 {
            clean.p50_ms / incremental.p50_ms
        } else {
            0.0
        }
    }

    /// README-ready Markdown summary.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("### Build time — clean vs incremental\n\n");
        out.push_str("| Build | p50 | p90 | min | max | samples |\n");
        out.push_str("|---|--:|--:|--:|--:|--:|\n");
        let row = |name: &str, s: &LatencyStats| {
            format!(
                "| {name} | {:.1} ms | {:.1} ms | {:.1} ms | {:.1} ms | {} |\n",
                s.p50_ms, s.p90_ms, s.min_ms, s.max_ms, s.count
            )
        };
        out.push_str(&row("clean (cold cache)", &self.clean));
        out.push_str(&row("incremental (warm)", &self.incremental));
        out.push_str(&format!(
            "\nIncremental is **{:.1}×** faster than a clean build (p50).\n",
            self.speedup
        ));
        out
    }
}

/// Run the build-time bench. Times `clean_samples` cold builds (each
/// preceded by a `clean`), then — leaving the now-warm cache intact —
/// times `incremental_samples` warm builds. The first build failure or
/// clean failure aborts with a clear error.
pub fn run_build_bench(
    workload: &dyn BuildWorkload,
    config: &BuildBenchConfig,
) -> Result<BuildBenchReport, BuildBenchError> {
    if config.clean_samples == 0 || config.incremental_samples == 0 {
        return Err(BuildBenchError::NoSamples);
    }

    // Cold builds: wipe everything, then time a from-scratch build.
    let mut clean_ms = Vec::with_capacity(config.clean_samples as usize);
    for _ in 0..config.clean_samples {
        workload.clean().map_err(BuildBenchError::Clean)?;
        let started = Instant::now();
        workload.build().map_err(BuildBenchError::Build)?;
        clean_ms.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    // Warm builds: cache from the final clean build is intact; sources
    // are unchanged so the incremental path should short-circuit most
    // re-analysis.
    let mut incremental_ms = Vec::with_capacity(config.incremental_samples as usize);
    for _ in 0..config.incremental_samples {
        let started = Instant::now();
        workload.build().map_err(BuildBenchError::Build)?;
        incremental_ms.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    let clean = LatencyStats::from_millis(clean_ms);
    let incremental = LatencyStats::from_millis(incremental_ms);
    let speedup = BuildBenchReport::compute_speedup(&clean, &incremental);

    Ok(BuildBenchReport {
        version: BuildBenchReport::VERSION.to_string(),
        clean,
        incremental,
        speedup,
    })
}

/// CLI-facing workload: a real `albedo build` invocation. `clean`
/// removes the listed artifact paths (the `.albedo` dist dir + the
/// incremental cache file); `build` runs the command and treats a
/// non-zero exit as a build failure.
pub struct CommandBuildWorkload {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<std::path::PathBuf>,
    /// Paths removed before each clean build (files or directories).
    pub artifacts: Vec<std::path::PathBuf>,
    /// Inherit child stdout/stderr (false silences the build chatter).
    pub inherit_io: bool,
}

impl BuildWorkload for CommandBuildWorkload {
    fn clean(&self) -> std::io::Result<()> {
        for path in &self.artifacts {
            if path.is_dir() {
                match std::fs::remove_dir_all(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            } else {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn build(&self) -> Result<(), String> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        if !self.inherit_io {
            cmd.stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
        }
        let status = cmd
            .status()
            .map_err(|e| format!("could not run '{}': {e}", self.program))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "build command exited with {}",
                status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Fake workload: each clean increments a counter; each build sleeps
    /// for a configured duration that depends on whether the cache is
    /// "warm" (a build since the last clean). This lets us assert the
    /// orchestration sequences clean→build and warm-build correctly and
    /// that the incremental path is recorded as faster.
    struct FakeWorkload {
        cleans: Arc<AtomicUsize>,
        builds: Arc<AtomicUsize>,
        warm: Arc<std::sync::atomic::AtomicBool>,
        cold_cost: Duration,
        warm_cost: Duration,
    }

    impl BuildWorkload for FakeWorkload {
        fn clean(&self) -> std::io::Result<()> {
            self.cleans.fetch_add(1, Ordering::SeqCst);
            self.warm.store(false, Ordering::SeqCst); // cache wiped → next build cold
            Ok(())
        }
        fn build(&self) -> Result<(), String> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            let cost = if self.warm.load(Ordering::SeqCst) {
                self.warm_cost
            } else {
                self.cold_cost
            };
            std::thread::sleep(cost);
            self.warm.store(true, Ordering::SeqCst); // cache now warm
            Ok(())
        }
    }

    #[test]
    fn sequences_clean_and_incremental_and_records_speedup() {
        let cleans = Arc::new(AtomicUsize::new(0));
        let builds = Arc::new(AtomicUsize::new(0));
        let workload = FakeWorkload {
            cleans: Arc::clone(&cleans),
            builds: Arc::clone(&builds),
            warm: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cold_cost: Duration::from_millis(30),
            warm_cost: Duration::from_millis(5),
        };
        let config = BuildBenchConfig {
            clean_samples: 2,
            incremental_samples: 3,
        };
        let report = run_build_bench(&workload, &config).expect("bench runs");

        assert_eq!(cleans.load(Ordering::SeqCst), 2, "one clean per clean sample");
        assert_eq!(builds.load(Ordering::SeqCst), 5, "2 clean + 3 incremental builds");
        assert_eq!(report.clean.count, 2);
        assert_eq!(report.incremental.count, 3);
        assert!(
            report.clean.p50_ms > report.incremental.p50_ms,
            "cold build must be slower than warm: clean {} vs incr {}",
            report.clean.p50_ms,
            report.incremental.p50_ms
        );
        assert!(report.speedup > 1.0, "speedup recorded > 1");
        assert!(report.to_markdown().contains("faster than a clean build"));
    }

    #[test]
    fn zero_samples_is_an_error() {
        struct Noop;
        impl BuildWorkload for Noop {
            fn clean(&self) -> std::io::Result<()> {
                Ok(())
            }
            fn build(&self) -> Result<(), String> {
                Ok(())
            }
        }
        let config = BuildBenchConfig {
            clean_samples: 0,
            incremental_samples: 1,
        };
        assert!(matches!(
            run_build_bench(&Noop, &config),
            Err(BuildBenchError::NoSamples)
        ));
    }

    #[test]
    fn build_failure_aborts_loudly() {
        struct Failing;
        impl BuildWorkload for Failing {
            fn clean(&self) -> std::io::Result<()> {
                Ok(())
            }
            fn build(&self) -> Result<(), String> {
                Err("boom".to_string())
            }
        }
        let err = run_build_bench(&Failing, &BuildBenchConfig::default())
            .expect_err("must abort on build failure");
        assert!(matches!(err, BuildBenchError::Build(_)));
    }
}
