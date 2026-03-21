use dom_render_compiler::dev_contract::resolve_dev_contract;
use dom_render_compiler::scanner::ProjectScanner;
use dom_render_compiler::showcase::{build_showcase_artifact, ShowcaseRenderRequest};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

const RULE_WIDTH: usize = 84;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "analyze" => {
            if args.len() < 3 {
                print_error("Missing directory path");
                print_usage();
                std::process::exit(1);
            }

            let path = PathBuf::from(&args[2]);
            let verbose =
                args.contains(&"--verbose".to_string()) || args.contains(&"-v".to_string());
            analyze_project(&path, verbose);
        }
        "showcase" => {
            if let Err(err) = run_showcase_command(&args[2..]) {
                print_error(err);
                print_usage();
                std::process::exit(1);
            }
        }
        "bundle" => {
            if let Err(err) = run_bundle_command(&args[2..]) {
                print_error(err);
                print_usage();
                std::process::exit(1);
            }
        }
        "dev" => {
            if let Err(err) = run_dev_command(&args[2..]) {
                print_error(err);
                print_usage();
                std::process::exit(1);
            }
        }
        "help" | "--help" | "-h" => {
            print_usage();
        }
        unknown => {
            print_error(format!("Unknown command '{unknown}'"));
            print_usage();
            std::process::exit(1);
        }
    }
}

fn analyze_project(path: &Path, verbose: bool) {
    print_banner();
    print_section("Analyze");
    print_kv("Path", path.display());
    print_kv("Verbose", verbose);

    let scanner = ProjectScanner::new();
    let scan_start = Instant::now();

    let components = match scanner.scan_directory(path) {
        Ok(comps) => comps,
        Err(err) => {
            print_error(format!("Scan failed: {err}"));
            std::process::exit(1);
        }
    };

    let scan_time = scan_start.elapsed();
    print_ok(format!("Discovered {} component(s)", components.len()));
    print_ok(format!(
        "Scan completed in {:.2}ms",
        scan_time.as_secs_f64() * 1000.0
    ));

    print_section("Component Inventory");
    for (idx, component) in components.iter().enumerate() {
        println!(
            "  {:>2}. {:<24} {}",
            idx + 1,
            style(&component.name, "1"),
            style(&format!("({})", component.file_path), "2")
        );

        if verbose {
            let mut imports = component.imports.clone();
            imports.sort();
            imports.dedup();

            print_kv(
                "Export",
                if component.is_default_export {
                    "default"
                } else {
                    "named"
                },
            );
            print_kv(
                "Estimated Size",
                format!("{} bytes", component.estimated_size),
            );
            print_kv(
                "Imports",
                if imports.is_empty() {
                    "-".to_string()
                } else {
                    imports.join(", ")
                },
            );
        }
    }

    print_section("Build Graph");
    let build_start = Instant::now();
    let compiler = scanner.build_compiler(components.clone());
    let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;

    let mut edge_count = 0usize;
    for component_id in compiler.graph().component_ids() {
        edge_count += compiler.graph().get_dependencies(&component_id).len();
    }

    print_ok(format!("Dependency graph built in {:.2}ms", build_ms));
    print_kv("Nodes", compiler.graph().len());
    print_kv("Edges", edge_count);

    if verbose {
        print_section("Dependency Map");
        let mut nodes = compiler.graph().components();
        nodes.sort_by(|left, right| left.name.cmp(&right.name));

        for node in nodes {
            let mut dependencies = compiler
                .graph()
                .get_dependencies(&node.id)
                .into_iter()
                .filter_map(|id| compiler.graph().get(&id).map(|dep| dep.name))
                .collect::<Vec<_>>();
            dependencies.sort();

            let mut dependents = compiler
                .graph()
                .get_dependents(&node.id)
                .into_iter()
                .filter_map(|id| compiler.graph().get(&id).map(|dep| dep.name))
                .collect::<Vec<_>>();
            dependents.sort();

            println!("  - {}", style(&node.name, "1"));
            print_kv(
                "Requires",
                if dependencies.is_empty() {
                    "-".to_string()
                } else {
                    dependencies.join(", ")
                },
            );
            print_kv(
                "Used By",
                if dependents.is_empty() {
                    "-".to_string()
                } else {
                    dependents.join(", ")
                },
            );
        }
    }

    print_section("Optimize");
    let optimize_start = Instant::now();
    let result = match compiler.optimize() {
        Ok(result) => result,
        Err(err) => {
            print_error(format!("Optimization failed: {err}"));
            std::process::exit(1);
        }
    };
    let optimize_time = optimize_start.elapsed();

    print_ok("Optimization complete");

    print_section("Metrics");
    print_kv("Components", result.metrics.total_components);
    print_kv(
        "Total Weight",
        format!("{:.2} KB", result.metrics.total_weight_kb),
    );
    print_kv(
        "Scan Time",
        format!("{:.2}ms", scan_time.as_secs_f64() * 1000.0),
    );
    print_kv(
        "Optimization Time",
        format!("{:.2}ms", optimize_time.as_secs_f64() * 1000.0),
    );
    print_kv(
        "Total Time",
        format!(
            "{:.2}ms",
            (scan_time + optimize_time).as_secs_f64() * 1000.0
        ),
    );
    print_kv("Parallel Batches", result.parallel_batches.len());
    print_kv(
        "Estimated Improvement",
        format!("{:.2}ms", result.metrics.estimated_improvement_ms),
    );

    print_section(&format!(
        "Critical Path ({} components)",
        result.critical_path.len()
    ));
    for (index, component_id) in result.critical_path.iter().enumerate() {
        if let Some(component) = compiler.graph().get(component_id) {
            let dep_count = compiler.graph().get_dependencies(component_id).len();
            println!(
                "  {:>2}. {:<24} weight={:<8.0} deps={}",
                index + 1,
                component.name,
                component.weight,
                dep_count
            );
        }
    }

    print_section("Render Batches");
    print_kv("Legend", "[L]=LCP, [A]=Above Fold, [I]=Interactive");
    for batch in &result.parallel_batches {
        println!(
            "  {} {:02}  components={}  time={:.0}ms  defer={}",
            style("Batch", "1;35"),
            batch.level,
            batch.components.len(),
            batch.estimated_time_ms,
            if batch.can_defer { "yes" } else { "no" }
        );

        for component_id in &batch.components {
            if let Some(component) = compiler.graph().get(component_id) {
                let marker = if component.is_lcp_candidate {
                    "[L]"
                } else if component.is_above_fold {
                    "[A]"
                } else if component.is_interactive {
                    "[I]"
                } else {
                    "   "
                };

                if verbose {
                    let dep_count = compiler.graph().get_dependencies(component_id).len();
                    let dependent_count = compiler.graph().get_dependents(component_id).len();
                    println!(
                        "    {} {:<22} weight={:<8.0} bitrate={:<8.0} priority={:.2} deps={} dependents={}",
                        marker,
                        component.name,
                        component.weight,
                        component.bitrate,
                        component.bitrate / component.weight.max(1.0),
                        dep_count,
                        dependent_count
                    );
                } else {
                    println!(
                        "    {} {:<22} {:.0}B",
                        marker, component.name, component.weight
                    );
                }
            }
        }
    }

    if verbose {
        print_section("Detailed Analysis");
        for batch in &result.parallel_batches {
            for component_id in &batch.components {
                if let Some(component) = compiler.graph().get(component_id) {
                    println!("  {}", style(&component.name, "1"));
                    print_kv("Level", batch.level);
                    print_kv("Weight", format!("{:.2} KB", component.weight / 1024.0));
                    print_kv("Bitrate", format!("{:.0}", component.bitrate));
                    print_kv(
                        "Priority",
                        format!("{:.2}", component.bitrate / component.weight.max(1.0)),
                    );
                    print_kv("Above Fold", component.is_above_fold);
                    print_kv("LCP Candidate", component.is_lcp_candidate);
                    print_kv("Interactive", component.is_interactive);
                    print_kv("File", component.file_path);
                }
            }
        }
    }

    print_section("Recommendations");
    let mut recommendations = Vec::new();
    for batch in &result.parallel_batches {
        if batch.level != 0 {
            continue;
        }
        for component_id in &batch.components {
            if let Some(component) = compiler.graph().get(component_id) {
                if component.is_lcp_candidate {
                    recommendations.push(format!(
                        "{} correctly prioritized in first batch (LCP)",
                        component.name
                    ));
                }
                if component.weight > 100000.0 {
                    recommendations.push(format!(
                        "{} is heavy ({:.2} KB), consider splitting",
                        component.name,
                        component.weight / 1024.0
                    ));
                }
            }
        }
    }

    if recommendations.is_empty() {
        print_ok("No immediate tuning flags detected");
    } else {
        for recommendation in recommendations {
            print_warn(recommendation);
        }
    }

    let output_path = path.join("render-order.json");
    match compiler.export_json() {
        Ok(json) => {
            if let Err(err) = std::fs::write(&output_path, json) {
                print_warn(format!("Failed to write output: {err}"));
            } else {
                print_section("Output");
                print_kv("File", output_path.display());
            }
        }
        Err(err) => {
            print_warn(format!("Failed to serialize output JSON: {err}"));
        }
    }

    println!();
    println!("{}", accent(&"-".repeat(RULE_WIDTH)));
    println!("{}", style("Analysis complete", "1;32"));
    println!("{}", accent(&"-".repeat(RULE_WIDTH)));
}

#[derive(Debug, Clone)]
struct BundleCliOptions {
    components_root: PathBuf,
    output_dir: PathBuf,
}

fn run_bundle_command(raw_args: &[String]) -> Result<(), String> {
    let options = parse_bundle_args(raw_args)?;

    print_banner();
    print_section("Bundle");
    print_kv("Components Root", options.components_root.display());
    print_kv("Output Dir", options.output_dir.display());

    let scanner = ProjectScanner::new();
    let scan_start = Instant::now();
    let components = scanner
        .scan_directory(&options.components_root)
        .map_err(|err| {
            format!(
                "failed to scan '{}': {err}",
                options.components_root.display()
            )
        })?;

    if components.is_empty() {
        return Err(format!(
            "no component files found under '{}'; expected .js/.jsx/.ts/.tsx files",
            options.components_root.display()
        ));
    }

    print_ok(format!("Discovered {} component(s)", components.len()));
    print_kv(
        "Scan Time",
        format!("{:.2}ms", scan_start.elapsed().as_secs_f64() * 1000.0),
    );

    let compiler = scanner.build_compiler(components);
    let manifest = compiler
        .optimize_manifest_v2()
        .map_err(|err| format!("failed to build manifest v2: {err}"))?;
    let mut module_sources = HashMap::new();
    for component in &manifest.components {
        if module_sources.contains_key(&component.module_path) {
            continue;
        }

        match std::fs::read_to_string(&component.module_path) {
            Ok(source) => {
                module_sources.insert(component.module_path.clone(), source);
            }
            Err(err) => {
                print_warn(format!(
                    "failed to read module source for static slice '{}': {}",
                    component.module_path, err
                ));
            }
        }
    }

    let emit_start = Instant::now();
    let report = compiler
        .emit_bundle_artifacts_from_manifest_v2_with_sources(
            &manifest,
            &module_sources,
            &dom_render_compiler::bundler::BundlePlanOptions::default(),
            &options.output_dir,
        )
        .map_err(|err| format!("failed to emit bundle artifacts: {err}"))?;

    let manifest_json = serde_json::to_string_pretty(&manifest)
        .map_err(|err| format!("failed to serialize manifest JSON: {err}"))?;
    let manifest_path = options.output_dir.join("render-manifest.v2.json");
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed to create manifest output directory '{}': {err}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&manifest_path, manifest_json).map_err(|err| {
        format!(
            "failed to write manifest JSON to '{}': {err}",
            manifest_path.display()
        )
    })?;

    print_section("Bundle Output");
    print_kv("Manifest", manifest_path.display());
    print_kv("Artifacts", report.artifacts.len() + 1);
    print_kv(
        "Emit Time",
        format!("{:.2}ms", emit_start.elapsed().as_secs_f64() * 1000.0),
    );

    for artifact in &report.artifacts {
        println!("  - {} ({} bytes)", artifact.relative_path, artifact.bytes);
    }

    print_ok("Bundle artifacts emitted successfully");
    Ok(())
}

fn parse_bundle_args(raw_args: &[String]) -> Result<BundleCliOptions, String> {
    if raw_args.is_empty() {
        return Err(
            "Missing components directory. Usage: dom-compiler bundle <DIR> [--out <DIR>]"
                .to_string(),
        );
    }

    let components_root = PathBuf::from(&raw_args[0]);
    if !components_root.exists() {
        return Err(format!(
            "components directory '{}' does not exist",
            components_root.display()
        ));
    }
    if !components_root.is_dir() {
        return Err(format!(
            "components path '{}' is not a directory",
            components_root.display()
        ));
    }

    let mut output_dir = components_root.join(".albedo").join("bundle");

    let mut idx = 1usize;
    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--out" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --out".to_string())?;
                output_dir = PathBuf::from(value);
            }
            unknown => {
                return Err(format!("unknown bundle option '{unknown}'"));
            }
        }
        idx += 1;
    }

    Ok(BundleCliOptions {
        components_root,
        output_dir,
    })
}

fn run_dev_command(raw_args: &[String]) -> Result<(), String> {
    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current working directory: {err}"))?;
    let resolved = resolve_dev_contract(raw_args, &cwd)?;
    let contract_json = serde_json::to_string_pretty(&resolved)
        .map_err(|err| format!("failed to serialize resolved contract: {err}"))?;

    print_banner();
    print_section("Dev Contract");
    print_kv("Contract Version", resolved.contract_version);
    print_kv(
        "Config File",
        resolved
            .config_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(defaults only)".to_string()),
    );
    print_kv("Project Dir", resolved.project_dir.display());
    print_kv("Root", resolved.root.display());
    print_kv("Entry", resolved.entry.as_str());
    print_kv(
        "Server",
        format!("{}:{}", resolved.server.host, resolved.server.port),
    );
    print_kv(
        "HMR",
        if resolved.hmr.enabled {
            format!("enabled ({:?})", resolved.hmr.transport)
        } else {
            "disabled".to_string()
        },
    );
    print_kv(
        "Watch Debounce",
        format!("{}ms", resolved.watch.debounce_ms),
    );
    print_kv("Hot Set", format!("{}/32", resolved.hot_set.len()));
    print_kv(
        "Static Slice",
        if resolved.static_slice.enabled {
            "enabled"
        } else {
            "disabled"
        },
    );
    print_kv("Strict Mode", resolved.strict);
    print_kv("Verbose", resolved.verbose);
    print_kv("Open Browser", resolved.open);

    print_section("Resolved Contract JSON");
    println!("{contract_json}");

    print_ok("Developer contract validated and frozen for this run");
    print_warn(
        "`dev` currently validates contract only; watcher/HMR server loop is not wired yet.",
    );
    Ok(())
}

#[derive(Debug, Clone)]
struct ShowcaseCliOptions {
    components_root: PathBuf,
    entry_module: Option<String>,
    props_json: String,
    page_title: String,
    output_html: Option<PathBuf>,
    serve: bool,
    port: u16,
}

fn run_showcase_command(raw_args: &[String]) -> Result<(), String> {
    let options = parse_showcase_args(raw_args)?;
    let entry = options
        .entry_module
        .clone()
        .or_else(|| detect_default_entry_module(&options.components_root))
        .ok_or_else(|| {
            format!(
                "No entry module found in '{}'. Pass --entry <FILE> (example: App.tsx).",
                options.components_root.display()
            )
        })?;

    let request = ShowcaseRenderRequest {
        components_root: options.components_root.clone(),
        entry_module: entry.clone(),
        props_json: options.props_json.clone(),
        page_title: options.page_title.clone(),
    };

    let artifact = build_showcase_artifact(&request).map_err(|err| err.to_string())?;
    let html = artifact.html_document;
    let stats = artifact.stats;
    let out_path = options
        .output_html
        .clone()
        .unwrap_or_else(|| options.components_root.join("albedo-showcase.html"));

    std::fs::write(&out_path, html.as_bytes()).map_err(|err| {
        format!(
            "failed to write showcase output to '{}': {err}",
            out_path.display()
        )
    })?;

    print_banner();
    print_section("Showcase Output");
    print_kv("File", out_path.display());
    print_kv("Entry", entry);
    print_kv("Components Root", options.components_root.display());

    print_section("Showcase Stats");
    print_kv(
        "Pipeline",
        format!(
            "scan={:.2}ms, graph={:.2}ms, optimize={:.2}ms, render={:.2}ms, total={:.2}ms",
            stats.timings.scan_ms,
            stats.timings.graph_build_ms,
            stats.timings.optimize_ms,
            stats.timings.render_ms,
            stats.timings.total_ms
        ),
    );
    print_kv(
        "Graph",
        format!(
            "components={}, dependencies={}, roots={}, leaves={}, critical-path={}, batches={}",
            stats.graph.total_components,
            stats.graph.total_dependencies,
            stats.graph.root_components,
            stats.graph.leaf_components,
            stats.graph.critical_path_len,
            stats.graph.parallel_batches
        ),
    );
    print_kv(
        "Degree Peaks",
        format!(
            "max-dependencies={}, max-dependents={}, total-weight={:.2}KB",
            stats.graph.max_dependencies_per_component,
            stats.graph.max_dependents_per_component,
            stats.graph.total_weight_kb
        ),
    );
    print_kv(
        "Hash Samples",
        format!(
            "showing {} of {}",
            stats.dependency_hashes.len().min(8),
            stats.dependency_hashes.len()
        ),
    );
    for hash in stats.dependency_hashes.iter().take(8) {
        println!(
            "  - {:<22} {} (resolved {}/{})",
            hash.component_name,
            hash.dependency_hash,
            hash.resolved_dependency_count,
            hash.import_count
        );
    }

    if options.serve {
        print_section("Serve");
        print_ok(format!(
            "Serving showcase on http://127.0.0.1:{}",
            options.port
        ));
        print_kv("Render Time", format!("{:.2}ms", stats.timings.render_ms));
        print_kv(
            "Total Build+Render",
            format!("{:.2}ms", stats.timings.total_ms),
        );
        print_kv("Stop", "Ctrl+C");
        serve_showcase_html(
            &html,
            options.port,
            stats.timings.render_ms,
            stats.timings.total_ms,
        )
        .map_err(|err| err.to_string())?;
    }

    Ok(())
}

fn parse_showcase_args(raw_args: &[String]) -> Result<ShowcaseCliOptions, String> {
    if raw_args.is_empty() {
        return Err(
            "Missing components directory. Usage: dom-compiler showcase <DIR> [OPTIONS]"
                .to_string(),
        );
    }

    let components_root = PathBuf::from(&raw_args[0]);
    if !components_root.exists() {
        return Err(format!(
            "components directory '{}' does not exist",
            components_root.display()
        ));
    }
    if !components_root.is_dir() {
        return Err(format!(
            "components path '{}' is not a directory",
            components_root.display()
        ));
    }

    let mut entry_module = None;
    let mut props_json = "{}".to_string();
    let mut page_title = "ALBEDO Showcase".to_string();
    let mut output_html = None;
    let mut serve = false;
    let mut port = 4173u16;

    let mut idx = 1usize;
    while idx < raw_args.len() {
        let arg = &raw_args[idx];
        match arg.as_str() {
            "--entry" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --entry".to_string())?;
                entry_module = Some(value.clone());
            }
            "--props" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --props".to_string())?;
                props_json = value.clone();
            }
            "--title" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --title".to_string())?;
                page_title = value.clone();
            }
            "--out" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --out".to_string())?;
                output_html = Some(PathBuf::from(value));
            }
            "--serve" => {
                serve = true;
            }
            "--port" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --port".to_string())?;
                port = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port '{value}'"))?;
            }
            unknown => {
                return Err(format!("unknown showcase option '{unknown}'"));
            }
        }
        idx += 1;
    }

    serde_json::from_str::<serde_json::Value>(&props_json)
        .map_err(|err| format!("--props must be valid JSON: {err}"))?;

    Ok(ShowcaseCliOptions {
        components_root,
        entry_module,
        props_json,
        page_title,
        output_html,
        serve,
        port,
    })
}

fn detect_default_entry_module(components_root: &Path) -> Option<String> {
    for candidate in ["App.tsx", "App.jsx", "App.ts", "App.js"] {
        if components_root.join(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn serve_showcase_html(
    html: &str,
    port: u16,
    render_ms: f64,
    total_ms: f64,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) =
                    handle_showcase_connection(stream, html.as_bytes(), render_ms, total_ms)
                {
                    if !is_benign_network_error(&err) {
                        eprintln!("showcase request failed: {err}");
                    }
                }
            }
            Err(err) => {
                if !is_benign_network_error(&err) {
                    eprintln!("showcase accept failed: {err}");
                }
            }
        }
    }

    Ok(())
}

fn handle_showcase_connection(
    mut stream: TcpStream,
    html: &[u8],
    render_ms: f64,
    total_ms: f64,
) -> std::io::Result<()> {
    let request_start = Instant::now();
    let mut first_line = String::new();
    {
        let mut reader = BufReader::new(stream.try_clone()?);
        reader.read_line(&mut first_line)?;
    }

    let method = first_line.split_whitespace().next().unwrap_or("GET");
    let raw_target = first_line.split_whitespace().nth(1).unwrap_or("/");
    let path = normalize_request_path(raw_target);

    let status = if method != "GET" {
        write_http_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "text/plain; charset=utf-8",
            b"Method not allowed\n",
            &[],
        )?;
        405
    } else if path == "/health" {
        write_http_response(
            &mut stream,
            200,
            "OK",
            "text/plain; charset=utf-8",
            b"ok\n",
            &[],
        )?;
        200
    } else if path == "/" || path == "/index.html" || is_route_like_path(path.as_str()) {
        let extra_headers = vec![
            ("x-albedo-render-ms", format!("{render_ms:.2}")),
            ("x-albedo-total-ms", format!("{total_ms:.2}")),
            ("cache-control", "no-store".to_string()),
        ];
        write_http_response(
            &mut stream,
            200,
            "OK",
            "text/html; charset=utf-8",
            html,
            &extra_headers,
        )?;
        200
    } else {
        write_http_response(
            &mut stream,
            404,
            "Not Found",
            "text/plain; charset=utf-8",
            b"Not found\n",
            &[],
        )?;
        404
    };

    let duration_ms = request_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "  [serve] {method} {path} -> {status} ({duration_ms:.2}ms)",
        method = method,
        path = path,
        status = status,
        duration_ms = duration_ms
    );
    Ok(())
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
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if !extra_headers.is_empty() {
        let mut replacement = String::new();
        replacement.push_str(&format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        ));
        for (name, value) in extra_headers {
            replacement.push_str(name);
            replacement.push_str(": ");
            replacement.push_str(value.as_str());
            replacement.push_str("\r\n");
        }
        replacement.push_str("\r\n");
        headers = replacement;
    }

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

fn print_usage() {
    print_banner();
    print_section("Usage");
    println!("  {}", style("dom-compiler <COMMAND> [OPTIONS]", "1"));

    print_section("Commands");
    println!(
        "  {:<22} {}",
        style("analyze <DIR>", "1"),
        "Analyze component graph and export render-order.json"
    );
    println!(
        "  {:<22} {}",
        style("bundle <DIR>", "1"),
        "Generate bundle artifacts (plan + wrappers + manifest)"
    );
    println!(
        "  {:<22} {}",
        style("showcase <DIR>", "1"),
        "Render JSX/TSX to browser-ready HTML (with stats)"
    );
    println!(
        "  {:<22} {}",
        style("dev [DIR]", "1"),
        "Resolve and validate the frozen developer contract"
    );
    println!("  {:<22} {}", style("help", "1"), "Display command usage");

    print_section("Options");
    println!(
        "  {:<22} {}",
        style("-v, --verbose", "1"),
        "Detailed analysis output"
    );
    println!(
        "  {:<22} {}",
        style("--entry <FILE>", "1"),
        "Showcase entry module (default: auto-detect App.*)"
    );
    println!(
        "  {:<22} {}",
        style("--props <JSON>", "1"),
        "Initial props JSON passed to showcase renderer"
    );
    println!(
        "  {:<22} {}",
        style("--title <TEXT>", "1"),
        "Showcase HTML page title"
    );
    println!(
        "  {:<22} {}",
        style("showcase --out <FILE>", "1"),
        "Showcase output file path"
    );
    println!(
        "  {:<22} {}",
        style("bundle --out <DIR>", "1"),
        "Bundle output directory (default: <DIR>/.albedo/bundle)"
    );
    println!(
        "  {:<22} {}",
        style("--serve", "1"),
        "Host showcase on local HTTP server"
    );
    println!(
        "  {:<22} {}",
        style("--port <PORT>", "1"),
        "Port for local showcase server (default: 4173)"
    );
    println!(
        "  {:<22} {}",
        style("dev --config <FILE>", "1"),
        "Use explicit albedo.config.json/ts path"
    );
    println!(
        "  {:<22} {}",
        style("dev --entry <FILE>", "1"),
        "Override entry module relative to dev root"
    );
    println!(
        "  {:<22} {}",
        style("dev --host <IP>", "1"),
        "Override dev server host"
    );
    println!(
        "  {:<22} {}",
        style("dev --port <PORT>", "1"),
        "Override dev server port"
    );
    println!(
        "  {:<22} {}",
        style("dev --no-hmr", "1"),
        "Disable HMR channel in resolved contract"
    );
    println!(
        "  {:<22} {}",
        style("dev --strict", "1"),
        "Fail on scanner parse warnings in dev mode"
    );
    println!(
        "  {:<22} {}",
        style("dev --open", "1"),
        "Open browser on startup (contract flag)"
    );
    println!(
        "  {:<22} {}",
        style("dev --print-contract", "1"),
        "Emit resolved dev contract JSON"
    );

    print_section("Examples");
    println!("  {}", style("dom-compiler analyze ./src --verbose", "2"));
    println!(
        "  {}",
        style(
            "dom-compiler bundle ./test-app/src/components --out ./test-app/.albedo/bundle",
            "2"
        )
    );
    println!(
        "  {}",
        style(
            "dom-compiler showcase ./test-app/src/components --entry App.jsx --serve",
            "2"
        )
    );
    println!(
        "  {}",
        style(
            "dom-compiler showcase ./components --entry App.tsx --props \"{\\\"name\\\":\\\"ALBEDO\\\"}\"",
            "2"
        )
    );
    println!(
        "  {}",
        style(
            "dom-compiler dev ./test-app/src/components --entry App.jsx --host 127.0.0.1 --port 3000 --strict --print-contract",
            "2"
        )
    );
    println!();
}

fn print_banner() {
    let rule = "-".repeat(RULE_WIDTH);
    println!();
    println!("{}", accent(&rule));
    println!(
        "{} {}",
        style("ALBEDO CLI", "1;36"),
        style("Advanced DOM Render Compiler", "2")
    );
    println!("{}", accent(&rule));
}

fn print_section(title: &str) {
    println!();
    println!("{}", style(&format!("[{title}]"), "1;34"));
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
    eprintln!("  {} {}", style("[ERROR]", "1;31"), message);
}

fn accent(value: &str) -> String {
    style(value, "36")
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
    use std::fs;
    use std::io::Error;

    #[test]
    fn test_parse_showcase_args_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let args = vec![temp.path().to_string_lossy().to_string()];
        let options = parse_showcase_args(&args).unwrap();

        assert_eq!(options.props_json, "{}");
        assert_eq!(options.page_title, "ALBEDO Showcase");
        assert_eq!(options.port, 4173);
        assert!(!options.serve);
    }

    #[test]
    fn test_parse_bundle_args_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let args = vec![temp.path().to_string_lossy().to_string()];
        let options = parse_bundle_args(&args).unwrap();

        assert_eq!(options.components_root, temp.path().to_path_buf());
        assert_eq!(
            options.output_dir,
            temp.path().join(".albedo").join("bundle")
        );
    }

    #[test]
    fn test_parse_bundle_args_custom_output_dir() {
        let temp = tempfile::tempdir().unwrap();
        let custom_out = temp.path().join("custom-out");
        let args = vec![
            temp.path().to_string_lossy().to_string(),
            "--out".to_string(),
            custom_out.to_string_lossy().to_string(),
        ];

        let options = parse_bundle_args(&args).unwrap();
        assert_eq!(options.output_dir, custom_out);
    }

    #[test]
    fn test_detect_default_entry_module_prefers_tsx() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("App.tsx"),
            "export default function App(){return <div/>;}",
        )
        .unwrap();
        fs::write(
            temp.path().join("App.jsx"),
            "export default function App(){return <div/>;}",
        )
        .unwrap();

        let detected = detect_default_entry_module(temp.path());
        assert_eq!(detected.as_deref(), Some("App.tsx"));
    }

    #[test]
    fn test_is_benign_network_error_for_connection_abort() {
        let err = Error::from(ErrorKind::ConnectionAborted);
        assert!(is_benign_network_error(&err));
    }

    #[test]
    fn test_is_benign_network_error_for_invalid_input() {
        let err = Error::from(ErrorKind::InvalidInput);
        assert!(!is_benign_network_error(&err));
    }

    #[test]
    fn test_normalize_request_path_strips_query_and_fragment() {
        assert_eq!(
            normalize_request_path("/dashboard/users/42?tab=activity#top"),
            "/dashboard/users/42"
        );
        assert_eq!(normalize_request_path(""), "/");
    }

    #[test]
    fn test_is_route_like_path_filters_assets() {
        assert!(is_route_like_path("/dashboard/users/42"));
        assert!(is_route_like_path("/blog"));
        assert!(!is_route_like_path("/favicon.ico"));
        assert!(!is_route_like_path("/assets/main.js"));
    }
}
