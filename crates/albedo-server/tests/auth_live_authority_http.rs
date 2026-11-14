//! R1b (T-0026) · live connections end when their session ends or their roles
//! change — over real HTTP, against a real server.
//!
//! Falsifiers (each watched failing on the pre-T-0026 tree):
//! 1. logout vs SSE — the stream stayed open;
//! 2. sign-in rotation vs SSE — the old token's stream stayed open;
//! 3. expiry vs SSE — the stream outlived the session;
//! 4/5. CLI revoke vs a PHOSPHOR trunk and an SSE stream — both kept going.
//!
//! Every closing assertion has a positive control beside it: another session,
//! an anonymous stream, a connection opened after the change.
//!
//! People are written before the server boots, then the substrate is dropped —
//! a second in-process substrate on a served file is this rig's known crash
//! class (see `auth_role_gate_http.rs`). The CLI revoke runs as a separate
//! process, as an operator's would.

#![cfg(feature = "forge")]

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl, SessionDecl};
use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{
    create_session, grant_role, upsert_principal, ProviderProfile,
};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};
use dom_render_compiler::transforms::form::FORM_HIDDEN_INPUTS;
use reqwest::{redirect::Policy, StatusCode};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SESSION_COOKIE: &str = "__Host-albedo_session";
const DAY_MS: i64 = 86_400_000;

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn manifest_json() -> String {
    let route = |path: &str, roles: Option<&str>| {
        let mut route = serde_json::json!({
            "route": path,
            "shell": {
                "doctype_and_head": "<!DOCTYPE html><html><head><title>t</title></head>",
                "body_open": "<body><div id=\"root\"><!--__SLOT___a_0--></div>",
                "body_close": "</body></html>",
                "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
            },
            "tier_a_root": [{
                "component_id": "Home",
                "placeholder_id": "__a_0",
                "html": format!(r#"<form action="/_albedo/auth/logout" method="POST">{FORM_HIDDEN_INPUTS}</form>"#),
                "position": { "parent_placeholder": null, "slot": "default", "order": 0 }
            }],
            "tier_b": [], "tier_c": [],
            "shared_slot_topics": ["guestbook"],
            "action_ids": [], "layout_chain": [],
            "error_component": null, "loading_component": null
        });
        if let Some(role) = roles {
            route["auth"] = serde_json::json!({ "roles": [role] });
        }
        route
    };
    serde_json::json!({
        "version": 2,
        "build_id": "live-authority-test",
        "routes": { "/": route("/", None), "/payroll": route("/payroll", Some("payroll")) },
        "assets": { "chunks": {}, "css": [], "runtime": "/_albedo/runtime.js" },
        "schema_version": "2.0",
        "generated_at": "",
        "components": [{
            "id": 0, "name": "Home", "module_path": "src/Home.tsx", "tier": "A",
            "weight_bytes": 100, "priority": 1.0, "dependencies": [], "can_defer": true,
            "hydration_mode": "none"
        }],
        "parallel_batches": [], "critical_path": [], "vendor_chunks": [], "wt_streams": []
    })
    .to_string()
}

struct Rig {
    _temp: tempfile::TempDir,
    project: PathBuf,
    db_path: PathBuf,
}

fn rig() -> Rig {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().to_path_buf();
    let dist = project.join(".albedo").join("dist");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(dist.join("_albedo")).unwrap();
    fs::create_dir_all(project.join("data")).unwrap();
    fs::write(
        project.join("src").join("Home.tsx"),
        "export default function Home() { return <p>home</p>; }\n",
    )
    .unwrap();
    fs::write(dist.join("render-manifest.v2.json"), manifest_json()).unwrap();
    fs::write(dist.join("_albedo").join("runtime.js"), b"// stub\n").unwrap();
    let db_path = project.join("data").join("forge.db");
    Rig {
        _temp: temp,
        project,
        db_path,
    }
}

/// A principal with a password-less account, its roles, and `sessions`
/// sessions of `ttl_ms` each; returns their cookies.
async fn person(
    db: &LibSqlSubstrate,
    who: &str,
    roles: &[&str],
    sessions: usize,
    ttl_ms: i64,
) -> Vec<String> {
    let profile = ProviderProfile {
        email: Some(format!("{who}@example.com")),
        ..ProviderProfile::default()
    };
    let principal = upsert_principal(db, "password", who, &profile, now_ms())
        .await
        .unwrap();
    for role in roles {
        grant_role(db, &principal.id, role, Some("test"), now_ms())
            .await
            .unwrap();
    }
    let mut cookies = Vec::new();
    for _ in 0..sessions {
        let token = create_session(db, &principal.id, "password", now_ms(), ttl_ms)
            .await
            .unwrap();
        cookies.push(format!("{SESSION_COOKIE}={}", token.expose_for_cookie()));
    }
    cookies
}

async fn prepare(rig: &Rig) -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_local(&rig.db_path).await.unwrap();
    let tables = schema::augment(&ForgeSchema::build(Vec::new()).unwrap()).unwrap();
    bootstrap_schema(&db, &tables).await.unwrap();
    db
}

async fn boot(rig: &Rig, ttl: Option<&str>) -> String {
    boot_with_email(rig, ttl, false).await
}

async fn boot_with_email(rig: &Rig, ttl: Option<&str>, email_flows: bool) -> String {
    boot_configured(rig, ttl, email_flows, true).await
}

async fn boot_configured(
    rig: &Rig,
    ttl: Option<&str>,
    email_flows: bool,
    providers_enabled: bool,
) -> String {
    std::env::set_current_dir(&rig.project).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut providers = BTreeMap::new();
    providers.insert(
        "password".to_string(),
        ProviderDecl {
            reset: email_flows.then(|| "email".to_string()),
            verify: email_flows.then(|| "required".to_string()),
            ..Default::default()
        },
    );
    if email_flows {
        providers.insert("magiclink".to_string(), ProviderDecl::default());
    }
    if !providers_enabled {
        providers.clear();
    }
    let opts = ProductionServerOptions {
        project_dir: rig.project.clone(),
        source_root: rig.project.join("src"),
        dist_dir: rig.project.join(".albedo").join("dist"),
        host: "127.0.0.1".to_string(),
        port,
        dev_mode: false,
        hmr: false,
        forge: Default::default(),
        sources: Default::default(),
        uploads: Default::default(),
        email: email_flows.then(|| {
            serde_json::from_value(serde_json::json!({
                "from": "test@example.com", "provider": "log"
            }))
            .unwrap()
        }),
        redirects: Vec::new(),
        webhooks: Default::default(),
        auth: AuthDeclaration {
            providers,
            roles: if providers_enabled {
                vec!["payroll".to_string()]
            } else {
                Vec::new()
            },
            session: SessionDecl {
                ttl: ttl.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        },
        tls: Default::default(),
        forge_db: Some(rig.db_path.clone()),
    };
    let server = boot_production_server(&opts).expect("server boots");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = server
            .run_with_ready(move |_| {
                let _ = ready_tx.send(());
            })
            .await;
    });
    ready_rx.await.expect("ready");
    format!("http://127.0.0.1:{port}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .unwrap()
}

/// Open the SSE patch stream for `/`, as `cookie`.
async fn sse(client: &reqwest::Client, base: &str, cookie: Option<&str>) -> reqwest::Response {
    let mut request = client.get(format!("{base}/_albedo/patches?p=/"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let mut response = request.send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The `open` comment proves the stream is up.
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk()).await;
    assert!(matches!(first, Ok(Ok(Some(_)))), "the stream did not open");
    response
}

/// Whether `stream` ends (the server closed it) within `within`. Keep-alive
/// pings are read and ignored.
async fn ends_within(stream: &mut reqwest::Response, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match tokio::time::timeout(left, stream.chunk()).await {
            Err(_) => return false,
            Ok(Ok(Some(_))) => continue,
            Ok(Ok(None)) | Ok(Err(_)) => return true,
        }
    }
}

/// A tab cookie and the logout form's CSRF token, from the public page.
async fn form(client: &reqwest::Client, base: &str, cookie: &str) -> (String, String) {
    let response = client
        .get(format!("{base}/"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    let tab = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| v.starts_with("__Host-albedo-session="))
        .map(str::to_string)
        .unwrap_or_default();
    let html = response.text().await.unwrap();
    let needle = r#"name="_csrf" value=""#;
    let start = html.find(needle).expect("csrf field") + needle.len();
    let csrf = html[start..start + html[start..].find('"').unwrap()].to_string();
    (tab, csrf)
}

async fn post_form(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    cookies: &str,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    client
        .post(format!("{base}{path}"))
        .header("cookie", cookies)
        .form(fields)
        .send()
        .await
        .unwrap()
}

/// Falsifier 1 · AC 9 — logout ends **that** session's stream, and only it.
#[tokio::test]
async fn logout_ends_that_sessions_stream_and_no_other() {
    let rig = rig();
    let (laptop, phone) = {
        let db = prepare(&rig).await;
        let cookies = person(&db, "pat", &[], 2, DAY_MS).await;
        (cookies[0].clone(), cookies[1].clone())
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut on_laptop = sse(&client, &base, Some(&laptop)).await;
    let mut on_phone = sse(&client, &base, Some(&phone)).await;
    let mut stranger = sse(&client, &base, None).await;

    let (tab, csrf) = form(&client, &base, &laptop).await;
    let cookies = format!("{tab}; {laptop}");
    let response = post_form(
        &client,
        &base,
        "/_albedo/auth/logout",
        &cookies,
        &[("_csrf", &csrf), ("_albedo_return", "/")],
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "logout: {}",
        response.status()
    );

    assert!(
        ends_within(&mut on_laptop, Duration::from_secs(3)).await,
        "the logged-out session's stream kept going"
    );
    assert!(
        !ends_within(&mut on_phone, Duration::from_millis(800)).await,
        "logout ended another session's stream"
    );
    assert!(
        !ends_within(&mut stranger, Duration::from_millis(300)).await,
        "logout ended an anonymous stream"
    );
}

/// Falsifier 2 · AC 9 — signing in again rotates the token; the old token's
/// stream ends and the new token's works.
#[tokio::test]
async fn signing_in_again_ends_the_old_tokens_stream() {
    let rig = rig();
    drop(prepare(&rig).await);
    let base = boot(&rig, None).await;
    let client = client();

    let (tab, csrf) = form(&client, &base, "x=y").await;
    let creds = [
        ("_csrf", csrf.as_str()),
        ("_albedo_return", "/"),
        ("email", "ada@example.com"),
        ("password", "correct horse battery"),
    ];
    let registered = post_form(
        &client,
        &base,
        "/_albedo/auth/password/register",
        &tab,
        &creds,
    )
    .await;
    let first = session_cookie(&registered).expect("register opened a session");
    let mut old_stream = sse(&client, &base, Some(&first)).await;

    let again = post_form(
        &client,
        &base,
        "/_albedo/auth/password/login",
        &format!("{tab}; {first}"),
        &creds,
    )
    .await;
    let second = session_cookie(&again).expect("login rotated the session");
    assert_ne!(first, second);

    assert!(
        ends_within(&mut old_stream, Duration::from_secs(3)).await,
        "the rotated-away token's stream kept going"
    );
    let mut new_stream = sse(&client, &base, Some(&second)).await;
    assert!(
        !ends_within(&mut new_stream, Duration::from_millis(800)).await,
        "the new token's stream was closed"
    );
}

fn session_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(|v| v.split(';').next())
        .find(|v| {
            v.starts_with(&format!("{SESSION_COOKIE}=")) && v.len() > SESSION_COOKIE.len() + 1
        })
        .map(str::to_string)
}

/// Falsifier 3 · AC 9 — a stream outliving its session's expiry is closed by
/// the sweep within one tick, with no further request.
#[tokio::test]
async fn an_expired_sessions_stream_closes_within_a_tick() {
    let rig = rig();
    let (short, long, expires_at) = {
        let db = prepare(&rig).await;
        let short = person(&db, "pat", &[], 1, 4_000).await.remove(0);
        let long = person(&db, "sam", &[], 1, DAY_MS).await.remove(0);
        let rows = db
            .query("SELECT MIN(expires_at) FROM albedo_sessions", &[])
            .await
            .unwrap();
        let expires_at = match rows.rows[0].get(0) {
            Some(dom_render_compiler::forge::SqlValue::Integer(value)) => *value,
            value => panic!("invalid session expiry: {value:?}"),
        };
        (short, long, expires_at)
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut expiring = sse(&client, &base, Some(&short)).await;
    let mut lasting = sse(&client, &base, Some(&long)).await;
    // 4 s session + one 2 s tick + slack.
    assert!(
        ends_within(&mut expiring, Duration::from_secs(8)).await,
        "the stream outlived its session"
    );
    let delay = now_ms() - expires_at;
    eprintln!("session expiry ? HTTP stream ended after {delay} ms");
    assert!(
        (0..=2_000).contains(&delay),
        "expiry closing exceeded one tick: {delay} ms"
    );
    assert!(
        !ends_within(&mut lasting, Duration::from_millis(500)).await,
        "expiry closed an unexpired session's stream"
    );
}

/// Falsifiers 4 + 5 · AC 7 — `albedo roles revoke` from another process closes
/// the holder's trunk and SSE stream within two ticks; a connection opened
/// after the change survives the next tick.
#[tokio::test]
async fn a_cli_revoke_closes_stale_connections_and_spares_new_ones() {
    let rig = rig();
    let holder = {
        let db = prepare(&rig).await;
        person(&db, "pat", &["payroll"], 1, DAY_MS).await.remove(0)
    };
    let cli_app = cli_app();
    let base = boot(&rig, None).await;
    let client = client();

    let mut stream = sse(&client, &base, Some(&holder)).await;
    let mut trunk = client
        .get(format!("{base}/_albedo/phosphor"))
        .header("cookie", &holder)
        .send()
        .await
        .unwrap();
    let hello = String::from_utf8_lossy(&trunk.chunk().await.unwrap().unwrap()).to_string();
    let marker = r#""lane":""#;
    let start = hello.find(marker).unwrap() + marker.len();
    let lane = hello[start..start + hello[start..].find('"').unwrap()].to_string();

    let started = Instant::now();
    let (ok, text) = revoke(&cli_app, &rig.db_path, "pat@example.com", "payroll");
    assert!(ok, "albedo roles revoke failed\n{text}");

    assert!(
        ends_within(&mut stream, Duration::from_secs(5)).await,
        "a CLI revoke left the SSE stream open"
    );
    assert!(
        ends_within(&mut trunk, Duration::from_secs(5)).await,
        "a CLI revoke left the trunk open"
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "closing took {elapsed:?} (bound: two 2 s ticks)"
    );
    eprintln!("CLI revoke → closed in {elapsed:?}");

    // Falsifier 4 · a subscribe on the stale lane installs nothing.
    let body =
        serde_json::json!({ "lane": lane, "add": [{ "p": "/payroll", "n": "0" }] }).to_string();
    let response = client
        .post(format!("{base}/_albedo/phosphor/routes"))
        .header("cookie", &holder)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    if response.status() == StatusCode::OK {
        let outcome: serde_json::Value =
            serde_json::from_str(&response.text().await.unwrap()).unwrap();
        assert!(
            outcome["ok"].as_array().unwrap().is_empty(),
            "the stale lane admitted /payroll: {outcome}"
        );
    } else {
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "the stale lane is gone"
        );
    }

    // Positive control: opened after the change, it survives the next tick.
    let mut fresh = sse(&client, &base, Some(&holder)).await;
    assert!(
        !ends_within(&mut fresh, Duration::from_millis(2_600)).await,
        "a connection opened after the revoke was closed"
    );
}

/// `albedo init` an app with `payroll` declared, for the CLI to read roles from.
fn cli_app() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_albedo"))
        .current_dir(work.path())
        .env("USERPROFILE", home.path())
        .env("HOME", home.path())
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1")
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "init: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let app = work.path().join("app");
    let config = app.join("albedo.config.ts");
    let text = fs::read_to_string(&config).unwrap();
    let declared = text.replacen(
        "login: \"/sign-in\",",
        "login: \"/sign-in\",\n    roles: [\"payroll\"],",
        1,
    );
    assert_ne!(declared, text);
    fs::write(config, declared).unwrap();
    (home, work, app)
}

fn revoke(
    app: &(tempfile::TempDir, tempfile::TempDir, PathBuf),
    db: &Path,
    who: &str,
    role: &str,
) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_albedo"))
        .current_dir(&app.2)
        .env("USERPROFILE", app.0.path())
        .env("HOME", app.0.path())
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1")
        .env("ALBEDO_FORGE_DB", db)
        .args(["roles", "revoke", who, role])
        .output()
        .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// An authenticated PHOSPHOR connection must follow the same session lifecycle
/// as SSE, including while it has not subscribed to a route yet.
async fn trunk(client: &reqwest::Client, base: &str, cookie: &str) -> reqwest::Response {
    let mut response = client
        .get(format!("{base}/_albedo/phosphor"))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), response.chunk()).await,
        Ok(Ok(Some(_)))
    ));
    response
}

#[tokio::test]
async fn logout_all_closes_both_transports_for_every_session_only_of_its_principal() {
    let rig = rig();
    let (holder, other) = {
        let db = prepare(&rig).await;
        let holder = person(&db, "holder", &[], 2, DAY_MS).await;
        let other = person(&db, "other", &[], 1, DAY_MS).await.remove(0);
        (holder, other)
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut streams = Vec::new();
    for cookie in &holder {
        streams.push(sse(&client, &base, Some(cookie)).await);
        streams.push(trunk(&client, &base, cookie).await);
    }
    let mut untouched = sse(&client, &base, Some(&other)).await;
    let mut anonymous = sse(&client, &base, None).await;
    let (tab, csrf) = form(&client, &base, &holder[0]).await;
    let response = post_form(
        &client,
        &base,
        "/_albedo/auth/logout-all",
        &format!("{tab}; {}", holder[0]),
        &[("_csrf", &csrf), ("_albedo_return", "/")],
    )
    .await;
    assert!(response.status().is_redirection());
    for stream in &mut streams {
        assert!(ends_within(stream, Duration::from_secs(3)).await);
    }
    assert!(!ends_within(&mut untouched, Duration::from_millis(300)).await);
    assert!(!ends_within(&mut anonymous, Duration::from_millis(300)).await);
}

/// Unlinking password leaves its initiating token and a different provider's
/// token alive; only the other password token is in the committed delete set.
#[tokio::test]
async fn unlink_closes_only_the_removed_providers_other_sessions_on_both_transports() {
    use dom_render_compiler::auth::store::link_account;
    let rig = rig();
    let (passwords, other_provider) = {
        let db = prepare(&rig).await;
        let passwords = person(&db, "holder", &[], 2, DAY_MS).await;
        let id = upsert_principal(
            &db,
            "password",
            "holder",
            &ProviderProfile::default(),
            now_ms(),
        )
        .await
        .unwrap()
        .id;
        link_account(&db, &id, "github", "holder-github", now_ms())
            .await
            .unwrap();
        let token = create_session(&db, &id, "github", now_ms(), DAY_MS)
            .await
            .unwrap();
        (
            passwords,
            format!("{SESSION_COOKIE}={}", token.expose_for_cookie()),
        )
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut removed_sse = sse(&client, &base, Some(&passwords[1])).await;
    let mut removed_trunk = trunk(&client, &base, &passwords[1]).await;
    let mut initiating = sse(&client, &base, Some(&passwords[0])).await;
    let mut other_provider = trunk(&client, &base, &other_provider).await;
    let (tab, csrf) = form(&client, &base, &passwords[0]).await;
    let response = post_form(
        &client,
        &base,
        "/_albedo/auth/unlink",
        &format!("{tab}; {}", passwords[0]),
        &[
            ("_csrf", &csrf),
            ("_albedo_return", "/"),
            ("provider", "password"),
        ],
    )
    .await;
    assert!(
        response.status().is_redirection(),
        "unlink: {}",
        response.status()
    );
    assert!(ends_within(&mut removed_sse, Duration::from_secs(3)).await);
    assert!(ends_within(&mut removed_trunk, Duration::from_secs(3)).await);
    assert!(!ends_within(&mut initiating, Duration::from_millis(300)).await);
    assert!(!ends_within(&mut other_provider, Duration::from_millis(300)).await);
}

/// The trigger aborts the actual DELETE inside the store operation, rather
/// than rejecting the request before it reaches the invalidation branch.
async fn failed_delete_leaves_connections_working(path: &str) {
    use dom_render_compiler::auth::store::link_account;
    let rig = rig();
    let holder = {
        let db = prepare(&rig).await;
        let cookies = person(&db, "holder", &["payroll"], 2, DAY_MS).await;
        let id = upsert_principal(
            &db,
            "password",
            "holder",
            &ProviderProfile::default(),
            now_ms(),
        )
        .await
        .unwrap()
        .id;
        link_account(&db, &id, "github", "holder-github", now_ms())
            .await
            .unwrap();
        db.execute(
            "CREATE TRIGGER refuse_session_delete BEFORE DELETE ON albedo_sessions \
             BEGIN SELECT RAISE(ABORT, 'review injected delete failure'); END",
            &[],
        )
        .await
        .unwrap();
        cookies
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut streams = Vec::new();
    for cookie in &holder {
        streams.push(sse(&client, &base, Some(cookie)).await);
        streams.push(trunk(&client, &base, cookie).await);
    }
    let (tab, csrf) = form(&client, &base, &holder[0]).await;
    let response = post_form(
        &client,
        &base,
        path,
        &format!("{tab}; {}", holder[0]),
        &[
            ("_csrf", &csrf),
            ("_albedo_return", "/"),
            ("provider", "password"),
        ],
    )
    .await;
    if path.ends_with("unlink") {
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    } else {
        assert!(response.status().is_redirection());
    }
    for stream in &mut streams {
        assert!(
            !ends_within(stream, Duration::from_millis(300)).await,
            "{path} closed a stream although its delete failed"
        );
    }
    // The original cookie still resolves, rather than merely leaving an idle
    // response open after losing its backing session.
    let response = client
        .get(format!("{base}/payroll"))
        .header("cookie", &holder[0])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_failed_logout_delete_closes_neither_transport() {
    failed_delete_leaves_connections_working("/_albedo/auth/logout").await;
}

#[tokio::test]
async fn a_failed_logout_all_delete_closes_neither_transport() {
    failed_delete_leaves_connections_working("/_albedo/auth/logout-all").await;
}

#[tokio::test]
async fn a_failed_unlink_delete_closes_neither_transport() {
    failed_delete_leaves_connections_working("/_albedo/auth/unlink").await;
}

#[tokio::test]
async fn a_failed_rotation_keeps_the_old_token_and_both_transports_alive() {
    use dom_render_compiler::auth::password::hash_password;
    use dom_render_compiler::auth::store::create_credential_account;
    let rig = rig();
    let holder = {
        let db = prepare(&rig).await;
        let profile = ProviderProfile {
            email: Some("holder@example.com".to_string()),
            ..ProviderProfile::default()
        };
        let password = hash_password("correct horse battery").unwrap();
        let principal = create_credential_account(
            &db,
            "password",
            "holder@example.com",
            &profile,
            &password,
            now_ms(),
        )
        .await
        .unwrap()
        .unwrap();
        grant_role(&db, &principal.id, "payroll", Some("test"), now_ms())
            .await
            .unwrap();
        let token = create_session(&db, &principal.id, "password", now_ms(), DAY_MS)
            .await
            .unwrap();
        db.execute(
            "CREATE TRIGGER refuse_session_delete BEFORE DELETE ON albedo_sessions \
             BEGIN SELECT RAISE(ABORT, 'review injected delete failure'); END",
            &[],
        )
        .await
        .unwrap();
        format!("{SESSION_COOKIE}={}", token.expose_for_cookie())
    };
    let base = boot(&rig, None).await;
    let client = client();
    let mut stream = sse(&client, &base, Some(&holder)).await;
    let mut lane = trunk(&client, &base, &holder).await;
    let (tab, csrf) = form(&client, &base, &holder).await;
    let response = post_form(
        &client,
        &base,
        "/_albedo/auth/password/login",
        &format!("{tab}; {holder}"),
        &[
            ("_csrf", &csrf),
            ("_albedo_return", "/"),
            ("email", "holder@example.com"),
            ("password", "correct horse battery"),
        ],
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(session_cookie(&response).is_none());
    assert!(!ends_within(&mut stream, Duration::from_millis(300)).await);
    assert!(!ends_within(&mut lane, Duration::from_millis(300)).await);
    let response = client
        .get(format!("{base}/payroll"))
        .header("cookie", &holder)
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "failed rotation lost the old session"
    );
}

/// Seed the mailed token at the store seam, then redeem it through real HTTP:
/// delivery is outside this ticket, but its session-ending commit is not.
async fn email_redemption_closes_exactly_the_committed_sessions(magic: bool, fail_delete: bool) {
    use dom_render_compiler::auth::email_token::{issue, Purpose};
    use dom_render_compiler::auth::store::create_credential_account;
    let rig = rig();
    let (holders, unrelated, link) = {
        let db = prepare(&rig).await;
        for ddl in dom_render_compiler::auth::email_token::ddl() {
            db.execute(&ddl, &[]).await.unwrap();
        }
        let profile = ProviderProfile {
            email: Some("holder@example.com".to_string()),
            ..ProviderProfile::default()
        };
        let principal = create_credential_account(
            &db,
            "password",
            "holder@example.com",
            &profile,
            "old-secret",
            now_ms(),
        )
        .await
        .unwrap()
        .unwrap();
        grant_role(&db, &principal.id, "payroll", Some("test"), now_ms())
            .await
            .unwrap();
        let mut holders = Vec::new();
        for _ in 0..2 {
            let token = create_session(&db, &principal.id, "password", now_ms(), DAY_MS)
                .await
                .unwrap();
            holders.push(format!("{SESSION_COOKIE}={}", token.expose_for_cookie()));
        }
        let unrelated = person(&db, "unrelated", &["payroll"], 1, DAY_MS)
            .await
            .remove(0);
        let purpose = if magic {
            Purpose::MagicLink
        } else {
            Purpose::PasswordReset
        };
        let provider = if magic { "magiclink" } else { "password" };
        let link = issue(
            &db,
            purpose,
            provider,
            Some(&principal.id),
            "holder@example.com",
            None,
            now_ms(),
        )
        .await
        .unwrap()
        .expose_for_cookie()
        .to_string();
        if fail_delete {
            db.execute(
                "CREATE TRIGGER refuse_session_delete BEFORE DELETE ON albedo_sessions \
                 BEGIN SELECT RAISE(ABORT, 'review injected delete failure'); END",
                &[],
            )
            .await
            .unwrap();
        }
        (holders, unrelated, link)
    };
    let base = boot_with_email(&rig, None, true).await;
    let client = client();
    let mut old = Vec::new();
    for cookie in &holders {
        old.push(sse(&client, &base, Some(cookie)).await);
        old.push(trunk(&client, &base, cookie).await);
    }
    let mut unrelated_stream = sse(&client, &base, Some(&unrelated)).await;
    let mut anonymous_stream = sse(&client, &base, None).await;
    let (tab, csrf) = form(&client, &base, &holders[0]).await;
    let path = if magic {
        "/_albedo/auth/magiclink/signin"
    } else {
        "/_albedo/auth/password/reset"
    };
    let response = post_form(
        &client,
        &base,
        path,
        &format!("{tab}; {}", holders[0]),
        &[
            ("_csrf", &csrf),
            ("token", &link),
            ("password", "a replacement password"),
        ],
    )
    .await;
    if fail_delete {
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(session_cookie(&response).is_none());
        for stream in &mut old {
            assert!(
                !ends_within(stream, Duration::from_millis(150)).await,
                "a failed email transaction closed a live connection"
            );
        }
        let still_authorized = client
            .get(format!("{base}/payroll"))
            .header("cookie", &holders[0])
            .send()
            .await
            .unwrap();
        assert_eq!(still_authorized.status(), StatusCode::OK);
    } else {
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let replacement = session_cookie(&response).expect("a replacement session");
        assert!(!holders.contains(&replacement));
        for stream in &mut old {
            assert!(
                ends_within(stream, Duration::from_secs(3)).await,
                "email redemption left a deleted session's live connection open"
            );
        }
        let authorized = client
            .get(format!("{base}/payroll"))
            .header("cookie", &replacement)
            .send()
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        let mut replacement_sse = sse(&client, &base, Some(&replacement)).await;
        let mut replacement_trunk = trunk(&client, &base, &replacement).await;
        assert!(!ends_within(&mut replacement_sse, Duration::from_millis(300)).await);
        assert!(!ends_within(&mut replacement_trunk, Duration::from_millis(300)).await);
    }
    assert!(!ends_within(&mut unrelated_stream, Duration::from_millis(150)).await);
    assert!(!ends_within(&mut anonymous_stream, Duration::from_millis(150)).await);
}

#[tokio::test]
async fn password_reset_closes_old_sse_and_trunks_and_spares_replacement() {
    email_redemption_closes_exactly_the_committed_sessions(false, false).await;
}

#[tokio::test]
async fn a_failed_password_reset_keeps_old_sse_and_trunks_alive() {
    email_redemption_closes_exactly_the_committed_sessions(false, true).await;
}

#[tokio::test]
async fn magic_link_takeover_closes_old_sse_and_trunks_and_spares_replacement() {
    email_redemption_closes_exactly_the_committed_sessions(true, false).await;
}

#[tokio::test]
async fn a_failed_magic_link_takeover_keeps_old_sse_and_trunks_alive() {
    email_redemption_closes_exactly_the_committed_sessions(true, true).await;
}

#[tokio::test]
async fn public_streams_without_providers_survive_unrelated_authority_changes() {
    let rig = rig();
    let cookie = {
        let db = prepare(&rig).await;
        person(&db, "unconfigured", &["payroll"], 1, DAY_MS)
            .await
            .remove(0)
    };
    let app = cli_app();
    let mut manifest: serde_json::Value = serde_json::from_str(&manifest_json()).unwrap();
    manifest["routes"].as_object_mut().unwrap().remove("/payroll");
    fs::write(
        rig.project.join(".albedo/dist/render-manifest.v2.json"),
        manifest.to_string(),
    )
    .unwrap();
    let base = boot_configured(&rig, None, false, false).await;
    let client = client();
    // Even a cookie for a persisted session resolves anonymously when this
    // server has no providers. Public transport admission must still work.
    let mut cookie_sse = sse(&client, &base, Some(&cookie)).await;
    let mut anonymous_sse = sse(&client, &base, None).await;
    let mut cookie_trunk = trunk(&client, &base, &cookie).await;
    let mut anonymous_trunk = trunk(&client, &base, "").await;
    let (ok, text) = revoke(&app, &rig.db_path, "unconfigured@example.com", "payroll");
    assert!(ok, "{text}");
    assert!(!ends_within(&mut cookie_sse, Duration::from_millis(2_600)).await);
    for stream in [&mut anonymous_sse, &mut cookie_trunk, &mut anonymous_trunk] {
        assert!(!ends_within(stream, Duration::from_millis(150)).await);
    }
}
