//! JOBS · 15.5 — the queue protocol, against a real database.
//!
//! ## Why none of this is a unit test
//!
//! Every property here lives **in the SQL**, not in Rust. The claim is a
//! `SELECT` and a guarded `UPDATE` inside a transaction; exactly-once firing is
//! a primary key and an `INSERT OR IGNORE`; lease reclamation is a `WHERE`
//! clause comparing two integers. `RecordingSubstrate` does not interpret SQL —
//! it records it — so a test of this protocol written against that double would
//! assert that the strings were sent and prove nothing at all about whether they
//! do the right thing.
//!
//! So these run on `LibSqlSubstrate::open_ephemeral`, which is file-backed
//! SQLite: the thing that actually ships.
//!
//! ## The rule this file inherits
//!
//! From `forge_reserve_concurrency`: **refuse an error where a verdict is
//! owed.** A loser in a race must come back `Ok(None)` — "somebody else has
//! it" — and never a substrate error. An earlier version of that file passed
//! while lying, because its losers were failing before they reached the check
//! being tested. A claim that errors under contention is a broken claim, even
//! if the exactly-once property technically survives it.

#![cfg(feature = "forge")]

use std::collections::HashSet;
use std::sync::Arc;

use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};
use dom_render_compiler::jobs::queue::{
    self, ClaimedJob, Enqueue, JobState, DEFAULT_LEASE_MS, JOBS,
};

/// 2026-09-17T00:00:00Z.
const NOW: i64 = 1_789_603_200_000;

async fn substrate() -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_ephemeral()
        .await
        .expect("an ephemeral database opens");
    for statement in queue::ddl() {
        db.migrate(&statement).await.expect("the jobs DDL applies");
    }
    db
}

fn work(id: &str, name: &str, run_at_ms: i64, max_attempts: u32) -> Enqueue {
    Enqueue {
        id: id.to_string(),
        name: name.to_string(),
        args: "{}".to_string(),
        principal: None,
        run_at_ms,
        max_attempts,
        fanout: false,
    }
}

async fn state_of(db: &dyn DataSubstrate, id: &str) -> String {
    let rows = db
        .query(
            &format!("SELECT state FROM {JOBS} WHERE id = ?"),
            &[dom_render_compiler::forge::SqlValue::Text(id.to_string())],
        )
        .await
        .expect("the row reads back");
    rows.rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(dom_render_compiler::forge::SqlValue::as_str)
        .expect("a state")
        .to_string()
}

async fn count(db: &dyn DataSubstrate) -> i64 {
    let rows = db
        .query(&format!("SELECT COUNT(*) FROM {JOBS}"), &[])
        .await
        .expect("counts");
    rows.rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(dom_render_compiler::forge::SqlValue::as_i64)
        .expect("a count")
}

/// The exactly-once claim, at the level it is actually made: the same derived
/// fire id inserted many times is one row.
#[tokio::test]
async fn one_tick_is_one_row_however_many_processes_compute_it() {
    let db = substrate().await;
    let fire = queue::fire_id("digest", NOW);

    let mut accepted = 0;
    for _ in 0..8 {
        if queue::enqueue(&db, &work(&fire, "digest", NOW, 1))
            .await
            .expect("an enqueue never errors on a duplicate — it declines")
        {
            accepted += 1;
        }
    }

    assert_eq!(accepted, 1, "exactly one insert may win the tick");
    assert_eq!(count(&db).await, 1, "and exactly one row may exist");
}

/// The same property under real concurrency rather than in sequence — this is
/// the shape two `albedo serve` processes actually produce.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_processes_racing_one_tick_produce_one_row() {
    let db = Arc::new(substrate().await);
    let fire = queue::fire_id("digest", NOW);

    let mut handles = Vec::new();
    for _ in 0..16 {
        let db = Arc::clone(&db);
        let fire = fire.clone();
        handles.push(tokio::spawn(async move {
            queue::enqueue(db.as_ref(), &work(&fire, "digest", NOW, 1)).await
        }));
    }

    let mut accepted = 0;
    for handle in handles {
        // Refuse an error where a verdict is owed: a loser must be told
        // "already there", not handed a substrate failure.
        let won = handle
            .await
            .expect("the task completes")
            .expect("a racing enqueue returns a verdict, never an error");
        if won {
            accepted += 1;
        }
    }

    assert_eq!(accepted, 1, "exactly one racer may win");
    assert_eq!(count(db.as_ref()).await, 1);
}

#[tokio::test]
async fn a_due_row_is_claimed_and_a_future_one_is_not() {
    let db = substrate().await;
    queue::enqueue(&db, &work("due", "a", NOW, 3)).await.unwrap();
    queue::enqueue(&db, &work("later", "b", NOW + 60_000, 3))
        .await
        .unwrap();

    let claimed = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .expect("claims")
        .expect("something was due");
    assert_eq!(claimed.id, "due");
    assert_eq!(claimed.attempts, 1, "the claim counts as the first attempt");
    assert_eq!(state_of(&db, "due").await, JobState::Claimed.as_str());

    assert!(
        queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
            .await
            .expect("claims")
            .is_none(),
        "a row that is not yet due must not be handed out early"
    );
    assert_eq!(state_of(&db, "later").await, JobState::Pending.as_str());
}

/// Two runners, one row. This is the property that makes a claim a claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claims_of_one_row_produce_one_holder() {
    let db = Arc::new(substrate().await);
    queue::enqueue(db.as_ref(), &work("only", "a", NOW, 5))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..16 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            queue::claim_due(db.as_ref(), NOW, DEFAULT_LEASE_MS).await
        }));
    }

    let mut winners: Vec<ClaimedJob> = Vec::new();
    for handle in handles {
        let outcome = handle
            .await
            .expect("the task completes")
            .expect("a losing claimant is told None, never handed an error");
        if let Some(job) = outcome {
            winners.push(job);
        }
    }

    assert_eq!(winners.len(), 1, "exactly one runner may hold the row");
    assert_eq!(winners[0].id, "only");
}

/// Sixteen runners, sixteen rows, once each — the fan-out's real shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claims_never_hand_one_row_to_two_runners() {
    let db = Arc::new(substrate().await);
    for index in 0..16 {
        queue::enqueue(db.as_ref(), &work(&format!("row-{index}"), "a", NOW, 5))
            .await
            .unwrap();
    }

    let mut handles = Vec::new();
    for _ in 0..16 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            queue::claim_due(db.as_ref(), NOW, DEFAULT_LEASE_MS).await
        }));
    }

    let mut ids = Vec::new();
    for handle in handles {
        let outcome = handle
            .await
            .expect("the task completes")
            .expect("a claim returns a verdict, never an error");
        if let Some(job) = outcome {
            ids.push(job.id);
        }
    }

    let unique: HashSet<&String> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "the same row was handed to two runners: {ids:?}"
    );
}

/// The crash path. Nothing notices the process died; the lease simply expires.
#[tokio::test]
async fn an_expired_lease_returns_the_work_and_counts_the_attempt() {
    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 5)).await.unwrap();

    let first = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed");
    assert_eq!(first.attempts, 1);

    // The holder dies here — no `complete`, no `fail`, no cleanup of any kind.
    assert!(
        queue::claim_due(&db, NOW + 1000, DEFAULT_LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "while the lease is good the row stays with its holder"
    );

    let reclaimed = queue::claim_due(&db, NOW + DEFAULT_LEASE_MS + 1, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("an expired lease returns the work");
    assert_eq!(reclaimed.id, "job");
    // 🪤 The counted reclaim is what stops a job that kills its own process
    // from being immortal.
    assert_eq!(
        reclaimed.attempts, 2,
        "a reclaim must count, or a poison row loops forever"
    );
}

/// The poison row: it kills the process every time, and it still dead-letters.
#[tokio::test]
async fn a_row_that_always_kills_its_holder_eventually_dead_letters() {
    let db = substrate().await;
    queue::enqueue(&db, &work("poison", "a", NOW, 3)).await.unwrap();

    let mut now = NOW;
    let mut claims = 0;
    // Simulate: claim, die, lease expires, claim again — never completing,
    // never failing cleanly. The only thing that advances is the clock.
    while let Some(job) = queue::claim_due(&db, now, DEFAULT_LEASE_MS).await.unwrap() {
        claims += 1;
        assert!(claims <= 10, "it must stop being reclaimed, not loop forever");
        if job.attempts >= job.max_attempts {
            queue::fail(&db, &job, now, "the holder died", 1.0)
                .await
                .unwrap();
            break;
        }
        now += DEFAULT_LEASE_MS + 1;
    }

    assert_eq!(
        state_of(&db, "poison").await,
        JobState::Failed.as_str(),
        "an unkillable row must become a dead letter an operator can find"
    );
}

#[tokio::test]
async fn a_failure_with_attempts_left_is_retried_later() {
    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 3)).await.unwrap();

    let job = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed");
    let retry_at = queue::fail(&db, &job, NOW, "upstream 503", 1.0)
        .await
        .unwrap()
        .expect("attempts remain, so it retries");

    assert!(retry_at > NOW, "a retry must be delayed, not immediate");
    assert_eq!(state_of(&db, "job").await, JobState::Pending.as_str());
    assert!(
        queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "the backoff must actually hold the row back"
    );
    assert!(
        queue::claim_due(&db, retry_at, DEFAULT_LEASE_MS)
            .await
            .unwrap()
            .is_some(),
        "and release it when the backoff has passed"
    );
}

#[tokio::test]
async fn a_failure_with_no_attempts_left_dead_letters_and_keeps_the_reason() {
    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 1)).await.unwrap();

    let job = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed");
    assert!(queue::fail(&db, &job, NOW, "it threw", 1.0)
        .await
        .unwrap()
        .is_none());

    assert_eq!(state_of(&db, "job").await, JobState::Failed.as_str());
    let rows = db
        .query(
            &format!("SELECT last_error FROM {JOBS} WHERE id = 'job'"),
            &[],
        )
        .await
        .unwrap();
    let stored = rows.rows[0]
        .get(0)
        .and_then(dom_render_compiler::forge::SqlValue::as_str)
        .expect("the reason is kept");
    assert_eq!(stored, "it threw", "a dead letter nobody can read is a job that vanished");
}

#[tokio::test]
async fn completing_clears_the_lease_and_the_row_is_never_handed_out_again() {
    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 3)).await.unwrap();
    let job = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed");
    queue::complete(&db, &job.id, NOW).await.unwrap();

    assert_eq!(state_of(&db, "job").await, JobState::Done.as_str());
    assert!(
        queue::claim_due(&db, NOW + 10 * DEFAULT_LEASE_MS, DEFAULT_LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "a done row must not come back when its old lease instant passes"
    );
}

/// What lets the runner sleep exactly as long as it should instead of polling.
#[tokio::test]
async fn the_next_due_instant_is_the_earliest_future_row() {
    let db = substrate().await;
    assert_eq!(
        queue::next_due_after(&db, NOW).await.unwrap(),
        None,
        "an empty queue parks the runner rather than waking it on a timer"
    );

    queue::enqueue(&db, &work("late", "a", NOW + 90_000, 1))
        .await
        .unwrap();
    queue::enqueue(&db, &work("soon", "a", NOW + 30_000, 1))
        .await
        .unwrap();
    queue::enqueue(&db, &work("past", "a", NOW - 30_000, 1))
        .await
        .unwrap();

    assert_eq!(
        queue::next_due_after(&db, NOW).await.unwrap(),
        Some(NOW + 30_000),
        "the earliest *future* row, ignoring what is already due"
    );
}

#[tokio::test]
async fn the_sweep_removes_finished_work_but_never_a_dead_letter() {
    let db = substrate().await;
    queue::enqueue(&db, &work("ok", "a", NOW, 1)).await.unwrap();
    queue::enqueue(&db, &work("bad", "a", NOW, 1)).await.unwrap();

    let ok = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS).await.unwrap().unwrap();
    queue::complete(&db, &ok.id, NOW).await.unwrap();
    let bad = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS).await.unwrap().unwrap();
    queue::fail(&db, &bad, NOW, "threw", 1.0).await.unwrap();

    let removed = queue::sweep_done(&db, NOW + 1).await.unwrap();
    assert_eq!(removed, 1, "only the done row");
    assert_eq!(
        state_of(&db, "bad").await,
        JobState::Failed.as_str(),
        "a queue that quietly deletes its own failures is worse than one with no failures table"
    );
}

/// A fan-out row expands in batches and yields between them, so a large
/// expansion is many small writes rather than one enormous one.
#[tokio::test]
async fn a_fan_out_row_advances_by_cursor_and_then_finishes() {
    let db = substrate().await;
    let fire = queue::fire_id("digest", NOW);
    queue::enqueue(
        &db,
        &Enqueue {
            fanout: true,
            ..work(&fire, "digest", NOW, 1)
        },
    )
    .await
    .unwrap();

    let first = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed");
    assert!(first.is_fanout, "the runner must be told to expand, not to run");
    assert_eq!(first.cursor, None, "the first pass starts at the beginning");

    queue::advance_fanout(&db, &first.id, Some("user-500"), NOW)
        .await
        .unwrap();

    let second = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("the row comes back for its next batch");
    assert_eq!(second.cursor.as_deref(), Some("user-500"));

    queue::advance_fanout(&db, &second.id, None, NOW).await.unwrap();
    assert_eq!(state_of(&db, &fire).await, JobState::Done.as_str());
    assert!(
        queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
            .await
            .unwrap()
            .is_none(),
        "a finished expansion must not be claimed again"
    );
}

/// A child of a fan-out carries exactly one principal — the mechanism that
/// makes a per-user job possible with no god-mode identity in the tree.
#[tokio::test]
async fn a_fan_out_child_carries_its_principal_and_nothing_wider() {
    let db = substrate().await;
    let fire = queue::fire_id("digest", NOW);

    for principal in ["alice", "bob"] {
        queue::enqueue(
            &db,
            &Enqueue {
                id: queue::fanout_child_id(&fire, principal),
                principal: Some(principal.to_string()),
                ..work("unused", "digest", NOW, 1)
            },
        )
        .await
        .unwrap();
    }

    let mut seen = Vec::new();
    while let Some(job) = queue::claim_due(&db, NOW, DEFAULT_LEASE_MS).await.unwrap() {
        seen.push(job.principal.clone().expect("a child always carries one"));
        queue::complete(&db, &job.id, NOW).await.unwrap();
    }
    seen.sort();
    assert_eq!(seen, vec!["alice".to_string(), "bob".to_string()]);
}

/// The guard on the claim's `UPDATE`, proved directly — because the
/// concurrency tests above **cannot** prove it.
///
/// `LibSqlSubstrate` serialises every write and transaction behind one writer
/// mutex, so two claimants never interleave and the second one's `SELECT`
/// already excludes the row. Removing `AND state = ?` therefore fails none of
/// those tests; it was tried. The guard exists for the day that serialisation
/// stops being true, so it needs a test that exercises its semantics rather
/// than a race that can never reach it.
#[tokio::test]
async fn the_claim_update_refuses_a_row_whose_state_has_already_moved() {
    use dom_render_compiler::forge::SqlValue;

    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 5)).await.unwrap();
    queue::claim_due(&db, NOW, DEFAULT_LEASE_MS)
        .await
        .unwrap()
        .expect("claimed, so the row is now `claimed`");

    // Exactly the statement `claim_due` issues, with the state a second
    // claimant would have read before the first one won.
    let affected = db
        .execute(
            &format!(
                "UPDATE {JOBS} SET state = 'claimed', lease_until = ?, attempts = ?, \
                 updated_at = ? WHERE id = ? AND state = ?"
            ),
            &[
                SqlValue::Integer(NOW + DEFAULT_LEASE_MS),
                SqlValue::Integer(1),
                SqlValue::Integer(NOW),
                SqlValue::Text("job".to_string()),
                SqlValue::Text(JobState::Pending.as_str().to_string()),
            ],
        )
        .await
        .expect("the statement runs");

    assert_eq!(
        affected, 0,
        "a stale-state update must touch nothing — this is the invariant the guard holds \
         if the substrate ever stops serialising writers"
    );
}

/// Re-running the DDL must be a no-op, because boot runs it every time.
#[tokio::test]
async fn the_ddl_is_idempotent() {
    let db = substrate().await;
    queue::enqueue(&db, &work("job", "a", NOW, 1)).await.unwrap();
    for statement in queue::ddl() {
        db.migrate(&statement).await.expect("re-applying is a no-op");
    }
    assert_eq!(count(&db).await, 1, "and it does not clear the table");
}
