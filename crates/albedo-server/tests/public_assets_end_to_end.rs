//! Phase N · public/ static asset serving end-to-end.
//!
//! Boots an AlbedoServer with one `with_public_dir(..)` mount, fires
//! axum oneshot requests for the canonical hit / miss / traversal
//! cases, and asserts the response shape.

use albedo_server::config::{AppConfig, ServerConfig};
use albedo_server::server::AlbedoServerBuilder;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use std::fs;
use tempfile::tempdir;
use tower::ServiceExt;

const MAX_BODY: usize = 1024 * 1024;

fn build_server(public_dir: &std::path::Path) -> albedo_server::server::AlbedoServer {
    let config = AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    AlbedoServerBuilder::new(config)
        .with_public_dir(public_dir)
        .with_dev_mode(false)
        .build()
        .expect("server build")
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

fn head(path: &str) -> Request<Body> {
    Request::builder()
        .method("HEAD")
        .uri(path)
        .body(Body::empty())
        .expect("request builds")
}

#[tokio::test]
async fn public_directory_serves_files_at_url_root() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(&public).unwrap();
    fs::write(public.join("logo.svg"), b"<svg/>").unwrap();

    let server = build_server(&public);
    let response = server.router().oneshot(get("/logo.svg")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("image/svg+xml")
    );
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("public, max-age=3600")
    );
    let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    assert_eq!(body.as_ref(), b"<svg/>");
}

#[tokio::test]
async fn public_directory_serves_nested_paths() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(public.join("images")).unwrap();
    fs::write(public.join("images").join("cover.png"), b"PNGDATA").unwrap();

    let server = build_server(&public);
    let response = server
        .router()
        .oneshot(get("/images/cover.png"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    assert_eq!(body.as_ref(), b"PNGDATA");
}

#[tokio::test]
async fn public_directory_misses_fall_through_to_router_404() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(&public).unwrap();
    let server = build_server(&public);
    let response = server.router().oneshot(get("/missing.txt")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn public_directory_rejects_parent_directory_traversal() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(&public).unwrap();
    fs::write(temp.path().join("secret.txt"), b"NOPE").unwrap();
    let server = build_server(&public);

    // The traversal-shaped URL never resolves to a file under the
    // mount; falls through to the router which yields 404.
    let response = server
        .router()
        .oneshot(get("/../secret.txt"))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn public_directory_head_returns_empty_body_with_headers() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(&public).unwrap();
    fs::write(public.join("hello.txt"), b"hello").unwrap();
    let server = build_server(&public);

    let response = server.router().oneshot(head("/hello.txt")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
    assert!(body.is_empty());
}

#[tokio::test]
async fn dev_mode_picks_no_store_cache_header_by_default() {
    let temp = tempdir().unwrap();
    let public = temp.path().join("public");
    fs::create_dir_all(&public).unwrap();
    fs::write(public.join("a.txt"), b"a").unwrap();

    let config = AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    };
    let server = AlbedoServerBuilder::new(config)
        .with_public_dir(&public)
        .with_dev_mode(true)
        .build()
        .unwrap();

    let response = server.router().oneshot(get("/a.txt")).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
}
