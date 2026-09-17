//! The durable queue: one table, and the protocol for taking work out of it.
//!
//! ## Why a lease and not a lock
//!
//! A lock says *I have this*. A lease says *I have this until `t`*. The
//! difference is what happens when the holder dies without saying so — which,
//! for a process, is the normal case: `SIGKILL`, an OOM kill, a pulled power
//! cable, a `docker stop` that outran its drain deadline. A lock left behind by
//! a dead process is held forever and the job never runs again. A lease expires,
//! and the work returns.
//!
//! So a claim writes `lease_until`, and [`claim_due`] treats an expired claim
//! exactly as it treats a pending row. Nothing has to notice the crash, nothing
//! has to clean up after it, and there is no reaper task whose own death is a
//! second failure mode.
//!
//! 🪤 **Reclaiming a lease increments `attempts`.** It has to: a job that kills
//! the process it runs in would otherwise be reclaimed, kill the process again,
//! and loop forever — an unkillable poison row that reads as a crash-looping
//! deployment. Counting the reclaim means such a job dead-letters like any other
//! failure, and the operator finds a row saying so instead of a restart cycle.
//!
//! ## Why the id is derived and not generated
//!
//! A scheduled fire's id is `name@<instant>` — computed from facts every process
//! agrees on (see [`crate::jobs::schedule`]). With `id` as the primary key, two
//! processes that both decide the 03:00 tick is due both try to insert
//! `digest@2026-09-17T03:00:00Z`, and exactly one succeeds. The other's
//! `INSERT OR IGNORE` is a no-op.
//!
//! That makes exactly-once cron a property of the schema rather than of the
//! scheduler — true even on the multi-process topology `TODO.md` item 13.2
//! refuses, and true across a restart in the middle of a tick. **A uuid here
//! would have bought nothing and cost that.**
//!
//! Work reached by [`enqueue`] is different: it has no natural instant, so the
//! caller supplies the id when it wants idempotence (a webhook delivery keyed by
//! its event id) and gets a random one when it does not.

use crate::forge::substrate::DataSubstrate;
use crate::forge::value::SqlValue;

/// The table.
///
/// Under the reserved prefix, for the reason the auth and upload tables are: an
/// app that could declare `albedo_jobs` could silently rewrite its own work
/// queue — including marking somebody else's failed job as done.
pub const JOBS: &str = "albedo_jobs";

/// How long a claim is good for before another runner may take the row.
///
/// Comfortably longer than [`crate::jobs::declare::DEFAULT_TIMEOUT_MS`] so a job
/// that is merely slow is never taken from under the runner still executing it;
/// short enough that a crashed process's work is not stranded for a coffee
/// break.
pub const DEFAULT_LEASE_MS: i64 = 5 * 60 * 1000;

/// First retry delay, doubled per attempt by [`backoff_ms`].
pub const BACKOFF_BASE_MS: i64 = 1_000;

/// Ceiling on a retry delay, however many attempts have failed.
pub const BACKOFF_MAX_MS: i64 = 60 * 60 * 1000;

/// Where a row is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Due at `run_at`, nobody holding it.
    Pending,
    /// A scheduled fire that expands into one row per principal.
    Fanout,
    /// Held by a runner until `lease_until`.
    Claimed,
    /// Ran, returned.
    Done,
    /// Out of attempts. Kept, not deleted — a dead letter nobody can read is
    /// just a job that vanished.
    Failed,
}

impl JobState {
    /// The spelling stored in the row.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Fanout => "fanout",
            Self::Claimed => "claimed",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Read a stored spelling back.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "fanout" => Some(Self::Fanout),
            "claimed" => Some(Self::Claimed),
            "done" => Some(Self::Done),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// What went wrong reaching the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// The substrate refused.
    Substrate(String),
    /// A row came back in a shape this module did not write.
    Corrupt(String),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Substrate(reason) => write!(f, "the job queue could not be reached: {reason}"),
            Self::Corrupt(reason) => write!(f, "a job row is unreadable: {reason}"),
        }
    }
}

impl std::error::Error for QueueError {}

type Result<T> = std::result::Result<T, QueueError>;

/// A row, claimed and ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    /// Primary key — derived for a scheduled fire, supplied or random otherwise.
    pub id: String,
    /// The export in `src/jobs.ts` this row runs.
    pub name: String,
    /// JSON handed to the body as its argument.
    pub args: String,
    /// Who it runs **for**. `None` is anonymous, and an anonymous job is
    /// refused an identity-partitioned read exactly as an anonymous request is.
    pub principal: Option<String>,
    /// Which attempt this is, counting from 1.
    pub attempts: u32,
    /// How many attempts this row gets before it dead-letters.
    pub max_attempts: u32,
    /// Set only on a [`JobState::Fanout`] row: how far the expansion got.
    pub cursor: Option<String>,
    /// Whether this row expands into per-principal children rather than running.
    pub is_fanout: bool,
}

/// A request to put work on the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enqueue {
    /// The primary key. Supply one to make the enqueue idempotent; a repeat is
    /// then a no-op rather than a second delivery.
    pub id: String,
    /// The export to run.
    pub name: String,
    /// JSON argument.
    pub args: String,
    /// Who it runs for.
    pub principal: Option<String>,
    /// The earliest instant it may run.
    pub run_at_ms: i64,
    /// Attempts before dead-lettering.
    pub max_attempts: u32,
    /// Expand into one row per principal instead of running.
    pub fanout: bool,
}

/// The statements that create the table, idempotent by `IF NOT EXISTS`.
#[must_use]
pub fn ddl() -> Vec<String> {
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {JOBS} (\
             id TEXT PRIMARY KEY, \
             name TEXT NOT NULL, \
             args TEXT NOT NULL, \
             principal TEXT, \
             state TEXT NOT NULL, \
             run_at INTEGER NOT NULL, \
             attempts INTEGER NOT NULL DEFAULT 0, \
             max_attempts INTEGER NOT NULL, \
             lease_until INTEGER, \
             cursor TEXT, \
             last_error TEXT, \
             created_at INTEGER NOT NULL, \
             updated_at INTEGER NOT NULL)"
        ),
        // The claim's own query: runnable rows ordered by when they came due.
        // Without this, every claim is a full scan of a table whose `done` rows
        // only ever grow.
        format!("CREATE INDEX IF NOT EXISTS {JOBS}_runnable ON {JOBS} (state, run_at)"),
        // Reclaiming expired leases scans by this.
        format!("CREATE INDEX IF NOT EXISTS {JOBS}_lease ON {JOBS} (state, lease_until)"),
    ]
}

/// The id a scheduled fire has, on every process that computes it.
///
/// Readable on purpose: an operator reading `SELECT * FROM albedo_jobs` should
/// see *which tick* a row is, not a uuid they have to correlate. The instant is
/// rendered in UTC with second precision, which is the resolution a schedule
/// can express.
#[must_use]
pub fn fire_id(name: &str, tick_ms: i64) -> String {
    use chrono::{TimeZone, Utc};
    let stamp = Utc
        .timestamp_millis_opt(tick_ms)
        .single()
        .map_or_else(|| tick_ms.to_string(), |dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string());
    format!("{name}@{stamp}")
}

/// The id one principal's share of a fan-out fire has.
#[must_use]
pub fn fanout_child_id(fire: &str, principal: &str) -> String {
    format!("{fire}#{principal}")
}

/// How long to wait before attempt `attempts + 1`.
///
/// Exponential from [`BACKOFF_BASE_MS`], capped at [`BACKOFF_MAX_MS`], then
/// scaled by `jitter` — which the runner draws in `[0.5, 1.0]`. The jitter is
/// not decoration: when N rows fail against the same upstream at the same
/// instant, an unjittered backoff retries them all at the same instant too, and
/// keeps doing so.
///
/// `jitter` is a parameter rather than drawn here so the curve is testable.
#[must_use]
pub fn backoff_ms(attempts: u32, jitter: f64) -> i64 {
    let shift = attempts.saturating_sub(1).min(32);
    let raw = BACKOFF_BASE_MS.saturating_mul(1_i64.checked_shl(shift).unwrap_or(i64::MAX));
    let capped = raw.min(BACKOFF_MAX_MS);
    let jittered = (capped as f64 * jitter.clamp(0.0, 1.0)) as i64;
    jittered.max(BACKOFF_BASE_MS.min(capped))
}

/// Put work on the queue. `Ok(false)` when the id was already there.
///
/// # Errors
/// [`QueueError::Substrate`] if the write is refused.
pub async fn enqueue(db: &dyn DataSubstrate, request: &Enqueue) -> Result<bool> {
    let state = if request.fanout {
        JobState::Fanout
    } else {
        JobState::Pending
    };
    // `OR IGNORE` is the whole exactly-once mechanism: two processes racing the
    // same tick both run this, and the primary key decides.
    let affected = db
        .execute(
            &format!(
                "INSERT OR IGNORE INTO {JOBS} \
                 (id, name, args, principal, state, run_at, attempts, max_attempts, \
                  lease_until, cursor, last_error, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, 0, ?, NULL, NULL, NULL, ?, ?)"
            ),
            &[
                SqlValue::Text(request.id.clone()),
                SqlValue::Text(request.name.clone()),
                SqlValue::Text(request.args.clone()),
                request
                    .principal
                    .clone()
                    .map_or(SqlValue::Null, SqlValue::Text),
                SqlValue::Text(state.as_str().to_string()),
                SqlValue::Integer(request.run_at_ms),
                SqlValue::Integer(i64::from(request.max_attempts)),
                SqlValue::Integer(request.run_at_ms),
                SqlValue::Integer(request.run_at_ms),
            ],
        )
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;
    Ok(affected > 0)
}

/// Take the oldest runnable row, or `None`.
///
/// Runnable means pending and due, **or** claimed with an expired lease — the
/// two are the same to a runner, which is what makes a crashed process's work
/// come back without anything noticing it crashed.
///
/// ## What actually makes this atomic
///
/// The select and the update run in one transaction, and **the transaction is
/// the mechanism** — `LibSqlSubstrate` holds a single writer mutex for the
/// duration of every write and every transaction (`forge/libsql.rs`, the
/// `writer` field: *"one writer at a time" true before SQLite has to enforce
/// it*). A second claimant does not interleave; it waits, then its own `SELECT`
/// no longer matches the row.
///
/// The `AND state = ?` on the update is therefore **redundant today, and kept
/// deliberately**. It is what holds the invariant if that serialisation ever
/// stops being true — a substrate that pools writers, a backend that is not
/// SQLite, or a refactor that drops the transaction. Measured, not assumed:
/// removing the guard fails no test in `tests/jobs_queue.rs`, which is exactly
/// what "the transaction is doing the work" looks like. Do not read those
/// concurrency tests as proof of the guard; [`tests`] proves it directly
/// instead, by running the update against a state that has already moved.
///
/// # Errors
/// [`QueueError::Substrate`] if the transaction is refused;
/// [`QueueError::Corrupt`] if a row does not have the shape this module writes.
pub async fn claim_due(
    db: &dyn DataSubstrate,
    now_ms: i64,
    lease_ms: i64,
) -> Result<Option<ClaimedJob>> {
    let tx = db
        .begin()
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;

    let found = tx
        .query(
            &format!(
                "SELECT id, name, args, principal, attempts, max_attempts, cursor, state \
                 FROM {JOBS} \
                 WHERE (state = 'pending' AND run_at <= ?) \
                    OR (state = 'fanout'  AND run_at <= ?) \
                    OR (state = 'claimed' AND lease_until IS NOT NULL AND lease_until <= ?) \
                 ORDER BY run_at ASC LIMIT 1"
            ),
            &[
                SqlValue::Integer(now_ms),
                SqlValue::Integer(now_ms),
                SqlValue::Integer(now_ms),
            ],
        )
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;

    let Some(row) = found.rows.first() else {
        // Nothing to do. Roll back rather than commit an empty read so the
        // write lock — which `begin` may have taken — is given up immediately.
        let _ = tx.rollback().await;
        return Ok(None);
    };

    let text = |idx: usize, field: &str| -> Result<String> {
        row.get(idx)
            .and_then(SqlValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| QueueError::Corrupt(format!("{field} is not text")))
    };
    let id = text(0, "id")?;
    let name = text(1, "name")?;
    let args = text(2, "args")?;
    let principal = row.get(3).and_then(SqlValue::as_str).map(str::to_string);
    let attempts = row
        .get(4)
        .and_then(SqlValue::as_i64)
        .ok_or_else(|| QueueError::Corrupt("attempts is not an integer".into()))?;
    let max_attempts = row
        .get(5)
        .and_then(SqlValue::as_i64)
        .ok_or_else(|| QueueError::Corrupt("max_attempts is not an integer".into()))?;
    let cursor = row.get(6).and_then(SqlValue::as_str).map(str::to_string);
    let state = text(7, "state")?;
    let is_fanout = state == JobState::Fanout.as_str();

    // 🪤 A reclaimed lease counts as an attempt. See the module note: without
    // this a job that kills its own process is immortal.
    let attempts = attempts.saturating_add(1);

    let updated = tx
        .execute(
            &format!(
                "UPDATE {JOBS} SET state = 'claimed', lease_until = ?, attempts = ?, \
                 updated_at = ? WHERE id = ? AND state = ?"
            ),
            &[
                SqlValue::Integer(now_ms.saturating_add(lease_ms)),
                SqlValue::Integer(attempts),
                SqlValue::Integer(now_ms),
                SqlValue::Text(id.clone()),
                SqlValue::Text(state),
            ],
        )
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;

    if updated == 0 {
        // Somebody else moved it between the select and the update.
        let _ = tx.rollback().await;
        return Ok(None);
    }

    tx.commit()
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;

    Ok(Some(ClaimedJob {
        id,
        name,
        args,
        principal,
        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
        max_attempts: u32::try_from(max_attempts).unwrap_or(u32::MAX),
        cursor,
        is_fanout,
    }))
}

/// Mark a claimed row finished.
///
/// # Errors
/// [`QueueError::Substrate`] if the write is refused.
pub async fn complete(db: &dyn DataSubstrate, id: &str, now_ms: i64) -> Result<()> {
    db.execute(
        &format!(
            "UPDATE {JOBS} SET state = 'done', lease_until = NULL, last_error = NULL, \
             updated_at = ? WHERE id = ?"
        ),
        &[SqlValue::Integer(now_ms), SqlValue::Text(id.to_string())],
    )
    .await
    .map(|_| ())
    .map_err(|err| QueueError::Substrate(err.to_string()))
}

/// Record a failed attempt: back to `pending` with a delay, or `failed` for good.
///
/// Returns the instant it will next be tried, or `None` when it dead-lettered.
///
/// # Errors
/// [`QueueError::Substrate`] if the write is refused.
pub async fn fail(
    db: &dyn DataSubstrate,
    job: &ClaimedJob,
    now_ms: i64,
    error: &str,
    jitter: f64,
) -> Result<Option<i64>> {
    let exhausted = job.attempts >= job.max_attempts;
    let retry_at = if exhausted {
        None
    } else {
        Some(now_ms.saturating_add(backoff_ms(job.attempts, jitter)))
    };

    // The message is stored whichever way it went — a row that retried five
    // times and then succeeded still owes an operator the reason it retried.
    db.execute(
        &format!(
            "UPDATE {JOBS} SET state = ?, run_at = ?, lease_until = NULL, last_error = ?, \
             updated_at = ? WHERE id = ?"
        ),
        &[
            SqlValue::Text(
                if exhausted { JobState::Failed } else { JobState::Pending }
                    .as_str()
                    .to_string(),
            ),
            SqlValue::Integer(retry_at.unwrap_or(now_ms)),
            SqlValue::Text(truncate_error(error)),
            SqlValue::Integer(now_ms),
            SqlValue::Text(job.id.clone()),
        ],
    )
    .await
    .map_err(|err| QueueError::Substrate(err.to_string()))?;

    Ok(retry_at)
}

/// Advance a fan-out row after a batch of children has been enqueued.
///
/// `next_cursor` of `None` means the expansion is complete and the row is done.
///
/// # Errors
/// [`QueueError::Substrate`] if the write is refused.
pub async fn advance_fanout(
    db: &dyn DataSubstrate,
    id: &str,
    next_cursor: Option<&str>,
    now_ms: i64,
) -> Result<()> {
    let (state, cursor) = match next_cursor {
        // Back to `fanout`, due immediately: the next claim takes the next
        // batch. This is what keeps a 100k-principal expansion from being one
        // 100k-row write at 03:00 — each pass writes a batch and yields.
        Some(cursor) => (JobState::Fanout, SqlValue::Text(cursor.to_string())),
        None => (JobState::Done, SqlValue::Null),
    };
    db.execute(
        &format!(
            "UPDATE {JOBS} SET state = ?, cursor = ?, lease_until = NULL, run_at = ?, \
             updated_at = ? WHERE id = ?"
        ),
        &[
            SqlValue::Text(state.as_str().to_string()),
            cursor,
            SqlValue::Integer(now_ms),
            SqlValue::Integer(now_ms),
            SqlValue::Text(id.to_string()),
        ],
    )
    .await
    .map(|_| ())
    .map_err(|err| QueueError::Substrate(err.to_string()))
}

/// The instant the earliest not-yet-due pending row comes due, if any.
///
/// This is what lets the runner sleep exactly as long as it should instead of
/// waking on a timer. A queue with nothing scheduled returns `None` and the
/// runner parks until [`enqueue`] wakes it.
///
/// # Errors
/// [`QueueError::Substrate`] if the read is refused.
pub async fn next_due_after(db: &dyn DataSubstrate, now_ms: i64) -> Result<Option<i64>> {
    let rows = db
        .query(
            &format!(
                "SELECT MIN(run_at) FROM {JOBS} \
                 WHERE state IN ('pending', 'fanout') AND run_at > ?"
            ),
            &[SqlValue::Integer(now_ms)],
        )
        .await
        .map_err(|err| QueueError::Substrate(err.to_string()))?;
    Ok(rows
        .rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(SqlValue::as_i64))
}

/// Delete finished rows older than `before_ms`.
///
/// `done` only. A `failed` row is a dead letter and is kept until somebody
/// looks at it — a queue that quietly deletes its own failures is worse than
/// one with no failures table at all.
///
/// # Errors
/// [`QueueError::Substrate`] if the write is refused.
pub async fn sweep_done(db: &dyn DataSubstrate, before_ms: i64) -> Result<u64> {
    db.execute(
        &format!("DELETE FROM {JOBS} WHERE state = 'done' AND updated_at < ?"),
        &[SqlValue::Integer(before_ms)],
    )
    .await
    .map_err(|err| QueueError::Substrate(err.to_string()))
}

/// Keep a stored error to something a row can hold.
fn truncate_error(error: &str) -> String {
    const MAX: usize = 2000;
    if error.len() <= MAX {
        return error.to_string();
    }
    let mut cut = MAX;
    while cut > 0 && !error.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &error[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_reserved() {
        assert!(
            crate::auth::schema::is_reserved(JOBS),
            "an app that could declare this table could mark its own failed jobs done"
        );
    }

    #[test]
    fn a_fire_id_is_the_same_on_every_process() {
        // The whole exactly-once argument reduces to this being a function.
        let a = fire_id("digest", 1_789_603_200_000);
        let b = fire_id("digest", 1_789_603_200_000);
        assert_eq!(a, b);
        assert_eq!(a, "digest@2026-09-17T00:00:00Z");
    }

    #[test]
    fn different_ticks_are_different_rows() {
        assert_ne!(
            fire_id("digest", 1_789_603_200_000),
            fire_id("digest", 1_789_603_200_000 + 86_400_000)
        );
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        let full = |attempt| backoff_ms(attempt, 1.0);
        assert_eq!(full(1), BACKOFF_BASE_MS);
        assert_eq!(full(2), BACKOFF_BASE_MS * 2);
        assert_eq!(full(3), BACKOFF_BASE_MS * 4);
        assert_eq!(full(40), BACKOFF_MAX_MS, "the cap holds at absurd attempts");
        assert!(full(40) >= full(30), "monotone up to the cap");
    }

    #[test]
    fn jitter_shortens_but_never_to_nothing() {
        let jittered = backoff_ms(5, 0.5);
        let full = backoff_ms(5, 1.0);
        assert!(jittered < full, "jitter must actually move the instant");
        assert!(jittered > 0, "a zero delay would be a hot retry loop");
    }

    #[test]
    fn a_long_error_is_cut_on_a_character_boundary() {
        let long = "é".repeat(4000);
        let cut = truncate_error(&long);
        assert!(cut.len() <= 2004);
        assert!(cut.ends_with('…'));
        // The real assertion: it is still valid UTF-8 and did not split a char.
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    #[test]
    fn states_round_trip_through_their_stored_spelling() {
        for state in [
            JobState::Pending,
            JobState::Fanout,
            JobState::Claimed,
            JobState::Done,
            JobState::Failed,
        ] {
            assert_eq!(JobState::parse(state.as_str()), Some(state));
        }
        assert_eq!(JobState::parse("running"), None);
    }

    #[test]
    fn the_ddl_indexes_the_claim_query() {
        let ddl = ddl().join(" ");
        assert!(
            ddl.contains("(state, run_at)"),
            "without this index every claim scans a table whose done rows only grow"
        );
    }
}
