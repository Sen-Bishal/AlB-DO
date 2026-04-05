use crate::scanner::ProjectScanner;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use thiserror::Error;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Error)]
pub enum BenchmarkError {
    #[error("failed to read '{path}': {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse JSON '{path}': {source}")]
    ParseJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("workload scenario '{id}' path does not exist: {path}")]
    MissingScenarioPath { id: String, path: PathBuf },
    #[error("workload scenario '{id}' path is not a directory: {path}")]
    InvalidScenarioPath { id: String, path: PathBuf },
    #[error("workload scenario '{id}' has sample_runs=0")]
    InvalidSampleRuns { id: String },
    #[error("scan failed for scenario '{id}': {source}")]
    Scan {
        id: String,
        source: crate::types::CompilerError,
    },
    #[error("optimize failed for scenario '{id}': {source}")]
    Optimize {
        id: String,
        source: crate::types::CompilerError,
    },
    #[error("failed to serialize benchmark report: {0}")]
    SerializeReport(#[from] serde_json::Error),
    #[error("failed to write benchmark report '{path}': {source}")]
    WriteReport {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, BenchmarkError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionPolicy {
    pub max_regression_percent: f64,
}

impl Default for RegressionPolicy {
    fn default() -> Self {
        Self {
            max_regression_percent: 10.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineCompetitor {
    pub id: String,
    pub name: String,
    pub category: String,
    pub benchmark_notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkScenario {
    pub id: String,
    pub name: String,
    pub path: String,
    pub warmup_runs: u32,
    pub sample_runs: u32,
    pub max_p95_scan_ms: f64,
    pub max_p95_optimize_ms: f64,
    pub max_p95_total_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkWorkloads {
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub regression_policy: RegressionPolicy,
    #[serde(default)]
    pub baseline_competitors: Vec<BaselineCompetitor>,
    pub scenarios: Vec<BenchmarkScenario>,
}

impl BenchmarkWorkloads {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| BenchmarkError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice::<Self>(&bytes).map_err(|source| BenchmarkError::ParseJson {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineEnvelopeFile {
    pub version: String,
    pub created_at: String,
    pub notes: String,
    pub scenarios: BTreeMap<String, BaselineScenarioEnvelope>,
}

impl BaselineEnvelopeFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| BenchmarkError::ReadFile {
            path: path.to_path_buf(),
            source,
        })?;
        serde_json::from_slice::<Self>(&bytes).map_err(|source| BenchmarkError::ParseJson {
            path: path.to_path_buf(),
            source,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineScenarioEnvelope {
    pub p95_scan_ms: f64,
    pub p95_optimize_ms: f64,
    pub p95_total_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricSummary {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioMetrics {
    pub scan_ms: MetricSummary,
    pub optimize_ms: MetricSummary,
    pub total_ms: MetricSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioGateReport {
    pub passed: bool,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioBenchmarkResult {
    pub id: String,
    pub name: String,
    pub path: String,
    pub warmup_runs: u32,
    pub sample_runs: u32,
    pub component_count: usize,
    pub metrics: ScenarioMetrics,
    pub gate: ScenarioGateReport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub version: String,
    pub generated_at: String,
    pub workload_version: String,
    pub regression_policy: RegressionPolicy,
    pub baseline_competitors: Vec<BaselineCompetitor>,
    pub scenarios: Vec<ScenarioBenchmarkResult>,
    pub overall_status: GateStatus,
}

pub fn run_workloads(
    project_root: impl AsRef<Path>,
    workloads: &BenchmarkWorkloads,
    baseline: Option<&BaselineEnvelopeFile>,
) -> Result<BenchmarkReport> {
    let project_root = project_root.as_ref();
    let mut scenarios = Vec::with_capacity(workloads.scenarios.len());

    for scenario in &workloads.scenarios {
        scenarios.push(run_single_scenario(
            project_root,
            scenario,
            baseline.and_then(|b| b.scenarios.get(&scenario.id)),
            workloads.regression_policy.max_regression_percent,
        )?);
    }

    let overall_status = if scenarios.iter().all(|s| s.gate.passed) {
        GateStatus::Pass
    } else {
        GateStatus::Fail
    };

    Ok(BenchmarkReport {
        version: "1.0".to_string(),
        generated_at: now_rfc3339(),
        workload_version: workloads.version.clone(),
        regression_policy: workloads.regression_policy.clone(),
        baseline_competitors: workloads.baseline_competitors.clone(),
        scenarios,
        overall_status,
    })
}

pub fn write_report_json(report: &BenchmarkReport, output: impl AsRef<Path>) -> Result<()> {
    let output = output.as_ref();
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|source| BenchmarkError::WriteReport {
            path: output.to_path_buf(),
            source,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(report)?;
    fs::write(output, bytes).map_err(|source| BenchmarkError::WriteReport {
        path: output.to_path_buf(),
        source,
    })?;
    Ok(())
}

fn run_single_scenario(
    project_root: &Path,
    scenario: &BenchmarkScenario,
    baseline: Option<&BaselineScenarioEnvelope>,
    max_regression_percent: f64,
) -> Result<ScenarioBenchmarkResult> {
    if scenario.sample_runs == 0 {
        return Err(BenchmarkError::InvalidSampleRuns {
            id: scenario.id.clone(),
        });
    }

    let scenario_path = project_root.join(&scenario.path);
    if !scenario_path.exists() {
        return Err(BenchmarkError::MissingScenarioPath {
            id: scenario.id.clone(),
            path: scenario_path,
        });
    }
    if !scenario_path.is_dir() {
        return Err(BenchmarkError::InvalidScenarioPath {
            id: scenario.id.clone(),
            path: scenario_path,
        });
    }

    let scanner = ProjectScanner::new();
    let total_runs = scenario.warmup_runs + scenario.sample_runs;
    let mut scan_samples = Vec::with_capacity(scenario.sample_runs as usize);
    let mut optimize_samples = Vec::with_capacity(scenario.sample_runs as usize);
    let mut total_samples = Vec::with_capacity(scenario.sample_runs as usize);
    let mut component_count = 0usize;

    for run_index in 0..total_runs {
        let total_start = Instant::now();

        let scan_start = Instant::now();
        let components =
            scanner
                .scan_directory(&scenario_path)
                .map_err(|source| BenchmarkError::Scan {
                    id: scenario.id.clone(),
                    source,
                })?;
        let scan_ms = scan_start.elapsed().as_secs_f64() * 1000.0;
        component_count = components.len();

        let compiler = scanner.build_compiler(components);

        let optimize_start = Instant::now();
        compiler
            .optimize()
            .map_err(|source| BenchmarkError::Optimize {
                id: scenario.id.clone(),
                source,
            })?;
        let optimize_ms = optimize_start.elapsed().as_secs_f64() * 1000.0;
        let total_ms = total_start.elapsed().as_secs_f64() * 1000.0;

        if run_index >= scenario.warmup_runs {
            scan_samples.push(scan_ms);
            optimize_samples.push(optimize_ms);
            total_samples.push(total_ms);
        }
    }

    let scan_metrics = summarize_samples(&scan_samples);
    let optimize_metrics = summarize_samples(&optimize_samples);
    let total_metrics = summarize_samples(&total_samples);

    let mut failures = Vec::new();
    if scan_metrics.p95 > scenario.max_p95_scan_ms {
        failures.push(format!(
            "p95 scan_ms {:.2} exceeded scenario gate {:.2}",
            scan_metrics.p95, scenario.max_p95_scan_ms
        ));
    }
    if optimize_metrics.p95 > scenario.max_p95_optimize_ms {
        failures.push(format!(
            "p95 optimize_ms {:.2} exceeded scenario gate {:.2}",
            optimize_metrics.p95, scenario.max_p95_optimize_ms
        ));
    }
    if total_metrics.p95 > scenario.max_p95_total_ms {
        failures.push(format!(
            "p95 total_ms {:.2} exceeded scenario gate {:.2}",
            total_metrics.p95, scenario.max_p95_total_ms
        ));
    }

    if let Some(base) = baseline {
        let limit = 1.0 + (max_regression_percent.max(0.0) / 100.0);
        check_baseline_regression(
            "scan_ms",
            scan_metrics.p95,
            base.p95_scan_ms,
            limit,
            &mut failures,
        );
        check_baseline_regression(
            "optimize_ms",
            optimize_metrics.p95,
            base.p95_optimize_ms,
            limit,
            &mut failures,
        );
        check_baseline_regression(
            "total_ms",
            total_metrics.p95,
            base.p95_total_ms,
            limit,
            &mut failures,
        );
    }

    Ok(ScenarioBenchmarkResult {
        id: scenario.id.clone(),
        name: scenario.name.clone(),
        path: scenario.path.clone(),
        warmup_runs: scenario.warmup_runs,
        sample_runs: scenario.sample_runs,
        component_count,
        metrics: ScenarioMetrics {
            scan_ms: scan_metrics,
            optimize_ms: optimize_metrics,
            total_ms: total_metrics,
        },
        gate: ScenarioGateReport {
            passed: failures.is_empty(),
            failures,
        },
    })
}

fn check_baseline_regression(
    metric_label: &str,
    measured: f64,
    baseline: f64,
    limit_ratio: f64,
    failures: &mut Vec<String>,
) {
    if baseline <= 0.0 {
        return;
    }
    let allowed = baseline * limit_ratio;
    if measured > allowed {
        failures.push(format!(
            "p95 {metric_label} {:.2} exceeded baseline envelope {:.2} (baseline {:.2}, limit {:.0}%)",
            measured,
            allowed,
            baseline,
            (limit_ratio - 1.0) * 100.0
        ));
    }
}

fn summarize_samples(samples: &[f64]) -> MetricSummary {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let min = *sorted.first().unwrap_or(&0.0);
    let max = *sorted.last().unwrap_or(&0.0);
    let mean = if sorted.is_empty() {
        0.0
    } else {
        sorted.iter().sum::<f64>() / sorted.len() as f64
    };

    MetricSummary {
        min,
        max,
        mean,
        p50: percentile_nearest_rank(&sorted, 50.0),
        p95: percentile_nearest_rank(&sorted, 95.0),
    }
}

fn percentile_nearest_rank(sorted_samples: &[f64], percentile: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let p = percentile.clamp(0.0, 100.0);
    let n = sorted_samples.len() as f64;
    let rank = ((p / 100.0) * n).ceil().max(1.0) as usize;
    let index = rank.saturating_sub(1).min(sorted_samples.len() - 1);
    sorted_samples[index]
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percentile_nearest_rank() {
        let samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile_nearest_rank(&samples, 50.0), 3.0);
        assert_eq!(percentile_nearest_rank(&samples, 95.0), 5.0);
    }

    #[test]
    fn test_summarize_samples() {
        let summary = summarize_samples(&[10.0, 20.0, 30.0, 40.0, 50.0]);
        assert_eq!(summary.min, 10.0);
        assert_eq!(summary.max, 50.0);
        assert_eq!(summary.p50, 30.0);
        assert_eq!(summary.p95, 50.0);
    }

    #[test]
    fn test_check_baseline_regression_adds_failure_on_regression() {
        let mut failures = Vec::new();
        check_baseline_regression("total_ms", 130.0, 100.0, 1.2, &mut failures);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("total_ms"));
    }
}
