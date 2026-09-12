//! AUTH · 15.4 — the OAuth legs over real HTTP.
//!
//! ## What this file can and cannot prove
//!
//! `require_https` refuses an `http` provider endpoint in every environment,
//! deliberately and with no dev exemption — an authorization code crossing
//! plaintext is the same mistake in dev as in production. So a fake
//! authorization server on loopback **cannot be declared**, and this file does
//! not pretend to drive a full round trip.
//!
//! What it does drive is the half that has no third party in it, over a real
//! listener with a real browser client:
//!
//! - the dispatch — that `/_albedo/auth/oauth/{provider}/start` reaches a handler at all;
//! - the authorize redirect, in full, including PKCE and `state`;
//! - the pending-flow cookie's attributes, on the wire rather than in a format string;
//! - every refusal on the callback that does not require a provider.
//!
//! **The `start` leg for a plain OAuth provider makes no outbound call**, which
//! is what makes this possible: GitHub's endpoints are constants in the preset
//! table, so the whole redirect is computable offline.
//!
//! The token exchange and the profile read are covered in the compiler crate's
//! unit tests against fixed response bodies, including GitHub's form-encoded
//! token response and its integer `id`. What neither covers is a real TLS
//! conversation with a real provider — that needs an account and a registered
//! client, and it sits with the ACME rehearsal as a thing only a real third
//! party can settle.
//!
//! ## Why a running server and not `Router::oneshot`
//!
//! The same reason `auth_password_http.rs` gives: `AuthRuntime` is installed by
//! `run_with_ready`, after the substrate opens. A test driving `router()`
//! directly gets a server with no auth at all, where these endpoints answer
//! `404` — and would faithfully assert the absence of the feature.

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::auth::declare::{AuthDeclaration, ProviderDecl, SecretDecl};
use reqwest::{redirect::Policy, StatusCode};
use std::collections::BTreeMap;
use std::fs;
use url::Url;

/// A minimal manifest with one static route, so the server has something to
/// serve and `ReturnPath` has somewhere to point.
fn manifest_json() -> String {
    r#"{
  "version": 2,
  "build_id": "auth-oauth-test",
  "routes": {
    "/": {
      "route": "/",
      "shell": {
        "doctype_and_head": "<!DOCTYPE html><html><head><title>home</title></head>",
        "body_open": "<body><div id=\"root\"><!--__SLOT___a_home_0--></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      },
      "tier_a_root": [{
        "component_id": "Home",
        "placeholder_id": "__a_home_0",
        "html": "<p>home</p>",
        "position": { "parent_placeholder": null, "slot": "default", "order": 0 }
      }],
      "tier_b": [],
      "tier_c": [],
      "shared_slot_topics": [],
      "action_ids": [],
      "layout_chain": [],
      "error_component": null,
      "loading_component": null
    }
  },
  "assets": { "chunks": {}, "css": [], "runtime": "/_albedo/runtime.js" },
  "schema_version": "2.0",
  "generated_at": "",
  "components": [{
    "id": 0,
    "name": "Home",
    "module_path": "src/Home.tsx",
    "tier": "A",
    "weight_bytes": 100,
    "priority": 1.0,
    "dependencies": [],
    "can_defer": true,
    "hydration_mode": "none"
  }],
  "parallel_batches": [],
  "critical_path": [],
  "vendor_chunks": [],
  "wt_streams": []
}"#
    .to_string()
}

/// The `Set-Cookie` for one cookie name, whole, with its attributes.
fn cookie_named<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with(&format!("{name}=")))
}

#[tokio::test]
async fn the_oauth_legs_are_reachable_and_the_redirect_is_guarded() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().to_path_buf();
    let source_root = project_dir.join("src");
    let dist_dir = project_dir.join(".albedo").join("dist");
    fs::create_dir_all(&source_root).expect("src dir");
    fs::create_dir_all(dist_dir.join("_albedo")).expect("dist dir");
    fs::write(source_root.join("Home.tsx"), "export default function Home(){}")
        .expect("write Home.tsx");
    fs::write(dist_dir.join("render-manifest.v2.json"), manifest_json()).expect("write manifest");
    fs::write(
        dist_dir.join("_albedo").join("runtime.js"),
        b"// albedo-runtime stub for tests\n",
    )
    .expect("write runtime.js");

    // `forge.db` resolves against the process working directory.
    std::env::set_current_dir(&project_dir).expect("cd into the fixture project");

    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("probe addr").port()
    };

    // `github` is a preset, so it infers its kind and its three endpoints. The
    // credentials are literals because this test never reaches a token
    // endpoint — only the authorize URL, which carries the client id.
    let mut providers = BTreeMap::new();
    providers.insert(
        "github".to_string(),
        ProviderDecl {
            client_id: Some(SecretDecl::Value {
                value: "test-client-id".to_string(),
            }),
            client_secret: Some(SecretDecl::Value {
                value: "test-client-secret".to_string(),
            }),
            ..ProviderDecl::default()
        },
    );

    let opts = ProductionServerOptions {
        project_dir: project_dir.clone(),
        source_root,
        dist_dir,
        host: "127.0.0.1".to_string(),
        port,
        dev_mode: false,
        forge: Default::default(),
        sources: Default::default(),
        uploads: Default::default(),
        auth: AuthDeclaration {
            providers,
            ..Default::default()
        },
        tls: Default::default(),
    };

    let server = boot_production_server(&opts).expect("server boots with an oauth provider");
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

    // The redirect *is* the assertion.
    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .build()
        .expect("client");

    // ── 1 · the leg exists at all ──
    // The failure this catches is the one the whole item was: a declared
    // provider, a preset with endpoints, and no route to any of it. Before this
    // work the answer here was a 404 from the page router.
    let start = client
        .get(format!("{base}/_albedo/auth/oauth/github/start?next=/dashboard"))
        .send()
        .await
        .expect("start responds");
    assert_eq!(
        start.status(),
        StatusCode::FOUND,
        "the start leg must redirect to the provider, not answer a page"
    );

    // ── 2 · the redirect is the guarded one ──
    let location = start
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .expect("a Location header")
        .to_string();
    let authorize = Url::parse(&location).expect("Location is an absolute URL");
    assert_eq!(authorize.host_str(), Some("github.com"));
    assert_eq!(authorize.path(), "/login/oauth/authorize");

    let params: BTreeMap<String, String> = authorize.query_pairs().into_owned().collect();
    assert_eq!(params.get("client_id").map(String::as_str), Some("test-client-id"));
    assert_eq!(params.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(
        params.get("code_challenge_method").map(String::as_str),
        Some("S256"),
        "PKCE is not optional and there is no flag that removes it"
    );
    assert!(
        params.get("code_challenge").is_some_and(|c| !c.is_empty()),
        "a challenge method with no challenge is worse than neither"
    );
    assert!(
        params.get("state").is_some_and(|s| s.len() >= 40),
        "state must carry real entropy: {params:?}"
    );
    assert_eq!(
        params.get("redirect_uri").map(String::as_str),
        Some(format!("{base}/_albedo/auth/oauth/github/callback").as_str()),
        "the redirect URI must be absolute and must be our own callback"
    );
    assert_eq!(
        params.get("scope").map(String::as_str),
        Some("read:user user:email"),
        "the preset's scopes must survive the trip to the wire"
    );

    // ── 3 · the pending flow, on the wire ──
    let flow_cookie = cookie_named(&start, "__Host-albedo_oauth")
        .expect("the start leg must remember the flow")
        .to_string();
    assert!(
        flow_cookie.contains("HttpOnly"),
        "the PKCE verifier must not be script-readable: {flow_cookie}"
    );
    assert!(
        flow_cookie.contains("SameSite=Lax") && !flow_cookie.contains("SameSite=Strict"),
        "Strict would withhold this on the provider's navigation back, breaking every \
         sign-in: {flow_cookie}"
    );
    // The pairing, checked rather than assumed: the cookie's first segment is
    // the same `state` this very redirect carries. A cookie holding *a* state
    // and a URL carrying *a* state would satisfy every assertion above and
    // still be two unrelated flows.
    let cookie_state = flow_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.split_once('='))
        .map(|(_, value)| value.split('.').next().unwrap_or_default())
        .expect("the cookie has a value");
    assert_eq!(
        cookie_state,
        params.get("state").unwrap().as_str(),
        "the remembered state must be the one the browser is about to present"
    );

    // ── 4 · a callback with no flow in progress ──
    // A bookmarked callback URL, or an attacker opening one directly. There is
    // no cookie, so there is nothing to compare, and the request must not be
    // treated as a sign-in.
    let orphan = client
        .get(format!("{base}/_albedo/auth/oauth/github/callback?code=x&state=y"))
        .send()
        .await
        .expect("callback responds");
    assert_eq!(orphan.status(), StatusCode::BAD_REQUEST);
    assert!(
        cookie_named(&orphan, "__Host-albedo-session").is_none(),
        "a callback with no pending flow must not open a session"
    );

    // ── 5 · a callback whose state does not match ──
    // The CSRF case: an attacker who can make the browser issue the callback
    // but cannot read or write the `HttpOnly` cookie.
    let flow_value = flow_cookie.split(';').next().expect("cookie pair");
    let forged = client
        .get(format!(
            "{base}/_albedo/auth/oauth/github/callback?code=x&state=not-the-minted-one"
        ))
        .header("cookie", flow_value)
        .send()
        .await
        .expect("callback responds");
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
    assert!(
        cookie_named(&forged, "__Host-albedo-session").is_none(),
        "a state mismatch must not open a session"
    );
    let cleared = cookie_named(&forged, "__Host-albedo_oauth")
        .expect("a spent flow must be cleared whatever the outcome");
    assert!(
        cleared.contains("Max-Age=0"),
        "leaving the flow live would make the state replayable: {cleared}"
    );

    // ── 6 · a provider this app did not declare ──
    // A 404, not a 501: the route genuinely does not exist for this app.
    let undeclared = client
        .get(format!("{base}/_albedo/auth/oauth/gitlab/start"))
        .send()
        .await
        .expect("responds");
    assert_eq!(undeclared.status(), StatusCode::NOT_FOUND);

    // ── 7 · the method rule ──
    // A redirect cannot be a POST, and the password endpoints' POST-only rule
    // must not have swallowed this path on the way past.
    let posted = client
        .post(format!("{base}/_albedo/auth/oauth/github/start"))
        .send()
        .await
        .expect("responds");
    assert_eq!(posted.status(), StatusCode::METHOD_NOT_ALLOWED);
}
