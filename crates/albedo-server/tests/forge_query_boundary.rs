//! T-0033 · Q1a — the early refusal boundary (Acceptance 8, Falsifier 1).
//!
//! Before Q1a, misusing an `albedo/forge` collection outside `useSharedSlot`
//! was silent: `guestbook.where({ author: "ada" }).count()` built clean and
//! `GET /probe` served HTTP 200 with an error inject and nothing in the log.
//! Now every value use of a collection is classified before anything renders,
//! and a new-style query is lowered and then refused by name (running it
//! arrives with Q1b) — in `albedo build` and `albedo serve`, which share
//! `run_prod_build_with_budget`.
//!
//! Lives in this crate, not `tests/forge_query_boundary.rs` at the root, because
//! it drives the real `albedo` binary (`CARGO_BIN_EXE_albedo` exists only here).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd
}

fn scaffold(home: &Path, work: &Path) -> PathBuf {
    let out = albedo(home, work).args(["init", "app"]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    work.join("app")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The Why 2 probe. Line 4, column 19 is `guestbook`.
const PROBE: &str = r#"import { guestbook } from "albedo/forge";

export default async function Probe() {
  const n = await guestbook.where({ author: "ada" }).count();
  return <p>{n}</p>;
}
"#;

/// A probe whose page would ALSO fail to render (it throws), so the test can
/// tell which error comes first.
const PROBE_THAT_ALSO_THROWS: &str = r#"import { guestbook } from "albedo/forge";

export default async function Probe() {
  const n = await guestbook.where({ author: "ada" }).count();
  throw new Error("render-should-never-run");
  return <p>{n}</p>;
}
"#;

const ALIAS: &str = r#"import { guestbook } from "albedo/forge";

const x = guestbook;

export default function Alias() {
  return <p>{String(x)}</p>;
}
"#;

#[test]
fn a_scaffold_without_new_style_queries_still_builds() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        !app.join(".albedo/query-report.txt").exists(),
        "no query report for an app with no new-style query (Acceptance 7)"
    );
}

#[test]
fn build_refuses_a_new_style_query_naming_its_line() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    fs::write(app.join("src/routes/probe.tsx"), PROBE).unwrap();

    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    let all = text(&out);
    assert!(!out.status.success(), "the probe must fail the build:\n{all}");
    assert!(
        all.contains("src/routes/probe.tsx:4:19"),
        "names the call site in source coordinates:\n{all}"
    );
    assert!(all.contains("compiled and lowered"), "{all}");
    assert!(all.contains("Q1b"), "says what runs it:\n{all}");
    let report = fs::read_to_string(app.join(".albedo/query-report.txt"))
        .expect("the build wrote the query report");
    assert!(all.contains("query-report.txt"), "the refusal points at it:\n{all}");
    assert!(report.contains("src/routes/probe.tsx:4:19"), "{report}");
    assert!(
        report.contains("SELECT COUNT(*) AS a0 FROM guestbook WHERE author IS ?1"),
        "{report}"
    );

    // Remove the probe: the next build succeeds and the stale report is gone.
    fs::remove_file(app.join("src/routes/probe.tsx")).unwrap();
    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(!app.join(".albedo/query-report.txt").exists());
}

#[test]
fn the_query_refusal_is_the_first_error_ahead_of_render_failures() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    fs::write(app.join("src/routes/probe.tsx"), PROBE_THAT_ALSO_THROWS).unwrap();

    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    let all = text(&out);
    assert!(!out.status.success(), "{all}");
    assert!(all.contains("src/routes/probe.tsx:4:19"), "{all}");
    assert!(
        !all.contains("render-should-never-run"),
        "nothing rendered before the refusal:\n{all}"
    );
}

#[test]
fn serve_refuses_the_same_probe_before_listening() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    fs::write(app.join("src/routes/probe.tsx"), PROBE).unwrap();

    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    // On the old behaviour `serve` starts and listens forever, so bound the wait:
    // still running after 60 s means it got past the boundary, which is a failure.
    let mut child = albedo(home.path(), &app)
        .args(["serve", ".", "--port", &port.to_string()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let still_running = loop {
        if child.try_wait().unwrap().is_some() {
            break false;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let out = child.wait_with_output().unwrap();
    let all = text(&out);
    assert!(!still_running, "serve was still running after 60 s:\n{all}");
    assert!(!out.status.success(), "{all}");
    assert!(all.contains("src/routes/probe.tsx:4:19"), "{all}");
    assert!(!all.contains("serving"), "never started serving:\n{all}");
}

/// Acceptance 9: a new-style query is classified, and refused by path, in every
/// kind of compiled runtime module — not only route components.
#[test]
fn a_query_is_refused_in_every_kind_of_runtime_module() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    let src = app.join("src");
    let q = "await guestbook.where({ author: \"ada\" }).count()";
    let files: &[(&str, String)] = &[
        (
            "routes/component.tsx",
            format!("import {{ guestbook }} from \"albedo/forge\";\nexport default async function C() {{\n  const n = {q};\n  return <p>{{n}}</p>;\n}}\n"),
        ),
        (
            "routes/with-action.tsx",
            format!("import {{ action }} from \"albedo\";\nimport {{ guestbook }} from \"albedo/forge\";\nexport const tally = action(async () => {{\n  return {q};\n}});\nexport default function W() {{\n  return <p>w</p>;\n}}\n"),
        ),
        (
            "middleware.ts",
            format!("import {{ guestbook }} from \"albedo/forge\";\nexport default async function middleware(req, next) {{\n  const n = {q};\n  return next();\n}}\n"),
        ),
        (
            "jobs/nightly.ts",
            format!("import {{ guestbook }} from \"albedo/forge\";\nexport default async function nightly() {{\n  return {q};\n}}\n"),
        ),
        (
            "lib/helper.ts",
            format!("import {{ guestbook }} from \"albedo/forge\";\nexport async function tally() {{\n  return {q};\n}}\n"),
        ),
        (
            "routes/uses-helper.tsx",
            "import { tally } from \"../lib/helper\";\nexport default async function U() {\n  return <p>{await tally()}</p>;\n}\n".to_string(),
        ),
        (
            "lib/initializer.ts",
            format!("import {{ guestbook }} from \"albedo/forge\";\nexport const total = {q};\n"),
        ),
    ];
    for (path, body) in files {
        let file = src.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, body).unwrap();
    }

    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    let all = text(&out);
    assert!(!out.status.success(), "{all}");
    for site in [
        "src/routes/component.tsx:3:19",
        "src/routes/with-action.tsx:4:16",
        "src/middleware.ts:3:19",
        "src/jobs/nightly.ts:3:16",
        "src/lib/helper.ts:3:16",
        "src/lib/initializer.ts:2:28",
    ] {
        assert!(all.contains(site), "missing {site}:\n{all}");
    }
    assert!(
        all.contains("FORGE reads refused before anything rendered (6)"),
        "exactly the six queries, nothing for the module that only imports a helper:\n{all}"
    );
}

#[test]
fn a_bare_collection_value_fails_with_its_own_diagnostic() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let app = scaffold(home.path(), work.path());
    fs::write(app.join("src/routes/alias.tsx"), ALIAS).unwrap();

    let out = albedo(home.path(), &app).args(["build", "."]).output().unwrap();
    let all = text(&out);
    assert!(!out.status.success(), "{all}");
    assert!(all.contains("src/routes/alias.tsx:3:11"), "{all}");
    assert!(
        all.contains("`guestbook` is a collection"),
        "its own diagnostic, not the query refusal:\n{all}"
    );
}
