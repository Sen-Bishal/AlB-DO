use dom_render_compiler::budget::{
    compute_bundle_byte_report, evaluate_budget, evaluate_bundle_budget, format_report_pretty,
    load_budget_from_dir, BudgetReport, TierBudget,
};
use dom_render_compiler::bundler::emit::BundleEmitReport;
use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::dev_contract::{
    parse_dev_cli_args, resolve_dev_contract, ResolvedDevContract, DEV_CONFIG_TS,
};
use dom_render_compiler::manifest::schema::RenderManifestV2;
use dom_render_compiler::parser::ParsedComponent;
use dom_render_compiler::scanner::{ProjectScanner, ScanFailure, ScanMode};
use notify::{
    Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[path = "albedo/printer.rs"]
mod printer;

#[path = "albedo/first_run.rs"]
mod first_run;

const PORT_AUTO_INCREMENT_LIMIT: u16 = 10;

// Palette — "Halation". ALBEDO is the fraction of light a surface reflects, and
// the flagship (Halation, "the glow around bright things") lives in champagne
// gold on ink. The CLI matches: warm gold accents, not the old cold cyan. Every
// `print_*` helper flows through these, so the whole tool recolors from here.
const ACCENT: u8 = 179; // champagne gold — primary accent (glyphs, headings)
const ACCENT_SOFT: u8 = 223; // pale gold / cream — values, links, live state
const ACCENT_DEEP: u8 = 137; // deep gold — dividers, secondary marks
const MUTED: u8 = 245; // warm-neutral gray — labels, secondary copy

// Shared column width for help listings (commands + flags), so the description
// column aligns across both. Longest label is "completions <shell>" (19).
const COL_WIDTH: usize = 20;

// Wordmark shimmer: a low→high luminance ascent, deep gold rising to cream — the
// "glow" made literal, per-character (mirrors `gradient_text`). The "instrument
// for light" tier bars (A+B blend) live in `printer.rs` where the tier data is.
const BRAND_PALETTE: [u8; 6] = [137, 179, 221, 222, 223, 230];

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAMES_ASCII: [&str; 4] = ["|", "/", "-", "\\"];

// Phase P · Stream F.2 — scaffold refresh.
//
// The scaffold lives in `scaffold/` and is mirrored verbatim into a
// fresh project by `albedo init`. After F.2 it follows Phase N+
// conventions: `src/routes/` for file-based routing, a root
// `layout.tsx` wrapping every route, a `tier-budget.toml` at project
// root, and TS-side `action()` + `useSharedSlot()` demonstrated in
// the chat route. Old shape (`src/App.tsx` + Tier-C fetch demo)
// retired — no upgrade path from pre-Phase-P scaffolds; users on
// the old shape `albedo init --force` into a fresh dir.
const SCAFFOLD_LAYOUT: &str = include_str!("../../scaffold/src/routes/layout.tsx");
const SCAFFOLD_INDEX_ROUTE: &str = include_str!("../../scaffold/src/routes/index.tsx");
const SCAFFOLD_CHAT_ROUTE: &str = include_str!("../../scaffold/src/routes/chat.tsx");
const SCAFFOLD_HERO: &str = include_str!("../../scaffold/src/components/Hero.tsx");
const SCAFFOLD_COUNTER: &str = include_str!("../../scaffold/src/components/Counter.tsx");
const SCAFFOLD_ENV_DTS: &str = include_str!("../../scaffold/src/albedo-env.d.ts");
const SCAFFOLD_STYLES: &str = include_str!("../../scaffold/src/styles.css");
const SCAFFOLD_CONFIG: &str = include_str!("../../scaffold/albedo.config.ts");
const SCAFFOLD_PACKAGE_JSON: &str = include_str!("../../scaffold/package.json");
// Phase P · post-P wire-through — `public/index.html` removed from
// the scaffold. The production server's streaming arm renders `/`
// from the manifest's route entry; a static `index.html` at
// `public/index.html` was getting served by the public-assets
// dispatch BEFORE the manifest-streaming arm, shadowing the live
// route. Static-export targets (Cloudflare Pages etc.) should
// extract `routes["/"].shell` from the manifest at deploy time.
const SCAFFOLD_TSCONFIG: &str = include_str!("../../scaffold/tsconfig.json");
const SCAFFOLD_README: &str = include_str!("../../scaffold/README.md");
const SCAFFOLD_GITIGNORE: &str = include_str!("../../scaffold/.gitignore");
const SCAFFOLD_TIER_BUDGET: &str = include_str!("../../scaffold/tier-budget.toml");

fn main() {
    if let Err(err) = run(std::env::args().collect()) {
        print_error(err);
        std::process::exit(1);
    }
}

fn run(args: Vec<String>) -> Result<(), String> {
    // First-run detection — on a fresh install this re-launches as `albdo init`
    // and exits. On subsequent runs this is a no-op (single Path::exists check).
    first_run::check_and_run_init();

    if args.len() <= 1 {
        print_help();
        return Ok(());
    }

    match args[1].as_str() {
        "init" => run_init_command(&args[2..]),
        "dev" => run_dev_mode(&args[2..]),
        "build" => {
            let mut forwarded = args[2..].to_vec();
            forwarded.push("--prod".to_string());
            run_dev_mode(&forwarded)
        }
        "ship" => run_ship_command(&args[2..]),
        // Phase O.1 · standalone tier-budget evaluator. Loads
        // tier-budget.toml when present (built-in defaults
        // otherwise), runs the eval against the freshly-compiled
        // manifest, and exits non-zero on violation.
        "budget" => run_budget_command(&args[2..]),
        // Phase J CLI clarity:
        //   * `albedo files [dir]` — pure static file server; serves any
        //     directory verbatim. This is what `albedo serve` did before.
        //   * `albedo serve` — production server: builds the project via
        //     the same stitcher as `dev` / `dev --prod` / `build`, then
        //     serves the resulting `.albedo/dist`. One stitcher feeds
        //     every command — dev/prod parity by construction.
        //   * `albedo serve <dir>` — back-compat alias for `files <dir>`.
        "files" => run_files_command(&args[2..]),
        "serve" => run_serve_command(&args[2..]),
        "run" => run_command(&args[2..]),
        "completions" => run_completions_command(&args[2..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        unknown => Err(format!(
            "unknown command '{unknown}'. Run `albdo help` to see available commands."
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitOptions {
    target_dir: PathBuf,
    force: bool,
}

fn run_init_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_init_help();
        return Ok(());
    }

    let options = parse_init_args(raw_args)?;
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let target = if options.target_dir.is_absolute() {
        options.target_dir.clone()
    } else {
        cwd.join(&options.target_dir)
    };

    with_spinner("scaffolding project…", || scaffold_project(&target, &options))?;

    let relative_target = options.target_dir.display().to_string();
    print_init_success(relative_target.as_str());
    Ok(())
}

fn parse_init_args(raw_args: &[String]) -> Result<InitOptions, String> {
    let mut target_dir: Option<PathBuf> = None;
    let mut force = false;
    let mut target_set = false;
    let mut idx = 0usize;

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--force" => {
                force = true;
            }
            _ if !arg.starts_with('-') => {
                if target_set {
                    return Err("init accepts at most one target directory".to_string());
                }
                target_dir = Some(PathBuf::from(arg));
                target_set = true;
            }
            unknown => {
                return Err(format!("unknown init option '{unknown}'"));
            }
        }
        idx += 1;
    }

    let target_dir = target_dir.ok_or_else(|| {
        "missing project name. Usage: albedo init <project-name> [--force]".to_string()
    })?;

    Ok(InitOptions { target_dir, force })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipTarget {
    Vercel,
    Docker,
    Fly,
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ShipOptions {
    target: Option<ShipTarget>,
    forwarded: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServeOptions {
    dir: PathBuf,
    host: String,
    port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BudgetFormat {
    Pretty,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BudgetOptions {
    strict: bool,
    format: BudgetFormat,
    forwarded: Vec<String>,
}

fn parse_budget_args(raw_args: &[String]) -> Result<BudgetOptions, String> {
    let mut strict = false;
    let mut format = BudgetFormat::Pretty;
    let mut forwarded = Vec::new();
    let mut idx = 0usize;
    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "--strict" => strict = true,
            "--format" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --format".to_string())?;
                format = match value.as_str() {
                    "pretty" => BudgetFormat::Pretty,
                    "json" => BudgetFormat::Json,
                    other => {
                        return Err(format!(
                            "unknown --format '{other}'. Supported: pretty, json."
                        ))
                    }
                };
            }
            other => forwarded.push(other.to_string()),
        }
        idx += 1;
    }
    Ok(BudgetOptions {
        strict,
        format,
        forwarded,
    })
}

/// Phase O.1 · `albedo budget` — compiles the manifest in memory,
/// loads `tier-budget.toml` (or built-in defaults), and prints the
/// usage table. Exits non-zero when any ceiling is violated;
/// `--strict` additionally requires the budget file to be present
/// so CI fails loud rather than silently accepting defaults.
fn run_budget_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_budget_help();
        return Ok(());
    }
    let options = parse_budget_args(raw_args)?;
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let contract = resolve_dev_contract(&options.forwarded, &cwd)?;

    let manifest = build_manifest_for_budget(&contract)?;
    let (budget, source) = resolve_budget_for_contract(&contract, options.strict)?;
    let report = evaluate_budget(&manifest, &budget);

    match options.format {
        BudgetFormat::Pretty => {
            print_section("budget");
            print_kv("source", source);
            println!("{}", format_report_pretty(&report));
        }
        BudgetFormat::Json => {
            let json = serde_json::to_string_pretty(&report)
                .map_err(|err| format!("failed to serialize budget report: {err}"))?;
            println!("{json}");
        }
    }

    if !report.is_ok() {
        return Err(format!(
            "tier budget exceeded ({} violation{})",
            report.violations.len(),
            if report.violations.len() == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

/// Compile the project just far enough to produce a manifest without
/// writing artefacts to disk. Used by `albedo budget` (which has no
/// reason to emit a bundle) and by the build/ship gate (which emits
/// artefacts first and then re-evaluates against the same manifest).
fn build_manifest_for_budget(
    contract: &ResolvedDevContract,
) -> Result<RenderManifestV2, String> {
    let components = scan_components_with_contract_policy(contract, "evaluating tier budget")?;
    if components.is_empty() {
        return Err(format!(
            "no component files found under '{}' (.js/.jsx/.ts/.tsx expected)",
            contract.root.display()
        ));
    }
    let scanner = ProjectScanner::new();
    let compiler = scanner.build_compiler(components);
    compiler
        .optimize_manifest_v2()
        .map_err(|err| format!("failed to optimize manifest: {err}"))
}

/// Resolve the budget for a contract, returning the source label
/// ("tier-budget.toml" or "built-in defaults") so the CLI can
/// surface where the ceilings came from. `strict` rejects the
/// built-in fallback so CI never accidentally passes against
/// defaults.
fn resolve_budget_for_contract(
    contract: &ResolvedDevContract,
    strict: bool,
) -> Result<(TierBudget, String), String> {
    let loaded = load_budget_from_dir(&contract.project_dir)
        .map_err(|err| err.to_string())?;
    match loaded {
        Some(budget) => Ok((budget, "tier-budget.toml".to_string())),
        None => {
            if strict {
                Err(format!(
                    "tier-budget.toml not found in '{}' and --strict was set",
                    contract.project_dir.display()
                ))
            } else {
                Ok((TierBudget::default(), "built-in defaults".to_string()))
            }
        }
    }
}

/// Phase O.1 + O.3 · evaluate against the budget after a successful
/// build. File-gated: only runs when `tier-budget.toml` exists.
/// Build/ship callers pass `skip = true` to honour `--no-budget`.
///
/// Two gates run in sequence:
///   1. Source-weight (Phase O.1) — fast, uses only the manifest.
///   2. Bundle-byte (Phase O.3) — measures emitted wrapper bytes.
///      Only runs when `emit_report` is supplied; absent emit
///      report falls back to source-weight only.
///
/// Both gates' violations land in the same printed diff so the user
/// sees every reason the build is failing in one place.
fn enforce_budget_after_build(
    contract: &ResolvedDevContract,
    manifest: &RenderManifestV2,
    emit_report: Option<&BundleEmitReport>,
    skip: bool,
) -> Result<(), String> {
    if skip {
        return Ok(());
    }
    let loaded =
        load_budget_from_dir(&contract.project_dir).map_err(|err| err.to_string())?;
    let Some(budget) = loaded else {
        return Ok(());
    };

    let source_report = evaluate_budget(manifest, &budget);
    let bundle_report = emit_report
        .map(|er| bundle_budget_report(er, manifest, &budget))
        .transpose()?;

    let combined = merge_budget_reports(&source_report, bundle_report.as_ref());
    if combined.is_ok() {
        return Ok(());
    }
    print_section("budget");
    print_kv("source", "tier-budget.toml");
    println!("{}", format_report_pretty(&combined));
    Err(format!(
        "tier budget exceeded ({} violation{})",
        combined.violations.len(),
        if combined.violations.len() == 1 { "" } else { "s" }
    ))
}

/// Build the bundle-byte report by re-deriving the plan from the
/// manifest. The bundler is deterministic so a fresh plan matches
/// the one the emit step produced; we don't need to thread the plan
/// out of the build closure.
fn bundle_budget_report(
    emit_report: &BundleEmitReport,
    manifest: &RenderManifestV2,
    budget: &TierBudget,
) -> Result<BudgetReport, String> {
    let plan = dom_render_compiler::bundler::build_bundle_plan(
        manifest,
        &dom_render_compiler::bundler::BundlePlanOptions::default(),
    );
    let byte_report = compute_bundle_byte_report(emit_report, &plan, manifest);
    Ok(evaluate_bundle_budget(&byte_report, budget))
}

/// Concatenate two budget reports for display purposes. The
/// route-summary table comes from the source-weight pass (the
/// bundle pass doesn't have route-level data today); violations
/// from both are appended in order so a build that trips both gates
/// shows every reason at once.
fn merge_budget_reports(primary: &BudgetReport, secondary: Option<&BudgetReport>) -> BudgetReport {
    let mut violations = primary.violations.clone();
    if let Some(report) = secondary {
        violations.extend(report.violations.iter().cloned());
    }
    BudgetReport {
        violations,
        route_summaries: primary.route_summaries.clone(),
    }
}

fn run_ship_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_ship_help();
        return Ok(());
    }

    let options = parse_ship_args(raw_args)?;
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let contract = resolve_dev_contract(&options.forwarded, &cwd)?;
    let skip_budget = raw_args.iter().any(|arg| arg == "--no-budget");
    run_prod_build_with_budget(&contract, skip_budget, true, false)?;

    let target = if let Some(target) = options.target {
        target
    } else {
        prompt_ship_target()?
    };

    match target {
        ShipTarget::Vercel => configure_ship_vercel(&contract),
        ShipTarget::Docker => configure_ship_docker(&contract),
        ShipTarget::Fly => configure_ship_fly(&contract),
        ShipTarget::Static => {
            print_section("static");
            print_ok("static export ready");
            print_kv(
                "dist",
                contract.project_dir.join(".albedo").join("dist").display(),
            );
            Ok(())
        }
    }
}

fn parse_ship_args(raw_args: &[String]) -> Result<ShipOptions, String> {
    let mut target = None;
    let mut forwarded = Vec::new();
    let mut idx = 0usize;

    while idx < raw_args.len() {
        match raw_args[idx].as_str() {
            "--target" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --target".to_string())?;
                target = Some(parse_ship_target(value)?);
            }
            // Phase O.1 · `--no-budget` opts out of the tier
            // budget gate; consumed here so it doesn't reach the
            // dev-contract parser which would reject it as unknown.
            "--no-budget" => {}
            other => forwarded.push(other.to_string()),
        }
        idx += 1;
    }

    Ok(ShipOptions { target, forwarded })
}

fn parse_ship_target(raw: &str) -> Result<ShipTarget, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        // Phase N · vercel is no longer a supported runtime target.
        // The string still parses so the rejection message lands in
        // `run_ship_command` rather than at flag parsing, giving the
        // user the actual "why" instead of a generic "unknown target".
        "1" | "vercel" => Ok(ShipTarget::Vercel),
        "2" | "docker" => Ok(ShipTarget::Docker),
        "3" | "fly" | "flyio" | "fly.io" => Ok(ShipTarget::Fly),
        "4" | "static" => Ok(ShipTarget::Static),
        other => Err(format!(
            "unknown ship target '{other}'. Supported targets: docker, fly, static."
        )),
    }
}

fn prompt_ship_target() -> Result<ShipTarget, String> {
    print_section("pick a target");
    println!(
        "    {} docker     {}",
        style_256("2", ACCENT_SOFT, true),
        style("multi-stage binary image (recommended)", "2")
    );
    println!(
        "    {} fly        {}",
        style_256("3", ACCENT_SOFT, true),
        style("fly.toml + Dockerfile", "2")
    );
    println!(
        "    {} static     {}",
        style_256("4", ACCENT_SOFT, true),
        style("export dist/ for any CDN", "2")
    );
    println!();
    print!("  {} ", style_256("›", ACCENT, true));
    std::io::stdout()
        .flush()
        .map_err(|err| format!("failed to flush prompt: {err}"))?;

    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|err| format!("failed to read target selection: {err}"))?;
    parse_ship_target(input.trim())
}

/// Phase N · Vercel is not a supported target — its serverless
/// runtime does not execute the Rust binary that ALBEDO ships. The
/// honest answer is "use `--target docker` (and optionally
/// `--target fly`)" rather than emit a vercel.json that silently
/// won't work in production.
fn configure_ship_vercel(_contract: &ResolvedDevContract) -> Result<(), String> {
    Err(
        "vercel is not a supported ship target — Vercel's runtime does not execute Rust binaries. \
         Use `albedo ship --target docker` (or `--target fly`) to deploy the binary + dist."
            .to_string(),
    )
}

/// Phase N · Multi-stage Dockerfile. Stage 1 compiles the userland
/// app via `albedo build`; stage 2 ships the binary + `.albedo/dist`
/// on a slim Debian runtime. Port is configurable via
/// `ALBEDO_SERVER_PORT` at run time; defaults to 3000.
fn configure_ship_docker(contract: &ResolvedDevContract) -> Result<(), String> {
    let dockerfile = build_docker_template();
    let dockerignore = build_dockerignore_template();
    let dockerfile_path = contract.project_dir.join("Dockerfile");
    let dockerignore_path = contract.project_dir.join(".dockerignore");
    std::fs::write(&dockerfile_path, dockerfile.as_str())
        .map_err(|err| format!("failed to write '{}': {err}", dockerfile_path.display()))?;
    std::fs::write(&dockerignore_path, dockerignore.as_str())
        .map_err(|err| format!("failed to write '{}': {err}", dockerignore_path.display()))?;
    print_section("docker");
    print_ok("Dockerfile + .dockerignore written");
    print_kv("dockerfile", dockerfile_path.display());
    print_kv("ignore", dockerignore_path.display());
    print_kv(
        "build",
        style_256("docker build -t albedo-app .", ACCENT_SOFT, true),
    );
    print_kv(
        "run",
        style_256("docker run -p 3000:3000 albedo-app", ACCENT_SOFT, true),
    );
    Ok(())
}

fn configure_ship_fly(contract: &ResolvedDevContract) -> Result<(), String> {
    configure_ship_docker(contract)?;
    let app_name = infer_package_name(&contract.project_dir);
    let fly_toml = build_fly_toml_template(&app_name);
    let fly_toml_path = contract.project_dir.join("fly.toml");
    std::fs::write(&fly_toml_path, fly_toml.as_str())
        .map_err(|err| format!("failed to write '{}': {err}", fly_toml_path.display()))?;
    print_section("fly.io");
    print_ok("fly.toml written");
    print_kv("file", fly_toml_path.display());
    print_kv(
        "deploy",
        style_256("fly launch --copy-config && fly deploy", ACCENT_SOFT, true),
    );
    Ok(())
}

/// Multi-stage Dockerfile template emitted by `albedo ship --target
/// docker` (and reused by `--target fly`). Kept as a function so the
/// ship-target tests can assert key lines without depending on file
/// I/O.
fn build_docker_template() -> String {
    r#"# Phase N · multi-stage build emitted by `albedo ship --target docker`.
# Stage 1 compiles userland against the prebuilt `albedo` binary; stage
# 2 ships the resulting `.albedo/dist` artefacts under a slim runtime.
# Configurable at run time via ALBEDO_SERVER_PORT / ALBEDO_SERVER_HOST.

FROM rust:1-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN if [ ! -f ./target/release/albedo ]; then \
      cargo build --release --bin albedo; \
    fi
RUN ./target/release/albedo build .

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /workspace/target/release/albedo /usr/local/bin/albedo
COPY --from=builder /workspace/.albedo/dist /app/dist
COPY --from=builder /workspace/public /app/public

ENV ALBEDO_SERVER_HOST=0.0.0.0
ENV ALBEDO_SERVER_PORT=3000
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${ALBEDO_SERVER_PORT}/" >/dev/null 2>&1 || exit 1

CMD ["sh", "-c", "albedo serve --dir dist --host ${ALBEDO_SERVER_HOST} --port ${ALBEDO_SERVER_PORT}"]
"#
    .to_string()
}

fn build_dockerignore_template() -> String {
    r#".git
.gitignore
node_modules
target/debug
target/doc
target/package
target/tmp
**/*.log
**/.DS_Store
**/Thumbs.db
"#
    .to_string()
}

fn build_fly_toml_template(app_name: &str) -> String {
    format!(
        r#"# Phase N · fly.toml emitted by `albedo ship --target fly`.
# Pairs with the Dockerfile above; Fly builds the image remotely and
# runs it under a tiny VM. Adjust `primary_region` to whichever Fly
# region your users live closest to.

app = "{app_name}"
primary_region = "iad"

[build]
  dockerfile = "Dockerfile"

[env]
  ALBEDO_SERVER_HOST = "0.0.0.0"
  ALBEDO_SERVER_PORT = "3000"

[http_service]
  internal_port = 3000
  force_https = true
  auto_stop_machines = "stop"
  auto_start_machines = true
  min_machines_running = 0

[[http_service.checks]]
  grace_period = "10s"
  interval = "30s"
  method = "GET"
  path = "/"
  timeout = "5s"
"#
    )
}

/// Phase J CLI: `albedo serve` runs the production build (same stitcher
/// as `dev` / `dev --prod` / `build`) and then file-serves the resulting
/// `.albedo/dist` directory. The build step is what guarantees one
/// stitcher feeds dev and prod alike. If the user passes an explicit
/// directory argument, we treat it as the back-compat shape for the old
/// `albedo serve <dir>` (now spelt `albedo files <dir>`) and skip the
/// build to preserve the long-standing CLI ergonomic.
fn run_serve_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_serve_help();
        return Ok(());
    }

    // Phase P · post-P — only fall through to the static-file
    // back-compat path when the user explicitly asks for a directory.
    // Walk the args treating `--host`/`--port`/`--dir` values as
    // bound to their flag so a port number like `3139` doesn't get
    // mistaken for a positional dir arg.
    let user_passed_dir = {
        let mut explicit_dir = false;
        let mut idx = 0;
        while idx < raw_args.len() {
            let arg = &raw_args[idx];
            match arg.as_str() {
                "--dir" => {
                    explicit_dir = true;
                    break;
                }
                "--host" | "--port" => {
                    idx += 2; // skip the flag's value
                    continue;
                }
                _ if !arg.starts_with('-') => {
                    explicit_dir = true;
                    break;
                }
                _ => {}
            }
            idx += 1;
        }
        explicit_dir
    };
    if user_passed_dir {
        // Back-compat: `albedo serve <dir>` and `--dir <dir>` keep their
        // pre-Phase-J semantics — serve the directory directly. Equivalent
        // to `albedo files <dir>`, kept so existing scripts don't break.
        return run_files_command(raw_args);
    }

    // Phase P · Stream A — build then boot a real `AlbedoServer`. The
    // build emits the manifest with Stream B's pre-rendered Tier-B HTML
    // and bincode-encoded opcode frames; `boot_production_server`
    // loads them and registers every `CompiledProject` handler so
    // bakabox click → `/_albedo/action` → slot update closes end-to-end.
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let mut contract = resolve_dev_contract(raw_args, &cwd)?;
    print_boot_banner();
    print_section("serve");
    print_kv("project", contract.project_dir.display());
    print_kv("mode", "production (build + serve)");
    println!();
    run_prod_build(&contract)?;

    // `resolve_dev_contract` already absorbed `--host` / `--port` from
    // `raw_args`. Pull the bind address back out for the banner.
    let serve_options = parse_serve_args(raw_args)?;
    contract.server.host = serve_options.host.clone();
    contract.server.port = serve_options.port;

    boot_and_run_production_server(&contract)
}

/// Phase P · Stream A — turn a built `ResolvedDevContract` into a
/// running [`albedo_server::AlbedoServer`]. The Tokio runtime is
/// spun up here (not at `main`) so the dev path stays sync and only
/// pays the runtime cost on `albedo serve`.
fn boot_and_run_production_server(contract: &ResolvedDevContract) -> Result<(), String> {
    use albedo_server::{boot_production_server, ProductionServerOptions};

    let opts = ProductionServerOptions::from_contract(contract);
    let server = boot_production_server(&opts).map_err(|err| {
        format!(
            "failed to boot production server: {err}\n\
             hint: did you run `albedo build` first?"
        )
    })?;

    print_ok(format!(
        "serving · {}",
        style_256(
            &format!("http://{}:{}", contract.server.host, contract.server.port),
            ACCENT_SOFT,
            true,
        )
    ));
    println!(
        "    {} {}",
        style_256("·", MUTED, false),
        style(&format!("{}", contract.project_dir.display()), "2")
    );
    println!();
    println!(
        "    {}  stop the server",
        style_256("ctrl+c", MUTED, true)
    );
    println!();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start tokio runtime: {err}"))?;

    runtime
        .block_on(server.run())
        .map_err(|err| format!("server runtime error: {err}"))
}

/// Static file server. Phase-J rename of the previous `albedo serve`
/// command — the behavior is identical, just under a name that says what
/// it does (no rendering, no stitcher; bytes off disk).
fn run_files_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_serve_help();
        return Ok(());
    }

    let options = parse_serve_args(raw_args)?;
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let root = if options.dir.is_absolute() {
        options.dir.clone()
    } else {
        cwd.join(options.dir)
    };

    if !root.is_dir() {
        return Err(format!(
            "serve directory '{}' does not exist or is not a directory",
            root.display()
        ));
    }

    let (listener, addr, auto_incremented) =
        bind_dev_listener(options.host.as_str(), options.port)?;
    print_banner();
    print_section("files");
    if auto_incremented {
        print_warn(format!(
            "port {} busy — using {}",
            options.port,
            addr.port()
        ));
    }
    println!();
    print_ok(format!(
        "serving · {}",
        style_256(&format!("http://{}", addr), ACCENT_SOFT, true)
    ));
    println!(
        "    {} {}",
        style_256("·", MUTED, false),
        style(&format!("{}", root.display()), "2")
    );
    println!();
    println!(
        "    {}  stop the server",
        style_256("ctrl+c", MUTED, true)
    );
    println!();

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let root = root.clone();
                std::thread::spawn(move || {
                    serve_connection_guarded(stream, |stream| {
                        handle_static_connection(stream, root.as_path())
                    });
                });
            }
            Err(err) => {
                if !is_benign_network_error(&err) {
                    eprintln!("  {} accept failed: {err}", style("✗", "1;31"));
                }
            }
        }
    }

    Ok(())
}

fn parse_serve_args(raw_args: &[String]) -> Result<ServeOptions, String> {
    let mut dir = PathBuf::from(".albedo/dist");
    let mut host = "127.0.0.1".to_string();
    let mut port = 3000u16;
    let mut idx = 0usize;
    let mut dir_set = false;

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--dir" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --dir".to_string())?;
                dir = PathBuf::from(value);
                dir_set = true;
            }
            "--host" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --host".to_string())?;
                if value.trim().is_empty() {
                    return Err("--host must not be empty".to_string());
                }
                host = value.to_string();
            }
            "--port" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --port".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port '{value}'"))?;
                if port == 0 {
                    return Err("--port must be > 0".to_string());
                }
            }
            _ if !arg.starts_with('-') && !dir_set => {
                dir = PathBuf::from(arg);
                dir_set = true;
            }
            unknown => {
                return Err(format!("unknown serve option '{unknown}'"));
            }
        }
        idx += 1;
    }

    Ok(ServeOptions { dir, host, port })
}

fn handle_static_connection(mut stream: TcpStream, root: &Path) -> std::io::Result<()> {
    let (first_line, _headers, _body_prefetch) = read_http_request_head(&stream)?;
    if first_line.trim().is_empty() {
        return Ok(());
    }

    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let raw_target = parts.next().unwrap_or("/");
    let path = normalize_request_path(raw_target);

    if method != "GET" && method != "HEAD" {
        return write_http_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method Not Allowed",
            &[("allow", "GET, HEAD".to_string())],
        );
    }

    let selected = resolve_static_asset_path(root, path.as_str());
    match selected {
        Some(file_path) => {
            let body = std::fs::read(&file_path).unwrap_or_else(|_| Vec::new());
            let content_type = content_type_for_path(&file_path);
            let payload = if method == "HEAD" { Vec::new() } else { body };
            write_http_response(
                &mut stream,
                200,
                "OK",
                content_type,
                payload.as_slice(),
                &[("cache-control", "no-cache".to_string())],
            )
        }
        None => write_http_response(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"Not Found",
            &[("cache-control", "no-cache".to_string())],
        ),
    }
}

fn resolve_static_asset_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if request_path == "/" {
        candidates.push(root.join("index.html"));
    } else {
        let relative = request_path.trim_start_matches('/');
        if let Some(safe_rel) = sanitize_static_relative_path(relative) {
            let candidate = root.join(safe_rel);
            if candidate.is_dir() {
                candidates.push(candidate.join("index.html"));
            } else {
                candidates.push(candidate);
            }
        }
        if is_route_like_path(request_path) {
            candidates.push(root.join("index.html"));
        }
    }

    candidates.into_iter().find(|path| path.is_file())
}

fn sanitize_static_relative_path(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(segment) => out.push(segment),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    Some(out)
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("mjs") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn run_command(raw_args: &[String]) -> Result<(), String> {
    let Some(mode) = raw_args.first() else {
        return Err("missing run mode. Usage: albedo run dev [OPTIONS]".to_string());
    };

    match mode.as_str() {
        "dev" => run_dev_mode(&raw_args[1..]),
        unknown => Err(format!(
            "unknown run mode '{unknown}'. Supported modes: dev"
        )),
    }
}

fn run_dev_mode(raw_args: &[String]) -> Result<(), String> {
    let mut forwarded = Vec::new();
    let mut prod_mode = false;
    let mut skip_budget = false;
    for arg in raw_args {
        if arg == "--prod" || arg == "--production" {
            prod_mode = true;
        } else if arg == "--no-budget" {
            // Phase O.1 · `albedo build --no-budget` opts out of
            // the tier-budget gate even when tier-budget.toml is
            // present. Dev mode never gates on budget; the flag is
            // accepted there too so muscle memory is consistent.
            skip_budget = true;
        } else {
            forwarded.push(arg.clone());
        }
    }

    let cli_options = parse_dev_cli_args(&forwarded)?;
    let mut cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    if cli_options.config_path.is_none() {
        if let Some(root_override) = &cli_options.root_override {
            if root_override.is_absolute() {
                if let Some(inferred_dir) = infer_project_dir_from_root(root_override) {
                    cwd = inferred_dir;
                }
            }
        }
    }
    let contract = resolve_dev_contract(&forwarded, &cwd)?;

    print_boot_banner();
    print_section(if prod_mode { "build" } else { "dev" });
    print_kv("project", contract.project_dir.display());
    print_kv(
        "server",
        format!(
            "http://{}:{}",
            contract.server.host, contract.server.port
        ),
    );
    if contract.verbose {
        print_kv("root", contract.root.display());
        print_kv("entry", contract.entry.as_str());
        print_kv(
            "hmr",
            if contract.hmr.enabled {
                format!("{:?}", contract.hmr.transport)
            } else {
                "disabled".to_string()
            },
        );
        print_kv("hot set", format!("{}/32", contract.hot_set.len()));
        print_kv(
            "config",
            contract
                .config_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(defaults)".to_string()),
        );
        print_kv("strict", contract.strict);
    }

    if cli_options.print_contract {
        print_section("resolved contract");
        let contract_json = serde_json::to_string_pretty(&contract)
            .map_err(|err| format!("failed to serialize contract: {err}"))?;
        println!("{contract_json}");
    }

    if prod_mode {
        run_prod_build_with_budget(&contract, skip_budget, true, false)?;
        return Ok(());
    }

    // Dev mode never gates on budget; the flag is silently accepted.
    let _ = skip_budget;
    run_live_dev_runtime(contract)
}

/// One renderer for dev and prod. `albedo dev` boots the SAME production
/// streaming pipeline as `albedo serve` (Tier-A/B/C, island hydration, dynamic
/// metadata, error/loading boundaries, `head.html` pre-paint) with dev mode on
/// (error overlay + hot reload), plus a file watcher that rebuilds the dist and
/// hot-swaps the render world into the running server in place — no socket
/// churn, no second renderer. This is what closes the long-standing dev/serve
/// parity gap: everything verified on `serve` now renders identically in `dev`.
fn run_live_dev_runtime(contract: ResolvedDevContract) -> Result<(), String> {
    use albedo_server::{boot_production_server, ProductionServerOptions};

    // 1. Build the dist the production pipeline serves from.
    run_prod_build(&contract)?;

    // 2. Boot the production server with dev mode on (overlay + HMR endpoints +
    //    the shell dev-script injection from `StreamingAppState::with_dev_mode`).
    let mut opts = ProductionServerOptions::from_contract(&contract);
    opts.dev_mode = true;
    let server = boot_production_server(&opts)
        .map_err(|err| format!("failed to boot dev server: {err}"))?;

    // 3. Spawn the watch → rebuild → hot-swap loop. The reload handle shares the
    //    running server's world slot, so a swap is live for the next request.
    if let Some(reload) = server.dev_reload_handle() {
        let watch_contract = contract.clone();
        let watch_opts = opts.clone();
        let debounce = Duration::from_millis(contract.watch.debounce_ms.max(1));
        std::thread::spawn(move || {
            dev_watch_and_reload(watch_contract, watch_opts, reload, debounce);
        });
    }

    let addr = format!("{}:{}", contract.server.host, contract.server.port);
    println!();
    print_ok(format!(
        "dev · {}",
        style_256(&format!("http://{addr}"), ACCENT_SOFT, true)
    ));
    println!(
        "    {} same pipeline as `albedo serve` · overlay + hot reload on",
        style_256("·", MUTED, false)
    );
    println!();
    println!("    {}  stop the server", style_256("ctrl+c", MUTED, true));
    println!();

    if contract.open {
        let target = format!("http://{addr}");
        if let Err(err) = try_open_browser(target.as_str()) {
            print_warn(format!("failed to open browser automatically: {err}"));
        }
    }

    // 4. Run the production server on a fresh multi-thread runtime, same as
    //    `albedo serve` (the dev path stays sync until this point).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start tokio runtime: {err}"))?;
    runtime
        .block_on(server.run())
        .map_err(|err| format!("dev server runtime error: {err}"))
}

/// The dev file-watcher loop. Watches the source tree (NOT `.albedo/dist`, which
/// the rebuild writes to — so a rebuild can't retrigger itself), debounces a
/// save-burst, then rebuilds the dist and asks the reload handle to hot-swap the
/// fresh world and ping connected clients. A failed build leaves the last good
/// world serving and surfaces the error to the in-browser overlay.
fn dev_watch_and_reload(
    contract: ResolvedDevContract,
    opts: albedo_server::ProductionServerOptions,
    reload: albedo_server::DevReloadHandle,
    debounce: Duration,
) {
    let (event_tx, event_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            let _ = event_tx.send(res);
        },
        NotifyConfig::default(),
    ) {
        Ok(watcher) => watcher,
        Err(err) => {
            eprintln!("  {} watcher init failed: {err}", style("✗", "1;31"));
            return;
        }
    };
    if let Err(err) = watcher.watch(contract.root.as_path(), RecursiveMode::Recursive) {
        eprintln!(
            "  {} watcher failed to watch '{}': {}",
            style("✗", "1;31"),
            contract.root.display(),
            err
        );
        return;
    }

    loop {
        // Block until the first change, then drain the rest of the burst so a
        // multi-file save rebuilds once.
        if event_rx.recv().is_err() {
            return; // sender dropped — watcher gone
        }
        while event_rx.recv_timeout(debounce).is_ok() {}

        let rebuild_start = Instant::now();
        match run_prod_build_quiet(&contract) {
            Ok(()) => match reload.reload(&opts) {
                Ok(()) => print_ok(format!(
                    "reloaded in {}",
                    colorize_timing_ms(rebuild_start.elapsed().as_secs_f64() * 1000.0)
                )),
                Err(err) => {
                    reload.report_build_error(err.to_string());
                    eprintln!("  {} reload failed: {err}", style("✗", "1;31"));
                }
            },
            Err(err) => {
                reload.report_build_error(err.clone());
                eprintln!("  {} rebuild failed: {err}", style("✗", "1;31"));
            }
        }
    }
}

fn bind_dev_listener(
    host: &str,
    preferred_port: u16,
) -> Result<(TcpListener, SocketAddr, bool), String> {
    let ip: IpAddr = host
        .parse()
        .map_err(|err| format!("invalid host '{host}': {err}"))?;
    let start = preferred_port;
    let end = preferred_port.saturating_add(PORT_AUTO_INCREMENT_LIMIT);

    for port in start..=end {
        let addr = SocketAddr::new(ip, port);
        match TcpListener::bind(addr) {
            Ok(listener) => {
                return Ok((listener, addr, port != preferred_port));
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse && port < end => {
                continue;
            }
            Err(err) => {
                return Err(format!("failed to bind dev server on {}: {}", addr, err));
            }
        }
    }

    Err(format!(
        "all ports from {} to {} are in use",
        preferred_port,
        preferred_port.saturating_add(PORT_AUTO_INCREMENT_LIMIT)
    ))
}

fn scan_components_with_contract_policy(
    contract: &ResolvedDevContract,
    context: &str,
) -> Result<Vec<ParsedComponent>, String> {
    let scanner = ProjectScanner::new();
    let mode = if contract.strict {
        ScanMode::Strict
    } else {
        ScanMode::Lenient
    };

    let report = scanner
        .scan_directory_with_mode(&contract.root, mode)
        .map_err(|err| format!("component scan failed while {context}: {err}"))?;

    if contract.verbose {
        println!(
            "  {}  scanned {} component{} during {} ({} failure{})",
            style_256("·", ACCENT, false),
            report.components.len(),
            if report.components.len() == 1 { "" } else { "s" },
            context,
            report.failures.len(),
            if report.failures.len() == 1 { "" } else { "s" }
        );
    }

    if !report.failures.is_empty() {
        print_warn(format!(
            "{} parse failure(s) detected while {}. Continuing because strict mode is disabled.",
            report.failures.len(),
            context
        ));
        print_scan_failure_details(report.failures.as_slice(), contract.verbose);
    }

    Ok(report.components)
}

fn print_scan_failure_details(failures: &[ScanFailure], verbose: bool) {
    if failures.is_empty() {
        return;
    }

    if verbose {
        for failure in failures {
            eprintln!(
                "  {}  {} → {}",
                style("!", "1;33"),
                failure.path.display(),
                failure.message
            );
        }
        return;
    }

    if let Some(first) = failures.first() {
        print_warn(format!(
            "first parse failure: {} -> {}",
            first.path.display(),
            first.message
        ));
        print_warn("run with --verbose to print all parse failures");
    }
}

/// Reads the HTTP request line + headers from a stream.
///
/// Returns `(first_line, headers, pre_buffered_body)` where the third
/// element is whatever bytes the underlying `BufReader` slurped past the
/// final `\r\n` while parsing the head. POST handlers MUST prepend that
/// slice to whatever they read off the socket — otherwise the body's
/// first chunk goes to /dev/null and `read_exact(content_length)` blocks
/// forever waiting for bytes the OS has already delivered.
///
/// Reusing the `BufReader` directly would be cleaner, but the caller
/// continues to write directly into the `TcpStream` (response head),
/// so we drain whatever the head reader prefetched and hand it back as
/// a plain `Vec<u8>`. The `BufReader` is dropped here.
/// Hard ceiling on the total bytes of an HTTP request head the dev server will
/// read from one connection. Without it, a client that opens a socket and sends
/// an endless header stream (no terminating blank line) drives `read_line` to
/// grow a `String` until the process is out of memory — a single-connection DoS.
/// 64 KiB is orders of magnitude above any legitimate head (a browser sends a
/// few KiB) yet bounds the worst case to a harmless transient buffer.
const MAX_REQUEST_HEAD_BYTES: u64 = 64 * 1024;

/// Ceiling on a single request/header line. Caps the cost of a client that
/// floods one line with no newline (bounded anyway by the total-head cap, but
/// this rejects the abuse earlier and with a clearer error).
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;

/// Ceiling on the number of header lines. Bounds the `HashMap` a client can
/// force us to allocate; real requests carry well under a few dozen headers.
const MAX_REQUEST_HEADER_COUNT: usize = 128;

fn read_http_request_head(
    stream: &TcpStream,
) -> std::io::Result<(String, HashMap<String, String>, Vec<u8>)> {
    // `Take` hard-caps total bytes pulled from the connection for the head, so
    // every `read_line` below is bounded even against a newline-less flood.
    let mut reader = BufReader::new(stream.try_clone()?.take(MAX_REQUEST_HEAD_BYTES));
    parse_http_request_head(&mut reader)
}

/// Parses an HTTP request head (request line + headers) from a buffered reader,
/// enforcing the size/count bounds above. Split out from [`read_http_request_head`]
/// so it can be unit-tested against adversarial byte streams without a socket.
fn parse_http_request_head<R: std::io::Read>(
    reader: &mut BufReader<R>,
) -> std::io::Result<(String, HashMap<String, String>, Vec<u8>)> {
    use std::io::Error;

    let mut first_line = String::new();
    let mut headers = HashMap::new();
    reader.read_line(&mut first_line)?;
    if first_line.len() > MAX_REQUEST_LINE_BYTES {
        return Err(Error::new(ErrorKind::InvalidData, "request line exceeds limit"));
    }

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if line.len() > MAX_REQUEST_LINE_BYTES {
            return Err(Error::new(ErrorKind::InvalidData, "header line exceeds limit"));
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if headers.len() >= MAX_REQUEST_HEADER_COUNT {
            return Err(Error::new(ErrorKind::InvalidData, "header count exceeds limit"));
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    // Capture bytes the BufReader prefetched past end-of-head so a
    // following POST-body read doesn't deadlock on bytes that already
    // arrived from the client. Most short envelopes (the bakabox action
    // POST is ~7 bytes) fit entirely in this prefetch.
    let leftover = reader.buffer().to_vec();
    Ok((first_line, headers, leftover))
}

/// Global (non-module) CSS only. `.module.css` files carry their own
/// build-scoped class names and are injected per-route as scoped
/// `<style>` blocks by the manifest builder, so concatenating them
/// raw here would double-ship the rules with the wrong (unscoped)
/// selectors. Everything else under the tree is plain global CSS.
fn collect_global_css_bundle(root: &Path) -> String {
    collect_css_bundle_filtered(root, |path| {
        !path
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with(".module.css")
    })
}

fn collect_css_bundle_filtered(root: &Path, keep: impl Fn(&Path) -> bool) -> String {
    let mut css_files = WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("css"))
                .unwrap_or(false)
        })
        .filter(|entry| keep(entry.path()))
        .map(|entry| entry.path().to_path_buf())
        .collect::<Vec<_>>();
    css_files.sort();

    let mut out = String::new();
    for path in css_files {
        if let Ok(source) = std::fs::read_to_string(&path) {
            out.push_str("\n/* ");
            out.push_str(path.to_string_lossy().replace('\\', "/").as_str());
            out.push_str(" */\n");
            out.push_str(source.as_str());
            out.push('\n');
        }
    }
    out
}

/// A4 · inline every global `.css` file under `root` into each route's
/// shell `<head>` so a production `albedo serve` ships the same styles
/// the dev server inlines. Global CSS is never scanned as a component
/// (`ProjectScanner::is_component_file` rejects `.css`), so it has no
/// other path into the prod shell — without this, `albedo build` emits
/// zero global CSS and a real app needs a manual `public/styles.css` +
/// `<link>` workaround. Mirrors the dev path's `collect_css_bundle`
/// concatenation, minus `.module.css` (already injected scoped,
/// per-route, by the manifest builder). Returns the routes touched.
fn inject_global_css_into_shells(
    manifest: &mut dom_render_compiler::manifest::schema::RenderManifestV2,
    root: &Path,
) -> usize {
    let global_css = collect_global_css_bundle(root);
    if global_css.trim().is_empty() {
        return 0;
    }
    let style_block = format!("<style data-albedo-global-css>{global_css}</style>");
    let mut touched = 0usize;
    for route in manifest.routes.values_mut() {
        let head = &mut route.shell.doctype_and_head;
        // Idempotent — never double-inject if a shell already carries it.
        if head.contains("data-albedo-global-css") {
            continue;
        }
        match head.rfind("</head>") {
            Some(pos) => head.insert_str(pos, &style_block),
            None => head.push_str(&style_block),
        }
        touched += 1;
    }
    touched
}

/// Inline an optional app-authored `<head>` partial into every route shell,
/// immediately after the charset meta — so it runs BEFORE the body is parsed or
/// painted. The intended use is a tiny blocking preferences/theme bootstrap that
/// reads `localStorage` and stamps `data-*` attributes onto `<html>` pre-paint,
/// eliminating the flash-of-default-theme a hydration-time effect would
/// otherwise cause (the islands hydrate on idle, well after first paint).
///
/// Looked up at `<root>/src/head.html` then `<root>/head.html`; absent → no-op.
/// The file is raw head HTML — the app writes whatever it needs (a `<script>`,
/// `<link rel=preconnect>`, …) and ALBEDO injects it verbatim. Idempotent via a
/// sentinel marker. Returns the number of routes touched.
fn inject_head_partial_into_shells(
    manifest: &mut dom_render_compiler::manifest::schema::RenderManifestV2,
    root: &Path,
) -> usize {
    let partial = ["src/head.html", "head.html"]
        .into_iter()
        .map(|rel| root.join(rel))
        .find_map(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    if partial.trim().is_empty() {
        return 0;
    }

    const MARKER: &str = "<!--albedo:head-partial-->";
    let block = format!("{MARKER}{partial}");
    const CHARSET: &str = "<meta charset=\"utf-8\">";

    let mut touched = 0usize;
    for route in manifest.routes.values_mut() {
        let head = &mut route.shell.doctype_and_head;
        // Idempotent — never double-inject if a shell already carries it.
        if head.contains(MARKER) {
            continue;
        }
        // Place it right after the charset meta (keeps charset first, the
        // partial still inside the first bytes of <head> and ahead of <body>).
        if let Some(pos) = head.find(CHARSET) {
            head.insert_str(pos + CHARSET.len(), &block);
        } else if let Some(pos) = head.find("<head>") {
            head.insert_str(pos + "<head>".len(), &block);
        } else {
            head.insert_str(0, &block);
        }
        touched += 1;
    }
    touched
}

fn try_open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|err| err.to_string())?;
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err("automatic browser open is not supported on this platform".to_string())
}

/// Run a connection handler with both I/O errors and panics turned into graceful
/// outcomes. A panic mid-request becomes a `500` written back on the socket instead
/// of a silently dropped connection and a dead worker thread. The stream is cloned
/// up front so the fallback response can still be written after the handler (which
/// owns the original) has unwound.
fn serve_connection_guarded<F>(stream: TcpStream, handler: F)
where
    F: FnOnce(TcpStream) -> std::io::Result<()>,
{
    let fallback = stream.try_clone().ok();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || handler(stream)));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            if !is_benign_network_error(&err) {
                eprintln!("  {} request failed: {err}", style("✗", "1;31"));
            }
        }
        Err(panic) => {
            eprintln!(
                "  {} request panicked: {}",
                style("✗", "1;31"),
                panic_detail(panic.as_ref())
            );
            if let Some(mut socket) = fallback {
                let _ = write_http_response(
                    &mut socket,
                    500,
                    "Internal Server Error",
                    "application/json",
                    br#"{"error":"internal server error"}"#,
                    &[],
                );
            }
        }
    }
}

/// Best-effort human-readable message from a caught panic payload.
fn panic_detail(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&'static str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, String)],
) -> std::io::Result<()> {
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        headers.push_str(name);
        headers.push_str(": ");
        headers.push_str(value);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");

    stream.write_all(headers.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn normalize_request_path(raw_target: &str) -> String {
    let without_query = raw_target.split('?').next().unwrap_or(raw_target);
    let without_fragment = without_query.split('#').next().unwrap_or(without_query);

    if without_fragment.is_empty() {
        "/".to_string()
    } else {
        without_fragment.to_string()
    }
}

fn is_route_like_path(path: &str) -> bool {
    if path == "/" || path == "/index.html" {
        return true;
    }
    let segment = path.rsplit('/').next().unwrap_or(path);
    !segment.contains('.')
}

fn is_benign_network_error(err: &std::io::Error) -> bool {
    if let Some(code) = err.raw_os_error() {
        if code == 10053 || code == 10054 {
            return true;
        }
    }

    matches!(
        err.kind(),
        ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionReset
            | ErrorKind::BrokenPipe
            | ErrorKind::UnexpectedEof
    )
}

/// Phase O.1 · convenience wrapper preserving the old call sites'
/// signature; the gate work happens in
/// [`run_prod_build_with_budget`].
fn run_prod_build(contract: &ResolvedDevContract) -> Result<(), String> {
    // serve / dev startup call this — full build presentation, no tier report
    // (that's reserved for the explicit `albedo build`).
    run_prod_build_with_budget(contract, false, false, false)
}

/// Silent build for the dev hot-reload path — does the full build but prints
/// nothing (the watcher prints a single "reloaded in Xms" line instead of the
/// whole build log on every save). Warnings and errors still surface.
fn run_prod_build_quiet(contract: &ResolvedDevContract) -> Result<(), String> {
    run_prod_build_with_budget(contract, false, false, true)
}

fn run_prod_build_with_budget(
    contract: &ResolvedDevContract,
    skip_budget: bool,
    show_tiers: bool,
    quiet: bool,
) -> Result<(), String> {
    let out_dir = contract.project_dir.join(".albedo").join("dist");

    let scan_start = Instant::now();
    let components =
        scan_components_with_contract_policy(contract, "building production artifacts")?;

    if components.is_empty() {
        return Err(format!(
            "no component files found under '{}' (.js/.jsx/.ts/.tsx expected)",
            contract.root.display()
        ));
    }

    // `quiet` (dev hot reload) does the whole build silently so a save prints a
    // single "reloaded in Xms" line — the presentation below is suppressed, but
    // warnings and errors still surface.
    if !quiet {
        print_section("build");
        print_kv("components", components.len());
        print_kv(
            "scan",
            colorize_timing_ms(scan_start.elapsed().as_secs_f64() * 1000.0),
        );
    }

    let compile_start = Instant::now();
    let out_dir_for_closure = out_dir.clone();
    let build_work = move || {
        let scanner = ProjectScanner::new();
        let compiler = scanner.build_compiler(components);
        let (manifest, tier_report) = compiler
            .optimize_manifest_v2_with_tier_report()
            .map_err(|err| format!("failed to optimize manifest: {err}"))?;

        let mut module_sources = HashMap::new();
        let mut missing_sources = 0usize;
        for component in &manifest.components {
            if module_sources.contains_key(&component.module_path) {
                continue;
            }

            match read_manifest_module_source(contract, &component.module_path) {
                Ok(source) => {
                    module_sources.insert(component.module_path.clone(), source);
                }
                Err(_) => {
                    missing_sources += 1;
                }
            }
        }

        let report = compiler
            .emit_bundle_artifacts_from_manifest_v2_with_sources(
                &manifest,
                &module_sources,
                &BundlePlanOptions::default(),
                &out_dir_for_closure,
            )
            .map_err(|err| format!("failed to emit production artifacts: {err}"))?;
        Ok::<_, String>((manifest, tier_report, report, missing_sources))
    };
    let (mut manifest, tier_report, report, missing_sources) = if quiet {
        build_work()?
    } else {
        with_spinner("compiling production bundle…", build_work)?
    };

    // `albedo build` shows the tier breakdown — the "instrument for light" view
    // of what the app compiled to (luminance bars per tier). Suppressed for
    // serve / dev (re)builds so a hot reload stays a single line.
    if show_tiers {
        printer::print_tier_report(&tier_report, &contract.root.display().to_string());
    }

    // A4 · inline global CSS into every route shell so prod ships the
    // same styles dev inlines. Runs after the manifest is built but
    // before it's serialized (the prod server reads shells straight
    // from `render-manifest.v2.json`); the emit step above writes JS
    // chunks only and never the shell, so this ordering is safe.
    let css_routes = inject_global_css_into_shells(&mut manifest, &contract.root);

    // Inline the optional `src/head.html` pre-paint partial (theme/preferences
    // bootstrap) into every shell head, right after the charset meta.
    let head_partial_routes = inject_head_partial_into_shells(&mut manifest, &contract.root);
    if head_partial_routes > 0 && !quiet {
        println!("    head partial inlined into {head_partial_routes} routes");
    }

    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("failed to serialize manifest: {err}"))?;
    let manifest_path = out_dir.join("render-manifest.v2.json");
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create output directory '{}': {err}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&manifest_path, manifest_json).map_err(|err| {
        format!(
            "failed to write manifest '{}': {err}",
            manifest_path.display()
        )
    })?;
    let runtime_asset_path = out_dir.join("_albedo").join("runtime.js");
    if let Some(parent) = runtime_asset_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create runtime asset directory '{}': {err}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&runtime_asset_path, albedo_runtime_shim_template()).map_err(|err| {
        format!(
            "failed to write runtime shim '{}': {err}",
            runtime_asset_path.display()
        )
    })?;
    // Bakabox decoder. The runtime imports it as `./bincode.js`; both
    // files live in `_albedo/` so the relative import resolves.
    let bincode_asset_path = out_dir.join("_albedo").join("bincode.js");
    std::fs::write(&bincode_asset_path, albedo_bincode_template()).map_err(|err| {
        format!(
            "failed to write bakabox decoder '{}': {err}",
            bincode_asset_path.display()
        )
    })?;
    // Bakabox WT bootstrap. Imports `./bincode.js`, so it must be in the
    // same directory as the decoder above.
    let wt_bootstrap_asset_path = out_dir.join("_albedo").join("wt-bootstrap.js");
    std::fs::write(&wt_bootstrap_asset_path, albedo_wt_bootstrap_template()).map_err(|err| {
        format!(
            "failed to write WT bootstrap '{}': {err}",
            wt_bootstrap_asset_path.display()
        )
    })?;
    let hydration_asset_path = out_dir.join("_albedo").join("hydration.js");
    std::fs::write(&hydration_asset_path, albedo_hydration_runtime_template()).map_err(|err| {
        format!(
            "failed to write hydration runtime '{}': {err}",
            hydration_asset_path.display()
        )
    })?;
    // Phase L · ship the Link/form/Navigate client interception
    // module next to the rest of the bakabox assets. Loaded by the
    // shell shim after runtime.js so the IIFE finds
    // `__ALBEDO_RUNTIME` already wired.
    let link_forms_asset_path = out_dir.join("_albedo").join("link-forms.js");
    std::fs::write(&link_forms_asset_path, albedo_link_forms_template()).map_err(|err| {
        format!(
            "failed to write link/forms client '{}': {err}",
            link_forms_asset_path.display()
        )
    })?;
    // Phase P · post-P — no `<dist>/index.html` write. The
    // production server's streaming arm renders `/` from the
    // manifest's pre-baked route shell; a literal `index.html` in
    // dist would shadow it via the public-assets dispatch.

    if !quiet {
        print_ok(format!(
            "built in {}",
            colorize_timing_ms(compile_start.elapsed().as_secs_f64() * 1000.0)
        ));
        print_kv("output", out_dir.display());
        print_kv("artifacts", report.artifacts.len() + 5);
        if css_routes > 0 {
            print_kv(
                "global css",
                format!(
                    "inlined into {css_routes} route{}",
                    if css_routes == 1 { "" } else { "s" }
                ),
            );
        }
    }
    if missing_sources > 0 {
        print_warn(format!(
            "{missing_sources} module{} had unreadable sources — skipped from static precompile",
            if missing_sources == 1 { "" } else { "s" }
        ));
    }

    let _ = (
        &manifest_path,
        &runtime_asset_path,
        &hydration_asset_path,
        &link_forms_asset_path,
    );
    if !quiet {
        for artifact in report.artifacts.iter().take(6) {
            println!(
                "    {} {} {}",
                style_256("·", MUTED, false),
                artifact.relative_path,
                style(&format!("({} B)", artifact.bytes), "2")
            );
        }
        if report.artifacts.len() > 6 {
            println!(
                "    {} {}",
                style_256("·", MUTED, false),
                style(&format!("+{} more", report.artifacts.len() - 6), "2")
            );
        }
    }

    // Phase N · copy `<project>/public/` into `.albedo/dist/public/`
    // so a pure-static deploy (CDN / static-export ship target) ships
    // images, favicons, fonts, etc. alongside the rendered shell and
    // hydration JS. Idempotent — re-runs overwrite the existing files
    // without leaving stale entries because we copy into a fresh
    // sub-dir each build.
    let public_src = contract.project_dir.join("public");
    if public_src.is_dir() {
        let public_dst = out_dir.join("public");
        let copied = copy_public_dir(&public_src, &public_dst)?;
        if copied > 0 && !quiet {
            print_kv(
                "public",
                format!("{copied} file{}", if copied == 1 { "" } else { "s" }),
            );
        }
    }

    // Phase O.1 + O.3 · gate the build on the tier budget. File-gated:
    // only runs when tier-budget.toml exists. The emit report is
    // passed in so the O.3 bundle-byte pass can run against measured
    // wrapper sizes — without it, only the source-weight gate fires.
    // Passing skip_budget=true (via --no-budget on ship/build) opts
    // out of both passes even when the file is present.
    enforce_budget_after_build(contract, &manifest, Some(&report), skip_budget)?;

    Ok(())
}

/// Phase N · recursive directory copy used by the `public/` ship
/// step. Returns the count of files copied so the build summary can
/// print it. Skips symlinks (they'd require a portability story
/// across platforms) and surfaces the offending path on any IO
/// failure so the user sees exactly what went wrong.
fn copy_public_dir(src: &Path, dst: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dst)
        .map_err(|err| format!("failed to create '{}': {err}", dst.display()))?;
    let mut count = 0usize;
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.map_err(|err| format!("public/ walk failed: {err}"))?;
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        let rel = path
            .strip_prefix(src)
            .map_err(|err| format!("public/ path strip failed: {err}"))?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        if path.is_dir() {
            std::fs::create_dir_all(&target)
                .map_err(|err| format!("failed to create '{}': {err}", target.display()))?;
        } else if path.is_file() {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    format!("failed to create '{}': {err}", parent.display())
                })?;
            }
            std::fs::copy(path, &target).map_err(|err| {
                format!(
                    "failed to copy '{}' → '{}': {err}",
                    path.display(),
                    target.display()
                )
            })?;
            count += 1;
        }
    }
    Ok(count)
}

fn read_manifest_module_source(
    contract: &ResolvedDevContract,
    module_path: &str,
) -> Result<String, String> {
    let as_path = PathBuf::from(module_path);
    let candidates = if as_path.is_absolute() {
        vec![as_path]
    } else {
        vec![
            contract.project_dir.join(&as_path),
            contract.root.join(&as_path),
            PathBuf::from(module_path),
        ]
    };

    for candidate in candidates {
        if candidate.is_file() {
            return std::fs::read_to_string(&candidate).map_err(|err| {
                format!(
                    "failed to read module source '{}': {err}",
                    candidate.display()
                )
            });
        }
    }

    Err(format!("module source '{module_path}' not found"))
}

fn infer_project_dir_from_root(root: &Path) -> Option<PathBuf> {
    let parent = root.parent()?;
    let root_name = root.file_name().and_then(|name| name.to_str());
    let parent_name = parent.file_name().and_then(|name| name.to_str());

    if root_name == Some("components") && parent_name == Some("src") {
        return parent.parent().map(Path::to_path_buf);
    }

    Some(parent.to_path_buf())
}

fn scaffold_project(target: &Path, options: &InitOptions) -> Result<(), String> {
    if target.exists() && !target.is_dir() {
        return Err(format!(
            "target '{}' exists and is not a directory",
            target.display()
        ));
    }
    std::fs::create_dir_all(target).map_err(|err| {
        format!(
            "failed to create target directory '{}': {err}",
            target.display()
        )
    })?;

    // Phase P · Stream F.2 — file-based routing means `src/routes/`
    // is the new entry-shape. Components co-locate under
    // `src/components/`. `public/` ships static assets; the dev
    // server + production AlbedoServer both serve them at root.
    for dir in [
        target.join("src").join("routes"),
        target.join("src").join("components"),
        target.join("public"),
    ] {
        std::fs::create_dir_all(&dir).map_err(|err| {
            format!(
                "failed to create scaffold directory '{}': {err}",
                dir.display()
            )
        })?;
    }

    let package_name = infer_package_name(target);
    let package_json = SCAFFOLD_PACKAGE_JSON.replace("__ALBEDO_APP_NAME__", package_name.as_str());

    // Routes (file-based; one file per URL).
    write_scaffold_file(
        &target.join("src").join("routes").join("layout.tsx"),
        SCAFFOLD_LAYOUT,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("routes").join("index.tsx"),
        SCAFFOLD_INDEX_ROUTE,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("routes").join("chat.tsx"),
        SCAFFOLD_CHAT_ROUTE,
        options.force,
    )?;
    // Shared components (imported by routes).
    write_scaffold_file(
        &target.join("src").join("components").join("Hero.tsx"),
        SCAFFOLD_HERO,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("components").join("Counter.tsx"),
        SCAFFOLD_COUNTER,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("styles.css"),
        SCAFFOLD_STYLES,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("albedo-env.d.ts"),
        SCAFFOLD_ENV_DTS,
        options.force,
    )?;
    write_scaffold_file(&target.join(DEV_CONFIG_TS), SCAFFOLD_CONFIG, options.force)?;
    write_scaffold_file(
        &target.join("package.json"),
        package_json.as_str(),
        options.force,
    )?;
    // Phase P · post-P — public/ is intentionally empty in the
    // scaffold; users drop favicon / images / fonts here and they're
    // served at `/`. The renderer + streaming handler own the HTML.
    write_scaffold_file(
        &target.join("tsconfig.json"),
        SCAFFOLD_TSCONFIG,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("README.md"),
        SCAFFOLD_README,
        options.force,
    )?;
    write_scaffold_file(
        &target.join(".gitignore"),
        SCAFFOLD_GITIGNORE,
        options.force,
    )?;
    // Phase O.1 + O.3 budget gate. Drops in at project root; the
    // build / ship paths auto-enforce when present.
    write_scaffold_file(
        &target.join("tier-budget.toml"),
        SCAFFOLD_TIER_BUDGET,
        options.force,
    )?;

    Ok(())
}

fn write_scaffold_file(path: &Path, content: &str, force: bool) -> Result<(), String> {
    if path.exists() && !force {
        return Err(format!(
            "file '{}' already exists (use --force to overwrite)",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create directory '{}': {err}", parent.display()))?;
    }
    std::fs::write(path, content)
        .map_err(|err| format!("failed to write scaffold file '{}': {err}", path.display()))
}

fn print_init_success(project_name: &str) {
    print_banner();
    print_ok(format!(
        "created {}{}",
        style_256(project_name, ACCENT_SOFT, true),
        style("/", "2")
    ));
    println!();
    println!(
        "  {}",
        style("a starter, lit — three components, one at each tier of light.", "2")
    );
    println!();
    print_section("next");
    println!(
        "    {}  cd {}",
        style_256("1", ACCENT, true),
        style_256(project_name, ACCENT_SOFT, true)
    );
    println!(
        "    {}  {}",
        style_256("2", ACCENT, true),
        style_256("albedo dev", ACCENT_SOFT, true)
    );
    println!();
    println!(
        "  {}",
        style("run it, and watch albedo sort them by how much they move.", "2")
    );
    println!();
}

fn infer_package_name(target: &Path) -> String {
    let fallback = "albedo-app".to_string();
    let Some(name_os) = target.file_name() else {
        return fallback;
    };
    let raw = name_os.to_string_lossy().to_string();
    sanitize_package_name(&raw).unwrap_or(fallback)
}

fn sanitize_package_name(value: &str) -> Option<String> {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' || ch == '.' || ch == ' ' {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn albedo_runtime_shim_template() -> String {
    include_str!("../../assets/albedo-runtime.js").to_string()
}

fn albedo_hydration_runtime_template() -> String {
    include_str!("../../assets/albedo-hydration.js").to_string()
}

/// Bakabox bincode decoder, deployed to `_albedo/bincode.js` so the
/// runtime's `import './bincode.js'` resolves at the same `_albedo/`
/// origin. Pairs with [`albedo_runtime_shim_template`]; the two ship
/// together or the import will 404 at boot.
fn albedo_bincode_template() -> String {
    include_str!("../../assets/bincode.js").to_string()
}

/// Bakabox WT bootstrap, deployed to `_albedo/wt-bootstrap.js`. Imports
/// `./bincode.js` at runtime, so it must ship alongside both the
/// runtime and the decoder.
fn albedo_wt_bootstrap_template() -> String {
    include_str!("../../assets/albedo-wt-bootstrap.js").to_string()
}

/// Phase L · client-side Link / form-action / Navigate interception.
/// Deployed to `_albedo/link-forms.js`. The IIFE reads
/// `globalThis.__ALBEDO_RUNTIME` set up by `runtime.js`, so this
/// asset MUST load after the main runtime — the shell shim emits the
/// `<script>` tag in document order to guarantee that.
fn albedo_link_forms_template() -> String {
    include_str!("../../assets/albedo-link-forms.js").to_string()
}

fn run_completions_command(raw_args: &[String]) -> Result<(), String> {
    let shell = raw_args.first().map(|s| s.as_str()).unwrap_or("");
    let script = match shell {
        "bash" => COMPLETIONS_BASH,
        "zsh" => COMPLETIONS_ZSH,
        "fish" => COMPLETIONS_FISH,
        "powershell" | "pwsh" => COMPLETIONS_POWERSHELL,
        _ => {
            return Err(
                "usage: albdo completions <bash|zsh|fish|powershell>\n\
                 Examples:\n  \
                   albdo completions bash        >> ~/.bashrc\n  \
                   albdo completions zsh         >> ~/.zshrc\n  \
                   albdo completions fish        > ~/.config/fish/completions/albdo.fish\n  \
                   albdo completions powershell  >> $PROFILE"
                    .to_string(),
            );
        }
    };
    print!("{script}");
    Ok(())
}

// ─── Static completion scripts ────────────────────────────────────────────────
// Generated once here; the CI pipes `albdo completions <shell>` to produce the
// files that get bundled into each platform installer.

const COMPLETIONS_BASH: &str = r#"# albdo bash completions
_albdo_completions() {
    local cur prev words
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    local commands="init dev build ship serve files budget run completions help"

    case "$prev" in
        albdo)
            COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
            return 0
            ;;
        completions)
            COMPREPLY=( $(compgen -W "bash zsh fish powershell" -- "$cur") )
            return 0
            ;;
        ship)
            COMPREPLY=( $(compgen -W "--target --config --entry --no-budget" -- "$cur") )
            return 0
            ;;
        --target)
            COMPREPLY=( $(compgen -W "docker fly static" -- "$cur") )
            return 0
            ;;
        dev|build|run)
            COMPREPLY=( $(compgen -W "--config --entry --host --port --no-hmr --strict --verbose --open --prod --no-budget" -- "$cur") )
            return 0
            ;;
        budget)
            COMPREPLY=( $(compgen -W "--strict --format --config" -- "$cur") )
            return 0
            ;;
        --format)
            COMPREPLY=( $(compgen -W "pretty json" -- "$cur") )
            return 0
            ;;
        init)
            COMPREPLY=( $(compgen -W "--force" -- "$cur") )
            return 0
            ;;
        serve|files)
            COMPREPLY=( $(compgen -W "--dir --host --port" -- "$cur") )
            return 0
            ;;
    esac

    COMPREPLY=( $(compgen -W "$commands" -- "$cur") )
}
complete -F _albdo_completions albdo
"#;

const COMPLETIONS_ZSH: &str = r#"#compdef albdo
_albdo() {
    local -a commands
    commands=(
        'init:Create a tiered starter app scaffold'
        'dev:Start the live dev server with HMR'
        'build:Compile an optimised production build'
        'ship:Build and configure deployment target files'
        'serve:Production build + serve via the same stitcher as dev'
        'files:Static file server (defaults to .albedo/dist)'
        'budget:Evaluate the tier budget against the current build'
        'run:Run a sub-mode (e.g. run dev)'
        'completions:Emit shell completion script to stdout'
        'help:Show command list and examples'
    )

    local -a dev_flags
    dev_flags=(
        '--config[Use explicit albedo.config.json/ts]:file:_files'
        '--entry[Override entry module]:file:_files'
        '--host[Override server host]:host'
        '--port[Override server port]:port'
        '--no-hmr[Disable HMR]'
        '--strict[Enable strict startup behaviour]'
        '--verbose[Verbose diagnostics]'
        '--open[Open browser on startup]'
        '--prod[Production build mode]'
    )

    case $state in
        (cmd)
            _describe 'albdo commands' commands && return 0
            ;;
    esac

    _arguments -C \
        '1: :->cmd' \
        '*: :->args'

    case $state in
        (cmd)
            _describe 'albdo commands' commands
            ;;
        (args)
            case $words[2] in
                (completions)
                    _values 'shell' bash zsh fish powershell
                    ;;
                (dev|build)
                    _arguments $dev_flags
                    ;;
                (ship)
                    _arguments \
                        '--target[Deployment target]:target:(docker fly static)' \
                        '--no-budget[Skip the tier-budget gate]' \
                        $dev_flags
                    ;;
                (budget)
                    _arguments \
                        '--strict[Require tier-budget.toml; fail if missing]' \
                        '--format[Output format]:format:(pretty json)' \
                        '--config[Use explicit albedo.config.json/ts]:file:_files'
                    ;;
                (serve)
                    _arguments \
                        '--dir[Directory to serve]:directory:_files -/' \
                        '--host[Bind host]:host' \
                        '--port[Bind port]:port'
                    ;;
                (init)
                    _arguments '--force[Overwrite existing files]'
                    ;;
            esac
            ;;
    esac
}
_albdo "$@"
"#;

const COMPLETIONS_FISH: &str = r#"# albdo fish completions
set -l albdo_commands init dev build ship serve files budget run completions help

# Disable file completions for the main command
complete -c albdo -f

# Top-level commands
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a init        -d 'Create a tiered starter app scaffold'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a dev         -d 'Start the live dev server with HMR'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a build       -d 'Compile an optimised production build'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a ship        -d 'Build and configure deployment target files'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a serve       -d 'Serve static files from a directory'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a budget      -d 'Evaluate the tier budget against the current build'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a run         -d 'Run a sub-mode'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a completions -d 'Emit shell completion script to stdout'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a help        -d 'Show command list and examples'

# completions <shell>
complete -c albdo -n "__fish_seen_subcommand_from completions" -a "bash zsh fish powershell"

# dev / build flags
for sub in dev build run
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l config  -d 'Use explicit albedo config file'     -r
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l entry   -d 'Override entry module'               -r
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l host    -d 'Override server host'                -r
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l port    -d 'Override server port'                -r
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l no-hmr  -d 'Disable HMR'
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l strict  -d 'Enable strict startup behaviour'
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l verbose -d 'Verbose diagnostics'
    complete -c albdo -n "__fish_seen_subcommand_from $sub" -l open    -d 'Open browser on startup'
end
complete -c albdo -n "__fish_seen_subcommand_from dev build" -l prod -d 'Production build mode'

# ship flags
complete -c albdo -n "__fish_seen_subcommand_from ship" -l target -d 'Deployment target' -r -a "docker fly static"
complete -c albdo -n "__fish_seen_subcommand_from ship" -l config -d 'Use explicit albedo config file' -r
complete -c albdo -n "__fish_seen_subcommand_from ship" -l no-budget -d 'Skip the tier-budget gate'

# budget flags
complete -c albdo -n "__fish_seen_subcommand_from budget" -l strict -d 'Require tier-budget.toml; fail if missing'
complete -c albdo -n "__fish_seen_subcommand_from budget" -l format -d 'Output format' -r -a "pretty json"
complete -c albdo -n "__fish_seen_subcommand_from budget" -l config -d 'Use explicit albedo config file' -r

# serve flags
complete -c albdo -n "__fish_seen_subcommand_from serve" -l dir  -d 'Directory to serve' -r
complete -c albdo -n "__fish_seen_subcommand_from serve" -l host -d 'Bind host'          -r
complete -c albdo -n "__fish_seen_subcommand_from serve" -l port -d 'Bind port'          -r

# init flags
complete -c albdo -n "__fish_seen_subcommand_from init" -l force -d 'Overwrite existing files'
"#;

const COMPLETIONS_POWERSHELL: &str = r#"# albdo PowerShell tab-completion
Register-ArgumentCompleter -Native -CommandName @('albdo', 'albdo.exe') -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $tokens = $commandAst.CommandElements
    $nTokens = $tokens.Count

    $commands = @('init','dev','build','ship','serve','files','budget','run','completions','help')
    $devFlags = @('--config','--entry','--host','--port','--no-hmr','--strict','--verbose','--open','--prod','--no-budget')

    if ($nTokens -le 2) {
        $commands | Where-Object { $_ -like "$wordToComplete*" } |
            ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }
        return
    }

    $subcommand = $tokens[1].ToString()

    switch ($subcommand) {
        'completions' {
            @('bash','zsh','fish','powershell') | Where-Object { $_ -like "$wordToComplete*" } |
                ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }
        }
        { $_ -in 'dev','build','run' } {
            $devFlags | Where-Object { $_ -like "$wordToComplete*" } |
                ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
        }
        'ship' {
            if ($wordToComplete -eq '--target' -or ($nTokens -ge 3 -and $tokens[$nTokens-2] -eq '--target')) {
                @('docker','fly','static') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }
            } else {
                @('--target','--config','--entry','--no-budget') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
            }
        }
        'budget' {
            if ($wordToComplete -eq '--format' -or ($nTokens -ge 3 -and $tokens[$nTokens-2] -eq '--format')) {
                @('pretty','json') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }
            } else {
                @('--strict','--format','--config') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
            }
        }
        { $_ -in 'serve','files' } {
            @('--dir','--host','--port') | Where-Object { $_ -like "$wordToComplete*" } |
                ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
        }
        'init' {
            @('--force') | Where-Object { $_ -like "$wordToComplete*" } |
                ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
        }
    }
}
"#;

fn print_help() {
    print_banner();
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo <command> [options]", "1")
    );

    print_section("commands");
    print_command("init", "<name>", "scaffold a new app");
    print_command("dev", "[dir]", "start the dev server — live reload");
    print_command("build", "[dir]", "compile for production");
    print_command("serve", "", "build and run the production server");
    print_command("ship", "[dir]", "build and configure a deploy target");
    print_command("files", "[dir]", "serve static files from a folder");
    print_command("budget", "[dir]", "check the tier budget");
    print_command("completions", "<shell>", "print shell completions");
    print_command("help", "", "show this help");

    print_section("dev flags");
    print_option("--config <FILE>", "explicit albedo config");
    print_option("--entry <FILE>", "override entry module");
    print_option("--host <IP>", "server host");
    print_option("--port <PORT>", "server port");
    print_option("--no-hmr", "disable HMR");
    print_option("--strict", "strict startup");
    print_option("--verbose, -v", "verbose diagnostics");
    print_option("--open", "open browser on start");
    print_option("--prod", "production build mode");

    print_section("examples");
    print_example("albedo init my-app");
    print_example("cd my-app && albedo dev");
    print_example("albedo ship --target docker");
    print_example("albedo serve");
    print_example("albedo files ./.albedo/dist");
    println!();
}

fn print_init_help() {
    print_banner();
    print_section("init");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo init <project> [--force]", "1")
    );
    print_option("--force", "overwrite existing files");
    println!();
}

fn print_ship_help() {
    print_banner();
    print_section("ship");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo ship [dir] [--target <name>] [--no-budget]", "1")
    );
    print_option("--target <name>", "docker | fly | static");
    print_option("--config <FILE>", "explicit albedo config");
    print_option("--entry <FILE>", "override entry module");
    print_option("--no-budget", "skip the tier-budget gate");
    println!();
}

fn print_budget_help() {
    print_banner();
    print_section("budget");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo budget [dir] [--strict] [--format pretty|json]", "1")
    );
    print_option(
        "--strict",
        "require tier-budget.toml; fail if missing",
    );
    print_option("--format <kind>", "pretty (default) | json");
    print_option("--config <FILE>", "explicit albedo config");
    println!();
}

fn print_serve_help() {
    print_banner();
    print_section("serve");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo serve [--host <IP>] [--port <PORT>]", "1")
    );
    println!();
    println!(
        "    {} builds your app, then runs the production server:",
        style("·", "2")
    );
    println!(
        "        {} {}",
        style_256("·", ACCENT_DEEP, false),
        style("streams every route — static inline, dynamic on demand", "2")
    );
    println!(
        "        {} {}",
        style_256("·", ACCENT_DEEP, false),
        style("runs your server actions and live shared state", "2")
    );
    println!(
        "        {} {}",
        style_256("·", ACCENT_DEEP, false),
        style("hydrates interactive islands with zero round-trips", "2")
    );
    println!();
    print_option("--host <IP>", "bind host (default: 127.0.0.1)");
    print_option("--port <PORT>", "bind port (default: 3000)");
    print_option(
        "<dir> | --dir <DIR>",
        "BACK-COMPAT: falls through to `albedo files <dir>` (static-only)",
    );
    println!();
}

fn print_command(command: &str, args: &str, description: &str) {
    // Align the description column no matter how long the command/args are. ANSI
    // escapes have zero display width, so pad on the PLAIN text, then colorize —
    // padding a pre-styled string counts the escape bytes and skews the column.
    let plain_len = command.chars().count() + 1 + args.chars().count();
    let pad = COL_WIDTH.saturating_sub(plain_len);
    println!(
        "    {} {}{}  {}",
        style_256(command, ACCENT_SOFT, true),
        style(args, "2"),
        " ".repeat(pad),
        description,
    );
}

fn print_option(option: &str, description: &str) {
    let pad = COL_WIDTH.saturating_sub(option.chars().count());
    println!(
        "    {}{}  {}",
        style_256(option, ACCENT_SOFT, true),
        " ".repeat(pad),
        style(description, "2"),
    );
}

fn print_example(cmd: &str) {
    println!(
        "    {} {}",
        style_256("$", ACCENT, true),
        style(cmd, "2")
    );
}

fn print_banner() {
    println!();
    println!(
        "  {}  {}  {}  {}",
        gradient_text("albedo", &BRAND_PALETTE, true),
        style_256("·", ACCENT_DEEP, false),
        style(env!("CARGO_PKG_VERSION"), "1"),
        style("— fast JSX for Rust", "2")
    );
    // Halation halo — a dim champagne hairline under the wordmark (the glow
    // around a bright thing). Six glyphs to match "albedo".
    println!("  {}", style_256("──────", ACCENT_DEEP, false));
    println!();
}

/// The server-boot masthead — the first thing anyone sees when an ALBEDO
/// server comes up, so it earns a full block wordmark rather than the compact
/// `print_banner` line the help screens use. The letters glow top-down through
/// the champagne ramp (light catching the crown, cooling to deep gold in the
/// drop-shadow base — Halation made literal). `NO_COLOR` degrades gracefully:
/// the block shape still reads in monochrome.
fn print_boot_banner() {
    // "ALBDO" in the FIGlet "ANSI Shadow" font (generated with pyfiglet, not
    // hand-drawn). The apostrophe has no glyph in this font, so the mark reads
    // ALBDO up top and the literal "ALB'DO" lives in the tagline below. Every
    // row is 41 cells wide; regenerate rather than edit by hand if the text
    // ever changes: `pyfiglet -f ansi_shadow ALBDO`.
    const ART: [&str; 6] = [
        " █████╗ ██╗     ██████╗ ██████╗  ██████╗ ",
        "██╔══██╗██║     ██╔══██╗██╔══██╗██╔═══██╗",
        "███████║██║     ██████╔╝██║  ██║██║   ██║",
        "██╔══██║██║     ██╔══██╗██║  ██║██║   ██║",
        "██║  ██║███████╗██████╔╝██████╔╝╚██████╔╝",
        "╚═╝  ╚═╝╚══════╝╚═════╝ ╚═════╝  ╚═════╝ ",
    ];
    // Vertical glow: cream at the crown, cooling through gold to deep gold in
    // the shadow row. Reads as light catching the top edge of the letters.
    const ROW_LUMEN: [u8; 6] = [230, 223, 222, 221, 179, 137];

    println!();
    for (row, line) in ART.iter().enumerate() {
        println!("  {}", style_256(line, ROW_LUMEN[row], true));
    }
    println!();
    // The brand + the "version" label, then the solar tier ladder (STRATEGY's
    // Sol → Equinox → Umbra → Persephone) as the signature line — muted so the
    // hierarchy holds: mark brightest, Version Beta next, the tiers a quiet
    // footer. Slashes dimmed to let the names carry.
    let sep = style(" / ", "2");
    let tiers = ["SOL", "EQUINOX", "UMBRA", "PERSEPHONE"]
        .iter()
        .map(|name| style_256(name, MUTED, false))
        .collect::<Vec<_>>()
        .join(&sep);
    println!(
        "  {}  {}   {}",
        gradient_text("ALB'DO", &BRAND_PALETTE, true),
        style_256("Version Beta", ACCENT_SOFT, true),
        tiers,
    );
    // Full-width champagne hairline — the halo under the mark (41 to match art).
    println!("  {}", style_256(&"─".repeat(41), ACCENT_DEEP, false));
    println!();
}

fn print_section(title: &str) {
    println!();
    println!(
        "  {} {}",
        style_256("▸", ACCENT, true),
        style(title, "1")
    );
}

fn print_kv(label: &str, value: impl std::fmt::Display) {
    println!(
        "    {:<14} {}",
        style_256(label, MUTED, false),
        value
    );
}

fn print_ok(message: impl std::fmt::Display) {
    println!("  {} {}", style("✓", "1;32"), message);
}

fn print_warn(message: impl std::fmt::Display) {
    println!("  {} {}", style("!", "1;33"), message);
}

fn print_error(message: impl std::fmt::Display) {
    eprintln!("  {} {}", style("✗", "1;31"), message);
}

/// Runs a synchronous closure while animating a braille spinner on stderr.
/// Cache-friendly: if colour is disabled or the task is near-instant it still
/// prints a clean single-line "label… done" result. Spinner frames are cleared
/// before the final print.
fn with_spinner<F, R>(label: &str, f: F) -> R
where
    F: FnOnce() -> R,
{
    if !supports_color() {
        eprintln!("  · {}", label);
        return f();
    }

    let label = label.to_string();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let label_clone = label.clone();
    let handle = std::thread::spawn(move || {
        let mut i = 0usize;
        while !stop_clone.load(Ordering::Relaxed) {
            let frames = if cfg!(windows) && std::env::var_os("WT_SESSION").is_none() {
                &SPINNER_FRAMES_ASCII[..]
            } else {
                &SPINNER_FRAMES[..]
            };
            let frame = frames[i % frames.len()];
            eprint!(
                "\r  {}  {}",
                style_256(frame, ACCENT, true),
                style(&label_clone, "2")
            );
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(80));
            i += 1;
        }
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
    });

    let result = f();
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();
    result
}

fn colorize_timing_ms(value_ms: f64) -> String {
    // Glow intensity (A+B blend): a faster path burns brighter — sub-ms is cream
    // (hottest), then it cools through gold as the work gets heavier, and only a
    // genuinely slow path drops to a warm red. Speed reads as light.
    // Thresholds span both sub-ms renders/dispatch AND multi-hundred-ms builds,
    // so a normal build glows gold, not alarm-red — only a genuinely slow path
    // (>2s) cools to warm red.
    let color = if value_ms <= 1.0 {
        230 // cream — hottest (a sub-ms render / action)
    } else if value_ms <= 50.0 {
        222 // bright gold
    } else if value_ms <= 500.0 {
        ACCENT // gold — a snappy build
    } else if value_ms <= 2000.0 {
        ACCENT_DEEP // deep gold — a heavier build
    } else {
        167 // warm red — genuinely slow
    };
    style_256(&format!("{value_ms:.2}ms"), color, true)
}

fn gradient_text(value: &str, palette: &[u8], bold: bool) -> String {
    if !supports_color() || value.is_empty() || palette.is_empty() {
        return value.to_string();
    }
    let chars = value.chars().collect::<Vec<_>>();
    let mut out = String::new();
    let max_idx = chars.len().saturating_sub(1).max(1);
    for (idx, ch) in chars.iter().enumerate() {
        let palette_idx = (idx * (palette.len() - 1)) / max_idx;
        out.push_str(&style_256(
            ch.to_string().as_str(),
            palette[palette_idx],
            bold,
        ));
    }
    out
}

fn style_256(value: &str, color: u8, bold: bool) -> String {
    if !supports_color() {
        return value.to_string();
    }
    if bold {
        format!("\u{1b}[1;38;5;{color}m{value}\u{1b}[0m")
    } else {
        format!("\u{1b}[38;5;{color}m{value}\u{1b}[0m")
    }
}

fn style(value: &str, code: &str) -> String {
    if !supports_color() {
        return value.to_string();
    }
    format!("\u{1b}[{code}m{value}\u{1b}[0m")
}

fn supports_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wraps adversarial bytes the way `read_http_request_head` wraps a socket:
    /// a `Take`-capped `BufReader`, so the test exercises the exact bound path.
    fn parse_head(bytes: &[u8]) -> std::io::Result<(String, HashMap<String, String>, Vec<u8>)> {
        let mut reader =
            BufReader::new(std::io::Cursor::new(bytes.to_vec()).take(MAX_REQUEST_HEAD_BYTES));
        parse_http_request_head(&mut reader)
    }

    #[test]
    fn parse_http_head_reads_a_well_formed_request() {
        let (line, headers, leftover) =
            parse_head(b"GET /x HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n")
                .expect("well-formed head parses");
        assert!(line.starts_with("GET /x"));
        assert_eq!(headers.get("host").map(String::as_str), Some("localhost"));
        assert_eq!(headers.get("accept").map(String::as_str), Some("*/*"));
        assert!(leftover.is_empty());
    }

    #[test]
    fn parse_http_head_rejects_an_overlong_header_line() {
        // A single header line far past the per-line cap (no newline) must be
        // rejected, not buffered without bound.
        let mut input = b"GET / HTTP/1.1\r\nX-Flood: ".to_vec();
        input.extend(std::iter::repeat(b'A').take(MAX_REQUEST_LINE_BYTES + 1024));
        let err = parse_head(&input).expect_err("overlong header line must error");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn parse_http_head_rejects_too_many_headers() {
        let mut input = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..(MAX_REQUEST_HEADER_COUNT + 10) {
            input.extend(format!("X-H{i}: v\r\n").into_bytes());
        }
        input.extend_from_slice(b"\r\n");
        let err = parse_head(&input).expect_err("header flood must error");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn parse_http_head_is_bounded_against_a_newline_less_flood() {
        // No CRLF anywhere and more bytes than the total-head cap: the `Take`
        // bound must make this terminate (and the per-line cap reject it),
        // never grow without limit.
        let input = vec![b'A'; (MAX_REQUEST_HEAD_BYTES as usize) * 2];
        let err = parse_head(&input).expect_err("newline-less flood must error");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn test_parse_init_args_requires_target() {
        let err = parse_init_args(&[]).unwrap_err();
        assert!(err.contains("missing project name"));
    }

    #[test]
    fn test_parse_init_args_with_force() {
        let args = vec!["my-app".to_string(), "--force".to_string()];
        let options = parse_init_args(&args).unwrap();
        assert_eq!(options.target_dir, PathBuf::from("my-app"));
        assert!(options.force);
    }

    #[test]
    fn test_sanitize_package_name() {
        assert_eq!(
            sanitize_package_name("My Awesome_App").as_deref(),
            Some("my-awesome-app")
        );
        assert_eq!(sanitize_package_name("..."), None);
    }

    #[test]
    fn test_scaffold_project_writes_contract_config() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("starter");
        let options = InitOptions {
            target_dir: PathBuf::from("starter"),
            force: false,
        };
        scaffold_project(&target, &options).unwrap();

        // Phase P · Stream F.2 — scaffold now lays out file-based
        // routes under src/routes/, components under src/components/,
        // and includes tier-budget.toml at project root.
        assert!(target.join(DEV_CONFIG_TS).is_file());
        assert!(target.join("src/routes/layout.tsx").is_file());
        assert!(target.join("src/routes/index.tsx").is_file());
        assert!(target.join("src/routes/chat.tsx").is_file());
        assert!(target.join("src/components/Hero.tsx").is_file());
        assert!(target.join("src/components/Counter.tsx").is_file());
        assert!(target.join("src/styles.css").is_file());
        assert!(target.join("src/albedo-env.d.ts").is_file());
        // Phase P · post-P — public/ exists but is empty by default;
        // the scaffold no longer ships a placeholder index.html.
        assert!(target.join("public").is_dir());
        assert!(!target.join("public/index.html").exists());
        assert!(target.join("package.json").is_file());
        assert!(target.join("tsconfig.json").is_file());
        assert!(target.join("README.md").is_file());
        assert!(target.join(".gitignore").is_file());
        assert!(target.join("tier-budget.toml").is_file());

        // Phase P · F.2 — old shape should NOT exist; pins the
        // upgrade direction so a regression that re-adds src/App.tsx
        // gets caught.
        assert!(!target.join("src/App.tsx").exists());
        assert!(!target.join("src/components/LiveFeed.tsx").exists());
    }

    #[test]
    fn test_parse_ship_target_supports_named_targets() {
        assert_eq!(parse_ship_target("docker").unwrap(), ShipTarget::Docker);
        // vercel still parses so the dispatcher can return a specific
        // "Vercel doesn't run Rust binaries" message; see
        // test_configure_ship_vercel_rejects_with_explanation.
        assert_eq!(parse_ship_target("vercel").unwrap(), ShipTarget::Vercel);
        assert_eq!(parse_ship_target("fly").unwrap(), ShipTarget::Fly);
        assert_eq!(parse_ship_target("static").unwrap(), ShipTarget::Static);
    }

    #[test]
    fn test_docker_template_is_multi_stage_with_runtime_env() {
        let dockerfile = build_docker_template();
        assert!(dockerfile.contains("FROM rust:1-bookworm AS builder"));
        assert!(dockerfile.contains("FROM debian:bookworm-slim AS runtime"));
        assert!(dockerfile.contains("ALBEDO_SERVER_HOST=0.0.0.0"));
        assert!(dockerfile.contains("ALBEDO_SERVER_PORT=3000"));
        assert!(dockerfile.contains("HEALTHCHECK"));
        assert!(dockerfile.contains("EXPOSE 3000"));
        assert!(dockerfile.contains("COPY --from=builder /workspace/.albedo/dist"));
    }

    #[test]
    fn test_dockerignore_template_excludes_common_build_noise() {
        let ignore = build_dockerignore_template();
        assert!(ignore.contains(".git"));
        assert!(ignore.contains("node_modules"));
        assert!(ignore.contains("target/debug"));
    }

    #[test]
    fn test_fly_toml_template_uses_supplied_app_name() {
        let toml = build_fly_toml_template("demo-app");
        assert!(toml.contains("app = \"demo-app\""));
        assert!(toml.contains("dockerfile = \"Dockerfile\""));
        assert!(toml.contains("internal_port = 3000"));
        assert!(toml.contains("[[http_service.checks]]"));
    }

    #[test]
    fn test_configure_ship_vercel_rejects_with_explanation() {
        let temp = tempfile::tempdir().unwrap();
        let contract = ResolvedDevContract {
            contract_version: 1,
            project_dir: temp.path().to_path_buf(),
            config_path: None,
            root: temp.path().to_path_buf(),
            entry: "App.tsx".to_string(),
            server: dom_render_compiler::dev_contract::DevServerConfig::default(),
            watch: dom_render_compiler::dev_contract::DevWatchConfig::default(),
            hmr: dom_render_compiler::dev_contract::DevHmrConfig::default(),
            hot_set: Vec::new(),
            static_slice: dom_render_compiler::dev_contract::StaticSliceConfig::default(),
            strict: false,
            verbose: false,
            open: false,
            routes: HashMap::new(),
            route_layouts: HashMap::new(),
        };
        let err = configure_ship_vercel(&contract).unwrap_err();
        assert!(err.contains("vercel is not a supported"));
        assert!(err.contains("docker"));
        assert!(!temp.path().join("vercel.json").exists());
    }

    #[test]
    fn test_sanitize_static_relative_path_rejects_parent_segments() {
        assert!(sanitize_static_relative_path("../secret.txt").is_none());
        assert!(sanitize_static_relative_path("safe/file.txt").is_some());
    }

    #[test]
    fn copy_public_dir_recursively_copies_files_and_skips_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("public");
        let dst = temp.path().join("dist").join("public");
        std::fs::create_dir_all(src.join("images")).unwrap();
        std::fs::write(src.join("logo.svg"), b"<svg/>").unwrap();
        std::fs::write(src.join("images").join("cover.png"), b"PNG").unwrap();

        let count = copy_public_dir(&src, &dst).unwrap();
        assert_eq!(count, 2);
        assert!(dst.join("logo.svg").is_file());
        assert!(dst.join("images").join("cover.png").is_file());
        // Idempotent: a second run is a no-op overwrite, not a panic.
        let again = copy_public_dir(&src, &dst).unwrap();
        assert_eq!(again, 2);
    }

    #[test]
    fn copy_public_dir_returns_zero_for_empty_source() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("public");
        std::fs::create_dir_all(&src).unwrap();
        let count = copy_public_dir(&src, &temp.path().join("dist")).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_infer_project_dir_from_root_prefers_parent_of_src_components() {
        let root = PathBuf::from("C:/work/demo/src/components");
        let inferred = infer_project_dir_from_root(&root).unwrap();
        assert_eq!(inferred, PathBuf::from("C:/work/demo"));
    }

}
