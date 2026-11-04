//! Durable action admission and completion against the real storage boundary.

#![cfg(feature = "forge")]

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dom_render_compiler::aperture::admission::{ActionClaim, Admission, ACTION_LEASE_MS};
use dom_render_compiler::aperture::{JournalLedger, StepKind, StepOutcome};
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate, SqlValue, Transaction};
use dom_render_compiler::jobs::queue::{self, Enqueue};
use tokio::sync::{oneshot, Barrier};

const WORKFLOW: &str = "action-intent";
const BUILD: &str = "build-a";
const DIGEST: &str = "request-a";
const NOW: i64 = 1_000_000;
const RESULT: &[u8] = b"wire-result";

async fn initialize(path: &Path) -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_local(path).await.unwrap();
    JournalLedger::new(&db).migrate().await.unwrap();
    for ddl in queue::ddl() {
        db.migrate(&ddl).await.unwrap();
    }
    db.migrate(
        "CREATE TABLE IF NOT EXISTS action_effects \
         (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL)",
    )
    .await
    .unwrap();
    db
}

async fn acquire(db: &dyn DataSubstrate, now_ms: i64) -> ActionClaim {
    match ActionClaim::acquire(db, WORKFLOW, BUILD, DIGEST, now_ms)
        .await
        .unwrap()
    {
        Admission::Acquired(claim) => claim,
        other => panic!("expected ownership, got {other:?}"),
    }
}

async fn stage_effects(tx: &dyn Transaction) {
    tx.execute("INSERT INTO action_effects (payload) VALUES ('one')", &[])
        .await
        .unwrap();
    // A fresh id makes duplicate execution observable instead of hiding it
    // behind the job queue's independent idempotency mechanism.
    let job = Enqueue {
        id: uuid::Uuid::new_v4().to_string(),
        name: "process".into(),
        args: "{}".into(),
        principal: None,
        run_at_ms: NOW,
        max_attempts: 1,
        fanout: false,
    };
    assert!(queue::enqueue_in_transaction(tx, &job).await.unwrap());
}

async fn complete(db: &dyn DataSubstrate, claim: &ActionClaim, now_ms: i64) {
    let tx = db.begin().await.unwrap();
    claim.check(tx.as_ref(), now_ms).await.unwrap();
    stage_effects(tx.as_ref()).await;
    claim.complete(tx.as_ref(), RESULT, now_ms).await.unwrap();
    tx.commit().await.unwrap();
}

async fn count(db: &dyn DataSubstrate, table: &str) -> i64 {
    let rows = db
        .query(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .await
        .unwrap();
    rows.rows[0].get(0).and_then(SqlValue::as_i64).unwrap()
}

async fn assert_effects(db: &dyn DataSubstrate, expected: i64) {
    for table in ["action_effects", "albedo_jobs", "albedo_workflow_results"] {
        assert_eq!(count(db, table).await, expected, "{table}");
    }
    let rows = db
        .query(
            "SELECT completed_at FROM albedo_action_admissions WHERE workflow = ?",
            &[WORKFLOW.into()],
        )
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 1, "admission must survive independently");
    assert_eq!(
        matches!(rows.rows[0].get(0), Some(SqlValue::Null)),
        expected == 0,
        "admission completion and effects must agree"
    );
}

async fn assert_replay(db: &dyn DataSubstrate, now_ms: i64) {
    match ActionClaim::acquire(db, WORKFLOW, BUILD, DIGEST, now_ms)
        .await
        .unwrap()
    {
        Admission::Completed(bytes) => assert_eq!(bytes, RESULT),
        other => panic!("expected recorded completion, got {other:?}"),
    }
    assert_effects(db, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_connections_admit_one_concurrent_retry_and_replay_completion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("admission.db");
    let first = Arc::new(initialize(&path).await);
    let mut connections = vec![first.clone()];
    for _ in 1..4 {
        connections.push(Arc::new(LibSqlSubstrate::open_local(&path).await.unwrap()));
    }
    let barrier = Arc::new(Barrier::new(connections.len()));
    let mut attempts = Vec::new();
    for db in &connections {
        let db = db.clone();
        let barrier = barrier.clone();
        attempts.push(tokio::spawn(async move {
            barrier.wait().await;
            ActionClaim::acquire(db.as_ref(), WORKFLOW, BUILD, DIGEST, NOW).await
        }));
    }
    let mut winner = None;
    let mut busy = 0;
    for attempt in attempts {
        match attempt
            .await
            .unwrap()
            .expect("contention must yield a verdict")
        {
            Admission::Acquired(claim) => {
                assert!(
                    winner.replace(claim).is_none(),
                    "only one attempt owns the intent"
                );
            }
            Admission::Busy => busy += 1,
            other => panic!("nothing has completed yet: {other:?}"),
        }
    }
    assert_eq!(busy, connections.len() - 1);
    complete(first.as_ref(), &winner.unwrap(), NOW + 1).await;
    for db in &connections {
        assert_replay(db.as_ref(), NOW + 2).await;
    }
}

#[tokio::test]
async fn expired_owner_cannot_complete_or_mutate_the_workflow_journal_after_takeover() {
    let dir = tempfile::tempdir().unwrap();
    let db = initialize(&dir.path().join("admission.db")).await;
    // Journal mutations check wall time. Keep both synthetic leases ahead of
    // wall time so the stale-owner assertions prove fencing after takeover.
    let start = dom_render_compiler::aperture::now_ms() + 1_000_000;
    let old = acquire(&db, start).await;
    let old_ledger = JournalLedger::new(&db).with_claim(&old);
    old_ledger
        .begin_step(WORKFLOW, BUILD, 0, StepKind::Fetch, "step-0", start)
        .await
        .unwrap();
    let tx = db.begin().await.unwrap();
    assert!(old
        .check(tx.as_ref(), start + ACTION_LEASE_MS)
        .await
        .is_err());
    tx.rollback().await.unwrap();

    let takeover = start + ACTION_LEASE_MS;
    let current = acquire(&db, takeover).await;
    assert!(old_ledger
        .begin_step(WORKFLOW, BUILD, 1, StepKind::Fetch, "step-1", takeover)
        .await
        .is_err());
    assert!(old_ledger
        .settle_step(
            WORKFLOW,
            0,
            &StepOutcome::Completed(serde_json::json!("stale")),
            takeover
        )
        .await
        .is_err());
    let journal = JournalLedger::new(&db)
        .load(WORKFLOW)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal.steps()[0].outcome, StepOutcome::Unknown);

    let tx = db.begin().await.unwrap();
    stage_effects(tx.as_ref()).await;
    assert!(old
        .complete(tx.as_ref(), b"stale", takeover + 1)
        .await
        .is_err());
    tx.rollback().await.unwrap();
    assert_effects(&db, 0).await;
    JournalLedger::new(&db)
        .with_claim(&current)
        .settle_step(
            WORKFLOW,
            0,
            &StepOutcome::Completed(serde_json::json!("current")),
            takeover + 1,
        )
        .await
        .unwrap();
    complete(&db, &current, takeover + 1).await;
    assert_replay(&db, takeover + 2).await;
}

#[tokio::test]
async fn cancellation_drops_uncommitted_effects_jobs_and_result_and_releases_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(initialize(&dir.path().join("admission.db")).await);
    let claim = acquire(db.as_ref(), NOW).await;
    let (ready_tx, ready_rx) = oneshot::channel();
    let task_db = db.clone();
    let task = tokio::spawn(async move {
        let tx = task_db.begin().await.unwrap();
        stage_effects(tx.as_ref()).await;
        claim.complete(tx.as_ref(), RESULT, NOW + 1).await.unwrap();
        ready_tx.send(()).unwrap();
        std::future::pending::<()>().await;
        tx.commit().await.unwrap();
    });
    ready_rx.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_effects(db.as_ref(), 0).await;
    let retry_at = NOW + ACTION_LEASE_MS;
    let retry = tokio::time::timeout(Duration::from_secs(5), acquire(db.as_ref(), retry_at))
        .await
        .expect("cancellation must leave the writer reusable");
    complete(db.as_ref(), &retry, retry_at + 1).await;
    assert_replay(db.as_ref(), retry_at + 2).await;
}

#[tokio::test]
async fn reusing_an_intent_for_another_request_or_build_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db = initialize(&dir.path().join("admission.db")).await;
    let claim = acquire(&db, NOW).await;
    for (build, digest) in [(BUILD, "other-request"), ("other-build", DIGEST)] {
        assert!(
            ActionClaim::acquire(&db, WORKFLOW, build, digest, NOW + ACTION_LEASE_MS)
                .await
                .is_err()
        );
    }
    complete(&db, &claim, NOW + 1).await;
    for (build, digest) in [(BUILD, "other-request"), ("other-build", DIGEST)] {
        assert!(ActionClaim::acquire(&db, WORKFLOW, build, digest, NOW + 2)
            .await
            .is_err());
    }
    assert_replay(&db, NOW + 2).await;
}

#[tokio::test]
async fn sweep_keeps_active_admissions_then_removes_completed_history_together() {
    let dir = tempfile::tempdir().unwrap();
    let db = initialize(&dir.path().join("admission.db")).await;
    let now = dom_render_compiler::aperture::now_ms();
    let claim = acquire(&db, now).await;
    let ledger = JournalLedger::new(&db);
    JournalLedger::new(&db)
        .with_claim(&claim)
        .begin_step(WORKFLOW, BUILD, 0, StepKind::Fetch, "step-0", now)
        .await
        .unwrap();
    let cutoff = now + ACTION_LEASE_MS * 2;
    assert_eq!(ledger.sweep(cutoff).await.unwrap(), 0);
    assert_eq!(count(&db, "albedo_workflow_steps").await, 1);
    assert_eq!(count(&db, "albedo_action_admissions").await, 1);
    assert!(matches!(
        ActionClaim::acquire(&db, WORKFLOW, BUILD, DIGEST, now + 1)
            .await
            .unwrap(),
        Admission::Busy
    ));

    complete(&db, &claim, now + 1).await;
    assert_replay(&db, now + 2).await;
    assert_eq!(ledger.sweep(cutoff).await.unwrap(), 2);
    for table in [
        "albedo_workflow_steps",
        "albedo_workflow_results",
        "albedo_action_admissions",
    ] {
        assert_eq!(count(&db, table).await, 0, "{table}");
    }
    assert_eq!(count(&db, "action_effects").await, 1);
    assert_eq!(count(&db, "albedo_jobs").await, 1);
}

#[tokio::test]
async fn legacy_completion_replays_without_admission_and_rejects_a_different_build() {
    let dir = tempfile::tempdir().unwrap();
    let db = initialize(&dir.path().join("admission.db")).await;
    JournalLedger::new(&db)
        .complete(WORKFLOW, BUILD, RESULT, NOW)
        .await
        .unwrap();
    match ActionClaim::acquire(&db, WORKFLOW, BUILD, DIGEST, NOW + 1)
        .await
        .unwrap()
    {
        Admission::Completed(bytes) => assert_eq!(bytes, RESULT),
        other => panic!("legacy completion must replay, got {other:?}"),
    }
    let error = ActionClaim::acquire(&db, WORKFLOW, "new-build", DIGEST, NOW + 1)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("different build"), "{error}");
    assert_eq!(count(&db, "albedo_action_admissions").await, 0);
    assert_eq!(count(&db, "albedo_workflow_results").await, 1);
    assert_eq!(count(&db, "action_effects").await, 0);
    assert_eq!(count(&db, "albedo_jobs").await, 0);
}

#[tokio::test]
async fn unavailable_admission_or_result_storage_never_grants_execution() {
    for missing in ["albedo_action_admissions", "albedo_workflow_results"] {
        let dir = tempfile::tempdir().unwrap();
        let db = initialize(&dir.path().join("admission.db")).await;
        db.migrate(&format!("DROP TABLE {missing}")).await.unwrap();
        assert!(
            ActionClaim::acquire(&db, WORKFLOW, BUILD, DIGEST, NOW)
                .await
                .is_err(),
            "unavailable {missing} must fail closed"
        );
        assert_eq!(count(&db, "action_effects").await, 0);
        assert_eq!(count(&db, "albedo_jobs").await, 0);
        // Restoring storage must find no partial admission from the failed
        // attempt, and the failed transaction must have released its writer.
        JournalLedger::new(&db).migrate().await.unwrap();
        let claim = acquire(&db, NOW + 1).await;
        complete(&db, &claim, NOW + 2).await;
        assert_replay(&db, NOW + 3).await;
    }
}

#[tokio::test]
async fn a_completed_admission_without_its_result_is_an_error_instead_of_a_new_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let db = initialize(&dir.path().join("admission.db")).await;
    let claim = acquire(&db, NOW).await;
    complete(&db, &claim, NOW + 1).await;
    let before = db
        .query(
            "SELECT owner, completed_at FROM albedo_action_admissions",
            &[],
        )
        .await
        .unwrap();
    db.execute("DELETE FROM albedo_workflow_results", &[])
        .await
        .unwrap();
    let error = ActionClaim::acquire(&db, WORKFLOW, BUILD, DIGEST, NOW + ACTION_LEASE_MS)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no readable result"), "{error}");
    assert_eq!(
        db.query(
            "SELECT owner, completed_at FROM albedo_action_admissions",
            &[]
        )
        .await
        .unwrap(),
        before,
        "a missing result must not replace a completed admission"
    );
    assert_eq!(count(&db, "action_effects").await, 1);
    assert_eq!(count(&db, "albedo_jobs").await, 1);
    assert_eq!(count(&db, "albedo_workflow_results").await, 0);
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Invoked in a separate process by the parent test. Returning normally during
/// an ordinary test run keeps child-only environment handling out of the suite.
#[tokio::test]
async fn admission_process_child() {
    let Some(path) = std::env::var_os("ALBEDO_ADMISSION_CHILD_DB") else {
        return;
    };
    let stage = std::env::var("ALBEDO_ADMISSION_CHILD_STAGE").unwrap();
    let marker = std::env::var_os("ALBEDO_ADMISSION_CHILD_READY").unwrap();
    let db = LibSqlSubstrate::open_local(path).await.unwrap();
    let claim = acquire(&db, NOW).await;
    if stage == "after-claim" {
        std::fs::write(&marker, b"ready").unwrap();
        std::future::pending::<()>().await;
        return;
    }
    let tx = db.begin().await.unwrap();
    stage_effects(tx.as_ref()).await;
    if stage != "after-effects" {
        claim.complete(tx.as_ref(), RESULT, NOW + 1).await.unwrap();
    }
    if stage == "after-commit" {
        tx.commit().await.unwrap();
        std::fs::write(&marker, b"ready").unwrap();
        std::future::pending::<()>().await;
    } else {
        std::fs::write(&marker, b"ready").unwrap();
        std::future::pending::<()>().await;
        tx.commit().await.unwrap();
    }
}

#[tokio::test]
async fn process_death_at_each_commit_boundary_recovers_all_effects_or_none() {
    for stage in [
        "after-claim",
        "after-effects",
        "after-complete",
        "after-commit",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admission.db");
        drop(initialize(&path).await);
        let marker = dir.path().join("child.ready");
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "admission_process_child", "--nocapture"])
            .env("ALBEDO_ADMISSION_CHILD_DB", &path)
            .env("ALBEDO_ADMISSION_CHILD_STAGE", stage)
            .env("ALBEDO_ADMISSION_CHILD_READY", &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut child = KillOnDrop(child);
        let deadline = Instant::now() + Duration::from_secs(15);
        while !marker.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before {stage}"
            );
            assert!(Instant::now() < deadline, "child never reached {stage}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        child.0.kill().unwrap();
        assert!(
            !child.0.wait().unwrap().success(),
            "child must actually be killed"
        );

        let db = LibSqlSubstrate::open_local(&path).await.unwrap();
        assert_effects(&db, i64::from(stage == "after-commit")).await;
        let retry_at = NOW + ACTION_LEASE_MS;
        if stage == "after-commit" {
            assert_replay(&db, retry_at).await;
        } else {
            let retry = acquire(&db, retry_at).await;
            complete(&db, &retry, retry_at + 1).await;
        }
        assert_replay(&db, retry_at + 2).await;
    }
}
