//! A build never carries rows from the machine's own `forge.db`.
//!
//! `albedo build` pre-renders each Tier-B node so a page has content before its
//! per-request render arrives (`TierBNode::initial_html`), and the server seeds
//! that HTML into the shell for any node it did not render per request — every
//! node, on the WebTransport path. It used to pre-render against the project's
//! own `forge.db` — so whatever rows sat on the machine that ran the build were
//! baked into `.albedo/dist`, packed by `albedo ship` into the binary or image,
//! and sent in the shell to visitors over WebTransport. On a laptop that is the
//! developer's test data; on a build box next to production it is production
//! data, in an artifact.
//!
//! The pre-render now reads what a fresh deployment of this source would hold:
//! the declared schema and its `seed` rows, in a throwaway database. Watched
//! failing on the old build (the local row was in the manifest).
//!
//! Its own test binary, holding one database at a time: the libSQL teardown
//! crash needs several alive at once.

use dom_render_compiler::forge::{skeleton, ForgeSchema, LibSqlSubstrate};
use dom_render_compiler::forge::value::SqlValue;
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

fn run_ok(cmd: &mut Command, what: &str) -> String {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{what} failed\n{text}");
    text
}

/// Every file under `dir` whose bytes contain `needle`.
fn files_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
            {
                found.push(path);
            }
        }
    }
    found
}

/// Put a row in the project's own `forge.db`, with the schema the app declares.
async fn write_local_row(app: &Path, message: &str) {
    let declarations = dom_render_compiler::dev::contract::load_forge_declarations(app).unwrap();
    let schema = ForgeSchema::from_declarations(&declarations).unwrap();
    let substrate = LibSqlSubstrate::open_local(app.join("forge.db")).await.unwrap();
    skeleton::bootstrap_schema(&substrate, &schema).await.unwrap();
    use dom_render_compiler::forge::DataSubstrate;
    substrate
        .execute(
            "INSERT INTO guestbook (author, message) VALUES (?1, ?2)",
            &[
                SqlValue::Text("someone local".to_string()),
                SqlValue::Text(message.to_string()),
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_build_bakes_declared_seeds_and_never_the_local_database() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");

    // A declared seed row: source code, so a fresh deployment shows it and the
    // pre-render should too.
    let config_path = app.join("albedo.config.ts");
    let config = std::fs::read_to_string(&config_path).unwrap();
    let declared = "    guestbook: {\n      fields: { author: \"text\", message: \"text\" },\n";
    assert!(config.contains(declared), "the scaffold's guestbook block moved");
    std::fs::write(
        &config_path,
        config.replace(
            declared,
            &format!("{declared}      seed: [{{ author: \"the config\", message: \"DECLARED-SEED-ROW\" }}],\n"),
        ),
    )
    .unwrap();

    // A row that exists only on this machine.
    write_local_row(&app, "LOCAL-ONLY-ROW").await;

    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");
    let dist = app.join(".albedo").join("dist");

    let leaked = files_containing(&dist, "LOCAL-ONLY-ROW");
    assert!(
        leaked.is_empty(),
        "the build baked a row from the local forge.db into {leaked:?}"
    );

    // Not fixed by switching the pre-render off: the page still has its
    // content as HTML before any request, rendered from the declared seed.
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dist.join("render-manifest.v2.json")).unwrap(),
    )
    .unwrap();
    let initial = manifest["routes"]["/guestbook"]["tier_b"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|node| node["initial_html"].as_str())
        .collect::<Vec<_>>()
        .join("");
    assert!(initial.contains("class=\"ledger\""), "no pre-rendered guestbook:\n{initial}");
    assert!(
        initial.contains("DECLARED-SEED-ROW"),
        "the declared seed row was not pre-rendered:\n{initial}"
    );
}

/// The build reads no database, so it creates none: a fresh app has no
/// `forge.db` until something that serves it opens one.
#[test]
fn a_build_does_not_create_the_projects_database() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    run_ok(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    let app = work.path().join("app");

    run_ok(albedo(home.path(), &app).arg("build"), "albedo build");
    assert!(
        !app.join("forge.db").exists(),
        "albedo build created the project's forge.db"
    );
}
