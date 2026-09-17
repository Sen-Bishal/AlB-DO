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
    ExpireSessions,
}

impl Builtin {
    /// Every built-in, in the order they are registered.
    pub const ALL: &'static [Self] = &[Self::ExpireSessions];

    /// The queue name, including [`BUILTIN_PREFIX`].
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ExpireSessions => "albedo:expire_sessions",
        }
    }

    /// When it runs.
    #[must_use]
    pub fn schedule(self) -> Schedule {
        let source = match self {
            Self::ExpireSessions => "@hourly",
        };
        Schedule::parse(source).expect("a built-in's schedule is a literal in this file")
    }

    /// Attempts before it dead-letters.
    ///
    /// More than an app job's default of one: the failure mode is a transient
    /// database error, and giving up after the first would mean the table stops
    /// being swept until the next tick.
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        3
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
            let schedule = builtin.schedule();
            assert!(
                schedule.next_after(1_789_603_200_000).is_some(),
                "{} must have a next fire",
                builtin.name()
            );
        }
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
