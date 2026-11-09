//! R1a · `albedo roles grant | revoke | list` through the real binary.
//!
//! The database is prepared in this process (two signed-up people, and a third
//! sharing an email), closed, and then handed to the CLI — a separate process,
//! as an operator's terminal is. What the CLI wrote is read back here after it
//! exits. Nothing holds the file while the CLI runs.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{load_principal, upsert_principal, ProviderProfile};
use dom_render_compiler::auth::PrincipalId;
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::LibSqlSubstrate;
use std::fs;
use std::path::Path;
use std::process::Command;

const NOW: i64 = 1_754_400_000_000;

fn albedo(home: &Path, cwd: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd);
    cmd.env("USERPROFILE", home);
    cmd.env("HOME", home);
    cmd.env("NO_COLOR", "1");
    cmd.env("ALBEDO_NO_TUI", "1");
    cmd.env("ALBEDO_FORGE_DB", db);
    cmd
}

fn run(cmd: &mut Command) -> (bool, String) {
    let out = cmd.output().expect("albedo runs");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

async fn person(db: &LibSqlSubstrate, subject: &str, email: &str) -> PrincipalId {
    let profile = ProviderProfile {
        email: Some(email.to_string()),
        name: None,
        image: None,
        claims: serde_json::Map::new(),
    };
    upsert_principal(db, "password", subject, &profile, NOW)
        .await
        .expect("principal")
        .id
}

async fn roles_of(db_path: &Path, id: &PrincipalId) -> Vec<String> {
    let db = LibSqlSubstrate::open_local(db_path).await.expect("reopen");
    load_principal(&db, id).await.expect("load").expect("exists").roles
}

#[tokio::test]
async fn an_operator_grants_lists_and_revokes_roles() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let (ok, text) = run(Command::new(env!("CARGO_BIN_EXE_albedo"))
        .current_dir(work.path())
        .env("USERPROFILE", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1")
        .args(["init", "app"]));
    assert!(ok, "albedo init failed\n{text}");
    let app = work.path().join("app");

    // Declare roles in the scaffold's own auth block.
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    let declared = config.replacen(
        "login: \"/sign-in\",",
        "login: \"/sign-in\",\n    roles: [\"payroll\", \"hr\"],",
        1,
    );
    assert_ne!(declared, config, "the scaffold's auth block has a login line to anchor on");
    fs::write(&config_path, declared).expect("write config");

    let db_path = work.path().join("data").join("forge.db");
    fs::create_dir_all(db_path.parent().expect("dir")).expect("data dir");
    let (ada, bob_1, bob_2) = {
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        let auth_schema = schema::augment(&ForgeSchema::build(Vec::new()).expect("empty"))
            .expect("auth tables");
        bootstrap_schema(&db, &auth_schema).await.expect("bootstrap");
        (
            person(&db, "ada", "ada@example.com").await,
            person(&db, "bob-1", "bob@example.com").await,
            person(&db, "bob-2", "bob@example.com").await,
        )
    };

    // ── grant by email ──
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "grant",
        "ada@example.com",
        "payroll",
    ]));
    assert!(ok, "grant failed\n{text}");
    assert_eq!(roles_of(&db_path, &ada).await, ["payroll"]);

    // ── a second grant changes nothing, and says so ──
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "grant",
        ada.as_str(),
        "payroll",
    ]));
    assert!(ok, "a repeated grant is not an error\n{text}");
    assert!(text.contains("already"), "{text}");

    // ── refusals ──
    for (args, needle) in [
        (vec!["roles", "grant", "ada@example.com", "finance"], "finance"),
        (vec!["roles", "grant", "nobody@example.com", "payroll"], "nobody@example.com"),
        (vec!["roles", "grant", "bob@example.com", "payroll"], bob_1.as_str()),
        (vec!["roles", "grant", "ada@example.com"], "needs"),
    ] {
        let (ok, text) = run(albedo(home.path(), &app, &db_path).args(&args));
        assert!(!ok, "{args:?} must fail\n{text}");
        assert!(text.contains(needle), "{args:?} must name `{needle}`\n{text}");
    }
    assert!(roles_of(&db_path, &bob_1).await.is_empty());
    assert!(roles_of(&db_path, &bob_2).await.is_empty());

    // ── the ambiguous email, resolved by id ──
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "grant",
        bob_2.as_str(),
        "hr",
    ]));
    assert!(ok, "grant by id\n{text}");

    // ── list, all and one role ──
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args(["roles", "list"]));
    assert!(ok, "list\n{text}");
    assert!(text.contains("ada@example.com") && text.contains(bob_2.as_str()), "{text}");
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args(["roles", "list", "--role", "hr"]));
    assert!(ok, "list --role\n{text}");
    assert!(text.contains(bob_2.as_str()) && !text.contains("ada@example.com"), "{text}");

    // ── revoke ──
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "revoke",
        "ada@example.com",
        "payroll",
    ]));
    assert!(ok, "revoke\n{text}");
    assert!(roles_of(&db_path, &ada).await.is_empty());
    assert_eq!(roles_of(&db_path, &bob_2).await, ["hr"]);
}

/// A syntactically valid id must not bypass the CLI's unknown-person refusal.
#[tokio::test]
async fn revoke_refuses_an_unknown_principal_id() {
    let (home, work, app, db_path, _) = role_rig().await;
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "revoke",
        "u_doesnotexist",
        "payroll",
    ]));
    assert!(
        !ok,
        "an unknown account must be refused, not reported as a no-op\n{text}"
    );
    assert!(text.contains("u_doesnotexist"), "{text}");
    drop(work);
}

/// The work order requires the CLI to refuse undeclared roles on both writes.
#[tokio::test]
async fn revoke_refuses_an_undeclared_role() {
    let (home, work, app, db_path, id) = role_rig().await;
    let (ok, text) =
        run(albedo(home.path(), &app, &db_path).args(["roles", "revoke", id.as_str(), "finance"]));
    assert!(!ok, "an undeclared role must be refused\n{text}");
    assert!(text.contains("finance"), "{text}");
    drop(work);
}

async fn role_rig() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    PrincipalId,
) {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let (ok, text) = run(Command::new(env!("CARGO_BIN_EXE_albedo"))
        .current_dir(work.path())
        .env("USERPROFILE", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1")
        .args(["init", "app"]));
    assert!(ok, "init\n{text}");
    let app = work.path().join("app");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path).expect("config");
    let declared = config.replacen(
        "login: \"/sign-in\",",
        "login: \"/sign-in\",\n    roles: [\"payroll\", \"hr\"],",
        1,
    );
    assert_ne!(declared, config);
    fs::write(config_path, declared).expect("declare roles");
    let db_path = work.path().join("data with spaces").join("forge.db");
    fs::create_dir_all(db_path.parent().expect("parent")).expect("mkdir");
    let id = {
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        let tables =
            schema::augment(&ForgeSchema::build(Vec::new()).expect("schema")).expect("auth");
        bootstrap_schema(&db, &tables).await.expect("bootstrap");
        person(&db, "ada", "ada@example.com").await
    };
    (home, work, app, db_path, id)
}

/// Separate CLI processes share the same unique grant and the committed row
/// remains readable after every writer exits, even with a spaced Windows path.
#[tokio::test]
async fn concurrent_cli_grants_are_unique_and_persist_after_restart() {
    let (home, work, app, db_path, id) = role_rig().await;
    let mut first = albedo(home.path(), &app, &db_path)
        .args(["roles", "grant", id.as_str(), "payroll"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("first CLI");
    let second = albedo(home.path(), &app, &db_path)
        .args(["roles", "grant", "ADA@EXAMPLE.COM", "payroll"])
        .output()
        .expect("second CLI");
    assert!(second.status.success(), "{:?}", second);
    assert!(first.wait().expect("first exits").success());
    assert_eq!(roles_of(&db_path, &id).await, ["payroll"]);
    let (ok, text) =
        run(albedo(home.path(), &app, &db_path).args(["roles", "list", "--role", "payroll"]));
    assert!(ok && text.contains(id.as_str()), "{text}");
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles",
        "revoke",
        "ADA@EXAMPLE.COM",
        "payroll",
    ]));
    assert!(ok, "{text}");
    assert!(roles_of(&db_path, &id).await.is_empty());
    drop(work);
}

/// Shipping must forward all three operator commands into the embedded app,
/// while keeping its writable database outside the extracted source tree.
#[tokio::test]
async fn shipped_binary_forwards_role_changes_and_list() {
    let (home, work, app, db_path, id) = role_rig().await;
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args(["ship", "--target", "binary"]));
    assert!(ok, "ship\n{text}");
    let binary = app.join(if cfg!(windows) {
        "app.albedo-bin.exe"
    } else {
        "app.albedo-bin"
    });
    assert!(binary.is_file(), "{text}");
    let shipped = || {
        let mut cmd = Command::new(&binary);
        cmd.current_dir(work.path())
            .env("USERPROFILE", home.path())
            .env("HOME", home.path())
            .env("NO_COLOR", "1")
            .env("ALBEDO_NO_TUI", "1")
            .env("ALBEDO_FORGE_DB", &db_path);
        cmd
    };
    let (ok, text) = run(shipped().args(["roles", "grant", id.as_str(), "hr"]));
    assert!(ok, "shipped grant\n{text}");
    assert_eq!(roles_of(&db_path, &id).await, ["hr"]);
    let (ok, text) = run(shipped().args(["roles", "list", "--role", "hr"]));
    assert!(ok && text.contains(id.as_str()), "shipped list\n{text}");
    let (ok, text) = run(shipped().args(["roles", "revoke", id.as_str(), "hr"]));
    assert!(ok, "shipped revoke\n{text}");
    assert!(roles_of(&db_path, &id).await.is_empty());
}


#[tokio::test]
async fn malformed_requests_and_missing_database_do_not_write_grants() {
    let (home, _work, app, db_path, id) = role_rig().await;
    for args in [
        vec!["roles", "list", "--role"],
        vec!["roles", "grant", id.as_str(), "payroll'); DROP TABLE albedo_users;--"],
        vec!["roles", "grant", "' OR 1=1 --", "payroll"],
        vec!["roles", "not-a-command"],
    ] {
        let (ok, text) = run(albedo(home.path(), &app, &db_path).args(&args));
        assert!(!ok, "malformed request {args:?} succeeded\n{text}");
    }
    assert!(roles_of(&db_path, &id).await.is_empty());
    let missing = db_path.with_file_name("missing.db");
    let (ok, text) = run(albedo(home.path(), &app, &missing).args([
        "roles", "grant", id.as_str(), "payroll",
    ]));
    assert!(!ok && !missing.exists(), "missing DB should be refused without creating it\n{text}");
}


/// The brief's undeclared-role refusal applies to the listing filter too.
#[tokio::test]
async fn list_refuses_an_undeclared_role() {
    let (home, _work, app, db_path, _) = role_rig().await;
    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles", "list", "--role", "finance",
    ]));
    assert!(!ok, "an undeclared role filter must be refused\n{text}");
    assert!(text.contains("finance"), "{text}");
}

/// The one exception to the refusal: a grant left over from a role the config
/// used to declare can be listed and revoked. A misspelt revoke still fails, and
/// the real role stays granted.
#[tokio::test]
async fn leftover_grants_of_an_undeclared_role_can_be_cleaned_up() {
    let (home, _work, app, db_path, id) = role_rig().await;
    {
        // "finance" was declared once; the config has since dropped it.
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        for role in ["finance", "payroll"] {
            dom_render_compiler::auth::store::grant_role(&db, &id, role, Some("cli"), NOW)
                .await
                .expect("seed grant");
        }
    }
    assert_eq!(roles_of(&db_path, &id).await, ["finance", "payroll"]);

    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles", "revoke", id.as_str(), "payrol",
    ]));
    assert!(!ok, "a misspelt revoke must fail, not report a no-op\n{text}");
    assert!(text.contains("payrol") && text.contains("spelling"), "{text}");
    assert_eq!(roles_of(&db_path, &id).await, ["finance", "payroll"]);

    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles", "list", "--role", "finance",
    ]));
    assert!(ok, "leftovers of an undeclared role are listable\n{text}");
    assert!(text.contains(id.as_str()) && text.contains("not declared"), "{text}");

    let (ok, text) = run(albedo(home.path(), &app, &db_path).args([
        "roles", "revoke", id.as_str(), "finance",
    ]));
    assert!(ok, "a leftover grant is revocable\n{text}");
    assert!(text.contains("leftover"), "{text}");
    assert_eq!(roles_of(&db_path, &id).await, ["payroll"]);

    // Gone now, so the same commands are refused like any other undeclared role.
    for args in [
        vec!["roles", "revoke", id.as_str(), "finance"],
        vec!["roles", "list", "--role", "finance"],
    ] {
        let (ok, text) = run(albedo(home.path(), &app, &db_path).args(&args));
        assert!(!ok, "{args:?} must fail once no leftovers remain\n{text}");
        assert!(text.contains("finance"), "{text}");
    }
    assert_eq!(roles_of(&db_path, &id).await, ["payroll"]);
}

/// Upgrading an existing app must create the new table without losing its
/// accounts or changing which database the operator selected.
#[tokio::test]
async fn role_commands_upgrade_a_database_without_the_roles_table() {
    use dom_render_compiler::forge::DataSubstrate;
    let (home, _work, app, db_path, id) = role_rig().await;
    {
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        db.execute("DROP TABLE albedo_roles", &[])
            .await
            .expect("simulate pre-role schema");
    }
    let (ok, text) =
        run(albedo(home.path(), &app, &db_path).args(["roles", "grant", id.as_str(), "payroll"]));
    assert!(ok, "legacy database upgrade failed\n{text}");
    assert_eq!(roles_of(&db_path, &id).await, ["payroll"]);
    let (ok, text) =
        run(albedo(home.path(), &app, &db_path).args(["roles", "list", "--role", "payroll"]));
    assert!(ok && text.contains(id.as_str()), "{text}");
}

/// A leftover grant is only permission to clean it up. It must neither allow
/// a new grant nor turn a revoke for another person into a successful no-op.
#[tokio::test]
async fn leftover_cleanup_is_scoped_to_the_holder_and_never_allows_grant() {
    let (home, _work, app, db_path, id) = role_rig().await;
    let other = {
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        dom_render_compiler::auth::store::grant_role(&db, &id, "retired", Some("cli"), NOW)
            .await
            .expect("leftover grant");
        person(&db, "other", "other@example.com").await
    };
    for args in [
        vec!["roles", "grant", id.as_str(), "retired"],
        vec!["roles", "grant", other.as_str(), "retired"],
        vec!["roles", "revoke", other.as_str(), "retired"],
    ] {
        let (ok, text) = run(albedo(home.path(), &app, &db_path).args(&args));
        assert!(!ok && text.contains("retired"), "{args:?}\n{text}");
        assert_eq!(roles_of(&db_path, &id).await, ["retired"]);
        assert!(roles_of(&db_path, &other).await.is_empty());
    }
    let (ok, text) =
        run(albedo(home.path(), &app, &db_path).args(["roles", "revoke", id.as_str(), "retired"]));
    assert!(ok && text.contains("leftover"), "{text}");
    assert!(roles_of(&db_path, &id).await.is_empty());
}
