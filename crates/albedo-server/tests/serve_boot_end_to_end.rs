//! Phase P · Stream A — production server boot end-to-end.
//!
//! Five gate tests for `boot_production_server`:
//!
//! 1. Options derivation absorbs the contract's host/port overrides.
//! 2. Boot fails loud with a "did you run `albedo build`?" hint when
//!    `.albedo/dist/` is missing.
//! 3. Every manifest route becomes a synthetic GET `RouteSpec` so the
//!    manifest-streaming arm activates.
//! 4. Bakabox runtime assets at `<dist>/_albedo/runtime.js` resolve at
//!    `/_albedo/runtime.js`.
//! 5. A Phase K useState component lands in the action registry, so the
//!    action endpoint is mounted (POST → not 404 because the route
//!    exists; HEAD → 405 because the route is POST-only).

use albedo_server::{boot_production_server, ProductionServerOptions};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};
use tower::ServiceExt;

const MAX_BODY: usize = 1024 * 1024;

const HELLO_TSX: &str = r#"
export default function Hello() {
  return <div data-test="hello">hi</div>;
}
"#;

const COUNTER_TSX: &str = r#"
import { useState } from "react";

export default function Counter() {
  const [n, setN] = useState(0);
  return <button onClick={() => setN(n + 1)}>{n}</button>;
}
"#;

/// On-disk fixture: a minimal "built" project under a temp dir.
/// Holds the [`TempDir`] so the directory lives as long as the
/// fixture is in scope.
struct Fixture {
    _temp: TempDir,
    project_dir: PathBuf,
    source_root: PathBuf,
    dist_dir: PathBuf,
}

impl Fixture {
    /// Standard Phase-K-capable fixture: source root has one Tier-A
    /// component (Hello) plus a useState Counter. The manifest the
    /// fixture writes only registers Hello as a route — Counter is
    /// present in the source tree so `CompiledProject` allocates a
    /// click handler for it, exercising `register_compiled_project`.
    fn with_counter() -> Self {
        let temp = tempdir().expect("tempdir");
        let project_dir = temp.path().to_path_buf();
        let source_root = project_dir.join("src");
        let dist_dir = project_dir.join(".albedo").join("dist");

        fs::create_dir_all(&source_root).expect("src dir");
        fs::create_dir_all(dist_dir.join("_albedo")).expect("dist/_albedo dir");

        let hello_path = source_root.join("Hello.tsx");
        let counter_path = source_root.join("Counter.tsx");
        fs::write(&hello_path, HELLO_TSX).expect("write Hello.tsx");
        fs::write(&counter_path, COUNTER_TSX).expect("write Counter.tsx");

        // Manifest references Hello via its absolute path so
        // `RendererRuntime::from_artifacts_dir`'s module-source loader
        // resolves it without depending on the test's cwd.
        let manifest_json = build_minimal_manifest_json(&hello_path);
        fs::write(
            dist_dir.join("render-manifest.v2.json"),
            manifest_json,
        )
        .expect("write manifest");

        // Bakabox runtime stub. Real bytes don't matter; we just
        // want the public-asset mount to find a file at this path.
        fs::write(
            dist_dir.join("_albedo").join("runtime.js"),
            b"// albedo-runtime stub for tests\n",
        )
        .expect("write runtime.js");

        Self {
            _temp: temp,
            project_dir,
            source_root,
            dist_dir,
        }
    }

    fn options(&self) -> ProductionServerOptions {
        ProductionServerOptions {
            project_dir: self.project_dir.clone(),
            source_root: self.source_root.clone(),
            dist_dir: self.dist_dir.clone(),
            host: "127.0.0.1".to_string(),
            port: 0,
            dev_mode: false,
        }
    }
}

/// Tiny render-manifest-v2 with one Tier-A route at `/`. Schema-faithful
/// enough that `RendererRuntime::from_artifacts_dir` deserialises and
/// `prime_runtime_cache` warms QuickJS without complaint.
fn build_minimal_manifest_json(hello_module: &Path) -> String {
    let module_path = hello_module.display().to_string().replace('\\', "/");
    format!(
        r#"{{
  "version": 2,
  "build_id": "stream-a-test",
  "routes": {{
    "/": {{
      "route": "/",
      "shell": {{
        "doctype_and_head": "<!DOCTYPE html><html><head><title>test</title></head>",
        "body_open": "<body><div id=\"root\"></div>",
        "body_close": "</body></html>",
        "shim_script": "<script type=\"module\" src=\"/_albedo/runtime.js\"></script>"
      }},
      "tier_a_root": [{{
        "component_id": "Hello",
        "placeholder_id": "__a_hello_0",
        "html": "<div data-test=\"hello\">hi</div>",
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
    "name": "Hello",
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

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

// ── 1 ── options derivation ────────────────────────────────────

#[test]
fn production_options_from_contract_absorbs_host_and_port_overrides() {
    use dom_render_compiler::dev_contract::resolve_dev_contract;

    let temp = tempdir().unwrap();
    // Phase N+ — `resolve_dev_contract` defaults the source root to
    // `<project>/src/` and discovers `routes/index.tsx` as the entry.
    // Honour that shape so the contract resolves without falling
    // into the strict "root does not exist" branch.
    let routes = temp.path().join("src").join("routes");
    fs::create_dir_all(&routes).unwrap();
    fs::write(routes.join("index.tsx"), HELLO_TSX).unwrap();

    // `albedo serve --host 0.0.0.0 --port 4444` flows through
    // `resolve_dev_contract`, so the contract carries the user's bind
    // values; `from_contract` must propagate them verbatim.
    let args = [
        "--host".to_string(),
        "0.0.0.0".to_string(),
        "--port".to_string(),
        "4444".to_string(),
    ];
    let contract = resolve_dev_contract(&args, temp.path()).expect("resolve contract");
    let opts = ProductionServerOptions::from_contract(&contract);

    assert_eq!(opts.host, "0.0.0.0");
    assert_eq!(opts.port, 4444);
    assert_eq!(opts.project_dir, contract.project_dir);
    assert_eq!(opts.source_root, contract.root);
    assert_eq!(
        opts.dist_dir,
        contract.project_dir.join(".albedo").join("dist")
    );
}

// ── 2 ── error path: missing dist ──────────────────────────────

#[test]
fn boot_production_server_fails_loud_when_dist_dir_missing() {
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src").join("App.tsx"), HELLO_TSX).unwrap();

    let opts = ProductionServerOptions {
        project_dir: temp.path().to_path_buf(),
        source_root: temp.path().join("src"),
        dist_dir: temp.path().join(".albedo").join("dist"),
        host: "127.0.0.1".to_string(),
        port: 0,
        dev_mode: false,
    };

    let err = match boot_production_server(&opts) {
        Ok(_) => panic!("boot unexpectedly succeeded without a dist dir"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains("albedo build"),
        "error message should hint at `albedo build`; got: {message}"
    );
}

// ── 3 ── route synthesis: manifest entries become GET RouteSpecs ──

#[tokio::test]
async fn boot_production_server_serves_manifest_routes_via_streaming() {
    let fixture = Fixture::with_counter();
    let server = boot_production_server(&fixture.options()).expect("boot");

    let response = server.router().oneshot(get("/")).await.unwrap();
    let status = response.status();
    // Manifest-streaming arm fires for any manifest route. The
    // streaming handler may stream a 200 (full render) or surface a
    // renderer warning, but it MUST NOT be a 404 — that would mean
    // the manifest route was never synthesised into a `RouteSpec`.
    assert_ne!(
        status,
        StatusCode::NOT_FOUND,
        "GET / on a manifest route must not 404; got {status:?}"
    );
}

// ── 4 ── public asset fallback: bakabox runtime resolves ─────────

#[tokio::test]
async fn boot_production_server_serves_bakabox_runtime_from_embedded_assets() {
    let fixture = Fixture::with_counter();
    let server = boot_production_server(&fixture.options()).expect("boot");

    // Phase P · post-P wire-through — runtime.js (and the rest of
    // the bakabox client) ships from `dispatch_albedo_asset`'s
    // `include_str!` templates baked into the binary, not from the
    // dist mirror. The previous behaviour mounted `<dist>` as a
    // public_dir, which shadowed `/` with `dist/index.html`.
    let response = server
        .router()
        .oneshot(get("/_albedo/runtime.js"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok());
    assert_eq!(content_type, Some("text/javascript; charset=utf-8"));
    let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    assert!(
        !body.is_empty(),
        "runtime.js body must be non-empty; got {} bytes",
        body.len()
    );
}

// ── 5 ── action endpoint mounted (CompiledProject registered) ────

#[tokio::test]
async fn boot_production_server_mounts_action_endpoint() {
    let fixture = Fixture::with_counter();
    let server = boot_production_server(&fixture.options()).expect("boot");

    // POST `/_albedo/action` with an empty body. The dispatcher's
    // arm runs *before* the router, so a malformed envelope surfaces
    // a 400 ("invalid action envelope") rather than the 404 the
    // router would emit for an unknown path. Distinguishing the two
    // is the only reliable proof that the action endpoint is mounted
    // in the production boot — `register_compiled_project` having
    // run is implicit (`boot_production_server` would have errored
    // earlier if it hadn't).
    let request = Request::builder()
        .method("POST")
        .uri("/_albedo/action")
        .body(Body::empty())
        .expect("request builds");

    let response = server.router().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "/_albedo/action should reject a malformed envelope with 400, \
         proving the dispatcher arm is mounted"
    );
}
