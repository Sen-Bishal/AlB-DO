//! AUTH · P2 — the sign-up/sign-in round trip over real HTTP.
//!
//! `tests/auth_password_flow.rs` in the compiler crate covers the store half:
//! the uniqueness constraint, the hash, the session row. Its own header says the
//! rest — *"CSRF, the cookie, the redirect, the limiter — is the server
//! crate's"* — and this is that half. `AUTH.md` § 9's gate for P2 is **"a
//! stranger signs up and logs in with no third party involved"**, and a stranger
//! arrives over HTTP holding nothing.
//!
//! ## The regression this exists to prevent
//!
//! Every endpoint below was already correct when this file was written, and the
//! gate still failed, because **there was no way to author the form**. The
//! renderers emitted the hidden CSRF input only for a `<form action="action:NAME">`
//! sentinel; the sign-in endpoints are URLs, not action names, so a served login
//! form carried no token and `run_auth_route` answered every real submit with
//! `403 This form is stale`. Unit tests could not see it — each one supplied its
//! own token, so they proved the gate worked and never asked whether a browser
//! could get through it. Compare the egress bug in APERTURE: every unit test
//! called `check_address` itself, so none of them noticed that nothing *routed*
//! to it.
//!
//! So this starts where a stranger starts — at a GET of a page — and submits
//! **only** what that page actually served.
//!
//! ## Why a running server and not `Router::oneshot`
//!
//! The FORGE substrate is opened by `run_with_ready`, not by `build()`, and the
//! `AuthRuntime` is installed immediately after it because sessions, users and
//! credentials are FORGE rows. A test driving `server.router()` directly gets a
//! server with **no auth installed at all**, where every endpoint answers `404`
//! — it would faithfully assert the absence of the feature. So this binds a real
//! listener on a free port and speaks HTTP to it.
//!
//! ## Why one test and not five
//!
//! `LibSqlSubstrate::open_local("forge.db")` resolves against the **process**
//! working directory, so every test in this binary would share one database and
//! race for it. One sequential scenario against one server is the honest shape;
//! it is also what the gate actually says — a stranger's whole first session.

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl};
use dom_render_compiler::transforms::form::FORM_HIDDEN_INPUTS;
use reqwest::{redirect::Policy, StatusCode};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The sign-in page a stranger lands on, as source. Kept beside the baked markup
/// below so the fixture says what it is a render *of*: a plain same-origin POST
/// form with no `action:` sentinel, because a sign-in endpoint is a framework
/// route and not an app action.
const SIGN_IN_TSX: &str = r#"
export default function SignIn() {
  return (
    <div>
      <form action="/_albedo/auth/password/register" method="POST">
        <input name="email" />
        <input name="password" type="password" />
      </form>
      <form action="/_albedo/auth/password/login" method="POST">
        <input name="email" />
        <input name="password" type="password" />
      </form>
    </div>
  );
}
"#;

/// The Tier-A render of [`SIGN_IN_TSX`], as `albedo build` bakes it into the
/// manifest.
///
/// The hidden inputs come from [`FORM_HIDDEN_INPUTS`] rather than being spelled
/// out, which is the point: edit what the renderers emit and this fixture
/// follows, instead of quietly testing a stale copy of the markup. That the
/// renderers *do* emit them for a plain same-origin POST form is pinned in the
/// compiler crate — `transforms::form`'s unit tests, the QuickJS shim's parity
/// tests, and the `plain_post_form` conformance fixture, which renders it
/// through both renderers and compares bytes.
fn sign_in_html() -> String {
    let form = |endpoint: &str| {
        format!(
            r#"<form action="{endpoint}" method="POST">{FORM_HIDDEN_INPUTS}<input name="email" /><input name="password" type="password" /></form>"#
        )
    };
    format!(
        "<div>{}{}</div>",
        form("/_albedo/auth/password/register"),
        form("/_albedo/auth/password/login"),
    )
}

fn build_manifest_json(module: &Path) -> String {
    let module_path = module.display().to_string().replace('\\', "/");
    // Escaped for embedding in a JSON string literal.
    let baked = sign_in_html().replace('\\', r"\\").replace('"', r#"\""#);
    format!(
        r#"{{
  "version": 2,
  "build_id": "auth-p2-test",
  "routes": {{
    "/": {{
      "route": "/",
      "shell": {{
        "doctype_and_head": "<!DOCTYPE html><html><head><title>sign in</title></head>",
        "body_open": "<body><div id=\"root\"><!--__SLOT___a_signin_0--></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      }},
      "tier_a_root": [{{
        "component_id": "SignIn",
        "placeholder_id": "__a_signin_0",
        "html": "{baked}",
        "position": {{ "parent_placeholder": null, "slot": "default", "order": 0 }}
      }}],
      "tier_b": [],
      "tier_c": [],
      "shared_slot_topics": [],
      "action_ids": [],
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
    "name": "SignIn",
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
    )
}

/// What a browser holds after loading the sign-in page: the tab-session cookie,
/// and the two hidden fields the renderer stamped into the form.
struct ServedForm {
    cookie: String,
    csrf: String,
    return_path: String,
}

/// Read a hidden input's `value` out of served HTML by field name.
fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// GET the sign-in page and keep only what a browser would have.
async fn load_sign_in_page(client: &reqwest::Client, base: &str) -> ServedForm {
    let response = client
        .get(format!("{base}/"))
        .send()
        .await
        .expect("sign-in page responds");
    assert_eq!(response.status(), StatusCode::OK, "sign-in page must render");

    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with("__Host-albedo-session="))
        .expect("the render mints a tab session cookie")
        .to_string();

    let html = response.text().await.expect("html body");

    let csrf = hidden_field(&html, "_csrf").unwrap_or_else(|| {
        panic!("the served sign-in form carried no CSRF token — a stranger cannot submit it:\n{html}")
    });
    assert!(
        !csrf.is_empty(),
        "the CSRF placeholder reached the browser unfilled:\n{html}"
    );
    let return_path = hidden_field(&html, "_albedo_return")
        .unwrap_or_else(|| panic!("the served form carried no return path:\n{html}"));

    ServedForm {
        cookie,
        csrf,
        return_path,
    }
}

/// Submit a form exactly as a browser with no JavaScript would: urlencoded, with
/// the hidden fields the page supplied.
async fn submit(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    form: &ServedForm,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    let mut body: Vec<(String, String)> = vec![
        ("_csrf".to_string(), form.csrf.clone()),
        ("_albedo_return".to_string(), form.return_path.clone()),
    ];
    for (name, value) in fields {
        body.push(((*name).to_string(), (*value).to_string()));
    }

    client
        .post(format!("{base}{path}"))
        .header("cookie", &form.cookie)
        .form(&body)
        .send()
        .await
        .expect("auth endpoint responds")
}

/// The session cookie an auth response mints, if it minted one.
fn session_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("__Host-albedo_session="))
        .map(str::to_string)
}

/// 🔑 **The P2 gate**, start to finish. Every value submitted came off the served
/// page — nothing here reaches into the CSRF registry to mint itself a token,
/// because a stranger cannot.
#[tokio::test]
async fn a_stranger_signs_up_signs_in_and_signs_out_over_http() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().to_path_buf();
    let source_root = project_dir.join("src");
    let dist_dir = project_dir.join(".albedo").join("dist");
    fs::create_dir_all(&source_root).expect("src dir");
    fs::create_dir_all(dist_dir.join("_albedo")).expect("dist dir");

    let page = source_root.join("SignIn.tsx");
    fs::write(&page, SIGN_IN_TSX).expect("write SignIn.tsx");
    fs::write(
        dist_dir.join("render-manifest.v2.json"),
        build_manifest_json(&page),
    )
    .expect("write manifest");
    fs::write(
        dist_dir.join("_albedo").join("runtime.js"),
        b"// albedo-runtime stub for tests\n",
    )
    .expect("write runtime.js");

    // `forge.db` is opened relative to the process working directory, so move
    // there rather than writing a database into the crate root.
    std::env::set_current_dir(&project_dir).expect("cd into the fixture project");

    // Ask the OS for a free port, then hand it to the server. `BootReport`
    // carries no bound port, so port 0 would leave nothing to connect to.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("probe addr").port()
    };

    // `password` is a known preset, so it infers its kind. An unknown provider
    // name would have to declare one.
    let mut providers = BTreeMap::new();
    providers.insert("password".to_string(), ProviderDecl::default());

    let opts = ProductionServerOptions {
        project_dir: project_dir.clone(),
        source_root,
        dist_dir,
        host: "127.0.0.1".to_string(),
        port,
        dev_mode: false,
        // No `forge` block: boot installs the built-in default, which is what
        // opens the substrate. Auth *requires* one, and declaring providers
        // without a substrate is a startup error by design.
        forge: Default::default(),
        sources: Default::default(),
        auth: AuthDeclaration {
            providers,
            ..Default::default()
        },
    };

    let server = boot_production_server(&opts).expect("server boots with a password provider");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = server
            .run_with_ready(move |_report| {
                let _ = ready_tx.send(());
            })
            .await;
    });
    // The callback fires after the substrate is open and auth is installed —
    // which is the entire reason this test binds a listener at all.
    ready_rx.await.expect("the server signals ready");
    let base = format!("http://127.0.0.1:{port}");

    // 303s are the whole POST/Redirect/GET pattern here, so they are the
    // assertion — not something to follow.
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // ── 1 · the submit that used to be the only possible one ──
    // No token, because nothing emitted one. The fix makes a token *reachable*;
    // it must not relax the gate.
    let untokened = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .form(&[("email", "mallory@example.com"), ("password", "whatever")])
        .send()
        .await
        .expect("responds");
    assert_eq!(untokened.status(), StatusCode::FORBIDDEN);
    assert!(
        session_cookie(&untokened).is_none(),
        "a refused submit must not open a session"
    );

    // ── 2 · sign up ──
    let form = load_sign_in_page(&client, &base).await;
    let response = submit(
        &client,
        &base,
        "/_albedo/auth/password/register",
        &form,
        &[("email", "ada@example.com"), ("password", "a-long-passphrase")],
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "sign-up answers POST/Redirect/GET"
    );
    let signed_up = session_cookie(&response).expect("sign-up opens a session");
    assert!(
        signed_up.contains("HttpOnly") && signed_up.contains("SameSite=Lax"),
        "the session cookie must not be script-readable: {signed_up}"
    );

    // ── 3 · the same email again ──
    // The uniqueness constraint lives in SQLite; this is its HTTP face.
    let form = load_sign_in_page(&client, &base).await;
    let response = submit(
        &client,
        &base,
        "/_albedo/auth/password/register",
        &form,
        &[("email", "ada@example.com"), ("password", "another-one")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // ── 4 · a wrong password opens no session ──
    // The failure path minting a cookie would be the whole point lost.
    let form = load_sign_in_page(&client, &base).await;
    let response = submit(
        &client,
        &base,
        "/_albedo/auth/password/login",
        &form,
        &[("email", "ada@example.com"), ("password", "not-it")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(session_cookie(&response).is_none());

    // ── 5 · sign in ──
    let form = load_sign_in_page(&client, &base).await;
    let response = submit(
        &client,
        &base,
        "/_albedo/auth/password/login",
        &form,
        &[("email", "ada@example.com"), ("password", "a-long-passphrase")],
    )
    .await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session = session_cookie(&response).expect("sign-in opens a session");
    let session_pair = session
        .split(';')
        .next()
        .expect("cookie has a value")
        .to_string();

    // ── 6 · sign out ──
    // Answers success either way — "log out" asks for a state, and a distinct
    // answer for "you were not logged in" would be one more bit about who is
    // holding the cookie. What is asserted is the clear.
    let form = load_sign_in_page(&client, &base).await;
    let response = client
        .post(format!("{base}/_albedo/auth/logout"))
        .header("cookie", format!("{}; {}", form.cookie, session_pair))
        .form(&[
            ("_csrf", form.csrf.as_str()),
            ("_albedo_return", form.return_path.as_str()),
        ])
        .send()
        .await
        .expect("responds");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cleared = session_cookie(&response).expect("logout clears the cookie");
    assert!(
        cleared.contains("Max-Age=0"),
        "logout must expire the session cookie: {cleared}"
    );

    // ── 7 · SHUTTER's credential class ──
    // `AUTH.md` R6 recorded that rate limiting existed nowhere in the tree. This
    // is the check that it now sits on the path a credential-stuffer takes. The
    // assertion is "the run ends throttled" rather than a fixed attempt count —
    // the budget is a tuning decision, and pinning it here would make this fail
    // for a reason that is not a defect.
    let mut statuses = Vec::new();
    for attempt in 0..12 {
        let form = load_sign_in_page(&client, &base).await;
        let response = submit(
            &client,
            &base,
            "/_albedo/auth/password/login",
            &form,
            &[
                ("email", "ada@example.com"),
                ("password", &format!("guess-{attempt}")),
            ],
        )
        .await;
        let status = response.status();
        statuses.push(status);
        if status == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
    }
    assert_eq!(
        statuses.last(),
        Some(&StatusCode::TOO_MANY_REQUESTS),
        "a credential guessing run must be throttled, got {statuses:?}"
    );
}
