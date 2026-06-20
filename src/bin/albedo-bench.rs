use dom_render_compiler::benchmark::{
    run_workloads, write_report_json, BaselineEnvelopeFile, BenchmarkWorkloads, GateStatus,
};
use dom_render_compiler::dev::serve_bench::{
    run as run_serve_bench, RequestSpec, ServeBenchConfig, ServeBenchReport,
};
use dom_render_compiler::dev::proc_bench::{
    build_bench::{run_build_bench, BuildBenchConfig, CommandBuildWorkload},
    run_cold_starts, ColdStartConfig, ProcessSpawner,
};
use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope, ActionEventKind};
use dom_render_compiler::transforms::form::allocate_form_action_id;
use std::path::PathBuf;
use std::time::Duration;

/// Content type used for the action POST body. The handler decodes the
/// raw bincode `ActionEnvelope`; the value here is informational (the
/// server keys off the route, not the header), but we send the same
/// `application/octet-stream` the response uses for symmetry.
const ACTION_CONTENT_TYPE: &str = "application/octet-stream";
/// Default route the `action()` dispatcher listens on (see
/// `crates/albedo-server/src/handlers/action.rs`).
const DEFAULT_ACTION_PATH: &str = "/_albedo/action";

/// Resolved description of one `action()` round-trip to bench. Built
/// from the `--action*` CLI flags and lowered into a POST `RequestSpec`
/// whose body is a valid bincode `ActionEnvelope` — the missing piece
/// that kept the harness GET-only.
#[derive(Debug, Clone)]
struct ActionOptions {
    /// Either an action name (FNV-1a-32 hashed to the id, matching the
    /// compiler + server) or an explicit numeric id override.
    name: Option<String>,
    id: Option<u32>,
    event_kind: u8,
    payload: Vec<u8>,
    path: String,
}

impl ActionOptions {
    /// FNV-1a-32 of the name, or the explicit id when given. `--action-id`
    /// wins over `--action` so an operator can target a handler whose
    /// source name they don't have.
    fn resolve_id(&self) -> Result<u32, String> {
        match (self.id, &self.name) {
            (Some(id), _) => Ok(id),
            (None, Some(name)) => Ok(allocate_form_action_id(name)),
            (None, None) => {
                Err("action bench requires --action <name> or --action-id <u32>".to_string())
            }
        }
    }

    /// Lower into a POST `RequestSpec` carrying the encoded envelope.
    fn to_request_spec(&self) -> Result<RequestSpec, String> {
        let action_id = self.resolve_id()?;
        let envelope = ActionEnvelope {
            action_id,
            event_kind: self.event_kind,
            payload: self.payload.clone(),
        };
        let body = encode_action_envelope(&envelope)
            .map_err(|err| format!("failed to encode action envelope: {err}"))?;
        let name = self
            .name
            .clone()
            .unwrap_or_else(|| format!("action#{action_id}"));
        Ok(RequestSpec::post(
            name,
            self.path.clone(),
            body,
            ACTION_CONTENT_TYPE.to_string(),
        ))
    }
}

/// Map a `--event-kind` argument to its wire byte. Accepts the symbolic
/// names the bakabox dispatcher emits or a raw `u8`.
fn parse_event_kind(value: &str) -> Result<u8, String> {
    match value.to_ascii_lowercase().as_str() {
        "click" => Ok(ActionEventKind::Click as u8),
        "input" => Ok(ActionEventKind::Input as u8),
        "submit" => Ok(ActionEventKind::Submit as u8),
        "other" => Ok(ActionEventKind::Other as u8),
        other => other
            .parse::<u8>()
            .map_err(|_| format!("invalid --event-kind '{value}' (click|input|submit|other|0-255)")),
    }
}

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
    /// When set, an `action()` POST round-trip is benched in addition to
    /// any `--path` GETs.
    action: Option<ActionOptions>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Process-lifecycle modes short-circuit first (they own a child
    // process), then serve-time latency, then the build-time workloads.
    if let Some(cold) = parse_cold_start_args(&args)? {
        return run_cold_start(cold);
    }
    if let Some(build) = parse_build_bench_args(&args)? {
        return run_build_bench_cli(build);
    }
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
    // Default to GET `/` only when the operator named no endpoints at
    // all. If they asked for an action but no `--path`, bench just the
    // action (don't silently inject a GET they didn't request).
    let paths = if options.paths.is_empty() && options.action.is_none() {
        vec!["/".to_string()]
    } else {
        options.paths.clone()
    };
    let mut requests = paths
        .iter()
        .map(|p| {
            let name = if p == "/" { "root" } else { p.trim_start_matches('/') };
            RequestSpec::get(name.to_string(), p.clone())
        })
        .collect::<Vec<_>>();

    // Append the action POST round-trip, if requested. This is the slice
    // that closes the GET-only gap: a real bincode `ActionEnvelope` on
    // the wire, measured under the same percentile machinery as the GETs.
    if let Some(action) = &options.action {
        let spec = action.to_request_spec()?;
        eprintln!(
            "serve-bench action → {} {} (action_id {}, event_kind {}, {} payload bytes)",
            spec.method.as_str(),
            spec.path,
            action.resolve_id()?,
            action.event_kind,
            action.payload.len(),
        );
        requests.push(spec);
    }

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

/// Cold-process-start CLI options (`--cold-start`).
#[derive(Debug, Clone)]
struct ColdStartCli {
    url: String,
    program: String,
    exec_args: Vec<String>,
    cwd: Option<PathBuf>,
    path: String,
    iterations: u32,
    ready_timeout: Duration,
    poll_interval: Duration,
    request_timeout: Duration,
    settle: Duration,
    inherit_io: bool,
    markdown: bool,
    output_path: Option<PathBuf>,
}

/// Build-time CLI options (`--build-bench`).
#[derive(Debug, Clone)]
struct BuildBenchCli {
    program: String,
    exec_args: Vec<String>,
    cwd: Option<PathBuf>,
    artifacts: Vec<PathBuf>,
    clean_samples: u32,
    incremental_samples: u32,
    inherit_io: bool,
    markdown: bool,
    output_path: Option<PathBuf>,
}

fn parse_cold_start_args(args: &[String]) -> Result<Option<ColdStartCli>, String> {
    if !args.iter().any(|a| a == "--cold-start") {
        return Ok(None);
    }
    let mut url: Option<String> = None;
    let mut program: Option<String> = None;
    let mut exec_args: Vec<String> = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut path = "/".to_string();
    let mut iterations = 10u32;
    let mut ready_timeout = Duration::from_secs(30);
    let mut poll_interval = Duration::from_millis(25);
    let mut request_timeout = Duration::from_secs(10);
    let mut settle = Duration::from_millis(300);
    let mut inherit_io = false;
    let mut markdown = false;
    let mut output_path: Option<PathBuf> = None;

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--cold-start" => {}
            "--url" => {
                idx += 1;
                url = Some(arg_value(args.get(idx), "--url")?);
            }
            "--exec" => {
                idx += 1;
                program = Some(arg_value(args.get(idx), "--exec")?);
            }
            "--exec-arg" => {
                idx += 1;
                exec_args.push(arg_value(args.get(idx), "--exec-arg")?);
            }
            "--cwd" => {
                idx += 1;
                cwd = Some(PathBuf::from(arg_value(args.get(idx), "--cwd")?));
            }
            "--path" => {
                idx += 1;
                path = arg_value(args.get(idx), "--path")?;
            }
            "--iterations" => {
                idx += 1;
                iterations = parse_num(args.get(idx), "--iterations")?;
            }
            "--ready-timeout-ms" => {
                idx += 1;
                ready_timeout = Duration::from_millis(parse_num(args.get(idx), "--ready-timeout-ms")?);
            }
            "--poll-interval-ms" => {
                idx += 1;
                poll_interval = Duration::from_millis(parse_num(args.get(idx), "--poll-interval-ms")?);
            }
            "--timeout-ms" => {
                idx += 1;
                request_timeout = Duration::from_millis(parse_num::<u64>(args.get(idx), "--timeout-ms")?.max(1));
            }
            "--settle-ms" => {
                idx += 1;
                settle = Duration::from_millis(parse_num(args.get(idx), "--settle-ms")?);
            }
            "--inherit-io" => inherit_io = true,
            "--markdown" => markdown = true,
            "--output" => {
                idx += 1;
                output_path = Some(PathBuf::from(arg_value(args.get(idx), "--output")?));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' in --cold-start mode")),
        }
        idx += 1;
    }

    Ok(Some(ColdStartCli {
        url: url.ok_or_else(|| "--cold-start requires --url <http://host:port>".to_string())?,
        program: program.ok_or_else(|| "--cold-start requires --exec <program>".to_string())?,
        exec_args,
        cwd,
        path,
        iterations,
        ready_timeout,
        poll_interval,
        request_timeout,
        settle,
        inherit_io,
        markdown,
        output_path,
    }))
}

fn run_cold_start(cli: ColdStartCli) -> Result<(), String> {
    let spawner = ProcessSpawner {
        program: cli.program.clone(),
        args: cli.exec_args.clone(),
        cwd: cli.cwd.clone(),
        base_url: cli.url.clone(),
        inherit_io: cli.inherit_io,
    };
    let config = ColdStartConfig {
        probe: RequestSpec::get("cold", cli.path.clone()),
        iterations: cli.iterations,
        ready_timeout: cli.ready_timeout,
        poll_interval: cli.poll_interval,
        request_timeout: cli.request_timeout,
        settle: cli.settle,
    };

    eprintln!(
        "cold-start → spawn `{} {}` · url {} · {} boots",
        cli.program,
        cli.exec_args.join(" "),
        cli.url,
        cli.iterations,
    );

    let report = run_cold_starts(&spawner, &config)
        .map_err(|err| format!("cold-start bench failed: {err}"))?;

    // A non-2xx first hit means the cold number reflects an error page,
    // not a real render — refuse to cite it.
    if let Some(bad) = report.samples.iter().find(|s| !(200..400).contains(&s.status)) {
        return Err(format!(
            "cold hit returned status {} — boot is erroring, numbers not citable",
            bad.status
        ));
    }

    println!("ALBEDO Cold-Process-Start Report ({} boots)", report.iterations);
    println!(
        "  boot → ready:    p50={:.2}ms p90={:.2}ms p99={:.2}ms (min {:.2} / max {:.2})",
        report.boot_ready.p50_ms,
        report.boot_ready.p90_ms,
        report.boot_ready.p99_ms,
        report.boot_ready.min_ms,
        report.boot_ready.max_ms
    );
    println!(
        "  first-hit TTFB:  p50={:.2}ms p90={:.2}ms p99={:.2}ms",
        report.first_ttfb.p50_ms, report.first_ttfb.p90_ms, report.first_ttfb.p99_ms
    );
    println!(
        "  first-hit total: p50={:.2}ms p90={:.2}ms p99={:.2}ms",
        report.first_total.p50_ms, report.first_total.p90_ms, report.first_total.p99_ms
    );

    if cli.markdown {
        println!("\n{}", report.to_markdown());
    }
    if let Some(output) = &cli.output_path {
        write_json(&report, output)?;
    }
    Ok(())
}

fn parse_build_bench_args(args: &[String]) -> Result<Option<BuildBenchCli>, String> {
    if !args.iter().any(|a| a == "--build-bench") {
        return Ok(None);
    }
    let mut program: Option<String> = None;
    let mut exec_args: Vec<String> = Vec::new();
    let mut cwd: Option<PathBuf> = None;
    let mut artifacts: Vec<PathBuf> = Vec::new();
    let mut clean_samples = 3u32;
    let mut incremental_samples = 5u32;
    let mut inherit_io = false;
    let mut markdown = false;
    let mut output_path: Option<PathBuf> = None;

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--build-bench" => {}
            "--exec" => {
                idx += 1;
                program = Some(arg_value(args.get(idx), "--exec")?);
            }
            "--exec-arg" => {
                idx += 1;
                exec_args.push(arg_value(args.get(idx), "--exec-arg")?);
            }
            "--cwd" => {
                idx += 1;
                cwd = Some(PathBuf::from(arg_value(args.get(idx), "--cwd")?));
            }
            "--artifact" => {
                idx += 1;
                artifacts.push(PathBuf::from(arg_value(args.get(idx), "--artifact")?));
            }
            "--clean-samples" => {
                idx += 1;
                clean_samples = parse_num(args.get(idx), "--clean-samples")?;
            }
            "--incremental-samples" => {
                idx += 1;
                incremental_samples = parse_num(args.get(idx), "--incremental-samples")?;
            }
            "--inherit-io" => inherit_io = true,
            "--markdown" => markdown = true,
            "--output" => {
                idx += 1;
                output_path = Some(PathBuf::from(arg_value(args.get(idx), "--output")?));
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' in --build-bench mode")),
        }
        idx += 1;
    }

    Ok(Some(BuildBenchCli {
        program: program.ok_or_else(|| "--build-bench requires --exec <program>".to_string())?,
        exec_args,
        cwd,
        artifacts,
        clean_samples,
        incremental_samples,
        inherit_io,
        markdown,
        output_path,
    }))
}

fn run_build_bench_cli(cli: BuildBenchCli) -> Result<(), String> {
    if cli.artifacts.is_empty() {
        eprintln!(
            "warning: no --artifact paths given; a clean build won't actually be cold (cache survives)"
        );
    }
    let workload = CommandBuildWorkload {
        program: cli.program.clone(),
        args: cli.exec_args.clone(),
        cwd: cli.cwd.clone(),
        artifacts: cli.artifacts.clone(),
        inherit_io: cli.inherit_io,
    };
    let config = BuildBenchConfig {
        clean_samples: cli.clean_samples,
        incremental_samples: cli.incremental_samples,
    };

    eprintln!(
        "build-bench → `{} {}` · {} clean / {} incremental",
        cli.program,
        cli.exec_args.join(" "),
        cli.clean_samples,
        cli.incremental_samples,
    );

    let report = run_build_bench(&workload, &config)
        .map_err(|err| format!("build-bench failed: {err}"))?;

    println!("ALBEDO Build-Time Report");
    println!(
        "  clean (cold):    p50={:.1}ms p90={:.1}ms (min {:.1} / max {:.1}, n={})",
        report.clean.p50_ms,
        report.clean.p90_ms,
        report.clean.min_ms,
        report.clean.max_ms,
        report.clean.count
    );
    println!(
        "  incremental:     p50={:.1}ms p90={:.1}ms (min {:.1} / max {:.1}, n={})",
        report.incremental.p50_ms,
        report.incremental.p90_ms,
        report.incremental.min_ms,
        report.incremental.max_ms,
        report.incremental.count
    );
    println!("  incremental speedup: {:.1}x (p50)", report.speedup);

    if cli.markdown {
        println!("\n{}", report.to_markdown());
    }
    if let Some(output) = &cli.output_path {
        write_json(&report, output)?;
    }
    Ok(())
}

/// Shared `--flag VALUE` reader for the new modes.
fn arg_value(value: Option<&String>, flag: &str) -> Result<String, String> {
    value
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}

/// Serialize any report to pretty JSON at `path`, creating parents.
fn write_json<T: serde::Serialize>(report: &T, path: &PathBuf) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|err| format!("failed to serialize report: {err}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, json)
        .map_err(|err| format!("failed to write report '{}': {err}", path.display()))?;
    println!("Wrote report: {}", path.display());
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
    // Action-bench accumulators — any one of these opts in.
    let mut action_name: Option<String> = None;
    let mut action_id: Option<u32> = None;
    let mut event_kind: u8 = ActionEventKind::Click as u8;
    let mut action_payload: Option<Vec<u8>> = None;
    let mut action_path = DEFAULT_ACTION_PATH.to_string();
    let mut action_requested = false;

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
            "--action" => {
                idx += 1;
                action_name = Some(
                    args.get(idx)
                        .cloned()
                        .ok_or_else(|| "missing value for --action <name>".to_string())?,
                );
                action_requested = true;
            }
            "--action-id" => {
                idx += 1;
                action_id = Some(parse_num(args.get(idx), "--action-id")?);
                action_requested = true;
            }
            "--event-kind" => {
                idx += 1;
                let raw = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --event-kind".to_string())?;
                event_kind = parse_event_kind(raw)?;
            }
            "--action-payload" => {
                idx += 1;
                let raw = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --action-payload".to_string())?;
                action_payload = Some(raw.clone().into_bytes());
            }
            "--action-payload-file" => {
                idx += 1;
                let path = args
                    .get(idx)
                    .ok_or_else(|| "missing value for --action-payload-file".to_string())?;
                let bytes = std::fs::read(path)
                    .map_err(|err| format!("failed to read --action-payload-file '{path}': {err}"))?;
                action_payload = Some(bytes);
            }
            "--action-path" => {
                idx += 1;
                action_path = args
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| "missing value for --action-path".to_string())?;
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument '{other}' in --serve mode")),
        }
        idx += 1;
    }

    let action = if action_requested {
        Some(ActionOptions {
            name: action_name,
            id: action_id,
            event_kind,
            payload: action_payload.unwrap_or_default(),
            path: action_path,
        })
    } else {
        None
    };

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
        action,
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
    println!();
    println!("  Action round-trip (POST a bincode ActionEnvelope to /_albedo/action):");
    println!("  --action <NAME>            Action name; FNV-1a-32 hashed to the wire action_id");
    println!("  --action-id <U32>          Explicit action_id (overrides --action's hash)");
    println!("  --event-kind <KIND>        click|input|submit|other|0-255 (default: click)");
    println!("  --action-payload <STR>     UTF-8 payload bytes (e.g. an Input value)");
    println!("  --action-payload-file <F>  Read the payload bytes from a file");
    println!("  --action-path <PATH>       Action route (default: /_albedo/action)");
    println!();
    println!("Cold-process-start mode (spawn the server, time the first hit after boot):");
    println!("  --cold-start               Enable cold-start mode");
    println!("  --url <URL>                Base url the spawned server binds (e.g. http://127.0.0.1:3000)");
    println!("  --exec <PROGRAM>           Server binary to spawn (e.g. ../target/release/albedo)");
    println!("  --exec-arg <ARG>           One server arg (repeatable: serve --port 3000)");
    println!("  --cwd <DIR>                Working dir for the spawned server (the app dir)");
    println!("  --path <PATH>              Route to hit on first boot (default: /)");
    println!("  --iterations <N>           Number of cold boots to sample (default: 10)");
    println!("  --ready-timeout-ms <N>     Max wait for the listener per boot (default: 30000)");
    println!("  --settle-ms <N>            Pause after kill before next boot (default: 300)");
    println!("  --inherit-io               Show the spawned server's stdout/stderr");
    println!();
    println!("Build-time mode (clean vs incremental `albedo build` wall clock):");
    println!("  --build-bench              Enable build-time mode");
    println!("  --exec <PROGRAM>           Build binary (e.g. ../target/release/albedo)");
    println!("  --exec-arg <ARG>           One build arg (repeatable: build my-app)");
    println!("  --cwd <DIR>                Working dir for the build");
    println!("  --artifact <PATH>          Path wiped before each clean build (repeatable:");
    println!("                             the app's .albedo dir + .dom-compiler-cache.bin)");
    println!("  --clean-samples <N>        Cold builds to time (default: 3)");
    println!("  --incremental-samples <N>  Warm builds to time (default: 5)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::dev::serve_bench::Method;
    use dom_render_compiler::ir::action::decode_action_envelope;

    #[test]
    fn action_id_resolves_from_name_via_fnv1a_then_id_override() {
        let by_name = ActionOptions {
            name: Some("submitContact".to_string()),
            id: None,
            event_kind: 0,
            payload: Vec::new(),
            path: DEFAULT_ACTION_PATH.to_string(),
        };
        assert_eq!(
            by_name.resolve_id().unwrap(),
            allocate_form_action_id("submitContact"),
            "name path must match the compiler/server FNV-1a-32 family",
        );

        let overridden = ActionOptions {
            id: Some(999),
            ..by_name.clone()
        };
        assert_eq!(
            overridden.resolve_id().unwrap(),
            999,
            "--action-id wins over the hashed name",
        );

        let neither = ActionOptions {
            name: None,
            id: None,
            ..by_name
        };
        assert!(neither.resolve_id().is_err(), "no name and no id is an error");
    }

    #[test]
    fn action_spec_encodes_a_decodable_envelope() {
        let opts = ActionOptions {
            name: Some("setCount".to_string()),
            id: None,
            event_kind: ActionEventKind::Input as u8,
            payload: b"42".to_vec(),
            path: "/_albedo/action".to_string(),
        };
        let spec = opts.to_request_spec().expect("spec builds");
        assert_eq!(spec.method, Method::Post);
        assert_eq!(spec.path, "/_albedo/action");
        assert_eq!(spec.content_type.as_deref(), Some(ACTION_CONTENT_TYPE));

        let body = spec.body.expect("post body present");
        let (envelope, consumed) = decode_action_envelope(&body).expect("body is a valid envelope");
        assert_eq!(consumed, body.len(), "no trailing bytes");
        assert_eq!(envelope.action_id, allocate_form_action_id("setCount"));
        assert_eq!(envelope.event_kind, ActionEventKind::Input as u8);
        assert_eq!(envelope.payload, b"42");
    }

    #[test]
    fn event_kind_parses_symbolic_and_numeric() {
        assert_eq!(parse_event_kind("click").unwrap(), 0);
        assert_eq!(parse_event_kind("Input").unwrap(), 1);
        assert_eq!(parse_event_kind("SUBMIT").unwrap(), 2);
        assert_eq!(parse_event_kind("other").unwrap(), 3);
        assert_eq!(parse_event_kind("7").unwrap(), 7);
        assert!(parse_event_kind("nope").is_err());
    }

    #[test]
    fn cold_start_args_parse() {
        let args = vec![
            "--cold-start".to_string(),
            "--url".to_string(),
            "http://127.0.0.1:3000".to_string(),
            "--exec".to_string(),
            "../target/release/albedo".to_string(),
            "--exec-arg".to_string(),
            "serve".to_string(),
            "--exec-arg".to_string(),
            "--port".to_string(),
            "--exec-arg".to_string(),
            "3000".to_string(),
            "--cwd".to_string(),
            "my-app".to_string(),
            "--iterations".to_string(),
            "5".to_string(),
        ];
        let cli = parse_cold_start_args(&args).unwrap().expect("cold-start mode");
        assert_eq!(cli.url, "http://127.0.0.1:3000");
        assert_eq!(cli.program, "../target/release/albedo");
        assert_eq!(cli.exec_args, vec!["serve", "--port", "3000"]);
        assert_eq!(cli.cwd, Some(PathBuf::from("my-app")));
        assert_eq!(cli.iterations, 5);
    }

    #[test]
    fn cold_start_requires_url_and_exec() {
        // --cold-start with neither --url nor --exec must error.
        let args = vec!["--cold-start".to_string()];
        assert!(parse_cold_start_args(&args).is_err());
    }

    #[test]
    fn build_bench_args_parse() {
        let args = vec![
            "--build-bench".to_string(),
            "--exec".to_string(),
            "../target/release/albedo".to_string(),
            "--exec-arg".to_string(),
            "build".to_string(),
            "--exec-arg".to_string(),
            "my-app".to_string(),
            "--artifact".to_string(),
            "my-app/.albedo".to_string(),
            "--artifact".to_string(),
            "my-app/.dom-compiler-cache.bin".to_string(),
            "--clean-samples".to_string(),
            "2".to_string(),
            "--incremental-samples".to_string(),
            "4".to_string(),
        ];
        let cli = parse_build_bench_args(&args).unwrap().expect("build-bench mode");
        assert_eq!(cli.exec_args, vec!["build", "my-app"]);
        assert_eq!(cli.artifacts.len(), 2);
        assert_eq!(cli.clean_samples, 2);
        assert_eq!(cli.incremental_samples, 4);
    }

    #[test]
    fn modes_are_mutually_exclusive_by_first_match() {
        // A plain serve invocation must not be mistaken for a lifecycle
        // mode, and vice versa — the dispatch checks each trigger flag.
        let serve = vec!["--serve".to_string(), "http://x:1".to_string()];
        assert!(parse_cold_start_args(&serve).unwrap().is_none());
        assert!(parse_build_bench_args(&serve).unwrap().is_none());
        assert!(parse_serve_args(&serve).unwrap().is_some());
    }

    #[test]
    fn serve_args_parse_action_flags() {
        let args = vec![
            "--serve".to_string(),
            "http://127.0.0.1:3000".to_string(),
            "--action".to_string(),
            "submitContact".to_string(),
            "--event-kind".to_string(),
            "submit".to_string(),
            "--action-payload".to_string(),
            "hello".to_string(),
        ];
        let opts = parse_serve_args(&args).unwrap().expect("serve mode");
        let action = opts.action.expect("action requested");
        assert_eq!(action.name.as_deref(), Some("submitContact"));
        assert_eq!(action.event_kind, ActionEventKind::Submit as u8);
        assert_eq!(action.payload, b"hello");
        assert_eq!(action.path, DEFAULT_ACTION_PATH);
        // No --path given, but an action was: paths must NOT default to "/".
        assert!(opts.paths.is_empty());
    }
}
