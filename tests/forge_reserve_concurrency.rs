//! FORGE · the oversell harness — contention, at volume.
//!
//! Rung ③ of THE DROP's Beat-1 ladder. [`reserve()`]'s own unit tests prove the
//! shape of each outcome; this proves the **property** that the whole
//! backend-less claim rests on, under real concurrency, a thousand times over:
//!
//! > **Supply is conserved. It never goes negative, never oversells, and the
//! > losers lose cleanly.**
//!
//! ## Why "lose cleanly" is load-bearing, not a nicety
//!
//! An earlier, more forgiving version of this test tolerated losers failing
//! with a substrate error. It passed — and it was lying. Under a strict
//! assertion, 15 of 16 concurrent claimants were failing with `cannot start a
//! transaction within a transaction`: the oversell invariant held only because
//! the losers never reached the supply check. The property had never actually
//! been exercised. So every assertion here refuses to accept an error where a
//! verdict is owed: a buyer must be told *sold out*, not handed a 500.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use std::collections::HashSet;
use std::sync::Arc;

use dom_render_compiler::forge::{
    DataSubstrate, LibSqlSubstrate, ReserveOutcome, ReserveRequest, Reservations, SqlValue,
};

/// Rounds of contention to run. The gate is stated as "×1000" — enough that a
/// race with even a small per-round probability shows up rather than hiding.
const ROUNDS: usize = 1000;

/// Deterministic xorshift64. Seeded, so a failure is reproducible: the same
/// seed replays the exact sequence of rounds that broke it.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform-ish in `lo..=hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
}

/// What one contended round actually produced.
struct RoundReport {
    winners: Vec<i64>,
    exhausted: usize,
    /// Losers that errored instead of being told "sold out". Must always be
    /// empty — see the module docs.
    refused: Vec<String>,
}

/// Fire `buyers` concurrent claims of `quantity` units at one resource.
async fn contend(
    db: &Arc<LibSqlSubstrate>,
    resource: &str,
    buyers: usize,
    quantity: i64,
) -> RoundReport {
    let mut handles = Vec::with_capacity(buyers);
    for buyer in 0..buyers {
        let db = Arc::clone(db);
        let resource = resource.to_owned();
        handles.push(tokio::spawn(async move {
            let reservations = Reservations::new(db.as_ref());
            let holder = format!("buyer-{buyer}");
            reservations
                .reserve(&ReserveRequest::new(&resource, &holder).quantity(quantity))
                .await
        }));
    }

    let mut report = RoundReport {
        winners: Vec::new(),
        exhausted: 0,
        refused: Vec::new(),
    };
    for handle in handles {
        match handle.await.expect("buyer task panicked") {
            Ok(ReserveOutcome::Reserved { reservation_id, .. }) => {
                report.winners.push(reservation_id);
            }
            Ok(ReserveOutcome::Exhausted { .. }) => report.exhausted += 1,
            Ok(ReserveOutcome::Replayed { .. }) => {
                panic!("no idempotency key was set, so nothing can be a replay")
            }
            Err(e) => report.refused.push(e.to_string()),
        }
    }
    report
}

/// Units actually recorded as claimed against `resource`. The independent
/// witness: `remaining` is what the resource row *says*, this is what the
/// reservations *prove*. Conservation means they agree.
async fn claimed_units(db: &LibSqlSubstrate, resource: &str) -> i64 {
    let rows = db
        .query(
            "SELECT COALESCE(SUM(quantity), 0) FROM forge_reservation WHERE resource_key = ?1",
            &[resource.into()],
        )
        .await
        .expect("sum query");
    rows.rows[0]
        .get(0)
        .and_then(SqlValue::as_i64)
        .expect("sum is an integer")
}

/// The substrate must survive concurrent **reads**.
///
/// Not a hypothetical: the serve path holds one substrate behind an `Arc` and
/// every request reads through it, so concurrent `query` is the *normal* case.
/// This is isolated from any claim logic on purpose — if it fails, the bug is
/// under [`DataSubstrate`], not in [`Reservations`].
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn substrate_survives_concurrent_reads() {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let reservations = Reservations::new(db.as_ref());
    reservations.migrate().await.unwrap();
    reservations.define("ticket", 1).await.unwrap();

    let mut handles = Vec::new();
    for _ in 0..64 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                let rows = db
                    .query(
                        "SELECT remaining FROM forge_resource WHERE key = ?1",
                        &["ticket".into()],
                    )
                    .await
                    .expect("concurrent read");
                assert_eq!(rows.rows[0].get(0).and_then(SqlValue::as_i64), Some(1));
            }
        }));
    }
    for handle in handles {
        handle.await.expect("a concurrent reader died");
    }
}

/// Reads on the substrate's shared connection, concurrent with transactions
/// opening their own connections — the exact mix `reserve()` now performs, and
/// the one the serve path will perform under load.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn substrate_survives_reads_concurrent_with_transactions() {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let reservations = Reservations::new(db.as_ref());
    reservations.migrate().await.unwrap();
    reservations.define("pool", 1_000_000).await.unwrap();

    let mut handles = Vec::new();
    // Readers: hammer the shared connection.
    for _ in 0..32 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            for _ in 0..100 {
                db.query(
                    "SELECT remaining FROM forge_resource WHERE key = ?1",
                    &["pool".into()],
                )
                .await
                .expect("read");
            }
        }));
    }
    // Writers: each opens its own connection and commits.
    for w in 0..32 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let reservations = Reservations::new(db.as_ref());
            for i in 0..20 {
                let holder = format!("w{w}-{i}");
                reservations
                    .reserve(&ReserveRequest::new("pool", &holder))
                    .await
                    .expect("claim");
            }
        }));
    }
    for handle in handles {
        handle.await.expect("a task died");
    }
}

/// The gate: a thousand contended rounds, randomised over capacity, buyer
/// count and claim size, each asserting the full invariant — not just "nobody
/// oversold" but "the books balance and every loser got a straight answer".
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn oversell_invariant_holds_across_a_thousand_contended_rounds() {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    Reservations::new(db.as_ref()).migrate().await.unwrap();

    let mut rng = Rng::new(0x5EED_1234_ABCD_0001);

    for round in 0..ROUNDS {
        // Vary the shape so the harness explores the boundary rather than one
        // lucky configuration: sometimes supply is scarcer than demand,
        // sometimes it is plentiful; sometimes claims are multi-unit.
        let capacity = rng.range(1, 8) as i64;
        let buyers = rng.range(2, 8) as usize;
        let quantity = rng.range(1, 3) as i64;

        let resource = format!("r{round}");
        let reservations = Reservations::new(db.as_ref());
        reservations.define(&resource, capacity).await.unwrap();

        let report = contend(&db, &resource, buyers, quantity).await;

        let context = format!(
            "round {round}: capacity={capacity} buyers={buyers} quantity={quantity}"
        );

        // 1. Every loser was told "sold out" — nobody got an error.
        assert!(
            report.refused.is_empty(),
            "{context}: losers must be told sold-out, not handed an error: {:?}",
            report.refused
        );

        // 2. Exactly the number of claims supply can cover, won.
        let expected_winners = usize::try_from(capacity / quantity)
            .unwrap()
            .min(buyers);
        assert_eq!(
            report.winners.len(),
            expected_winners,
            "{context}: wrong number of winners"
        );

        // 3. Everyone else lost — no claim vanished.
        assert_eq!(
            report.winners.len() + report.exhausted,
            buyers,
            "{context}: every buyer must get exactly one verdict"
        );

        // 4. No reservation id was handed out twice.
        let unique: HashSet<i64> = report.winners.iter().copied().collect();
        assert_eq!(
            unique.len(),
            report.winners.len(),
            "{context}: duplicate reservation id"
        );

        // 5. Supply never went negative, and never below what was claimable.
        let remaining = reservations
            .remaining(&resource)
            .await
            .unwrap()
            .expect("resource is defined");
        assert!(remaining >= 0, "{context}: supply went negative");

        // 6. Conservation — the books balance. What the resource row says is
        //    gone equals what the reservations say was taken.
        let claimed = claimed_units(&db, &resource).await;
        assert_eq!(
            claimed,
            capacity - remaining,
            "{context}: claimed units and consumed supply disagree"
        );
        assert_eq!(
            claimed,
            expected_winners as i64 * quantity,
            "{context}: claimed units are not a whole number of claims"
        );
    }
}

/// **THE DROP's actual shape:** far more buyers than tickets, all arriving at
/// once. Everything else in this file has roughly as much supply as demand;
/// a real drop is 95% losers, and the losers are the load.
///
/// This is the case the lock-free pre-check exists for. Without it every one of
/// these buyers takes the exclusive write lock to be told "no"; with it, only
/// plausible winners ever reach it and the rest are answered by parallel reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_drop_serves_far_more_losers_than_winners() {
    const BUYERS: usize = 2000;
    const TICKETS: i64 = 100;

    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let reservations = Reservations::new(db.as_ref());
    reservations.migrate().await.unwrap();
    reservations.define("drop", TICKETS).await.unwrap();

    let started = std::time::Instant::now();
    let report = contend(&db, "drop", BUYERS, 1).await;
    let elapsed = started.elapsed();

    assert!(
        report.refused.is_empty(),
        "a losing buyer must be told sold-out, not handed an error: {:?}",
        report.refused
    );
    assert_eq!(
        report.winners.len(),
        usize::try_from(TICKETS).unwrap(),
        "exactly the supply may win"
    );
    assert_eq!(report.exhausted, BUYERS - usize::try_from(TICKETS).unwrap());
    assert_eq!(reservations.remaining("drop").await.unwrap(), Some(0));
    assert_eq!(claimed_units(&db, "drop").await, TICKETS);

    // Not an assertion — a receipt. Printed with `--no-capture`.
    println!("THE DROP: {BUYERS} buyers, {TICKETS} tickets, settled in {elapsed:?}");
}

/// The retry storm: a client double-submits, or a crashed request is retried
/// while the original is still in flight. Concurrent claims sharing one
/// idempotency key must collapse to **one** reservation and take supply
/// **once** — otherwise "crash-atomic" is a story, not a property.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_retries_of_one_idempotency_key_take_supply_once() {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let reservations = Reservations::new(db.as_ref());
    reservations.migrate().await.unwrap();
    reservations.define("ticket", 10).await.unwrap();

    let mut handles = Vec::new();
    for _ in 0..12 {
        let db = Arc::clone(&db);
        handles.push(tokio::spawn(async move {
            let reservations = Reservations::new(db.as_ref());
            reservations
                .reserve(&ReserveRequest::new("ticket", "ada").idempotency_key("retry-storm"))
                .await
        }));
    }

    let mut reserved = 0;
    let mut replayed = 0;
    let mut ids = HashSet::new();
    for handle in handles {
        match handle.await.expect("task panicked") {
            Ok(ReserveOutcome::Reserved { reservation_id, .. }) => {
                reserved += 1;
                ids.insert(reservation_id);
            }
            Ok(ReserveOutcome::Replayed { reservation_id, .. }) => {
                replayed += 1;
                ids.insert(reservation_id);
            }
            Ok(ReserveOutcome::Exhausted { .. }) => panic!("capacity 10 cannot be exhausted by 1"),
            Err(e) => panic!("a retry must never error: {e}"),
        }
    }

    assert_eq!(reserved, 1, "exactly one racer may create the reservation");
    assert_eq!(replayed, 11, "every other racer must observe the replay");
    assert_eq!(ids.len(), 1, "all racers must name the same reservation");
    assert_eq!(
        reservations.remaining("ticket").await.unwrap(),
        Some(9),
        "a retry storm may take supply exactly once"
    );
    assert_eq!(claimed_units(&db, "ticket").await, 1);
}

/// A multi-unit claim is all-or-nothing under contention: the last two units
/// cannot be split between two buyers who each asked for two.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn contended_multi_unit_claims_never_partially_fill() {
    let db = Arc::new(LibSqlSubstrate::open_ephemeral().await.unwrap());
    let reservations = Reservations::new(db.as_ref());
    reservations.migrate().await.unwrap();
    // 5 units, everyone wants 2: at most two buyers can be served, and one
    // unit must be left stranded — never shaved off a third buyer.
    reservations.define("pair", 5).await.unwrap();

    let report = contend(&db, "pair", 8, 2).await;

    assert!(report.refused.is_empty(), "{:?}", report.refused);
    assert_eq!(report.winners.len(), 2);
    assert_eq!(report.exhausted, 6);
    assert_eq!(reservations.remaining("pair").await.unwrap(), Some(1));
    assert_eq!(claimed_units(&db, "pair").await, 4);
}
