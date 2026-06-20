use dom_render_compiler::benchmark::{
    run_workloads, write_report_json, BaselineEnvelopeFile, BenchmarkWorkloads, GateStatus,
};
use dom_render_compiler::dev::serve_bench::{
    run as run_serve_bench, RequestSpec, ServeBenchConfig, ServeBenchReport,
};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
struct CliOptions {
    config_path: PathBuf,
    baseline_path: Option<PathBuf>,
    output_path: Option<PathBuf>,
    assert_gates: bool,
    project_root: PathBuf,
}

/// Workstream C · serve-time latency options. Active when `--serve
/// <url>` is passed; otherwise the binary runs the build-time
/// (scan/optimize) workload path.
#[derive(Debug, Clone)]
struct ServeOptions {
    base_url: String,
    paths: Vec<String>,
    warmup: u32,
    samples: u32,
    concurrency: usize,
    timeout: Duration,
    keep_alive: bool,
    output_path: Option<PathBuf>,
    markdown: bool,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Serve-time latency mode short-circuits the build-time path.
    if let Some(serve) = parse_serve_args(&args)? {
        return run_serve(serve);
    }

    let options = parse_args(args)?;
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

/// Run the serve-time HTTP latency harness against an already-running
/// `albedo serve`. The caller is responsible for booting the server
/// (ideally `--release`) and tearing it down; this just measures.
fn run_serve(options: ServeOptions) -> Result<(), String> {
    let paths = if options.paths.is_empty() {
        vec!["/".to_string()]
    } else {
        options.paths.clone()
    };
    let requests = paths
        .iter()
        .map(|p| {
            let name = if p == "/" { "root" } else { p.trim_start_matches('/') };
            RequestSpec::get(name.to_string(), p.clone())
        })
        .collect::<Vec<_>>();

    let config = ServeBenchConfig {
        base_url: options.base_url.clone(),
        warmup: options.warmup,
        samples: options.samples,
        concurrency: options.concurrency,
        timeout: options.timeout,
        keep_alive: options.keep_alive,
        requests,
    };

    eprintln!(
        "serve-bench → {} · warmup {} · samples {} · concurrency {} · {}",
        config.base_url,
        config.warmup,
        config.samples,
        config.concurrency,
        if config.keep_alive { "keep-alive" } else { "close" }
    );

    let report = run_serve_bench(&config).map_err(|err| format!("serve bench failed: {err}"))?;

    print_serve_summary(&report);

    if options.markdown {
        println!("\n{}", report.to_markdown());
    }

    if let Some(output) = &options.output_path {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|err| format!("failed to serialize serve report: {err}"))?;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(output, json)
            .map_err(|err| format!("failed to write report '{}': {err}", output.display()))?;
        println!("Wrote serve-bench report: {}", output.display());
    }

    // A non-2xx endpoint means the latency numbers are meaningless —
    // fail loudly rather than let a broken route get cited.
    if let Some(bad) = report.endpoints.iter().find(|e| e.ok_ratio < 1.0) {
        return Err(format!(
            "endpoint '{}' ({} {}) returned non-2xx for {:.0}% of requests — numbers not citable",
            bad.name,
            method_str(bad),
            bad.path,
            (1.0 - bad.ok_ratio) * 100.0
        ));
    }

    Ok(())
}

fn method_str(ep: &dom_render_compiler::dev::serve_bench::EndpointResult) -> &'static str {
    match ep.method {
        dom_render_compiler::dev::serve_bench::Method::Get => "GET",
        dom_render_compiler::dev::serve_bench::Method::Post => "POST",
    }
}

fn parse_serve_args(args: &[String]) -> Result<Option<ServeOptions>, String> {
    if !args.iter().any(|a| a == "--serve") {
        return Ok(None);
    }
    let mut base_url: Option<String> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut warmup = 50u32;
    let mut samples = 500u32;
    let mut concurrency = 16usize;
    let mut timeout = Duration::from_secs(10);
    let mut keep_alive = false;
    let mut output_path: Option<PathBuf> = None;
    let mut markdown = false;

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--serve" => {
                idx += 1;
                base_url = Some(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "missing value for --serve <url>".to_string())?,
                );
            }
            "--path" => {
                idx += 1;
                paths.push(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "missing value for --path".to_string())?,
                );
            }
            "--warmup" => {
                idx += 1;
                warmup = parse_num(args.get(idx), "--warmup")?;
            }
            "--samples" => {
                idx += 1;
                samples = parse_num(args.get(idx), "--samples")?;
            }
            "--concurrency" => {
                idx += 1;
                concurrency = parse_num(args.get(idx), "--concurrency")?;
            }
            "--timeout-ms" => {
                idx += 1;
                let ms: u64 = parse_num(args.get(idx), "--timeout-ms")?;
                timeout = Duration::from_millis(ms.max(1));
            }
            "--output" => {
                idx += 1;
                output_path = Some(PathBuf::from(
                    args.get(idx)
                        .ok_or_else(|| "missing value for --output".to_string())?,
                ));
            }
            "--keep-alive" => {
                keep_alive = true;
            }
            "--markdown" => {
                markdown = true;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' in --serve mode")),
        }
        idx += 1;
    }

    Ok(Some(ServeOptions {
        base_url: base_url.ok_or_else(|| "--serve requires a <url>".to_string())?,
        paths,
        warmup,
        samples,
        concurrency,
        timeout,
        keep_alive,
        output_path,
        markdown,
    }))
}

fn parse_num<T: std::str::FromStr>(value: Option<&String>, flag: &str) -> Result<T, String> {
    value
        .ok_or_else(|| format!("missing value for {flag}"))?
        .parse::<T>()
        .map_err(|_| format!("invalid value for {flag}"))
}

fn print_serve_summary(report: &ServeBenchReport) {
    println!("ALBEDO Serve-Time Latency Report");
    println!("  base_url: {}", report.base_url);
    println!(
        "  warmup: {}  samples: {}  concurrency: {}",
        report.warmup, report.samples, report.concurrency
    );
    println!();
    for ep in &report.endpoints {
        println!("Endpoint: {} ({} {})", ep.name, method_str(ep), ep.path);
        println!(
            "  status: {} ({:.0}% 2xx)",
            ep.status,
            ep.ok_ratio * 100.0
        );
        println!(
            "  cold:       ttfb={:.2}ms total={:.2}ms ({} B)",
            ep.cold.ttfb_ms, ep.cold.total_ms, ep.cold.bytes
        );
        println!(
            "  warm ttfb:  p50={:.2}ms p90={:.2}ms p99={:.2}ms (min {:.2} / max {:.2})",
            ep.warm_ttfb.p50_ms,
            ep.warm_ttfb.p90_ms,
            ep.warm_ttfb.p99_ms,
            ep.warm_ttfb.min_ms,
            ep.warm_ttfb.max_ms
        );
        println!(
            "  warm total: p50={:.2}ms p90={:.2}ms p99={:.2}ms",
            ep.warm_total.p50_ms, ep.warm_total.p90_ms, ep.warm_total.p99_ms
        );
        println!();
    }
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
    println!("Build-time mode (scan/optimize workloads) — default:");
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
    println!();
    println!("Serve-time latency mode (HTTP TTFB / total against a running server):");
    println!("  --serve <URL>          Base url of a running `albedo serve` (e.g. http://127.0.0.1:3000)");
    println!("  --path <PATH>          Route to bench (repeatable; default: /)");
    println!("  --warmup <N>           Discarded warmup requests per endpoint (default: 50)");
    println!("  --samples <N>          Measured requests per endpoint (default: 500)");
    println!("  --concurrency <N>      Concurrent connections (default: 16)");
    println!("  --keep-alive           Reuse one connection per worker (strips per-req TCP connect)");
    println!("  --timeout-ms <N>       Per-request timeout in ms (default: 10000)");
    println!("  --output <FILE>        Write the serve-bench report JSON");
    println!("  --markdown             Print a README-ready Markdown table");
}
