//! FORGE Gate 1 · the end-to-end read loop: substrate → hydrate → render → HTML.
//!
//! Boots a real libSQL substrate, bootstraps + seeds the guestbook table,
//! hydrates the `guestbook` broadcast topic through the FORGE skeleton, then
//! renders a component that reads it via `useSharedSlot` and asserts the
//! persisted rows land in the SSR HTML. That is the whole Gate-1 claim: the
//! data is on the page with no per-request I/O and no query authored by hand
//! in the component — the topic value was materialised from storage at boot.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::forge::{skeleton, LibSqlSubstrate};
use dom_render_compiler::runtime::eval::{CompiledProject, RenderOptions, SessionSlotView};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::{render_entry_with_broadcast, BroadcastRegistry, SessionId};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("forge_guestbook")
}

#[tokio::test]
async fn seeded_guestbook_rows_render_into_ssr_html() {
    // 1. A real substrate, bootstrapped + seeded, hydrated into the registry —
    //    exactly what `AlbedoServer::run` does at boot before serving.
    let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
    skeleton::bootstrap_schema(&db).await.unwrap();

    let broadcast = BroadcastRegistry::new();
    skeleton::hydrate_topics(&db, &broadcast).await.unwrap();

    // 2. Render the guestbook component against that registry.
    let project = CompiledProject::load_from_dir(fixture()).expect("fixture compiles");
    let store = Arc::new(SlotStore::new());
    let slots = SessionSlotView::new(SessionId::random(), store);
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(16);
    let opts = RenderOptions { hook_compile: true };

    let out = render_entry_with_broadcast(
        &project,
        "Component.tsx",
        &Value::Object(Default::default()),
        &slots,
        &broadcast,
        tx,
        &opts,
    )
    .expect("render succeeds");

    println!("--- SSR HTML ---\n{}\n----------------", out.html);

    // 3. The persisted rows are on the page — no write, no query in the
    //    component, no per-request DB round trip.
    assert!(
        out.html.contains("first light"),
        "seed row 1 message missing from SSR HTML; got: {}",
        out.html
    );
    assert!(
        out.html.contains("the machine stirs"),
        "seed row 2 message missing from SSR HTML; got: {}",
        out.html
    );
    assert!(
        out.html.contains("ada"),
        "seed row author missing from SSR HTML; got: {}",
        out.html
    );
}
