//! SHUTTER · the cost — what an operation is worth, derived rather than typed.
//!
//! ## The thesis
//!
//! Every rate limiter outside this codebase sees an **opaque request**: a path,
//! a method, an address. So every limit it enforces is a number somebody
//! guessed, tuned once during an incident, and never revisited. `100/min` is not
//! a measurement of anything.
//!
//! ALBEDO does not have to guess, because it already knows what the endpoint
//! *does*:
//!
//! - the compiler knows the [`EffectProfile`](crate::effects::EffectProfile) of every component and action, whether it writes to FORGE, and whether it calls a declared APERTURE source;
//! - the live registry knows, **before the write happens**, how many lanes are subscribed to the topic it will fan out to.
//!
//! 🔑 **A FORGE write is not an O(1) operation, and this is the only rate
//! limiter that can know that.** Writing to a topic with five hundred open lanes
//! costs five hundred frame encodings and sends. Pricing that write the same as
//! one nobody is watching is the mistake every path-and-IP limiter must make,
//! because owning the write path *and* the subscription graph is the only way to
//! see it. That is the edge — not the language.
//!
//! ## Two axes, deliberately not conflated
//!
//! It is tempting to fold "dangerous" into "expensive" and end up with one
//! number. That is wrong, and the login endpoint is the proof: a password check
//! is *cheap* — one indexed read and a hash — and it needs the tightest limit in
//! the system. Cost and posture are independent:
//!
//! | | means | set by |
//! |---|---|---|
//! | [`Cost::weight`] | how much of the resource this consumes | the effect profile × the blast radius |
//! | [`OperationClass`] | which limit it answers to | what breaks if it is abused |
//!
//! So a credential check has weight 1 and answers to a quota measured in
//! attempts per minute, while a fan-out write has weight 11 and answers to a
//! quota measured in writes per second. One number could not say both.
//!
//! ## Why fan-out is compressed and not linear
//!
//! A weight above the burst can never be admitted
//! ([`QuotaError::WeightExceedsBurst`](super::gcra::QuotaError)), so a linear
//! fan-out price would make a popular topic permanently unwritable — the
//! limiter would convert success into an outage. Blast radius is therefore
//! priced **logarithmically**: it preserves the ordering that matters (a write
//! seen by a thousand people really should cost more than one seen by nobody)
//! while staying bounded by `log2(u32::MAX) ≈ 32`, so it fits inside any sane
//! burst by construction rather than by tuning.
//!
//! The limiter's job is to stop abuse, not to bill precisely. Compression is the
//! honest shape for that job.

use crate::effects::EffectProfile;

/// What kind of thing is being attempted, and therefore which limit it answers
/// to.
///
/// Ordered from most permissive to most restrictive, so the derived class of a
/// compound operation is `max` of its parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationClass {
    /// A render served from the manifest — no substrate, no network. Bounded by
    /// bandwidth rather than by anything we own.
    StaticRead,
    /// A read that reaches FORGE.
    Read,
    /// A write to FORGE: durable, and fans out to every subscribed lane.
    Write,
    /// An APERTURE call — **someone else's quota**. The most expensive thing an
    /// app can do on a per-request basis, and the only one where abuse can cost
    /// the operator money or get their API key revoked by a third party.
    Outbound,
    /// A credential operation: login, reset, token exchange. Cheap to execute
    /// and the tightest limit in the system, because what is being rationed is
    /// *guesses*, not resources.
    Credential,
}

impl OperationClass {
    /// Human name, for the report and for error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StaticRead => "static-read",
            Self::Read => "read",
            Self::Write => "write",
            Self::Outbound => "outbound",
            Self::Credential => "credential",
        }
    }

    /// What this class costs before blast radius is considered.
    ///
    /// These are small integers on a shared scale, not measurements — the
    /// *ratios* are the claim, and each is defensible: a FORGE read is a query
    /// where a static read is a memcpy; a write is a query plus a durable commit
    /// plus a fan-out; an outbound call is a network round trip against a quota
    /// that is not ours to spend.
    #[must_use]
    pub const fn base_weight(self) -> u32 {
        match self {
            Self::StaticRead => 1,
            Self::Read => 2,
            Self::Write => 4,
            Self::Outbound => 8,
            // Deliberately 1. A credential check is genuinely cheap; its
            // restriction comes from the quota it answers to, not from
            // pretending it burns resources it does not.
            Self::Credential => 1,
        }
    }
}

/// A derived cost, and the derivation that produced it.
///
/// The breakdown is carried rather than discarded because a limit that fires
/// without being explainable is a limit somebody will disable. `albedo doctor`
/// and CITRINITAS both want to print *why* — "this write costs 11: FORGE write
/// (4) + fan-out to 512 lanes (+7)" — which makes the number auditable instead
/// of magic. It is the same reason the authorization matrix is a derived
/// artefact rather than a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Cost {
    /// Which limit this answers to.
    pub class: OperationClass,
    /// Units charged against that limit.
    pub weight: u32,
    /// The class's contribution.
    pub base: u32,
    /// The blast-radius contribution, already compressed.
    pub fanout: u32,
    /// Subscribed lanes at the moment of derivation, before compression.
    /// Carried for the explanation, not for the arithmetic.
    pub observed_subscribers: u32,
}

impl Cost {
    /// A cost with no fan-out.
    #[must_use]
    pub const fn flat(class: OperationClass) -> Self {
        Self {
            class,
            weight: class.base_weight(),
            base: class.base_weight(),
            fanout: 0,
            observed_subscribers: 0,
        }
    }

    /// A write's cost, priced by what it will actually reach.
    ///
    /// `subscribers` is read from the live broadcast registry *before* the write
    /// commits, so this is the real blast radius and not an estimate. See the
    /// module docs for why it is compressed.
    #[must_use]
    pub fn fan_out(class: OperationClass, subscribers: u32) -> Self {
        let base = class.base_weight();
        let fanout = compress_fanout(subscribers);
        Self {
            class,
            weight: base.saturating_add(fanout),
            base,
            fanout,
            observed_subscribers: subscribers,
        }
    }

    /// The blast-radius half of a write's price, on its own.
    ///
    /// [`Self::fan_out`] is the whole price, and it is only computable once the
    /// write has resolved which channels it touches — which is after the request
    /// was admitted. So the price is paid in two parts: [`Self::flat`] at
    /// admission, this afterwards, through
    /// [`Shutter::debit`](super::Shutter::debit). The two sum to `fan_out`
    /// exactly, so splitting the payment does not change what the operation
    /// costs — only when the caller is told.
    ///
    /// Zero subscribers is a real answer, not a missing one: a write nobody is
    /// watching costs its base and nothing more.
    #[must_use]
    pub fn surcharge(class: OperationClass, subscribers: u32) -> Self {
        let fanout = compress_fanout(subscribers);
        Self {
            class,
            weight: fanout,
            base: 0,
            fanout,
            observed_subscribers: subscribers,
        }
    }

    /// One sentence explaining the number, for the report and the 429 body.
    #[must_use]
    pub fn explain(&self) -> String {
        if self.base == 0 && self.fanout > 0 {
            return format!(
                "{} fan-out to {} subscribed lane{} costs {}",
                self.class.as_str(),
                self.observed_subscribers,
                if self.observed_subscribers == 1 { "" } else { "s" },
                self.weight
            );
        }
        if self.fanout == 0 {
            format!(
                "{} costs {} ({} base)",
                self.class.as_str(),
                self.weight,
                self.base
            )
        } else {
            format!(
                "{} costs {} ({} base + {} for fan-out to {} subscribed lane{})",
                self.class.as_str(),
                self.weight,
                self.base,
                self.fanout,
                self.observed_subscribers,
                if self.observed_subscribers == 1 { "" } else { "s" }
            )
        }
    }
}

/// Blast radius, compressed to `floor(log2(n))`.
///
/// Bounded above by 32 for any `u32`, which is what keeps a derived weight
/// inside a sane burst without anyone tuning it.
#[must_use]
pub fn compress_fanout(subscribers: u32) -> u32 {
    match subscribers {
        0 | 1 => 0,
        n => n.ilog2(),
    }
}

/// Derive an operation's class from what the compiler already knows about it.
///
/// **The whole point of the module.** Nothing here is a policy the author wrote;
/// every input is a fact the build already established for other reasons —
/// the effect profile drives tiering, the FORGE write set drives the delta
/// kernel, the APERTURE binding drives the egress allowlist. The limit is a
/// by-product of facts, which is what makes it correct by default rather than
/// correct if someone remembered.
/// The class a manifest route answers to, from what the build recorded about it.
///
/// The manifest already lists every topic a route reads — PRISM partitions, an
/// APERTURE source, a plain shared slot — because the streaming render and the
/// subscribe lane both need that list. A route that declares none renders from
/// the manifest without touching the substrate; one that declares any reaches it
/// on every request.
///
/// 🔑 **Lives here, once.** The dispatcher charges by this and `albedo doctor`
/// prints by it, and the moment those two disagree the audit artefact stops
/// describing the running system. Compare the standing "three implementations of
/// the paint rule" trap — same shape, and this is the cut that prevents it.
#[must_use]
pub fn classify_route(route: &crate::manifest::schema::RouteManifest) -> OperationClass {
    if route.shared_slot_topics.is_empty()
        && route.shared_slot_partitions.is_empty()
        && route.shared_slot_sources.is_empty()
    {
        OperationClass::StaticRead
    } else {
        OperationClass::Read
    }
}

#[must_use]
pub fn classify(effects: EffectProfile, writes_forge: bool, calls_aperture: bool) -> OperationClass {
    // Ordered most-restrictive-first: an action that both writes and calls out
    // answers to the outbound limit, because the third party's quota is the
    // scarcer resource and the one whose exhaustion we cannot undo.
    if calls_aperture {
        return OperationClass::Outbound;
    }
    if writes_forge {
        return OperationClass::Write;
    }
    // `io` without a declared source or a FORGE write still reaches *something*
    // — the effect analysis saw an await on an external boundary. Priced as a
    // read rather than as static, because it is not free and we cannot see what
    // it is.
    if effects.io || effects.side_effects || effects.asynchronous {
        return OperationClass::Read;
    }
    OperationClass::StaticRead
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pure() -> EffectProfile {
        EffectProfile::pure()
    }

    fn with_io() -> EffectProfile {
        EffectProfile {
            io: true,
            ..EffectProfile::pure()
        }
    }

    #[test]
    fn a_pure_render_is_the_cheapest_thing_there_is() {
        assert_eq!(classify(pure(), false, false), OperationClass::StaticRead);
        assert_eq!(Cost::flat(OperationClass::StaticRead).weight, 1);
    }

    #[test]
    fn a_forge_write_outranks_a_read() {
        assert_eq!(classify(pure(), true, false), OperationClass::Write);
        assert!(OperationClass::Write > OperationClass::Read);
    }

    /// A third party's quota is the scarcer resource, and the only one whose
    /// exhaustion we cannot undo by adding capacity.
    #[test]
    fn an_outbound_call_outranks_everything_it_is_combined_with() {
        assert_eq!(classify(pure(), true, true), OperationClass::Outbound);
        assert_eq!(classify(with_io(), true, true), OperationClass::Outbound);
        assert_eq!(
            OperationClass::Outbound.max(OperationClass::Write),
            OperationClass::Outbound
        );
    }

    /// An effect the analysis saw but cannot name is not free. Pricing it as a
    /// static read would make "we could not tell" the cheapest answer, which is
    /// the wrong incentive for an analysis to have.
    #[test]
    fn unattributed_io_is_priced_as_a_read_not_as_static() {
        assert_eq!(classify(with_io(), false, false), OperationClass::Read);
        let side_effecting = EffectProfile {
            side_effects: true,
            ..EffectProfile::pure()
        };
        assert_eq!(
            classify(side_effecting, false, false),
            OperationClass::Read
        );
    }

    /// **The claim nobody else can make.** The same write costs more when more
    /// people are watching, because it genuinely does more work.
    #[test]
    fn a_write_costs_more_when_more_lanes_are_subscribed() {
        let quiet = Cost::fan_out(OperationClass::Write, 0);
        let busy = Cost::fan_out(OperationClass::Write, 512);
        assert!(
            busy.weight > quiet.weight,
            "fan-out is not priced: {} vs {}",
            quiet.weight,
            busy.weight
        );
        assert_eq!(quiet.weight, OperationClass::Write.base_weight());
        assert_eq!(busy.weight, 4 + 9);
    }

    /// The two-part payment must total the one-part price, or admission and
    /// surcharge would together charge a write something nobody derived.
    #[test]
    fn admission_plus_surcharge_equals_the_derived_price() {
        for subscribers in [0, 1, 2, 7, 512, u32::MAX] {
            let whole = Cost::fan_out(OperationClass::Write, subscribers);
            let admission = Cost::flat(OperationClass::Write);
            let after = Cost::surcharge(OperationClass::Write, subscribers);
            assert_eq!(
                admission.weight + after.weight,
                whole.weight,
                "split payment diverged from the derived price at {subscribers} subscribers"
            );
        }
    }

    /// A surcharge explains itself as a surcharge. "0 base" would read as a bug
    /// in the very message that exists to make the number auditable.
    #[test]
    fn a_surcharge_explains_itself_without_claiming_a_base() {
        let explanation = Cost::surcharge(OperationClass::Write, 512).explain();
        assert!(explanation.contains("512 subscribed lanes"), "{explanation}");
        assert!(!explanation.contains("0 base"), "{explanation}");
    }

    /// Compression is what stops a popular topic from becoming permanently
    /// unwritable. Linear pricing would turn success into an outage.
    #[test]
    fn fan_out_stays_inside_any_sane_burst() {
        assert_eq!(compress_fanout(0), 0);
        assert_eq!(compress_fanout(1), 0);
        assert_eq!(compress_fanout(2), 1);
        assert_eq!(compress_fanout(3), 1);
        assert_eq!(compress_fanout(4), 2);
        assert_eq!(compress_fanout(1_024), 10);

        // The bound is the property that matters: no subscriber count, however
        // absurd, can produce a weight that a reasonable burst cannot admit.
        assert!(compress_fanout(u32::MAX) <= 32);
        assert!(
            Cost::fan_out(OperationClass::Write, u32::MAX).weight < 64,
            "a maximally popular topic must still be writable"
        );
    }

    #[test]
    fn fan_out_pricing_is_monotonic() {
        let mut previous = 0;
        for subscribers in [0u32, 1, 2, 8, 64, 1_000, 100_000, u32::MAX] {
            let weight = Cost::fan_out(OperationClass::Write, subscribers).weight;
            assert!(
                weight >= previous,
                "pricing went backwards at {subscribers} subscribers"
            );
            previous = weight;
        }
    }

    /// Cost and posture are separate axes. A password check is genuinely cheap;
    /// its restriction lives in the quota it answers to.
    #[test]
    fn a_credential_check_is_cheap_but_answers_to_the_strictest_class() {
        let cost = Cost::flat(OperationClass::Credential);
        assert_eq!(cost.weight, 1, "a password check really is cheap to run");
        assert_eq!(
            OperationClass::Credential.max(OperationClass::Outbound),
            OperationClass::Credential,
            "credential work answers to the tightest limit despite costing least"
        );
    }

    /// A limit that fires without being explainable is a limit somebody will
    /// disable.
    #[test]
    fn a_cost_explains_itself_in_terms_a_human_can_check() {
        let explained = Cost::fan_out(OperationClass::Write, 512).explain();
        assert!(explained.contains("write costs 13"), "{explained}");
        assert!(explained.contains("512 subscribed lanes"), "{explained}");

        let singular = Cost::fan_out(OperationClass::Write, 2).explain();
        assert!(singular.contains("2 subscribed lanes"), "{singular}");
        assert!(Cost::flat(OperationClass::Read).explain().contains("2 base"));
    }
}
