//! AUTH § 8.1.3 · `export const auth = "required"` on **every** door, over real
//! HTTP.
//!
//! ## The regression this exists to prevent
//!
//! Route gating shipped enforcing itself at the page render and nowhere else.
//! Two other entry points reach the same route's data and neither consulted the
//! declaration:
//!
//! - `POST /_albedo/phosphor/routes` — the live lane. `authorize_route` resolved
//!   the route's topics and granted them, on the recorded reasoning that a
//!   subscribe *"grants exactly the read the page GET already granted"*. That
//!   was true while the only way a page GET could refuse was resolving no topic.
//!   A declared gate refuses a page whose topics resolve perfectly well — which
//!   is § 8.1.3's motivating case, a dashboard over global data — so an
//!   anonymous lane received the rows of a page whose GET had just answered 401.
//! - `POST /_albedo/action/{name}` and `POST /_albedo/action` — the action the
//!   gated route declares. F1 refuses an anonymous write to an
//!   *identity-partitioned* collection; an action appending to a global one has
//!   no owner to compare against and ran for anybody.
//!
//! ## Why this binds a listener instead of driving the router
//!
//! Same reason `auth_password_http.rs` does: `AuthRuntime` is installed by
//! `run_with_ready` (it needs the FORGE substrate), so a test driving
//! `Router::oneshot` sees no auth at all and would faithfully assert the
//! feature's absence. The principal has to be a *real* one, obtained the way a
//! stranger obtains it, or this proves nothing about reachability — the lesson
//! the CSRF seam paid for.
//!
//! ## Every refusal here is paired with a control
//!
//! A gate that refused everything passes each "is it refused?" assertion and is
//! indistinguishable from a correct one. So each anonymous refusal is asserted
//! **in the same request or against the same server** as something that must
//! still succeed: the public route subscribes while the gated one is denied, and
//! the identical calls succeed once a session exists.

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl};
use dom_render_compiler::transforms::form::{allocate_form_action_id, FORM_HIDDEN_INPUTS};
use reqwest::{redirect::Policy, StatusCode};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// The action the gated route declares. Named once so the manifest entry and the
/// POSTs below cannot drift apart.
const GATED_ACTION: &str = "dash_write";

/// A page carrying a plain same-origin POST form, so the render stamps the
/// hidden inputs a stranger needs (`_csrf`, `_albedo_return`). Public, and it
/// stays public — it is the control.
const PUBLIC_TSX: &str = r#"
export default function Home() {
  return (
    <form action="/_albedo/auth/password/register" method="POST">
      <input name="email" />
      <input name="password" type="password" />
    </form>
  );
}
"#;

fn public_html() -> String {
    format!(
        r#"<form action="/_albedo/auth/password/register" method="POST">{FORM_HIDDEN_INPUTS}<input name="email" /><input name="password" type="password" /></form>"#
    )
}

/// Two routes over the **same** global topic, differing only in the declaration.
///
/// 🔑 That is the whole design of this fixture. `guestbook` is not partitioned
/// by anything, so derived authorization has nothing to say about either route
/// and both resolve the identical topic list. The only thing that can separate
/// them is the declared gate, which is exactly the case § 8.1.3 says route
/// gating exists for and exactly the case the live lane ignored.
fn build_manifest_json(module: &Path) -> String {
    let module_path = module.display().to_string().replace('\\', "/");
    let baked = public_html().replace('\\', r"\\").replace('"', r#"\""#);
    let gated_action_id = allocate_form_action_id(GATED_ACTION);
    format!(
        r#"{{
  "version": 2,
  "build_id": "auth-route-gate-test",
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
    "/dash": {{
      "route": "/dash",
      "auth": "required",
      "shell": {{
        "doctype_and_head": "<!DOCTYPE html><html><head><title>dash</title></head>",
        "body_open": "<body><div id=\"root\"><!--__SLOT___a_dash_0--></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      }},
      "tier_a_root": [{{
        "component_id": "Home",
        "placeholder_id": "__a_dash_0",
        "html": "<p>dash</p>",
        "position": {{ "parent_placeholder": null, "slot": "default", "order": 0 }}
      }}],
      "tier_b": [],
      "tier_c": [],
      "shared_slot_topics": ["guestbook"],
      "action_ids": [{{ "name": "{GATED_ACTION}", "action_id": {gated_action_id} }}],
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
    )
}

/// What a browser holds after loading a page: the tab cookie and the hidden
/// fields the renderer stamped. Read off the served HTML, never minted here.
struct ServedForm {
    cookie: String,
    csrf: String,
    return_path: String,
}

fn hidden_field(html: &str, name: &str) -> Option<String> {
    let needle = format!(r#"name="{name}" value=""#);
    let start = html.find(&needle)? + needle.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

async fn load_public_page(client: &reqwest::Client, base: &str) -> ServedForm {
    let response = client
        .get(format!("{base}/"))
        .send()
        .await
        .expect("home page responds");
    assert_eq!(response.status(), StatusCode::OK, "the public route renders");

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
    let csrf = hidden_field(&html, "_csrf")
        .unwrap_or_else(|| panic!("the served form carried no CSRF token:\n{html}"));
    let return_path = hidden_field(&html, "_albedo_return")
        .unwrap_or_else(|| panic!("the served form carried no return path:\n{html}"));

    ServedForm {
        cookie,
        csrf,
        return_path,
    }
}

/// Open a PHOSPHOR trunk and read the `hello` event's lane id off the SSE
/// stream, without waiting for the stream to end (it never does).
///
/// 🪤 **The response comes back with the id and the caller must hold it.** A
/// lane lives exactly as long as its trunk: `LaneGuard::drop` unregisters it the
/// moment the stream is dropped, so a helper that returned only the `String`
/// would hand back the id of a lane that no longer exists and every subscribe
/// would answer `404 unknown lane` — which reads exactly like a route being
/// denied if you are not looking closely.
async fn open_lane(
    client: &reqwest::Client,
    base: &str,
    cookie: Option<&str>,
) -> (String, reqwest::Response) {
    let mut request = client.get(format!("{base}/_albedo/phosphor"));
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let mut response = request.send().await.expect("trunk opens");
    assert_eq!(response.status(), StatusCode::OK, "the trunk is served");

    // `hello` is the first thing written, before the select loop, so one chunk
    // is enough.
    let chunk = response
        .chunk()
        .await
        .expect("trunk streams")
        .expect("the trunk announces itself");
    let text = String::from_utf8_lossy(&chunk).to_string();
    let marker = r#""lane":""#;
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("no lane id in the hello event:\n{text}"))
        + marker.len();
    let rest = &text[start..];
    let end = rest.find('"').expect("lane id terminates");
    (rest[..end].to_string(), response)
}

/// Subscribe a lane to route paths and return the raw `{ok, denied}` answer.
async fn subscribe(
    client: &reqwest::Client,
    base: &str,
    lane: &str,
    paths: &[&str],
    cookie: Option<&str>,
) -> serde_json::Value {
    let add: Vec<serde_json::Value> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| serde_json::json!({ "p": path, "n": format!("probe{index}") }))
        .collect();
    let body = serde_json::json!({ "lane": lane, "add": add }).to_string();
    let mut request = client
        .post(format!("{base}/_albedo/phosphor/routes"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(cookie) = cookie {
        request = request.header("cookie", cookie);
    }
    let response = request.send().await.expect("subscribe responds");
    assert_eq!(response.status(), StatusCode::OK, "subscribe is served");
    let text = response.text().await.expect("subscribe answers");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("not JSON ({err}): {text}"))
}

fn granted(outcome: &serde_json::Value, key: &str) -> Vec<String> {
    outcome[key]
        .as_array()
        .expect("outcome carries both lists")
        .iter()
        .map(|value| value.as_str().expect("a route path").to_string())
        .collect()
}

/// 🔑 The gate, on all three doors, with controls throughout.
#[tokio::test]
async fn a_gated_route_refuses_the_live_lane_and_its_action_to_a_stranger() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().to_path_buf();
    let source_root = project_dir.join("src");
    let dist_dir = project_dir.join(".albedo").join("dist");
    fs::create_dir_all(&source_root).expect("src dir");
    fs::create_dir_all(dist_dir.join("_albedo")).expect("dist dir");

    let page = source_root.join("Home.tsx");
    fs::write(&page, PUBLIC_TSX).expect("write Home.tsx");
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

    std::env::set_current_dir(&project_dir).expect("cd into the fixture project");

    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("probe addr").port()
    };

    // A provider is required for `auth = "required"` to boot at all — a gated
    // route with nobody able to sign in refuses every request forever, and the
    // boot check says so.
    let mut providers = BTreeMap::new();
    providers.insert("password".to_string(), ProviderDecl::default());

    let opts = ProductionServerOptions {
        project_dir: project_dir.clone(),
        source_root,
        dist_dir,
        host: "127.0.0.1".to_string(),
        port,
        dev_mode: false,
        forge: Default::default(),
        sources: Default::default(),
        auth: AuthDeclaration {
            providers,
            ..Default::default()
        },
    };

    let server = boot_production_server(&opts).expect("server boots");
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = server
            .run_with_ready(move |_report| {
                let _ = ready_tx.send(());
            })
            .await;
    });
    ready_rx.await.expect("the server signals ready");
    let base = format!("http://127.0.0.1:{port}");

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // ── 1 · the door that already worked, so the fixture is known-good ──
    let anonymous_page = client
        .get(format!("{base}/dash"))
        .send()
        .await
        .expect("responds");
    assert_eq!(
        anonymous_page.status(),
        StatusCode::UNAUTHORIZED,
        "the page render gate is the one that already shipped; if this fails the \
         fixture is wrong, not the fix"
    );

    // ── 2 · the live lane, anonymously ──
    //
    // Both routes read the *same* topic, so a lane that grants `/` and denies
    // `/dash` is separating them on the declaration alone. Asking for both in one
    // request is the control: it rules out "the lane is simply broken".
    let form = load_public_page(&client, &base).await;
    let (lane, _anonymous_trunk) = open_lane(&client, &base, None).await;
    let outcome = subscribe(&client, &base, &lane, &["/", "/dash"], None).await;

    assert_eq!(
        granted(&outcome, "denied"),
        vec!["/dash".to_string()],
        "an anonymous lane must not subscribe to a route whose GET answers 401"
    );
    assert_eq!(
        granted(&outcome, "ok"),
        vec!["/".to_string()],
        "and the public route over the identical topic must still be granted — \
         without this the refusal above could just be a dead lane"
    );

    // ── 3 · the action the gated route declares, anonymously ──
    let anonymous_action = client
        .post(format!("{base}/_albedo/action/{GATED_ACTION}"))
        .header("cookie", &form.cookie)
        .form(&[
            ("_csrf", form.csrf.as_str()),
            ("_albedo_return", form.return_path.as_str()),
            ("author", "intruder"),
        ])
        .send()
        .await
        .expect("responds");
    assert_eq!(
        anonymous_action.status(),
        StatusCode::UNAUTHORIZED,
        "a valid CSRF token proves the request is genuine, not that it is anybody"
    );

    // ── 4 · become somebody, the way a stranger does ──
    let registered = client
        .post(format!("{base}/_albedo/auth/password/register"))
        .header("cookie", &form.cookie)
        .form(&[
            ("_csrf", form.csrf.as_str()),
            ("_albedo_return", form.return_path.as_str()),
            ("email", "dash@example.com"),
            ("password", "correct horse battery staple"),
        ])
        .send()
        .await
        .expect("responds");
    let session = registered
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .find(|value| value.starts_with("__Host-albedo_session="))
        .expect("registering signs the new account in")
        .to_string();
    let both_cookies = format!("{}; {session}", form.cookie);

    // ── 5 · every refusal above, now under a principal ──
    //
    // This is what makes the three assertions above evidence rather than the
    // behaviour of a gate that says no to everything.
    let signed_in_page = client
        .get(format!("{base}/dash"))
        .header("cookie", &both_cookies)
        .send()
        .await
        .expect("responds");
    assert_eq!(signed_in_page.status(), StatusCode::OK, "control: the render");

    let (lane, _signed_in_trunk) = open_lane(&client, &base, Some(&both_cookies)).await;
    let outcome = subscribe(
        &client,
        &base,
        &lane,
        &["/", "/dash"],
        Some(&both_cookies),
    )
    .await;
    assert!(
        granted(&outcome, "denied").is_empty(),
        "control: a signed-in lane is denied nothing — got {outcome}"
    );
    assert_eq!(
        granted(&outcome, "ok"),
        vec!["/".to_string(), "/dash".to_string()],
        "control: the live lane"
    );

    let signed_in_action = client
        .post(format!("{base}/_albedo/action/{GATED_ACTION}"))
        .header("cookie", &both_cookies)
        .form(&[
            ("_csrf", form.csrf.as_str()),
            ("_albedo_return", form.return_path.as_str()),
            ("author", "the owner"),
        ])
        .send()
        .await
        .expect("responds");
    // 🪤 The success shape here is `404`, not `200`, and that is correct: this
    // fixture is a hand-written manifest with no compiled project behind it, so
    // no handler is registered under the id. What matters is *which* refusal —
    // the gate answers before the registry lookup, so `401` means the gate fired
    // and anything else means it did not. Asserting the exact success status
    // would pin this test to the absence of a handler rather than to the gate.
    assert_ne!(
        signed_in_action.status(),
        StatusCode::UNAUTHORIZED,
        "control: the same action, the same token, a resolved principal"
    );
}
