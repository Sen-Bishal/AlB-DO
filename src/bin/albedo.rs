use dom_render_compiler::budget::{
    compute_bundle_byte_report, evaluate_budget, evaluate_bundle_budget, format_report_pretty,
    load_budget_from_dir, BudgetReport, TierBudget,
};
use dom_render_compiler::bundler::emit::BundleEmitReport;
use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::dev_contract::{
    parse_dev_cli_args, resolve_dev_contract, HotSetPriority, HotSetRegistration,
    ResolvedDevContract, DEV_CONFIG_JSON, DEV_CONFIG_TS,
};
use dom_render_compiler::manifest::schema::RenderManifestV2;
use dom_render_compiler::parser::ParsedComponent;
use dom_render_compiler::runtime::eval::{ComponentProject, PatchReport};
use dom_render_compiler::runtime::hot_set::{
    HotSetRegistry, RenderPriority, SentinelRing, HOT_SET_MAX,
};
use dom_render_compiler::scanner::{ProjectScanner, ScanFailure, ScanMode};
use dom_render_compiler::types::ComponentId;
use notify::{
    Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

#[path = "albedo/printer.rs"]
mod printer;

#[path = "albedo/first_run.rs"]
mod first_run;

#[path = "albedo/inspector.rs"]
mod inspector;

const PORT_AUTO_INCREMENT_LIMIT: u16 = 10;

const ACCENT: u8 = 81;
const ACCENT_SOFT: u8 = 117;
const ACCENT_DEEP: u8 = 45;
const MUTED: u8 = 244;

const BRAND_PALETTE: [u8; 5] = [45, 51, 87, 123, 159];
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

#[derive(Clone)]
struct DevAllRoutesArtifact {
    /// URL path → full HTML document
    route_documents: std::collections::HashMap<String, String>,
    render_ms: f64,
    total_ms: f64,
}

#[derive(Clone)]
struct SharedDevState {
    /// route path (e.g. "/", "/analytics") → rendered HTML document
    project: ComponentProject,
    /// Phase P · Stream D.1 — Phase K facade around `project`,
    /// rebuilt on every patch so the dev render path can hook-compile
    /// `useState` + dispatch JSX `on*` handlers. Same `CompiledProject`
    /// instance drives render (via [`render_entry_with_broadcast`])
    /// AND action dispatch (via [`CompiledProject::invoke_action_with_broadcast`])
    /// so slot ids + proxy ids align.
    compiled: Arc<dom_render_compiler::runtime::CompiledProject>,
    /// Phase P · Stream D.1 — single shared slot store for the dev
    /// process. Surviving a re-render is what makes useState values
    /// persist across HMR swaps: the slot store is the same `Arc`
    /// before and after the rebuild, so the next render reads back
    /// every value the previous action handlers wrote.
    slot_store: Arc<dom_render_compiler::runtime::SlotStore>,
    /// Phase P · Stream D.1 — broadcast registry shared between dev
    /// render and dev action dispatch. Same role as the production
    /// server's per-server registry; in dev there's only one process
    /// so all `useSharedSlot` topics land here.
    broadcast: Arc<dom_render_compiler::runtime::BroadcastRegistry>,
    /// Phase P · Stream D.1 — fixed session id for the dev process.
    /// One id per `albedo dev` invocation means every request +
    /// every action dispatch hits the same slot-store partition, so
    /// state continuity is automatic without cookie plumbing in dev.
    session_id: dom_render_compiler::runtime::SessionId,
    project_css: String,
    routes: std::collections::HashMap<String, String>,
    /// Phase P · post-P — resolved contract held alongside the cached
    /// route docs so `handle_dev_connection` can re-render a single
    /// route on demand. Without this, the cached `routes` snapshot is
    /// the only doc shape the dev server ever serves; a broadcast
    /// write in between renders never lands in the inline opcode
    /// frame the next GET ships back. `Option<Arc<...>>` so the
    /// in-process action dispatcher tests can build a state without
    /// a full contract resolution.
    contract: Option<Arc<ResolvedDevContract>>,
    render_ms: f64,
    total_ms: f64,
    last_error: Option<String>,
}

#[derive(Debug, Default)]
struct PendingRebuild {
    changed: Vec<PathBuf>,
    deleted: Vec<PathBuf>,
    force_rebuild: bool,
    css_touched: bool,
}

impl PendingRebuild {
    fn merge(&mut self, mut other: PendingRebuild) {
        self.force_rebuild |= other.force_rebuild;
        self.css_touched |= other.css_touched;
        for path in other.changed.drain(..) {
            self.add_changed(path);
        }
        for path in other.deleted.drain(..) {
            self.add_deleted(path);
        }
    }

    fn add_changed(&mut self, path: PathBuf) {
        if self.changed.contains(&path) {
            return;
        }
        self.deleted.retain(|existing| existing != &path);
        self.changed.push(path);
    }

    fn add_deleted(&mut self, path: PathBuf) {
        if self.deleted.contains(&path) {
            return;
        }
        self.changed.retain(|existing| existing != &path);
        self.deleted.push(path);
    }

    fn should_rebuild(&self) -> bool {
        self.force_rebuild || !self.changed.is_empty() || !self.deleted.is_empty()
    }
}

fn map_hot_priority(p: HotSetPriority) -> RenderPriority {
    match p {
        HotSetPriority::Low => RenderPriority::Low,
        HotSetPriority::Normal => RenderPriority::Normal,
        HotSetPriority::High => RenderPriority::High,
        HotSetPriority::Critical => RenderPriority::Critical,
    }
}

fn build_hot_set(
    project: &ComponentProject,
    registrations: &[HotSetRegistration],
) -> (HotSetRegistry, SentinelRing) {
    let registry = HotSetRegistry::new();
    resolve_hot_set_registrations(project, registrations, &registry);
    let ring = SentinelRing::from_registry(&registry)
        .unwrap_or_else(|_| SentinelRing::new(&[]).expect("empty ring always succeeds"));
    (registry, ring)
}

fn resolve_hot_set_registrations(
    project: &ComponentProject,
    registrations: &[HotSetRegistration],
    registry: &HotSetRegistry,
) -> usize {
    let mut inserted = 0usize;
    for reg in registrations {
        if let Some(id) = project.component_id_by_name(&reg.component) {
            let newly_inserted = registry
                .register(id, map_hot_priority(reg.priority))
                .unwrap_or(false);
            if newly_inserted {
                inserted += 1;
            }
        }
    }
    inserted
}

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
    run_prod_build_with_budget(&contract, skip_budget)?;

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
    print_banner();
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
                    if let Err(err) = handle_static_connection(stream, root.as_path()) {
                        if !is_benign_network_error(&err) {
                            eprintln!("  {} request failed: {err}", style("✗", "1;31"));
                        }
                    }
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

    print_banner();
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
        run_prod_build_with_budget(&contract, skip_budget)?;
        return Ok(());
    }

    // Dev mode never gates on budget; the flag is silently accepted.
    let _ = skip_budget;
    run_live_dev_runtime(contract)
}

fn run_live_dev_runtime(contract: ResolvedDevContract) -> Result<(), String> {
    let scanned_components = if contract.strict || contract.verbose {
        Some(scan_components_with_contract_policy(
            &contract,
            "starting dev runtime",
        )?)
    } else {
        None
    };

    let (manifest, tier_report, project, compiled, slot_store, broadcast, session_id, project_css, initial) =
        with_spinner("compiling components…", || {
            use dom_render_compiler::runtime::{
                BroadcastRegistry, CompiledProject, SessionId, SlotStore,
            };
            let (manifest, tier_report) =
                compile_manifest_and_tier_report(&contract, scanned_components.as_deref())?;
            let project = ComponentProject::load_from_dir(&contract.root)
                .map_err(|err| format!("failed to load components: {err}"))?;
            // Phase P · Stream D.1 — Phase K facade + per-process
            // slot store + broadcast registry. Shared across every
            // render and every action dispatch in this dev process
            // so the substrate behaves identically to production.
            let compiled = Arc::new(
                CompiledProject::wrap(project.clone())
                    .map_err(|err| format!("failed to build Phase K project: {err}"))?,
            );
            let slot_store = Arc::new(SlotStore::new());
            let broadcast = Arc::new(BroadcastRegistry::new());
            // Pre-register every `useSharedSlot` topic the project
            // references so a `broadcast(topic, ...)` write from an
            // action dispatch finds a live `BroadcastTopic` to
            // attach to. Same shape as
            // `AlbedoServerBuilder::register_compiled_project` does
            // in production (Stream C.3).
            for topic in compiled.shared_slot_topics() {
                broadcast.topic(topic, b"null".to_vec());
            }
            let session_id = SessionId::random();
            let project_css = collect_css_bundle(&contract.root);
            let initial = render_all_routes(
                compiled.as_ref(),
                &slot_store,
                &broadcast,
                session_id,
                &contract,
                &project_css,
            )
            .map_err(|err| {
                format!(
                    "failed to render initial dev document (entry='{}'): {err}",
                    contract.entry
                )
            })?;
            Ok::<_, String>((
                manifest,
                tier_report,
                project,
                compiled,
                slot_store,
                broadcast,
                session_id,
                project_css,
                initial,
            ))
        })?;

    // Build the dev inspector state from the manifest, then install both
    // observer hooks so every `render_local` and every runtime `frame_tick`
    // emits live data to `/__albedo`. Failure to install (only happens if
    // someone else won the OnceLock race in this process) is non-fatal.
    let inspector_state = Arc::new(inspector::InspectorState::default());
    inspector_state.set_graph(inspector::GraphSnapshot::from_manifest(&manifest));
    inspector_state.set_tier_map(inspector::tier_map_from_manifest(&manifest));
    let publisher = Arc::new(inspector::InspectorPublisher::new(Arc::clone(&inspector_state)));
    if let Err(_existing) = dom_render_compiler::runtime::render_observer::install_render_observer(
        publisher.clone(),
    ) {
        // Another observer beat us to it; not an error in dev — keep going.
    }
    if let Err(_existing) =
        dom_render_compiler::runtime::render_observer::install_lane_observer(publisher.clone())
    {
        // Same as above — second installer is a no-op.
    }

    let root_label = contract.root.display().to_string();
    printer::print_tier_report(&tier_report, root_label.as_str());

    let (listener, addr, auto_incremented) =
        bind_dev_listener(contract.server.host.as_str(), contract.server.port)?;

    let (hot_registry, hot_ring) = build_hot_set(&project, &contract.hot_set);
    let hot_registry = Arc::new(hot_registry);
    let sentinel_ring = Arc::new(Mutex::new(hot_ring));

    print_ok(format!(
        "compiled {} route{} in {}",
        initial.route_documents.len(),
        if initial.route_documents.len() == 1 { "" } else { "s" },
        colorize_timing_ms(initial.render_ms)
    ));
    let shared_state = Arc::new(Mutex::new(SharedDevState {
        project,
        compiled,
        slot_store,
        broadcast,
        session_id,
        project_css,
        routes: initial.route_documents,
        contract: Some(Arc::new(contract.clone())),
        render_ms: initial.render_ms,
        total_ms: initial.total_ms,
        last_error: None,
    }));
    let sse_clients = Arc::new(Mutex::new(Vec::<TcpStream>::new()));
    let revision = Arc::new(AtomicU64::new(0));

    {
        let watcher_contract = contract.clone();
        let watcher_state = Arc::clone(&shared_state);
        let watcher_clients = Arc::clone(&sse_clients);
        let watcher_revision = Arc::clone(&revision);
        let watcher_registry = Arc::clone(&hot_registry);
        let watcher_ring = Arc::clone(&sentinel_ring);
        std::thread::spawn(move || {
            watch_and_rebuild_loop(
                watcher_contract,
                watcher_state,
                watcher_clients,
                watcher_revision,
                watcher_registry,
                watcher_ring,
            );
        });
    }

    if auto_incremented {
        print_warn(format!(
            "port {} busy — using {}",
            contract.server.port,
            addr.port()
        ));
    }
    println!();
    print_ok(format!(
        "ready · {}",
        style_256(&format!("http://{}", addr), ACCENT_SOFT, true)
    ));
    let route_count = 1 + contract.routes.len();
    println!(
        "    {} {} route{}{}",
        style_256("·", MUTED, false),
        route_count,
        if route_count == 1 { "" } else { "s" },
        if contract.hmr.enabled {
            style("  · hmr on", "2").to_string()
        } else {
            style("  · hmr off", "2").to_string()
        }
    );
    println!(
        "    {} inspector · {}",
        style_256("·", MUTED, false),
        style_256(&format!("http://{}/__albedo", addr), ACCENT_SOFT, true)
    );
    if contract.verbose {
        if let Some(components) = scanned_components.as_ref() {
            print_kv("components", components.len());
        }
        print_kv("hot set", format!("{}/{}", hot_registry.len(), HOT_SET_MAX));
        print_kv(
            "watcher",
            format!(
                "'{}' (debounce={}ms)",
                contract.root.display(),
                contract.watch.debounce_ms
            ),
        );
        for (url, entry) in &contract.routes {
            println!(
                "    {} {} {}",
                style_256("·", MUTED, false),
                style(url, "2"),
                style(&format!("→ {}", entry), "2")
            );
        }
    }
    println!();
    println!(
        "    {}  stop the server",
        style_256("ctrl+c", MUTED, true)
    );
    println!();

    if contract.open {
        let target = format!("http://{}", addr);
        if let Err(err) = try_open_browser(target.as_str()) {
            print_warn(format!("failed to open browser automatically: {err}"));
        }
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = Arc::clone(&shared_state);
                let clients = Arc::clone(&sse_clients);
                let inspector = Arc::clone(&inspector_state);
                let hmr_enabled = contract.hmr.enabled;
                std::thread::spawn(move || {
                    if let Err(err) =
                        handle_dev_connection(stream, state, clients, inspector, hmr_enabled)
                    {
                        if !is_benign_network_error(&err) {
                            eprintln!("  {} request failed: {err}", style("✗", "1;31"));
                        }
                    }
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

/// Computes the v2 manifest *and* tier report in one pass. Used by the dev
/// runtime to seed the inspector graph and tier map without redoing the
/// component scan.
fn compile_manifest_and_tier_report(
    contract: &ResolvedDevContract,
    scanned_components: Option<&[ParsedComponent]>,
) -> Result<
    (
        dom_render_compiler::manifest::schema::RenderManifestV2,
        dom_render_compiler::types::TierReport,
    ),
    String,
> {
    let components = if let Some(components) = scanned_components {
        components.to_vec()
    } else {
        scan_components_with_contract_policy(contract, "analyzing component tiers")?
    };

    let scanner = ProjectScanner::new();
    let compiler = scanner.build_compiler(components);
    compiler
        .optimize_manifest_v2_with_tier_report()
        .map_err(|err| format!("failed to compute tier report: {err}"))
}

fn watch_and_rebuild_loop(
    contract: ResolvedDevContract,
    shared_state: Arc<Mutex<SharedDevState>>,
    sse_clients: Arc<Mutex<Vec<TcpStream>>>,
    revision: Arc<AtomicU64>,
    hot_registry: Arc<HotSetRegistry>,
    sentinel_ring: Arc<Mutex<SentinelRing>>,
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

    let debounce = Duration::from_millis(contract.watch.debounce_ms);
    loop {
        let first = match event_rx.recv() {
            Ok(event) => event,
            Err(_) => break,
        };

        let mut pending = accumulate_rebuild_paths(&first, &contract.watch.ignore);
        loop {
            match event_rx.recv_timeout(debounce) {
                Ok(next) => {
                    pending.merge(accumulate_rebuild_paths(&next, &contract.watch.ignore));
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }

        if !pending.should_rebuild() {
            continue;
        }

        let rebuild_start = Instant::now();
        if contract.strict || contract.verbose {
            if let Err(err) =
                scan_components_with_contract_policy(&contract, "rebuilding after file changes")
            {
                let overlay = build_dev_error_overlay(
                    format!("Build failed during component scan:\n{}", err).as_str(),
                    contract.hmr.enabled,
                );
                if let Ok(mut state) = shared_state.lock() {
                    let overlay_map = std::collections::HashMap::from([("/".to_string(), overlay)]);
                    state.routes = overlay_map;
                    state.last_error = Some(err.clone());
                }
                let next_revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
                if contract.hmr.enabled {
                    broadcast_reload_event(&sse_clients, next_revision);
                }
                eprintln!("  {} rebuild failed: {}", style("✗", "1;31"), err);
                continue;
            }
        }

        match rebuild_with_pending(&contract, &shared_state, &pending) {
            Ok((patch_report, rendered)) => {
                if rendered {
                    let next_revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
                    if contract.hmr.enabled {
                        let mut fallback_full_reload = pending.force_rebuild || pending.css_touched;
                        let mut invalidated_components = Vec::<ComponentId>::new();

                        if !fallback_full_reload {
                            match shared_state.lock() {
                                Ok(state) => {
                                    let inserted = resolve_hot_set_registrations(
                                        &state.project,
                                        &contract.hot_set,
                                        hot_registry.as_ref(),
                                    );
                                    if inserted > 0 {
                                        match sentinel_ring.lock() {
                                            Ok(mut ring) => {
                                                if let Err(err) = ring
                                                    .rebuild_from_registry(hot_registry.as_ref())
                                                {
                                                    fallback_full_reload = true;
                                                    eprintln!(
                                                        "  {} hot ring rebuild failed: {}",
                                                        style("!", "1;33"),
                                                        err
                                                    );
                                                }
                                            }
                                            Err(_) => {
                                                fallback_full_reload = true;
                                                eprintln!(
                                                    "  {} hot ring lock poisoned during rebuild",
                                                    style("!", "1;33"),
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    fallback_full_reload = true;
                                    eprintln!(
                                        "  {} shared state lock poisoned while resolving hot set",
                                        style("!", "1;33"),
                                    );
                                }
                            }
                        }

                        if !fallback_full_reload {
                            match collect_hot_set_invalidations(
                                &patch_report,
                                hot_registry.as_ref(),
                                &sentinel_ring,
                            ) {
                                Ok((drained_ids, has_non_hot_component_changes)) => {
                                    if has_non_hot_component_changes {
                                        fallback_full_reload = true;
                                    } else {
                                        invalidated_components = drained_ids;
                                    }
                                }
                                Err(err) => {
                                    fallback_full_reload = true;
                                    eprintln!(
                                        "  {} hot invalidation pass failed: {}",
                                        style("!", "1;33"),
                                        err
                                    );
                                }
                            }
                        }

                        if fallback_full_reload || invalidated_components.is_empty() {
                            broadcast_reload_event(&sse_clients, next_revision);
                        } else {
                            for component_id in invalidated_components {
                                broadcast_component_invalidation_event(
                                    &sse_clients,
                                    next_revision,
                                    component_id,
                                );
                            }
                        }
                    }
                    let rebuild_ms = rebuild_start.elapsed().as_secs_f64() * 1000.0;
                    let _ = patch_report.skipped_unchanged;
                    let _ = patch_report.deleted;
                    println!(
                        "  {}  rebuilt {} component{} in {}",
                        style("✓", "1;32"),
                        patch_report.reparsed,
                        if patch_report.reparsed == 1 { "" } else { "s" },
                        colorize_timing_ms(rebuild_ms),
                    );
                } else if contract.verbose {
                    let noop_ms = rebuild_start.elapsed().as_secs_f64() * 1000.0;
                    println!(
                        "  {}  no-op in {}",
                        style_256("·", MUTED, false),
                        colorize_timing_ms(noop_ms),
                    );
                }
            }
            Err(err) => {
                let overlay = build_dev_error_overlay(
                    format!("Build failed during incremental rebuild:\n{}", err).as_str(),
                    contract.hmr.enabled,
                );
                if let Ok(mut state) = shared_state.lock() {
                    let overlay_map = std::collections::HashMap::from([("/".to_string(), overlay)]);
                    state.routes = overlay_map;
                    state.last_error = Some(err.clone());
                }
                let next_revision = revision.fetch_add(1, Ordering::SeqCst) + 1;
                if contract.hmr.enabled {
                    broadcast_reload_event(&sse_clients, next_revision);
                }
                eprintln!("  {} rebuild failed: {}", style("✗", "1;31"), err);
            }
        }
    }
}

fn collect_hot_set_invalidations(
    patch_report: &PatchReport,
    hot_registry: &HotSetRegistry,
    sentinel_ring: &Arc<Mutex<SentinelRing>>,
) -> Result<(Vec<ComponentId>, bool), String> {
    let mut changed_component_ids = patch_report.reparsed_ids.clone();
    changed_component_ids.extend(patch_report.deleted_ids.iter().copied());

    if changed_component_ids
        .iter()
        .any(|component_id| !hot_registry.contains(*component_id))
    {
        return Ok((Vec::new(), true));
    }

    let ring = sentinel_ring
        .lock()
        .map_err(|_| "sentinel ring lock poisoned".to_string())?;

    for component_id in &changed_component_ids {
        ring.mark_dirty(*component_id);
    }

    let mut invalidated_components = Vec::new();
    ring.drain(|component_id| invalidated_components.push(component_id));
    Ok((invalidated_components, false))
}

fn accumulate_rebuild_paths(
    event: &notify::Result<Event>,
    ignore_patterns: &[String],
) -> PendingRebuild {
    let mut pending = PendingRebuild::default();

    let Ok(event) = event else {
        return pending;
    };
    let relevant_kind = matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) | EventKind::Any
    );
    if !relevant_kind {
        return pending;
    }
    let is_remove = matches!(event.kind, EventKind::Remove(_));

    if event.paths.is_empty() {
        pending.force_rebuild = true;
        return pending;
    }

    for path in &event.paths {
        if should_ignore_path(path, ignore_patterns) {
            continue;
        }

        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == DEV_CONFIG_JSON || name == DEV_CONFIG_TS)
            .unwrap_or(false)
        {
            pending.force_rebuild = true;
            continue;
        }

        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase());
        match extension.as_deref() {
            Some("tsx") | Some("ts") | Some("jsx") | Some("js") => {
                if is_remove {
                    pending.add_deleted(path.clone());
                } else {
                    pending.add_changed(path.clone());
                }
            }
            Some("css") => {
                pending.css_touched = true;
                pending.force_rebuild = true;
            }
            Some("json") | Some("html") => {
                pending.force_rebuild = true;
            }
            _ => {}
        }
    }

    pending
}

fn rebuild_with_pending(
    contract: &ResolvedDevContract,
    shared_state: &Arc<Mutex<SharedDevState>>,
    pending: &PendingRebuild,
) -> Result<(PatchReport, bool), String> {
    use dom_render_compiler::runtime::CompiledProject;

    let (patch_report, project_snapshot, css_snapshot_before_refresh, slot_store, broadcast, session_id) = {
        let mut state = shared_state
            .lock()
            .map_err(|_| "shared state lock poisoned".to_string())?;

        let patch_report = state
            .project
            .patch(&pending.changed, &pending.deleted)
            .map_err(|err| format!("failed to patch components: {err}"))?;

        if !pending.force_rebuild
            && !pending.css_touched
            && patch_report.reparsed == 0
            && patch_report.deleted == 0
        {
            state.last_error = None;
            return Ok((patch_report, false));
        }

        (
            patch_report,
            state.project.clone(),
            state.project_css.clone(),
            state.slot_store.clone(),
            state.broadcast.clone(),
            state.session_id,
        )
    };

    let css_snapshot = if pending.css_touched {
        collect_css_bundle(&contract.root)
    } else {
        css_snapshot_before_refresh
    };

    // Phase P · Stream D.1 — rebuild the Phase K facade against the
    // freshly-patched project. The CompiledProject doesn't implement
    // Clone (handler bodies hold SWC AST), so we re-wrap here rather
    // than patching in place. CompiledProject::wrap is a metadata
    // pass over already-parsed ParsedModules — cheap. The slot store
    // + broadcast registry survive unchanged so useState values
    // persist across the swap.
    let compiled = Arc::new(
        CompiledProject::wrap(project_snapshot)
            .map_err(|err| format!("failed to rebuild Phase K project: {err}"))?,
    );
    // Re-register any new shared-slot topics the patched project
    // declares. `topic()` is idempotent on the name key so existing
    // topics (and their accumulated values) are preserved.
    for topic in compiled.shared_slot_topics() {
        broadcast.topic(topic, b"null".to_vec());
    }
    let artifact = render_all_routes(
        compiled.as_ref(),
        &slot_store,
        &broadcast,
        session_id,
        contract,
        &css_snapshot,
    )?;

    let mut state = shared_state
        .lock()
        .map_err(|_| "shared state lock poisoned".to_string())?;
    state.compiled = compiled;
    if pending.css_touched {
        state.project_css = css_snapshot;
    }
    state.routes = artifact.route_documents;
    state.render_ms = artifact.render_ms;
    state.total_ms = artifact.total_ms;
    state.last_error = None;

    Ok((patch_report, true))
}

fn should_ignore_path(path: &Path, ignore_patterns: &[String]) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.contains("/.git/") || normalized.contains("/node_modules/") {
        return true;
    }

    for pattern in ignore_patterns {
        let mut token = pattern.replace('\\', "/");
        token = token.replace("**/", "");
        token = token.replace("/**", "");
        token = token.replace('*', "");
        let token = token.trim_matches('/');
        if !token.is_empty() && normalized.contains(token) {
            return true;
        }
    }

    false
}

fn handle_dev_connection(
    mut stream: TcpStream,
    shared_state: Arc<Mutex<SharedDevState>>,
    sse_clients: Arc<Mutex<Vec<TcpStream>>>,
    inspector_state: Arc<inspector::InspectorState>,
    hmr_enabled: bool,
) -> std::io::Result<()> {
    let socket_start = Instant::now();
    let (first_line, request_headers, body_prefetch) = read_http_request_head(&stream)?;
    let socket_wait_ms = socket_start.elapsed().as_secs_f64() * 1000.0;
    let request_start = Instant::now();
    let client = stream
        .peer_addr()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let method = first_line.split_whitespace().next().unwrap_or("GET");
    let raw_target = first_line.split_whitespace().nth(1).unwrap_or("/");
    let path = normalize_request_path(raw_target);
    let transport = determine_dev_transport(path.as_str(), &request_headers, hmr_enabled);
    let transport_label = format_dev_transport_label(transport);
    let transport_header_value = transport.active.to_string();

    // Inspector dispatch — sits ahead of the route ladder so /__albedo never
    // falls through to the route renderer. Only GET reaches here; the
    // method-not-allowed case below still applies for other verbs.
    if method == "GET" && inspector::matches_path(path.as_str()) {
        return match inspector::dispatch(&inspector_state, path.as_str(), &mut stream)? {
            inspector::Dispatch::Handled | inspector::Dispatch::StreamOwned => Ok(()),
        };
    }

    let (status, build_render_ms, build_total_ms, route_like) = if method == "POST"
        && path == "/_albedo/action"
    {
        // Phase P · Stream D.3 — dev-side action dispatch. Mirrors
        // `crates/albedo-server/src/handlers/action.rs` but inline
        // because the dev path runs on std::net rather than axum.
        // The same `CompiledProject` that rendered the page handles
        // the dispatch, and writes hit the same `slot_store` so the
        // next render (HMR or otherwise) sees the updated value.
        let body =
            read_http_request_body(&mut stream, &request_headers, body_prefetch.as_slice())?;
        let (status, payload, content_type) =
            run_dev_action(body.as_slice(), &shared_state);
        let headers = [("x-albedo-transport", transport_header_value.clone())];
        write_http_response(
            &mut stream,
            status,
            if status == 200 { "OK" } else { "Error" },
            content_type,
            payload.as_slice(),
            &headers,
        )?;
        (status, 0.0, 0.0, false)
    } else if method != "GET" {
        let headers = [("x-albedo-transport", transport_header_value.clone())];
        write_http_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method not allowed\n",
            &headers,
        )?;
        (405, 0.0, 0.0, false)
    } else if path == "/_albedo/health" {
        let headers = [("x-albedo-transport", transport_header_value.clone())];
        write_http_response(
            &mut stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            b"ok\n",
            &headers,
        )?;
        (200, 0.0, 0.0, false)
    } else if path == "/_albedo/hmr" && hmr_enabled {
        write_sse_handshake(&mut stream)?;
        if let Ok(mut clients) = sse_clients.lock() {
            clients.push(stream);
        }
        return Ok(());
    } else if let Some((body, content_type)) = dev_static_asset(path.as_str()) {
        // Phase P · Stream D.4 — serve the bakabox client assets
        // (runtime.js / bincode.js / link-forms.js / hydration.js /
        // wt-bootstrap.js) from the in-binary `include_str!`
        // templates. The dev server doesn't run `albedo build`, so
        // the `.albedo/dist/_albedo/` mirror that production
        // serves from doesn't exist here — we hand the bytes back
        // directly. `cache-control: no-store` so a TSX edit picks
        // up a fresh runtime if the build pipeline ever varies it.
        let headers = [
            ("x-albedo-transport", transport_header_value.clone()),
            ("cache-control", "no-store".to_string()),
        ];
        write_http_response(
            &mut stream,
            200,
            "OK",
            content_type,
            body.as_bytes(),
            &headers,
        )?;
        (200, 0.0, 0.0, false)
    } else if path == "/" || path == "/index.html" || is_route_like_path(path.as_str()) {
        // Phase P · post-P — re-render this route on every GET. The
        // dev server used to serve `state.routes` verbatim, but that
        // cached snapshot was rendered once at startup; a
        // `broadcast(topic, ...)` write between renders never landed
        // in the inline opcode frame the next GET shipped back, so
        // `/chat` always painted "null" no matter how many times the
        // bump button had fired. Per-request render reads the topic
        // store fresh, costs <1ms per route, and falls back to the
        // cached doc when the on-demand render fails for any reason.
        let (doc, render_ms, total_ms, error) = {
            let state = shared_state.lock().expect("shared state lock poisoned");
            let lookup = if path == "/index.html" {
                "/".to_string()
            } else {
                path.clone()
            };
            let fallback = state
                .routes
                .get(&lookup)
                .or_else(|| state.routes.get("/"))
                .cloned()
                .unwrap_or_default();
            let rendered = render_single_dev_route(&state, lookup.as_str()).ok();
            let doc = rendered.map(|(html, _ms)| html).unwrap_or(fallback);
            (
                doc,
                state.render_ms,
                state.total_ms,
                state.last_error.clone(),
            )
        };
        let header_request_ms = request_start.elapsed().as_secs_f64() * 1000.0;
        let mut headers = vec![
            ("x-albedo-socket-wait-ms", format!("{:.2}", socket_wait_ms)),
            ("x-albedo-request-ms", format!("{:.2}", header_request_ms)),
            ("x-albedo-build-render-ms", format!("{:.2}", render_ms)),
            ("x-albedo-build-total-ms", format!("{:.2}", total_ms)),
            ("x-albedo-render-ms", format!("{:.2}", render_ms)),
            ("x-albedo-total-ms", format!("{:.2}", total_ms)),
            ("cache-control", "no-store".to_string()),
            ("x-albedo-transport", transport_header_value.clone()),
        ];
        if error.is_some() {
            headers.push(("x-albedo-dev-state", "error".to_string()));
        } else {
            headers.push(("x-albedo-dev-state", "ok".to_string()));
        }
        write_http_response(
            &mut stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            doc.as_bytes(),
            headers.as_slice(),
        )?;
        (200, render_ms, total_ms, true)
    } else {
        let headers = [("x-albedo-transport", transport_header_value.clone())];
        write_http_response(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"Not found\n",
            &headers,
        )?;
        (404, 0.0, 0.0, false)
    };
    let request_ms = request_start.elapsed().as_secs_f64() * 1000.0;
    let _ = (socket_wait_ms, build_render_ms, build_total_ms, transport_label, client);

    if route_like {
        let status_styled = if status < 400 {
            style_256(&status.to_string(), ACCENT_SOFT, true)
        } else {
            style(&status.to_string(), "1;31")
        };
        println!(
            "  {}  {}  {}  {}  {}",
            style_256("→", ACCENT, true),
            style(method, "1"),
            path,
            status_styled,
            colorize_timing_ms(request_ms)
        );
    }
    Ok(())
}

/// Phase P · Stream D.3 — read the request body for a POST. The dev
/// HTTP server reads the head via a `BufReader` whose internal buffer
/// often slurps part (or all) of the body in the same syscall; that
/// prefetch arrives here as `body_prefetch` and is consumed before we
/// touch the raw socket. Without it `read_exact` blocks forever waiting
/// for bytes the OS already delivered, and bakabox's POST /_albedo/action
/// silently hangs — which is the user-visible "counter stuck at zero".
///
/// We honour `content-length`, bail if it's missing or unparseable, and
/// cap at 2 MiB (same ceiling `MAX_REQUEST_BODY_BYTES` enforces in the
/// production action route).
fn read_http_request_body(
    stream: &mut TcpStream,
    request_headers: &HashMap<String, String>,
    body_prefetch: &[u8],
) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    const MAX_BODY: usize = 2 * 1024 * 1024;
    let length: usize = request_headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if length == 0 {
        return Ok(Vec::new());
    }
    if length > MAX_BODY {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "content-length exceeds 2 MiB cap",
        ));
    }
    let mut buf = Vec::with_capacity(length);
    // Drain whatever the head parser prefetched first. For tiny POSTs
    // (bakabox envelopes are typically 7-32 bytes) this is the entire
    // body; for larger requests the remainder lands via `read_exact`.
    let take = body_prefetch.len().min(length);
    buf.extend_from_slice(&body_prefetch[..take]);
    if buf.len() < length {
        let rest = length - buf.len();
        let mut tail = vec![0u8; rest];
        stream.read_exact(&mut tail)?;
        buf.extend_from_slice(&tail);
    }
    Ok(buf)
}

/// Phase P · Stream D.3 — dispatch a bincode `ActionEnvelope` against
/// the dev process's shared `CompiledProject`. Returns
/// `(status_code, body_bytes, content_type)` so the caller can write
/// the HTTP response without further branching.
///
/// Wire-aligned with `crates/albedo-server/src/handlers/action.rs`:
///   - Malformed envelope → 400 with a short text reason.
///   - Unknown action_id → 404 with text reason.
///   - Handler error → 500 with the underlying message.
///   - Success → 200 with the bincode-encoded `OpcodeFrame`.
///
/// CSRF validation is skipped in dev mode — there's no cookie session
/// in the dev server path. Production routes through the
/// `CompiledProjectActionAdapter` for full CSRF enforcement.
fn run_dev_action(
    body: &[u8],
    shared_state: &Arc<Mutex<SharedDevState>>,
) -> (u16, Vec<u8>, &'static str) {
    use dom_render_compiler::ir::action::decode_action_envelope;
    use dom_render_compiler::ir::opcode::OpcodeFrame;
    use dom_render_compiler::ir::wire::encode_frame;
    use dom_render_compiler::runtime::SessionSlotView;

    let envelope = match decode_action_envelope(body) {
        Ok((envelope, _consumed)) => envelope,
        Err(err) => {
            return (
                400,
                format!("invalid action envelope: {err}").into_bytes(),
                "text/plain; charset=utf-8",
            );
        }
    };

    let (compiled, slot_store, broadcast, session_id) = match shared_state.lock() {
        Ok(state) => (
            state.compiled.clone(),
            state.slot_store.clone(),
            state.broadcast.clone(),
            state.session_id,
        ),
        Err(_) => {
            return (
                500,
                b"dev shared state lock poisoned".to_vec(),
                "text/plain; charset=utf-8",
            );
        }
    };

    if compiled.handler(envelope.action_id).is_none() {
        return (
            404,
            format!("no handler registered for action_id {}", envelope.action_id).into_bytes(),
            "text/plain; charset=utf-8",
        );
    }

    let slots = SessionSlotView::new(session_id, slot_store);
    let instructions =
        match compiled.invoke_action_with_broadcast(&envelope, &slots, broadcast.as_ref()) {
            Ok(instructions) => instructions,
            Err(err) => {
                return (
                    500,
                    format!("dev action handler failed: {err:#}").into_bytes(),
                    "text/plain; charset=utf-8",
                );
            }
        };

    let frame = OpcodeFrame {
        frame_id: 0,
        component_id: None,
        instructions,
    };
    match encode_frame(&frame) {
        Ok(bytes) => (200, bytes, "application/octet-stream"),
        Err(err) => (
            500,
            format!("failed to encode opcode frame: {err}").into_bytes(),
            "text/plain; charset=utf-8",
        ),
    }
}

fn write_sse_handshake(stream: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-store, must-revalidate\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\nx-albedo-transport: sse\r\n\r\n";
    stream.write_all(headers.as_bytes())?;
    stream.write_all(b"data: connected\n\n")?;
    stream.flush()
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
fn read_http_request_head(
    stream: &TcpStream,
) -> std::io::Result<(String, HashMap<String, String>, Vec<u8>)> {
    let mut first_line = String::new();
    let mut headers = HashMap::new();
    let mut reader = BufReader::new(stream.try_clone()?);
    reader.read_line(&mut first_line)?;

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
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

fn broadcast_sse_payload(clients: &Arc<Mutex<Vec<TcpStream>>>, payload: &str) {
    let mut active = match clients.lock() {
        Ok(guard) => guard,
        Err(_) => return,
    };
    let mut retained = Vec::with_capacity(active.len());
    for mut stream in active.drain(..) {
        if stream
            .write_all(payload.as_bytes())
            .and_then(|_| stream.flush())
            .is_ok()
        {
            retained.push(stream);
        }
    }
    *active = retained;
}

fn broadcast_reload_event(clients: &Arc<Mutex<Vec<TcpStream>>>, revision: u64) {
    let payload = format!("data: reload:{revision}\n\n");
    broadcast_sse_payload(clients, payload.as_str());
}

fn broadcast_component_invalidation_event(
    clients: &Arc<Mutex<Vec<TcpStream>>>,
    revision: u64,
    component_id: ComponentId,
) {
    let payload = format!("data: invalidate:{revision}:{}\n\n", component_id.as_u64());
    broadcast_sse_payload(clients, payload.as_str());
}

/// Phase P · post-P — render one route on demand using the dev
/// process's shared `CompiledProject`/`SlotStore`/`BroadcastRegistry`.
/// Mirrors the inline `render_entry` closure in [`render_all_routes`]
/// — same shell template, same `render_entry_with_broadcast` call,
/// same `dev_bakabox_script_tags` injection — so a per-GET render
/// produces bytes indistinguishable from the cached snapshot **except**
/// that the inline opcode frame reflects the **current** broadcast
/// topic value. Returns `Err` when the contract or matching route
/// entry isn't known (caller falls back to the cached doc).
fn render_single_dev_route(
    state: &SharedDevState,
    url_path: &str,
) -> Result<(String, f64), String> {
    use dom_render_compiler::runtime::{
        render_entry_with_broadcast, RenderOptions, SessionSlotView,
    };

    let contract = state
        .contract
        .as_ref()
        .ok_or_else(|| "dev contract not installed".to_string())?;

    let entry = if url_path == "/" {
        contract.entry.clone()
    } else {
        let trimmed = url_path.trim_start_matches('/');
        contract
            .routes
            .iter()
            .find(|(url, _)| url.trim_start_matches('/') == trimmed)
            .map(|(_, entry)| entry.clone())
            .ok_or_else(|| format!("no route entry for path '{url_path}'"))?
    };

    let render_start = Instant::now();
    let props = serde_json::json!({});
    let slots = SessionSlotView::new(state.session_id, state.slot_store.clone());
    let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
    let opts = RenderOptions { hook_compile: true };
    let output = render_entry_with_broadcast(
        state.compiled.as_ref(),
        entry.as_str(),
        &props,
        &slots,
        state.broadcast.as_ref(),
        tx,
        &opts,
    )
    .map_err(|err| err.to_string())?;
    let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;

    let opcode_script = render_inline_opcode_script(&output.opcodes);
    let base_css = dev_shell_base_css();
    let project_css = state.project_css.as_str();
    let document = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\" />\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  <title>ALBEDO Dev</title>\n  <style>\n{base_css}\n{project_css}\n  </style>\n  {bakabox_scripts}\n</head>\n<body>\n{rendered_html}\n{opcode_script}\n</body>\n</html>\n",
        rendered_html = output.html,
        bakabox_scripts = dev_bakabox_script_tags(),
    );
    let html = inject_hmr_client_script(&document, contract.hmr.enabled);
    Ok((html, render_ms))
}

fn render_all_routes(
    compiled: &dom_render_compiler::runtime::CompiledProject,
    slot_store: &Arc<dom_render_compiler::runtime::SlotStore>,
    broadcast: &Arc<dom_render_compiler::runtime::BroadcastRegistry>,
    session_id: dom_render_compiler::runtime::SessionId,
    contract: &ResolvedDevContract,
    project_css: &str,
) -> Result<DevAllRoutesArtifact, String> {
    use dom_render_compiler::runtime::{
        render_entry_with_broadcast, RenderOptions, SessionSlotView,
    };

    let total_start = Instant::now();
    let base_css = dev_shell_base_css();

    let mut route_documents = std::collections::HashMap::new();
    let mut total_render_ms = 0.0_f64;
    let props = serde_json::json!({});

    let render_entry = |entry: &str| -> Result<(String, f64), String> {
        let render_start = Instant::now();
        // Phase P · Stream D.1 — Phase K render path. Fresh
        // SessionSlotView wrapping the SAME `slot_store` Arc that
        // persists across re-renders, so useState values written by
        // earlier action dispatches surface in this render. Dummy
        // mpsc channel matches the build-time pattern Stream B uses
        // — the receiver is dropped, broadcast `try_send` is
        // non-blocking, so a topic write during render is a clean
        // no-op rather than a hang.
        let slots = SessionSlotView::new(session_id, slot_store.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let opts = RenderOptions { hook_compile: true };
        let output = render_entry_with_broadcast(
            compiled, entry, &props, &slots, broadcast, tx, &opts,
        )
        .map_err(|err| err.to_string())?;
        let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;

        // Phase P · post-P wire-through — inline the Phase K opcode
        // frame as a `<script type="application/x-albedo-frame">`
        // so bakabox's bootstrap can apply BindEvent / SetTextRef /
        // initial SlotSet instructions even without a WT patches
        // lane (which dev mode doesn't have). Without this, clicks
        // on `useState`-driven buttons silently no-op because no
        // event listener gets attached.
        let opcode_script = render_inline_opcode_script(&output.opcodes);

        let document = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\" />\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  <title>ALBEDO Dev</title>\n  <style>\n{base_css}\n{project_css}\n  </style>\n  {bakabox_scripts}\n</head>\n<body>\n{rendered_html}\n{opcode_script}\n</body>\n</html>\n",
            rendered_html = output.html,
            bakabox_scripts = dev_bakabox_script_tags(),
        );
        let html = inject_hmr_client_script(&document, contract.hmr.enabled);
        Ok((html, render_ms))
    };

    match render_entry(contract.entry.as_str()) {
        Ok((html, ms)) => {
            total_render_ms += ms;
            route_documents.insert("/".to_string(), html);
        }
        Err(err) => {
            let overlay =
                build_dev_error_overlay(&format!("Route '/' failed:\n{err}"), contract.hmr.enabled);
            route_documents.insert("/".to_string(), overlay);
        }
    }

    for (url_path, entry) in &contract.routes {
        let url = if url_path.starts_with('/') {
            url_path.clone()
        } else {
            format!("/{url_path}")
        };
        match render_entry(entry.as_str()) {
            Ok((html, ms)) => {
                total_render_ms += ms;
                route_documents.insert(url, html);
            }
            Err(err) => {
                let overlay = build_dev_error_overlay(
                    &format!("Route '{url}' failed (entry='{entry}'):\n{err}"),
                    contract.hmr.enabled,
                );
                route_documents.insert(url, overlay);
            }
        }
    }

    Ok(DevAllRoutesArtifact {
        route_documents,
        render_ms: total_render_ms,
        total_ms: total_start.elapsed().as_secs_f64() * 1000.0,
    })
}

fn collect_css_bundle(root: &Path) -> String {
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

fn dev_shell_base_css() -> &'static str {
    r#"
:root {
  --bg-0: #04050b;
  --bg-1: #0a0f1f;
  --bg-2: #1b1330;
  --ink: #f3f4f6;
  --muted: #aeb4c7;
  --line: #f3f4f6;
}

* {
  box-sizing: border-box;
}

html,
body {
  margin: 0;
  min-height: 100%;
}

body {
  background:
    radial-gradient(circle at 12% 15%, rgba(56, 189, 248, 0.18), transparent 35%),
    radial-gradient(circle at 90% 10%, rgba(236, 72, 153, 0.2), transparent 34%),
    linear-gradient(140deg, var(--bg-0), var(--bg-1) 46%, var(--bg-2));
  color: var(--ink);
}
"#
}

/// Phase P · post-P wire-through — encode the Phase K opcode frame
/// as a bootstrap `<script>` tag the bakabox runtime auto-applies.
/// Empty string when the renderer emitted no opcodes (Tier-A-only
/// route) so we don't ship a useless empty frame. Wraps the bincode
/// bytes in a fresh `OpcodeFrame` envelope so the wire shape matches
/// what bakabox's `applyFrameBytes` expects.
fn render_inline_opcode_script(opcodes: &[dom_render_compiler::ir::opcode::Instruction]) -> String {
    use dom_render_compiler::ir::opcode::OpcodeFrame;
    use dom_render_compiler::ir::wire::encode_frame;
    if opcodes.is_empty() {
        return String::new();
    }
    let frame = OpcodeFrame {
        frame_id: 0,
        component_id: None,
        instructions: opcodes.to_vec(),
    };
    let bytes = match encode_frame(&frame) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    let b64 = base64_encode(&bytes);
    format!(
        "<script type=\"application/x-albedo-frame\" data-base64=\"{b64}\"></script>"
    )
}

/// Phase P · post-P wire-through — tiny base64 encoder. Std lacks
/// one and the dependency footprint isn't worth pulling in a crate
/// for ~80 lines of cross-platform byte-shuffling. Standard alphabet
/// + `=` padding per RFC 4648; no line wrapping (we embed in HTML
/// attribute values).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        1 => {
            let n = (rem[0] as u32) << 16;
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
            out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Phase P · Stream D.2 — bakabox client script tags injected into
/// the dev page head. Same shape as the production
/// `default_shim_script` minus the WT bootstrap (the dev server
/// doesn't carry a QUIC listener, so the WT path 404s; SSE/HMR
/// suffices for dev). `runtime.js` is the entry; it imports
/// `./bincode.js` and `./link-forms.js` is loaded explicitly so the
/// Phase L `<Link>` / form-action interception fires.
fn dev_bakabox_script_tags() -> &'static str {
    "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>\
     <script type=\"module\" src=\"/_albedo/link-forms.js\"></script>"
}

/// Phase P · Stream D.4 — resolve a URL path to one of the in-binary
/// bakabox client assets. Mirrors what `albedo build` writes to
/// `.albedo/dist/_albedo/` in production, but serves from the
/// `include_str!`-baked templates so dev iteration doesn't require a
/// rebuild. Returns `(body, content_type)` for the matching asset, or
/// `None` for anything else.
fn dev_static_asset(path: &str) -> Option<(String, &'static str)> {
    match path {
        "/_albedo/runtime.js" => Some((albedo_runtime_shim_template(), "text/javascript; charset=utf-8")),
        "/_albedo/bincode.js" => Some((albedo_bincode_template(), "text/javascript; charset=utf-8")),
        "/_albedo/link-forms.js" => Some((albedo_link_forms_template(), "text/javascript; charset=utf-8")),
        "/_albedo/hydration.js" => Some((albedo_hydration_runtime_template(), "text/javascript; charset=utf-8")),
        "/_albedo/wt-bootstrap.js" => Some((albedo_wt_bootstrap_template(), "text/javascript; charset=utf-8")),
        _ => None,
    }
}

fn inject_hmr_client_script(html_document: &str, hmr_enabled: bool) -> String {
    if !hmr_enabled {
        return html_document.to_string();
    }

    // Phase M.2 · in-place HTML swap instead of full reload. Server-
    // side slot store survives the swap (cookie unchanged), and a
    // best-effort draft-input capture keeps the operator's typed
    // text alive across the rebuild cycle. Falls back to a hard
    // reload when the fetch or parse fails.
    let script = r#"<script>
(function () {
  var connect = function () {
    try {
      var es = new EventSource('/_albedo/hmr');
      es.onmessage = function (event) {
        if (typeof event.data !== 'string') {
          return;
        }
        if (event.data.indexOf('reload') === 0) {
          applyInPlaceSwap();
          return;
        }
        if (event.data.indexOf('invalidate:') === 0) {
          var parts = event.data.slice('invalidate:'.length).split(':');
          try {
            window.dispatchEvent(new CustomEvent('albedo:component-invalidated', {
              detail: { revision: parts[0] || '', component_id: parts[1] || '' }
            }));
          } catch (_eventErr) {}
          applyInPlaceSwap();
        }
      };
      es.onerror = function () {
        try { es.close(); } catch (_e) {}
        setTimeout(connect, 800);
      };
    } catch (_err) {
      setTimeout(connect, 1000);
    }
  };

  // Refetch the current URL and swap document.body's innerHTML in
  // place. Slot state lives on the server keyed by the
  // albedo-session cookie, so the round-trip preserves it. Draft
  // input values (text/textarea with a `name`) are best-effort
  // restored across the swap.
  var applyInPlaceSwap = function () {
    var draft = captureDraft();
    var scroll = { x: window.scrollX, y: window.scrollY };
    fetch(window.location.href, { credentials: 'same-origin', cache: 'no-store' })
      .then(function (response) {
        if (!response.ok) {
          window.location.reload();
          return null;
        }
        return response.text();
      })
      .then(function (html) {
        if (html == null) return;
        var doc;
        try {
          doc = new DOMParser().parseFromString(html, 'text/html');
        } catch (_err) {
          window.location.reload();
          return;
        }
        if (!doc || !doc.body) {
          window.location.reload();
          return;
        }
        document.body.innerHTML = doc.body.innerHTML;
        restoreDraft(draft);
        try { window.scrollTo(scroll.x, scroll.y); } catch (_e) {}
        try {
          window.dispatchEvent(new CustomEvent('albedo:hmr-applied', {
            detail: { route: window.location.pathname }
          }));
        } catch (_e) {}
      })
      .catch(function () {
        window.location.reload();
      });
  };

  var captureDraft = function () {
    var out = {};
    var fields = document.querySelectorAll('input[name], textarea[name]');
    for (var i = 0; i < fields.length; i++) {
      var el = fields[i];
      var name = el.getAttribute('name');
      if (!name) continue;
      var type = (el.type || '').toLowerCase();
      if (type === 'password') continue;
      if (type === 'checkbox' || type === 'radio') {
        out[name + ':' + (el.value || '') + ':checked'] = !!el.checked;
      } else {
        out[name] = el.value;
      }
    }
    return out;
  };

  var restoreDraft = function (draft) {
    if (!draft) return;
    var fields = document.querySelectorAll('input[name], textarea[name]');
    for (var i = 0; i < fields.length; i++) {
      var el = fields[i];
      var name = el.getAttribute('name');
      if (!name) continue;
      var type = (el.type || '').toLowerCase();
      if (type === 'password') continue;
      if (type === 'checkbox' || type === 'radio') {
        var key = name + ':' + (el.value || '') + ':checked';
        if (key in draft) el.checked = !!draft[key];
      } else if (name in draft) {
        el.value = draft[name];
      }
    }
  };

  connect();
})();
</script>"#;

    if html_document.contains("</body>") {
        html_document.replacen("</body>", &format!("{script}\n</body>"), 1)
    } else {
        format!("{html_document}\n{script}")
    }
}

fn build_dev_error_overlay(message: &str, hmr_enabled: bool) -> String {
    let escaped = escape_html(message);
    let reconnect = if hmr_enabled {
        "<script>(function(){var c=function(){try{var es=new EventSource('/_albedo/hmr');es.onmessage=function(e){if(typeof e.data!=='string'){return;}if(e.data.indexOf('reload')===0){window.location.reload();return;}if(e.data.indexOf('invalidate:')===0){var p=e.data.slice('invalidate:'.length).split(':');try{window.dispatchEvent(new CustomEvent('albedo:component-invalidated',{detail:{revision:p[0]||'',component_id:p[1]||''}}));}catch(_eventErr){}}};es.onerror=function(){try{es.close();}catch(_e){}setTimeout(c,800);};}catch(_e){setTimeout(c,1000);}};c();})();</script>"
    } else {
        ""
    };

    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"/><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/><title>ALBEDO Dev Error</title><style>body{{margin:0;background:#09090b;color:#f4f4f5;font-family:\"Segoe UI\",sans-serif}}main{{max-width:900px;margin:4rem auto;padding:2rem;border:1px solid #3f3f46;border-radius:16px;background:#18181b}}h1{{margin:0 0 1rem;color:#fb7185}}pre{{white-space:pre-wrap;background:#111827;color:#e5e7eb;padding:1rem;border-radius:12px;border:1px solid #374151}}</style></head><body><main><h1>ALBEDO Dev Build Error</h1><p>Fix the error and save a file to trigger a rebuild.</p><pre>{escaped}</pre></main>{reconnect}</body></html>"
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DevTransportDecision {
    active: &'static str,
    fallback_reason: Option<&'static str>,
}

fn determine_dev_transport(
    path: &str,
    headers: &HashMap<String, String>,
    hmr_enabled: bool,
) -> DevTransportDecision {
    if path == "/_albedo/hmr" && hmr_enabled {
        return DevTransportDecision {
            active: "sse",
            fallback_reason: None,
        };
    }

    if request_wants_webtransport(headers) {
        return DevTransportDecision {
            active: "sse",
            fallback_reason: Some("dev_http1_sse_fallback"),
        };
    }

    DevTransportDecision {
        active: "sse",
        fallback_reason: None,
    }
}

fn request_wants_webtransport(headers: &HashMap<String, String>) -> bool {
    header_has_token(headers, "upgrade", "webtransport")
        || headers
            .keys()
            .any(|name| name.starts_with("sec-webtransport-http3-draft"))
}

fn header_has_token(headers: &HashMap<String, String>, name: &str, token: &str) -> bool {
    let Some(value) = headers.get(name) else {
        return false;
    };
    value
        .split(',')
        .map(str::trim)
        .any(|entry| entry.eq_ignore_ascii_case(token))
}

fn format_dev_transport_label(decision: DevTransportDecision) -> String {
    match decision.fallback_reason {
        Some(reason) => format!("{} (fallback={})", decision.active, reason),
        None => decision.active.to_string(),
    }
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

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Phase O.1 · convenience wrapper preserving the old call sites'
/// signature; the gate work happens in
/// [`run_prod_build_with_budget`].
fn run_prod_build(contract: &ResolvedDevContract) -> Result<(), String> {
    run_prod_build_with_budget(contract, false)
}

fn run_prod_build_with_budget(
    contract: &ResolvedDevContract,
    skip_budget: bool,
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

    print_section("build");
    print_kv("components", components.len());
    print_kv(
        "scan",
        colorize_timing_ms(scan_start.elapsed().as_secs_f64() * 1000.0),
    );

    let compile_start = Instant::now();
    let out_dir_for_closure = out_dir.clone();
    let (manifest, report, missing_sources) = with_spinner("compiling production bundle…", move || {
        let scanner = ProjectScanner::new();
        let compiler = scanner.build_compiler(components);
        let manifest = compiler
            .optimize_manifest_v2()
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
        Ok::<_, String>((manifest, report, missing_sources))
    })?;

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

    print_ok(format!(
        "built in {}",
        colorize_timing_ms(compile_start.elapsed().as_secs_f64() * 1000.0)
    ));
    print_kv("output", out_dir.display());
    print_kv("artifacts", report.artifacts.len() + 5);
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
            style(
                &format!("+{} more", report.artifacts.len() - 6),
                "2"
            )
        );
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
        if copied > 0 {
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
    print_section("next steps");
    println!(
        "    {}  cd {}",
        style_256("1", ACCENT, true),
        style(project_name, "1")
    );
    println!(
        "    {}  albedo dev",
        style_256("2", ACCENT, true)
    );
    println!();
    println!(
        "  {}",
        style(
            "the starter has three components — one at each effect tier.",
            "2"
        )
    );
    println!(
        "  {}",
        style("run albedo dev to see how AlBDO classifies them.", "2")
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
    print_command("init", "<project>", "scaffold a new app");
    print_command("dev", "[dir]", "Phase K dev server with HMR + actions");
    print_command("build", "[dir]", "compile manifest + bundle (tier-budget gated)");
    print_command("ship", "[dir]", "build + configure deploy target");
    print_command(
        "serve",
        "",
        "build then boot a real AlbedoServer (actions, broadcast, WT)",
    );
    print_command("files", "[dir]", "static file server (defaults to .albedo/dist)");
    print_command("budget", "[dir]", "standalone tier-budget gate (CI-friendly)");
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
        "    {} build the project then boot a real AlbedoServer:",
        style("·", "2")
    );
    println!(
        "        {} {}",
        style("·", "2"),
        "manifest-streaming for every route (Tier-A inline, Tier-B opcodes)"
    );
    println!(
        "        {} {}",
        style("·", "2"),
        "POST /_albedo/action dispatch into the CompiledProject"
    );
    println!(
        "        {} {}",
        style("·", "2"),
        "broadcast-topic fan-out over the WT patches lane"
    );
    println!(
        "        {} {}",
        style("·", "2"),
        "GET /_albedo/runtime.js + /_albedo/link-forms.js served from <dist>/_albedo/"
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
    println!(
        "    {} {}  {}",
        style_256(command, ACCENT_SOFT, true),
        style(&format!("{:<14}", args), "2"),
        description
    );
}

fn print_option(option: &str, description: &str) {
    println!(
        "    {:<22} {}",
        style(option, "1"),
        style(description, "2")
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
    let code = if value_ms <= 1.0 {
        "1;32"
    } else if value_ms <= 25.0 {
        "1;36"
    } else if value_ms <= 250.0 {
        "1;33"
    } else {
        "1;31"
    };
    style(&format!("{value_ms:.2}ms"), code)
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

    #[test]
    fn test_determine_dev_transport_defaults_to_sse() {
        let headers = HashMap::new();
        let decision = determine_dev_transport("/", &headers, true);
        assert_eq!(decision.active, "sse");
        assert_eq!(decision.fallback_reason, None);
    }

    #[test]
    fn test_determine_dev_transport_records_webtransport_fallback_reason() {
        let mut headers = HashMap::new();
        headers.insert("upgrade".to_string(), "webtransport".to_string());
        let decision = determine_dev_transport("/", &headers, true);
        assert_eq!(decision.active, "sse");
        assert_eq!(decision.fallback_reason, Some("dev_http1_sse_fallback"));
    }

    #[test]
    fn test_determine_dev_transport_hmr_path_is_sse_without_fallback_reason() {
        let mut headers = HashMap::new();
        headers.insert("upgrade".to_string(), "webtransport".to_string());
        let decision = determine_dev_transport("/_albedo/hmr", &headers, true);
        assert_eq!(decision.active, "sse");
        assert_eq!(decision.fallback_reason, None);
    }

    // ── Phase P · Stream D tests ────────────────────────────────────

    /// Stream D.2 — the bakabox script tags injected into the dev
    /// page head must reference the three client assets the dev
    /// HTTP handler serves: runtime.js (the entry, which itself
    /// imports bincode.js as a relative module), and link-forms.js
    /// (Phase L Link/form/Navigate interception). Without these the
    /// browser receives Phase K opcode-stamped HTML but no client
    /// to apply patches against.
    #[test]
    fn dev_bakabox_script_tags_include_runtime_and_link_forms() {
        let tags = dev_bakabox_script_tags();
        assert!(
            tags.contains("/_albedo/runtime.js"),
            "expected runtime.js module reference; got: {tags}"
        );
        assert!(
            tags.contains("/_albedo/link-forms.js"),
            "expected link-forms.js module reference; got: {tags}"
        );
        assert!(
            tags.contains("type=\"module\""),
            "bakabox scripts must be ES modules so relative imports resolve"
        );
    }

    /// Stream D.4 — the dev HTTP handler must resolve the bakabox
    /// asset URLs to in-binary `include_str!` templates without
    /// requiring `albedo build` to have run.
    #[test]
    fn dev_static_asset_serves_runtime_bincode_link_forms() {
        let (runtime, runtime_ct) = dev_static_asset("/_albedo/runtime.js").unwrap();
        assert!(!runtime.is_empty(), "runtime.js asset must have a body");
        assert_eq!(runtime_ct, "text/javascript; charset=utf-8");

        let (bincode, _ct) = dev_static_asset("/_albedo/bincode.js").unwrap();
        assert!(!bincode.is_empty(), "bincode.js asset must have a body");

        let (link_forms, _ct) = dev_static_asset("/_albedo/link-forms.js").unwrap();
        assert!(
            !link_forms.is_empty(),
            "link-forms.js asset must have a body"
        );

        let (hydration, _ct) = dev_static_asset("/_albedo/hydration.js").unwrap();
        assert!(!hydration.is_empty(), "hydration.js asset must have a body");
    }

    #[test]
    fn dev_static_asset_returns_none_for_unrelated_paths() {
        assert!(dev_static_asset("/").is_none());
        assert!(dev_static_asset("/_albedo/action").is_none());
        assert!(dev_static_asset("/_albedo/runtime.js.map").is_none());
        assert!(dev_static_asset("/runtime.js").is_none());
    }

    /// Stream D.3 — a malformed bincode body must surface a 400 from
    /// `run_dev_action` (not a 500 / panic). Proves the dispatcher
    /// rejects garbage before reaching the handler lookup.
    #[test]
    fn run_dev_action_rejects_malformed_envelope_with_400() {
        let state = build_dev_state_for_tests();
        let (status, body, content_type) = run_dev_action(b"not a real bincode envelope", &state);
        assert_eq!(status, 400);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert!(
            std::str::from_utf8(&body)
                .unwrap_or_default()
                .contains("invalid action envelope"),
            "expected diagnostic about envelope decode; got: {body:?}"
        );
    }

    /// Stream D.3 — a well-formed envelope whose `action_id` doesn't
    /// resolve to a registered handler must return 404. Proves the
    /// dispatcher's lookup-before-invoke fork.
    #[test]
    fn run_dev_action_returns_404_for_unknown_action_id() {
        use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
        let state = build_dev_state_for_tests();
        let envelope = ActionEnvelope {
            action_id: 0xdead_beef,
            event_kind: 0,
            payload: Vec::new(),
        };
        let bytes = encode_action_envelope(&envelope).expect("encode envelope");
        let (status, _body, _ct) = run_dev_action(bytes.as_slice(), &state);
        assert_eq!(
            status, 404,
            "unknown action_id must surface 404 from run_dev_action"
        );
    }

    /// Helper: a SharedDevState wired to a tiny in-memory project so
    /// `run_dev_action` can resolve `CompiledProject::handler`.
    fn build_dev_state_for_tests() -> Arc<Mutex<SharedDevState>> {
        use dom_render_compiler::runtime::eval::ComponentProject;
        use dom_render_compiler::runtime::{
            BroadcastRegistry, CompiledProject, SessionId, SlotStore,
        };

        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("Counter.tsx"),
            "import { useState } from \"react\";\n\
             export default function Counter() {\n\
               const [n, setN] = useState(0);\n\
               return <button onClick={() => setN(n + 1)}>{n}</button>;\n\
             }\n",
        )
        .expect("write fixture");

        let project = ComponentProject::load_from_dir(temp.path()).expect("load project");
        let compiled = Arc::new(
            CompiledProject::wrap(project.clone()).expect("wrap compiled project"),
        );
        let state = SharedDevState {
            project,
            compiled,
            slot_store: Arc::new(SlotStore::new()),
            broadcast: Arc::new(BroadcastRegistry::new()),
            session_id: SessionId::random(),
            project_css: String::new(),
            routes: std::collections::HashMap::new(),
            // Tests don't drive route re-rendering, so the contract
            // is unset — the dispatch path under test only consults
            // compiled/slot_store/broadcast/session_id.
            contract: None,
            render_ms: 0.0,
            total_ms: 0.0,
            last_error: None,
        };
        // Hold the tempdir alive for the test's lifetime by leaking
        // it; the test process exits immediately after so the OS
        // reclaims the directory. Avoids threading TempDir through.
        std::mem::forget(temp);
        Arc::new(Mutex::new(state))
    }

    /// Stream D.1 — slot store + broadcast registry survive a state
    /// clone (the watch loop pattern). Pins the contract that HMR
    /// preserves slot values: cloning SharedDevState doesn't drop
    /// the shared Arcs, so a re-render reads back the same store.
    #[test]
    fn shared_dev_state_clone_preserves_slot_store_arc() {
        let original = build_dev_state_for_tests();
        let (slot_arc, broadcast_arc, session) = {
            let s = original.lock().unwrap();
            (s.slot_store.clone(), s.broadcast.clone(), s.session_id)
        };
        let cloned = {
            let s = original.lock().unwrap();
            s.clone()
        };
        assert!(
            Arc::ptr_eq(&slot_arc, &cloned.slot_store),
            "cloning SharedDevState must reuse the same SlotStore Arc"
        );
        assert!(
            Arc::ptr_eq(&broadcast_arc, &cloned.broadcast),
            "cloning SharedDevState must reuse the same BroadcastRegistry Arc"
        );
        assert_eq!(session, cloned.session_id);
    }
}
