use dom_render_compiler::budget::{
    compute_bundle_byte_report, evaluate_budget, evaluate_bundle_budget, format_report_pretty,
    load_budget_from_dir, BudgetReport, TierBudget,
};
use dom_render_compiler::bundler::emit::BundleEmitReport;
use dom_render_compiler::bundler::BundlePlanOptions;
use dom_render_compiler::dev_contract::{
    parse_dev_cli_args, resolve_dev_contract, ResolvedDevContract, DEV_CONFIG_TS,
};
use dom_render_compiler::manifest::schema::{RenderManifestV2, Tier};
use dom_render_compiler::parser::ParsedComponent;
use dom_render_compiler::scanner::{ProjectScanner, ScanFailure, ScanMode};
use dom_render_compiler::types::TierReport;
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
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

#[path = "albedo/tui/mod.rs"]
mod tui;

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
// the guestbook route, which reads and writes a FORGE collection
// declared in `albedo.config.ts`. Old shape (`src/App.tsx` + Tier-C
// fetch demo) retired — no upgrade path from pre-Phase-P scaffolds;
// users on the old shape `albedo init --force` into a fresh dir.
//
// The guestbook replaced a scalar `broadcast()` counter route that
// demonstrated a primitive which does not paint live (TODO.md § 2b):
// the scaffold's headline demo was a known-broken feature, and it
// failed in a way that read as "broadcast is broken" rather than as a
// paint bug. A list topic exercises the same substrate and works.
const SCAFFOLD_LAYOUT: &str = include_str!("../../scaffold/src/routes/layout.tsx");
const SCAFFOLD_INDEX_ROUTE: &str = include_str!("../../scaffold/src/routes/index.tsx");
const SCAFFOLD_GUESTBOOK_ROUTE: &str =
    include_str!("../../scaffold/src/routes/guestbook.tsx");
const SCAFFOLD_ROOM_ROUTE: &str = include_str!("../../scaffold/src/routes/room/[id].tsx");
// AUTH P2 — the on-ramp's account page. The `auth` block in
// `scaffold/albedo.config.ts` is what mounts the endpoints these forms post
// to, so the two are edited together or not at all.
const SCAFFOLD_SIGN_IN_ROUTE: &str = include_str!("../../scaffold/src/routes/sign-in.tsx");
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
    install_tracing();
    if let Err(err) = run(std::env::args().collect()) {
        print_error(err);
        std::process::exit(1);
    }
}

/// Attach a `tracing` subscriber, but only when `RUST_LOG` asks for one.
///
/// Until this existed **nothing in this binary ever subscribed**, so all ~66
/// `info!`/`warn!`/`error!` sites in the tree — a panicked request handler, a
/// rejected action envelope, every WebTransport failure — wrote to no one. They
/// were not dead code, they were live code with no receiver, which is worse:
/// grepping the source suggests the diagnostics exist.
///
/// ## Why silent by default
///
/// The two audiences want opposite things and there is no default that serves
/// both. An author running `albedo dev` gets a composed, styled console; a line
/// like `2026-08-04T…Z  INFO albedo_server::server: ALBEDO server listening on
/// 127.0.0.1:3000` underneath `✓ dev · http://…` is noise restating what the
/// banner already said. Someone debugging wants all of it. `RUST_LOG` is the
/// switch every Rust operator already reaches for, so it decides — and its
/// absence means the curated output stays curated.
///
/// **This is not a way to reach a user.** Anything an author *must* see is not a
/// log: it belongs in the CLI's own output, the way a FORGE schema migration
/// travels out through [`albedo_server::BootReport`] and gets printed. That
/// split is the whole lesson — a message on a channel nobody is listening to is
/// indistinguishable from no message at all.
fn install_tracing() {
    use tracing_subscriber::EnvFilter;

    // `try_from_default_env` errs when RUST_LOG is unset or unparseable. Both
    // mean "the operator did not ask for logs", and an unparseable filter is
    // not worth failing a build command over.
    let Ok(filter) = EnvFilter::try_from_default_env() else {
        return;
    };
    // stderr, so logs never interleave into stdout for anything piping our
    // output. `try_init` rather than `init`: a double install should not panic
    // a CLI over its logging.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn run(args: Vec<String>) -> Result<(), String> {
    // 🔑 **A shipped binary is not a CLI.** If this executable carries an
    // appended project, it exists to serve that project and nothing else — the
    // person running it copied one file to a box, and `init`/`build`/`ship`
    // are meaningless there. Checked before anything else so no other command
    // can shadow it, and before the first-run greeting so a server does not
    // print a welcome banner.
    if let Some(shipped) = shipped_project()? {
        return run_shipped_app(shipped, &args[1..]);
    }

    // First-run greeting — one-time welcome on a fresh install, then a no-op
    // (single Path::exists check). It never dispatches a command itself, so
    // whatever the user typed still reaches the match below.
    first_run::welcome_on_first_run();

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
        // Trust polish · what the build already knows about itself. Every check
        // is a derivation, never a maintained list — see `doctor::matrix`.
        "doctor" => run_doctor_command(&args[2..]),
        // The instrument panel as intended, drawn from synthetic signal. Kept
        // behind its own verb precisely so `dev`'s rule — if a number appears,
        // something measured it — stays true of `dev`. See `tui::demo`.
        "demo" => tui::demo::run(),
        // Phase J CLI clarity:
        //   * `albedo files [dir]` — pure static file server; serves any directory verbatim. This
        //     is what `albedo serve` did before.
        //   * `albedo serve` — production server: builds the project via the same stitcher as `dev`
        //     / `dev --prod` / `build`, then serves the resulting `.albedo/dist`. One stitcher
        //     feeds every command — dev/prod parity by construction.
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

    with_spinner("scaffolding project…", || {
        scaffold_project(&target, &options)
    })?;

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
    /// One file: this runtime with the project appended to it.
    Binary,
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
    /// TLS, as flags. Seeded from the environment so a container that sets
    /// `ALBEDO_TLS_CERT` needs no flags, and overwritten by anything typed —
    /// the flag is the documented surface, the variable is the fallback.
    tls: albedo_server::tls::TlsSettings,
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
            if report.violations.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    Ok(())
}

/// `albedo doctor` — report what the build already knows about itself.
///
/// 🔑 **Every section is a derivation, and that is the organising rule.** A
/// health tool that carried a hand-maintained list would drift from the system
/// exactly the way the things it is auditing drift, which is the failure it
/// exists to remove. So doctor may report what the compiler established for its
/// own reasons — the reads on each route, the class SHUTTER charges — and may
/// shell out to a checker that is itself authoritative (`tsc`), and nothing else.
///
/// It exits non-zero only on a **hard failure** (a type error), never on a
/// finding. A finding is a judgement the author has to make — a partition keyed
/// by a route parameter is correct for an unguessable id and a leak for a
/// sequential one — and a tool that failed the build over a judgement would be
/// turned off, taking the type check with it.
fn run_doctor_command(raw_args: &[String]) -> Result<(), String> {
    if raw_args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_doctor_help();
        return Ok(());
    }
    let json = raw_args.iter().any(|arg| arg == "--json");
    let forwarded: Vec<String> = raw_args
        .iter()
        .filter(|arg| arg.as_str() != "--json")
        .cloned()
        .collect();

    let cwd = std::env::current_dir()
        .map_err(|err| format!("failed to resolve current directory: {err}"))?;
    let contract = resolve_dev_contract(&forwarded, &cwd)?;

    // Compiled fresh rather than read out of `.albedo/dist`. A report derived
    // from a stale build describes a system that is not running, which is the
    // one thing an audit artefact must never do.
    let manifest = build_manifest_for_budget(&contract)?;
    let matrix = dom_render_compiler::doctor::Matrix::derive(&manifest.routes);
    let findings = matrix.findings();
    let types = check_types(&contract.project_dir);
    let unattributed = unattributed_route_files(&contract.root);

    if json {
        // Built as a typed value rather than through `serde_json::json!`, which
        // unwraps internally — so a type that stops serializing cleanly takes the
        // CLI down with a panic instead of an error. That is not hypothetical:
        // the first cut panicked here on a newtype variant serde's tagged
        // representation cannot encode.
        let report = DoctorReport {
            typescript: match &types {
                TypeCheck::Clean => TypeCheckReport {
                    status: "clean",
                    detail: None,
                },
                TypeCheck::Skipped(why) => TypeCheckReport {
                    status: "skipped",
                    detail: Some(why.clone()),
                },
                TypeCheck::Failed(output) => TypeCheckReport {
                    status: "failed",
                    detail: Some(output.clone()),
                },
            },
            matrix: &matrix,
            findings: findings
                .iter()
                .map(|finding| FindingReport {
                    route: finding.route().to_string(),
                    explain: finding.explain(),
                    detail: finding,
                })
                .collect(),
            unattributed_route_files: unattributed.clone(),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report)
                .map_err(|err| format!("failed to serialize doctor report: {err}"))?
        );
    } else {
        print_banner();
        print_doctor_report(
            &matrix,
            &findings,
            &types,
            &unattributed,
            contract.detected_layout.as_deref(),
        );
    }

    match types {
        TypeCheck::Failed(_) => Err("typescript reported errors".to_string()),
        _ => Ok(()),
    }
}

/// The `--json` shape, for CI.
#[derive(serde::Serialize)]
struct DoctorReport<'a> {
    typescript: TypeCheckReport,
    matrix: &'a dom_render_compiler::doctor::Matrix,
    findings: Vec<FindingReport<'a>>,
    /// Route-shaped files that produced no row in the matrix. See
    /// [`unattributed_route_files`] for why this is the most important field in
    /// the report.
    unattributed_route_files: Vec<String>,
}

#[derive(serde::Serialize)]
struct TypeCheckReport {
    status: &'static str,
    /// Why it was skipped, or what it reported. Absent when clean — there is
    /// nothing to say and an empty string would read like output nobody parsed.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(serde::Serialize)]
struct FindingReport<'a> {
    route: String,
    /// The same sentence the pretty printer shows, so a CI log and a terminal
    /// never describe one finding two ways.
    explain: String,
    detail: &'a dom_render_compiler::doctor::Finding,
}

/// What `tsc --noEmit` had to say, if it could be asked at all.
enum TypeCheck {
    Clean,
    /// No local TypeScript, or no `tsconfig.json`. Reported rather than
    /// silently passing: "we did not check" and "we checked and it was fine"
    /// are different claims, and only one of them is reassuring.
    Skipped(String),
    Failed(String),
}

/// Run the project's own TypeScript against its own config.
///
/// `TODO.md` item 8 names this as doctor's obvious first check, and the reason
/// is item 1.5: `tsconfig`'s `exclude` shadowed its `include` for `.albedo/`, so
/// the generated `.d.ts` files never loaded — a defect invisible to every check
/// we own, and a one-line `tsc --noEmit` away from being caught.
///
/// 🪤 **Resolved as a path, never through `npx`.** The first cut shelled out to
/// `npx --no-install tsc`, which exits non-zero *and prints to stderr* when the
/// package is simply absent — so a project with no TypeScript installed was
/// reported as having type errors, and doctor failed the run. Sniffing npx's
/// wording to tell the two apart is guessing at another tool's prose. Looking for
/// the binary is a fact.
///
/// Offline by construction as a consequence, which is the behaviour a check
/// wants anyway: one that silently downloads a toolchain behaves differently in
/// CI than on a laptop.
fn check_types(project_dir: &Path) -> TypeCheck {
    if !project_dir.join("tsconfig.json").exists() {
        return TypeCheck::Skipped("no tsconfig.json in the project".to_string());
    }

    let binary = project_dir.join("node_modules").join(".bin").join(if cfg!(windows) {
        "tsc.cmd"
    } else {
        "tsc"
    });
    if !binary.exists() {
        return TypeCheck::Skipped(
            "typescript is not installed in this project (`npm i -D typescript`)".to_string(),
        );
    }

    // `.cmd` is a batch script, so on Windows it needs a shell to launch it.
    let mut command = if cfg!(windows) {
        let mut command = std::process::Command::new("cmd");
        command.arg("/C").arg(&binary).arg("--noEmit");
        command
    } else {
        let mut command = std::process::Command::new(&binary);
        command.arg("--noEmit");
        command
    };

    match command.current_dir(project_dir).output() {
        Ok(output) if output.status.success() => TypeCheck::Clean,
        Ok(output) => TypeCheck::Failed(
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
            .trim()
            .to_string(),
        ),
        Err(err) => TypeCheck::Skipped(format!("could not run {}: {err}", binary.display())),
    }
}

/// Say so when the root and entry came from a *foreign* convention.
///
/// 🔴 **Adding layout auto-discovery created a new way to be quietly wrong, and
/// this is the line that closes it.** Before detection, `albedo build` on a
/// Next.js project failed loudly. After it, the build *succeeds* — over one
/// entry module, because ALBEDO discovers routes from `<root>/routes` and knows
/// nothing about `app/**/page.tsx`. A green build over a fraction of an app is a
/// worse outcome than a refusal, so the fraction has to be stated where the
/// build is announced.
///
/// Silent for an ALBEDO layout, and silent when the author declared `root` and
/// `entry` themselves: neither is a guess, and there is nothing to disclose.
fn print_foreign_layout_notice(contract: &ResolvedDevContract) {
    let Some(layout) = contract.detected_layout.as_deref() else {
        return;
    };
    if layout.starts_with("ALBEDO") {
        return;
    }
    print_kv("layout", style_256(layout, ACCENT_SOFT, true));
    print_warn(format!(
        "{}",
        style(
            "root and entry were inferred. ALBEDO discovers routes from `routes/`, so this \
             framework's own routes are NOT in the build — run `albedo doctor` to see which.",
            "2"
        )
    ));
}

/// Route-shaped files that ALBEDO's own convention did not pick up.
///
/// 🔴 **The failure this exists to prevent is silent understatement, which for an
/// audit artefact is worse than being wrong.** Found by running doctor against a
/// real Next.js app: routes are discovered from `<root>/routes`
/// ([`ROUTES_DIRNAME`](dom_render_compiler::routing::file_based::ROUTES_DIRNAME)),
/// so an App Router project's `app/**/page.tsx` contributed nothing — and the
/// report listed one route for a three-route app while announcing "nothing to
/// report". A reader has no way to tell that from a genuinely clean bill of
/// health.
///
/// Doctor cannot know how many routes a foreign project "really" has, and must
/// not guess. What it can do is name the files that *look* like routes under
/// conventions it does not implement, and say plainly that they are not in the
/// table above. That is a fact, which is all this tool is allowed to report.
fn unattributed_route_files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    // The two conventions a foreign project is overwhelmingly likely to use.
    for convention in ["app", "pages"] {
        let dir = root.join(convention);
        if !dir.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&dir)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.path();
            let is_route_shaped = matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("tsx" | "jsx" | "ts" | "js")
            );
            let name = path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("");
            // App Router names the route file; Pages Router makes every file one.
            if is_route_shaped && (convention == "pages" || matches!(name, "page" | "route")) {
                if let Ok(relative) = path.strip_prefix(root) {
                    found.push(relative.display().to_string().replace('\\', "/"));
                }
            }
        }
    }
    found.sort();
    found
}

fn print_doctor_report(
    matrix: &dom_render_compiler::doctor::Matrix,
    findings: &[dom_render_compiler::doctor::Finding],
    types: &TypeCheck,
    unattributed: &[String],
    detected_layout: Option<&str>,
) {
    if let Some(layout) = detected_layout {
        print_section("layout");
        print_kv("detected", style_256(layout, ACCENT_SOFT, true));
        if !layout.starts_with("ALBEDO") {
            println!(
                "    {}",
                style(
                    "root and entry were inferred — nothing here was declared in an albedo config",
                    "2"
                )
            );
        }
    }

    print_section("types");
    match types {
        TypeCheck::Clean => print_ok("tsc --noEmit clean"),
        TypeCheck::Skipped(why) => print_kv("skipped", style(why, "2")),
        TypeCheck::Failed(output) => {
            print_warn("tsc --noEmit reported errors");
            for line in output.lines().take(20) {
                println!("      {}", style(line, "2"));
            }
        }
    }

    print_section("reach");
    println!(
        "    {}",
        style("what each route reads, and what decides which rows come back", "2")
    );
    println!();
    for row in &matrix.routes {
        println!(
            "    {}  {}",
            style_256(&row.route, ACCENT_SOFT, true),
            style(&format!("[{}]", row.cost.class.as_str()), "2")
        );
        if row.reads.is_empty() {
            println!("      {}", style("reads nothing — served from the manifest", "2"));
        }
        for read in &row.reads {
            // Pad the PLAIN text and colorize after. ANSI escapes have zero
            // display width, so padding a pre-styled string counts the escape
            // bytes and skews the column — the same trap `print_command` names.
            let subject = read.subject.to_string();
            println!(
                "      {}{}  {}{}  {}",
                style_256(&read.binding, MUTED, false),
                " ".repeat(20usize.saturating_sub(read.binding.chars().count())),
                subject,
                " ".repeat(26usize.saturating_sub(subject.chars().count())),
                style_256(&format!("← {}", read.key), ACCENT_DEEP, false)
            );
        }
    }

    print_section("findings");
    if findings.is_empty() {
        print_ok("nothing to report");
    }
    for finding in findings {
        print_warn(format!(
            "{}  {}",
            style_256(finding.route(), ACCENT_SOFT, true),
            finding.explain()
        ));
    }

    // The honest half. A report that lists only what it can answer reads as a
    // clean bill of health.
    //
    // 🔴 This section used to close with an unconditional *"no read can be keyed
    // by the signed-in user until AUTH item 5's P1 lands"*. P1 landed —
    // `ReachKey::Principal` is reachable and `user.id` lowers to
    // `PartitionKeySource::Identity` — so the tool was reporting its own
    // capabilities as of a date rather than the project in front of it. A
    // trust-polish surface that is wrong about the framework is worse than one
    // that says less.
    //
    // What replaces it is a fact about **this project**: whether any read is
    // keyed by the principal. In a scaffold with no auth-keyed reads that is
    // still worth saying — it is the difference between "absent because nothing
    // asked" and "absent because it cannot be expressed".
    let has_principal_read = matrix
        .routes
        .iter()
        .flat_map(|row| row.reads.iter())
        .any(|read| matches!(read.key, dom_render_compiler::doctor::ReachKey::Principal));
    if unattributed.is_empty() && has_principal_read {
        // Nothing unanswerable: say nothing rather than print an empty heading.
        println!();
        return;
    }

    print_section("not yet answerable");
    if !unattributed.is_empty() {
        print_warn(format!(
            "{} route-shaped file{} produced no row above — ALBEDO discovers routes from \
             `routes/`, not `app/`/`pages/`:",
            unattributed.len(),
            if unattributed.len() == 1 { "" } else { "s" }
        ));
        for path in unattributed.iter().take(10) {
            println!("      {}", style_256(path, MUTED, false));
        }
        if unattributed.len() > 10 {
            println!(
                "      {}",
                style(&format!("… and {} more", unattributed.len() - 10), "2")
            );
        }
        println!(
            "    {}",
            style(
                "the matrix above therefore describes only part of this project.",
                "2"
            )
        );
        println!();
    }
    if !has_principal_read {
        println!(
            "    {}",
            style(
                "no read in this project is keyed by the signed-in user. `← user.id` is",
                "2"
            )
        );
        println!(
            "    {}",
            style(
                "expressible — nothing here asks for it, so every row above is reachable",
                "2"
            )
        );
        println!(
            "    {}",
            style("by anyone who can reach the route.", "2")
        );
    }
    println!();
}

/// Compile the project just far enough to produce a manifest without
/// writing artefacts to disk. Used by `albedo budget` (which has no
/// reason to emit a bundle) and by the build/ship gate (which emits
/// artefacts first and then re-evaluates against the same manifest).
fn build_manifest_for_budget(contract: &ResolvedDevContract) -> Result<RenderManifestV2, String> {
    let components = scan_components_with_contract_policy(contract, "evaluating tier budget")?;
    if components.is_empty() {
        return Err(format!(
            "no component files found under '{}' (.js/.jsx/.ts/.tsx expected)",
            contract.root.display()
        ));
    }
    let scanner = ProjectScanner::new();
    let compiler = scanner
        .build_compiler(components)
        .with_project_root(contract.project_dir.clone());
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
    let loaded = load_budget_from_dir(&contract.project_dir).map_err(|err| err.to_string())?;
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
///   2. Bundle-byte (Phase O.3) — measures emitted wrapper bytes. Only runs when `emit_report` is
///      supplied; absent emit report falls back to source-weight only.
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
    let loaded = load_budget_from_dir(&contract.project_dir).map_err(|err| err.to_string())?;
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
        if combined.violations.len() == 1 {
            ""
        } else {
            "s"
        }
    ))
}

/// Measure the client bytes this build actually produced.
///
/// Two numbers, both checkable by hand:
///
/// * **Tier-C island bytes** — each Tier-C component lowered through
///   [`compile_client_island_module`], which is the *same* lowering
///   `RendererRuntime::build_hydration_blocks` ships to the browser, so the
///   count is the payload rather than a proxy for it. A component whose island
///   fails to compile is skipped and reported as skipped; the total is then a
///   floor, never a guess.
/// * **Runtime bytes** — the framework client a rendered page actually loads,
///   stat'd off disk. This is the cost a page pays no matter how its components
///   tiered, and omitting it is what let a "zero JS" build still transfer ~96 KB
///   of JavaScript.
///
/// The runtime set is read from the output directory rather than from
/// [`BundleEmitReport`], which carries only the assets the bundler itself wrote
/// (`wt-bootstrap.js`, `phosphor.js`) and not the ones copied out of the binary.
/// Summing the report gave 39.9 kB against a real 93.9 kB — and a number that
/// under-reports is worse than no number at all.
///
/// Deliberately *not* measured: the emitted `__albedo__/wrappers/*.mjs`. They
/// are ~610-byte re-export shims pointing at absolute source paths, nothing in
/// `assets/*.js` references them, and no browser loads one — counting them
/// would swap one meaningless number for another.
fn measure_client_bytes(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
    out_dir: &Path,
    project_root: &Path,
) -> printer::MeasuredBytes {
    use dom_render_compiler::bundler::client_npm::{build_client_npm_graph, ClientIsland};
    use dom_render_compiler::runtime::quickjs_engine::{
        compile_client_island_module_with_npm, ClientNpmBindings,
    };

    // Tier C · Phase 2 — the same graph the serve path builds, from the same
    // function, so the measurement is the payload rather than a proxy for it.
    // Building it here (rather than measuring islands without npm and adding a
    // guess) is what keeps `albedo build`'s number equal to what a browser
    // downloads.
    let islands: Vec<ClientIsland<'_>> = manifest
        .components
        .iter()
        .filter(|component| component.tier == Tier::C)
        .filter_map(|component| {
            module_sources
                .get(&component.module_path)
                .map(|source| ClientIsland {
                    module_path: component.module_path.as_str(),
                    source: source.as_str(),
                })
        })
        .collect();
    let npm_graph = build_client_npm_graph(project_root, &islands);
    let empty_npm_bindings = ClientNpmBindings::default();

    let mut tier_c_island_bytes = 0u64;
    let mut tier_c_measured = 0usize;
    // Why every failure is now recorded rather than skipped: this loop used to
    // be `if let Ok(iife) = …`, which threw away a `RuntimeError` that names
    // the exact cause. A Tier-C component that fails here ships an empty
    // placeholder and never hydrates, so dropping the reason turned a
    // diagnosable build error into a page that is quietly missing a component.
    let mut tier_c_failures = Vec::new();

    for component in &manifest.components {
        if component.tier != Tier::C {
            continue;
        }
        let Some(source) = module_sources.get(&component.module_path) else {
            // Also worth saying out loud: no source means nothing to compile,
            // which is the same outcome for the reader.
            tier_c_failures.push((
                component.name.clone(),
                format!(
                    "no module source recorded for '{}'",
                    component.module_path
                ),
            ));
            continue;
        };
        match compile_client_island_module_with_npm(
            &component.module_path,
            source,
            component.id,
            module_sources,
            npm_graph
                .bindings_for(&component.module_path)
                .unwrap_or(&empty_npm_bindings),
        ) {
            Ok(iife) => {
                tier_c_island_bytes = tier_c_island_bytes.saturating_add(iife.len() as u64);
                tier_c_measured += 1;
            }
            Err(err) => tier_c_failures.push((component.name.clone(), err.to_string())),
        }
    }

    // Exactly the three scripts `manifest::builder`'s shell emits a tag for
    // (`runtime.js` and `link-forms.js` unconditionally, `wt-bootstrap.js` on a
    // live route). Not the whole `_albedo/` directory: `bincode.js`,
    // `phosphor.js` and `hydration.js` are imported on demand, so summing the
    // folder would overstate by ~47 kB in the other direction.
    const SHELL_RUNTIME_ASSETS: [&str; 3] =
        ["runtime.js", "link-forms.js", "wt-bootstrap.js"];
    let runtime_bytes = SHELL_RUNTIME_ASSETS
        .iter()
        .filter_map(|name| std::fs::metadata(out_dir.join("_albedo").join(name)).ok())
        .map(|meta| meta.len())
        .sum();

    // A package that would not bundle is a Tier-C failure like any other: the
    // island imports it, so the island will not hydrate. Reported through the
    // same channel the compile failures use rather than a second list nobody
    // reads.
    for failure in npm_graph.failures() {
        tier_c_failures.push((
            failure.module_path.clone(),
            format!("npm '{}': {}", failure.specifier, failure.reason),
        ));
    }

    printer::MeasuredBytes {
        tier_c_island_bytes,
        tier_c_measured,
        runtime_bytes,
        tier_c_failures,
        npm_chunks: npm_graph
            .chunks()
            .iter()
            .map(|chunk| (chunk.package.clone(), chunk.bytes() as u64))
            .collect(),
    }
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

/// The project this executable carries, if it carries one.
///
/// `Ok(None)` is the ordinary `albedo` CLI and by far the common case. An
/// error means a payload is present but damaged, which must be said rather
/// than degraded into "no project here" — see `bundle_payload::read_index`.
fn shipped_project() -> Result<Option<(PathBuf, albedo_server::bundle_payload::PayloadIndex)>, String>
{
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        // Not being able to find our own path is not a reason to refuse to run
        // as a CLI; it only means we cannot be a shipped app.
        Err(_) => return Ok(None),
    };
    let mut file = match std::fs::File::open(&exe) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let Some(index) = albedo_server::bundle_payload::read_index(&mut file)? else {
        return Ok(None);
    };
    Ok(Some((exe, index)))
}

/// Serve the project appended to this executable.
///
/// Unpacks beside the binary on first run, then delegates to the ordinary
/// serve path — the same code every other deployment uses, so a shipped app
/// cannot drift into being a second, less-tested server.
fn run_shipped_app(
    (exe, index): (PathBuf, albedo_server::bundle_payload::PayloadIndex),
    user_args: &[String],
) -> Result<(), String> {
    let mut file = std::fs::File::open(&exe)
        .map_err(|err| format!("failed to open this executable to read its payload: {err}"))?;
    let project =
        albedo_server::bundle_payload::prepare_unpacked_project(&mut file, &index, &exe)?;

    // 🔑 **The database belongs beside the executable, not inside the unpacked
    // project.** The unpack directory is keyed by build id, so shipping a new
    // build makes a new directory — a database inside it would be silently
    // left behind with every deploy, which is data loss that looks like a
    // successful release. Only a default: an operator who set the variable, or
    // mounted a volume, has already said where it goes.
    if std::env::var(albedo_server::forge_db_path::FORGE_DB_ENV).is_err() {
        let beside = exe
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(albedo_server::forge_db_path::DEFAULT_FORGE_DB_FILENAME);
        std::env::set_var(
            albedo_server::forge_db_path::FORGE_DB_ENV,
            beside.as_os_str(),
        );
    }

    // The positional project directory is ours to decide; everything else the
    // operator typed (`--host`, `--port`) still applies.
    let mut forwarded: Vec<String> = vec![project.display().to_string()];
    forwarded.extend(
        user_args
            .iter()
            .filter(|arg| !matches!(arg.as_str(), "serve" | "run"))
            .cloned(),
    );

    run_serve_command(&forwarded)
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
        ShipTarget::Binary => configure_ship_binary(&contract),
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

/// `albedo ship --target binary` — write one file that is runtime plus app.
///
/// The output is this executable, byte for byte, with the project appended.
/// It needs no Rust toolchain on the machine that produces it and no source
/// tree on the machine that runs it: copy the file, run it, the site is up.
fn configure_ship_binary(contract: &ResolvedDevContract) -> Result<(), String> {
    use albedo_server::bundle_payload;

    print_section("binary");

    let exe = std::env::current_exe()
        .map_err(|err| format!("failed to locate the albedo runtime: {err}"))?;

    // The build ran a moment ago in `run_ship_command`, so the manifest is on
    // disk and current. Its build id names the payload, which is what lets the
    // runtime tell one shipped build from another.
    let manifest_path = contract
        .project_dir
        .join(".albedo")
        .join("dist")
        .join("render-manifest.v2.json");
    let build_id = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|manifest| {
            manifest
                .get("build_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .ok_or_else(|| {
            format!(
                "no build id in '{}' — run `albedo build` first",
                manifest_path.display()
            )
        })?;

    let files = bundle_payload::collect_project_files(&contract.project_dir)?;
    if files.is_empty() {
        return Err(format!(
            "nothing to ship from '{}'",
            contract.project_dir.display()
        ));
    }

    let name = contract
        .project_dir
        .file_name()
        .map_or_else(|| "app".to_string(), |name| name.to_string_lossy().to_string());
    let extension = if cfg!(windows) { ".exe" } else { "" };
    let output = contract
        .project_dir
        .join(format!("{name}.albedo-bin{extension}"));

    // Written to a temporary sibling and renamed, so an interrupted ship never
    // leaves a half-written file that looks runnable.
    let staging = output.with_extension("partial");
    {
        let mut host = std::fs::File::open(&exe)
            .map_err(|err| format!("failed to read the albedo runtime: {err}"))?;
        let mut out = std::fs::File::create(&staging)
            .map_err(|err| format!("failed to create '{}': {err}", staging.display()))?;
        bundle_payload::write_payload(&mut out, &mut host, &build_id, &files)?;
    }
    std::fs::rename(&staging, &output)
        .map_err(|err| format!("failed to finish '{}': {err}", output.display()))?;

    // On Unix the copy has to be executable; `create` gives 0644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&output)
            .map_err(|err| format!("failed to stat '{}': {err}", output.display()))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&output, perms)
            .map_err(|err| format!("failed to make '{}' executable: {err}", output.display()))?;
    }

    let runtime_bytes = std::fs::metadata(&exe).map(|meta| meta.len()).unwrap_or(0);
    let total = std::fs::metadata(&output)
        .map(|meta| meta.len())
        .unwrap_or(0);

    print_ok("one file, runtime and app");
    print_kv("output", output.display());
    print_kv("files", files.len());
    print_kv(
        "size",
        format!(
            "{:.1} MB ({:.1} MB runtime + {:.1} MB app)",
            total as f64 / 1_048_576.0,
            runtime_bytes as f64 / 1_048_576.0,
            total.saturating_sub(runtime_bytes) as f64 / 1_048_576.0
        ),
    );
    print_kv("build", build_id.as_str());
    println!();
    println!(
        "    {}",
        style("copy it to any box and run it — no Node, no source tree, no node_modules.", "2")
    );
    println!(
        "    {}",
        style(
            "it unpacks beside itself on first run; forge.db is created next to the binary.",
            "2"
        )
    );

    Ok(())
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
        "5" | "binary" | "bin" | "single" => Ok(ShipTarget::Binary),
        other => Err(format!(
            "unknown ship target '{other}'. Supported targets: binary, docker, fly, static."
        )),
    }
}

fn prompt_ship_target() -> Result<ShipTarget, String> {
    print_section("pick a target");
    println!(
        "    {} binary     {}",
        style_256("5", ACCENT_SOFT, true),
        style("one file: this runtime + your app (recommended)", "2")
    );
    println!(
        "    {} docker     {}",
        style_256("2", ACCENT_SOFT, true),
        style("multi-stage binary image", "2")
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
/// The linux/amd64 `albedo` the image needs, by convention, in the project.
const LINUX_RUNTIME_FILENAME: &str = "albedo-linux-amd64";

/// Does this file start with the ELF magic?
///
/// Cheaper and more honest than trusting a filename: the whole failure this
/// guards against is a Windows `albedo.exe` sitting where a Linux one is
/// expected, which a name check would happily accept.
fn is_linux_elf(path: &Path) -> bool {
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == [0x7f, b'E', b'L', b'F']
}

/// Find the linux/amd64 runtime the image will carry, or say why we cannot.
///
/// 🔴 **Found 2026-09-06 by running `docker build` for the first time.** The
/// builder stage ran `cargo build --release --bin albedo` against the build
/// context — but the context is **the user's app**, which has no `Cargo.toml`
/// and no Rust source. It failed in 0.17 s with
/// `could not find Cargo.toml in /workspace`. That line was in the template
/// before item 13.4 touched it too, so **`ship --target docker` had never
/// produced a working image in any version.** It read plausibly because it was
/// written as if the context were the ALBEDO repo.
///
/// 🔑 **The image does not need a Rust toolchain — it needs one Linux
/// executable.** `ship --target binary` appends the project to the *running*
/// executable, so given a linux/amd64 `albedo` inside the image, the builder
/// stage is `debian-slim` and does no compiling at all. That is what item 13.4
/// actually wanted; the missing piece was never the template, it was the
/// binary.
///
/// 🪤 **Refusing here is deliberate, and follows `--target vercel`.** Writing a
/// Dockerfile that cannot build is worse than writing none: the user finds out
/// several minutes into `docker build`, after an npm install, with a Rust error
/// that points at nothing they own.
fn resolve_linux_runtime(project_dir: &Path) -> Result<PathBuf, String> {
    let supplied = project_dir.join(LINUX_RUNTIME_FILENAME);
    if supplied.exists() {
        return if is_linux_elf(&supplied) {
            Ok(supplied)
        } else {
            Err(format!(
                "'{}' is not a linux/amd64 executable (no ELF header). The image runs Linux, \
                 so a Windows or macOS `albedo` cannot be the runtime inside it.",
                supplied.display()
            ))
        };
    }

    // Shipping *from* Linux: the running executable is already the right thing.
    let exe = std::env::current_exe()
        .map_err(|err| format!("failed to locate the albedo runtime: {err}"))?;
    if is_linux_elf(&exe) {
        std::fs::copy(&exe, &supplied).map_err(|err| {
            format!("failed to stage '{}': {err}", supplied.display())
        })?;
        return Ok(supplied);
    }

    Err(format!(
        "`ship --target docker` needs a linux/amd64 `albedo` to put inside the image, and this \
         one is not Linux.\n\n    \
         `ship --target binary` appends your app to the *running* executable, so a Windows or \
         macOS build cannot produce a Linux container runtime — and the image itself does not \
         build one, because your project has no Rust source in it.\n\n    \
         Put a linux/amd64 `albedo` at `{}` and run this again. Any of these produce one:\n      \
         · in the albedo repo: `docker build -f Dockerfile.linux --target export \
--output type=local,dest=./dist-linux .`\n      \
         · `cargo build --release -p albedo-server --bin albedo` on a Linux box or in WSL\n      \
         · a published linux/amd64 release binary, once one exists\n\n    \
         Until then `ship --target binary` on a Linux host is the working path.",
        supplied.display()
    ))
}

/// Warn when the staged Linux runtime is a different albedo than this one.
///
/// 🔴 **Walked into this within minutes of building the mechanism.** The image
/// embeds a *pinned* runtime, so a change to albedo is invisible to
/// `docker build` until `albedo-linux-amd64` is re-staged — the container goes
/// on running old code, and the symptom is whatever that old code did. Here it
/// was a boot refusal that had already been fixed.
///
/// 🪤 **Checked by version string, not by mtime.** `cp` does not preserve mtime,
/// so a freshly copied stale runtime looks new — the heuristic would be wrong in
/// exactly the situation it exists for. Searching for this build's version is
/// exact: a runtime of another version cannot contain the string.
///
/// A warning, not a refusal: during development of albedo itself the version is
/// unchanged while the code moves, so this cannot see every drift, and a check
/// that is silent half the time must not be allowed to block a build.
fn warn_if_staged_runtime_is_a_different_version(runtime: &Path) {
    let Ok(bytes) = std::fs::read(runtime) else {
        return;
    };
    let version = env!("CARGO_PKG_VERSION").as_bytes();
    let matches = bytes
        .windows(version.len())
        .any(|window| window == version);
    if !matches {
        print_warn(format!(
            "'{}' does not look like albedo {} — the image would run a different runtime than \
             this one. Re-stage it (see Dockerfile.linux in the albedo repo) unless you meant to \
             pin an older build.",
            runtime.display(),
            env!("CARGO_PKG_VERSION")
        ));
    }
}

fn configure_ship_docker(contract: &ResolvedDevContract) -> Result<(), String> {
    // Refuse BEFORE writing anything. A Dockerfile on disk that cannot build is
    // a worse outcome than no Dockerfile.
    let runtime = resolve_linux_runtime(&contract.project_dir)?;
    warn_if_staged_runtime_is_a_different_version(&runtime);
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
    print_kv("runtime", runtime.display());
    print_kv(
        "build",
        style_256("docker build -t albedo-app .", ACCENT_SOFT, true),
    );
    // 🔑 The printed command must WORK. It did not: for any app with auth —
    // which the scaffold has — `docker run -p 3000:3000 albedo-app` exits 1 at
    // boot, because a container binds 0.0.0.0 and that is what the plain-HTTP
    // auth refusal stops. Found 2026-09-06 by running the line we print.
    print_kv(
        "run",
        style_256(
            "docker run -p 3000:3000 -v albedo-data:/data \\\n           \
             -e ALBEDO_PUBLIC_ORIGIN=http://localhost:3000 albedo-app",
            ACCENT_SOFT,
            true,
        ),
    );
    // 🔴 Found 2026-09-06 by running the printed command: for any app with auth
    // configured — which the scaffold has — that `run` line **refuses to boot**.
    // A container must bind 0.0.0.0, and binding 0.0.0.0 over plain HTTP with
    // auth configured is exactly what `tls::refuse_plain_http_auth` exists to
    // stop, because `__Host-` cookies are never stored over HTTP and signing in
    // would silently do nothing. The refusal is correct; printing a command
    // that trips it is not.
    println!();
    println!(
        "    {}",
        style(
            "in production, say how users really reach you — an app with auth needs it:",
            "2"
        )
    );
    println!(
        "      {}",
        style_256(
            "-e ALBEDO_PUBLIC_ORIGIN=https://app.example.com",
            ACCENT_SOFT,
            true
        )
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
    r#"# Emitted by `albedo ship --target docker`.
#
# Stage 1 installs npm dependencies, stage 2 builds THE ONE FILE, and the
# runtime stage is a slim base with that one file in it. Nothing here compiles
# Rust: `albedo-linux-amd64`, written beside this file by
# `albedo ship --target docker`, is the runtime the builder uses.
#
# --- corrected 2026-09-02 -----------------------------------------------
# This template used to copy only `.albedo/dist` + `public` and run
# `albedo serve --dir dist`, which is the STATIC FILE SERVER. The container
# served markup and nothing else: no FORGE, no actions, no per-request
# render, no auth -- for a framework whose claim is that the compiler emits
# your backend. The runtime stage also carried no source tree, so
# project-mode `serve` could not have worked there even with the CMD fixed,
# and `.dockerignore` excluded `node_modules`, so any app with an npm
# dependency failed in the builder too. Three faults, one cause: the target
# was written for a static-site model and never revisited when the framework
# grew a backend and npm support.
#
# --- corrected 2026-09-06 (item 13.4) ------------------------------------
# The fix above left this target contradicting `ship --target binary`. That
# target's whole claim is "one file -- copy it to any box and run it"; this
# one still assembled a runtime stage by hand out of four COPY lines
# (`src/`, `albedo.config.*`, `public/`, `npm-server-bundles.json`), which is
# the same list `bundle_payload::should_ship` already computes. Two
# definitions of "what a deployment consists of", drifting independently --
# and the Dockerfile's copy had already drifted once, which is why the block
# above exists.
#
# There is now ONE definition. The builder runs `ship --target binary`, and
# the runtime stage copies its output. Anything `should_ship` starts or stops
# including follows this image automatically.
#
# 🪤 The builder stage cannot be removed. `ship --target binary` appends the
# project to *the running executable*, so the file it produces is for the
# host that produced it -- a Windows or macOS dev box cannot emit a Linux
# ELF this way. Compiling inside the image is what makes the output
# linux/amd64 regardless of where `albedo ship` was typed. Use
# `--platform=$BUILDPLATFORM` and a cross toolchain only if you need the
# builder to run natively on an arm64 machine.
# ------------------------------------------------------------------------

FROM node:22-bookworm AS deps
WORKDIR /workspace
# `albedo build` resolves bare specifiers out of `node_modules`, so they are
# installed here rather than copied from the host: a lockfile install is
# reproducible, and the host's tree may be built for another platform.
COPY package.json package-lock.json* ./
RUN if [ -f package-lock.json ]; then npm ci; \
    elif [ -f package.json ]; then npm install; \
    else mkdir -p node_modules; fi

# --- corrected 2026-09-06, second pass (13.4c) ---------------------------
# This stage was `FROM rust:1-bookworm` and ran `cargo build --release --bin
# albedo`. It could never have worked: the build context is YOUR APP, which has
# no `Cargo.toml` and no Rust source, so it failed in 0.17 s with
# `could not find Cargo.toml in /workspace`. The line predates the 13.4 rewrite,
# so this target had never built an image in any version -- it was written as
# though the context were the ALBEDO repo.
#
# 🔑 The image never needed a Rust toolchain. It needs ONE Linux executable.
# `albedo ship --target binary` appends your project to the *running* albedo, so
# with a linux/amd64 albedo in the image this stage is a slim base that compiles
# nothing. `albedo ship --target docker` puts that binary next to this file.
# ------------------------------------------------------------------------
FROM debian:bookworm-slim AS builder
WORKDIR /workspace
COPY albedo-linux-amd64 /usr/local/bin/albedo
RUN chmod +x /usr/local/bin/albedo
COPY . .
COPY --from=deps /workspace/node_modules ./node_modules
# Fail HERE, not at container start. `ship` runs the same production build
# `albedo build` does, so preflight still refuses the defects that would
# otherwise serve HTTP 200 with something missing -- an unresolvable import,
# a topic nothing writes, a route with no component to render.
#
# The output is named for the project directory, so it is moved to a fixed
# path rather than guessed at in the COPY below.
RUN albedo ship --target binary \
    && mkdir -p /out \
    && mv /workspace/*.albedo-bin /out/albedo-app \
    && chmod +x /out/albedo-app

FROM debian:bookworm-slim AS runtime
# ca-certificates for outbound TLS (APERTURE fetches, ACME); wget for the
# healthcheck below. Nothing else -- no Node, no source tree, no toolchain.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /out/albedo-app /app/albedo-app

ENV ALBEDO_SERVER_HOST=0.0.0.0
ENV ALBEDO_SERVER_PORT=3000
EXPOSE 3000

# FORGE's database, named explicitly rather than left to the default.
#
# 🪤 A shipped binary defaults its database to a path BESIDE ITSELF, and
# unpacks its project into a directory keyed by build id. Left implicit, the
# database would land in `/app` next to a per-build unpack directory, inside
# the container's writable layer -- so `docker run` twice is two empty
# databases, which for an app with real data is data loss that looks like a
# successful release. `VOLUME` makes docker create a real volume even when
# the operator forgets `-v`; name it to keep it across `docker rm`:
#
#   docker run -v albedo-data:/data -p 3000:3000 <image>
ENV ALBEDO_FORGE_DB=/data/forge.db
VOLUME /data

# 🪤 `shutdown_timeout_ms` bounds the drain that `docker stop` starts. Docker
# sends SIGTERM and then SIGKILLs after its own grace period (10s default),
# so keep this under `docker stop -t`.
# 📏 Measured 2026-09-06: `docker stop` returns in 0s with exit code 0 — the
# process handles SIGTERM and drains, rather than being killed at the 10s mark.
ENV ALBEDO_SHUTDOWN_TIMEOUT_MS=5000

# 🔴 ALBEDO_PUBLIC_ORIGIN — an app with auth will not boot without it.
#
# A container has to bind 0.0.0.0, and binding 0.0.0.0 over plain HTTP with auth
# configured is refused on purpose: session cookies carry the `__Host-` prefix,
# browsers will not store those over HTTP, and signing in would silently do
# nothing forever with no error anywhere.
#
# 🔑 Inside a container the bind address cannot answer that question. `-p
# 127.0.0.1:3000:3000`, an ingress terminating TLS, and a port open to the
# internet all look identical from in here. So say how users actually reach you:
#
#   local testing:   -e ALBEDO_PUBLIC_ORIGIN=http://localhost:3000
#   production:      -e ALBEDO_PUBLIC_ORIGIN=https://app.example.com
#
# Deliberately NOT defaulted in this image. A default would be a lie the moment
# the image is deployed anywhere real, and it would disarm the one check that
# catches a login which silently never works.
#
# Serving HTTPS from the container itself is the alternative:
#   -v /certs:/certs -e ALBEDO_TLS_CERT=/certs/fullchain.pem \
#                    -e ALBEDO_TLS_KEY=/certs/key.pem
#
# 🪤 ALBEDO_TRUSTED_PROXIES also satisfies the check, but do not reach for it to
# silence this. It decides whose `X-Forwarded-For` the rate limiter believes —
# a security setting, not a checkbox.

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD wget -qO- "http://127.0.0.1:${ALBEDO_SERVER_PORT}/" >/dev/null 2>&1 || exit 1

# 🔑 The shipped binary is NOT a CLI -- it carries a payload, so it serves
# that payload and ignores subcommands (`src/bin/albedo.rs`, `run_shipped_app`).
# `--host`/`--port` still apply. Exec form via `sh -c` so the ENV above is
# expanded, and `exec` so the binary is PID 1 and receives SIGTERM directly
# rather than through a shell that would not forward it.
CMD ["sh", "-c", "exec /app/albedo-app --host ${ALBEDO_SERVER_HOST} --port ${ALBEDO_SERVER_PORT}"]
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
# A previously shipped binary is the runtime plus a whole copy of the app.
# Sending it into the build context would upload it and then have the builder
# overwrite it -- `bundle_payload::should_ship` already excludes both of these
# from the payload for the same reason.
*.albedo-bin
*.albedo-bin.exe
.albedo-app-*
# The database and its WAL/SHM sidecars. A shipped WAL is a torn half
# transaction, and the container mounts its own volume anyway.
forge.db
forge.db-*
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

# 🔑 `auto_stop_machines` below means the platform stops this machine on every
# idle cycle, not just on deploy — so the stop signal is on the hot path, and a
# machine that dies instantly cuts whatever requests were in flight. Stated
# explicitly rather than left to the platform default. `kill_timeout` is the
# outer bound on ALBEDO_SHUTDOWN_TIMEOUT_MS below: Fly SIGKILLs after it, so
# the drain has to finish first.
#
# 🪤 These are TOP-LEVEL keys and must stay above the first `[table]` header —
# a bare key after `[mounts]` is `mounts.kill_signal`, which Fly does not read.
kill_signal = "SIGTERM"
kill_timeout = "10s"

[build]
  dockerfile = "Dockerfile"

[env]
  ALBEDO_SERVER_HOST = "0.0.0.0"
  ALBEDO_SERVER_PORT = "3000"
  ALBEDO_FORGE_DB = "/data/forge.db"
  ALBEDO_SHUTDOWN_TIMEOUT_MS = "5000"
  # 🔑 Unlike a bare container, Fly's topology is KNOWN: `force_https` below
  # means the edge terminates TLS and every app gets `<app>.fly.dev`. So the
  # origin browsers use is not a guess here, and an app with auth boots without
  # the operator having to work it out. Change this when you attach a custom
  # domain — it is what decides whether the `__Host-` session cookie is stored.
  ALBEDO_PUBLIC_ORIGIN = "https://{app_name}.fly.dev"

# 🪤 Without this the database lives in the machine's ephemeral filesystem and
# every deploy starts empty. `fly volumes create albedo_data --size 1` first.
[mounts]
  source = "albedo_data"
  destination = "/data"

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

    // 🔴 A bare positional used to land here too, and it silently changed
    // what this command IS: `albedo build .` compiled the project, then
    // `albedo serve .` served the project directory as **static files**, so
    // every route 404'd — including the scaffold's own. Found 2026-09-02 by
    // building a real app and pointing a browser at it.
    //
    // 🔑 The positional now selects the PROJECT, which is what it already
    // means on `dev`, `build`, `ship` and `doctor` — the on-ramp audit fixed it
    // there and left `serve`, the most-used of the five, meaning the opposite.
    // One spelling, one meaning, across every command that takes it.
    //
    // Nothing is lost: `albedo files <dir>` is a first-class advertised command
    // ("serve static files from a folder") and is exactly what the old
    // behaviour was. `--dir` is kept as the explicit opt-in because it is
    // load-bearing — `albedo ship --target docker` emits a `CMD` that uses it —
    // and because nobody types `--dir` by accident, whereas `.` is typed
    // constantly.
    //
    // The walk still binds `--host`/`--port` values to their flag so a port
    // like `3139` is never read as anything else.
    let explicit_files_dir = {
        let mut explicit_dir = false;
        let mut idx = 0;
        while idx < raw_args.len() {
            let arg = &raw_args[idx];
            match arg.as_str() {
                "--dir" => {
                    explicit_dir = true;
                    break;
                }
                // 🪤 Every value-taking flag must be skipped *with its value*
                // here. Miss one and `--domain app.example.com` leaves
                // `app.example.com` looking like a bare positional, which this
                // walk would read as a directory to serve as static files.
                "--host" | "--port" | "--tls-cert" | "--tls-key" | "--domain"
                | "--acme-contact" | "--acme-cache" => {
                    idx += 2; // skip the flag's value
                    continue;
                }
                _ => {}
            }
            idx += 1;
        }
        explicit_dir
    };
    if explicit_files_dir {
        // `--dir <dir>` is the explicit "serve this folder as files" request,
        // identical to `albedo files <dir>`.
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
    print_foreign_layout_notice(&contract);
    print_kv("mode", "production (build + serve)");
    println!();
    // The tier report is the dashboard's "tiers" panel — the same numbers the
    // `dev` lane keeps. Dropping it here is what left `serve` showing
    // "no build report yet" for the whole session: the build produced the
    // classification and the only consumer never received it.
    let tier_report = run_prod_build(&contract)?;

    // `resolve_dev_contract` already absorbed `--host` / `--port` from
    // `raw_args`. Pull the bind address back out for the banner.
    let serve_options = parse_serve_args(raw_args)?;
    contract.server.host = serve_options.host.clone();
    contract.server.port = serve_options.port;

    boot_and_run_production_server(&contract, Some(tier_report), serve_options.tls)
}

/// Phase P · Stream A — turn a built `ResolvedDevContract` into a
/// running [`albedo_server::AlbedoServer`]. The Tokio runtime is
/// spun up here (not at `main`) so the dev path stays sync and only
/// pays the runtime cost on `albedo serve`.
fn boot_and_run_production_server(
    contract: &ResolvedDevContract,
    report: Option<TierReport>,
    tls: albedo_server::tls::TlsSettings,
) -> Result<(), String> {
    use albedo_server::{boot_production_server, ProductionServerOptions};

    let mut opts = ProductionServerOptions::from_contract(contract);
    opts.tls = tls;
    let server = boot_production_server(&opts).map_err(|err| {
        // 🔴 The hint used to be unconditional. `albedo serve` builds first, so
        // by the time this runs the build has already succeeded — and every
        // config or code error (a bad `partition_by`, a topic nothing writes)
        // arrived carrying the advice to do the one thing that had just worked.
        // A wrong hint on a correct diagnosis costs more than no hint.
        let built = opts.dist_dir.join("bundle-plan.json").is_file();
        if built {
            format!("failed to boot production server: {err}")
        } else {
            format!(
                "failed to boot production server: {err}\n\
                 hint: no build output at {} — run `albedo build` first",
                opts.dist_dir.display()
            )
        }
    })?;

    let url = format!("http://{}:{}", contract.server.host, contract.server.port);

    // `serve` is long-running and holds the same live state `dev` does — minus
    // the watcher, so its event log stays quiet. Same dashboard, same fallback:
    // piped or `ALBEDO_NO_TUI=1` and it prints exactly as before.
    if tui::available() {
        let (dash_tx, dash_rx) = mpsc::channel::<tui::dev::DashEvent>();
        return run_dev_dashboard(
            tui::dev::Mode::Serve,
            url,
            contract.project_dir.display().to_string(),
            report,
            server,
            // `serve` has no `--open`; it is a production run, not an on-ramp.
            false,
            dash_tx,
            dash_rx,
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start tokio runtime: {err}"))?;

    // The banner is the server's own readiness signal, not ours to guess at.
    // Printed from `run_with_ready`, it cannot claim a server that failed to
    // boot — and a startup error now reaches the user with nothing above it
    // contradicting it.
    let banner_url = format!("http://{}:{}", contract.server.host, contract.server.port);
    let banner_dir = contract.project_dir.display().to_string();

    runtime
        .block_on(server.run_with_ready(move |report| {
            print_boot_report(report);
            print_ok(format!(
                "serving · {}",
                style_256(&banner_url, ACCENT_SOFT, true)
            ));
            println!(
                "    {} {}",
                style_256("·", MUTED, false),
                style(&banner_dir, "2")
            );
            println!();
            println!("    {}  stop the server", style_256("ctrl+c", MUTED, true));
            println!();
        }))
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
    println!("    {}  stop the server", style_256("ctrl+c", MUTED, true));
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
    let mut tls = albedo_server::tls::TlsSettings::from_env();

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
            "--tls-cert" => {
                idx += 1;
                tls.cert_path = Some(
                    raw_args
                        .get(idx)
                        .ok_or_else(|| "missing value after --tls-cert".to_string())?
                        .clone(),
                );
            }
            "--tls-key" => {
                idx += 1;
                tls.key_path = Some(
                    raw_args
                        .get(idx)
                        .ok_or_else(|| "missing value after --tls-key".to_string())?
                        .clone(),
                );
            }
            // Repeatable, and comma-separated, because one certificate
            // routinely covers an apex and its `www`.
            "--domain" => {
                idx += 1;
                let value = raw_args
                    .get(idx)
                    .ok_or_else(|| "missing value after --domain".to_string())?;
                tls.domains
                    .extend(albedo_server::tls::split_domains(value));
            }
            "--acme-contact" => {
                idx += 1;
                tls.acme_contact = Some(
                    raw_args
                        .get(idx)
                        .ok_or_else(|| "missing value after --acme-contact".to_string())?
                        .clone(),
                );
            }
            "--acme-cache" => {
                idx += 1;
                tls.acme_cache = Some(
                    raw_args
                        .get(idx)
                        .ok_or_else(|| "missing value after --acme-cache".to_string())?
                        .clone(),
                );
            }
            "--acme-staging" => {
                tls.acme_staging = true;
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

    Ok(ServeOptions {
        dir,
        host,
        port,
        tls,
    })
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
    print_foreign_layout_notice(&contract);
    print_kv(
        "server",
        format!("http://{}:{}", contract.server.host, contract.server.port),
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

    // 1. Build the dist the production pipeline serves from. The tier report it
    //    produces is the dashboard's opening view — the same numbers
    //    `albedo build` prints, kept instead of discarded.
    let tier_report = run_prod_build(&contract)?;

    // Decide up front whether this session gets a dashboard. Everything below
    // branches on it exactly once: with a dashboard the request timings and the
    // watcher's results are routed into it, without one they keep printing as
    // they always have.
    let dashboard = tui::available();
    let (dash_tx, dash_rx) = mpsc::channel::<tui::dev::DashEvent>();

    // 2. Boot the production server with dev mode on (overlay + HMR endpoints + the shell
    //    dev-script injection from `StreamingAppState::with_dev_mode`).
    let mut opts = ProductionServerOptions::from_contract(&contract);
    opts.dev_mode = true;
    let server =
        boot_production_server(&opts).map_err(|err| format!("failed to boot dev server: {err}"))?;

    // 3. Spawn the watch → rebuild → hot-swap loop. The reload handle shares the running server's
    //    world slot, so a swap is live for the next request.
    if let Some(reload) = server.dev_reload_handle() {
        let watch_contract = contract.clone();
        let watch_opts = opts.clone();
        let debounce = Duration::from_millis(contract.watch.debounce_ms.max(1));
        let watch_tx = dashboard.then(|| dash_tx.clone());
        std::thread::spawn(move || {
            dev_watch_and_reload(watch_contract, watch_opts, reload, debounce, watch_tx);
        });
    }

    let addr = format!("{}:{}", contract.server.host, contract.server.port);
    let url = format!("http://{addr}");

    if dashboard {
        return run_dev_dashboard(
            tui::dev::Mode::Dev,
            url,
            contract.project_dir.display().to_string(),
            Some(tier_report),
            server,
            contract.open,
            dash_tx,
            dash_rx,
        );
    }

    // 4. Run the production server on a fresh multi-thread runtime, same as `albedo serve` (the dev
    //    path stays sync until this point).
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start tokio runtime: {err}"))?;

    // Banner *and* browser both wait on the server's own readiness signal.
    // Opening a tab at a URL that never bound is the same lie the banner was
    // telling, just harder to notice.
    let banner_url = url.clone();
    let open_browser = contract.open;
    runtime
        .block_on(server.run_with_ready(move |report| {
            println!();
            print_boot_report(report);
            print_ok(format!(
                "dev · {}",
                style_256(&banner_url, ACCENT_SOFT, true)
            ));
            println!(
                "    {} same pipeline as `albedo serve` · overlay + hot reload on",
                style_256("·", MUTED, false)
            );
            println!();
            println!("    {}  stop the server", style_256("ctrl+c", MUTED, true));
            println!();

            if open_browser {
                if let Err(err) = try_open_browser(banner_url.as_str()) {
                    print_warn(format!("failed to open browser automatically: {err}"));
                }
            }
        }))
        .map_err(|err| format!("dev server runtime error: {err}"))
}

/// Own the main thread with the dashboard while the server runs behind it.
///
/// The order matters. The request sink is installed **before** the server is
/// spawned, so no timing can reach stdout and tear the first frame; the terminal
/// is claimed after that, so a boot failure still reports on a normal screen.
/// When the dashboard returns — the user pressed `q` — the guard restores the
/// terminal on the way out and the runtime is dropped, which stops the server.
///
/// ## Boot is awaited, not assumed
///
/// This blocks on the server's readiness signal before claiming the terminal.
/// It has to: the dashboard is a full-screen alternate buffer showing a URL, so
/// painting it before the listener binds turns *any* startup failure — a busy
/// port, an unopenable `forge.db`, a schema the database disagrees with — into
/// a confident dashboard for a server that does not exist. This lane runs
/// whenever stdout is a TTY, which is every interactive user, so a message that
/// only survives the piped path is a message nobody reads.
fn run_dev_dashboard(
    mode: tui::dev::Mode,
    url: String,
    project: String,
    report: Option<TierReport>,
    server: albedo_server::AlbedoServer,
    open_browser: bool,
    dash_tx: mpsc::Sender<tui::dev::DashEvent>,
    dash_rx: mpsc::Receiver<tui::dev::DashEvent>,
) -> Result<(), String> {
    // Bridge the server's timing records onto the dashboard channel. A thread
    // rather than a shared type, so `albedo-server` stays unaware the CLI has a
    // UI at all — it publishes measurements and does not care who listens.
    let (req_tx, req_rx) = mpsc::channel::<albedo_server::timing::RequestRecord>();
    if albedo_server::timing::install_request_sink(req_tx) {
        let bridge_tx = dash_tx.clone();
        std::thread::spawn(move || {
            for record in req_rx {
                if bridge_tx
                    .send(tui::dev::DashEvent::Request {
                        method: record.method,
                        path: record.path,
                        elapsed: record.elapsed,
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    // The one line the print path opens with, kept rather than lost to the
    // dashboard: it tells the reader that `dev` is not a lesser pipeline.
    let _ = dash_tx.send(tui::dev::DashEvent::Note {
        message: match mode {
            tui::dev::Mode::Dev => {
                "same pipeline as `albedo serve` · overlay + hot reload on".to_string()
            }
            tui::dev::Mode::Serve => "production pipeline · no watcher".to_string(),
        },
    });

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("failed to start tokio runtime: {err}"))?;

    // One channel carries both outcomes, so the wait below is a single blocking
    // `recv` and the first message decides. Readiness fires from inside the
    // server once everything fallible has succeeded; the failure arm can only
    // win the race if `run_with_ready` returned before signalling, which is
    // exactly the definition of a boot that did not finish.
    let (boot_tx, boot_rx) = mpsc::channel::<Result<(), String>>();
    let ready_tx = boot_tx.clone();
    // The dashboard owns the screen, so a `println!` here would be painted over
    // by the first frame. A note is how this lane says things, and it survives
    // in the log pane where the author can still find it.
    let note_tx = dash_tx.clone();
    runtime.spawn(async move {
        let outcome = server.run_with_ready(move |report| {
            for message in report.lines() {
                let _ = note_tx.send(tui::dev::DashEvent::Note { message });
            }
            let _ = ready_tx.send(Ok(()));
        });
        if let Err(err) = outcome.await {
            // After readiness nobody is receiving and this send is dropped —
            // the dashboard owns the screen by then. Before it, this is the
            // startup error, and it is the whole reason for the channel.
            let _ = boot_tx.send(Err(err.to_string()));
        }
    });

    match boot_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(format!("server startup failed: {err}")),
        // The task ended without either signalling ready or reporting an error.
        Err(_) => return Err("server exited before it finished starting".to_string()),
    }

    // Only now is there something at the other end of this URL.
    if open_browser {
        let _ = try_open_browser(url.as_str());
    }

    let mut guard = tui::TerminalGuard::new()
        .map_err(|err| format!("failed to start the terminal UI: {err}"))?;
    let result = tui::dev::Dashboard::new(mode, url, project, report)
        .run(&mut guard, dash_rx)
        .map_err(|err| format!("terminal UI error: {err}"));
    drop(guard);
    result
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
    dash: Option<mpsc::Sender<tui::dev::DashEvent>>,
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
        // Two sinks, one decision made at startup: with a dashboard every
        // outcome becomes a row in its event log, without one it prints exactly
        // as it always did. The browser overlay is told either way — a build
        // error belongs on the page that failed to render, regardless of what
        // the terminal is doing.
        let report = |event: tui::dev::DashEvent| {
            if let Some(sender) = &dash {
                let _ = sender.send(event);
            }
        };
        match run_prod_build_quiet(&contract) {
            Ok(_tiers) => match reload.reload(&opts) {
                Ok(()) => {
                    let millis = rebuild_start.elapsed().as_secs_f64() * 1000.0;
                    if dash.is_some() {
                        report(tui::dev::DashEvent::Reloaded { millis });
                    } else {
                        print_ok(format!("reloaded in {}", colorize_timing_ms(millis)));
                    }
                }
                Err(err) => {
                    reload.report_build_error(err.to_string());
                    if dash.is_some() {
                        report(tui::dev::DashEvent::BuildFailed {
                            message: format!("reload failed: {err}"),
                        });
                    } else {
                        eprintln!("  {} reload failed: {err}", style("✗", "1;31"));
                    }
                }
            },
            Err(err) => {
                reload.report_build_error(err.clone());
                if dash.is_some() {
                    report(tui::dev::DashEvent::BuildFailed { message: err });
                } else {
                    eprintln!("  {} rebuild failed: {err}", style("✗", "1;31"));
                }
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
            if report.components.len() == 1 {
                ""
            } else {
                "s"
            },
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
        return Err(Error::new(
            ErrorKind::InvalidData,
            "request line exceeds limit",
        ));
    }

    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if line.len() > MAX_REQUEST_LINE_BYTES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "header line exceeds limit",
            ));
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if headers.len() >= MAX_REQUEST_HEADER_COUNT {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "header count exceeds limit",
            ));
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
            // Project-relative, never absolute. This comment is inlined into a
            // `<style>` block in **every route shell**, so an absolute path here
            // does not sit in a build artifact — it is served to every visitor's
            // browser, disclosing the author's directory layout on each page
            // load. (Found 2026-08-03 while removing the wrapper modules, which
            // leaked the same thing into files nobody ever fetched. This one
            // ships.)
            let display = path.strip_prefix(root).unwrap_or(path.as_path());
            out.push_str("\n/* ");
            out.push_str(display.to_string_lossy().replace('\\', "/").as_str());
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
fn run_prod_build(contract: &ResolvedDevContract) -> Result<TierReport, String> {
    // serve / dev startup call this — full build presentation, no tier report
    // (that's reserved for the explicit `albedo build`).
    run_prod_build_with_budget(contract, false, false, false)
}

/// Silent build for the dev hot-reload path — does the full build but prints
/// nothing (the watcher prints a single "reloaded in Xms" line instead of the
/// whole build log on every save). Warnings and errors still surface.
fn run_prod_build_quiet(contract: &ResolvedDevContract) -> Result<TierReport, String> {
    run_prod_build_with_budget(contract, false, false, true)
}

fn run_prod_build_with_budget(
    contract: &ResolvedDevContract,
    skip_budget: bool,
    show_tiers: bool,
    quiet: bool,
) -> Result<TierReport, String> {
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
    // Tier C · Phase 2 — the `node_modules` search root for client bundles.
    let project_root_for_closure = contract.project_dir.clone();
    let build_work = move || {
        let scanner = ProjectScanner::new();
        // Artifacts are written below, so every `module_path` they key on has
        // to be expressed relative to the project rather than to this machine.
        let compiler = scanner
            .build_compiler(components)
            .with_project_root(contract.project_dir.clone());
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
        let measured = measure_client_bytes(
            &manifest,
            &module_sources,
            &out_dir_for_closure,
            &project_root_for_closure,
        );
        Ok::<_, String>((manifest, tier_report, report, missing_sources, measured))
    };
    let (mut manifest, tier_report, report, missing_sources, measured) = if quiet {
        build_work()?
    } else {
        with_spinner("compiling production bundle…", build_work)?
    };

    // A Tier-A component that would not render is decided HERE, in the build,
    // and for `albedo build` this is the last moment anyone is looking —
    // somebody who builds and ships the output would otherwise learn about it
    // from the missing section on the live page.
    //
    // Printed even under `quiet`, which suppresses progress, not findings: a
    // hot reload that just deleted a component's markup is exactly the reload
    // worth interrupting for.
    //
    // The one lane that stays silent here is the startup build behind `serve` /
    // `dev` — `show_tiers` off AND `quiet` off is exactly that lane, and a
    // server is about to boot and print the identical line out of its
    // `BootReport`. Item 6.5's rule is one event, one wording, one line; the
    // wording itself lives on `StaticRenderFailure::report_line` so the two
    // lanes cannot drift.
    // ── preflight, for the lane where nothing else will run it ────────────
    //
    // 🔴 Every check in `preflight` was written at **boot**, which meant
    // `albedo build` — the thing CI runs — passed apps that `albedo serve`
    // refuses. A broken build shipped and failed at container start instead of
    // in the pipeline that produced it.
    //
    // Gated on `show_tiers`, which is exactly the standalone `albedo build`
    // lane: the startup build behind `serve` / `dev` is followed immediately by
    // a boot that runs the same checks against the same facts, and paying for a
    // second source-tree parse there would slow every hot reload to catch
    // nothing new. Same reasoning as `boot_report_will_print_this` below.
    if show_tiers {
        let served: Vec<String> = manifest.routes.keys().cloned().collect();
        match dom_render_compiler::runtime::CompiledProject::load_from_dir(&contract.root) {
            Ok(compiled) => {
                // The auth tables are deliberately not augmented in here. The
                // only thing the schema contributes is *"is this a declared
                // collection"*, and `albedo_*` is never a legitimate topic — so
                // their absence cannot produce a false accusation, which is the
                // failure direction that matters.
                let schema = if contract.forge.is_empty() {
                    dom_render_compiler::forge::ForgeSchema::guestbook_default()
                } else {
                    match dom_render_compiler::forge::ForgeSchema::from_declarations(
                        &contract.forge,
                    ) {
                        Ok(schema) => schema,
                        // 🔴 First written as "boot makes a better diagnosis,
                        // so say nothing here" — which is exactly wrong for the
                        // lane this branch is in. `albedo build` is what CI
                        // runs, and boot never happens there, so deferring meant
                        // a malformed `forge` block sailed through the pipeline
                        // and failed at container start. The message is boot's,
                        // word for word, so the two lanes cannot drift.
                        Err(err) => {
                            return Err(format!(
                                "invalid `forge` block in albedo.config: {err}"
                            ));
                        }
                    }
                };
                // AUTH · the `auth` block is lowered at boot and, until
                // 2026-09-02, nowhere else — so a declared `login` that is not a
                // rooted path (an absolute URL, `//host`, a control character)
                // passed `albedo build` and failed at container start. The
                // `forge` arm above already learned this lesson for its own
                // block; this is the same lane and the same argument.
                //
                // 🔑 The message is boot's, word for word (`boot.rs`), so the
                // two lanes cannot describe one failure two ways.
                if let Err(err) = contract.auth.lower() {
                    return Err(format!("invalid `auth` block in albedo.config: {err}"));
                }

                dom_render_compiler::preflight::check(
                    &compiled,
                    &schema,
                    &served,
                    Some(&manifest),
                )?;
                // Write the server npm bundles beside the rest of the build
                // output. They were just built in memory to run the checks
                // above and were then thrown away, so serving the app required
                // `node_modules` wherever it ran — 58 MB / 6 526 files for a
                // five-dependency app, to re-derive 409 KB the build already
                // had. See `bundler::npm_prebuilt`.
                //
                // A failure here is reported and not fatal: the file is an
                // optimisation, and boot falls back to bundling from
                // `node_modules` exactly as before.
                let prebuilt = dom_render_compiler::bundler::npm_prebuilt::PrebuiltNpmBundles::new(
                    compiled.npm_bundles().to_vec(),
                );
                match prebuilt.to_json() {
                    Ok(json) => {
                        let path = out_dir.join(
                            dom_render_compiler::bundler::npm_prebuilt::NPM_SERVER_BUNDLES_FILENAME,
                        );
                        if let Err(err) = std::fs::write(&path, json) {
                            print_warn(format!(
                                "could not write {}: {err} — the app will still serve, but it \
                                 will need node_modules wherever it runs",
                                path.display()
                            ));
                        }
                    }
                    Err(err) => print_warn(format!("could not encode npm bundles: {err}")),
                }
            }
            // 🔴 This arm was `Err(_) => {}`, and swallowing it made
            // `albedo build` exit 0 on a project `albedo serve` refuses to
            // start — the exact defect `preflight` exists to prevent, one
            // branch away from the sibling above that already carries the
            // correction for it. Found 2026-09-02 by building a real app: a
            // `useSharedSlot` whose topic is not derivable fails HERE, the
            // build reported success, and boot then refused the artifact CI
            // had just green-lit.
            //
            // The old comment said this failure "is its own error with its own
            // message, produced on the path that actually needs the tree". The
            // build needs the tree too: it is the lane that decides whether the
            // artifact ships.
            //
            // 🔑 The message is boot's, word for word (`boot.rs`'s
            // `ServerStartup`), so the two lanes cannot drift into describing
            // one failure two ways.
            Err(err) => {
                return Err(format!(
                    "failed to load source tree at '{}': {err}",
                    contract.root.display()
                ));
            }
        }
    }

    let boot_report_will_print_this = !show_tiers && !quiet;
    if !boot_report_will_print_this {
        for failure in &manifest.static_render_failures {
            print_warn(failure.report_line());
        }
    }

    // `albedo build` shows the tier breakdown — the "instrument for light" view
    // of what the app compiled to (luminance bars per tier). Suppressed for
    // serve / dev (re)builds so a hot reload stays a single line.
    if show_tiers {
        printer::print_tier_report(&tier_report, &contract.root.display().to_string(), &measured);
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
    // PHOSPHOR · the shared per-browser lane. Imported by the WT bootstrap
    // as `./phosphor.js`, so it too must sit beside it.
    let phosphor_asset_path = out_dir.join("_albedo").join("phosphor.js");
    std::fs::write(&phosphor_asset_path, albedo_phosphor_template()).map_err(|err| {
        format!(
            "failed to write phosphor lane '{}': {err}",
            phosphor_asset_path.display()
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

    // Handed back so the dev dashboard can show the tier mix it just computed.
    // Every existing caller discards it with `?;`, which is why this widened
    // rather than growing an out-parameter.
    Ok(tier_report)
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
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create '{}': {err}", parent.display()))?;
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
        &target.join("src").join("routes").join("guestbook.tsx"),
        SCAFFOLD_GUESTBOOK_ROUTE,
        options.force,
    )?;
    write_scaffold_file(
        &target
            .join("src")
            .join("routes")
            .join("room")
            .join("[id].tsx"),
        SCAFFOLD_ROOM_ROUTE,
        options.force,
    )?;
    write_scaffold_file(
        &target.join("src").join("routes").join("sign-in.tsx"),
        SCAFFOLD_SIGN_IN_ROUTE,
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
    write_scaffold_file(&target.join("README.md"), SCAFFOLD_README, options.force)?;
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

    // PRISM · emit `albedo/forge`'s types now, not on the first build.
    //
    // `import { messages } from "albedo/forge"` resolves only through this
    // generated file. The serve/dev boot regenerates it, which is correct but
    // too late for the one moment that matters most: a stranger runs
    // `albedo init`, opens the editor or `npm run typecheck`, and sees errors on
    // code they did not write. That is exactly the failure TODO #1 item 1.5
    // closed (146 of them), and shipping a scaffold route that imports the
    // module would have reopened it — three errors, all of them ours.
    //
    // Generated from the scaffold's own config rather than a second hard-coded
    // copy of the declarations, so the types cannot drift from the `forge` block
    // the user is reading two files away.
    write_scaffold_forge_types(target)?;

    Ok(())
}

/// Generate `.albedo/forge.d.ts` from the config just written.
///
/// Best-effort by design: a failure costs autocomplete on a fresh project, and
/// the next `albedo dev` regenerates it. Failing the scaffold over a types file
/// would trade a small degradation for a total one.
fn write_scaffold_forge_types(target: &Path) -> Result<(), String> {
    let Ok(config) = dom_render_compiler::dev::contract::load_forge_declarations(target) else {
        return Ok(());
    };
    if config.is_empty() {
        return Ok(());
    }
    let dts = dom_render_compiler::forge::emit_forge_dts(&config);
    let albedo_dir = target.join(".albedo");
    if std::fs::create_dir_all(&albedo_dir).is_ok() {
        let _ = std::fs::write(albedo_dir.join("forge.d.ts"), dts);
    }
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
        style(
            "a starter, lit — three components, one at each tier of light.",
            "2"
        )
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
        style(
            "run it, and watch albedo sort them by how much they move.",
            "2"
        )
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

/// PHOSPHOR · the shared per-browser lane, deployed to
/// `_albedo/phosphor.js`. Imported by the WT bootstrap as `./phosphor.js`,
/// so the two ship together or the import 404s at boot.
fn albedo_phosphor_template() -> String {
    include_str!("../../assets/phosphor.js").to_string()
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
            return Err("usage: albdo completions <bash|zsh|fish|powershell>\n\
                 Examples:\n  \
                   albdo completions bash        >> ~/.bashrc\n  \
                   albdo completions zsh         >> ~/.zshrc\n  \
                   albdo completions fish        > ~/.config/fish/completions/albdo.fish\n  \
                   albdo completions powershell  >> $PROFILE"
                .to_string());
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

    local commands="init dev build ship serve files budget doctor demo run completions help"

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
        doctor)
            COMPREPLY=( $(compgen -W "--json --config" -- "$cur") )
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
        'doctor:Report what the build already knows about itself'
        'demo:Draw the instrument panel from synthetic signal'
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
                (doctor)
                    _arguments \
                        '--json[Machine-readable report for CI]' \
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
set -l albdo_commands init dev build ship serve files budget doctor demo run completions help

# Disable file completions for the main command
complete -c albdo -f

# Top-level commands
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a init        -d 'Create a tiered starter app scaffold'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a dev         -d 'Start the live dev server with HMR'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a build       -d 'Compile an optimised production build'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a ship        -d 'Build and configure deployment target files'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a serve       -d 'Serve static files from a directory'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a budget      -d 'Evaluate the tier budget against the current build'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a doctor      -d 'Report what the build already knows about itself'
complete -c albdo -n "__fish_use_subcommand $albdo_commands" -a demo        -d 'Draw the instrument panel from synthetic signal'
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

    $commands = @('init','dev','build','ship','serve','files','budget','doctor','demo','run','completions','help')
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
        'doctor' {
            @('--json','--config') | Where-Object { $_ -like "$wordToComplete*" } |
                ForEach-Object { [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterName', $_) }
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
    print_command("doctor", "[dir]", "report what the build knows about itself");
    print_command("demo", "", "draw the instrument panel — synthetic signal");
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
    print_option("--target <name>", "binary | docker | fly | static");
    print_option("--config <FILE>", "explicit albedo config");
    print_option("--entry <FILE>", "override entry module");
    print_option("--no-budget", "skip the tier-budget gate");
    println!();
}

fn print_doctor_help() {
    print_banner();
    print_section("doctor");
    println!(
        "  {}  {}",
        style("usage", "2"),
        style("albedo doctor [dir] [--json]", "1")
    );
    print_option("--json", "machine-readable report for CI");
    print_option("--config <FILE>", "explicit albedo config");
    println!();
    println!(
        "  {}",
        style(
            "reports the reach matrix (what each route reads and what keys it), the rate-limit",
            "2"
        )
    );
    println!(
        "  {}",
        style(
            "class each route answers to, and `tsc --noEmit`. Exits non-zero on a type error only.",
            "2"
        )
    );
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
    print_option("--strict", "require tier-budget.toml; fail if missing");
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
        style("albedo serve [dir] [--host <IP>] [--port <PORT>]", "1")
    );
    println!();
    println!(
        "    {} builds your app, then runs the production server:",
        style("·", "2")
    );
    println!(
        "        {} {}",
        style_256("·", ACCENT_DEEP, false),
        style(
            "streams every route — static inline, dynamic on demand",
            "2"
        )
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
    print_option("--domain <NAME>", "serve HTTPS with an automatic Let's Encrypt certificate");
    print_option("--acme-contact <EMAIL>", "optional ACME account contact");
    print_option("--acme-cache <DIR>", "where certificates persist (default: .albedo-acme)");
    print_option("--acme-staging", "use the Let's Encrypt staging CA while rehearsing");
    print_option("--tls-cert <FILE>", "serve HTTPS with a certificate you already have");
    print_option("--tls-key <FILE>", "the PEM key for --tls-cert");
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
    println!("    {} {}", style_256("$", ACCENT, true), style(cmd, "2"));
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
    // Version label and ladder come from the dashboard's constants, not a second
    // copy — the banner and the TUI masthead are the same statement rendered by
    // two different engines, and a drift between them is the kind of thing
    // nobody notices until a screenshot goes out.
    let sep = style(" / ", "2");
    let tiers = tui::dev::TIER_LADDER
        .iter()
        .map(|name| style_256(name, MUTED, false))
        .collect::<Vec<_>>()
        .join(&sep);
    println!(
        "  {}  {}   {}",
        gradient_text("ALB'DO", &BRAND_PALETTE, true),
        style_256(tui::dev::VERSION_LABEL, ACCENT_SOFT, true),
        tiers,
    );
    // Full-width champagne hairline — the halo under the mark (41 to match art).
    println!("  {}", style_256(&"─".repeat(41), ACCENT_DEEP, false));
    println!();
}

fn print_section(title: &str) {
    println!();
    println!("  {} {}", style_256("▸", ACCENT, true), style(title, "1"));
}

fn print_kv(label: &str, value: impl std::fmt::Display) {
    println!("    {:<14} {}", style_256(label, MUTED, false), value);
}

fn print_ok(message: impl std::fmt::Display) {
    println!("  {} {}", style("✓", "1;32"), message);
}

fn print_warn(message: impl std::fmt::Display) {
    println!("  {} {}", style("!", "1;33"), message);
}

/// Announce anything the boot changed on the author's behalf, above the banner.
///
/// A FORGE schema migration is the one startup side effect that alters a file
/// the author owns, so it is *told* to them rather than logged: nothing in this
/// binary installs a `tracing` subscriber, so `info!` reaches no one — and a
/// change to someone's database that reaches no one is the exact defect the
/// drift check exists to remove.
///
/// Printed with `!` rather than `✓` on purpose. It is not a step that succeeded,
/// it is a thing that happened, and the author should look at it once.
fn print_boot_report(report: &albedo_server::BootReport) {
    for line in report.lines() {
        print_warn(line);
    }
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
        assert!(target.join("src/routes/guestbook.tsx").is_file());
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
        assert!(dockerfile.contains("FROM debian:bookworm-slim AS builder"));
        assert!(dockerfile.contains("FROM debian:bookworm-slim AS runtime"));
        assert!(dockerfile.contains("ALBEDO_SERVER_HOST=0.0.0.0"));
        assert!(dockerfile.contains("ALBEDO_SERVER_PORT=3000"));
        assert!(dockerfile.contains("HEALTHCHECK"));
        assert!(dockerfile.contains("EXPOSE 3000"));

        // 🔴 This used to assert `COPY --from=builder /workspace/.albedo/dist`,
        // which **pinned the defect**: shipping the build output and nothing
        // else is exactly what made the container a static file server. The
        // test passed for as long as the bug existed, which is the whole
        // problem with asserting the shape a template happens to have rather
        // than the thing it has to achieve.
        //
        // `dist` is now deliberately absent — `albedo serve` rebuilds before it
        // serves, so copying it would ship bytes that are immediately discarded.
        // `dist` itself is still not shipped -- `albedo serve` rebuilds it --
        // but the npm bundles inside it are, because that rebuild cannot
        // regenerate them without `node_modules`, which is what they replace.
        assert!(
            !dockerfile.contains("COPY --from=builder /workspace/.albedo/dist /"),
            "the runtime stage should not ship a whole dist that serve rebuilds"
        );
        assert!(
            !dockerfile.contains("/workspace/node_modules /app/node_modules"),
            "the runtime image must not carry node_modules: the build lowered the \
             bundles so that it would not have to"
        );
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
            detected_layout: None,
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
            forge: Default::default(),
            sources: Default::default(),
            auth: Default::default(),
            uploads: Default::default(),
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

    /// 🔴 **`albedo build` must fail on a project `albedo serve` would refuse.**
    ///
    /// The preflight block in `run_build_command` loads the source tree so it
    /// can run the checks. That load is also the only place several defects are
    /// detected at all — a `useSharedSlot` whose topic is not derivable fails
    /// there and nowhere else. The arm was written `Err(_) => {}`, so the build
    /// reported success and boot then refused the artifact CI had green-lit:
    /// the exact defect `preflight` exists to prevent, one branch away from the
    /// sibling that already carries the correction for it.
    ///
    /// Derived from this file's own source rather than asserted about behaviour,
    /// for the same reason `preflight`'s fence is: the failure mode is a branch
    /// that silently does nothing, which no behavioural test of the success path
    /// can see.
    #[test]
    fn the_build_lane_never_swallows_a_source_tree_load_failure() {
        let source = include_str!("albedo.rs");
        let block = source
            .split_once("match dom_render_compiler::runtime::CompiledProject::load_from_dir")
            .expect("the build's preflight block is here")
            .1;
        // Up to the end of that `match`, which is the arm list we care about.
        let block = block
            .split_once("
    }")
            .expect("the match has an end")
            .0;

        // Comment lines are skipped: the arm below documents the bug by
        // quoting it, and a naive `contains` reads its own prose as the
        // defect. Fired exactly that way when first written.
        let swallows = block
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .any(|line| line.starts_with("Err(_)") && line.contains("{}"));

        assert!(
            !swallows,
            "the build's `CompiledProject::load_from_dir` failure is being swallowed, so              `albedo build` will exit 0 on a project `albedo serve` cannot start"
        );
        assert!(
            block.contains("failed to load source tree at"),
            "the build must report the load failure using boot's wording, word for word, so              the two lanes cannot describe one failure two ways"
        );
    }

    /// 🔴 **`albedo serve .` must run the app, not serve the folder.**
    ///
    /// A bare positional used to route to the static-file server, so the
    /// sequence every newcomer types —
    ///
    /// ```text
    /// albedo build .   # compiles the project
    /// albedo serve .   # served the DIRECTORY: every route 404, including the
    ///                  # scaffold's own
    /// ```
    ///
    /// — produced a dead site with no diagnostic. The positional means *the
    /// project* on `dev`, `build`, `ship` and `doctor`; `serve` was the fifth
    /// and most-used, and meant the opposite.
    ///
    /// Derived from this file's own source, like the other CLI fences here,
    /// because the failure is a branch that silently reroutes the command
    /// rather than one that errors.
    #[test]
    fn a_bare_positional_on_serve_selects_the_project_not_a_files_root() {
        let source = include_str!("albedo.rs");
        let body = source
            .split_once("fn run_serve_command(")
            .expect("serve command is here")
            .1;
        let body = body
            .split_once("let cwd = std::env::current_dir()")
            .expect("serve reaches the project path")
            .0;

        let code: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with("//"))
            .collect();

        // The arm that made any non-flag argument mean "serve this folder".
        assert!(
            !code.iter().any(|line| line.contains("!arg.starts_with('-')")),
            "a bare positional is being treated as a files root again, so              `albedo serve .` will serve the directory instead of the app"
        );
        // `--dir` stays the explicit opt-in: the emitted Dockerfile's CMD uses it.
        assert!(
            code.iter().any(|line| line.contains("\"--dir\"")),
            "`--dir` must stay the explicit files opt-in — the Dockerfile              `albedo ship --target docker` emits depends on it"
        );
    }

    /// 🔴 **The docker target must ship the APP, not a folder of files.**
    ///
    /// The emitted Dockerfile used to copy only `.albedo/dist` + `public` and
    /// run `albedo serve --dir dist` — the static file server. The container had
    /// no FORGE, no actions, no per-request render and no auth, and carried no
    /// source tree, so project-mode serve could not have run there either.
    ///
    /// 🔑 **Item 13.4, 2026-09-06.** This test used to enumerate the four COPY
    /// lines the runtime stage happened to have — `src/`, `albedo.config.*`,
    /// `npm-server-bundles.json` — which is precisely the failure its sibling
    /// `test_docker_template_is_multi_stage_with_runtime_env` documents:
    /// *asserting the shape a template happens to have rather than the thing it
    /// has to achieve.* That list was a second, hand-maintained definition of
    /// "what a deployment consists of", and it had already drifted once.
    ///
    /// `bundle_payload::should_ship` is the one definition now. What this test
    /// asserts is the property that makes that true: the runtime stage receives
    /// **the shipped artifact**, not a hand-assembled copy of the project.
    #[test]
    fn the_emitted_dockerfile_runs_the_app_and_not_a_files_root() {
        let dockerfile = build_docker_template();

        let cmd = dockerfile
            .lines()
            .find(|line| line.starts_with("CMD "))
            .expect("the template has a CMD");
        // The original defect, still the most important line in this file.
        assert!(
            !cmd.contains("--dir"),
            "the container is running the static file server: {cmd}"
        );
        assert!(
            cmd.contains("/app/albedo-app"),
            "the container must run the shipped app: {cmd}"
        );
        // 🪤 Without `exec`, `sh` is PID 1 and never forwards SIGTERM — which
        // would defeat the graceful-shutdown handler (13.1) inside a container
        // and put us straight back to "every deploy cuts live requests".
        assert!(
            cmd.contains("exec "),
            "the app must be PID 1 or it never receives SIGTERM: {cmd}"
        );

        // One definition of a deployment: the builder ships, the runtime copies.
        assert!(
            dockerfile.contains("ship --target binary"),
            "the builder must produce the same artifact `--target binary` does"
        );
        assert!(
            dockerfile.contains("COPY --from=builder /out/albedo-app"),
            "the runtime stage must receive the shipped artifact"
        );

        // The drift this closed: any COPY that re-assembles the project by hand
        // is a second answer to a question `should_ship` already answers.
        for hand_assembled in [
            "COPY --from=builder /workspace/src",
            "COPY --from=builder /workspace/albedo.config.",
            "COPY --from=builder /workspace/public",
            "COPY --from=builder /workspace/.albedo",
        ] {
            assert!(
                !dockerfile.contains(hand_assembled),
                "{hand_assembled:?} re-derives the payload the ship target already \
                 computes — that is how this template drifted the first time"
            );
        }

        // 🪤 A shipped binary defaults its database beside itself and unpacks
        // per build id, so an unmounted `/app` means every deploy starts empty.
        assert!(
            dockerfile.contains("ALBEDO_FORGE_DB=/data/forge.db")
                && dockerfile.contains("VOLUME /data"),
            "the database must be on a volume, not in the container's writable layer"
        );
    }

    /// 🪤 The fly target inherits the Dockerfile, so it inherits the stop
    /// signal — and `auto_stop_machines` puts that signal on the *idle* path,
    /// not just the deploy path. A machine that dies instantly cuts in-flight
    /// requests every time it scales to zero.
    /// 🪤 **Parsed, not string-matched.** `kill_signal` written after the
    /// `[mounts]` header is `mounts.kill_signal` — valid TOML, silently
    /// ignored by Fly, and indistinguishable from the correct file by any
    /// `contains()` assertion. Caught by emitting the file and reading it,
    /// which is the same method that found every other defect in this target.
    #[test]
    fn the_fly_target_states_the_stop_signal_and_persists_the_database() {
        let raw = build_fly_toml_template("demo-app");
        let parsed: toml::Value = toml::from_str(&raw).expect("the emitted fly.toml must parse");
        let table = parsed.as_table().expect("fly.toml is a table");

        assert_eq!(
            table.get("kill_signal").and_then(toml::Value::as_str),
            Some("SIGTERM"),
            "kill_signal must be a TOP-LEVEL key — nested under a table Fly never reads it"
        );
        assert!(
            table.contains_key("kill_timeout"),
            "kill_timeout must be top-level too, and bounds the drain"
        );

        let mounts = table
            .get("mounts")
            .and_then(toml::Value::as_table)
            .expect("a machine without a volume loses its database on every deploy");
        assert_eq!(
            mounts.get("destination").and_then(toml::Value::as_str),
            Some("/data")
        );

        let env = table
            .get("env")
            .and_then(toml::Value::as_table)
            .expect("fly.toml has an [env] table");
        assert_eq!(
            env.get("ALBEDO_FORGE_DB").and_then(toml::Value::as_str),
            Some("/data/forge.db"),
            "the mount is useless unless FORGE is pointed at it"
        );
    }

    /// 🔴 **13.4c — the defect `docker build` found on its first ever run.**
    ///
    /// The builder stage was `FROM rust:1-bookworm` running
    /// `cargo build --release --bin albedo`. The build context is **the user's
    /// app**, which has no `Cargo.toml`, so it failed in 0.17 s with
    /// `could not find Cargo.toml in /workspace`. That line predates the 13.4
    /// rewrite too — **this target had never built an image in any version.**
    ///
    /// 🔑 Every previous test here asserted things *about* the template and all
    /// of them passed, because none of them could run `docker build`. The
    /// assertion that would have caught it is the crude one below: **the image
    /// must not try to compile Rust**, because the source is not there.
    #[test]
    fn the_image_never_compiles_rust_because_the_source_is_not_in_the_context() {
        let dockerfile = build_docker_template();
        // 🪤 Directives only. The template *documents* the old `cargo build`
        // line so the mistake is not repeated, and a whole-file `contains`
        // matches that prose — this assertion failed on its own comment the
        // first time it ran.
        let directives: String = dockerfile
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !directives.contains("cargo build"),
            "the build context is the user's app — there is no Cargo.toml in it, so any \
             `cargo build` here fails in under a second and always has"
        );
        assert!(
            !directives.contains("FROM rust:"),
            "a Rust toolchain in the image is the symptom of that mistake"
        );
        assert!(
            directives.contains(&format!("COPY {LINUX_RUNTIME_FILENAME} /usr/local/bin/albedo")),
            "the image needs one Linux executable, staged by `ship --target docker`"
        );
    }

    /// 🪤 The runtime must not end up inside the payload it carries. The
    /// builder does `COPY . .`, so without an exclusion the ~25 MB Linux albedo
    /// is embedded in every shipped app — a whole albedo inside every albedo.
    #[test]
    fn the_staged_linux_runtime_is_not_shipped_inside_the_payload() {
        assert!(
            !albedo_server::bundle_payload::should_ship(LINUX_RUNTIME_FILENAME),
            "`{LINUX_RUNTIME_FILENAME}` must be excluded from the payload"
        );
    }

    /// A Windows `albedo.exe` under the Linux runtime's name must be caught by
    /// its **contents**, not its name — that is the whole failure mode.
    #[test]
    fn a_non_elf_file_is_not_accepted_as_the_linux_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let planted = temp.path().join(LINUX_RUNTIME_FILENAME);
        std::fs::write(&planted, b"MZ\x90\x00 not an ELF").unwrap();

        assert!(!is_linux_elf(&planted));
        let err = resolve_linux_runtime(temp.path()).expect_err("must refuse");
        assert!(
            err.contains("not a linux/amd64 executable"),
            "refusal should name the real problem: {err}"
        );
    }

    /// 🪤 `node_modules` stays out of the build context on purpose — the host's
    /// tree may be built for another platform — so the image has to install it.
    /// Without that, `albedo build` in the builder cannot resolve a single bare
    /// specifier and every app with an npm dependency fails.
    #[test]
    fn the_docker_target_installs_npm_dependencies() {
        assert!(
            build_dockerignore_template().contains("node_modules"),
            "the host's node_modules must stay out of the build context"
        );
        let dockerfile = build_docker_template();
        assert!(
            dockerfile.contains("npm ci") && dockerfile.contains("npm install"),
            "the image must install npm dependencies itself"
        );
        assert!(
            dockerfile.contains("COPY --from=deps /workspace/node_modules"),
            "the builder needs the installed tree to resolve bare specifiers"
        );
    }
}
