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

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, NpmBundleError};
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
}

fn probe_one(project: &str, dir: &Path, specifier: &str) -> Probe {
    match bundle_npm_dependency(dir, specifier) {
        Ok(bundle) => Probe {
            project: project.to_string(),
            specifier: specifier.to_string(),
            outcome: Outcome::Loaded,
            artifacts: bundle.artifacts.len(),
            detail: String::new(),
        },
        Err(err) => Probe {
            project: project.to_string(),
            specifier: specifier.to_string(),
            outcome: Outcome::from_error(&err),
            artifacts: 0,
            detail: err.to_string(),
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
        let specifiers = project["specifiers"]
            .as_array()
            .expect("specifiers array")
            .iter()
            .map(|s| s.as_str().expect("specifier string").to_string())
            .collect::<Vec<_>>();

        eprintln!("── {name}: probing {} specifiers ──", specifiers.len());
        for specifier in specifiers {
            let probe = probe_one(name, &dir, &specifier);
            eprintln!(
                "   {:<40} {}",
                probe.specifier,
                if probe.outcome == Outcome::Loaded {
                    format!("OK ({} artifacts)", probe.artifacts)
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
