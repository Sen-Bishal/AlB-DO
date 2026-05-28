//! Phase P · Stream C.3 — auto topic registration gate.
//!
//! `register_compiled_project` must walk
//! `CompiledProject::shared_slot_topics()` and call
//! `BroadcastRegistry::topic` for each, so that:
//!
//!   1. Stream C.4's auto-subscribe pass in the streaming handler
//!      finds a live `BroadcastTopic` for every topic the route's
//!      JSX declared via `useSharedSlot`, even before the first
//!      explicit write.
//!   2. A `broadcast(topic, updater)` call inside an action handler
//!      against a topic that exists only in the JSX-side
//!      `useSharedSlot` declaration (no prior `topic()` from
//!      userland) resolves cleanly rather than falling through to
//!      the auto-create branch.
//!
//! Topic count + topic membership both checked so a registration
//! regression that silently loses a topic gets caught.

use albedo_server::config::{AppConfig, ServerConfig};
use albedo_server::server::AlbedoServerBuilder;
use dom_render_compiler::runtime::eval::CompiledProject;
use std::path::PathBuf;
use std::sync::Arc;

fn shared_slot_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("albedo-server crate dir")
        .parent()
        .expect("workspace root")
        .join("tests")
        .join("fixtures")
        .join("shared_slot")
        .join("lobby")
}

fn empty_config() -> AppConfig {
    AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    }
}

#[test]
fn register_compiled_project_auto_creates_every_shared_slot_topic() {
    let project = Arc::new(
        CompiledProject::load_from_dir(shared_slot_fixture()).expect("project compiles"),
    );
    // The `lobby` fixture declares one `useSharedSlot("chat:lobby")`.
    let expected: Vec<String> = project.shared_slot_topics();
    assert_eq!(
        expected,
        vec!["chat:lobby".to_string()],
        "fixture sanity: lobby/Component.tsx declares exactly one topic"
    );

    let builder = AlbedoServerBuilder::new(empty_config()).register_compiled_project(project);
    let broadcast = builder.broadcast();

    assert!(
        broadcast.get("chat:lobby").is_some(),
        "register_compiled_project must auto-create every useSharedSlot topic; \
         'chat:lobby' missing from the registry"
    );
    assert_eq!(broadcast.topic_count(), 1);
}

#[test]
fn auto_topic_registration_is_idempotent_across_repeated_register_calls() {
    let project = Arc::new(
        CompiledProject::load_from_dir(shared_slot_fixture()).expect("project compiles"),
    );
    // Two registrations of the same project — topic_count should
    // stay at 1, not grow to 2. `BroadcastRegistry::topic` is
    // idempotent on the topic-name key, but this test pins the
    // contract at the builder boundary so a future change can't
    // silently break it.
    let builder = AlbedoServerBuilder::new(empty_config())
        .register_compiled_project(project.clone())
        .register_compiled_project(project);
    assert_eq!(builder.broadcast().topic_count(), 1);
}

#[test]
fn auto_topic_registration_preserves_existing_topic_value() {
    let project = Arc::new(
        CompiledProject::load_from_dir(shared_slot_fixture()).expect("project compiles"),
    );
    let builder = AlbedoServerBuilder::new(empty_config());
    // Seed the topic with a meaningful value before registering the
    // project. The auto-registration pass must NOT overwrite this
    // value — `BroadcastRegistry::topic` returns the existing entry
    // on a second call with the same name, ignoring the `initial`
    // argument.
    builder
        .broadcast()
        .topic("chat:lobby", serde_json::to_vec(&["seeded"]).unwrap());
    let builder = builder.register_compiled_project(project);
    let topic = builder.broadcast().get("chat:lobby").unwrap();
    let value: serde_json::Value = serde_json::from_slice(&topic.current_value()).unwrap();
    assert_eq!(
        value,
        serde_json::json!(["seeded"]),
        "auto topic registration must NOT clobber a pre-seeded value; \
         BroadcastRegistry::topic is idempotent on the name key"
    );
}
