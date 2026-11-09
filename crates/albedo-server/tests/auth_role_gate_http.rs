//! R1a · `export const auth = { role: … }` on **every** door, over real HTTP.
//!
//! The same three doors `auth_route_gate_http.rs` pins for `auth = "required"`
//! — the page render, the live lane, the route's action — now with three
//! callers instead of two: a stranger, someone signed in **without** the role,
//! and someone holding it. The middle caller is the one a principal-present
//! check lets through, which is exactly the hole a role gate exists to close.
//!
//! ## Why the people are made before the server starts
//!
//! Granting a role needs a write to the server's database. Opening a second
//! substrate on that file while the server holds one, in one process, is the
//! known co-liveness crash class on this rig. So the principals, their sessions
//! and the grant are written first, the substrate is dropped, and only then does
//! the server boot on the same file — the order an operator's `albedo roles
//! grant` followed by a restart would produce.

#![cfg(feature = "forge")]

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl};
use dom_render_compiler::auth::schema;
use dom_render_compiler::auth::store::{
    create_session, grant_role, upsert_principal, GrantOutcome, ProviderProfile,
};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::LibSqlSubstrate;
use dom_render_compiler::transforms::form::{allocate_form_action_id, FORM_HIDDEN_INPUTS};
use reqwest::{redirect::Policy, StatusCode};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const GATED_ACTION: &str = "payroll_close";
const SHARED_ACTION: &str = "shared_close";
const ANY_ROLE_ACTION: &str = "either_close";
const SESSION_COOKIE: &str = "__Host-albedo_session";

const PUBLIC_TSX: &str = r#"
export default function Home() {
  return (
    <form action="/_albedo/auth/password/register" method="POST">
      <input name="email" />
    </form>
  );
}
"#;

fn public_html() -> String {
    format!(
        r#"<form action="/_albedo/auth/password/register" method="POST">{FORM_HIDDEN_INPUTS}<input name="email" /></form>"#
    )
}

/// `/` is public; `/payroll` reads the **same** global topic and differs only in
/// its declaration, so nothing but the role gate can separate them.
fn manifest_json(module: &Path) -> String {
    let module_path = module.display().to_string().replace('\\', "/");
    let baked = public_html().replace('\\', r"\\").replace('"', r#"\""#);
    let action_id = allocate_form_action_id(GATED_ACTION);
    let original = format!(
        r#"{{
  "version": 2,
  "build_id": "auth-role-gate-test",
  "routes": {{
    "/": {{
      "route": "/",
      "shell": {{
        "doctype_and_head": "<!DOCTYPE html><html><head><title>home</title></head>",
        "body_open": "<body><div id=\"root\"><!--__SLOT___a_home_0--></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      }},
      "tier_a_root": [{{
        "component_id": "Home",
        "placeholder_id": "__a_home_0",
        "html": "{baked}",
        "position": {{ "parent_placeholder": null, "slot": "default", "order": 0 }}
      }}],
      "tier_b": [],
      "tier_c": [],
      "shared_slot_topics": ["guestbook"],
      "action_ids": [],
      "layout_chain": [],
      "error_component": null,
      "loading_component": null
    }},
    "/payroll": {{
      "route": "/payroll",
      "auth": {{ "roles": ["payroll"] }},
      "shell": {{
        "doctype_and_head": "<!DOCTYPE html><html><head><title>payroll</title></head>",
        "body_open": "<body><div id=\"root\"><!--__SLOT___a_payroll_0--></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      }},
      "tier_a_root": [{{
        "component_id": "Home",
        "placeholder_id": "__a_payroll_0",
        "html": "<p>payroll</p>",
        "position": {{ "parent_placeholder": null, "slot": "default", "order": 0 }}
      }}],
      "tier_b": [],
      "tier_c": [],
      "shared_slot_topics": ["guestbook"],
      "action_ids": [{{ "name": "{GATED_ACTION}", "action_id": {action_id} }}],
      "layout_chain": [],
      "error_component": null,
      "loading_component": null
    }}
  }},
  "assets": {{ "chunks": {{}}, "css": [], "runtime": "/_albedo/runtime.js" }},
  "schema_version": "2.0",
  "generated_at": "",
  "components": [{{
    "id": 0,
    "name": "Home",
    "module_path": "{module_path}",
    "tier": "A",
    "weight_bytes": 100,
    "priority": 1.0,
    "dependencies": [],
    "can_defer": true,
    "hydration_mode": "none"
  }}],
  "parallel_batches": [],
  "critical_path": [],
  "vendor_chunks": [],
  "wt_streams": []
}}"#
    );
    let mut manifest: serde_json::Value = serde_json::from_str(&original).expect("manifest JSON");
    let shared = serde_json::json!({
        "name": SHARED_ACTION,
        "action_id": allocate_form_action_id(SHARED_ACTION),
    });
    manifest["routes"]["/"]["action_ids"] = serde_json::json!([shared]);
    manifest["routes"]["/payroll"]["action_ids"]
        .as_array_mut()
        .expect("actions")
        .push(shared.clone());
    let mut hr = manifest["routes"]["/payroll"].clone();
    hr["route"] = serde_json::json!("/hr");
    hr["auth"] = serde_json::json!({ "roles": ["hr"] });
    hr["action_ids"] = serde_json::json!([shared]);
    manifest["routes"]["/hr"] = hr;
    let mut either = manifest["routes"]["/payroll"].clone();
    either["route"] = serde_json::json!("/either");
    either["auth"] = serde_json::json!({ "roles": ["payroll", "hr"] });
    either["action_ids"] = serde_json::json!([{
        "name": ANY_ROLE_ACTION,
        "action_id": allocate_form_action_id(ANY_ROLE_ACTION),
    }]);
    manifest["routes"]["/either"] = either;
    manifest.to_string()
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_millis(),
    )
    .expect("fits")
}

/// Make a principal with a live session and return its session cookie.
async fn signed_in(db: &LibSqlSubstrate, subject: &str, roles: &[&str]) -> String {
    let profile = ProviderProfile {
        email: Some(format!("{subject}@example.com")),
        name: None,
        image: None,
        claims: serde_json::Map::new(),
    };
    let principal = upsert_principal(db, "password", subject, &profile, now_ms())
        .await
        .expect("principal");
    for role in roles {
        assert_eq!(
            grant_role(db, &principal.id, role, Some("test"), now_ms())
                .await
                .expect("grant"),
            GrantOutcome::Granted
        );
    }
    let token = create_session(db, &principal.id, "password", now_ms(), 86_400_000)
        .await
        .expect("session");
    format!("{SESSION_COOKIE}={}", token.expose_for_cookie())
}

struct ServedForm {
    cookie: String,
    csrf: String,
    return_path: String,
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    Some(rest[..rest.find('"')?].to_string())
}

async fn load_public_page(client: &reqwest::Client, base: &str) -> ServedForm {
    let response = client.get(format!("{base}/")).send().await.expect("home");
    assert_eq!(response.status(), StatusCode::OK, "the public route renders");
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with("__Host-albedo-session="))
        .expect("a tab cookie")
        .to_string();
    let html = response.text().await.expect("html");
    ServedForm {
        cookie,
        csrf: hidden_field(&html, "_csrf").expect("csrf"),
        return_path: hidden_field(&html, "_albedo_return").expect("return path"),
    }
}

async fn open_lane(
    client: &reqwest::Client,
    base: &str,
    cookie: Option<&str>,
) -> (String, reqwest::Response) {
    let mut request = client.get(format!("{base}/_albedo/phosphor"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let mut response = request.send().await.expect("trunk");
    assert_eq!(response.status(), StatusCode::OK);
    let chunk = response.chunk().await.expect("streams").expect("hello");
    let text = String::from_utf8_lossy(&chunk).to_string();
    let marker = r#""lane":""#;
    let start = text.find(marker).expect("lane id") + marker.len();
    let rest = &text[start..];
    (rest[..rest.find('"').expect("ends")].to_string(), response)
}

async fn subscribe(
    client: &reqwest::Client,
    base: &str,
    cookie: Option<&str>,
    paths: &[&str],
) -> (Vec<String>, Vec<String>) {
    let (lane, _trunk) = open_lane(client, base, cookie).await;
    let body = serde_json::json!({
        "lane": lane,
        "add": paths.iter().enumerate().map(|(index, path)| {
            serde_json::json!({ "p": path, "n": index.to_string() })
        }).collect::<Vec<_>>(),
    })
    .to_string();
    let mut request = client
        .post(format!("{base}/_albedo/phosphor/routes"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = request.send().await.expect("subscribe");
    assert_eq!(response.status(), StatusCode::OK);
    let outcome: serde_json::Value =
        serde_json::from_str(&response.text().await.expect("body")).expect("json");
    let list = |key: &str| -> Vec<String> {
        outcome[key]
            .as_array()
            .expect("list")
            .iter()
            .map(|value| value.as_str().expect("path").to_string())
            .collect()
    };
    (list("ok"), list("denied"))
}

/// 🔑 Three callers × three doors, with the public route as the control on
/// every lane.
#[tokio::test]
async fn a_role_gated_route_admits_only_holders_on_every_door() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().to_path_buf();
    let source_root = project_dir.join("src");
    let dist_dir = project_dir.join(".albedo").join("dist");
    let db_path = project_dir.join("data").join("forge.db");
    fs::create_dir_all(&source_root).expect("src");
    fs::create_dir_all(dist_dir.join("_albedo")).expect("dist");
    fs::create_dir_all(db_path.parent().expect("data dir")).expect("data");
    fs::write(source_root.join("Home.tsx"), PUBLIC_TSX).expect("Home.tsx");
    fs::write(
        dist_dir.join("render-manifest.v2.json"),
        manifest_json(Path::new("src/Home.tsx")),
    )
    .expect("manifest");
    fs::write(dist_dir.join("_albedo").join("runtime.js"), b"// stub\n").expect("runtime");

    // People first, then the server — see the module docs.
    let (holder, outsider, hr_holder, both_holder) = {
        let db = LibSqlSubstrate::open_local(&db_path).await.expect("db");
        let auth_schema = schema::augment(&ForgeSchema::build(Vec::new()).expect("empty"))
            .expect("auth tables");
        bootstrap_schema(&db, &auth_schema).await.expect("bootstrap");
        let holder = signed_in(&db, "pat", &["payroll"]).await;
        let outsider = signed_in(&db, "sam", &[]).await;
        let hr_holder = signed_in(&db, "harriet", &["hr"]).await;
        let both_holder = signed_in(&db, "boss", &["hr", "payroll"]).await;
        (holder, outsider, hr_holder, both_holder)
    };

    std::env::set_current_dir(&project_dir).expect("cd");
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let mut providers = BTreeMap::new();
    providers.insert("password".to_string(), ProviderDecl::default());
    let opts = ProductionServerOptions {
        project_dir: project_dir.clone(),
        source_root,
        dist_dir,
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
            roles: vec!["payroll".to_string(), "hr".to_string()],
            ..Default::default()
        },
        tls: Default::default(),
        forge_db: Some(db_path),
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
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");
    let form = load_public_page(&client, &base).await;

    for (who, session, page, lane_ok, action_refusal) in [
        ("a stranger", None, StatusCode::UNAUTHORIZED, vec!["/"], Some(StatusCode::UNAUTHORIZED)),
        ("someone without the role", Some(&outsider), StatusCode::FORBIDDEN, vec!["/"], Some(StatusCode::FORBIDDEN)),
        ("the role holder", Some(&holder), StatusCode::OK, vec!["/", "/payroll"], None),
    ] {
        let cookies = session.map(|session| format!("{}; {session}", form.cookie));

        // ── the render ──
        let mut request = client.get(format!("{base}/payroll"));
        if let Some(cookies) = &cookies {
            request = request.header("cookie", cookies);
        }
        let response = request.send().await.expect("page");
        assert_eq!(response.status(), page, "{who}: the page render");
        if page == StatusCode::FORBIDDEN {
            let body = response.text().await.expect("body");
            assert!(
                !body.contains("payroll</p>"),
                "{who}: a refusal must not carry the page: {body}"
            );
        }

        // Negotiation hints and a guessed transport session must not bypass
        // the render gate that precedes the WebTransport bridge.
        let mut request = client
            .get(format!("{base}/payroll"))
            .header("x-albedo-wt-session", "00000000-0000-0000-0000-000000000001")
            .header("x-albedo-wt-prefer", "webtransport")
            .header("x-forwarded-proto", "h3");
        if let Some(cookies) = &cookies {
            request = request.header("cookie", cookies);
        }
        let response = request.send().await.expect("transport-hinted page");
        assert_eq!(response.status(), page, "{who}: transport-hinted gate");
        if page == StatusCode::FORBIDDEN {
            assert_eq!(response.headers()["vary"], "Cookie");
            assert_eq!(response.headers()["cache-control"], "no-store");
            assert!(!response.text().await.expect("refusal body").contains("payroll"));
        }

        // ── the live lane, with the public route as the control ──
        let (ok, denied) = subscribe(&client, &base, cookies.as_deref(), &["/", "/payroll"]).await;
        assert_eq!(ok, lane_ok, "{who}: the live lane grants");
        let expected_denied: Vec<String> = if lane_ok.len() == 2 {
            Vec::new()
        } else {
            vec!["/payroll".to_string()]
        };
        assert_eq!(denied, expected_denied, "{who}: the live lane denies");

        // ── the route's action ──
        let mut request = client
            .post(format!("{base}/_albedo/action/{GATED_ACTION}"))
            .header("cookie", cookies.clone().unwrap_or_else(|| form.cookie.clone()))
            .form(&[
                ("_csrf", form.csrf.as_str()),
                ("_albedo_return", form.return_path.as_str()),
                ("author", "x"),
            ]);
        request = request.header("accept", "application/json");
        let status = request.send().await.expect("action").status();
        match action_refusal {
            Some(expected) => assert_eq!(status, expected, "{who}: the action"),
            // No handler is registered in a hand-written manifest, so success is
            // "not refused by the gate" — the gate answers before the lookup.
            None => assert!(
                status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                "{who}: the action must pass the gate, got {status}"
            ),
        }
    }

    // A list on one route is OR. The same action on separate routes is AND,
    // even when a third, public route declares that action too.
    for (who, session, hr_page, either_page, shared_refusal, either_refusal) in [
        ("stranger", None, StatusCode::UNAUTHORIZED, StatusCode::UNAUTHORIZED,
            Some(StatusCode::UNAUTHORIZED), Some(StatusCode::UNAUTHORIZED)),
        ("no roles", Some(&outsider), StatusCode::FORBIDDEN, StatusCode::FORBIDDEN,
            Some(StatusCode::FORBIDDEN), Some(StatusCode::FORBIDDEN)),
        ("payroll only", Some(&holder), StatusCode::FORBIDDEN, StatusCode::OK,
            Some(StatusCode::FORBIDDEN), None),
        ("hr only", Some(&hr_holder), StatusCode::OK, StatusCode::OK,
            Some(StatusCode::FORBIDDEN), None),
        ("both roles", Some(&both_holder), StatusCode::OK, StatusCode::OK, None, None),
    ] {
        let cookies = session.map(|session| format!("{}; {session}", form.cookie));
        for (path, expected) in [("/hr", hr_page), ("/either", either_page)] {
            let mut request = client.get(format!("{base}{path}"));
            if let Some(cookies) = &cookies {
                request = request.header("cookie", cookies);
            }
            assert_eq!(request.send().await.expect("page").status(), expected,
                "{who}: {path} render");
        }
        let (ok, denied) = subscribe(&client, &base, cookies.as_deref(), &["/", "/hr", "/either"]).await;
        for (path, allowed) in [("/", true), ("/hr", hr_page == StatusCode::OK),
            ("/either", either_page == StatusCode::OK)] {
            assert_eq!(ok.iter().any(|candidate| candidate == path), allowed,
                "{who}: {path} live grant");
            assert_eq!(denied.iter().any(|candidate| candidate == path), !allowed,
                "{who}: {path} live denial");
        }
        for (action, refusal) in [(SHARED_ACTION, shared_refusal), (ANY_ROLE_ACTION, either_refusal)] {
            let status = client.post(format!("{base}/_albedo/action/{action}"))
                .header("cookie", cookies.clone().unwrap_or_else(|| form.cookie.clone()))
                .header("accept", "application/json")
                .form(&[("_csrf", form.csrf.as_str()),
                    ("_albedo_return", form.return_path.as_str())])
                .send().await.expect("action").status();
            match refusal {
                Some(expected) => assert_eq!(status, expected, "{who}: {action}"),
                None => assert!(status != StatusCode::UNAUTHORIZED && status != StatusCode::FORBIDDEN,
                    "{who}: {action} must pass its gate, got {status}"),
            }
        }
    }
}
