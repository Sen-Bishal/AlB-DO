//! Phase O.2 · end-to-end broadcast through the server-facing API.
//!
//! Exercises the public surface a real route handler / chat-demo
//! would touch: build a server, reach the broadcast registry through
//! `AlbedoServer::broadcast()`, register a topic, subscribe two
//! "sessions" with their own mpsc receivers, write, and assert each
//! receiver decoded the `SlotSet` opcode the broadcast registry
//! emitted.

use albedo_server::config::{AppConfig, ServerConfig};
use albedo_server::server::AlbedoServerBuilder;
use albedo_server::BroadcastRegistry;
use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::{broadcast_slot_id, SessionId};
use std::sync::Arc;
use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 16;

fn make_server() -> albedo_server::server::AlbedoServer {
    AlbedoServerBuilder::new(AppConfig {
        server: ServerConfig::default(),
        renderer: None,
        layouts: Vec::new(),
        routes: Vec::new(),
    })
    .build()
    .expect("server build")
}

fn decode_one_slot_set(payload: &[u8]) -> (u32, Vec<u8>) {
    let (frame, _) = decode_frame(payload).expect("decode frame");
    assert_eq!(frame.instructions.len(), 1);
    match &frame.instructions[0] {
        Instruction::SlotSet { slot_id, value } => (slot_id.0, value.clone()),
        other => panic!("expected SlotSet, got {other:?}"),
    }
}

#[tokio::test]
async fn two_sessions_subscribed_to_same_topic_both_receive_writes() {
    let server = make_server();
    let broadcast: Arc<BroadcastRegistry> = server.broadcast();

    let topic = broadcast.topic("chat:room-1", b"[]".to_vec());
    assert_eq!(topic.slot_id(), broadcast_slot_id("chat:room-1"));

    let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let (tx_b, mut rx_b) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let session_a = SessionId::random();
    let session_b = SessionId::random();
    let seed_a = broadcast.subscribe(session_a, "chat:room-1", tx_a).unwrap();
    let seed_b = broadcast.subscribe(session_b, "chat:room-1", tx_b).unwrap();
    assert_eq!(seed_a, b"[]".to_vec());
    assert_eq!(seed_b, b"[]".to_vec());

    let report = broadcast
        .write_topic("chat:room-1", br#"[{"from":"alice","text":"hi"}]"#.to_vec())
        .unwrap();
    assert_eq!(report.delivered, 2);
    assert!(report.dropped_full.is_empty());
    assert!(report.dropped_closed.is_empty());

    let payload_a = rx_a.recv().await.expect("session A receives a frame");
    let payload_b = rx_b.recv().await.expect("session B receives a frame");
    let (slot_a, value_a) = decode_one_slot_set(&payload_a);
    let (slot_b, value_b) = decode_one_slot_set(&payload_b);
    assert_eq!(slot_a, broadcast_slot_id("chat:room-1").0);
    assert_eq!(slot_a, slot_b);
    assert_eq!(value_a, br#"[{"from":"alice","text":"hi"}]"#.to_vec());
    assert_eq!(value_a, value_b);
}

#[tokio::test]
async fn late_joiner_gets_current_value_via_subscribe_return() {
    let server = make_server();
    let broadcast = server.broadcast();
    broadcast.topic("cursor:doc-7", b"{\"x\":0,\"y\":0}".to_vec());
    broadcast
        .write_topic("cursor:doc-7", b"{\"x\":42,\"y\":7}".to_vec())
        .unwrap_or_else(|err| panic!("write should succeed without subscribers: {err:?}"));

    let (tx, _rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let value = broadcast
        .subscribe(SessionId::random(), "cursor:doc-7", tx)
        .unwrap();
    assert_eq!(value, b"{\"x\":42,\"y\":7}".to_vec());
}

#[tokio::test]
async fn cleanup_session_removes_subscription_so_subsequent_writes_skip_it() {
    let server = make_server();
    let broadcast = server.broadcast();
    broadcast.topic("alerts", b"[]".to_vec());

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let session = SessionId::random();
    broadcast.subscribe(session, "alerts", tx).unwrap();

    let report = broadcast
        .write_topic("alerts", b"[\"first\"]".to_vec())
        .unwrap();
    assert_eq!(report.delivered, 1);
    let _ = rx.recv().await.expect("first write delivers");

    broadcast.cleanup_session(session);

    let report = broadcast
        .write_topic("alerts", b"[\"second\"]".to_vec())
        .unwrap();
    assert_eq!(report.delivered, 0);
    assert!(report.dropped_closed.is_empty(), "cleanup is eager; close path shouldn't observe a closed channel");
}

#[tokio::test]
async fn multiple_topics_isolated_per_session() {
    let server = make_server();
    let broadcast = server.broadcast();
    broadcast.topic("topic-a", b"".to_vec());
    broadcast.topic("topic-b", b"".to_vec());

    let (tx_a, mut rx_a) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let (tx_b, mut rx_b) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    let session = SessionId::random();
    broadcast.subscribe(session, "topic-a", tx_a).unwrap();
    broadcast.subscribe(session, "topic-b", tx_b).unwrap();

    broadcast.write_topic("topic-a", b"A".to_vec()).unwrap();

    let (slot_a, val_a) = decode_one_slot_set(&rx_a.recv().await.unwrap());
    assert_eq!(slot_a, broadcast_slot_id("topic-a").0);
    assert_eq!(val_a, b"A".to_vec());
    // The other topic's receiver must still be empty — writes are
    // strictly topic-scoped even when both share a session id.
    assert!(rx_b.try_recv().is_err());
}

#[tokio::test]
async fn broadcast_registry_is_shared_across_clones_of_the_arc() {
    // Confirms `AlbedoServer::broadcast()` returns a clone of the
    // same Arc — a write through one clone must be visible to a
    // subscriber registered through another. Without this guarantee,
    // a route handler that grabs the broadcast handle once at start
    // would silently miss writes performed later from another path.
    let server = make_server();
    let broadcast_a = server.broadcast();
    let broadcast_b = server.broadcast();
    assert!(Arc::ptr_eq(&broadcast_a, &broadcast_b));

    broadcast_a.topic("shared", b"".to_vec());
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(CHANNEL_CAPACITY);
    broadcast_b
        .subscribe(SessionId::random(), "shared", tx)
        .unwrap();

    let report = broadcast_a.write_topic("shared", b"x".to_vec()).unwrap();
    assert_eq!(report.delivered, 1);
    let _ = rx.recv().await.unwrap();
}
