//! TODO #1 item 4.7 · **npm coverage — count it, do not fix it.**
//!
//! "This is the single largest unknown in the business." Every projection in
//! `THE_UNDERSTATEMENT.md` carries an unknown multiplier until we can say what
//! fraction of a real project's `dependencies` this bundler can actually load.
//! This harness produces that number.
//!
//! It is **a measurement, not a gate.** It is `#[ignore]`d because it needs a
//! real `node_modules` tree on disk (and therefore npm and a network), and it
//! never asserts a coverage threshold — a bad number is information, and a
//! ratchet here would only tempt someone to sample easier packages.
//!
//! Run it with:
//!
//! ```text
//! ALBEDO_NPM_COVERAGE_MANIFEST=<path-to-manifest.json> \
//!   cargo test --test npm_coverage_probe -- --ignored --nocapture
//! ```
//!
//! The manifest is a JSON array of projects:
//!
//! ```json
//! [{ "project": "vercel-commerce", "dir": "C:/tmp/vercel-commerce",
//!    "specifiers": ["next", "react", "clsx"] }]
//! ```
//!
//! `dir` is the directory holding the installed `node_modules`; each specifier
//! is resolved from there exactly as `CompiledProject::wrap` resolves the bare
//! imports it discovers in user source.
//!
//! Set `ALBEDO_NPM_COVERAGE_REPORT` to also write the markdown table to a file.

use dom_render_compiler::bundler::npm::{
    bundle_npm_dependency, NpmBundleError, NpmDependencyBundle,
};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Which stage a specifier died at, split by **what the failure means about
/// us** rather than by which error variant was raised.
///
/// 🪤 That distinction is not academic — it is the bug this file already had.
/// The first run reported **95.8%** because twelve Node built-in failures
/// arrived as `PackageNotFound` (the resolver has no concept of a built-in, so
/// it searches `node_modules` for `util` and reports it missing) and were
/// written off as fixture noise. The honest number is **85.2%**, and the twelve
/// are one real, bounded gap. **An error taxonomy that groups "your fixture is
/// wrong" with "we cannot do this" will always round in our favour** — hence
/// [`Outcome::NodeBuiltin`] and [`Outcome::counts_against_bundler`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Loaded,
    PackageNotFound,
    /// A Node built-in (`util`, `fs`, `crypto`, …) reached the resolver.
    ///
    /// This surfaces as `PackageNotFound` — the resolver has no concept of
    /// built-ins, so it searches `node_modules` for `util` and reports it
    /// missing. Separating it out is the whole reason this enum is not just
    /// `Result`: lumped together, a **genuine capability gap reads as fixture
    /// noise**. It is exactly what makes `react-dom/server` fail.
    NodeBuiltin,
    SubpathNotExported,
    FileNotFound,
    PackageJson,
    Io,
    Compile,
    GraphTooLarge,
    /// Tier C · Phase 2 — a *client* bundle reached a module the browser host
    /// declines to provide, or a name it does not have.
    ///
    /// Unreachable through this probe, which measures the **server** bundler
    /// (`ShakeOptions::default()`, no externals), and listed so that the day a
    /// client-side coverage run is added the number lands in its own bucket
    /// instead of being counted as a resolution failure. A refusal is a
    /// capability answer, not a bug.
    ClientHostRefused,
}

/// Node's built-in module set. A specifier equal to one of these, or carrying
/// the `node:` prefix, is a runtime capability question, not a resolution one.
///
/// Kept as a packed table rather than rustfmt's one-per-line: it is a lookup
/// set, and 42 lines of single words is harder to scan for "is `dgram` here?".
#[rustfmt::skip]
const NODE_BUILTINS: &[&str] = &[
    "assert", "async_hooks", "buffer", "child_process", "cluster", "console", "constants",
    "crypto", "dgram", "diagnostics_channel", "dns", "domain", "events", "fs", "http", "http2",
    "https", "inspector", "module", "net", "os", "path", "perf_hooks", "process", "punycode",
    "querystring", "readline", "repl", "stream", "string_decoder", "sys", "timers", "tls",
    "trace_events", "tty", "url", "util", "v8", "vm", "wasi", "worker_threads", "zlib",
];

fn is_node_builtin(package: &str) -> bool {
    let bare = package.strip_prefix("node:").unwrap_or(package);
    let root = bare.split('/').next().unwrap_or(bare);
    NODE_BUILTINS.contains(&root)
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Loaded => "loaded",
            Outcome::PackageNotFound => "package-not-found (fixture)",
            Outcome::NodeBuiltin => "node-builtin (REAL GAP)",
            Outcome::SubpathNotExported => "subpath-not-exported",
            Outcome::FileNotFound => "file-not-found",
            Outcome::PackageJson => "bad-package-json",
            Outcome::Io => "io",
            Outcome::Compile => "compile",
            Outcome::GraphTooLarge => "graph-too-large",
            Outcome::ClientHostRefused => "client-host-refused (client bundles only)",
        }
    }

    /// `PackageNotFound` means the package is not on disk, so it says nothing
    /// about the bundler. It is excluded from the coverage denominator and
    /// reported separately.
    fn counts_against_bundler(self) -> bool {
        !matches!(self, Outcome::Loaded | Outcome::PackageNotFound)
    }

    fn from_error(err: &NpmBundleError) -> Self {
        match err {
            NpmBundleError::PackageNotFound { package, .. } if is_node_builtin(package) => {
                Outcome::NodeBuiltin
            }
            NpmBundleError::PackageNotFound { .. } => Outcome::PackageNotFound,
            NpmBundleError::SubpathNotExported { .. } => Outcome::SubpathNotExported,
            NpmBundleError::FileNotFound { .. } => Outcome::FileNotFound,
            NpmBundleError::PackageJson { .. } => Outcome::PackageJson,
            NpmBundleError::Io { .. } => Outcome::Io,
            NpmBundleError::Compile { .. } => Outcome::Compile,
            NpmBundleError::GraphTooLarge { .. } => Outcome::GraphTooLarge,
            NpmBundleError::ExternalRefused { .. }
            | NpmBundleError::ExternalExportMissing { .. } => Outcome::ClientHostRefused,
        }
    }
}

/// Item 9.0 · the second question: **does the thing that loaded actually run?**
///
/// `Outcome::Loaded` means the graph resolved and lowered to artifacts. It has
/// never meant the package *executes*, and `NPM_COVERAGE.md` § Caveats 2 has
/// carried that limit as a standing admission since the 85.2% run. This answers
/// it for every specifier that gets that far.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EvalOutcome {
    /// The stage did not run (`ALBEDO_NPM_COVERAGE_EVAL` unset), or the
    /// specifier never loaded so there was nothing to evaluate.
    NotRun,
    /// The entry module's body executed without throwing.
    Ran,
    /// It threw. `detail` carries the message.
    Threw,
}

impl EvalOutcome {
    fn label(self) -> &'static str {
        match self {
            EvalOutcome::NotRun => "—",
            EvalOutcome::Ran => "runs",
            EvalOutcome::Threw => "THREW",
        }
    }
}

struct Probe {
    project: String,
    specifier: String,
    outcome: Outcome,
    /// Artifact count on success — a rough proxy for graph size, useful for
    /// spotting the packages that dominate bundle time.
    artifacts: usize,
    detail: String,
    /// How many source files write this specifier, from the import scan. The
    /// coverage number every previous run reported weights every specifier
    /// equally; this is what lets the report also say what fraction of real
    /// import *sites* would compile, which is the thing an author feels.
    sites: u32,
    eval: EvalOutcome,
    eval_detail: String,
}

/// Evaluate a bundle's entry module in a **fresh** QuickJS engine.
///
/// 🔑 **Fresh per specifier, deliberately.** `__albedo_require_record` memoizes
/// through `__ALBEDO_MODULES`, and a package's top-level side effects would
/// otherwise be visible to the next probe — so a shared engine would make this
/// measurement order-dependent, which is the one thing a measurement may not be.
///
/// The mechanism, in the order the runtime itself uses:
/// 1. `init` installs the npm linker helpers;
/// 2. each artifact registers a **lazy** factory — nothing executes yet;
/// 3. `__albedo_require_record(entry_key)` is what finally runs the module body.
///
/// Step 3 is why this is a real answer rather than a restatement of step 2: an
/// exception in the module body surfaces as `Err` from `load_module`.
///
/// 🪤 A `Threw` here is **not automatically a compatibility defect.** A package
/// reaching for a Node built-in, a browser global, or a top-level `await` is
/// three different findings — item 9.4, item 9.x and item 11 respectively — and
/// the report keeps the message so they can be told apart rather than pooled
/// into one discouraging number.
fn evaluate_bundle(bundle: &NpmDependencyBundle) -> (EvalOutcome, String) {
    use dom_render_compiler::runtime::engine::{BootstrapPayload, RuntimeEngine};
    use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;

    let mut engine = QuickJsEngine::new();
    if let Err(err) = engine.init(&BootstrapPayload::default()) {
        return (EvalOutcome::Threw, format!("engine init failed: {err}"));
    }

    for artifact in &bundle.artifacts {
        if let Err(err) =
            engine.load_precompiled_module(&artifact.key, &artifact.script, artifact.source_hash)
        {
            return (
                EvalOutcome::Threw,
                format!("registering '{}' failed: {err}", artifact.key),
            );
        }
    }

    let probe = format!(
        "globalThis.__albedo_require_record({});",
        serde_json::to_string(&bundle.entry_key).expect("a key is a string")
    );
    match engine.load_module("__albedo_npm_eval_probe__", &probe) {
        Ok(()) => (EvalOutcome::Ran, String::new()),
        Err(err) => (EvalOutcome::Threw, err.to_string()),
    }
}

fn probe_one(project: &str, dir: &Path, specifier: &str, sites: u32, run_eval: bool) -> Probe {
    match bundle_npm_dependency(dir, specifier) {
        Ok(bundle) => {
            let (eval, eval_detail) = if run_eval {
                evaluate_bundle(&bundle)
            } else {
                (EvalOutcome::NotRun, String::new())
            };
            Probe {
                project: project.to_string(),
                specifier: specifier.to_string(),
                outcome: Outcome::Loaded,
                artifacts: bundle.artifacts.len(),
                detail: String::new(),
                sites,
                eval,
                eval_detail,
            }
        }
        Err(err) => Probe {
            project: project.to_string(),
            specifier: specifier.to_string(),
            outcome: Outcome::from_error(&err),
            artifacts: 0,
            detail: err.to_string(),
            sites,
            eval: EvalOutcome::NotRun,
            eval_detail: String::new(),
        },
    }
}

#[test]
#[ignore = "needs a real node_modules tree; run explicitly with ALBEDO_NPM_COVERAGE_MANIFEST set"]
fn measure_npm_coverage_across_real_projects() {
    let Ok(manifest_path) = std::env::var("ALBEDO_NPM_COVERAGE_MANIFEST") else {
        eprintln!("SKIP: set ALBEDO_NPM_COVERAGE_MANIFEST to a projects JSON file");
        return;
    };

    let raw = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("read manifest '{manifest_path}': {err}"));
    let manifest: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|err| panic!("parse manifest: {err}"));
    let projects = manifest.as_array().expect("manifest is a JSON array");

    // Off by default: a fresh QuickJS engine per specifier is the correct
    // isolation and it is not free, so the resolution-only number stays the
    // cheap default and the harder question is opted into.
    let run_eval = std::env::var("ALBEDO_NPM_COVERAGE_EVAL").is_ok();
    if run_eval {
        eprintln!("eval stage ON — each loaded specifier also runs in a fresh QuickJS engine");
    }

    let mut probes: Vec<Probe> = Vec::new();

    for project in projects {
        let name = project["project"].as_str().expect("project name");
        let dir = PathBuf::from(project["dir"].as_str().expect("project dir"));
        if !dir.join("node_modules").is_dir() {
            eprintln!(
                "SKIP project '{name}': no node_modules at {}",
                dir.display()
            );
            continue;
        }
        // `specifiers` is either the original string array, or — as item 9.0's
        // import scan produces — an object mapping each specifier to the number
        // of source files that write it. Both are accepted so the manifests from
        // the 85.2% run still drive this harness unchanged; a string array just
        // weights every specifier as one site.
        let specifiers: Vec<(String, u32)> = match &project["specifiers"] {
            serde_json::Value::Array(list) => list
                .iter()
                .map(|s| (s.as_str().expect("specifier string").to_string(), 1))
                .collect(),
            serde_json::Value::Object(map) => map
                .iter()
                .map(|(spec, count)| (spec.clone(), count.as_u64().unwrap_or(1) as u32))
                .collect(),
            other => panic!("`specifiers` must be an array or an object, got {other}"),
        };

        eprintln!("── {name}: probing {} specifiers ──", specifiers.len());
        for (specifier, sites) in specifiers {
            let probe = probe_one(name, &dir, &specifier, sites, run_eval);
            eprintln!(
                "   {:<40} {}",
                probe.specifier,
                if probe.outcome == Outcome::Loaded {
                    match probe.eval {
                        EvalOutcome::NotRun => format!("OK ({} artifacts)", probe.artifacts),
                        EvalOutcome::Ran => format!("OK + runs ({} artifacts)", probe.artifacts),
                        EvalOutcome::Threw => {
                            format!("loads but THREW — {}", probe.eval_detail)
                        }
                    }
                } else {
                    // Print the detail inline. A bare class name sent the first
                    // run of this probe chasing a `react-dom/server` failure
                    // that the message would have explained immediately.
                    format!("{} — {}", probe.outcome.label(), probe.detail)
                }
            );
            probes.push(probe);
        }
    }

    let report = render_report(&probes);
    println!("{report}");

    if let Ok(path) = std::env::var("ALBEDO_NPM_COVERAGE_REPORT") {
        std::fs::write(&path, &report).unwrap_or_else(|err| panic!("write report '{path}': {err}"));
        eprintln!("report written to {path}");
    }
}

/// Coverage weighted by **import sites** — how much of the code an author
/// actually wrote would compile.
///
/// 🔑 Why this is reported beside the unique-package number rather than instead
/// of it: they answer different questions and can disagree sharply in both
/// directions. Ten failing specifiers that appear once each are a footnote; one
/// failing specifier imported in two hundred files is the whole project. The
/// unique number is what a comparison to the 85.2% run needs; this is what a
/// user feels, and quoting only the flattering one of the two is the rounding
/// error this harness's history is a warning about.
fn render_weighted(out: &mut String, probes: &[Probe]) {
    // Per (project, specifier) so a package shared by two projects contributes
    // both projects' site counts — the question is how many import lines exist,
    // not how many packages do.
    let mut total = 0u64;
    let mut ok = 0u64;
    let mut missing = 0u64;
    for probe in probes {
        let sites = u64::from(probe.sites);
        if probe.outcome == Outcome::PackageNotFound {
            missing += sites;
            continue;
        }
        total += sites;
        if probe.outcome == Outcome::Loaded {
            ok += sites;
        }
    }
    if total == 0 {
        return;
    }
    let pct = (ok as f64 / total as f64) * 100.0;
    out.push_str("## Weighted by import sites\n\n");
    let _ = writeln!(
        out,
        "**{ok} / {total} import sites resolve = {pct:.1}%** \
         ({missing} more sites name a package that is not on disk).\n"
    );
    let _ = writeln!(
        out,
        "A *site* is one source file writing one specifier. This is the number an \
         author experiences; the unique-package number above is the one comparable \
         to earlier runs.\n"
    );
}

/// Root specifiers vs subpaths — the bias item 9.0 exists to quantify.
///
/// The 85.2% run probed only root entries (`import "next"`). A package's root is
/// typically its heaviest and most server-oriented entry, so probing it is
/// probing the specifier most likely to fail *and* often one real code never
/// writes. This table is what turns that suspicion into a number.
fn render_subpath_split(out: &mut String, probes: &[Probe]) {
    fn is_subpath(spec: &str) -> bool {
        let segments = spec.split('/').count();
        if spec.starts_with('@') {
            segments > 2
        } else {
            segments > 1
        }
    }

    let mut rows: Vec<(&str, usize, usize)> = Vec::new();
    for (label, want_subpath) in [("root specifiers", false), ("subpath specifiers", true)] {
        let considered: Vec<&Probe> = probes
            .iter()
            .filter(|p| is_subpath(&p.specifier) == want_subpath)
            .filter(|p| p.outcome != Outcome::PackageNotFound)
            .collect();
        let loaded = considered
            .iter()
            .filter(|p| p.outcome == Outcome::Loaded)
            .count();
        rows.push((label, loaded, considered.len()));
    }
    if rows.iter().all(|(_, _, total)| *total == 0) {
        return;
    }

    out.push_str("## Root vs subpath\n\n");
    out.push_str("| kind | loaded | on disk | coverage |\n|---|---:|---:|---:|\n");
    for (label, loaded, total) in &rows {
        let pct = if *total == 0 {
            0.0
        } else {
            (*loaded as f64 / *total as f64) * 100.0
        };
        let _ = writeln!(out, "| {label} | {loaded} | {total} | {pct:.1}% |");
    }
    out.push('\n');
}

/// The second question: of the specifiers that load, how many actually run?
fn render_eval(out: &mut String, probes: &[Probe]) {
    let ran = probes.iter().filter(|p| p.eval == EvalOutcome::Ran).count();
    let threw: Vec<&Probe> = probes
        .iter()
        .filter(|p| p.eval == EvalOutcome::Threw)
        .collect();
    if ran == 0 && threw.is_empty() {
        return;
    }

    let attempted = ran + threw.len();
    let pct = (ran as f64 / attempted as f64) * 100.0;
    out.push_str("## Does it run? (evaluated under QuickJS)\n\n");
    let _ = writeln!(
        out,
        "**{ran} / {attempted} of the specifiers that load also execute = {pct:.1}%**\n"
    );
    out.push_str(
        "This retires `NPM_COVERAGE.md` § Caveats 2, which said resolution is not \
         execution and left the second question untouched. A throw below is **not \
         automatically a compatibility defect** — a missing Node built-in is item 9.4, \
         a browser global is item 9.x, and a top-level `await` belongs to item 11's \
         async bridge. Read the message, not the count.\n\n",
    );

    if !threw.is_empty() {
        out.push_str("| specifier | threw with |\n|---|---|\n");
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for probe in &threw {
            seen.entry(probe.specifier.as_str())
                .or_insert(probe.eval_detail.as_str());
        }
        for (specifier, detail) in &seen {
            // Strip the wrapper prefixes so the column carries the *cause*. The
            // first pass of this table clipped at 160 characters and every row
            // was consumed by `LoadError[engine_failure]: failed to load
            // precompiled module '<300-character path>'` — the diagnosis was
            // present and off the right-hand edge.
            let one_line = detail.replace('\n', " ").replace('|', "\\|");
            let cause = one_line
                .rsplit_once("': ")
                .map_or(one_line.as_str(), |(_, tail)| tail);
            let clipped: String = cause.chars().take(200).collect();
            let _ = writeln!(out, "| `{specifier}` | {clipped} |");
        }
        out.push('\n');
    }
}

fn render_report(probes: &[Probe]) -> String {
    let mut out = String::new();
    out.push_str("# npm coverage measurement\n\n");

    // Per project.
    out.push_str("## Per project\n\n");
    out.push_str("| project | probed | on disk | loaded | coverage |\n");
    out.push_str("|---|---:|---:|---:|---:|\n");

    let mut by_project: BTreeMap<&str, Vec<&Probe>> = BTreeMap::new();
    for probe in probes {
        by_project
            .entry(probe.project.as_str())
            .or_default()
            .push(probe);
    }

    for (project, rows) in &by_project {
        let probed = rows.len();
        let on_disk = rows
            .iter()
            .filter(|p| p.outcome != Outcome::PackageNotFound)
            .count();
        let loaded = rows.iter().filter(|p| p.outcome == Outcome::Loaded).count();
        let pct = if on_disk == 0 {
            0.0
        } else {
            (loaded as f64 / on_disk as f64) * 100.0
        };
        let _ = writeln!(
            out,
            "| {project} | {probed} | {on_disk} | {loaded} | {pct:.1}% |"
        );
    }

    // Overall, counted over unique specifiers so a package shared by three
    // projects is not counted three times.
    let mut unique: BTreeMap<&str, Outcome> = BTreeMap::new();
    for probe in probes {
        let entry = unique
            .entry(probe.specifier.as_str())
            .or_insert(probe.outcome);
        // A package that loads anywhere is a package the bundler can load.
        if probe.outcome == Outcome::Loaded {
            *entry = Outcome::Loaded;
        }
    }
    let unique_on_disk = unique
        .values()
        .filter(|o| **o != Outcome::PackageNotFound)
        .count();
    let unique_loaded = unique.values().filter(|o| **o == Outcome::Loaded).count();
    let unique_pct = if unique_on_disk == 0 {
        0.0
    } else {
        (unique_loaded as f64 / unique_on_disk as f64) * 100.0
    };

    let _ = writeln!(
        out,
        "\n**Unique packages: {unique_loaded} / {unique_on_disk} load = {unique_pct:.1}%** \
         (of {} distinct specifiers probed)\n",
        unique.len()
    );

    render_weighted(&mut out, probes);
    render_subpath_split(&mut out, probes);
    render_eval(&mut out, probes);

    // Failure breakdown.
    out.push_str("## Failure classes (unique packages)\n\n");
    out.push_str("| class | count |\n|---|---:|\n");
    let mut class_counts: BTreeMap<Outcome, usize> = BTreeMap::new();
    for outcome in unique.values() {
        *class_counts.entry(*outcome).or_insert(0) += 1;
    }
    for (outcome, count) in &class_counts {
        let _ = writeln!(out, "| {} | {count} |", outcome.label());
    }

    // Every distinct failure, with the first message seen.
    out.push_str("\n## Failing packages\n\n");
    let mut seen: BTreeMap<&str, &Probe> = BTreeMap::new();
    for probe in probes {
        if probe.outcome.counts_against_bundler()
            && unique.get(probe.specifier.as_str()) != Some(&Outcome::Loaded)
        {
            seen.entry(probe.specifier.as_str()).or_insert(probe);
        }
    }
    if seen.is_empty() {
        out.push_str("_None._\n");
    } else {
        out.push_str("| package | class | detail |\n|---|---|---|\n");
        for (specifier, probe) in &seen {
            let detail = probe.detail.replace('|', "\\|");
            let detail = if detail.len() > 200 {
                format!("{}…", &detail[..200])
            } else {
                detail
            };
            let _ = writeln!(
                out,
                "| `{specifier}` | {} | {detail} |",
                probe.outcome.label()
            );
        }
    }

    // Heaviest graphs — these are what dominate bundle time.
    out.push_str("\n## Largest graphs (artifact count)\n\n");
    let mut heavy: Vec<&Probe> = probes
        .iter()
        .filter(|p| p.outcome == Outcome::Loaded)
        .collect();
    heavy.sort_by(|a, b| b.artifacts.cmp(&a.artifacts));
    heavy.dedup_by(|a, b| a.specifier == b.specifier);
    out.push_str("| package | files |\n|---|---:|\n");
    for probe in heavy.iter().take(15) {
        let _ = writeln!(out, "| `{}` | {} |", probe.specifier, probe.artifacts);
    }

    out
}
