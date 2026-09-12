//! UPLOADS · 15.1 — the serve route over real HTTP.
//!
//! ## What this covers that the unit tests cannot
//!
//! `albedo_server::uploads`' own tests drive `decode_multipart` over real wire
//! bytes and cover every bound, every refusal and the content-addressed layout.
//! What they cannot see is whether any of it is **reachable** — whether the
//! `uploads` block survives config → boot → registry, whether a route exists at
//! `/_albedo/uploads/{id}`, and whether the substrate the serve path reads is
//! the one boot opened.
//!
//! That gap is this codebase's most expensive recurring bug: `auth_password_http.rs`
//! exists because every endpoint was correct and no browser could reach the
//! form, and APERTURE's egress check was correct while nothing routed to it. So
//! this test binds a listener and asks for bytes.
//!
//! ## Why it stores through the API rather than by POSTing a form
//!
//! A multipart submit needs a compiled action to submit *to*, which means a
//! manifest fixture with an action registry — a large amount of scaffolding for
//! a path `decode_multipart`'s tests already cover byte for byte. Storing via
//! the store API and asking for it over HTTP isolates the half that is
//! genuinely untested: the route, the headers, and the guards.

use albedo_server::{boot_production_server, ProductionServerOptions};
use dom_render_compiler::upload::store::{self, StoredUpload};
use dom_render_compiler::upload::UploadDecl;
use reqwest::StatusCode;
use std::collections::BTreeMap;
use std::fs;

fn manifest_json() -> String {
    r#"{
  "version": 2,
  "build_id": "uploads-test",
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
      "tier_b": [], "tier_c": [], "shared_slot_topics": [], "action_ids": [],
      "layout_chain": [], "error_component": null, "loading_component": null
    }
  },
  "assets": { "chunks": {}, "css": [], "runtime": "/_albedo/runtime.js" },
  "schema_version": "2.0",
  "generated_at": "",
  "components": [{
    "id": 0, "name": "Home", "module_path": "src/Home.tsx", "tier": "A",
    "weight_bytes": 100, "priority": 1.0, "dependencies": [],
    "can_defer": true, "hydration_mode": "none"
  }],
  "parallel_batches": [], "critical_path": [], "vendor_chunks": [], "wt_streams": []
}"#
    .to_string()
}

#[tokio::test]
async fn stored_bytes_are_reachable_and_the_guards_hold() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project_dir = temp.path().to_path_buf();
    let source_root = project_dir.join("src");
    let dist_dir = project_dir.join(".albedo").join("dist");
    fs::create_dir_all(&source_root).expect("src dir");
    fs::create_dir_all(dist_dir.join("_albedo")).expect("dist dir");
    fs::write(source_root.join("Home.tsx"), "export default function Home(){}").expect("write");
    fs::write(dist_dir.join("render-manifest.v2.json"), manifest_json()).expect("manifest");
    fs::write(dist_dir.join("_albedo").join("runtime.js"), b"// stub\n").expect("runtime");

    std::env::set_current_dir(&project_dir).expect("cd into the fixture project");

    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("addr").port()
    };

    let mut uploads: BTreeMap<String, UploadDecl> = BTreeMap::new();
    uploads.insert(
        "avatars".to_string(),
        UploadDecl {
            accept: Some(vec!["image/png".to_string()]),
            max_bytes: Some("1mb".to_string()),
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
        uploads,
        auth: Default::default(),
        tls: Default::default(),
    };

    let server = boot_production_server(&opts).expect("boots with an uploads block");
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

    // Store one file the way the request path does: bytes at the content
    // address, a row pointing at them.
    let bytes = b"\x89PNG\r\n\x1a\nnot-really-a-png-but-the-type-is-what-we-serve";
    let id = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(bytes));
    let path = store::path_for(&project_dir, &id).expect("a real id");
    fs::create_dir_all(path.parent().expect("parent")).expect("fan-out dir");
    fs::write(&path, bytes).expect("write the bytes");

    // A second handle onto the same `forge.db` the server opened. The table is
    // already there because boot ran its DDL — which is itself the check that
    // the `uploads` block reached the schema, since an app that declared no
    // bucket contributes no table and this insert would fail.
    let substrate = dom_render_compiler::forge::LibSqlSubstrate::open_local(
        project_dir.join("forge.db"),
    )
    .await
    .expect("the database boot opened");
    store::record(
        &substrate,
        &StoredUpload {
            id: id.clone(),
            bucket: "avatars".to_string(),
            content_type: "image/png".to_string(),
            byte_size: bytes.len() as u64,
            original_name: "pixel.png".to_string(),
            principal: None,
        },
        0,
    )
    .await
    .expect("records");

    let client = reqwest::Client::new();

    // ── 1 · the route exists and returns the exact bytes ──
    let response = client
        .get(format!("{base}/_albedo/uploads/{id}"))
        .send()
        .await
        .expect("responds");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the serve route must be reachable — this is the 'nothing routed to it' check"
    );
    let headers = response.headers().clone();
    assert_eq!(
        response.bytes().await.expect("body").as_ref(),
        bytes.as_slice(),
        "the bytes served must be the bytes stored"
    );

    // ── 2 · the headers that make this safe ──
    assert_eq!(
        headers.get("content-type").and_then(|v| v.to_str().ok()),
        Some("image/png"),
        "the type comes from the row, never from sniffing the bytes"
    );
    assert_eq!(
        headers
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff"),
        "a part's declared type is a claim; nosniff is what stops a lying file \
         from being served as whatever a sniffer decides it looks like"
    );
    assert!(
        headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| value.contains("immutable")),
        "the id is the hash, so immutable is honest: {headers:?}"
    );

    // ── 3 · the guards ──
    // Every one of these would be a filesystem read if the id alphabet were a
    // blocklist instead of a whitelist.
    for hostile in [
        "..%2f..%2fetc%2fpasswd",
        "..",
        "abc",
        &"A".repeat(64),
        &"g".repeat(64),
        &format!("{id}/extra"),
    ] {
        let status = client
            .get(format!("{base}/_albedo/uploads/{hostile}"))
            .send()
            .await
            .expect("responds")
            .status();
        assert!(
            status == StatusCode::NOT_FOUND,
            "`{hostile}` answered {status}, which is not a refusal"
        );
    }

    // A well-formed id nobody stored is a 404 too, and must not distinguish
    // itself from a malformed one in any way a prober could use.
    let unknown = client
        .get(format!("{base}/_albedo/uploads/{}", "a".repeat(64)))
        .send()
        .await
        .expect("responds");
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // ── 4 · uploads are read-only over HTTP ──
    let posted = client
        .post(format!("{base}/_albedo/uploads/{id}"))
        .send()
        .await
        .expect("responds");
    assert_eq!(posted.status(), StatusCode::METHOD_NOT_ALLOWED);
}
