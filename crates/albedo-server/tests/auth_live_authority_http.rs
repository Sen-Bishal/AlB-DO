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
use dom_render_compiler::forge::LibSqlSubstrate;
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
    std::env::set_current_dir(&rig.project).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut providers = BTreeMap::new();
    providers.insert("password".to_string(), ProviderDecl::default());
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
        email: None,
        redirects: Vec::new(),
        webhooks: Default::default(),
        auth: AuthDeclaration {
            providers,
            roles: vec!["payroll".to_string()],
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
    let (short, long) = {
        let db = prepare(&rig).await;
        let short = person(&db, "pat", &[], 1, 4_000).await.remove(0);
        let long = person(&db, "sam", &[], 1, DAY_MS).await.remove(0);
        (short, long)
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
