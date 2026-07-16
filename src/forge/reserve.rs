//! Atomic claiming of a bounded resource — FORGE's contention primitive.
//!
//! A *resource* is anything with a finite supply that many actors race to
//! take a piece of: tickets in a drop, units of stock, seats on a flight,
//! licence seats, appointment slots, redemptions of a promo code, tokens in
//! a quota. A *reservation* is one actor's successful claim on some of it.
//!
//! The whole module exists to make one sentence true under concurrency and
//! process death:
//!
//! > **The supply never goes negative, never oversells, and a retried
//! > request never claims twice.**
//!
//! That is a property, not a feature of any one app. Nothing here knows what
//! a ticket is.
//!
//! ## Who touches the lock
//!
//! SQLite's write lock is exclusive and database-wide: everything that takes it
//! stands in one queue. The naive shape takes it for *every* caller, including
//! the ones about to be told "sold out" — who then write nothing. In a drop
//! with 10,000 buyers and 500 tickets that is 9,500 exclusive locks acquired to
//! accomplish nothing, and **the losers become the load**.
//!
//! So [`reserve`](Reservations::reserve) answers what it can from lock-free WAL
//! reads (which run in parallel and block nobody) and takes the lock only for
//! callers who could plausibly win. The fast paths decide nothing: the
//! authoritative verdict is always the conditional `UPDATE` inside the
//! transaction. See that method's docs for why each is correct.
//!
//! ## How the invariant is held
//!
//! Three layers, deliberately redundant — the invariant is the product, so
//! it does not rest on any single line being right:
//!
//! 1. **The conditional decrement.** `UPDATE … SET remaining = remaining - n
//!    WHERE key = ? AND remaining >= n` is a single atomic statement. Rows
//!    affected is the verdict: `1` won, `0` lost. No read-then-write race
//!    exists, because there is no read.
//! 2. **The transaction.** The decrement and the reservation insert commit as
//!    one unit through [`DataSubstrate::begin`] (`BEGIN IMMEDIATE` on libSQL —
//!    the write lock is taken up front, so concurrent claims serialise
//!    instead of racing a deferred lock upgrade into `SQLITE_BUSY`). A crash
//!    between the two cannot leave supply consumed with no claim recorded, or
//!    a claim recorded against supply that was never taken.
//! 3. **The schema.** `CHECK (remaining >= 0 AND remaining <= capacity)` means
//!    the *store itself* refuses to represent an oversold or over-released
//!    resource. If layers 1 and 2 were both wrong, the write still fails loudly
//!    rather than silently corrupting the count.
//!
//! ## Idempotency
//!
//! A caller may attach an [`idempotency_key`](ReserveRequest::idempotency_key).
//! Replaying a request with the same key returns the *original* reservation
//! ([`ReserveOutcome::Replayed`]) and takes no further supply. This is what
//! makes the crash story honest end to end: a client that never saw the
//! response can safely retry, and will not be charged twice. Reusing a key
//! with *different* parameters is a caller bug and fails loudly
//! ([`ReserveError::IdempotencyConflict`]) rather than guessing which one was
//! meant.
//!
//! ## Scope
//!
//! This is crash-**atomic**, not crash-**resumable**. A process killed
//! mid-transaction leaves the store consistent and loses the in-flight claim;
//! it does not resume a partially-run workflow on restart. Resumability
//! (an intent log) layers *over* this primitive and arrives separately — see
//! [`DataSubstrate`]'s docs.
//!
//! The [`SCHEMA`] below is hand-authored. Once escape analysis (Pillar 1)
//! lands, a schema of this shape is what the compiler *emits* for a bounded
//! collection — this module is the runtime half that will consume it, not a
//! permanent hand-written table.

use thiserror::Error;

use crate::forge::substrate::{DataSubstrate, Transaction};
use crate::forge::value::{SqlValue, SubstrateError};

/// DDL for the reservation tables. Idempotent — safe to apply on every boot.
///
/// The `CHECK` constraints are the third layer of the oversell invariant: the
/// store will not represent a resource with negative supply, or one released
/// beyond the capacity it started with.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS forge_resource (
    key       TEXT    PRIMARY KEY,
    capacity  INTEGER NOT NULL CHECK (capacity >= 0),
    remaining INTEGER NOT NULL CHECK (remaining >= 0 AND remaining <= capacity)
);

CREATE TABLE IF NOT EXISTS forge_reservation (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    resource_key    TEXT    NOT NULL REFERENCES forge_resource(key),
    holder          TEXT    NOT NULL,
    quantity        INTEGER NOT NULL CHECK (quantity > 0),
    idempotency_key TEXT    UNIQUE,
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE INDEX IF NOT EXISTS forge_reservation_resource
    ON forge_reservation (resource_key);
";

/// A request to claim some of a resource.
///
/// Built for the common case first: `ReserveRequest::new(resource, holder)`
/// claims exactly one unit with no idempotency. Reach for
/// [`quantity`](Self::quantity) and [`idempotency_key`](Self::idempotency_key)
/// when the call needs them.
#[derive(Debug, Clone)]
pub struct ReserveRequest<'a> {
    resource: &'a str,
    holder: &'a str,
    quantity: i64,
    idempotency_key: Option<&'a str>,
}

impl<'a> ReserveRequest<'a> {
    /// Claim one unit of `resource` for `holder`.
    ///
    /// `holder` is opaque to FORGE — a user id, a session, a cart, whatever
    /// the caller's identity model is.
    pub const fn new(resource: &'a str, holder: &'a str) -> Self {
        Self {
            resource,
            holder,
            quantity: 1,
            idempotency_key: None,
        }
    }

    /// Claim `quantity` units instead of one. Must be positive; a
    /// non-positive quantity is rejected as [`ReserveError::InvalidQuantity`]
    /// rather than silently treated as a no-op.
    #[must_use]
    pub const fn quantity(mut self, quantity: i64) -> Self {
        self.quantity = quantity;
        self
    }

    /// Make the claim replay-safe. A second call with the same key returns the
    /// original reservation and takes no further supply.
    #[must_use]
    pub const fn idempotency_key(mut self, key: &'a str) -> Self {
        self.idempotency_key = Some(key);
        self
    }
}

/// The verdict of a [`Reservations::reserve`] call.
///
/// [`Reserved`](Self::Reserved) and [`Replayed`](Self::Replayed) are both
/// success — the caller holds the reservation either way. They are distinct so
/// a caller can tell a fresh claim from a retry (for metrics, or to skip
/// re-sending a receipt) without a second query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// The claim succeeded and supply was taken.
    Reserved {
        /// Identity of the new reservation.
        reservation_id: i64,
        /// Supply left after this claim.
        remaining: i64,
    },
    /// An earlier claim with the same idempotency key already exists; no
    /// further supply was taken.
    Replayed {
        /// Identity of the *original* reservation.
        reservation_id: i64,
        /// Supply left (unchanged by this call).
        remaining: i64,
    },
    /// Not enough supply. The resource exists; it just cannot cover the
    /// request.
    Exhausted {
        /// Supply left — may be non-zero if the request asked for more than
        /// remains.
        remaining: i64,
    },
}

/// The verdict of a [`Reservations::release`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// The reservation was released and its units returned to supply.
    Released {
        /// Supply left after the return.
        remaining: i64,
    },
    /// No such reservation. Releasing an already-released reservation lands
    /// here, which makes release naturally safe to retry.
    NotFound,
}

/// What was stored versus what a replay asked for, when an idempotency key is
/// reused for a different request.
///
/// Carried behind a [`Box`] in [`ReserveError::IdempotencyConflict`]: it is
/// seven fields wide and only ever built on a caller bug, so inlining it would
/// tax the size of every [`Result`] in this module — including the success path
/// that runs on every claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdempotencyConflict {
    /// The reused key.
    pub key: String,
    /// Resource the stored reservation was against.
    pub stored_resource: String,
    /// Holder of the stored reservation.
    pub stored_holder: String,
    /// Quantity of the stored reservation.
    pub stored_quantity: i64,
    /// Resource the replay asked for.
    pub requested_resource: String,
    /// Holder the replay asked for.
    pub requested_holder: String,
    /// Quantity the replay asked for.
    pub requested_quantity: i64,
}

impl std::fmt::Display for IdempotencyConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "idempotency key {:?} was already used for a different reservation \
             (stored: resource={:?} holder={:?} quantity={}; \
             requested: resource={:?} holder={:?} quantity={})",
            self.key,
            self.stored_resource,
            self.stored_holder,
            self.stored_quantity,
            self.requested_resource,
            self.requested_holder,
            self.requested_quantity,
        )
    }
}

/// Everything that can go wrong claiming a resource.
///
/// Per the engine's loud-error doctrine, each variant is a case the caller
/// should know about, not one to paper over: there is no "it probably worked"
/// path out of this type.
#[derive(Debug, Error)]
pub enum ReserveError {
    /// The storage plane failed underneath us.
    #[error(transparent)]
    Substrate(#[from] SubstrateError),

    /// No resource is defined under this key. Distinct from
    /// [`ReserveOutcome::Exhausted`] on purpose: "sold out" and "you asked for
    /// something that does not exist" are different bugs.
    #[error("unknown resource: {0}")]
    UnknownResource(String),

    /// Quantity was zero or negative.
    #[error("quantity must be positive, got {0}")]
    InvalidQuantity(i64),

    /// An idempotency key was reused for a *different* request. Honouring
    /// either interpretation would be a guess, so this fails loudly.
    #[error("{0}")]
    IdempotencyConflict(Box<IdempotencyConflict>),

    /// The insert reported success but returned no id — a substrate that does
    /// not honour `RETURNING`. Surfaced rather than fabricating an id.
    #[error("substrate returned no id for the inserted reservation")]
    MissingReservationId,
}

/// Convenience alias for reservation results.
pub type Result<T> = std::result::Result<T, ReserveError>;

/// A prior reservation found by idempotency key.
#[derive(Debug)]
struct PriorReservation {
    id: i64,
    resource_key: String,
    holder: String,
    quantity: i64,
}

/// Bounded-resource claiming over any [`DataSubstrate`].
///
/// Borrows the substrate rather than owning it: the serve path holds one
/// substrate behind an `Arc` for the process lifetime and this is a thin,
/// cheap view onto it, constructed per call site.
pub struct Reservations<'a> {
    substrate: &'a dyn DataSubstrate,
}

impl<'a> Reservations<'a> {
    /// View `substrate` through the reservation primitive.
    pub fn new(substrate: &'a dyn DataSubstrate) -> Self {
        Self { substrate }
    }

    /// Apply [`SCHEMA`]. Idempotent; safe on every boot.
    ///
    /// # Errors
    /// [`ReserveError::Substrate`] if the DDL cannot be applied.
    pub async fn migrate(&self) -> Result<()> {
        self.substrate.migrate(SCHEMA).await?;
        Ok(())
    }

    /// Declare a resource with `capacity` units, all initially available.
    ///
    /// Create-if-absent: re-declaring an existing key is a no-op and does
    /// **not** resize it. Changing the capacity of a live resource is a
    /// separate concern with its own semantics (what happens to claims already
    /// beyond the new capacity?) and is deliberately not folded in here.
    ///
    /// # Errors
    /// [`ReserveError::InvalidQuantity`] if `capacity` is negative;
    /// [`ReserveError::Substrate`] on a storage failure.
    pub async fn define(&self, resource: &str, capacity: i64) -> Result<()> {
        if capacity < 0 {
            return Err(ReserveError::InvalidQuantity(capacity));
        }
        self.substrate
            .execute(
                "INSERT INTO forge_resource (key, capacity, remaining) VALUES (?1, ?2, ?2)
                 ON CONFLICT (key) DO NOTHING",
                &[resource.into(), SqlValue::Integer(capacity)],
            )
            .await?;
        Ok(())
    }

    /// Supply currently available, or `None` if no such resource is defined.
    ///
    /// # Errors
    /// [`ReserveError::Substrate`] on a storage failure.
    pub async fn remaining(&self, resource: &str) -> Result<Option<i64>> {
        let rows = self
            .substrate
            .query(
                "SELECT remaining FROM forge_resource WHERE key = ?1",
                &[resource.into()],
            )
            .await?;
        Ok(first_i64(&rows))
    }

    /// Claim some of a resource, atomically.
    ///
    /// Wins ([`ReserveOutcome::Reserved`]) and losses
    /// ([`ReserveOutcome::Exhausted`]) are both ordinary outcomes, not errors —
    /// "sold out" is the system working. Errors are reserved for the caller
    /// being wrong or the substrate failing.
    ///
    /// ## Two lock-free fast paths, and why they are correct
    ///
    /// The write lock is exclusive and database-wide, so anything that takes it
    /// stands in one queue. The naive shape took it for *every* caller —
    /// including the ones about to be told "sold out", who then wrote nothing.
    /// In a drop with 10,000 buyers and 500 tickets that is 9,500 exclusive
    /// locks acquired to accomplish nothing, and the losers become the load.
    ///
    /// So two answers are given without ever calling
    /// [`begin`](DataSubstrate::begin) — WAL reads take no lock and run in
    /// parallel:
    ///
    /// 1. **A replay.** Checked *first*, because a retry must be honoured even
    ///    when supply is gone: the buyer who took the last unit and never saw
    ///    the response is owed their reservation, not a "sold out".
    /// 2. **A claim supply cannot cover.** A fresh read of `remaining` that
    ///    cannot cover the request is a truthful verdict *for a request
    ///    arriving at that instant* — which is exactly what linearizability
    ///    asks. It is never stale in a way that matters: nothing is cached, and
    ///    a claim that loses this check would equally have lost the race a
    ///    microsecond later. Under [`release`](Self::release) supply can rise
    ///    again, so this answers "sold out *now*", not "sold out forever" —
    ///    the next caller reads the new value.
    ///
    /// Neither fast path decides anything. **The authoritative verdict is still
    /// the conditional `UPDATE` inside the transaction**; these only spare the
    /// lock from callers who provably have no business holding it. A racer that
    /// slips past the pre-check simply loses properly, inside the lock, as
    /// before.
    ///
    /// # Errors
    /// [`ReserveError::InvalidQuantity`] for a non-positive quantity;
    /// [`ReserveError::UnknownResource`] if `resource` is not defined;
    /// [`ReserveError::IdempotencyConflict`] if the key was reused for a
    /// different request; [`ReserveError::Substrate`] on a storage failure.
    pub async fn reserve(&self, req: &ReserveRequest<'_>) -> Result<ReserveOutcome> {
        if req.quantity <= 0 {
            return Err(ReserveError::InvalidQuantity(req.quantity));
        }

        // Fast path 1 — a replay needs no lock, and must outrank the supply
        // check so a retry is never told "sold out".
        if let Some(key) = req.idempotency_key {
            if let Some(prior) = self.find_prior(key).await? {
                ensure_matches(&prior, req, key)?;
                let remaining = self
                    .remaining(&prior.resource_key)
                    .await?
                    .ok_or_else(|| ReserveError::UnknownResource(prior.resource_key.clone()))?;
                return Ok(ReserveOutcome::Replayed {
                    reservation_id: prior.id,
                    remaining,
                });
            }
        }

        // Fast path 2 — a claim supply cannot cover never touches the lock.
        match self.remaining(req.resource).await? {
            None => return Err(ReserveError::UnknownResource(req.resource.to_owned())),
            Some(remaining) if remaining < req.quantity => {
                return Ok(ReserveOutcome::Exhausted { remaining })
            }
            Some(_) => {}
        }

        // Slow path: this caller could plausibly win, so it earns the lock.
        let tx = self.substrate.begin().await?;

        // Re-check under the lock: a racer may have committed this key between
        // fast path 1 and here. The lock-free read was a filter; this is truth.
        if let Some(key) = req.idempotency_key {
            if let Some(prior) = find_by_idempotency_key(&*tx, key).await? {
                let outcome = replay(&*tx, req, key, prior).await;
                // Read-only either way; nothing to commit.
                tx.rollback().await?;
                return outcome;
            }
        }

        // The verdict *and* the new supply, in one atomic statement.
        // `remaining >= n` is the whole invariant: no read precedes it, so
        // nothing can race between the check and the decrement. `RETURNING`
        // means no second round trip while holding an exclusive lock — rows
        // present is a win, rows absent is a loss.
        let updated = tx
            .query(
                "UPDATE forge_resource SET remaining = remaining - ?1
                 WHERE key = ?2 AND remaining >= ?1
                 RETURNING remaining",
                &[SqlValue::Integer(req.quantity), req.resource.into()],
            )
            .await?;

        let Some(remaining) = first_i64(&updated) else {
            // Lost at the boundary — supply went while we queued.
            let remaining = remaining_in_tx(&*tx, req.resource).await?;
            tx.rollback().await?;
            let remaining =
                remaining.ok_or_else(|| ReserveError::UnknownResource(req.resource.to_owned()))?;
            return Ok(ReserveOutcome::Exhausted { remaining });
        };

        let reservation_id = insert_reservation(&*tx, req).await?;

        // Supply taken and claim recorded land together or not at all.
        tx.commit().await?;

        Ok(ReserveOutcome::Reserved {
            reservation_id,
            remaining,
        })
    }

    /// Look up a reservation by idempotency key **without** taking the write
    /// lock. A filter for the fast path; the in-transaction lookup remains the
    /// authority.
    async fn find_prior(&self, key: &str) -> Result<Option<PriorReservation>> {
        let rows = self.substrate.query(PRIOR_BY_KEY_SQL, &[key.into()]).await?;
        Ok(parse_prior(&rows))
    }

    /// Release a reservation, returning its units to supply.
    ///
    /// Safe to retry: releasing an unknown or already-released reservation
    /// reports [`ReleaseOutcome::NotFound`] rather than failing.
    ///
    /// # Errors
    /// [`ReserveError::Substrate`] on a storage failure — including the
    /// schema's `remaining <= capacity` check, which would reject a release
    /// that tried to return more than was ever taken.
    pub async fn release(&self, reservation_id: i64) -> Result<ReleaseOutcome> {
        let tx = self.substrate.begin().await?;

        let rows = tx
            .query(
                "SELECT resource_key, quantity FROM forge_reservation WHERE id = ?1",
                &[SqlValue::Integer(reservation_id)],
            )
            .await?;

        let Some((resource_key, quantity)) = rows.rows.first().and_then(|row| {
            let key = row.get(0)?.as_str()?.to_owned();
            let qty = row.get(1)?.as_i64()?;
            Some((key, qty))
        }) else {
            tx.rollback().await?;
            return Ok(ReleaseOutcome::NotFound);
        };

        tx.execute(
            "DELETE FROM forge_reservation WHERE id = ?1",
            &[SqlValue::Integer(reservation_id)],
        )
        .await?;
        tx.execute(
            "UPDATE forge_resource SET remaining = remaining + ?1 WHERE key = ?2",
            &[SqlValue::Integer(quantity), resource_key.as_str().into()],
        )
        .await?;

        let remaining = remaining_in_tx(&*tx, &resource_key)
            .await?
            .ok_or_else(|| ReserveError::UnknownResource(resource_key.clone()))?;

        tx.commit().await?;

        Ok(ReleaseOutcome::Released { remaining })
    }
}

/// Answer a replayed request: confirm it matches the stored claim, then report
/// the original reservation. Mismatches are a caller bug and fail loudly.
async fn replay(
    tx: &dyn Transaction,
    req: &ReserveRequest<'_>,
    key: &str,
    prior: PriorReservation,
) -> Result<ReserveOutcome> {
    ensure_matches(&prior, req, key)?;

    let remaining = remaining_in_tx(tx, &prior.resource_key)
        .await?
        .ok_or_else(|| ReserveError::UnknownResource(prior.resource_key.clone()))?;

    Ok(ReserveOutcome::Replayed {
        reservation_id: prior.id,
        remaining,
    })
}

/// Confirm a replay asks for what the stored claim actually holds.
///
/// Shared by the lock-free and in-transaction replay paths so the two can never
/// disagree about what counts as the same request.
///
/// # Errors
/// [`ReserveError::IdempotencyConflict`] when the key was reused for a
/// different request — honouring either reading would be a guess.
fn ensure_matches(prior: &PriorReservation, req: &ReserveRequest<'_>, key: &str) -> Result<()> {
    let stored = (
        prior.resource_key.as_str(),
        prior.holder.as_str(),
        prior.quantity,
    );
    let requested = (req.resource, req.holder, req.quantity);

    if stored != requested {
        return Err(ReserveError::IdempotencyConflict(Box::new(
            IdempotencyConflict {
                key: key.to_owned(),
                stored_resource: prior.resource_key.clone(),
                stored_holder: prior.holder.clone(),
                stored_quantity: prior.quantity,
                requested_resource: req.resource.to_owned(),
                requested_holder: req.holder.to_owned(),
                requested_quantity: req.quantity,
            },
        )));
    }
    Ok(())
}

/// The reservation-by-idempotency-key lookup. One string, used by both the
/// lock-free filter and the authoritative in-transaction check.
const PRIOR_BY_KEY_SQL: &str = "SELECT id, resource_key, holder, quantity FROM forge_reservation
     WHERE idempotency_key = ?1";

/// Lift a [`PRIOR_BY_KEY_SQL`] result set into a [`PriorReservation`].
fn parse_prior(rows: &crate::forge::value::Rows) -> Option<PriorReservation> {
    let row = rows.rows.first()?;
    Some(PriorReservation {
        id: row.get(0)?.as_i64()?,
        resource_key: row.get(1)?.as_str()?.to_owned(),
        holder: row.get(2)?.as_str()?.to_owned(),
        quantity: row.get(3)?.as_i64()?,
    })
}

/// Look up a reservation by idempotency key within the transaction — the
/// authority, as opposed to [`Reservations::find_prior`]'s lock-free filter.
async fn find_by_idempotency_key(
    tx: &dyn Transaction,
    key: &str,
) -> Result<Option<PriorReservation>> {
    let rows = tx.query(PRIOR_BY_KEY_SQL, &[key.into()]).await?;
    Ok(parse_prior(&rows))
}

/// Record the claim, returning its id. Uses `RETURNING` rather than
/// `last_insert_rowid()` so the id belongs to *this* statement and not to
/// whatever else the connection last did.
async fn insert_reservation(tx: &dyn Transaction, req: &ReserveRequest<'_>) -> Result<i64> {
    let idempotency = req.idempotency_key.map_or(SqlValue::Null, Into::into);

    let rows = tx
        .query(
            "INSERT INTO forge_reservation (resource_key, holder, quantity, idempotency_key)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            &[
                req.resource.into(),
                req.holder.into(),
                SqlValue::Integer(req.quantity),
                idempotency,
            ],
        )
        .await?;

    first_i64(&rows).ok_or(ReserveError::MissingReservationId)
}

/// Supply for `resource` as seen inside the transaction.
async fn remaining_in_tx(tx: &dyn Transaction, resource: &str) -> Result<Option<i64>> {
    let rows = tx
        .query(
            "SELECT remaining FROM forge_resource WHERE key = ?1",
            &[resource.into()],
        )
        .await?;
    Ok(first_i64(&rows))
}

/// First column of the first row as an integer, if there is one.
fn first_i64(rows: &crate::forge::value::Rows) -> Option<i64> {
    rows.rows.first()?.get(0)?.as_i64()
}

#[cfg(all(test, feature = "forge"))]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;
    use crate::forge::libsql::LibSqlSubstrate;

    /// A migrated, in-memory substrate with one resource defined.
    async fn with_resource(resource: &str, capacity: i64) -> LibSqlSubstrate {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let r = Reservations::new(&db);
        r.migrate().await.unwrap();
        r.define(resource, capacity).await.unwrap();
        db
    }

    /// The headline property: supply runs out exactly once, at exactly the
    /// right place, and never goes negative.
    #[tokio::test]
    async fn claims_until_capacity_then_reports_exhausted() {
        let db = with_resource("ticket", 3).await;
        let r = Reservations::new(&db);

        let mut reserved = 0;
        let mut exhausted = 0;
        for i in 0..5 {
            let holder = format!("buyer{i}");
            match r.reserve(&ReserveRequest::new("ticket", &holder)).await.unwrap() {
                ReserveOutcome::Reserved { .. } => reserved += 1,
                ReserveOutcome::Exhausted { remaining } => {
                    exhausted += 1;
                    assert_eq!(remaining, 0);
                }
                other => panic!("unexpected outcome: {other:?}"),
            }
        }

        assert_eq!(reserved, 3, "exactly capacity claims may win");
        assert_eq!(exhausted, 2);
        assert_eq!(r.remaining("ticket").await.unwrap(), Some(0));
    }

    /// A claim larger than the remaining supply loses without partially
    /// consuming it — `remaining` is untouched, not driven to zero.
    #[tokio::test]
    async fn oversized_claim_is_rejected_whole() {
        let db = with_resource("seat", 5).await;
        let r = Reservations::new(&db);

        let outcome = r
            .reserve(&ReserveRequest::new("seat", "group").quantity(6))
            .await
            .unwrap();
        assert_eq!(outcome, ReserveOutcome::Exhausted { remaining: 5 });
        assert_eq!(r.remaining("seat").await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn multi_unit_claim_takes_exactly_its_quantity() {
        let db = with_resource("stock", 10).await;
        let r = Reservations::new(&db);

        let outcome = r
            .reserve(&ReserveRequest::new("stock", "cart").quantity(3))
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            ReserveOutcome::Reserved { remaining: 7, .. }
        ));
        assert_eq!(r.remaining("stock").await.unwrap(), Some(7));
    }

    /// The crash-retry story: a client that never saw the response retries and
    /// is not charged twice.
    #[tokio::test]
    async fn replayed_request_returns_the_original_and_takes_no_supply() {
        let db = with_resource("ticket", 2).await;
        let r = Reservations::new(&db);
        let req = ReserveRequest::new("ticket", "ada").idempotency_key("req-1");

        let first = r.reserve(&req).await.unwrap();
        let ReserveOutcome::Reserved {
            reservation_id,
            remaining,
        } = first
        else {
            panic!("first claim should reserve, got {first:?}");
        };
        assert_eq!(remaining, 1);

        let second = r.reserve(&req).await.unwrap();
        assert_eq!(
            second,
            ReserveOutcome::Replayed {
                reservation_id,
                remaining: 1
            },
            "a replay returns the original claim and takes nothing further"
        );
        assert_eq!(r.remaining("ticket").await.unwrap(), Some(1));
    }

    /// Reusing a key for a different request is a caller bug — guessing which
    /// one was meant would be silently wrong.
    #[tokio::test]
    async fn idempotency_key_reused_for_a_different_request_fails_loudly() {
        let db = with_resource("ticket", 5).await;
        let r = Reservations::new(&db);

        r.reserve(&ReserveRequest::new("ticket", "ada").idempotency_key("k"))
            .await
            .unwrap();

        let err = r
            .reserve(&ReserveRequest::new("ticket", "alan").idempotency_key("k"))
            .await
            .expect_err("same key, different holder must not be honoured");

        let ReserveError::IdempotencyConflict(conflict) = err else {
            panic!("expected an idempotency conflict, got {err:?}");
        };
        assert_eq!(conflict.stored_holder, "ada");
        assert_eq!(conflict.requested_holder, "alan");
        // The conflicting call took nothing.
        assert_eq!(r.remaining("ticket").await.unwrap(), Some(4));
    }

    #[tokio::test]
    async fn unknown_resource_is_an_error_not_a_sold_out() {
        let db = with_resource("ticket", 1).await;
        let r = Reservations::new(&db);

        let err = r
            .reserve(&ReserveRequest::new("nope", "ada"))
            .await
            .expect_err("claiming an undefined resource is a bug, not sold out");
        assert!(matches!(err, ReserveError::UnknownResource(k) if k == "nope"));
    }

    #[tokio::test]
    async fn non_positive_quantity_is_rejected() {
        let db = with_resource("ticket", 5).await;
        let r = Reservations::new(&db);

        for bad in [0, -1] {
            let err = r
                .reserve(&ReserveRequest::new("ticket", "ada").quantity(bad))
                .await
                .expect_err("non-positive quantity must be rejected");
            assert!(matches!(err, ReserveError::InvalidQuantity(q) if q == bad));
        }
        assert_eq!(r.remaining("ticket").await.unwrap(), Some(5));
    }

    #[tokio::test]
    async fn release_returns_supply_and_is_retry_safe() {
        let db = with_resource("seat", 1).await;
        let r = Reservations::new(&db);

        let ReserveOutcome::Reserved { reservation_id, .. } =
            r.reserve(&ReserveRequest::new("seat", "ada")).await.unwrap()
        else {
            panic!("expected a reservation");
        };
        assert_eq!(r.remaining("seat").await.unwrap(), Some(0));

        assert_eq!(
            r.release(reservation_id).await.unwrap(),
            ReleaseOutcome::Released { remaining: 1 }
        );
        assert_eq!(r.remaining("seat").await.unwrap(), Some(1));

        // Retrying the release neither errors nor double-credits.
        assert_eq!(
            r.release(reservation_id).await.unwrap(),
            ReleaseOutcome::NotFound
        );
        assert_eq!(r.remaining("seat").await.unwrap(), Some(1));

        // The freed unit is genuinely claimable again.
        assert!(matches!(
            r.reserve(&ReserveRequest::new("seat", "alan")).await.unwrap(),
            ReserveOutcome::Reserved { .. }
        ));
    }

    #[tokio::test]
    async fn define_is_idempotent_and_does_not_resize() {
        let db = with_resource("ticket", 3).await;
        let r = Reservations::new(&db);

        r.reserve(&ReserveRequest::new("ticket", "ada")).await.unwrap();
        r.define("ticket", 99).await.unwrap();

        assert_eq!(
            r.remaining("ticket").await.unwrap(),
            Some(2),
            "re-defining must not resurrect consumed supply"
        );
    }

    /// Layer 3 of the invariant: even a hand-written statement that bypasses
    /// `reserve()` cannot drive supply negative — the store itself refuses.
    #[tokio::test]
    async fn schema_refuses_to_represent_negative_supply() {
        let db = with_resource("ticket", 1).await;

        let err = db
            .execute(
                "UPDATE forge_resource SET remaining = -1 WHERE key = ?1",
                &["ticket".into()],
            )
            .await
            .expect_err("CHECK (remaining >= 0) must reject this");
        assert!(matches!(err, SubstrateError::Query(_)));
    }

    /// Concurrent claimants on one last unit: exactly one wins, and no two
    /// winners ever share a reservation id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_claimants_never_oversell() {
        let db = Arc::new(with_resource("ticket", 1).await);

        let mut handles = Vec::new();
        for i in 0..16 {
            let db = Arc::clone(&db);
            handles.push(tokio::spawn(async move {
                let r = Reservations::new(db.as_ref());
                let holder = format!("buyer{i}");
                r.reserve(&ReserveRequest::new("ticket", &holder)).await
            }));
        }

        let mut winners = HashSet::new();
        let mut exhausted = 0;
        let mut refused = Vec::new();
        for handle in handles {
            match handle.await.unwrap() {
                Ok(ReserveOutcome::Reserved { reservation_id, .. }) => {
                    assert!(winners.insert(reservation_id), "duplicate reservation id");
                }
                Ok(ReserveOutcome::Exhausted { .. }) => exhausted += 1,
                Ok(other) => panic!("unexpected outcome: {other:?}"),
                // A contended substrate refusing a claim would never oversell,
                // but it *would* show a buyer an error instead of an honest
                // "sold out". Track it: this is the signal for whether the
                // single-connection substrate needs a pool.
                Err(ReserveError::Substrate(e)) => refused.push(e.to_string()),
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        assert_eq!(winners.len(), 1, "exactly one claimant may win one unit");
        assert!(
            refused.is_empty(),
            "every loser must lose cleanly as Exhausted, not error: {refused:?}"
        );
        assert_eq!(exhausted, 15, "the other 15 claimants see an honest sold-out");
        assert_eq!(db.query("SELECT remaining FROM forge_resource", &[]).await.unwrap().rows[0]
            .get(0)
            .and_then(SqlValue::as_i64), Some(0));
    }
}
