//! `albedo db backup` / `restore` through the real binary — the drill an
//! operator runs: rows exist, a backup is taken, more rows arrive, the restore
//! is staged *while the server is still serving*, the server restarts, and the
//! app is back at the backup with the replaced database kept beside it.
//!
//! Also: a scheduled backup appears as soon as the server boots with
//! `ALBEDO_BACKUP_DIR` set, and a backup that no longer fits the declared
//! schema is refused before anything is staged.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn albedo(home: &Path, cwd: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd.env_remove("ALBEDO_BACKUP_DIR");
    cmd.env_remove("ALBEDO_FORGE_DB");
    cmd
}

fn run(cmd: &mut Command, what: &str) -> (bool, String) {
    let out: Output = cmd.output().unwrap_or_else(|err| panic!("{what}: {err}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// Start `serve`, wait until it answers, and return it with its base URL and
/// the file its stdout goes to.
async fn serve(home: &Path, app: &Path, env: &[(&str, &str)], log: &str) -> (Server, String, PathBuf) {
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("addr").port()
    };
    let stdout = app.join(format!("{log}.stdout.log"));
    let mut cmd = albedo(home, app);
    cmd.args(["serve", ".", "--port", &port.to_string()])
        .envs(env.iter().copied())
        .stdout(fs::File::create(&stdout).expect("stdout log"))
        .stderr(fs::File::create(app.join(format!("{log}.stderr.log"))).expect("stderr log"));
    let server = Server {
        child: cmd.spawn().expect("albedo serve starts"),
    };
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(90);
    while reqwest::get(format!("{base}/_albedo/runtime.js")).await.is_err() {
        assert!(
            Instant::now() < deadline,
            "albedo serve never answered on {base}\n{}",
            fs::read_to_string(app.join(format!("{log}.stderr.log"))).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base, stdout)
}

async fn page(base: &str) -> String {
    reqwest::get(format!("{base}/list"))
        .await
        .expect("GET /list")
        .text()
        .await
        .expect("body")
}

const LIST_PAGE: &str = r#"import { useSharedSlot } from "albedo";

interface Entry { id: number; message: string }

export default function List() {
  const entries = useSharedSlot<Entry[]>("guestbook");
  return (
    <ul>
      {entries.map((entry) => (
        <li key={entry.id}>{entry.id}:{entry.message}</li>
      ))}
    </ul>
  );
}
"#;

fn import(home: &Path, app: &Path, name: &str, rows: &str) {
    fs::write(app.join(name), rows).unwrap();
    let (ok, text) = run(
        albedo(home, app).args(["import", "guestbook", name]),
        "albedo import",
    );
    assert!(ok, "albedo import failed\n{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_staged_while_serving_lands_on_the_next_start() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (ok, text) = run(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    assert!(ok, "albedo init failed\n{text}");
    let app = work.path().join("app");
    fs::write(app.join("src/routes/list.tsx"), LIST_PAGE).unwrap();

    import(
        home.path(),
        &app,
        "before.jsonl",
        "{\"id\": 1, \"author\": \"a\", \"message\": \"kept one\"}\n\
         {\"id\": 2, \"author\": \"a\", \"message\": \"kept two\"}\n",
    );

    // Nothing to back up yet is a refusal, not an empty backup — but here
    // there is a database, so this is the real one.
    let (ok, text) = run(albedo(home.path(), &app).args(["db", "backup"]), "albedo db backup");
    assert!(ok, "albedo db backup failed\n{text}");
    assert!(text.contains("backup taken and checked"), "{text}");
    let snapshots: Vec<PathBuf> = fs::read_dir(app.join("backups"))
        .expect("backups/ beside the database")
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert!(snapshots[0].join("manifest.json").is_file());

    import(
        home.path(),
        &app,
        "after.jsonl",
        "{\"id\": 3, \"author\": \"a\", \"message\": \"written after the backup\"}\n",
    );

    let (ok, text) = run(albedo(home.path(), &app).args(["db", "backups"]), "albedo db backups");
    let name = snapshots[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(ok && text.contains(&name), "the snapshot is not listed\n{text}");

    {
        let (_server, base, stdout) = serve(
            home.path(),
            &app,
            &[("ALBEDO_BACKUP_DIR", "scheduled")],
            "first",
        )
        .await;
        let before = page(&base).await;
        assert!(before.contains("3:written after the backup"), "{before}");

        // The schedule takes one at boot: there were none in its directory.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let taken = fs::read_dir(app.join("scheduled"))
                .map(|entries| {
                    entries
                        .flatten()
                        .any(|entry| entry.path().join("manifest.json").is_file())
                })
                .unwrap_or(false);
            if taken {
                break;
            }
            assert!(Instant::now() < deadline, "no scheduled backup appeared");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let banner = fs::read_to_string(&stdout).unwrap_or_default();
        assert!(banner.contains("every 1d to"), "the schedule is not announced\n{banner}");

        // Staged while serving: checked, compared against what is live, and
        // the running server is untouched by it.
        let (ok, text) = run(
            albedo(home.path(), &app).args(["db", "restore", "latest"]),
            "albedo db restore",
        );
        assert!(ok, "albedo db restore failed\n{text}");
        assert!(text.contains("staged and checked"), "{text}");
        assert!(text.contains("2 rows (now 3)"), "the comparison with live is missing\n{text}");
        assert!(page(&base).await.contains("3:written after the backup"));
    }

    // The next start applies it, says so, and keeps what it replaced.
    let (_server, base, stdout) = serve(home.path(), &app, &[], "second").await;
    let after = page(&base).await;
    assert!(after.contains("1:kept one") && after.contains("2:kept two"), "{after}");
    assert!(!after.contains("written after the backup"), "the restore did not land\n{after}");
    let banner = fs::read_to_string(&stdout).unwrap_or_default();
    assert!(banner.contains("BACKUP · restored the backup taken"), "{banner}");
    assert!(banner.contains("backups are off"), "an unscheduled server must say so\n{banner}");
    assert!(!app.join("forge.db.restore-pending").exists());

    // The replaced database is kept — and whole. The first server was killed,
    // not stopped, so its last commits were still in its `-wal`; the swap moves
    // that with it under the same name, and opening the pair recovers them.
    let kept: Vec<PathBuf> = fs::read_dir(&app)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path.file_name().unwrap().to_string_lossy();
            name.starts_with("forge.db.before-restore-") && !name.ends_with("-wal")
        })
        .collect();
    assert_eq!(kept.len(), 1, "one replaced database is kept: {kept:?}");
    let replaced = dom_render_compiler::forge::LibSqlSubstrate::open_local(&kept[0])
        .await
        .expect("the kept database opens");
    let rows = dom_render_compiler::forge::DataSubstrate::query(
        &replaced,
        "SELECT message FROM guestbook WHERE id = 3",
        &[],
    )
    .await
    .expect("the kept database holds the guestbook");
    assert_eq!(rows.rows.len(), 1, "the row written after the backup is lost");
}

/// Found by review 2026-10-30: restore read the live database for its
/// before/after comparison and stopped on the error — so a corrupt database,
/// the commonest reason to restore, blocked its own recovery. The comparison is
/// now best effort; the backup is still checked in full.
#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_live_database_does_not_block_its_own_restore() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (ok, text) = run(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    assert!(ok, "albedo init failed\n{text}");
    let app = work.path().join("app");
    fs::write(app.join("src/routes/list.tsx"), LIST_PAGE).unwrap();
    import(
        home.path(),
        &app,
        "rows.jsonl",
        "{\"id\": 1, \"author\": \"a\", \"message\": \"survives the damage\"}\n",
    );
    let (ok, text) = run(albedo(home.path(), &app).args(["db", "backup"]), "albedo db backup");
    assert!(ok, "{text}");

    for sidecar in ["forge.db-wal", "forge.db-shm"] {
        let _ = fs::remove_file(app.join(sidecar));
    }
    fs::write(app.join("forge.db"), vec![0xA5u8; 8192]).unwrap();

    let (ok, text) = run(
        albedo(home.path(), &app).args(["db", "restore", "latest"]),
        "albedo db restore",
    );
    assert!(ok, "a damaged live database blocked the restore\n{text}");
    assert!(text.contains("could not be read"), "the missing comparison is not said\n{text}");
    assert!(app.join("forge.db.restore-pending").exists());

    let (_server, base, _stdout) = serve(home.path(), &app, &[], "restored").await;
    assert!(page(&base).await.contains("1:survives the damage"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_that_no_longer_fits_the_schema_is_refused_before_staging() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let (ok, text) = run(albedo(home.path(), work.path()).args(["init", "app"]), "albedo init");
    assert!(ok, "albedo init failed\n{text}");
    let app = work.path().join("app");

    // No database yet: nothing to back up, and nothing invented to back up.
    let (ok, text) = run(albedo(home.path(), &app).args(["db", "backup"]), "albedo db backup");
    assert!(!ok && text.contains("nothing to back up"), "{text}");
    assert!(!app.join("forge.db").exists(), "a backup created the database it was asked to copy");

    import(
        home.path(),
        &app,
        "rows.jsonl",
        "{\"id\": 1, \"author\": \"a\", \"message\": \"m\"}\n",
    );
    let (ok, text) = run(albedo(home.path(), &app).args(["db", "backup"]), "albedo db backup");
    assert!(ok, "{text}");

    // The app drops a field the backup's table still has.
    let config = app.join("albedo.config.ts");
    let declared = fs::read_to_string(&config).unwrap();
    let narrowed = declared.replace(
        "fields: { author: \"text\", message: \"text\" }",
        "fields: { message: \"text\" }",
    );
    assert_ne!(declared, narrowed, "the scaffold's guestbook declaration moved");
    fs::write(&config, narrowed).unwrap();

    let (ok, text) = run(
        albedo(home.path(), &app).args(["db", "restore", "latest"]),
        "albedo db restore",
    );
    assert!(!ok, "a restore the server would refuse to boot on was staged\n{text}");
    assert!(text.contains("does not fit the app as it is declared now"), "{text}");
    assert!(text.contains("author"), "the drifted field is not named\n{text}");
    assert!(!app.join("forge.db.restore-pending").exists());
}
