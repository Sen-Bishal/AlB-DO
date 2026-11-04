//! Durable admission and fencing for compiled action attempts.

use crate::forge::substrate::{DataSubstrate, Transaction};
use crate::forge::value::{Result, SqlValue, SubstrateError};

/// Longer than the server's attempt deadline. A dead process needs no cleanup:
/// a retry can replace its owner after this lease expires.
pub const ACTION_LEASE_MS: i64 = 60_000;

pub(crate) const DDL: &str = "CREATE TABLE IF NOT EXISTS albedo_action_admissions (\
    workflow TEXT PRIMARY KEY, build_id TEXT NOT NULL, request_digest TEXT NOT NULL, \
    owner TEXT NOT NULL, lease_until INTEGER NOT NULL, completed_at INTEGER)";

/// The durable decision made before any action body executes.
#[derive(Debug)]
pub enum Admission {
    /// This attempt owns the intent until its lease expires.
    Acquired(ActionClaim),
    /// The intent already committed; return its recorded wire instructions.
    Completed(Vec<u8>),
    /// Another attempt still owns this intent. The caller may retry later.
    Busy,
}

/// An unguessable attempt token. Every durable mutation checks it under the
/// same writer transaction, so an expired attempt cannot complete after takeover.
#[derive(Debug)]
pub struct ActionClaim {
    workflow: String,
    build_id: String,
    owner: String,
}

fn refused(reason: &str) -> SubstrateError {
    SubstrateError::Backend(format!("action admission: {reason}"))
}

impl ActionClaim {
    /// Acquire using the wall clock sampled after obtaining the writer lock.
    ///
    /// # Errors
    /// Fails closed on storage errors or a reused intent with different inputs.
    pub async fn acquire_now(
        db: &dyn DataSubstrate,
        workflow: &str,
        build_id: &str,
        request_digest: &str,
    ) -> Result<Admission> {
        Self::acquire_clock(
            db,
            workflow,
            build_id,
            request_digest,
            crate::aperture::now_ms,
        )
        .await
    }

    /// Atomically acquire, reject, or replay an intent. Storage failures and
    /// request/build mismatches fail closed rather than executing again.
    ///
    /// # Errors
    /// Returns a storage or admission error without running the action.
    pub async fn acquire(
        db: &dyn DataSubstrate,
        workflow: &str,
        build_id: &str,
        request_digest: &str,
        now_ms: i64,
    ) -> Result<Admission> {
        Self::acquire_clock(db, workflow, build_id, request_digest, move || now_ms).await
    }

    async fn acquire_clock(
        db: &dyn DataSubstrate,
        workflow: &str,
        build_id: &str,
        request_digest: &str,
        clock: impl FnOnce() -> i64 + Send,
    ) -> Result<Admission> {
        let tx = db.begin().await?;
        let now_ms = clock();
        let rows = tx
            .query(
                "SELECT build_id, request_digest, owner, lease_until, completed_at \
             FROM albedo_action_admissions WHERE workflow = ?",
                &[workflow.into()],
            )
            .await?;
        if let Some(row) = rows.rows.first() {
            if row.get(0).and_then(SqlValue::as_str) != Some(build_id)
                || row.get(1).and_then(SqlValue::as_str) != Some(request_digest)
            {
                return Err(refused(
                    "intent was already used with a different request or build",
                ));
            }
            if !matches!(row.get(4), Some(SqlValue::Null)) {
                let result = Self::result(tx.as_ref(), workflow, build_id)
                    .await?
                    .ok_or_else(|| refused("completed intent has no readable result"))?;
                tx.rollback().await?;
                return Ok(Admission::Completed(result));
            }
            if row.get(3).and_then(SqlValue::as_i64).unwrap_or(i64::MAX) > now_ms {
                tx.rollback().await?;
                return Ok(Admission::Busy);
            }
        } else {
            // Preserve completion protection for results written before admission
            // existed. Never run an old completed intent under a different build.
            let legacy = tx
                .query(
                    "SELECT build_id, instructions FROM albedo_workflow_results WHERE workflow = ?",
                    &[workflow.into()],
                )
                .await?;
            if let Some(row) = legacy.rows.first() {
                if row.get(0).and_then(SqlValue::as_str) != Some(build_id) {
                    return Err(refused("intent completed under a different build"));
                }
                let Some(SqlValue::Blob(result)) = row.get(1) else {
                    return Err(refused("unreadable legacy completion result"));
                };
                let result = result.clone();
                tx.rollback().await?;
                return Ok(Admission::Completed(result));
            }
        }
        let owner = uuid::Uuid::new_v4().simple().to_string();
        tx.execute(
            "INSERT INTO albedo_action_admissions \
             (workflow, build_id, request_digest, owner, lease_until, completed_at) \
             VALUES (?, ?, ?, ?, ?, NULL) ON CONFLICT(workflow) DO UPDATE SET \
             owner = excluded.owner, lease_until = excluded.lease_until",
            &[
                workflow.into(),
                build_id.into(),
                request_digest.into(),
                owner.clone().into(),
                now_ms.saturating_add(ACTION_LEASE_MS).into(),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(Admission::Acquired(Self {
            workflow: workflow.to_string(),
            build_id: build_id.to_string(),
            owner,
        }))
    }

    async fn result(
        tx: &dyn Transaction,
        workflow: &str,
        build_id: &str,
    ) -> Result<Option<Vec<u8>>> {
        let rows = tx.query(
            "SELECT instructions FROM albedo_workflow_results WHERE workflow = ? AND build_id = ?",
            &[workflow.into(), build_id.into()],
        ).await?;
        Ok(rows.rows.first().and_then(|row| match row.get(0) {
            Some(SqlValue::Blob(bytes)) => Some(bytes.clone()),
            _ => None,
        }))
    }

    /// Validate ownership inside the transaction that will mutate effects.
    ///
    /// # Errors
    /// Returns an error if storage fails or this attempt no longer owns its lease.
    pub async fn check(&self, tx: &dyn Transaction, now_ms: i64) -> Result<()> {
        let rows = tx
            .query(
                "SELECT 1 FROM albedo_action_admissions WHERE workflow = ? AND build_id = ? \
             AND owner = ? AND lease_until > ? AND completed_at IS NULL",
                &[
                    self.workflow.clone().into(),
                    self.build_id.clone().into(),
                    self.owner.clone().into(),
                    now_ms.into(),
                ],
            )
            .await?;
        if rows.rows.is_empty() {
            return Err(refused("attempt lost its lease; retry the intent"));
        }
        Ok(())
    }

    /// Record completion in the SAME transaction as collection writes and jobs.
    /// The caller commits; any error/cancellation must drop or roll back it all.
    ///
    /// # Errors
    /// Returns storage failures or loss of admission ownership.
    pub async fn complete(
        &self,
        tx: &dyn Transaction,
        instructions: &[u8],
        now_ms: i64,
    ) -> Result<()> {
        self.check(tx, now_ms).await?;
        tx.execute(
            "INSERT INTO albedo_workflow_results (workflow, build_id, instructions, completed_at) \
             VALUES (?, ?, ?, ?)",
            &[
                self.workflow.clone().into(),
                self.build_id.clone().into(),
                SqlValue::Blob(instructions.to_vec()),
                now_ms.into(),
            ],
        )
        .await?;
        tx.execute(
            "UPDATE albedo_action_admissions SET completed_at = ?, lease_until = 0 \
             WHERE workflow = ? AND owner = ?",
            &[
                now_ms.into(),
                self.workflow.clone().into(),
                self.owner.clone().into(),
            ],
        )
        .await?;
        Ok(())
    }
}
