use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::dev_contract::{
    parse_dev_cli_args, resolve_dev_contract, HotSetPriority, HotSetRegistration,
    ResolvedDevContract, DEV_CONFIG_JSON, DEV_CONFIG_TS,
};
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

const PORT_AUTO_INCREMENT_LIMIT: u16 = 10;

const ACCENT: u8 = 81;
const ACCENT_SOFT: u8 = 117;
const ACCENT_DEEP: u8 = 45;
const MUTED: u8 = 244;

const BRAND_PALETTE: [u8; 5] = [45, 51, 87, 123, 159];
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_FRAMES_ASCII: [&str; 4] = ["|", "/", "-", "\\"];

const SCAFFOLD_APP: &str = include_str!("../../scaffold/src/App.tsx");
const SCAFFOLD_HERO: &str = include_str!("../../scaffold/src/Hero.tsx");
const SCAFFOLD_COUNTER: &str = include_str!("../../scaffold/src/Counter.tsx");
const SCAFFOLD_LIVE_FEED: &str = include_str!("../../scaffold/src/LiveFeed.tsx");
const SCAFFOLD_ENV_DTS: &str = include_str!("../../scaffold/src/albedo-env.d.ts");
const SCAFFOLD_STYLES: &str = include_str!("../../scaffold/src/styles.css");
const SCAFFOLD_CONFIG: &str = include_str!("../../scaffold/albedo.config.ts");
const SCAFFOLD_PACKAGE_JSON: &str = include_str!("../../scaffold/package.json");
const SCAFFOLD_INDEX_HTML: &str = include_str!("../../scaffold/index.html");
const SCAFFOLD_TSCONFIG: &str = include_str!("../../scaffold/tsconfig.json");
const SCAFFOLD_README: &str = include_str!("../../scaffold/README.md");
const SCAFFOLD_GITIGNORE: &str = include_str!("../../scaffold/.gitignore");

#[derive(Clone)]
struct DevAllRoutesArtifact {
    /// URL path → full HTML document
    route_documents: std::collections::HashMap<String, String>,
    render_ms: f64,
    total_ms: f64,
}

#[derive(Debug, Clone)]
struct SharedDevState {
    /// route path (e.g. "/", "/analytics") → rendered HTML document
    project: ComponentProject,
    project_css: String,
    routes: std::collections::HashMap<String, String>,
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

fn run_ship_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_ship_help();
        return Ok(());
    }

    let options = parse_ship_args(raw_args)?;
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let contract = resolve_dev_contract(&options.forwarded, &cwd)?;
    run_prod_build(&contract)?;

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
            other => forwarded.push(other.to_string()),
        }
        idx += 1;
    }

    Ok(ShipOptions { target, forwarded })
}

fn parse_ship_target(raw: &str) -> Result<ShipTarget, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "vercel" => Ok(ShipTarget::Vercel),
        "2" | "docker" => Ok(ShipTarget::Docker),
        "3" | "fly" | "flyio" | "fly.io" => Ok(ShipTarget::Fly),
        "4" | "static" => Ok(ShipTarget::Static),
        other => Err(format!(
            "unknown ship target '{other}'. Supported targets: vercel, docker, fly, static."
        )),
    }
}

fn prompt_ship_target() -> Result<ShipTarget, String> {
    print_section("pick a target");
    println!(
        "    {} vercel     {}",
        style_256("1", ACCENT_SOFT, true),
        style("static export + vercel.json", "2")
    );
    println!(
        "    {} docker     {}",
        style_256("2", ACCENT_SOFT, true),
        style("single binary image", "2")
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

fn configure_ship_vercel(contract: &ResolvedDevContract) -> Result<(), String> {
    let vercel_json = "{\n  \"version\": 2,\n  \"cleanUrls\": true,\n  \"trailingSlash\": false,\n  \"outputDirectory\": \".albedo/dist\"\n}\n";
    let path = contract.project_dir.join("vercel.json");
    std::fs::write(&path, vercel_json)
        .map_err(|err| format!("failed to write '{}': {err}", path.display()))?;
    print_section("vercel");
    print_ok("vercel.json written");
    print_kv("file", path.display());
    print_kv("deploy", style_256("vercel --prod", ACCENT_SOFT, true));
    Ok(())
}

fn configure_ship_docker(contract: &ResolvedDevContract) -> Result<(), String> {
    let dockerfile = "FROM debian:bookworm-slim\nCOPY ./target/release/albedo /usr/local/bin/albedo\nCOPY ./.albedo/dist /app/dist\nWORKDIR /app\nEXPOSE 3000\nCMD [\"albedo\", \"serve\", \"--dir\", \"dist\", \"--host\", \"0.0.0.0\", \"--port\", \"3000\"]\n";
    let dockerignore = ".git\nnode_modules\ntarget/debug\n";
    let dockerfile_path = contract.project_dir.join("Dockerfile");
    let dockerignore_path = contract.project_dir.join(".dockerignore");
    std::fs::write(&dockerfile_path, dockerfile)
        .map_err(|err| format!("failed to write '{}': {err}", dockerfile_path.display()))?;
    std::fs::write(&dockerignore_path, dockerignore)
        .map_err(|err| format!("failed to write '{}': {err}", dockerignore_path.display()))?;
    print_section("docker");
    print_ok("Dockerfile + .dockerignore written");
    print_kv("dockerfile", dockerfile_path.display());
    print_kv("ignore", dockerignore_path.display());
    print_kv("build", style_256("docker build -t albedo-app .", ACCENT_SOFT, true));
    print_kv("run", style_256("docker run -p 3000:3000 albedo-app", ACCENT_SOFT, true));
    Ok(())
}

fn configure_ship_fly(contract: &ResolvedDevContract) -> Result<(), String> {
    configure_ship_docker(contract)?;
    let app_name = infer_package_name(&contract.project_dir);
    let fly_toml = format!(
        "app = \"{app_name}\"\nprimary_region = \"iad\"\n\n[build]\n  dockerfile = \"Dockerfile\"\n\n[http_service]\n  internal_port = 3000\n  force_https = true\n  auto_stop_machines = \"stop\"\n  auto_start_machines = true\n  min_machines_running = 0\n"
    );
    let fly_toml_path = contract.project_dir.join("fly.toml");
    std::fs::write(&fly_toml_path, fly_toml)
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

fn run_serve_command(raw_args: &[String]) -> Result<(), String> {
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
    print_section("serve");
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
    let (first_line, _headers) = read_http_request_head(&stream)?;
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
    for arg in raw_args {
        if arg == "--prod" || arg == "--production" {
            prod_mode = true;
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
        run_prod_build(&contract)?;
        return Ok(());
    }

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

    let (tier_report, project, project_css, initial) = with_spinner("compiling components…", || {
        let tier_report = compile_tier_report(&contract, scanned_components.as_deref())?;
        let project = ComponentProject::load_from_dir(&contract.root)
            .map_err(|err| format!("failed to load components: {err}"))?;
        let project_css = collect_css_bundle(&contract.root);
        let initial = render_all_routes(&project, &contract, &project_css).map_err(|err| {
            format!(
                "failed to render initial dev document (entry='{}'): {err}",
                contract.entry
            )
        })?;
        Ok::<_, String>((tier_report, project, project_css, initial))
    })?;

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
        project_css,
        routes: initial.route_documents,
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
                let hmr_enabled = contract.hmr.enabled;
                std::thread::spawn(move || {
                    if let Err(err) = handle_dev_connection(stream, state, clients, hmr_enabled) {
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

fn compile_tier_report(
    contract: &ResolvedDevContract,
    scanned_components: Option<&[ParsedComponent]>,
) -> Result<dom_render_compiler::types::TierReport, String> {
    let components = if let Some(components) = scanned_components {
        components.to_vec()
    } else {
        scan_components_with_contract_policy(contract, "analyzing component tiers")?
    };

    let scanner = ProjectScanner::new();
    let compiler = scanner.build_compiler(components);
    let (_, tier_report) = compiler
        .optimize_manifest_v2_with_tier_report()
        .map_err(|err| format!("failed to compute tier report: {err}"))?;
    Ok(tier_report)
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
    let (patch_report, project_snapshot, css_snapshot_before_refresh) = {
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
        )
    };

    let css_snapshot = if pending.css_touched {
        collect_css_bundle(&contract.root)
    } else {
        css_snapshot_before_refresh
    };

    let artifact = render_all_routes(&project_snapshot, contract, &css_snapshot)?;

    let mut state = shared_state
        .lock()
        .map_err(|_| "shared state lock poisoned".to_string())?;
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
    hmr_enabled: bool,
) -> std::io::Result<()> {
    let socket_start = Instant::now();
    let (first_line, request_headers) = read_http_request_head(&stream)?;
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

    let (status, build_render_ms, build_total_ms, route_like) = if method != "GET" {
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
    } else if path == "/" || path == "/index.html" || is_route_like_path(path.as_str()) {
        let (doc, render_ms, total_ms, error) = {
            let state = shared_state.lock().expect("shared state lock poisoned");
            let lookup = if path == "/index.html" {
                "/".to_string()
            } else {
                path.clone()
            };
            let doc = state
                .routes
                .get(&lookup)
                .or_else(|| state.routes.get("/"))
                .cloned()
                .unwrap_or_default();
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

fn write_sse_handshake(stream: &mut TcpStream) -> std::io::Result<()> {
    let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache, no-store, must-revalidate\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\nx-albedo-transport: sse\r\n\r\n";
    stream.write_all(headers.as_bytes())?;
    stream.write_all(b"data: connected\n\n")?;
    stream.flush()
}

fn read_http_request_head(
    stream: &TcpStream,
) -> std::io::Result<(String, HashMap<String, String>)> {
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

    Ok((first_line, headers))
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

fn render_all_routes(
    project: &ComponentProject,
    contract: &ResolvedDevContract,
    project_css: &str,
) -> Result<DevAllRoutesArtifact, String> {
    let total_start = Instant::now();
    let base_css = dev_shell_base_css();

    let mut route_documents = std::collections::HashMap::new();
    let mut total_render_ms = 0.0_f64;
    let props = serde_json::json!({});

    let render_entry = |entry: &str| -> Result<(String, f64), String> {
        let render_start = Instant::now();
        let rendered_html = project
            .render_entry(entry, &props)
            .map_err(|err| err.to_string())?;
        let render_ms = render_start.elapsed().as_secs_f64() * 1000.0;
        let document = format!(
            "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\" />\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  <title>ALBEDO Dev</title>\n  <style>\n{base_css}\n{project_css}\n  </style>\n</head>\n<body>\n{rendered_html}\n</body>\n</html>\n"
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

fn inject_hmr_client_script(html_document: &str, hmr_enabled: bool) -> String {
    if !hmr_enabled {
        return html_document.to_string();
    }

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
          window.location.reload();
          return;
        }
        if (event.data.indexOf('invalidate:') === 0) {
          var parts = event.data.slice('invalidate:'.length).split(':');
          try {
            window.dispatchEvent(new CustomEvent('albedo:component-invalidated', {
              detail: { revision: parts[0] || '', component_id: parts[1] || '' }
            }));
          } catch (_eventErr) {}
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

fn run_prod_build(contract: &ResolvedDevContract) -> Result<(), String> {
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
    let hydration_asset_path = out_dir.join("_albedo").join("hydration.js");
    std::fs::write(&hydration_asset_path, albedo_hydration_runtime_template()).map_err(|err| {
        format!(
            "failed to write hydration runtime '{}': {err}",
            hydration_asset_path.display()
        )
    })?;
    let index_html_path = out_dir.join("index.html");
    std::fs::write(&index_html_path, SCAFFOLD_INDEX_HTML).map_err(|err| {
        format!(
            "failed to write index html '{}': {err}",
            index_html_path.display()
        )
    })?;

    print_ok(format!(
        "built in {}",
        colorize_timing_ms(compile_start.elapsed().as_secs_f64() * 1000.0)
    ));
    print_kv("output", out_dir.display());
    print_kv("artifacts", report.artifacts.len() + 4);
    if missing_sources > 0 {
        print_warn(format!(
            "{missing_sources} module{} had unreadable sources — skipped from static precompile",
            if missing_sources == 1 { "" } else { "s" }
        ));
    }

    let _ = (&manifest_path, &runtime_asset_path, &hydration_asset_path, &index_html_path);
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

    Ok(())
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

    std::fs::create_dir_all(target.join("src").join("components")).map_err(|err| {
        format!(
            "failed to create scaffold directory '{}': {err}",
            target.join("src/components").display()
        )
    })?;
    std::fs::create_dir_all(target.join("public")).map_err(|err| {
        format!(
            "failed to create scaffold directory '{}': {err}",
            target.join("public").display()
        )
    })?;

    let package_name = infer_package_name(target);
    let package_json = SCAFFOLD_PACKAGE_JSON.replace("__ALBEDO_APP_NAME__", package_name.as_str());

    write_scaffold_file(
        &target.join("src").join("App.tsx"),
        SCAFFOLD_APP,
        options.force,
    )?;
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
        &target.join("src").join("components").join("LiveFeed.tsx"),
        SCAFFOLD_LIVE_FEED,
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
    write_scaffold_file(
        &target.join("public").join("index.html"),
        SCAFFOLD_INDEX_HTML,
        options.force,
    )?;
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

    local commands="init dev build ship serve run completions help"

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
            COMPREPLY=( $(compgen -W "--target --config --entry" -- "$cur") )
            return 0
            ;;
        --target)
            COMPREPLY=( $(compgen -W "vercel docker fly static" -- "$cur") )
            return 0
            ;;
        dev|build|run)
            COMPREPLY=( $(compgen -W "--config --entry --host --port --no-hmr --strict --verbose --open --prod" -- "$cur") )
            return 0
            ;;
        init)
            COMPREPLY=( $(compgen -W "--force" -- "$cur") )
            return 0
            ;;
        serve)
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
        'serve:Serve static files from a directory'
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
                        '--target[Deployment target]:target:(vercel docker fly static)' \
                        $dev_flags
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
set -l albdo_commands init dev build ship serve run completions help

# Disable file completions for the main command
complete -c albdo -f

# Top-level commands
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a init        -d 'Create a tiered starter app scaffold'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a dev         -d 'Start the live dev server with HMR'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a build       -d 'Compile an optimised production build'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a ship        -d 'Build and configure deployment target files'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a serve       -d 'Serve static files from a directory'
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
complete -c albdo -n "__fish_seen_subcommand_from ship" -l target -d 'Deployment target' -r -a "vercel docker fly static"
complete -c albdo -n "__fish_seen_subcommand_from ship" -l config -d 'Use explicit albedo config file' -r

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

    $commands = @('init','dev','build','ship','serve','run','completions','help')
    $devFlags = @('--config','--entry','--host','--port','--no-hmr','--strict','--verbose','--open','--prod')

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
                @('vercel','docker','fly','static') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }
            } else {
                @('--target','--config','--entry') | Where-Object { $_ -like "$wordToComplete*" } |
                    ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
            }
        }
        'serve' {
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
    print_command("dev", "[dir]", "start the dev server");
    print_command("build", "[dir]", "compile an optimized bundle");
    print_command("ship", "[dir]", "build + configure deploy target");
    print_command("serve", "[dir]", "serve a static directory");
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
    print_example("albedo serve ./.albedo/dist");
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
        style("albedo ship [dir] [--target <name>]", "1")
    );
    print_option("--target <name>", "vercel | docker | fly | static");
    print_option("--config <FILE>", "explicit albedo config");
    print_option("--entry <FILE>", "override entry module");
    println!();
}

fn print_serve_help() {
    print_banner();
    print_section("serve");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo serve [dir] [--host <IP>] [--port <PORT>]", "1")
    );
    print_option("--dir <DIR>", "directory to serve (default: .albedo/dist)");
    print_option("--host <IP>", "bind host (default: 127.0.0.1)");
    print_option("--port <PORT>", "bind port (default: 3000)");
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

        assert!(target.join(DEV_CONFIG_TS).is_file());
        assert!(target.join("src/App.tsx").is_file());
        assert!(target.join("src/components/Hero.tsx").is_file());
        assert!(target.join("src/components/Counter.tsx").is_file());
        assert!(target.join("src/components/LiveFeed.tsx").is_file());
        assert!(target.join("src/styles.css").is_file());
        assert!(target.join("src/albedo-env.d.ts").is_file());
        assert!(target.join("public/index.html").is_file());
        assert!(target.join("package.json").is_file());
        assert!(target.join("tsconfig.json").is_file());
        assert!(target.join("README.md").is_file());
        assert!(target.join(".gitignore").is_file());
    }

    #[test]
    fn test_parse_ship_target_supports_named_targets() {
        assert_eq!(parse_ship_target("docker").unwrap(), ShipTarget::Docker);
        assert_eq!(parse_ship_target("vercel").unwrap(), ShipTarget::Vercel);
        assert_eq!(parse_ship_target("fly").unwrap(), ShipTarget::Fly);
        assert_eq!(parse_ship_target("static").unwrap(), ShipTarget::Static);
    }

    #[test]
    fn test_sanitize_static_relative_path_rejects_parent_segments() {
        assert!(sanitize_static_relative_path("../secret.txt").is_none());
        assert!(sanitize_static_relative_path("safe/file.txt").is_some());
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
}
