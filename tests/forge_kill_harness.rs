//! FORGE · the kill harness — rung ④, and the Beat-1 gate.
//!
//! Everything else FORGE claims is worthless if this fails. The promise is not
//! "it is fast" or "it never oversells under load" — both already proven — it is:
//!
//! > **Kill the server mid-purchase and the books are still exact. Nothing was
//! > sold twice, nothing was lost, nothing is half-written.**
//!
//! ## How it is proven
//!
//! A real child process claims as fast as it can, in several concurrent tasks,
//! against a real database file. The parent kills it with
//! [`Child::kill`] — `TerminateProcess` on Windows, `SIGKILL` on Unix. That is
//! *uncatchable*: no destructors, no unwinding, no flush, no graceful commit.
//! The process simply stops existing, possibly with a transaction half-applied
//! and the write lock still held.
//!
//! The parent then reopens the database and audits it. This repeats over many
//! cycles with a varying delay, so the kill lands at different points in the
//! transaction lifecycle rather than one lucky spot — between the decrement and
//! the insert, mid-commit, mid-fsync.
//!
//! ## What is asserted after every kill
//!
//! - **The database is not corrupt** (`PRAGMA integrity_check`).
//! - **Supply never went negative.**
//! - **Conservation** — units claimed, summed from the reservations themselves,
//!   equals the supply consumed. This is the one that would catch a torn write:
//!   supply taken with no claim recorded, or a claim recorded against supply
//!   never taken. A kill between those two statements is *precisely* the failure
//!   the transaction exists to prevent.
//! - **Durability** — claims committed before the kill are all still there
//!   afterwards. Work never goes backwards across a crash.
//! - **The kill actually interrupted work** — if the child never claimed
//!   anything, the cycle proved nothing and the harness says so rather than
//!   passing quietly.
//!
//! ## Scope: atomic, not resumable
//!
//! A claim in flight at the instant of the kill is **lost**, and that is correct
//! and intended. The buyer never got a confirmation, so nothing is owed. FORGE
//! promises the books are consistent, not that a half-finished purchase
//! continues on reboot — that is crash-*resumability*, a separate milestone.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use dom_render_compiler::forge::{
    DataSubstrate, LibSqlSubstrate, ReserveOutcome, ReserveRequest, Reservations, SqlValue,
};

/// Set on the child to tell it which database to hammer. Its presence is also
/// how the child knows it is the worker rather than the harness.
const WORKER_ENV: &str = "FORGE_KILL_WORKER_DB";

/// Deliberately far more supply than any run will consume: the worker must
/// never stop for lack of tickets, because we need it *writing* when it dies.
const CAPACITY: i64 = 1_000_000;

/// Kill cycles. Each uses a different delay so the kill lands somewhere new.
const CYCLES: usize = 12;

/// The child: claim in a loop, from several tasks, until killed.
///
/// It is expected to die here. The sleep is only a fuse so a failure to kill
/// shows up as a finished test rather than a hung one.
async fn run_worker(db_path: &str) {
    let db = Arc::new(
        LibSqlSubstrate::open_local(db_path)
            .await
            .expect("worker opens db"),
    );

    for task in 0..4 {
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            let reservations = Reservations::new(db.as_ref());
            let pid = std::process::id();
            let mut n = 0u64;
            loop {
                let holder = format!("p{pid}-t{task}-{n}");
                match reservations
                    .reserve(&ReserveRequest::new("drop", &holder))
                    .await
                {
                    Ok(ReserveOutcome::Reserved { .. }) => n += 1,
                    // Out of supply or the substrate is gone: nothing left to
                    // prove from this task.
                    _ => break,
                }
            }
        });
    }

    tokio::time::sleep(Duration::from_secs(20)).await;
    // Reaching here means the parent never killed us — say so loudly rather
    // than exiting 0 and letting the harness think the cycle was meaningful.
    eprintln!("worker was never killed");
    std::process::exit(9);
}

/// Units recorded as claimed — summed from the reservations themselves, which
/// is the independent witness against the resource row's own bookkeeping.
async fn claimed_units(db: &LibSqlSubstrate) -> i64 {
    let rows = db
        .query(
            "SELECT COALESCE(SUM(quantity), 0) FROM forge_reservation WHERE resource_key = ?1",
            &["drop".into()],
        )
        .await
        .expect("sum query");
    rows.rows[0]
        .get(0)
        .and_then(SqlValue::as_i64)
        .expect("sum is an integer")
}

async fn remaining_supply(db: &LibSqlSubstrate) -> i64 {
    let rows = db
        .query(
            "SELECT remaining FROM forge_resource WHERE key = ?1",
            &["drop".into()],
        )
        .await
        .expect("remaining query");
    rows.rows[0]
        .get(0)
        .and_then(SqlValue::as_i64)
        .expect("remaining is an integer")
}

/// `PRAGMA integrity_check` returns the single row `ok` on a healthy database.
async fn integrity(db: &LibSqlSubstrate) -> String {
    let rows = db
        .query("PRAGMA integrity_check", &[])
        .await
        .expect("integrity_check");
    rows.rows
        .first()
        .and_then(|r| r.get(0))
        .and_then(SqlValue::as_str)
        .unwrap_or("<no result>")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_claims_survive_a_kill_mid_write() {
    // Re-executed as the child? Then be the worker and never come back.
    if let Ok(db_path) = std::env::var(WORKER_ENV) {
        run_worker(&db_path).await;
        return;
    }

    let dir = std::env::temp_dir().join(format!("forge-kill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let db_path = dir.join("forge.db");
    let db_arg = db_path.to_string_lossy().into_owned();

    // Seed the database, then let go of it so the child owns the file.
    {
        let db = LibSqlSubstrate::open_local(&db_path).await.unwrap();
        let reservations = Reservations::new(&db);
        reservations.migrate().await.unwrap();
        reservations.define("drop", CAPACITY).await.unwrap();
    }

    let exe = std::env::current_exe().expect("test binary path");
    let mut previous_claimed = 0i64;
    let mut cycles_that_did_work = 0;

    for cycle in 0..CYCLES {
        let mut child = Command::new(&exe)
            .arg("--exact")
            .arg("committed_claims_survive_a_kill_mid_write")
            .env(WORKER_ENV, &db_arg)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn worker");

        // Vary when the axe falls so it lands at a different point in the
        // transaction lifecycle each cycle, not one lucky spot.
        let delay = 30 + (cycle as u64 * 11) % 70;
        tokio::time::sleep(Duration::from_millis(delay)).await;

        // Uncatchable. No destructors, no flush, no graceful commit — the
        // process stops existing, possibly mid-transaction, possibly holding
        // the write lock.
        child.kill().expect("kill worker");
        let status = child.wait().expect("reap worker");
        assert!(
            !status.success(),
            "cycle {cycle}: worker exited on its own — it was never killed mid-write, \
             so this cycle proves nothing"
        );

        // Audit the wreckage.
        let db = LibSqlSubstrate::open_local(&db_path).await.unwrap();

        let health = integrity(&db).await;
        assert_eq!(health, "ok", "cycle {cycle}: database corrupted by the kill");

        let remaining = remaining_supply(&db).await;
        let claimed = claimed_units(&db).await;

        assert!(remaining >= 0, "cycle {cycle}: supply went negative");
        assert_eq!(
            claimed,
            CAPACITY - remaining,
            "cycle {cycle}: TORN WRITE — the books do not balance. Supply consumed \
             and claims recorded disagree, which means a kill split a transaction."
        );
        assert!(
            claimed >= previous_claimed,
            "cycle {cycle}: committed claims went backwards ({previous_claimed} -> {claimed}) \
             — a crash lost durable work"
        );

        if claimed > previous_claimed {
            cycles_that_did_work += 1;
        }
        previous_claimed = claimed;
        drop(db);
    }

    // A harness that never caught the worker writing would pass while proving
    // nothing at all — the exact trap rung ③ taught us.
    assert!(
        cycles_that_did_work >= CYCLES / 2,
        "only {cycles_that_did_work}/{CYCLES} cycles actually interrupted work; \
         this harness is not testing what it claims"
    );
    assert!(
        previous_claimed > 0,
        "no claims were ever committed — nothing was proven"
    );

    println!(
        "kill harness: {CYCLES} kills mid-write, {previous_claimed} claims committed and intact, \
         {cycles_that_did_work} cycles interrupted live writes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
