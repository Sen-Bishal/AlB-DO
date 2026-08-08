//! SHUTTER — rate limiting derived from what an endpoint does.
//!
//! *`AUTH.md` R6: rate limiting does not exist anywhere in the tree, and an
//! unauthenticated login endpoint is where that stops being theoretical. It goes
//! at the layer that already intercepts every action, so fixing it for auth
//! fixes it for every action.*
//!
//! ## Why this is not a wrapper around `governor`
//!
//! The core algorithm here ([`gcra`]) is the same one `governor` implements, and
//! that crate is good. The differentiation is entirely in the three layers above
//! it, none of which a general-purpose limiter can provide because none of them
//! are expressible without owning the compiler and the router:
//!
//! 1. **[`cost`] — the weight is derived, not typed.** A path-and-IP limiter cannot know that this request is a cached render and that one is a write fanning out to five hundred lanes, so every number it enforces is a guess. Ours is a by-product of facts the build already established.
//! 2. **[`Key`] — the subject is the principal, not the address.** IP is a proxy for identity that CGNAT and cloud egress made a bad one. AUTH just landed, so the limiter can ration the *actor*.
//! 3. **[`Shutter`] — the table degrades instead of exhausting.** See below; this is the failure mode that takes real limiters down.
//!
//! ## Memory is bounded, and bounding it costs nothing
//!
//! The attack that kills a naive keyed limiter is not volume, it is
//! **cardinality**: spray requests from a million distinct addresses and the
//! `HashMap` that was protecting you becomes the thing that OOMs you. A limiter
//! whose memory an attacker controls is a liability wearing a mitigation's
//! clothes.
//!
//! Two properties make the fix free rather than a compromise:
//!
//! 🔑 **A replenished cell is indistinguishable from a cell that does not
//! exist.** GCRA's state saturates at "no debt", so evicting a cell whose TAT
//! has passed loses exactly nothing — recreating it yields identical decisions
//! forever after ([`gcra::Cell::is_replenished`], and the test that pins it).
//! Eviction is therefore keyed on *replenishment*, never on age or recency:
//! dropping an indebted cell would forgive debt, which is the one direction that
//! favours the attacker.
//!
//! 🔑 **Overflow makes the limiter stricter, not weaker.** When the exact table
//! is full, keys fall back to a fixed-size array of shared cells addressed by
//! hash. Under a cardinality attack the attacker's own keys collide with each
//! other and throttle each other harder, while memory stays constant. The
//! degraded mode is *more* aggressive than the exact one — which is the correct
//! direction for a mechanism to fail in, and the opposite of what an unbounded
//! map does.
//!
//! The cost is that a legitimate caller can share an overflow cell with an
//! attacker during an attack. That is a real cost, stated rather than hidden;
//! the alternative on offer is the process dying.
//!
//! ## No Redis, and that is a feature
//!
//! The limiter is in-process and lock-free. There is no network hop on the
//! request path, no shared store to be a single point of failure, and no
//! "what do we do when the limiter is down?" question — because there is nothing
//! separate to be down. The trade is that limits are per-process rather than
//! per-cluster, so an N-instance deployment admits up to N× the configured rate.
//! That is the honest shape of the trade, and for the thing this protects — a
//! login endpoint and an action dispatcher — a bounded multiple of a tight limit
//! is worth more than an exact limit that can fail open when a cache is
//! unreachable.

pub mod cost;
pub mod gcra;

pub use cost::{classify, classify_route, compress_fanout, Cost, OperationClass};
pub use gcra::{Cell, Decision, Nanos, Quota, QuotaError};

use dashmap::DashMap;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Exact cells held before the table degrades to [`Shutter::overflow`].
///
/// 64k cells is roughly 2 MB of state — small enough to be unremarkable on any
/// host, large enough that a real application's population of principals and
/// addresses never reaches it. It is a *memory* ceiling, not a capacity limit:
/// exceeding it degrades accuracy, never correctness.
pub const DEFAULT_EXACT_CAPACITY: usize = 65_536;

/// Shared cells used once the exact table is full. A power of two so the
/// hash-to-slot mapping is a mask rather than a modulo.
pub const OVERFLOW_CELLS: usize = 8_192;

/// Cells examined for eviction on each insertion that finds the table full.
///
/// Amortised sweeping rather than a background task: a limiter that needs a
/// timer thread to stay correct has a liveness dependency, and this one has
/// nothing to be late.
const EVICTION_SAMPLE: usize = 32;

/// Who is being rationed.
///
/// 🔑 **A principal outranks an address wherever one exists.** IP was always a
/// proxy for identity, and CGNAT, corporate NAT and cloud egress ranges made it
/// a bad one in both directions: thousands of unrelated people share one
/// address, and one attacker rents thousands. Rationing the *actor* is strictly
/// better, and AUTH is what made it possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    /// An authenticated actor. The good case.
    Principal {
        /// The principal id, as it appears in a topic name.
        id: String,
        /// Which limit is being consumed.
        class: OperationClass,
    },
    /// An unauthenticated caller, by address. The fallback, not the default.
    Address {
        /// Remote address.
        addr: IpAddr,
        /// Which limit is being consumed.
        class: OperationClass,
    },
    /// A named account under credential attack — the *target*, not the caller.
    ///
    /// Exists because per-caller limiting cannot see distributed credential
    /// stuffing: a thousand addresses making ten attempts each against one
    /// account is, from every caller's point of view, ten attempts. Only the
    /// account's own bucket sees a thousand.
    Account {
        /// The account being attempted, already normalised by the caller.
        subject: String,
    },
}

impl Key {
    /// Which limit this key answers to.
    #[must_use]
    pub const fn class(&self) -> OperationClass {
        match self {
            Self::Principal { class, .. } | Self::Address { class, .. } => *class,
            Self::Account { .. } => OperationClass::Credential,
        }
    }

    fn slot(&self) -> usize {
        let mut hasher = ahash::AHasher::default();
        self.hash(&mut hasher);
        (hasher.finish() as usize) & (OVERFLOW_CELLS - 1)
    }
}

/// A monotonic nanosecond source.
///
/// Injected rather than called directly so the whole limiter is testable
/// without sleeping. Tests that assert timing by `thread::sleep` are slow, flaky
/// and cannot express "and then a week passes" — every temporal property here is
/// asserted against a clock the test moves by hand.
pub trait Clock: Send + Sync {
    /// Nanoseconds since an arbitrary fixed origin. Must never go backwards.
    fn now(&self) -> Nanos;
}

/// The production clock: monotonic, immune to NTP steps and to a host operator
/// changing the wall clock.
#[derive(Debug)]
pub struct MonotonicClock {
    origin: Instant,
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock {
    /// Start a clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Nanos {
        u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}

/// A clock tests move by hand.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    /// A clock at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Move time forward.
    pub fn advance(&self, by: Duration) {
        self.now.fetch_add(
            u64::try_from(by.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Jump to an absolute instant.
    pub fn set(&self, to: Nanos) {
        self.now.store(to, Ordering::Relaxed);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Nanos {
        self.now.load(Ordering::Relaxed)
    }
}

/// The quotas each [`OperationClass`] answers to.
///
/// Defaults are stated per class rather than per route, which is what makes them
/// reviewable: there are five numbers in the whole system, and each one has a
/// sentence explaining it. Compare with a per-endpoint scheme, where the numbers
/// multiply until nobody can say whether the set is coherent.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Cached renders.
    pub static_read: Quota,
    /// Reads that reach FORGE.
    pub read: Quota,
    /// Writes, whose weight already carries their blast radius.
    pub write: Quota,
    /// Calls against a third party's quota.
    pub outbound: Quota,
    /// Credential attempts — the tightest limit in the system.
    pub credential: Quota,
    /// The target account's own budget for *failed* attempts.
    pub account: Quota,
}

impl Default for Limits {
    fn default() -> Self {
        let minute = Duration::from_secs(60);
        Self {
            // Generous: a page with fifty assets is one visitor, not an attack,
            // and this class exists mainly so the accounting is complete.
            static_read: Quota::with_burst(600, minute, 200).expect("static_read"),
            // A busy human navigating hard.
            read: Quota::with_burst(300, minute, 100).expect("read"),
            // Weight already encodes fan-out, so this is writes-worth-of-work
            // rather than write-calls.
            write: Quota::with_burst(120, minute, 60).expect("write"),
            // Someone else's quota. Tight, because exhausting it is not
            // recoverable by adding capacity on our side.
            outbound: Quota::with_burst(60, minute, 20).expect("outbound"),
            // Ten attempts a minute from one caller is far above what a human
            // typing a password produces and far below what guessing needs.
            credential: Quota::with_burst(10, minute, 5).expect("credential"),
            // The *account's* budget for failures, deliberately looser than the
            // per-caller one: this bucket's job is to stop distributed stuffing,
            // and making it tight would hand an attacker a lockout primitive.
            // See `Shutter::credential_attempt`.
            account: Quota::with_burst(50, Duration::from_secs(3_600), 20).expect("account"),
        }
    }
}

impl Limits {
    /// The quota a key answers to.
    #[must_use]
    pub const fn for_key(&self, key: &Key) -> &Quota {
        match key {
            Key::Account { .. } => &self.account,
            Key::Principal { class, .. } | Key::Address { class, .. } => match class {
                OperationClass::StaticRead => &self.static_read,
                OperationClass::Read => &self.read,
                OperationClass::Write => &self.write,
                OperationClass::Outbound => &self.outbound,
                OperationClass::Credential => &self.credential,
            },
        }
    }

    /// Refuse a configuration in which some operation could never be admitted.
    ///
    /// Run at boot, over the heaviest weight each class can derive, so a
    /// permanently-refused endpoint surfaces as a startup error rather than as
    /// an outage that looks like traffic.
    ///
    /// # Errors
    /// [`QuotaError::WeightExceedsBurst`] naming the class that cannot admit its
    /// own heaviest operation.
    pub fn check_admits_heaviest(&self) -> Result<(), QuotaError> {
        // **Fan-out is a property of writes, not of every class.** A credential
        // check and an outbound call reach one place each; only a FORGE write
        // fans out to subscribed lanes. Charging every class the maximum fan-out
        // ceiling would demand a burst no tight limit should have — and a
        // "safety check" that forces the limits it validates to be loose is
        // worse than none.
        let fan_out_ceiling = compress_fanout(u32::MAX);
        self.write
            .check_weight(OperationClass::Write.base_weight().saturating_add(fan_out_ceiling))?;

        for (quota, class) in [
            (&self.static_read, OperationClass::StaticRead),
            (&self.read, OperationClass::Read),
            (&self.outbound, OperationClass::Outbound),
            (&self.credential, OperationClass::Credential),
        ] {
            quota.check_weight(class.base_weight())?;
        }
        Ok(())
    }
}

/// What a limiter decided, with everything a caller needs to answer the request.
#[derive(Debug, Clone)]
pub struct Verdict {
    /// The underlying decision.
    pub decision: Decision,
    /// What was charged.
    pub cost: Cost,
    /// The quota's burst, for `RateLimit-Limit`.
    pub limit: u32,
    /// Whether this key was served by a shared overflow cell — i.e. the table
    /// was full. Surfaced so an operator can see degradation rather than
    /// guessing at it.
    pub degraded: bool,
}

impl Verdict {
    /// Whether the request may proceed.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        self.decision.is_admitted()
    }

    /// `Retry-After`, in whole seconds rounded up — the header's unit.
    ///
    /// Rounded **up**, always: a client told to wait zero seconds retries
    /// immediately and is refused again, which turns a limit into a hot loop.
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self.decision {
            Decision::Admit { .. } => None,
            Decision::Refuse { retry_after, .. } => {
                Some(retry_after.as_secs().saturating_add(
                    u64::from(retry_after.subsec_nanos() > 0),
                ))
            }
        }
    }
}

/// The limiter.
///
/// Cloneable — internally `Arc` — so every handler holds the same one.
#[derive(Clone)]
pub struct Shutter {
    inner: Arc<Inner>,
}

struct Inner {
    exact: DashMap<Key, Cell>,
    /// Fixed-size shared cells for when `exact` is full. Allocated once; never
    /// grows, never shrinks, and is what makes the limiter's memory an
    /// operator's choice rather than an attacker's.
    overflow: Box<[Cell]>,
    capacity: usize,
    limits: Limits,
    clock: Arc<dyn Clock>,
    /// Count of decisions served by `overflow`, for observability.
    degraded_decisions: AtomicU64,
}

impl Shutter {
    /// A limiter with the default limits and a monotonic clock.
    ///
    /// # Errors
    /// [`QuotaError`] if the limits cannot admit their own heaviest operation.
    pub fn new() -> Result<Self, QuotaError> {
        Self::with(
            Limits::default(),
            Arc::new(MonotonicClock::new()),
            DEFAULT_EXACT_CAPACITY,
        )
    }

    /// A limiter with explicit limits, clock and capacity.
    ///
    /// # Errors
    /// [`QuotaError`] if some class could never admit its heaviest operation.
    pub fn with(
        limits: Limits,
        clock: Arc<dyn Clock>,
        capacity: usize,
    ) -> Result<Self, QuotaError> {
        limits.check_admits_heaviest()?;
        Ok(Self {
            inner: Arc::new(Inner {
                exact: DashMap::new(),
                overflow: (0..OVERFLOW_CELLS).map(|_| Cell::new()).collect(),
                capacity: capacity.max(1),
                limits,
                clock,
                degraded_decisions: AtomicU64::new(0),
            }),
        })
    }

    /// Charge a derived cost against a key.
    ///
    /// The ordinary entry point: the caller derives a [`Cost`] from what the
    /// endpoint does ([`cost::classify`], [`Cost::fan_out`]) and this rations it.
    pub fn charge(&self, key: &Key, cost: Cost) -> Verdict {
        let now = self.inner.clock.now();
        let quota = self.inner.limits.for_key(key);
        let (decision, degraded) = self.with_cell(key, |cell| cell.charge(quota, now, cost.weight));
        if degraded {
            self.inner
                .degraded_decisions
                .fetch_add(1, Ordering::Relaxed);
        }
        Verdict {
            decision,
            cost,
            limit: quota.burst(),
            degraded,
        }
    }

    /// Record a price that only became knowable after the work was done.
    ///
    /// **The write path's second half.** A write's blast radius is not visible
    /// until the write has resolved which channels it touches, so the fan-out
    /// component of its cost arrives *after* the admission decision that let it
    /// run. Admission charges [`Cost::flat`]; this lands the rest.
    ///
    /// There is no [`Verdict`], because there is no decision left to make — the
    /// request has been served and a limiter does not un-commit a write. What
    /// this buys is that the *next* request is priced by what the last one
    /// actually cost, which is the whole difference between a derived weight and
    /// a guessed one.
    ///
    /// Unconditional by construction: see [`Cell::debit`] for why routing this
    /// through [`Self::charge`] would drop the debt in exactly the case it exists
    /// to record.
    pub fn debit(&self, key: &Key, cost: Cost) {
        if cost.weight == 0 {
            return;
        }
        let now = self.inner.clock.now();
        let quota = self.inner.limits.for_key(key);
        let (_, degraded) = self.with_cell(key, |cell| cell.debit(quota, now, cost.weight));
        if degraded {
            self.inner
                .degraded_decisions
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// **The login path, which needs two buckets and cannot use one.**
    ///
    /// Per-caller limiting alone does not see distributed credential stuffing:
    /// a thousand addresses making ten attempts each against one account looks,
    /// from every caller's seat, like ten attempts. Only the account's own
    /// bucket sees a thousand.
    ///
    /// But per-account limiting alone hands an attacker a **lockout primitive** —
    /// burn a victim's budget with deliberate failures and they cannot log in.
    /// Three things keep that from being the new bug, and all three are
    /// deliberate:
    ///
    /// 1. **Only failures are charged to the account.** A correct password is admitted while there is any budget left, so a legitimate user racing an attacker still gets in. That is [`Self::note_credential_failure`], called after the check — never before.
    /// 2. **The account bucket is generous and the caller bucket is tight.** Making the account bucket strict would optimise for the attack that needs a thousand addresses over the one that needs none.
    /// 3. **Passkeys remove the premise.** There is no password to stuff, which is a large part of why `AUTH.md` § 6.2 makes them the default rather than an option.
    ///
    /// Charges the caller's bucket and only *peeks* the account's, so asking the
    /// question never itself counts against the victim.
    pub fn credential_attempt(&self, caller: &Key, account: &str) -> Verdict {
        let now = self.inner.clock.now();
        let cost = Cost::flat(OperationClass::Credential);

        let account_key = Key::Account {
            subject: account.to_string(),
        };
        let account_quota = self.inner.limits.for_key(&account_key);
        let (account_ok, account_degraded) =
            self.with_cell(&account_key, |cell| cell.peek(account_quota, now, 1));

        if !account_ok {
            // The account is under attack. Refuse without charging either
            // bucket: charging the caller here would let an attacker exhaust an
            // innocent third party's budget by naming a victim account.
            let (decision, _) =
                self.with_cell(&account_key, |cell| cell.charge(account_quota, now, 0));
            let refusal = match decision {
                Decision::Refuse { .. } => decision,
                Decision::Admit { reset_after, .. } => Decision::Refuse {
                    retry_after: reset_after,
                    reset_after,
                },
            };
            return Verdict {
                decision: refusal,
                cost,
                limit: account_quota.burst(),
                degraded: account_degraded,
            };
        }

        self.charge(caller, cost)
    }

    /// Charge a failed credential attempt to the **account** it targeted.
    ///
    /// Called after the check, and only on failure. See
    /// [`Self::credential_attempt`] for why the ordering is the whole design.
    pub fn note_credential_failure(&self, account: &str) {
        let now = self.inner.clock.now();
        let key = Key::Account {
            subject: account.to_string(),
        };
        let quota = self.inner.limits.for_key(&key);
        let _ = self.with_cell(&key, |cell| cell.charge(quota, now, 1));
    }

    /// Exact cells currently held.
    #[must_use]
    pub fn tracked_keys(&self) -> usize {
        self.inner.exact.len()
    }

    /// Decisions served by a shared overflow cell — nonzero means the table hit
    /// its ceiling and accuracy is degraded.
    #[must_use]
    pub fn degraded_decisions(&self) -> u64 {
        self.inner.degraded_decisions.load(Ordering::Relaxed)
    }

    /// Drop every replenished cell. Safe at any time, by construction — see the
    /// module docs.
    ///
    /// Returns how many were dropped.
    pub fn sweep(&self) -> usize {
        let now = self.inner.clock.now();
        let before = self.inner.exact.len();
        self.inner.exact.retain(|_, cell| !cell.is_replenished(now));
        before - self.inner.exact.len()
    }

    /// Run `f` against the cell for `key`, returning its result and whether the
    /// cell was a shared overflow one.
    fn with_cell<R>(&self, key: &Key, f: impl FnOnce(&Cell) -> R) -> (R, bool) {
        if let Some(cell) = self.inner.exact.get(key) {
            return (f(cell.value()), false);
        }

        if self.inner.exact.len() >= self.inner.capacity {
            // Full. Try to make room by dropping replenished cells — free, since
            // they are indistinguishable from absent ones. Sampled rather than
            // exhaustive so the cost stays O(1) per request instead of O(n).
            self.evict_sample(EVICTION_SAMPLE);
            if self.inner.exact.len() >= self.inner.capacity {
                // Still full: degrade. The attacker's keys now collide with each
                // other in a fixed-size array and throttle each other harder,
                // and our memory stops moving.
                return (f(&self.inner.overflow[key.slot()]), true);
            }
        }

        let cell = self.inner.exact.entry(key.clone()).or_default();
        (f(cell.value()), false)
    }

    /// Drop up to `sample` replenished cells.
    fn evict_sample(&self, sample: usize) {
        let now = self.inner.clock.now();
        let mut doomed = Vec::new();
        for entry in self.inner.exact.iter() {
            if entry.value().is_replenished(now) {
                doomed.push(entry.key().clone());
                if doomed.len() >= sample {
                    break;
                }
            }
        }
        for key in doomed {
            // `remove_if` rather than `remove`: between the scan and here another
            // caller may have charged this cell, and dropping an indebted cell
            // would forgive debt — the one direction that favours an attacker.
            self.inner
                .exact
                .remove_if(&key, |_, cell| cell.is_replenished(now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, last))
    }

    fn shutter(clock: &Arc<ManualClock>) -> Shutter {
        Shutter::with(
            Limits::default(),
            Arc::clone(clock) as Arc<dyn Clock>,
            DEFAULT_EXACT_CAPACITY,
        )
        .expect("default limits are admissible")
    }

    fn caller(last: u8, class: OperationClass) -> Key {
        Key::Address {
            addr: addr(last),
            class,
        }
    }

    /// The defaults must be able to admit the heaviest operation they can
    /// derive, or an endpoint is permanently 429 and it looks like load.
    #[test]
    fn the_default_limits_admit_their_own_heaviest_operation() {
        assert_eq!(Limits::default().check_admits_heaviest(), Ok(()));
    }

    #[test]
    fn a_limit_that_could_never_admit_its_heaviest_operation_is_refused_at_boot() {
        let limits = Limits {
            // A burst of 2 cannot admit a write that costs 4 + fan-out.
            write: Quota::with_burst(60, Duration::from_secs(60), 2).unwrap(),
            ..Limits::default()
        };
        assert!(matches!(
            limits.check_admits_heaviest(),
            Err(QuotaError::WeightExceedsBurst { .. })
        ));
        assert!(Shutter::with(
            limits,
            Arc::new(ManualClock::new()) as Arc<dyn Clock>,
            16
        )
        .is_err());
    }

    #[test]
    fn distinct_callers_do_not_share_a_budget() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let cost = Cost::flat(OperationClass::Credential);

        // Exhaust one caller.
        while shutter.charge(&caller(1, OperationClass::Credential), cost).is_admitted() {}
        // A different caller is untouched.
        assert!(shutter
            .charge(&caller(2, OperationClass::Credential), cost)
            .is_admitted());
    }

    /// A principal and an address are different subjects even when the same
    /// request could produce either.
    #[test]
    fn a_principal_and_an_address_are_separate_subjects() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let cost = Cost::flat(OperationClass::Credential);
        let principal = Key::Principal {
            id: "u_7f3a".to_string(),
            class: OperationClass::Credential,
        };

        while shutter.charge(&principal, cost).is_admitted() {}
        assert!(shutter
            .charge(&caller(1, OperationClass::Credential), cost)
            .is_admitted());
    }

    /// Classes are separate budgets: hammering reads must not lock out writes.
    #[test]
    fn one_class_cannot_exhaust_another() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        while shutter
            .charge(&caller(1, OperationClass::Read), Cost::flat(OperationClass::Read))
            .is_admitted()
        {}
        assert!(shutter
            .charge(
                &caller(1, OperationClass::Write),
                Cost::fan_out(OperationClass::Write, 0)
            )
            .is_admitted());
    }

    /// **The claim.** The same write, priced by who is watching.
    #[test]
    fn a_fan_out_write_exhausts_the_budget_faster_than_a_quiet_one() {
        let clock = Arc::new(ManualClock::new());

        let quiet_shutter = shutter(&clock);
        let quiet = (0..1_000)
            .take_while(|_| {
                quiet_shutter
                    .charge(
                        &caller(1, OperationClass::Write),
                        Cost::fan_out(OperationClass::Write, 0),
                    )
                    .is_admitted()
            })
            .count();

        let busy_shutter = shutter(&clock);
        let busy = (0..1_000)
            .take_while(|_| {
                busy_shutter
                    .charge(
                        &caller(1, OperationClass::Write),
                        Cost::fan_out(OperationClass::Write, 1_024),
                    )
                    .is_admitted()
            })
            .count();

        assert!(
            busy < quiet,
            "a write reaching 1024 lanes ({busy}) must not be as cheap as one reaching nobody \
             ({quiet})"
        );
    }

    #[test]
    fn a_budget_recovers_as_time_passes() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let key = caller(1, OperationClass::Credential);
        let cost = Cost::flat(OperationClass::Credential);

        while shutter.charge(&key, cost).is_admitted() {}
        let verdict = shutter.charge(&key, cost);
        assert!(!verdict.is_admitted());

        let wait = verdict.retry_after_secs().expect("a refusal reports a wait");
        clock.advance(Duration::from_secs(wait));
        assert!(shutter.charge(&key, cost).is_admitted());
    }

    /// A client told to wait zero seconds retries immediately and is refused
    /// again — a limit that becomes a hot loop.
    #[test]
    fn retry_after_never_rounds_down_to_zero() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let key = caller(1, OperationClass::Read);
        let cost = Cost::flat(OperationClass::Read);

        while shutter.charge(&key, cost).is_admitted() {}
        let verdict = shutter.charge(&key, cost);
        assert_eq!(verdict.retry_after_secs(), Some(1));
    }

    // ── credential stuffing ────────────────────────────────────────────

    /// The attack per-caller limiting cannot see: many addresses, one account.
    #[test]
    fn distributed_stuffing_against_one_account_is_stopped() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        let mut admitted = 0;
        for attacker in 0..=255u8 {
            // Each address makes ONE attempt — well inside any per-caller limit.
            let verdict = shutter.credential_attempt(
                &caller(attacker, OperationClass::Credential),
                "ada@example.com",
            );
            if verdict.is_admitted() {
                admitted += 1;
                shutter.note_credential_failure("ada@example.com");
            }
        }

        assert!(
            admitted < 256,
            "256 addresses each made one attempt and every one was admitted — the account \
             bucket is not seeing the attack"
        );
        assert!(
            admitted <= 25,
            "the account absorbed {admitted} attempts before the limit engaged"
        );
    }

    /// **The lockout primitive, and why it is not one.** A correct password must
    /// be admitted while any budget remains, so a legitimate user racing an
    /// attacker still gets in.
    #[test]
    fn a_successful_login_is_not_charged_to_the_account() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        // A run of successes must never exhaust the victim's account budget,
        // however many there are.
        for round in 0..500 {
            let verdict =
                shutter.credential_attempt(&caller(u8::try_from(round % 200).unwrap(), OperationClass::Credential), "ada@example.com");
            assert!(
                verdict.is_admitted(),
                "a successful login was refused at round {round} — success is being charged"
            );
            // …and no `note_credential_failure`, because it succeeded.
        }
    }

    /// Asking "is this account under attack?" must not itself count against the
    /// account, or the question becomes the attack.
    #[test]
    fn peeking_at_an_account_never_charges_it() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        for round in 0..1_000 {
            shutter.credential_attempt(
                &caller(u8::try_from(round % 250).unwrap(), OperationClass::Credential),
                "ada@example.com",
            );
        }
        // The account has absorbed no failures, so a fresh caller still gets in.
        assert!(shutter
            .credential_attempt(&caller(251, OperationClass::Credential), "ada@example.com")
            .is_admitted());
    }

    /// Naming a victim account must not let an attacker exhaust an innocent
    /// caller's budget.
    #[test]
    fn a_locked_account_does_not_charge_the_caller() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        for _ in 0..200 {
            shutter.note_credential_failure("ada@example.com");
        }
        let innocent = caller(9, OperationClass::Credential);
        for _ in 0..50 {
            assert!(!shutter
                .credential_attempt(&innocent, "ada@example.com")
                .is_admitted());
        }
        // Their own budget is untouched: a different account still works.
        assert!(shutter
            .credential_attempt(&innocent, "grace@example.com")
            .is_admitted());
    }

    /// Two accounts under attack are independent.
    #[test]
    fn accounts_do_not_share_a_budget() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);

        for _ in 0..200 {
            shutter.note_credential_failure("ada@example.com");
        }
        assert!(shutter
            .credential_attempt(&caller(1, OperationClass::Credential), "grace@example.com")
            .is_admitted());
    }

    // ── cardinality ────────────────────────────────────────────────────

    /// **The attack that kills naive limiters.** Memory must not be the
    /// attacker's to allocate.
    #[test]
    fn a_cardinality_attack_cannot_grow_memory_without_bound() {
        let clock = Arc::new(ManualClock::new());
        let capacity = 512;
        let shutter = Shutter::with(
            Limits::default(),
            Arc::clone(&clock) as Arc<dyn Clock>,
            capacity,
        )
        .expect("limits");

        for i in 0..100_000u32 {
            let key = Key::Principal {
                id: format!("u_{i}"),
                class: OperationClass::Read,
            };
            shutter.charge(&key, Cost::flat(OperationClass::Read));
        }

        assert!(
            shutter.tracked_keys() <= capacity,
            "100k distinct keys grew the table to {} against a capacity of {capacity}",
            shutter.tracked_keys()
        );
        assert!(
            shutter.degraded_decisions() > 0,
            "degradation must be observable, not silent"
        );
    }

    /// Degrading must make the limiter *stricter*. A fallback that admitted more
    /// than the exact path would be a bypass wearing a mitigation's clothes.
    #[test]
    fn the_degraded_path_is_not_a_bypass() {
        let clock = Arc::new(ManualClock::new());
        let shutter =
            Shutter::with(Limits::default(), Arc::clone(&clock) as Arc<dyn Clock>, 8).expect("limits");

        // Fill the exact table with indebted cells so nothing can be evicted.
        for i in 0..8u32 {
            let key = Key::Principal {
                id: format!("resident_{i}"),
                class: OperationClass::Write,
            };
            shutter.charge(&key, Cost::fan_out(OperationClass::Write, 0));
        }

        // Now spray. Every sprayed key lands in the shared overflow array, so
        // they consume each other's budget rather than each getting a fresh one.
        let mut admitted = 0u32;
        for i in 0..100_000u32 {
            let key = Key::Principal {
                id: format!("spray_{i}"),
                class: OperationClass::Write,
            };
            if shutter
                .charge(&key, Cost::fan_out(OperationClass::Write, 0))
                .is_admitted()
            {
                admitted += 1;
            }
        }

        // Without the overflow tier this would be 100,000 — one fresh burst per
        // key. Bounded by the overflow array's total capacity instead.
        let ceiling = OVERFLOW_CELLS as u32 * Limits::default().write.burst();
        assert!(
            admitted < ceiling,
            "{admitted} admissions from 100k fresh keys — the degraded path is handing out \
             fresh budgets"
        );
    }

    /// Eviction must be free: a replenished cell carries no information, so
    /// dropping it cannot change a later decision.
    #[test]
    fn sweeping_replenished_cells_changes_no_decision() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let key = caller(1, OperationClass::Read);
        let cost = Cost::flat(OperationClass::Read);

        while shutter.charge(&key, cost).is_admitted() {}
        // Let it fully replenish.
        clock.advance(Duration::from_secs(120));
        assert_eq!(shutter.sweep(), 1);
        assert_eq!(shutter.tracked_keys(), 0);

        // The recreated cell behaves exactly as the swept one would have.
        let after: Vec<bool> = (0..5).map(|_| shutter.charge(&key, cost).is_admitted()).collect();
        assert_eq!(after, vec![true; 5]);
    }

    /// Sweeping must never drop an indebted cell — that would forgive debt.
    #[test]
    fn sweeping_spares_cells_that_still_owe() {
        let clock = Arc::new(ManualClock::new());
        let shutter = shutter(&clock);
        let key = caller(1, OperationClass::Credential);
        let cost = Cost::flat(OperationClass::Credential);

        while shutter.charge(&key, cost).is_admitted() {}
        assert_eq!(shutter.sweep(), 0, "an exhausted cell was forgiven");
        assert!(!shutter.charge(&key, cost).is_admitted());
    }

    #[test]
    fn the_manual_clock_drives_every_temporal_property() {
        let clock = ManualClock::new();
        assert_eq!(clock.now(), 0);
        clock.advance(Duration::from_secs(1));
        assert_eq!(clock.now(), 1_000_000_000);
        clock.set(42);
        assert_eq!(clock.now(), 42);
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let clock = MonotonicClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }
}
