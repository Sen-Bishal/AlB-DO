//! SHUTTER · the cell — one rate limit, in one `u64`.
//!
//! The algorithm is **GCRA** (Generic Cell Rate Algorithm), the virtual
//! scheduling formulation of a leaky bucket that ATM traffic shaping standardised
//! and that the `governor` crate popularised in Rust. It is chosen over the
//! obvious alternatives for reasons that are all mechanical:
//!
//! | approach | state per key | exact? | burst at window edge |
//! |---|---|---|---|
//! | fixed window | counter + window start | no | **2× the limit** |
//! | sliding window log | one timestamp *per request* | yes | none |
//! | sliding window counter | two counters + start | approximate | none |
//! | token bucket | tokens + last-refill | yes | none |
//! | **GCRA** | **one timestamp** | **yes** | none |
//!
//! GCRA is exactly equivalent to a token bucket, and stores half the state: a
//! single *theoretical arrival time* (TAT). There is no token count to refill,
//! so there is no periodic sweep and no background task — the passage of time
//! **is** the refill. One `u64` is also small enough to be an `AtomicU64`, which
//! is what makes [`Cell`] lock-free rather than a mutex per key.
//!
//! ## The rule
//!
//! With emission interval `T` (one unit of quota per `T` nanoseconds) and
//! tolerance `τ = burst × T`, a request of weight `w` arriving at `now`:
//!
//! ```text
//! wait = saturating(TAT − now)          // how far the cell is "in debt"
//! allow  ⟺  wait + w·T ≤ τ
//! on allow:  TAT ← max(TAT, now) + w·T
//! ```
//!
//! Two properties fall out that are worth more than they look:
//!
//! 🔑 **Rejection is free.** `TAT` only advances on admission, so a refused
//! request costs the caller nothing. A limiter that charged on rejection would
//! let an attacker hold a victim out indefinitely by spending refusals.
//!
//! 🔑 **`Retry-After` is exact, not a guess.** The rejection condition inverts
//! to `now' ≥ TAT + w·T − τ`, so the cell knows precisely when the request would
//! have succeeded. Most limiters either omit the header or round a window
//! boundary; this one can be trusted, which is what makes a well-behaved client
//! back off correctly instead of polling.
//!
//! ## A weight larger than the burst can never be admitted
//!
//! If `w·T > τ` the condition is unsatisfiable at any `now` — the request is
//! refused forever, and the `Retry-After` grows without bound. That is a
//! configuration bug rather than a runtime condition, so [`Quota`] refuses to
//! build one: see [`Quota::max_weight`] and [`QuotaError::WeightExceedsBurst`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Nanoseconds on a monotonic timeline whose origin is arbitrary but fixed.
///
/// A `u64` of nanoseconds spans ~584 years, so wrap-around is not a case that
/// needs handling. **Monotonic, never wall-clock**: a limiter keyed on
/// `SystemTime` forgives every accumulated limit the moment NTP steps the clock
/// backwards, which is both a correctness bug and an exploitable one on a host
/// an attacker can nudge.
pub type Nanos = u64;

/// A rate, expressed the way a human states one and stored the way GCRA needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quota {
    /// Emission interval: nanoseconds per unit of quota.
    emission_interval: Nanos,
    /// `burst × emission_interval` — how far into debt a cell may go.
    tolerance: Nanos,
    /// Burst size, kept for reporting (`RateLimit-Limit`) and for the
    /// weight check.
    burst: u32,
}

/// Why a quota could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaError {
    /// A rate of zero admits nothing, which is a deny rule rather than a limit —
    /// and one that reports a nonsense `Retry-After` forever.
    ZeroRate,
    /// A burst of zero admits nothing, for the same reason.
    ZeroBurst,
    /// The period is too long to express, or zero.
    UnusablePeriod,
    /// An operation costs more than the whole burst, so it can never be
    /// admitted at any time. Caught here rather than surfacing as a permanent,
    /// silent 429 on one endpoint.
    WeightExceedsBurst {
        /// What the operation costs.
        weight: u32,
        /// The burst it would have to fit inside.
        burst: u32,
    },
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ZeroRate => f.write_str(
                "a quota of zero per period admits nothing — express a closed door as a deny \
                 rule, not as a rate limit",
            ),
            Self::ZeroBurst => f.write_str("a burst of zero admits nothing"),
            Self::UnusablePeriod => {
                f.write_str("the period is zero or too long to express in nanoseconds")
            }
            Self::WeightExceedsBurst { weight, burst } => write!(
                f,
                "an operation weighing {weight} can never be admitted by a limit whose burst is \
                 {burst} — it would be refused at every instant, with a Retry-After that only \
                 grows. Raise the burst to at least {weight}, or lower the operation's cost"
            ),
        }
    }
}

impl std::error::Error for QuotaError {}

impl Quota {
    /// `rate` units per `period`, allowing a burst of `rate` units.
    ///
    /// The default burst equals the rate because that is what "60 per minute"
    /// means to the person who wrote it: sixty available now, refilling over the
    /// minute. A burst smaller than the rate would enforce a cadence nobody
    /// asked for.
    ///
    /// # Errors
    /// [`QuotaError`] for a degenerate rate or period.
    pub fn per(rate: u32, period: Duration) -> Result<Self, QuotaError> {
        Self::with_burst(rate, period, rate)
    }

    /// `rate` units per `period`, with an explicit burst.
    ///
    /// # Errors
    /// [`QuotaError`] for a degenerate rate, burst or period.
    pub fn with_burst(rate: u32, period: Duration, burst: u32) -> Result<Self, QuotaError> {
        if rate == 0 {
            return Err(QuotaError::ZeroRate);
        }
        if burst == 0 {
            return Err(QuotaError::ZeroBurst);
        }
        let period_nanos = u64::try_from(period.as_nanos()).map_err(|_| QuotaError::UnusablePeriod)?;
        if period_nanos == 0 {
            return Err(QuotaError::UnusablePeriod);
        }
        let emission_interval = period_nanos / u64::from(rate);
        if emission_interval == 0 {
            // More units per period than nanoseconds in it. Nothing is being
            // limited, and the arithmetic below would divide by zero.
            return Err(QuotaError::UnusablePeriod);
        }
        Ok(Self {
            emission_interval,
            tolerance: emission_interval.saturating_mul(u64::from(burst)),
            burst,
        })
    }

    /// The heaviest single operation this quota can ever admit.
    #[must_use]
    pub const fn max_weight(&self) -> u32 {
        self.burst
    }

    /// Burst size, for `RateLimit-Limit`.
    #[must_use]
    pub const fn burst(&self) -> u32 {
        self.burst
    }

    /// Refuse a weight this quota could never admit.
    ///
    /// Call sites that derive a weight (rather than taking a literal) should run
    /// this at build time — a permanently-refused endpoint is a bug that looks
    /// exactly like heavy traffic.
    ///
    /// # Errors
    /// [`QuotaError::WeightExceedsBurst`].
    pub const fn check_weight(&self, weight: u32) -> Result<(), QuotaError> {
        if weight > self.burst {
            return Err(QuotaError::WeightExceedsBurst {
                weight,
                burst: self.burst,
            });
        }
        Ok(())
    }
}

/// What a cell decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Admitted, and the quota was charged.
    Admit {
        /// Whole units still available immediately after this admission.
        remaining: u32,
        /// When the cell will be fully replenished.
        reset_after: Duration,
    },
    /// Refused, and **nothing was charged**.
    Refuse {
        /// Exactly how long until this same request would be admitted. Not a
        /// rounded window boundary — the instant the condition flips.
        retry_after: Duration,
        /// When the cell will be fully replenished.
        reset_after: Duration,
    },
}

impl Decision {
    /// Whether the request may proceed.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admit { .. })
    }

    /// Units still available, zero when refused.
    #[must_use]
    pub const fn remaining(&self) -> u32 {
        match self {
            Self::Admit { remaining, .. } => *remaining,
            Self::Refuse { .. } => 0,
        }
    }

    /// When the cell returns to full, under either outcome.
    #[must_use]
    pub const fn reset_after(&self) -> Duration {
        match self {
            Self::Admit { reset_after, .. } | Self::Refuse { reset_after, .. } => *reset_after,
        }
    }
}

/// One limit's state: a single theoretical arrival time.
///
/// Lock-free. The whole cell is one `AtomicU64` updated by a compare-exchange
/// loop, so concurrent callers on the same key never block each other and there
/// is no mutex to poison, no lock to hold across an `.await`, and no
/// possibility of a limiter deadlocking the request path it guards.
///
/// `Relaxed` ordering throughout is correct and not a shortcut: the TAT is the
/// only shared datum, it protects no other memory, and every operation on it is
/// a read-modify-write on that one word. There is nothing for an acquire or
/// release to order against.
#[derive(Debug)]
pub struct Cell {
    tat: AtomicU64,
}

impl Default for Cell {
    fn default() -> Self {
        Self::new()
    }
}

impl Cell {
    /// A fresh, fully-replenished cell.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tat: AtomicU64::new(0),
        }
    }

    /// A cell whose debt is already known — used when rebuilding state.
    #[must_use]
    pub const fn at(tat: Nanos) -> Self {
        Self {
            tat: AtomicU64::new(tat),
        }
    }

    /// Charge `weight` units against `quota` at time `now`.
    ///
    /// The whole algorithm. See the module docs for the rule and for why
    /// rejection is free.
    pub fn charge(&self, quota: &Quota, now: Nanos, weight: u32) -> Decision {
        let increment = quota
            .emission_interval
            .saturating_mul(u64::from(weight.max(1)));

        loop {
            let tat = self.tat.load(Ordering::Relaxed);
            // How far in the future the cell's next free slot already is. A cell
            // whose TAT is in the past is fully replenished, and saturating here
            // is what makes a stale cell indistinguishable from a fresh one —
            // the property the table's eviction depends on.
            let debt = tat.saturating_sub(now);

            if debt.saturating_add(increment) > quota.tolerance {
                // Refused. `tat` is NOT advanced: a refusal must not push the
                // caller further away from being served.
                let retry = debt
                    .saturating_add(increment)
                    .saturating_sub(quota.tolerance);
                return Decision::Refuse {
                    retry_after: Duration::from_nanos(retry),
                    reset_after: Duration::from_nanos(debt),
                };
            }

            let new_tat = tat.max(now).saturating_add(increment);
            if self
                .tat
                .compare_exchange_weak(tat, new_tat, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let new_debt = new_tat.saturating_sub(now);
                let spare = quota.tolerance.saturating_sub(new_debt);
                return Decision::Admit {
                    remaining: u32::try_from(spare / quota.emission_interval).unwrap_or(u32::MAX),
                    reset_after: Duration::from_nanos(new_debt),
                };
            }
            // Lost the race; another caller charged this cell. Re-read and retry
            // — the loop is bounded in practice by contention, not by liveness:
            // every failed iteration means some other caller made progress.
        }
    }

    /// Advance the cell by `weight` units **whether or not a charge would have
    /// been admitted**, and never past full exhaustion.
    ///
    /// 🔑 **The one place "rejection is free" is the wrong rule.** That property
    /// protects a caller from being pushed further from service by a request that
    /// never ran. A *surcharge* is the opposite case: the work already happened,
    /// and its real price only became knowable afterwards — a write's blast
    /// radius is not visible until the write has resolved which channels it
    /// touches. Put through [`Self::charge`], that price would be refused exactly
    /// when the cell was already exhausted, so the debt would vanish precisely in
    /// the case it exists to record. An attacker's most expensive writes would be
    /// the free ones.
    ///
    /// 🪤 **The clamp is on the increment, never on the result.** Bounding the
    /// resulting debt at `tolerance` is the obvious guard and it is exactly
    /// wrong: a cell at its ceiling is a caller who has spent everything, so
    /// clamping there drops the surcharge for precisely the caller who earned it,
    /// and the heaviest writes become the free ones again. Clamping the increment
    /// instead means any weight that fits inside the burst lands in full — which
    /// every derived weight does, `compress_fanout` being bounded at 32 — while a
    /// weight nobody could have checked in advance costs at most one whole
    /// budget. That is [`QuotaError::WeightExceedsBurst`]'s rule applied at
    /// runtime: debt can exceed full, but never without bound, so the worst case
    /// is waiting rather than a cell that refuses forever.
    ///
    /// Returns the cell's remaining debt, in nanoseconds.
    pub fn debit(&self, quota: &Quota, now: Nanos, weight: u32) -> Nanos {
        if weight == 0 {
            return self.tat.load(Ordering::Relaxed).saturating_sub(now);
        }
        let increment = quota
            .emission_interval
            .saturating_mul(u64::from(weight))
            .min(quota.tolerance);

        loop {
            let tat = self.tat.load(Ordering::Relaxed);
            let new_tat = tat.max(now).saturating_add(increment);
            if self
                .tat
                .compare_exchange_weak(tat, new_tat, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return new_tat.saturating_sub(now);
            }
        }
    }

    /// Would `charge` admit this, without actually charging it?
    ///
    /// Exists for the one case where charging first would be the vulnerability:
    /// a login must be able to ask "is this *account* already under attack?"
    /// without the asking itself counting against the account. Otherwise an
    /// attacker locks a victim out by making the victim's own limiter fire.
    #[must_use]
    pub fn peek(&self, quota: &Quota, now: Nanos, weight: u32) -> bool {
        let increment = quota
            .emission_interval
            .saturating_mul(u64::from(weight.max(1)));
        let debt = self.tat.load(Ordering::Relaxed).saturating_sub(now);
        debt.saturating_add(increment) <= quota.tolerance
    }

    /// Whether this cell is fully replenished at `now`, and therefore
    /// indistinguishable from a cell that does not exist.
    ///
    /// 🔑 **The eviction predicate, and the reason bounded memory costs nothing
    /// here.** Dropping a replenished cell loses no information: recreating it
    /// yields `TAT = 0`, which `saturating_sub` treats identically. Dropping an
    /// *indebted* cell would forgive accumulated debt — which is why the table
    /// evicts on this predicate and never on age or recency.
    #[must_use]
    pub fn is_replenished(&self, now: Nanos) -> bool {
        self.tat.load(Ordering::Relaxed) <= now
    }

    /// Current TAT, for diagnostics and tests.
    #[must_use]
    pub fn tat(&self) -> Nanos {
        self.tat.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    fn quota(rate: u32, burst: u32) -> Quota {
        Quota::with_burst(rate, Duration::from_secs(1), burst).expect("valid quota")
    }

    #[test]
    fn a_fresh_cell_admits_exactly_its_burst_then_refuses() {
        let quota = quota(10, 3);
        let cell = Cell::new();

        for i in 0..3 {
            assert!(
                cell.charge(&quota, 0, 1).is_admitted(),
                "burst unit {i} must be admitted"
            );
        }
        assert!(
            !cell.charge(&quota, 0, 1).is_admitted(),
            "the fourth request exceeds a burst of three"
        );
    }

    #[test]
    fn remaining_counts_down_across_the_burst() {
        let quota = quota(10, 4);
        let cell = Cell::new();
        let seen: Vec<u32> = (0..4)
            .map(|_| cell.charge(&quota, 0, 1).remaining())
            .collect();
        assert_eq!(seen, vec![3, 2, 1, 0]);
    }

    /// **Rejection is free.** A refused request must not advance the TAT, or an
    /// attacker could hold a victim out by spending refusals.
    #[test]
    fn a_refusal_does_not_charge_the_cell() {
        let quota = quota(10, 2);
        let cell = Cell::new();
        assert!(cell.charge(&quota, 0, 1).is_admitted());
        assert!(cell.charge(&quota, 0, 1).is_admitted());

        let before = cell.tat();
        for _ in 0..1_000 {
            assert!(!cell.charge(&quota, 0, 1).is_admitted());
        }
        assert_eq!(
            cell.tat(),
            before,
            "a thousand refusals moved the cell — the limiter can be pushed by the traffic it \
             is refusing"
        );
    }

    /// The surcharge case, and why it cannot go through `charge`. A cell that is
    /// already exhausted must still record what the last admitted request turned
    /// out to cost — otherwise the most expensive writes are the free ones.
    #[test]
    fn a_debit_lands_on_a_cell_that_would_have_refused_it() {
        let quota = quota(10, 2);
        let cell = Cell::new();
        assert!(cell.charge(&quota, 0, 2).is_admitted());
        assert!(
            !cell.charge(&quota, 0, 1).is_admitted(),
            "the cell must be exhausted for this test to mean anything"
        );

        let before = cell.tat();
        cell.debit(&quota, 0, 1);
        assert!(
            cell.tat() > before,
            "a surcharge on an exhausted cell was silently dropped"
        );
    }

    /// A surcharge may push a cell past full — that is the point. It may never
    /// brick one: an unbounded advance is the runtime shape of
    /// `WeightExceedsBurst`, so the price of a weight nobody could have checked
    /// is capped at one whole budget.
    #[test]
    fn a_debit_never_costs_more_than_one_whole_budget() {
        let quota = quota(10, 3);
        let cell = Cell::new();

        assert_eq!(
            cell.debit(&quota, 0, u32::MAX),
            quota.tolerance,
            "an unbounded surcharge on a fresh cell should cost exactly one budget"
        );
        // Even stacked on an already-exhausted cell, the wait is bounded.
        let debt = cell.debit(&quota, 0, u32::MAX);
        assert!(debt <= quota.tolerance.saturating_mul(2), "debt {debt} is unbounded");

        // And it recovers on the ordinary schedule rather than never.
        assert!(cell.charge(&quota, debt, 1).is_admitted());
    }

    /// Any weight that fits inside the burst must land in full, including on a
    /// cell that is already at its ceiling — clamping the *result* instead of the
    /// increment would forgive exactly the caller who spent the most.
    #[test]
    fn a_debit_lands_in_full_on_a_cell_already_at_its_ceiling() {
        let quota = quota(10, 3);
        let cell = Cell::new();
        assert!(cell.charge(&quota, 0, 3).is_admitted());
        assert_eq!(cell.tat(), quota.tolerance, "the cell should be exactly full");

        let after = cell.debit(&quota, 0, 2);
        assert_eq!(
            after,
            quota.tolerance + 2 * quota.emission_interval,
            "a surcharge on a spent cell was clamped away"
        );
    }

    /// Debit and charge must price identically, or "pre-charge the base, debit
    /// the fan-out" would not equal "charge the derived weight".
    #[test]
    fn a_split_charge_costs_the_same_as_a_single_one() {
        let quota = quota(100, 50);

        let split = Cell::new();
        assert!(split.charge(&quota, 0, 4).is_admitted());
        split.debit(&quota, 0, 7);

        let whole = Cell::new();
        assert!(whole.charge(&quota, 0, 11).is_admitted());

        assert_eq!(split.tat(), whole.tat());
    }

    /// **`Retry-After` is exact.** Waiting precisely the reported duration must
    /// succeed, and one nanosecond less must not.
    #[test]
    fn retry_after_is_exact_to_the_nanosecond() {
        let quota = quota(10, 2);
        let cell = Cell::new();
        assert!(cell.charge(&quota, 0, 1).is_admitted());
        assert!(cell.charge(&quota, 0, 1).is_admitted());

        let Decision::Refuse { retry_after, .. } = cell.charge(&quota, 0, 1) else {
            panic!("expected a refusal");
        };
        let wait = u64::try_from(retry_after.as_nanos()).unwrap();

        assert!(
            !cell.charge(&quota, wait - 1, 1).is_admitted(),
            "admitted a nanosecond before the reported retry time"
        );
        assert!(
            cell.charge(&quota, wait, 1).is_admitted(),
            "still refused at exactly the reported retry time"
        );
    }

    #[test]
    fn quota_refills_at_the_declared_rate() {
        // 10/sec ⇒ one unit per 100ms.
        let quota = quota(10, 10);
        let cell = Cell::new();
        for _ in 0..10 {
            assert!(cell.charge(&quota, 0, 1).is_admitted());
        }
        assert!(!cell.charge(&quota, 0, 1).is_admitted());
        assert!(!cell.charge(&quota, SEC / 10 - 1, 1).is_admitted());
        assert!(cell.charge(&quota, SEC / 10, 1).is_admitted());
    }

    /// A steady stream exactly at the rate must never be refused, no matter how
    /// long it runs — the property that separates a limiter from a throttle
    /// that slowly strangles a well-behaved client.
    #[test]
    fn a_conforming_stream_is_never_refused() {
        let quota = quota(100, 10);
        let cell = Cell::new();
        let interval = SEC / 100;
        for tick in 0..10_000u64 {
            assert!(
                cell.charge(&quota, tick * interval, 1).is_admitted(),
                "a conforming request was refused at tick {tick}"
            );
        }
    }

    /// **The failure fixed-window limiters have and this one does not.**
    ///
    /// A fixed window admits a full burst just before the boundary and another
    /// full burst just after — 2× the stated limit inside a couple of
    /// milliseconds. GCRA has no boundary to straddle: what was spent stays
    /// spent until it is earned back.
    #[test]
    fn there_is_no_burst_at_a_window_boundary() {
        let quota = quota(10, 10);
        let cell = Cell::new();

        // Drain the burst at the very end of what a fixed window would call
        // window one.
        let end_of_window = SEC - SEC / 1_000;
        for _ in 0..10 {
            assert!(cell.charge(&quota, end_of_window, 1).is_admitted());
        }

        // Two milliseconds later a fixed-window limiter has reset and admits ten
        // more. Only ~0.02 units have actually been earned, so this must admit
        // none.
        let admitted = (0..10)
            .filter(|_| cell.charge(&quota, end_of_window + SEC / 500, 1).is_admitted())
            .count();
        assert_eq!(
            admitted, 0,
            "admitted {admitted} across a window boundary — 2× the limit in 2ms is exactly the \
             fixed-window pathology"
        );
    }

    /// The positive statement of the same property: over any window, admissions
    /// are bounded by `burst + rate × elapsed`. Nothing is lost — a caller who
    /// waits really does get their quota back.
    #[test]
    fn a_full_period_of_waiting_earns_exactly_one_periods_quota() {
        let quota = quota(10, 10);
        let cell = Cell::new();
        for _ in 0..10 {
            assert!(cell.charge(&quota, 0, 1).is_admitted());
        }
        let after_a_second = (0..20)
            .filter(|_| cell.charge(&quota, SEC, 1).is_admitted())
            .count();
        assert_eq!(
            after_a_second, 10,
            "one second at 10/sec earns exactly ten units back — no more, and no fewer"
        );
    }

    #[test]
    fn weight_consumes_proportionally() {
        let quota = quota(10, 10);
        let heavy = Cell::new();
        assert!(heavy.charge(&quota, 0, 7).is_admitted());
        assert_eq!(heavy.charge(&quota, 0, 3).remaining(), 0);
        assert!(!heavy.charge(&quota, 0, 1).is_admitted());
    }

    #[test]
    fn a_weight_of_zero_is_treated_as_one() {
        // Free operations would otherwise be an unmetered hole.
        let quota = quota(10, 1);
        let cell = Cell::new();
        assert!(cell.charge(&quota, 0, 0).is_admitted());
        assert!(!cell.charge(&quota, 0, 0).is_admitted());
    }

    /// A weight above the burst is unsatisfiable at every instant. It must be a
    /// build error, not a permanent silent 429 on one endpoint.
    #[test]
    fn a_weight_above_the_burst_is_refused_at_build() {
        let quota = quota(10, 5);
        assert_eq!(quota.max_weight(), 5);
        assert_eq!(quota.check_weight(5), Ok(()));
        assert_eq!(
            quota.check_weight(6),
            Err(QuotaError::WeightExceedsBurst {
                weight: 6,
                burst: 5
            })
        );
        // The message has to name the fix, because the symptom looks like load.
        let rendered = QuotaError::WeightExceedsBurst {
            weight: 6,
            burst: 5,
        }
        .to_string();
        assert!(rendered.contains("Raise the burst"));
    }

    #[test]
    fn degenerate_quotas_are_refused() {
        assert_eq!(
            Quota::per(0, Duration::from_secs(1)),
            Err(QuotaError::ZeroRate)
        );
        assert_eq!(
            Quota::with_burst(1, Duration::from_secs(1), 0),
            Err(QuotaError::ZeroBurst)
        );
        assert_eq!(
            Quota::per(1, Duration::ZERO),
            Err(QuotaError::UnusablePeriod)
        );
        // More units per second than there are nanoseconds in one.
        assert_eq!(
            Quota::per(2_000_000_000, Duration::from_secs(1)),
            Err(QuotaError::UnusablePeriod)
        );
    }

    /// The eviction predicate. A replenished cell must be indistinguishable
    /// from one that was never created, or bounded memory would cost accuracy.
    #[test]
    fn a_replenished_cell_behaves_exactly_like_a_fresh_one() {
        let quota = quota(10, 3);
        let used = Cell::new();
        assert!(used.charge(&quota, 0, 3).is_admitted());

        let replenished_at = used.tat();
        assert!(!used.is_replenished(replenished_at - 1));
        assert!(used.is_replenished(replenished_at));

        // From that instant on, the used cell and a brand-new one agree on
        // everything — which is what makes dropping it free.
        let fresh = Cell::new();
        for step in 0..8u64 {
            let now = replenished_at + step * (SEC / 10);
            assert_eq!(
                used.charge(&quota, now, 1),
                fresh.charge(&quota, now, 1),
                "a replenished cell diverged from a fresh one at step {step}"
            );
        }
    }

    /// Under concurrency the cell must admit *exactly* the burst — no more (a
    /// lost update would over-admit) and no fewer (a spurious CAS failure must
    /// retry, not refuse).
    #[test]
    fn concurrent_callers_admit_exactly_the_burst() {
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc;

        const BURST: u32 = 500;
        let quota = Arc::new(quota(1_000, BURST));
        let cell = Arc::new(Cell::new());
        let admitted = Arc::new(AtomicU32::new(0));

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let quota = Arc::clone(&quota);
                let cell = Arc::clone(&cell);
                let admitted = Arc::clone(&admitted);
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        if cell.charge(&quota, 0, 1).is_admitted() {
                            admitted.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("thread panicked");
        }

        assert_eq!(
            admitted.load(Ordering::Relaxed),
            BURST,
            "1600 concurrent attempts against a burst of {BURST} admitted the wrong count"
        );
    }

    /// A cell must not misbehave near the end of the `u64` timeline, even though
    /// reaching it would take centuries.
    #[test]
    fn arithmetic_saturates_rather_than_wrapping() {
        let quota = quota(10, 3);
        let cell = Cell::at(u64::MAX);
        // Deeply in debt: refuse, with a finite reported wait rather than a
        // wrapped one.
        let decision = cell.charge(&quota, 0, 1);
        assert!(!decision.is_admitted());
        assert!(decision.reset_after() > Duration::from_secs(0));
    }
}
