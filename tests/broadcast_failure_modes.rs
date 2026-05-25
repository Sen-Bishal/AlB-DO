//! Phase O.2 Week 3 · failure-mode tests for the broadcast registry.
//!
//! Stresses the substrate against the conditions a production
//! deployment will hit: concurrent subscribe + write, session-drop
//! races, large payloads, fan-out to many subscribers, write storms,
//! and bounded-channel backpressure. None of these involve new
//! functionality — they verify the existing invariants under load
//! and concurrency, which is what makes the registry production-
//! ready as the Phase H slot store became after its own soak tests.

use dom_render_compiler::ir::opcode::Instruction;
use dom_render_compiler::ir::wire::decode_frame;
use dom_render_compiler::runtime::{broadcast_slot_id, BroadcastRegistry, SessionId};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const TOPIC: &str = "stress:topic";

fn slot_set_value(payload: &[u8]) -> Vec<u8> {
    let (frame, _) = decode_frame(payload).expect("decode");
    match frame.instructions.into_iter().next().expect("non-empty") {
        Instruction::SlotSet { value, .. } => value,
        other => panic!("expected SlotSet, got {other:?}"),
    }
}

#[tokio::test]
async fn concurrent_subscribers_all_receive_a_single_write() {
    // Spawn N subscribe tasks racing with each other to register
    // against the same topic; once they're all in, issue one write
    // and assert every subscriber observes it. Catches subscriber-
    // map races where a subscribe completed AFTER iteration began.
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"seed".to_vec());

    const SUBSCRIBERS: usize = 64;
    let mut receivers = Vec::with_capacity(SUBSCRIBERS);
    let mut subscribe_set = JoinSet::new();
    for _ in 0..SUBSCRIBERS {
        let registry = registry.clone();
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        receivers.push(rx);
        subscribe_set.spawn(async move {
            let session = SessionId::random();
            registry.subscribe(session, TOPIC, tx).expect("subscribe");
        });
    }
    while let Some(result) = subscribe_set.join_next().await {
        result.expect("subscribe task did not panic");
    }

    let report = registry
        .write_topic(TOPIC, b"broadcast".to_vec())
        .expect("write");
    assert_eq!(report.delivered, SUBSCRIBERS);
    assert!(report.dropped_full.is_empty());
    assert!(report.dropped_closed.is_empty());

    for mut rx in receivers {
        let payload = rx.recv().await.expect("each subscriber receives");
        assert_eq!(slot_set_value(&payload), b"broadcast".to_vec());
    }
}

#[tokio::test]
async fn concurrent_cleanup_session_and_write_topic_do_not_deadlock() {
    // Race cleanup against an in-flight write. The registry uses
    // DashMap so neither side blocks the other; this test exists to
    // catch a future change that re-introduces a coarse lock.
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"".to_vec());

    let mut sessions = Vec::new();
    let mut receivers = Vec::new();
    for _ in 0..32 {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
        let session = SessionId::random();
        registry.subscribe(session, TOPIC, tx).unwrap();
        sessions.push(session);
        receivers.push(rx);
    }

    let cleanup_registry = registry.clone();
    let cleanup_sessions = sessions.clone();
    let cleanup_task = tokio::spawn(async move {
        for session in cleanup_sessions {
            cleanup_registry.cleanup_session(session);
            tokio::task::yield_now().await;
        }
    });

    let writer_registry = registry.clone();
    let writer_task = tokio::spawn(async move {
        for i in 0..32_u32 {
            let _ = writer_registry.write_topic(TOPIC, i.to_be_bytes().to_vec());
            tokio::task::yield_now().await;
        }
    });

    // Both must complete; absence of timeout is the assertion.
    tokio::time::timeout(Duration::from_secs(5), async {
        cleanup_task.await.expect("cleanup ran");
        writer_task.await.expect("writer ran");
    })
    .await
    .expect("no deadlock within 5s");

    // Final state: every session was cleaned up, so a follow-up
    // write delivers to zero subscribers.
    let final_report = registry
        .write_topic(TOPIC, b"final".to_vec())
        .expect("write");
    assert_eq!(final_report.delivered, 0);
}

#[tokio::test]
async fn slow_consumer_dropped_does_not_block_thousand_fast_writes() {
    // One subscriber with a tiny channel, one with a roomy channel.
    // 1000 writes; the slow consumer is pruned on the first overflow
    // and the fast consumer must observe every subsequent write.
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"".to_vec());

    let (slow_tx, _slow_rx) = mpsc::channel::<Vec<u8>>(1);
    let (fast_tx, mut fast_rx) = mpsc::channel::<Vec<u8>>(2048);
    let slow_session = SessionId::random();
    let fast_session = SessionId::random();
    registry.subscribe(slow_session, TOPIC, slow_tx.clone()).unwrap();
    registry.subscribe(fast_session, TOPIC, fast_tx).unwrap();
    slow_tx.try_send(b"prefill".to_vec()).unwrap(); // saturate slow channel

    let mut total_drop_full = 0usize;
    for i in 0..1000_u32 {
        let report = registry.write_topic(TOPIC, i.to_be_bytes().to_vec()).unwrap();
        total_drop_full += report.dropped_full.len();
    }

    assert_eq!(total_drop_full, 1, "slow session should be pruned on its first dropped write");

    let mut fast_received = 0;
    while fast_rx.try_recv().is_ok() {
        fast_received += 1;
    }
    assert_eq!(fast_received, 1000, "fast consumer must observe every write");

    assert_eq!(registry.get(TOPIC).unwrap().subscriber_count(), 1);
}

#[tokio::test]
async fn large_payload_broadcasts_round_trip_correctly() {
    // 256 KB payload — beyond the typical chat message but well
    // within reasonable broadcast sizes (think real-time cursor
    // arrays for a 100-user collaborative doc). Asserts the bincode
    // wire path handles big buffers without truncation.
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"".to_vec());
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(4);
    registry.subscribe(SessionId::random(), TOPIC, tx).unwrap();

    let big_value: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
    let report = registry.write_topic(TOPIC, big_value.clone()).unwrap();
    assert_eq!(report.delivered, 1);

    let payload = rx.recv().await.expect("delivered");
    let (frame, _) = decode_frame(&payload).unwrap();
    match frame.instructions.into_iter().next().unwrap() {
        Instruction::SlotSet { slot_id, value } => {
            assert_eq!(slot_id, broadcast_slot_id(TOPIC));
            assert_eq!(value, big_value, "large payload must round-trip without truncation");
        }
        other => panic!("expected SlotSet, got {other:?}"),
    }
}

#[tokio::test]
async fn write_storm_to_one_thousand_subscribers_delivers_every_event() {
    // 1000 sessions × 10 writes. Each session must receive 10
    // SlotSets in order. This is the "broadcast under load" smoke
    // test the sprint plan calls out — at this scale a deadlock or
    // a missing wake-up surfaces quickly.
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"".to_vec());

    const SESSIONS: usize = 1000;
    const WRITES: usize = 10;
    let mut receivers = Vec::with_capacity(SESSIONS);
    for _ in 0..SESSIONS {
        let (tx, rx) = mpsc::channel::<Vec<u8>>(WRITES + 4);
        registry.subscribe(SessionId::random(), TOPIC, tx).unwrap();
        receivers.push(rx);
    }

    for i in 0..WRITES as u32 {
        let report = registry.write_topic(TOPIC, i.to_be_bytes().to_vec()).unwrap();
        assert_eq!(report.delivered, SESSIONS, "write #{i} dropped sessions");
    }

    let mut join_set = JoinSet::new();
    for (idx, mut rx) in receivers.into_iter().enumerate() {
        join_set.spawn(async move {
            let mut values: Vec<u32> = Vec::with_capacity(WRITES);
            for _ in 0..WRITES {
                let payload = rx
                    .recv()
                    .await
                    .unwrap_or_else(|| panic!("session {idx} missed a write"));
                let value = slot_set_value(&payload);
                let mut buf = [0u8; 4];
                buf.copy_from_slice(&value);
                values.push(u32::from_be_bytes(buf));
            }
            assert_eq!(values, (0..WRITES as u32).collect::<Vec<_>>());
        });
    }
    while let Some(result) = join_set.join_next().await {
        result.expect("session task panicked");
    }
}

#[tokio::test]
async fn subscriber_dropped_mid_subscribe_does_not_corrupt_reverse_index() {
    // Subscribe → drop receiver → write (closed channel observed,
    // session pruned) → cleanup_session (should be a clean no-op).
    let registry = Arc::new(BroadcastRegistry::new());
    registry.topic(TOPIC, b"".to_vec());

    let session = SessionId::random();
    let (tx, rx) = mpsc::channel::<Vec<u8>>(4);
    registry.subscribe(session, TOPIC, tx).unwrap();
    drop(rx); // simulates client disconnect before any write

    let report = registry
        .write_topic(TOPIC, b"trigger-cleanup".to_vec())
        .unwrap();
    assert_eq!(report.dropped_closed, vec![session]);

    // Cleanup after lazy-pruning must not panic and must not surface
    // any stale topic references.
    registry.cleanup_session(session);
    assert_eq!(registry.get(TOPIC).unwrap().subscriber_count(), 0);
}

#[tokio::test]
async fn auto_subscribe_then_cleanup_session_releases_every_topic_subscription() {
    // The renderer auto-subscribes once per topic. When the WT
    // session drops, every topic this session subscribed to must
    // release the entry — otherwise the subscriber map grows
    // monotonically across reconnects.
    let registry = Arc::new(BroadcastRegistry::new());
    let session = SessionId::random();
    let (tx, _rx) = mpsc::channel::<Vec<u8>>(4);
    let topics: Vec<String> = (0..16).map(|i| format!("topic-{i}")).collect();
    let _ = registry.auto_subscribe(session, tx, &topics);
    for topic in &topics {
        assert_eq!(registry.get(topic).unwrap().subscriber_count(), 1);
    }

    registry.cleanup_session(session);
    for topic in &topics {
        assert_eq!(registry.get(topic).unwrap().subscriber_count(), 0);
    }
}
