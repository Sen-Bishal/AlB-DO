use dom_render_compiler::benchmark::{
    run_workloads, write_report_json, BaselineEnvelopeFile, BenchmarkWorkloads, GateStatus,
};
use std::path::PathBuf;

#[derive(Debug, Clone)]
struct CliOptions {
    config_path: PathBuf,
    baseline_path: Option<PathBuf>,
    output_path: Option<PathBuf>,
    assert_gates: bool,
    project_root: PathBuf,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let options = parse_args(std::env::args().skip(1).collect())?;
    let workloads = BenchmarkWorkloads::load(&options.config_path)
        .map_err(|err| format!("failed to load workloads: {err}"))?;
    let baseline = if let Some(path) = &options.baseline_path {
        Some(
            BaselineEnvelopeFile::load(path)
                .map_err(|err| format!("failed to load baseline: {err}"))?,
        )
    } else {
        None
    };

    let report = run_workloads(&options.project_root, &workloads, baseline.as_ref())
        .map_err(|err| format!("benchmark run failed: {err}"))?;

    print_summary(&report);

    if let Some(output) = &options.output_path {
        write_report_json(&report, output)
            .map_err(|err| format!("failed to write report '{}': {err}", output.display()))?;
        println!("Wrote benchmark report: {}", output.display());
    }

    if options.assert_gates && report.overall_status == GateStatus::Fail {
        return Err("benchmark gates failed".to_string());
    }

    Ok(())
}

fn parse_args(args: Vec<String>) -> Result<CliOptions, String> {
    let mut config_path = PathBuf::from("benchmarks/workloads.json");
    let mut baseline_path = Some(PathBuf::from("benchmarks/baseline.json"));
    let mut output_path = Some(PathBuf::from("target/benchmarks/latest.json"));
    let mut assert_gates = false;
    let mut project_root = PathBuf::from(".");

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--config" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --config".to_string())?;
                config_path = PathBuf::from(value);
            }
            "--baseline" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --baseline".to_string())?;
                baseline_path = Some(PathBuf::from(value));
            }
            "--no-baseline" => {
                baseline_path = None;
            }
            "--output" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output_path = Some(PathBuf::from(value));
            }
            "--no-output" => {
                output_path = None;
            }
            "--assert-gates" => {
                assert_gates = true;
            }
            "--project-root" => {
                idx += 1;
                let value = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --project-root".to_string())?;
                project_root = PathBuf::from(value);
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            unknown => {
                return Err(format!("unknown argument '{unknown}'"));
            }
        }
        idx += 1;
    }

    Ok(CliOptions {
        config_path,
        baseline_path,
        output_path,
        assert_gates,
        project_root,
    })
}

fn print_summary(report: &dom_render_compiler::benchmark::BenchmarkReport) {
    println!("ALBEDO Benchmark Report");
    println!("  workload_version: {}", report.workload_version);
    println!(
        "  regression_policy: max {}%",
        report.regression_policy.max_regression_percent
    );
    println!("  scenarios: {}", report.scenarios.len());
    println!();

    for scenario in &report.scenarios {
        println!("Scenario: {} ({})", scenario.id, scenario.name);
        println!("  path: {}", scenario.path);
        println!("  components: {}", scenario.component_count);
        println!(
            "  scan_ms: p50={:.2} p95={:.2}",
            scenario.metrics.scan_ms.p50, scenario.metrics.scan_ms.p95
        );
        println!(
            "  optimize_ms: p50={:.2} p95={:.2}",
            scenario.metrics.optimize_ms.p50, scenario.metrics.optimize_ms.p95
        );
        println!(
            "  total_ms: p50={:.2} p95={:.2}",
            scenario.metrics.total_ms.p50, scenario.metrics.total_ms.p95
        );
        println!(
            "  gate: {}",
            if scenario.gate.passed { "pass" } else { "fail" }
        );
        for failure in &scenario.gate.failures {
            println!("    - {failure}");
        }
        println!();
    }

    println!(
        "Overall: {}",
        match report.overall_status {
            GateStatus::Pass => "pass",
            GateStatus::Fail => "fail",
        }
    );
}

fn print_usage() {
    println!("Usage: albedo-bench [OPTIONS]");
    println!();
    println!("Options:");
    println!(
        "  --config <FILE>        Workload configuration file (default: benchmarks/workloads.json)"
    );
    println!("  --baseline <FILE>      Baseline envelope file (default: benchmarks/baseline.json)");
    println!("  --no-baseline          Disable baseline envelope checks");
    println!(
        "  --output <FILE>        Output report JSON path (default: target/benchmarks/latest.json)"
    );
    println!("  --no-output            Disable writing report JSON");
    println!("  --assert-gates         Exit with failure when any scenario gate fails");
    println!("  --project-root <DIR>   Project root for scenario paths (default: .)");
}
