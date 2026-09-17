//! When a scheduled job is next due.
//!
//! ## Why every instant here is derived from the epoch, never from "now"
//!
//! A scheduler that remembers *when it last ran* cannot survive a restart, a
//! second process, or a clock that moves. This one remembers nothing: every
//! schedule is a pure function from an instant to the next instant it fires, so
//! two processes that disagree about everything else still agree about the set
//! of fire times. That is what makes the deterministic fire id in
//! [`crate::jobs::queue`] a correctness property and not a convention — the id
//! is derived from an instant both processes compute independently and both get
//! right.
//!
//! The same rule governs [`Schedule::Every`]: the anchor is the Unix epoch, not
//! boot. `every 5m` fires at `:00, :05, :10`, the same five minutes on every
//! machine, rather than five minutes after whenever this process happened to
//! start. A boot-anchored interval would give two processes two different fire
//! sets, and no id could reconcile them.
//!
//! ## The five-field form is the declared surface
//!
//! `cron` parses the six-field form, seconds first. Nobody writes that. Every
//! crontab, every CI config and every operator's memory holds the five-field
//! form, so that is what a job declares, and the extra `0 ` is prepended here —
//! at the boundary, once, where it is visible and tested. The alternative is a
//! user writing `0 3 * * *` for 3 a.m. and getting a job that runs every minute
//! of the 3 a.m. hour, which is exactly the kind of silent-but-wrong this
//! codebase keeps finding.
//!
//! A six-field expression is still accepted, because the field count
//! disambiguates it completely and refusing it would only punish someone who
//! knew more than the surface required.

use std::str::FromStr;
use std::time::Duration;

use chrono::{TimeZone, Utc};

/// The shortest period a schedule may declare.
///
/// Not a performance limit — a bound on *declarations*. A job is a dispatch
/// onto the engine pool; one that asks to run ten times a second is asking for
/// the pool, and a typo (`every 10ms`) should fail at build rather than at 3
/// a.m. under a load nobody attributes to it. Raising it is a source edit,
/// which is the right amount of friction for "this app really does need that".
pub const MIN_PERIOD: Duration = Duration::from_secs(1);

/// How often a job runs, if it runs on its own.
///
/// A job with no schedule is not represented here: it is reached by
/// [`enqueue`](crate::jobs::queue::enqueue) and has no fire times at all.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// A cron expression, already normalised to the six-field form.
    Cron(Box<cron::Schedule>),
    /// A fixed period, anchored to the Unix epoch.
    Every(Duration),
}

/// What a schedule string failed to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// Not a cron expression, an `@alias`, or an `every <duration>`.
    Unparseable {
        /// What was written.
        value: String,
        /// What the parser objected to.
        reason: String,
    },
    /// An `every` duration that no unit suffix could be read from.
    BadDuration {
        /// What was written.
        value: String,
    },
    /// A period under [`MIN_PERIOD`].
    TooFrequent {
        /// What was written.
        value: String,
        /// The floor, in milliseconds.
        floor_ms: u128,
    },
    /// A cron expression that parses but can never fire (`0 0 30 2 *`).
    NeverFires {
        /// What was written.
        value: String,
    },
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable { value, reason } => write!(
                f,
                "`{value}` is not a schedule: {reason}. Write a five-field cron expression \
                 (`0 3 * * *`), an alias (`@daily`, `@hourly`), or an interval (`every 5m`)"
            ),
            Self::BadDuration { value } => write!(
                f,
                "`{value}` has no readable duration. Write a number and a unit: \
                 `every 30s`, `every 5m`, `every 2h`, `every 1d`"
            ),
            Self::TooFrequent { value, floor_ms } => write!(
                f,
                "`{value}` asks to run more often than once every {floor_ms}ms, which is the \
                 floor for a declared schedule. Use a job that re-enqueues itself if you need \
                 a tighter loop than that"
            ),
            Self::NeverFires { value } => write!(
                f,
                "`{value}` parses but names an instant that never arrives, so the job would \
                 never run. Check the day-of-month against the month"
            ),
        }
    }
}

impl Schedule {
    /// Read a schedule from what a job declared.
    ///
    /// # Errors
    /// [`ScheduleError`], naming the written value in every case — the message
    /// is shown to the author at `albedo build`, so it has to be about their
    /// string and not about ours.
    pub fn parse(value: &str) -> Result<Self, ScheduleError> {
        let trimmed = value.trim();

        if let Some(rest) = trimmed.strip_prefix('@') {
            return Self::from_alias(rest, trimmed);
        }
        if let Some(rest) = trimmed.strip_prefix("every ") {
            return Self::from_interval(rest.trim(), trimmed);
        }

        let normalised = normalise_cron(trimmed)?;
        let schedule = cron::Schedule::from_str(&normalised).map_err(|err| {
            ScheduleError::Unparseable {
                value: trimmed.to_string(),
                reason: err.to_string(),
            }
        })?;

        // A schedule with no upcoming instant is a typo that would otherwise
        // present as "the job silently never ran".
        if schedule.upcoming(Utc).next().is_none() {
            return Err(ScheduleError::NeverFires {
                value: trimmed.to_string(),
            });
        }
        Ok(Self::Cron(Box::new(schedule)))
    }

    fn from_alias(rest: &str, original: &str) -> Result<Self, ScheduleError> {
        let expression = match rest {
            "minutely" => "0 * * * * *",
            "hourly" => "0 0 * * * *",
            "daily" | "midnight" => "0 0 0 * * *",
            "weekly" => "0 0 0 * * SUN",
            "monthly" => "0 0 0 1 * *",
            "yearly" | "annually" => "0 0 0 1 1 *",
            other => {
                return Err(ScheduleError::Unparseable {
                    value: original.to_string(),
                    reason: format!(
                        "`@{other}` is not a known alias (@minutely, @hourly, @daily, \
                         @weekly, @monthly, @yearly)"
                    ),
                })
            }
        };
        let schedule =
            cron::Schedule::from_str(expression).expect("the alias table holds valid expressions");
        Ok(Self::Cron(Box::new(schedule)))
    }

    fn from_interval(rest: &str, original: &str) -> Result<Self, ScheduleError> {
        let period = parse_duration(rest).ok_or_else(|| ScheduleError::BadDuration {
            value: original.to_string(),
        })?;
        if period < MIN_PERIOD {
            return Err(ScheduleError::TooFrequent {
                value: original.to_string(),
                floor_ms: MIN_PERIOD.as_millis(),
            });
        }
        Ok(Self::Every(period))
    }

    /// The first instant this schedule fires strictly after `after_ms`.
    ///
    /// Strictly after, so feeding a fire time back in yields the *next* one and
    /// a runner cannot re-derive the tick it just handled.
    ///
    /// `None` only when a cron expression has run out of upcoming instants,
    /// which for a schedule that fires at all means the year 2100-odd bound
    /// inside `cron` — a runner treats it as "never again", not as an error.
    #[must_use]
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        match self {
            Self::Cron(schedule) => {
                let after = Utc.timestamp_millis_opt(after_ms).single()?;
                schedule.after(&after).next().map(|dt| dt.timestamp_millis())
            }
            Self::Every(period) => {
                // Epoch-anchored, so every process computes the same set.
                // `as i64` is safe for any period that passed `MIN_PERIOD` and
                // any instant this century.
                let period_ms = i64::try_from(period.as_millis()).ok()?;
                if period_ms <= 0 {
                    return None;
                }
                // Floor-divide so instants before the epoch (a clock far in the
                // past) still land on a grid point rather than rounding toward
                // zero and firing twice.
                let elapsed = after_ms.div_euclid(period_ms);
                elapsed.checked_add(1)?.checked_mul(period_ms)
            }
        }
    }
}

/// Turn what an author wrote into the six-field form `cron` parses.
fn normalise_cron(value: &str) -> Result<String, ScheduleError> {
    let fields = value.split_whitespace().count();
    match fields {
        // The declared surface. Seconds are not expressible, on purpose: a
        // sub-minute schedule is `every 30s`, which says what it means.
        5 => Ok(format!("0 {value}")),
        // Already seconds-first, or seconds-first with a year.
        6 | 7 => Ok(value.to_string()),
        other => Err(ScheduleError::Unparseable {
            value: value.to_string(),
            reason: format!(
                "a cron expression has five fields (minute hour day-of-month month \
                 day-of-week); this has {other}"
            ),
        }),
    }
}

/// `30s`, `5m`, `2h`, `1d` — and nothing else.
fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().ok()?;
    if number == 0 {
        return None;
    }
    let seconds = match unit.trim() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
        "d" | "day" | "days" => 24 * 60 * 60,
        _ => return None,
    };
    number.checked_mul(seconds).map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-17T00:00:00Z, a Thursday.
    const NOON: i64 = 1_789_603_200_000;

    #[test]
    fn the_five_field_form_means_what_a_crontab_means() {
        // The trap this normalisation exists for: read as six fields,
        // `0 3 * * *` means "every minute of the 3 a.m. hour". Read as five,
        // it means 03:00. It must mean 03:00.
        let schedule = Schedule::parse("0 3 * * *").expect("five fields parse");
        let first = schedule.next_after(NOON).expect("fires");
        let second = schedule.next_after(first).expect("fires again");
        assert_eq!(
            second - first,
            24 * 60 * 60 * 1000,
            "consecutive fires must be a day apart, not a minute"
        );
    }

    #[test]
    fn a_six_field_expression_is_still_accepted() {
        let schedule = Schedule::parse("0 0 3 * * *").expect("six fields parse");
        let first = schedule.next_after(NOON).expect("fires");
        assert_eq!(
            schedule.next_after(first).expect("fires again") - first,
            24 * 60 * 60 * 1000
        );
    }

    #[test]
    fn intervals_are_anchored_to_the_epoch_not_to_now() {
        let schedule = Schedule::parse("every 5m").expect("parses");
        // Two processes, two different "nows" inside the same five-minute
        // window, must agree on the next fire — that agreement is what the
        // deterministic fire id rests on.
        let from_early = schedule.next_after(NOON + 1).expect("fires");
        let from_late = schedule.next_after(NOON + 4 * 60 * 1000).expect("fires");
        assert_eq!(from_early, from_late);
        assert_eq!(from_early % (5 * 60 * 1000), 0, "lands on a grid point");
    }

    #[test]
    fn next_after_is_strict_so_a_tick_cannot_re_derive_itself() {
        let schedule = Schedule::parse("every 1h").expect("parses");
        let fire = schedule.next_after(NOON).expect("fires");
        assert!(
            schedule.next_after(fire).expect("fires again") > fire,
            "feeding a fire time back must advance, or the runner loops on one tick"
        );
    }

    #[test]
    fn aliases_resolve() {
        for alias in ["@minutely", "@hourly", "@daily", "@weekly", "@monthly", "@yearly"] {
            let schedule = Schedule::parse(alias).unwrap_or_else(|e| panic!("{alias}: {e}"));
            assert!(schedule.next_after(NOON).is_some(), "{alias} must fire");
        }
    }

    #[test]
    fn a_schedule_tighter_than_the_floor_is_refused() {
        let err = Schedule::parse("every 0s").expect_err("refused");
        assert!(matches!(err, ScheduleError::BadDuration { .. }));
    }

    #[test]
    fn the_wrong_field_count_says_how_many_it_saw() {
        let err = Schedule::parse("0 3 * *").expect_err("refused");
        let ScheduleError::Unparseable { reason, .. } = &err else {
            panic!("expected an unparseable, got {err:?}");
        };
        assert!(reason.contains('4'), "the message must name the count: {reason}");
    }

    #[test]
    fn nonsense_is_refused_rather_than_defaulted() {
        for bad in ["", "soon", "@fortnightly", "every", "every 5 bananas", "* * *"] {
            assert!(
                Schedule::parse(bad).is_err(),
                "`{bad}` must not silently become a schedule"
            );
        }
    }

    #[test]
    fn durations_read_every_unit_they_advertise() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("5 minutes"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("5x"), None);
        assert_eq!(parse_duration("m"), None);
    }
}
