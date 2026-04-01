use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::dev_contract::{
    parse_dev_cli_args, resolve_dev_contract, HotSetPriority, HotSetRegistration,
    ResolvedDevContract, DEV_CONFIG_JSON, DEV_CONFIG_TS,
};
use dom_render_compiler::parser::ParsedComponent;
use dom_render_compiler::runtime::ast_eval::{ComponentProject, PatchReport};
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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const RULE_WIDTH: usize = 92;
const PORT_AUTO_INCREMENT_LIMIT: u16 = 10;
static DEV_PULSE_TICK: AtomicU64 = AtomicU64::new(0);

const BANNER_PALETTE: [u8; 5] = [45, 81, 117, 153, 189];
const RULE_PALETTE: [u8; 4] = [39, 45, 81, 117];
const LOADING_FRAMES: [&str; 4] = [".", "o", "O", "o"];
const RENDER_FRAMES: [&str; 4] = ["-", "=", "~", "="];
const LOADING_COLORS: [u8; 4] = [45, 81, 117, 81];
const RENDER_COLORS: [u8; 4] = [39, 45, 81, 117];

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
        "run" => run_command(&args[2..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        unknown => Err(format!(
            "unknown command '{unknown}'. Run `albedo help` to see available commands."
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitOptions {
    target_dir: PathBuf,
    force: bool,
    javascript: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            target_dir: PathBuf::from("."),
            force: false,
            javascript: false,
        }
    }
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

    scaffold_project(&target, &options)?;

    let entry_file = if options.javascript {
        "App.jsx"
    } else {
        "App.tsx"
    };
    let relative_target = if target == cwd {
        ".".to_string()
    } else {
        target.display().to_string()
    };

    print_banner();
    print_section("Project Initialized");
    print_kv("Location", target.display());
    print_kv("Entry", format!("src/components/{entry_file}"));
    print_kv("Config", DEV_CONFIG_JSON);
    print_kv(
        "Template",
        if options.javascript {
            "JavaScript"
        } else {
            "TypeScript"
        },
    );
    print_ok("Boilerplate web dev setup created");

    print_section("Next Steps");
    println!("  {}", style(&format!("cd {relative_target}"), "2"));
    println!("  {}", style("cargo run --bin albedo -- dev", "2"));
    println!("  {}", style("cargo run --bin albedo -- build", "2"));
    Ok(())
}

fn parse_init_args(raw_args: &[String]) -> Result<InitOptions, String> {
    let mut options = InitOptions::default();
    let mut target_set = false;
    let mut idx = 0usize;

    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--force" => {
                options.force = true;
            }
            "--js" => {
                options.javascript = true;
            }
            "--ts" => {
                options.javascript = false;
            }
            _ if !arg.starts_with('-') => {
                if target_set {
                    return Err("init accepts at most one target directory".to_string());
                }
                options.target_dir = PathBuf::from(arg);
                target_set = true;
            }
            unknown => {
                return Err(format!("unknown init option '{unknown}'"));
            }
        }
        idx += 1;
    }

    Ok(options)
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
    print_section(if prod_mode {
        "Run Dev (Production Build)"
    } else {
        "Run Dev"
    });
    print_kv("Project", contract.project_dir.display());
    print_kv("Root", contract.root.display());
    print_kv("Entry", contract.entry.as_str());
    print_kv(
        "Server",
        format!("{}:{}", contract.server.host, contract.server.port),
    );
    print_kv(
        "HMR",
        if contract.hmr.enabled {
            format!("{:?}", contract.hmr.transport)
        } else {
            "disabled".to_string()
        },
    );
    print_kv("Hot Set", format!("{}/32", contract.hot_set.len()));
    print_kv(
        "Config",
        contract
            .config_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(defaults)".to_string()),
    );
    print_kv("Strict", contract.strict);
    print_kv("Verbose", contract.verbose);

    if cli_options.print_contract {
        print_section("Resolved Contract");
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

    let project = ComponentProject::load_from_dir(&contract.root)
        .map_err(|err| format!("failed to load components: {err}"))?;
    let project_css = collect_css_bundle(&contract.root);

    let initial = render_all_routes(&project, &contract, &project_css).map_err(|err| {
        format!(
            "failed to render initial dev document (entry='{}'): {err}",
            contract.entry
        )
    })?;

    let (listener, addr, auto_incremented) =
        bind_dev_listener(contract.server.host.as_str(), contract.server.port)?;

    let (hot_registry, hot_ring) = build_hot_set(&project, &contract.hot_set);
    let hot_registry = Arc::new(hot_registry);
    let sentinel_ring = Arc::new(Mutex::new(hot_ring));

    println!(
        "  {} {} prewarming renderer ({} routes, {} render time)",
        style("[dev]", "1;34"),
        next_loading_icon(),
        initial.route_documents.len(),
        colorize_timing_ms(initial.render_ms)
    );
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

    print_section("Dev Runtime");
    if auto_incremented {
        print_warn(format!(
            "Port {} is busy; auto-switched to {}.",
            contract.server.port,
            addr.port()
        ));
    }
    print_ok("Live dev server is running");
    print_kv("URL", format!("http://{}", addr));
    print_kv(
        "HMR Channel",
        if contract.hmr.enabled {
            format!("SSE -> http://{}/_albedo/hmr", addr)
        } else {
            "disabled".to_string()
        },
    );
    print_kv(
        "File Watcher",
        format!(
            "watching '{}' (debounce={}ms)",
            contract.root.display(),
            contract.watch.debounce_ms
        ),
    );
    print_timing_legend();
    if let Some(components) = scanned_components.as_ref() {
        print_kv("Components", components.len());
    }
    print_kv("Hot Set", format!("{}/{}", hot_registry.len(), HOT_SET_MAX));
    let route_count = 1 + contract.routes.len();
    print_kv("Routes", route_count);
    for (url, entry) in &contract.routes {
        println!("    {} -> {}", style(url, "2"), entry);
    }
    print_kv("Stop", "Ctrl+C");

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
                            eprintln!("[dev] request failed: {err}");
                        }
                    }
                });
            }
            Err(err) => {
                if !is_benign_network_error(&err) {
                    eprintln!("[dev] accept failed: {err}");
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
            "  {} during {} -> {} component(s), {} parse failure(s)",
            style("[scan]", "1;34"),
            context,
            report.components.len(),
            report.failures.len()
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
                "  {} {} -> {}",
                style("[scan]", "1;33"),
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
            eprintln!("[dev] watcher init failed: {err}");
            return;
        }
    };

    if let Err(err) = watcher.watch(contract.root.as_path(), RecursiveMode::Recursive) {
        eprintln!(
            "[dev] watcher failed to watch '{}': {}",
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
                eprintln!("  {} rebuild failed: {}", style("[dev]", "1;31"), err);
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
                                                        style("[dev]", "1;33"),
                                                        err
                                                    );
                                                }
                                            }
                                            Err(_) => {
                                                fallback_full_reload = true;
                                                eprintln!(
                                                    "  {} hot ring lock poisoned during rebuild",
                                                    style("[dev]", "1;33"),
                                                );
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    fallback_full_reload = true;
                                    eprintln!(
                                        "  {} shared state lock poisoned while resolving hot set",
                                        style("[dev]", "1;33"),
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
                                        style("[dev]", "1;33"),
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
                    println!(
                        "  {} {} rebuild complete in {} (reparsed={}, skipped={}, deleted={})",
                        style("[dev]", "1;32"),
                        next_render_icon(),
                        colorize_timing_ms(rebuild_ms),
                        patch_report.reparsed,
                        patch_report.skipped_unchanged,
                        patch_report.deleted
                    );
                } else {
                    let noop_ms = rebuild_start.elapsed().as_secs_f64() * 1000.0;
                    println!(
                        "  {} {} no-op change in {} (skipped={})",
                        style("[dev]", "1;34"),
                        next_loading_icon(),
                        colorize_timing_ms(noop_ms),
                        patch_report.skipped_unchanged
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
                eprintln!("  {} rebuild failed: {}", style("[dev]", "1;31"), err);
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
        println!(
            "  [dev][transport] client={client} method={method} path={path} transport={transport}",
            client = client,
            method = method,
            path = path,
            transport = transport_label
        );
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
    let icon = if route_like {
        next_render_icon()
    } else {
        next_loading_icon()
    };
    let request_ms_colored = colorize_timing_ms(request_ms);
    let socket_wait_ms_colored = colorize_timing_ms(socket_wait_ms);
    let build_render_ms_colored = colorize_timing_ms(build_render_ms);
    let build_total_ms_colored = colorize_timing_ms(build_total_ms);

    if route_like {
        println!(
            "  [dev] {icon} {method} {path} -> {status} (request={request_ms_colored}, socket_wait={socket_wait_ms_colored}, build_render={build_render_ms_colored}, build_total={build_total_ms_colored}, transport={transport}, client={client})",
            icon = icon,
            method = method,
            path = path,
            status = status,
            request_ms_colored = request_ms_colored,
            socket_wait_ms_colored = socket_wait_ms_colored,
            build_render_ms_colored = build_render_ms_colored,
            build_total_ms_colored = build_total_ms_colored,
            transport = transport_label,
            client = client
        );
    } else {
        println!(
            "  [dev] {icon} {method} {path} -> {status} (request={request_ms_colored}, socket_wait={socket_wait_ms_colored}, transport={transport}, client={client})",
            icon = icon,
            method = method,
            path = path,
            status = status,
            request_ms_colored = request_ms_colored,
            socket_wait_ms_colored = socket_wait_ms_colored,
            transport = transport_label,
            client = client
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

    print_section("Production Pipeline");
    print_kv("Components", components.len());
    print_kv(
        "Scan Time",
        format!("{:.2}ms", scan_start.elapsed().as_secs_f64() * 1000.0),
    );

    let compile_start = Instant::now();
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
            &out_dir,
        )
        .map_err(|err| format!("failed to emit production artifacts: {err}"))?;

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

    print_ok("Optimized production build complete");
    print_kv("Output", out_dir.display());
    print_kv("Artifacts", report.artifacts.len() + 3);
    print_kv("Manifest", manifest_path.display());
    print_kv("Shim Runtime", runtime_asset_path.display());
    print_kv("Hydration Runtime", hydration_asset_path.display());
    print_kv(
        "Compile Time",
        format!("{:.2}ms", compile_start.elapsed().as_secs_f64() * 1000.0),
    );
    if missing_sources > 0 {
        print_warn(format!(
            "{missing_sources} manifest modules had unreadable sources and were skipped for static slice precompile."
        ));
    }

    for artifact in report.artifacts.iter().take(8) {
        println!("  - {} ({} bytes)", artifact.relative_path, artifact.bytes);
    }
    if report.artifacts.len() > 8 {
        println!("  ... and {} more artifacts", report.artifacts.len() - 8);
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

    let entry_file = if options.javascript {
        "App.jsx"
    } else {
        "App.tsx"
    };
    let package_name = infer_package_name(target);

    let config = serde_json::json!({
        "contract_version": 1,
        "root": "src/components",
        "entry": entry_file,
        "server": { "host": "127.0.0.1", "port": 3000 },
        "watch": { "debounce_ms": 75, "ignore": ["**/.git/**", "**/node_modules/**"] },
        "hmr": { "enabled": true, "transport": "sse" },
        "hot_set": [],
        "static_slice": { "enabled": true, "opt_out": [] }
    });
    let config_text = serde_json::to_string_pretty(&config)
        .map_err(|err| format!("failed to generate config: {err}"))?;

    let mut files = vec![
        (PathBuf::from(DEV_CONFIG_JSON), format!("{config_text}\n")),
        (
            PathBuf::from("package.json"),
            package_json_template(&package_name),
        ),
        (PathBuf::from("README.md"), readme_template(&package_name)),
        (PathBuf::from(".gitignore"), gitignore_template()),
        (
            PathBuf::from("src/components").join(entry_file),
            app_component_template(options.javascript),
        ),
        (
            PathBuf::from("src/components/theme.css"),
            theme_css_template(),
        ),
        (
            PathBuf::from("src/components/albedo-shell.html"),
            shell_template(),
        ),
        (
            PathBuf::from("public/_albedo/runtime.js"),
            albedo_runtime_shim_template(),
        ),
        (
            PathBuf::from("public/_albedo/hydration.js"),
            albedo_hydration_runtime_template(),
        ),
        (PathBuf::from("public/.gitkeep"), String::new()),
    ];

    if !options.javascript {
        files.push((PathBuf::from("tsconfig.json"), tsconfig_template()));
        files.push((PathBuf::from("albedo-env.d.ts"), albedo_env_template()));
    }

    if !options.force {
        for (relative, _) in &files {
            let file_path = target.join(relative);
            if file_path.exists() {
                return Err(format!(
                    "file '{}' already exists (use --force to overwrite)",
                    file_path.display()
                ));
            }
        }
    }

    for (relative, content) in files {
        let file_path = target.join(relative);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                format!("failed to create directory '{}': {err}", parent.display())
            })?;
        }
        std::fs::write(&file_path, content)
            .map_err(|err| format!("failed to write '{}': {err}", file_path.display()))?;
    }

    Ok(())
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

fn package_json_template(name: &str) -> String {
    format!(
        "{{\n  \"name\": \"{name}\",\n  \"private\": true,\n  \"version\": \"0.1.0\",\n  \"scripts\": {{\n    \"dev\": \"cargo run --bin albedo -- dev\",\n    \"build\": \"cargo run --bin albedo -- build\"\n  }}\n}}\n"
    )
}

fn readme_template(name: &str) -> String {
    format!(
        "# {name}\n\nBuilt with ALBEDO starter.\n\n## Commands\n\n- `cargo run --bin albedo -- dev`\n- `cargo run --bin albedo -- build`\n\n## Config\n\n- `{DEV_CONFIG_JSON}` (primary)\n- `{DEV_CONFIG_TS}` (supported)\n"
    )
}

fn gitignore_template() -> String {
    "node_modules/\ndist/\n.albedo/\ntarget/\n.DS_Store\n".to_string()
}

fn app_component_template(javascript: bool) -> String {
    if javascript {
        return "import \"./theme.css\";\n\nexport default function App() {\n  return (\n    <main className=\"landing\">\n      <section className=\"hero\">\n        <p className=\"badge\">ALBEDO</p>\n        <h1>ALBEDO - Making the internet faster</h1>\n        <p className=\"subcopy\">Get started by editing <code>src/components/App.tsx</code></p>\n        <div className=\"actions\">\n          <a href=\"https://github.com\" target=\"_blank\" rel=\"noreferrer\">Docs</a>\n          <a href=\"https://github.com\" target=\"_blank\" rel=\"noreferrer\">Examples</a>\n        </div>\n      </section>\n    </main>\n  );\n}\n"
            .to_string();
    }

    "import \"./theme.css\";\n\nexport default function App() {\n  return (\n    <main className=\"landing\">\n      <section className=\"hero\">\n        <p className=\"badge\">ALBEDO</p>\n        <h1>ALBEDO - Making the internet faster</h1>\n        <p className=\"subcopy\">Get started by editing <code>src/components/App.tsx</code></p>\n        <div className=\"actions\">\n          <a href=\"https://github.com\" target=\"_blank\" rel=\"noreferrer\">Docs</a>\n          <a href=\"https://github.com\" target=\"_blank\" rel=\"noreferrer\">Examples</a>\n        </div>\n      </section>\n    </main>\n  );\n}\n"
        .to_string()
}

fn theme_css_template() -> String {
    ":root {\n  --ink: #f5f7ff;\n  --muted: #b8bfd3;\n  --line: rgba(245, 247, 255, 0.28);\n  --glass: rgba(8, 12, 24, 0.62);\n  --chip: rgba(12, 18, 34, 0.82);\n  --accent: #8be9ff;\n}\n\n* {\n  box-sizing: border-box;\n}\n\nbody {\n  margin: 0;\n  min-height: 100vh;\n  font-family: \"Segoe UI\", \"Aptos\", sans-serif;\n}\n\n.landing {\n  min-height: 100vh;\n  display: grid;\n  place-items: center;\n  padding: 1.2rem;\n}\n\n.hero {\n  width: min(760px, 100%);\n  border: 1px solid var(--line);\n  border-radius: 20px;\n  background: var(--glass);\n  backdrop-filter: blur(8px);\n  padding: clamp(1.3rem, 4vw, 2.6rem);\n  box-shadow: 0 24px 70px rgba(0, 0, 0, 0.4);\n}\n\n.badge {\n  margin: 0;\n  font-size: 0.72rem;\n  letter-spacing: 0.22em;\n  text-transform: uppercase;\n  color: var(--accent);\n  font-weight: 700;\n}\n\nh1 {\n  margin: 0.75rem 0 0;\n  font-size: clamp(2rem, 7vw, 3.9rem);\n  line-height: 1.03;\n  letter-spacing: -0.02em;\n  max-width: 17ch;\n}\n\n.subcopy {\n  margin: 1rem 0 0;\n  color: var(--muted);\n  font-size: clamp(0.98rem, 2vw, 1.12rem);\n}\n\n.subcopy code {\n  color: var(--ink);\n  background: var(--chip);\n  border: 1px solid var(--line);\n  border-radius: 8px;\n  padding: 0.2rem 0.45rem;\n  font-family: \"Cascadia Code\", \"Consolas\", monospace;\n  font-size: 0.92em;\n}\n\n.actions {\n  margin-top: 1.2rem;\n  display: flex;\n  gap: 0.7rem;\n  flex-wrap: wrap;\n}\n\n.actions a {\n  text-decoration: none;\n  color: var(--ink);\n  border: 1px solid var(--line);\n  background: var(--chip);\n  border-radius: 999px;\n  padding: 0.55rem 0.95rem;\n  font-weight: 600;\n}\n\n.actions a:hover {\n  border-color: var(--accent);\n}\n"
        .to_string()
}

fn shell_template() -> String {
    "<!DOCTYPE html>\n<html>\n<head>\n  <meta charset=\"utf-8\" />\n  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\" />\n  <title>ALBEDO Shell</title>\n</head>\n<body>\n  <script type=\"module\" src=\"/_albedo/runtime.js\"></script>\n  <!--__SLOT___a_root-->\n</body>\n</html>\n"
        .to_string()
}

fn albedo_runtime_shim_template() -> String {
    include_str!("../../assets/albedo-runtime.js").to_string()
}

fn albedo_hydration_runtime_template() -> String {
    include_str!("../../assets/albedo-hydration.js").to_string()
}

fn tsconfig_template() -> String {
    "{\n  \"compilerOptions\": {\n    \"target\": \"ES2022\",\n    \"module\": \"ESNext\",\n    \"moduleResolution\": \"Bundler\",\n    \"strict\": true,\n    \"jsx\": \"preserve\",\n    \"noEmit\": true,\n    \"skipLibCheck\": true,\n    \"types\": []\n  },\n  \"include\": [\n    \"src/**/*\",\n    \"albedo-env.d.ts\"\n  ]\n}\n"
        .to_string()
}

fn albedo_env_template() -> String {
    "declare namespace JSX {\n  interface IntrinsicElements {\n    [elementName: string]: any;\n  }\n}\n"
        .to_string()
}

fn print_help() {
    print_banner();
    print_section("Usage");
    println!("  {}", style("albedo <COMMAND> [OPTIONS]", "1"));

    print_section("Commands");
    print_command(
        "init [DIR]",
        "Create a new ALBEDO project with starter boilerplate",
    );
    print_command("help", "Show command list and examples");
    print_command("dev [DIR]", "Alias for `albedo run dev`");
    print_command("build [DIR]", "Alias for `albedo run dev --prod`");
    print_command("run dev [DIR]", "Validate and run the development workflow");
    print_command(
        "run dev --prod [DIR]",
        "Compile an optimized production build into .albedo/dist",
    );

    print_section("Dev Flags");
    print_option("--config <FILE>", "Use explicit albedo.config.json/ts");
    print_option("--entry <FILE>", "Override entry module relative to root");
    print_option("--host <IP>", "Override server host");
    print_option("--port <PORT>", "Override server port");
    print_option("--no-hmr", "Disable HMR in resolved config");
    print_option("--strict", "Enable strict startup behavior");
    print_option("--verbose, -v", "Verbose diagnostics");
    print_option("--open", "Open browser on startup flag");
    print_option("--print-contract", "Print resolved merged contract JSON");
    print_option("--prod", "Production build mode for `run dev`");

    print_section("Examples");
    println!("  {}", style("albedo init my-next-run", "2"));
    println!(
        "  {}",
        style("albedo dev my-next-run/src/components --entry App.tsx", "2")
    );
    println!(
        "  {}",
        style("albedo build --config ./albedo.config.json", "2")
    );
}

fn print_init_help() {
    print_banner();
    print_section("Init Usage");
    println!(
        "  {}",
        style("albedo init [DIR] [--js|--ts] [--force]", "1")
    );
    print_option("--js", "Create JavaScript starter (App.jsx)");
    print_option("--ts", "Create TypeScript starter (App.tsx, default)");
    print_option("--force", "Overwrite existing scaffold files");
}

fn print_command(command: &str, description: &str) {
    println!("  {:<32} {}", style(command, "1"), description);
}

fn print_option(option: &str, description: &str) {
    println!("  {:<32} {}", style(option, "1"), description);
}

fn print_banner() {
    let rule = "=".repeat(RULE_WIDTH);
    println!();
    println!("{}", gradient_text(&rule, &RULE_PALETTE, false));
    println!(
        "{} {}",
        gradient_text("ALBEDO CLI", &BANNER_PALETTE, true),
        style("Modern Build Surface for Rust + JSX Projects", "2")
    );
    println!("{}", gradient_text(&rule, &RULE_PALETTE, false));
}

fn print_section(title: &str) {
    println!();
    println!("{}", style(&format!("[{title}]"), "1;34"));
}

fn print_timing_legend() {
    print_kv(
        "Timing Index",
        format!(
            "{} <=1ms  {} <=25ms  {} <=250ms  {} >250ms",
            style("GREEN", "1;32"),
            style("CYAN", "1;36"),
            style("YELLOW", "1;33"),
            style("RED", "1;31")
        ),
    );
    print_kv(
        "Metrics",
        format!(
            "request={} socket_wait={} build_render={} build_total={}",
            style("client->server", "2"),
            style("socket idle", "2"),
            style("route pre-render", "2"),
            style("full prewarm", "2")
        ),
    );
}

fn print_kv(label: &str, value: impl std::fmt::Display) {
    println!("  {:<20} {}", style(label, "2"), value);
}

fn print_ok(message: impl std::fmt::Display) {
    println!("  {} {}", style("[OK]", "1;32"), message);
}

fn print_warn(message: impl std::fmt::Display) {
    println!("  {} {}", style("[WARN]", "1;33"), message);
}

fn print_error(message: impl std::fmt::Display) {
    eprintln!("{} {}", style("[ERROR]", "1;31"), message);
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

fn next_loading_icon() -> String {
    next_pulse_icon(&LOADING_FRAMES, &LOADING_COLORS, true)
}

fn next_render_icon() -> String {
    next_pulse_icon(&RENDER_FRAMES, &RENDER_COLORS, true)
}

fn next_pulse_icon(frames: &[&str], colors: &[u8], bold: bool) -> String {
    if frames.is_empty() {
        return String::new();
    }
    let tick = DEV_PULSE_TICK.fetch_add(1, Ordering::Relaxed) as usize;
    let frame = frames[tick % frames.len()];
    let color = if colors.is_empty() {
        81
    } else {
        colors[tick % colors.len()]
    };
    style_256(frame, color, bold)
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
    fn test_parse_init_args_defaults() {
        let options = parse_init_args(&[]).unwrap();
        assert_eq!(options, InitOptions::default());
    }

    #[test]
    fn test_parse_init_args_with_js_and_force() {
        let args = vec![
            "my-app".to_string(),
            "--js".to_string(),
            "--force".to_string(),
        ];
        let options = parse_init_args(&args).unwrap();
        assert_eq!(options.target_dir, PathBuf::from("my-app"));
        assert!(options.javascript);
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
        let options = InitOptions::default();
        scaffold_project(&target, &options).unwrap();

        assert!(target.join(DEV_CONFIG_JSON).is_file());
        assert!(target.join("src/components/App.tsx").is_file());
        assert!(target.join("src/components/theme.css").is_file());
        assert!(target.join("tsconfig.json").is_file());
        assert!(target.join("albedo-env.d.ts").is_file());
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
