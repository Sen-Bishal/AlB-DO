//! Real-build role resolution through components, actions, and queued jobs.
#![cfg(feature = "forge")]

use dom_render_compiler::auth::{schema, store};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};
use reqwest::{redirect::Policy, StatusCode};
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PAGE: &str = r#"
import { action, useSharedSlot } from "albedo";
export const auth = { role: ["payroll", "hr"] };
export const record_roles = action(({ user }) => {
  append("guestbook", { author: "role_action", message: user.roles.join(",") });
  enqueue("record_roles_job", {}, { id: "role-job" });
});
export default function Roles({ user }) {
  const entries = useSharedSlot("guestbook");
  return <main><p>resolved_roles={user.roles.join(",")}</p>
    <p>rows={entries.length}</p>
    <form action="action:record_roles" method="POST"><button>record</button></form>
  </main>;
}
"#;

const JOBS: &str = r#"
import { job, append } from "albedo/jobs";
export const record_roles_job = job({}, (args, ctx) => {
  append("guestbook", { author: "role_job", message: ctx.user.roles.join(",") });
});
"#;

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(home: &Path, cwd: &Path, db: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_albedo"));
    cmd.current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("ALBEDO_FORGE_DB", db)
        .env("ALBEDO_NO_TUI", "1");
    cmd
}

fn run_ok(cmd: &mut Command) {
    let out = cmd.output().expect("CLI starts");
    assert!(
        out.status.success(),
        "CLI {:?}: {} {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn attr(tag: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let value = &tag[tag.find(&needle)? + needle.len()..];
    Some(value[..value.find('"')?].to_string())
}

async fn start(home: &Path, app: &Path, db: &Path) -> (Server, String) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port")
        .local_addr()
        .expect("addr")
        .port();
    let log = app.join("serve.stderr.log");
    let mut server = Server(
        cli(home, app, db)
            .args(["serve", ".", "--port", &port.to_string()])
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).expect("log"))
            .spawn()
            .expect("serve"),
    );
    let base = format!("http://127.0.0.1:{port}");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if reqwest::get(format!("{base}/_albedo/runtime.js"))
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            server.0.try_wait().expect("child status").is_none(),
            "serve exited: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "serve timed out: {}",
            fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (server, base)
}

async fn assert_live_access(client: &reqwest::Client, base: &str, cookie: &str, allowed: bool) {
    let mut trunk = client
        .get(format!("{base}/_albedo/phosphor"))
        .header("cookie", cookie)
        .send()
        .await
        .expect("trunk");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut hello = String::new();
    let lane = loop {
        if let Some(start) = hello.find(r#""lane":""#) {
            let rest = &hello[start + 8..];
            if let Some(end) = rest.find('"') {
                break rest[..end].to_string();
            }
        }
        let chunk = tokio::time::timeout_at(deadline, trunk.chunk())
            .await
            .expect("hello deadline")
            .expect("hello read")
            .expect("hello chunk");
        hello.push_str(&String::from_utf8_lossy(&chunk));
    };
    let response = client
        .post(format!("{base}/_albedo/phosphor/routes"))
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(serde_json::json!({"lane":lane,"add":[{"p":"/roles","n":"check"}]}).to_string())
        .send()
        .await
        .expect("subscribe");
    let outcome: serde_json::Value =
        serde_json::from_str(&response.text().await.expect("subscribe body")).expect("JSON");
    assert_eq!(
        outcome[if allowed { "ok" } else { "denied" }],
        serde_json::json!(["/roles"]),
        "{outcome}"
    );

    // A denied SSE lane stays open but must carry no patch, even on reconnect.
    let mut response = client
        .get(format!("{base}/_albedo/patches?p=/roles"))
        .header("cookie", cookie)
        .header("last-event-id", "0")
        .send()
        .await
        .expect("SSE");
    let deadline = tokio::time::Instant::now() + Duration::from_millis(350);
    let mut events = String::new();
    while let Ok(Ok(Some(chunk))) = tokio::time::timeout_at(deadline, response.chunk()).await {
        events.push_str(&String::from_utf8_lossy(&chunk));
    }
    assert_eq!(
        events.contains("event: patch"),
        allowed,
        "SSE allowed={allowed}: {events}"
    );
}

#[tokio::test]
async fn declared_roles_reach_real_components_actions_and_jobs_and_revoke_on_reconnect() {
    let home = tempfile::tempdir().expect("home");
    let work = tempfile::tempdir().expect("work");
    let db = work.path().join("role data.db");
    run_ok(cli(home.path(), work.path(), &db).args(["init", "roleapp"]));
    let app = work.path().join("roleapp");
    let config_path = app.join("albedo.config.ts");
    let config = fs::read_to_string(&config_path)
        .expect("config")
        .replace("// roles: [\"admin\"],", "roles: [\"payroll\", \"hr\"],");
    assert!(config.contains("roles: [\"payroll\", \"hr\"]"));
    fs::write(&config_path, config).expect("roles config");
    fs::write(app.join("src/routes/roles.tsx"), PAGE.replace('\n', "\r\n")).expect("CRLF route");
    fs::write(app.join("src/jobs.ts"), JOBS).expect("jobs");
    run_ok(cli(home.path(), &app, &db).arg("build"));

    let now = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms");
    let cookie = {
        let substrate = LibSqlSubstrate::open_local(&db).await.expect("db");
        let auth_schema =
            schema::augment(&ForgeSchema::build(Vec::new()).expect("schema")).expect("auth schema");
        bootstrap_schema(&substrate, &auth_schema)
            .await
            .expect("bootstrap");
        let principal = store::upsert_principal(
            &substrate,
            "password",
            "role-user",
            &store::ProviderProfile {
                email: Some("roles@example.com".into()),
                name: None,
                image: None,
                claims: Default::default(),
            },
            now,
        )
        .await
        .expect("person");
        // An old grant remains in storage, but must never reach any execution context.
        store::grant_role(&substrate, &principal.id, "retired", Some("test"), now)
            .await
            .expect("old grant");
        let session = store::create_session(&substrate, &principal.id, "password", now, 86_400_000)
            .await
            .expect("session");
        format!("__Host-albedo_session={}", session.expose_for_cookie())
    };
    let (server, base) = start(home.path(), &app, &db).await;
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");
    assert_eq!(
        client
            .get(format!("{base}/roles"))
            .header("accept", "text/html")
            .send()
            .await
            .expect("anonymous navigation")
            .status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        client
            .get(format!("{base}/roles"))
            .header("accept", "application/json")
            .send()
            .await
            .expect("anonymous JSON")
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_live_access(&client, &base, "", false).await;
    assert_eq!(
        client
            .get(format!("{base}/roles"))
            .header("cookie", &cookie)
            .send()
            .await
            .expect("outsider")
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_live_access(&client, &base, &cookie, false).await;
    // The only writer is the actual CLI in another process while serve is live.
    run_ok(cli(home.path(), &app, &db).args(["roles", "grant", "roles@example.com", "payroll"]));
    let page = client
        .get(format!("{base}/roles"))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("holder page");
    assert_eq!(page.status(), StatusCode::OK);
    let tab_cookies = page
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| v.split(';').next().expect("cookie").to_string())
        .collect::<Vec<_>>()
        .join("; ");
    let cookies = format!("{cookie}; {tab_cookies}");
    let html = page.text().await.expect("HTML");
    assert!(
        html.contains("resolved_roles=payroll"),
        "component did not receive narrowed roles: {html}"
    );
    assert!(!html.contains("retired"), "undeclared role leaked: {html}");
    assert_live_access(&client, &base, &cookie, true).await;
    let form_tag = html
        .split("<form")
        .nth(1)
        .expect("form")
        .split('>')
        .next()
        .expect("tag");
    let action = attr(form_tag, "action").expect("action URL");
    let fields: Vec<_> = html
        .split("<input")
        .skip(1)
        .filter_map(|input| {
            let tag = input.split('>').next()?;
            if attr(tag, "type").as_deref() != Some("hidden") {
                return None;
            }
            Some((attr(tag, "name")?, attr(tag, "value")?))
        })
        .collect();
    let posted = client
        .post(format!("{base}{action}"))
        .header("cookie", &cookies)
        .form(&fields)
        .send()
        .await
        .expect("action");
    assert!(
        posted.status().is_success() || posted.status().is_redirection(),
        "action failed: {} {}",
        posted.status(),
        posted.text().await.expect("error")
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let html = client
            .get(format!("{base}/guestbook"))
            .send()
            .await
            .expect("guestbook")
            .text()
            .await
            .expect("body");
        if html.contains("role_action") && html.contains("role_job") {
            assert!(
                !html.contains("retired"),
                "undeclared role leaked in action or job: {html}"
            );
            assert!(
                html.matches("payroll").count() >= 2,
                "both execution contexts must write payroll: {html}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "action/job did not write their roles: {html}"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    run_ok(cli(home.path(), &app, &db).args(["roles", "revoke", "roles@example.com", "payroll"]));
    assert_eq!(
        client
            .get(format!("{base}/roles"))
            .header("cookie", &cookie)
            .send()
            .await
            .expect("revoked page")
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .post(format!("{base}{action}"))
            .header("cookie", &cookies)
            .form(&fields)
            .send()
            .await
            .expect("revoked action")
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_live_access(&client, &base, &cookie, false).await;
    drop(server);
    let substrate = LibSqlSubstrate::open_local(&db)
        .await
        .expect("reopen after serve");
    let rows = substrate.query(
        "SELECT author, message FROM guestbook WHERE author IN ('role_action', 'role_job') ORDER BY author",
        &[],
    ).await.expect("read execution evidence");
    assert_eq!(rows.rows.len(), 2, "each context must write exactly once");
    for (row, author) in rows.rows.iter().zip(["role_action", "role_job"]) {
        assert_eq!(row.get(0).and_then(|value| value.as_str()), Some(author));
        assert_eq!(
            row.get(1).and_then(|value| value.as_str()),
            Some("payroll"),
            "the actual persisted context must contain only declared roles"
        );
    }
}
