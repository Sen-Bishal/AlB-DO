//! Jobs the framework declares for itself.
//!
//! ## Why these exist at all
//!
//! Some of the framework's own work is periodic, and until 15.5 there was
//! nowhere to put it — so it was written and then never called.
//!
//! [`auth::store::purge_expired_sessions`](crate::auth::store::purge_expired_sessions)
//! is the example that produced this module. It has been correct since AUTH P0
//! and, at the point 15.5 was started, had **exactly one caller in the whole
//! repository: a test**. Every deployed app has therefore been accumulating
//! expired rows in `albedo_sessions` forever, because the function that removes
//! them had no periodic thing to run it. That is this project's signature
//! defect — a fact fully computed and nothing consuming it — and a scheduler is
//! precisely the consumer it was missing.
//!
//! ## Why they are not written in `src/jobs.ts`
//!
//! They must run in an app that has no jobs file at all, they touch reserved
//! tables an app cannot name, and an app must not be able to delete or reschedule
//! them by editing a file. They run in Rust, on the same queue, under the same
//! lease, retries and dead-lettering — so an operator inspecting `albedo_jobs`
//! sees the framework's work next to their own, in one place, with one set of
//! rules.
//!
//! ## The prefix is what keeps the two apart
//!
//! A built-in's name starts with [`BUILTIN_PREFIX`], which contains a character
//! no JavaScript identifier may contain. An app therefore **cannot** declare a
//! job that collides with one of these, and the runner can tell from the name
//! alone whether a claimed row dispatches to Rust or to the engine — with no
//! lookup that could disagree with itself.

use crate::jobs::schedule::Schedule;

/// Prefix on every framework-declared job name.
///
/// The `:` is load-bearing: `export const albedo:x = …` is a syntax error, so
/// no app can declare a name in this space by accident or on purpose.
pub const BUILTIN_PREFIX: &str = "albedo:";

/// Work the framework schedules for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// Delete session rows whose `expires_at` has passed.
    ///
    /// Hourly. An expired session is already refused on every request — this is
    /// about the table not growing without bound, which is a disk problem rather
    /// than a security one, so it does not need to be prompt.
    ///
    /// Also sweeps expired mailed tokens (15.3), for the same reason.
    ExpireSessions,
    /// EMAIL · 15.3 — send one message. Never scheduled: every row is a
    /// `sendEmail()` call or an AUTH flow, and its `args` is a
    /// [`QueuedEmail`](crate::email::QueuedEmail).
    SendEmail,
}

impl Builtin {
    /// Every built-in, in the order they are registered.
    pub const ALL: &'static [Self] = &[Self::ExpireSessions, Self::SendEmail];

    /// The queue name, including [`BUILTIN_PREFIX`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ExpireSessions => "albedo:expire_sessions",
            Self::SendEmail => "albedo:send_email",
        }
    }

    /// When it runs, or `None` for a built-in that runs only when enqueued.
    #[must_use]
    pub fn schedule(self) -> Option<Schedule> {
        let source = match self {
            Self::ExpireSessions => "@hourly",
            Self::SendEmail => return None,
        };
        Some(Schedule::parse(source).expect("a built-in's schedule is a literal in this file"))
    }

    /// Attempts before it dead-letters.
    ///
    /// The sweep gets three: its failure mode is a transient database error, and
    /// the next tick is an hour away. A send gets thirteen: the queue's backoff
    /// doubles from 1 s, so twelve waits sum to 4 095 s before jitter — between
    /// 34 and 68 minutes of retrying, which outlasts the provider incidents
    /// that actually happen. A *permanent*
    /// refusal (a revoked key, an unverified domain) does not spend them: it
    /// dead-letters on the first answer.
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        match self {
            Self::ExpireSessions => 3,
            Self::SendEmail => 13,
        }
    }

    /// Whether app code may put this on the queue — through `sendEmail()`,
    /// which is `enqueue` of this name with the message validated on the way in.
    #[must_use]
    pub const fn is_enqueueable(self) -> bool {
        matches!(self, Self::SendEmail)
    }

    /// Whether this name belongs to the framework rather than to an app.
    #[must_use]
    pub fn is_builtin(name: &str) -> bool {
        name.starts_with(BUILTIN_PREFIX)
    }

    /// Look one up by queue name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|b| b.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_round_trips_through_its_name() {
        for builtin in Builtin::ALL {
            assert_eq!(Builtin::from_name(builtin.name()), Some(*builtin));
            assert!(Builtin::is_builtin(builtin.name()));
        }
    }

    #[test]
    fn every_builtin_schedule_parses_and_fires() {
        for builtin in Builtin::ALL {
            let Some(schedule) = builtin.schedule() else {
                assert!(builtin.is_enqueueable(), "{} would never run", builtin.name());
                continue;
            };
            assert!(
                schedule.next_after(1_789_603_200_000).is_some(),
                "{} must have a next fire",
                builtin.name()
            );
        }
    }

    /// The claim in `max_attempts`' doc, measured against the queue's own curve.
    #[test]
    fn the_send_retry_window_spans_half_an_hour_to_an_hour() {
        let waits = Builtin::SendEmail.max_attempts() - 1;
        let total = |jitter| -> i64 {
            (1..=waits).map(|a| crate::jobs::queue::backoff_ms(a, jitter)).sum()
        };
        assert_eq!(total(1.0), 4_095_000);
        assert!(total(0.5) >= 30 * 60 * 1000, "{}", total(0.5));
    }

    /// The framework's sweep must not be triggerable from app code.
    #[test]
    fn only_the_send_is_enqueueable() {
        assert!(Builtin::SendEmail.is_enqueueable());
        assert!(!Builtin::ExpireSessions.is_enqueueable());
    }

    /// The collision argument, stated as a test: a built-in's name cannot be
    /// spelled as a JavaScript export, so an app cannot shadow one.
    #[test]
    fn a_builtin_name_is_not_a_valid_javascript_identifier() {
        for builtin in Builtin::ALL {
            let name = builtin.name();
            assert!(
                name.contains(':'),
                "{name} must carry a character no identifier may contain"
            );
        }
    }

    #[test]
    fn an_app_job_name_is_never_mistaken_for_a_builtin() {
        for name in ["nightly", "expire_sessions", "albedo_expire", "send_email"] {
            assert!(
                !Builtin::is_builtin(name),
                "{name} must dispatch to the engine, not to Rust"
            );
        }
    }
}
