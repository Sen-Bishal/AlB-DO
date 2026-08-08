//! AUTH · P0 — the operations, over any [`DataSubstrate`].
//!
//! Everything the request path needs from the four tables: turn a cookie into a
//! [`Principal`], turn a provider's `(subject, claims)` into one, rotate a
//! session, revoke one or all of them.
//!
//! ## Why this is a free function over a trait object, not a struct with state
//!
//! There is nothing to cache. A session lookup is a unique-index hit, and the
//! one thing that must *not* happen is a principal outliving the request that
//! resolved it — `AUTH.md` invariant 2.2, the failure here that is a CVE rather
//! than a bug. A resolver holding a map from token to principal is exactly the
//! shape that gets that wrong under a dev reload or a session revocation, so
//! there is no such map. The database is the cache.
//!
//! ## The two writes that must be transactional, and why
//!
//! [`upsert_principal`] and [`rotate_session`] both read-then-write, and both
//! have a losing side that corrupts ownership rather than merely failing:
//!
//! - A first login racing itself would mint two principals for one human, and the loser's rows
//!   become unreachable — the id that owns them is not the id their session carries. The
//!   `(provider, subject)` unique index makes the second insert *fail*, and this module turns that
//!   failure into "read the winner's row" rather than propagating it.
//! - A rotation that inserted the new session before deleting the old one, without a transaction,
//!   leaves two live sessions for one login if the process dies between them.

use crate::auth::principal::{Principal, PrincipalId};
use crate::auth::schema::{ACCOUNTS, CREDENTIALS, SESSIONS, USERS};
use crate::auth::session::{SessionRecord, SessionRejection, SessionToken, TokenHash};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Row, SqlValue, SubstrateError};
use serde_json::Map as JsonMap;
use serde_json::Value as JsonValue;

/// A store failure that is ours, not the caller's.
#[derive(Debug)]
pub enum StoreError {
    /// The substrate refused or failed.
    Substrate(SubstrateError),
    /// A row came back in a shape the schema says is impossible — a principal
    /// outside the partition-key alphabet, a missing column. Surfaced rather
    /// than unwrapped because the realistic cause is a database edited by hand.
    Corrupt(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Substrate(err) => write!(f, "auth store: {err}"),
            Self::Corrupt(what) => write!(f, "auth store: unusable row — {what}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<SubstrateError> for StoreError {
    fn from(err: SubstrateError) -> Self {
        Self::Substrate(err)
    }
}

type Result<T> = std::result::Result<T, StoreError>;

/// Outcome of presenting a session cookie.
#[derive(Debug)]
pub enum Resolved {
    /// A live session belonging to a principal that still exists.
    Principal(Box<Principal>),
    /// Nobody. The variant says why, for our logs — never for the client.
    Anonymous(SessionRejection),
}

impl Resolved {
    /// The principal, if there is one.
    #[must_use]
    pub fn principal(&self) -> Option<&Principal> {
        match self {
            Self::Principal(principal) => Some(principal),
            Self::Anonymous(_) => None,
        }
    }
}

/// Turn a presented token into a principal.
///
/// **The one function the request path calls**, and the whole of
/// `AUTH.md` invariant 2.2's enforcement: it takes the token, touches the
/// database, and returns an owned value scoped to the caller. Nothing is
/// memoised, so nothing can be served to the wrong principal on the next
/// request.
///
/// One query, not two: the session and its user row are joined, because a
/// separate user lookup would be a second round trip on every authenticated
/// request to answer a question the first one could have.
///
/// # Errors
/// [`StoreError`] when the substrate fails. A token that simply does not
/// resolve is **not** an error — it is [`Resolved::Anonymous`], because "no
/// valid session" is an ordinary outcome and treating it as a failure pushes
/// callers toward `unwrap_or_default` on a security decision.
pub async fn resolve_session(
    db: &dyn DataSubstrate,
    token: &SessionToken,
    now_ms: i64,
) -> Result<Resolved> {
    let hash = token.hash();
    let rows = db
        .query(
            &format!(
                "SELECT s.principal, s.provider, s.expires_at, u.email, u.name, u.image \
                 FROM {SESSIONS} s JOIN {USERS} u ON u.principal = s.principal \
                 WHERE s.token_hash = ?1"
            ),
            &[SqlValue::Text(hash.as_str().to_string())],
        )
        .await?;

    let Some(row) = rows.rows.first() else {
        // Either the token is unknown, or its principal has no user row. The
        // join cannot tell those apart, and the caller must not be told either
        // way — but our own logs can be more precise, cheaply, because this
        // branch is not the hot path.
        let orphaned = db
            .query(
                &format!("SELECT 1 FROM {SESSIONS} WHERE token_hash = ?1"),
                &[SqlValue::Text(hash.as_str().to_string())],
            )
            .await?;
        return Ok(Resolved::Anonymous(if orphaned.rows.is_empty() {
            SessionRejection::UnknownToken
        } else {
            SessionRejection::PrincipalGone
        }));
    };

    let expires_at = int_at(row, 2, "sessions.expires_at")?;
    if expires_at <= now_ms {
        return Ok(Resolved::Anonymous(SessionRejection::Expired));
    }

    let id = PrincipalId::parse(text_at(row, 0, "sessions.principal")?)
        .map_err(|err| StoreError::Corrupt(err.to_string()))?;

    Ok(Resolved::Principal(Box::new(Principal {
        id,
        email: optional_text_at(row, 3),
        name: optional_text_at(row, 4),
        image: optional_text_at(row, 5),
        provider: text_at(row, 1, "sessions.provider")?,
        // Claims are per-login, not per-session-read: re-projecting them on
        // every request would mean either storing the provider's whole payload
        // on the session row or calling the provider again. Neither is worth it
        // for a field nothing in the authorization path reads.
        claims: JsonMap::new(),
    })))
}

/// Find or create the principal behind a provider's `(subject, claims)`.
///
/// **Where the id becomes ours.** The provider's subject is stored on
/// `albedo_accounts`; the principal is minted here, or read back if this human
/// has been seen before. See [`crate::auth::principal`] for why the subject can
/// never be the id.
///
/// Idempotent under a race by construction rather than by locking: the
/// `(provider, subject)` unique index means the losing insert fails, and the
/// loser then reads the winner's row. That is why the constraint is in the
/// schema and not in a comment.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn upsert_principal(
    db: &dyn DataSubstrate,
    provider: &str,
    subject: &str,
    profile: &ProviderProfile,
    now_ms: i64,
) -> Result<Principal> {
    if let Some(existing) = lookup_account(db, provider, subject).await? {
        // A returning human: refresh the profile, since a display name or
        // avatar changing at the provider should not require a new account.
        db.execute(
            &format!(
                "UPDATE {USERS} SET email = COALESCE(?2, email), name = COALESCE(?3, name), \
                 image = COALESCE(?4, image), updated_at = ?5 WHERE principal = ?1"
            ),
            &[
                SqlValue::Text(existing.as_str().to_string()),
                optional_param(profile.email.as_deref()),
                optional_param(profile.name.as_deref()),
                optional_param(profile.image.as_deref()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await?;
        return Ok(build_principal(existing, provider, profile));
    }

    let minted = PrincipalId::mint();
    let tx = db.begin().await?;
    tx.execute(
        &format!(
            "INSERT INTO {USERS} (principal, email, name, image, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)"
        ),
        &[
            SqlValue::Text(minted.as_str().to_string()),
            optional_param(profile.email.as_deref()),
            optional_param(profile.name.as_deref()),
            optional_param(profile.image.as_deref()),
            SqlValue::Integer(now_ms),
        ],
    )
    .await?;
    let account = tx
        .execute(
            &format!(
                "INSERT INTO {ACCOUNTS} (principal, provider, subject, created_at) \
                 VALUES (?1, ?2, ?3, ?4)"
            ),
            &[
                SqlValue::Text(minted.as_str().to_string()),
                SqlValue::Text(provider.to_string()),
                SqlValue::Text(subject.to_string()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await;

    match account {
        Ok(_) => {
            tx.commit().await?;
            Ok(build_principal(minted, provider, profile))
        }
        Err(_) => {
            // We lost the race. Roll back our half-built human and adopt the
            // winner's — the alternative is two principals for one person, with
            // the loser owning rows nobody can reach.
            tx.rollback().await?;
            let winner = lookup_account(db, provider, subject).await?.ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "insert into {ACCOUNTS} for ({provider}, <subject>) failed but no row exists"
                ))
            })?;
            Ok(build_principal(winner, provider, profile))
        }
    }
}

/// Open a session for a principal. Returns the token to put in the cookie.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn create_session(
    db: &dyn DataSubstrate,
    principal: &PrincipalId,
    provider: &str,
    now_ms: i64,
    ttl_ms: i64,
) -> Result<SessionToken> {
    let token = SessionToken::mint();
    db.execute(
        &format!(
            "INSERT INTO {SESSIONS} (principal, token_hash, provider, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5)"
        ),
        &[
            SqlValue::Text(principal.as_str().to_string()),
            SqlValue::Text(token.hash().as_str().to_string()),
            SqlValue::Text(provider.to_string()),
            SqlValue::Integer(now_ms),
            SqlValue::Integer(now_ms.saturating_add(ttl_ms)),
        ],
    )
    .await?;
    Ok(token)
}

/// Replace a session with a fresh one for the same principal — **`AUTH.md` R4,
/// session fixation.**
///
/// The rule this implements: *a session id must never survive a change in what
/// it authorizes.* An attacker who can plant a cookie value before login (via a
/// subdomain, a shared machine, a link) holds a token that becomes the victim's
/// session the moment they authenticate — unless the id changes at that moment.
/// So this is called on login, and on any later privilege change.
///
/// Both statements run in one transaction: a crash between them would otherwise
/// leave the old session live alongside the new one, which is the exact
/// condition rotation exists to prevent.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn rotate_session(
    db: &dyn DataSubstrate,
    old: &TokenHash,
    principal: &PrincipalId,
    provider: &str,
    now_ms: i64,
    ttl_ms: i64,
) -> Result<SessionToken> {
    let token = SessionToken::mint();
    let tx = db.begin().await?;
    tx.execute(
        &format!("DELETE FROM {SESSIONS} WHERE token_hash = ?1"),
        &[SqlValue::Text(old.as_str().to_string())],
    )
    .await?;
    tx.execute(
        &format!(
            "INSERT INTO {SESSIONS} \
             (principal, token_hash, provider, created_at, expires_at, rotated_from) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        ),
        &[
            SqlValue::Text(principal.as_str().to_string()),
            SqlValue::Text(token.hash().as_str().to_string()),
            SqlValue::Text(provider.to_string()),
            SqlValue::Integer(now_ms),
            SqlValue::Integer(now_ms.saturating_add(ttl_ms)),
            SqlValue::Text(old.as_str().to_string()),
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// End one session — log out this device.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn revoke_session(db: &dyn DataSubstrate, token: &TokenHash) -> Result<u64> {
    Ok(db
        .execute(
            &format!("DELETE FROM {SESSIONS} WHERE token_hash = ?1"),
            &[SqlValue::Text(token.as_str().to_string())],
        )
        .await?)
}

/// End every session for a principal — log out everywhere.
///
/// 🔑 **This is `AUTH.md` § 5, and it is one `DELETE`.** The rows are a
/// partition of `albedo_sessions` keyed by principal, so removing them is a
/// delta on a topic the delta kernel already fans out. Every other tab drops
/// its live lane in the same frame as any other write — no polling, no TTL, no
/// revocation list. The instant-global-logout property is the engine that
/// already shipped, pointed at a new table.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn revoke_all_sessions(db: &dyn DataSubstrate, principal: &PrincipalId) -> Result<u64> {
    Ok(db
        .execute(
            &format!("DELETE FROM {SESSIONS} WHERE principal = ?1"),
            &[SqlValue::Text(principal.as_str().to_string())],
        )
        .await?)
}

/// Drop sessions whose expiry has passed.
///
/// Housekeeping, not enforcement: [`resolve_session`] already refuses an expired
/// row, so a session missed here is unusable rather than dangerous. It exists so
/// the table does not grow without bound.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn purge_expired_sessions(db: &dyn DataSubstrate, now_ms: i64) -> Result<u64> {
    Ok(db
        .execute(
            &format!("DELETE FROM {SESSIONS} WHERE expires_at <= ?1"),
            &[SqlValue::Integer(now_ms)],
        )
        .await?)
}

/// Every live session for a principal — the device list, and what a revocation
/// screen renders from.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn sessions_for(
    db: &dyn DataSubstrate,
    principal: &PrincipalId,
    now_ms: i64,
) -> Result<Vec<SessionRecord>> {
    let rows = db
        .query(
            &format!(
                "SELECT id, principal, provider, created_at, expires_at, rotated_from \
                 FROM {SESSIONS} WHERE principal = ?1 AND expires_at > ?2 ORDER BY created_at"
            ),
            &[
                SqlValue::Text(principal.as_str().to_string()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await?;

    rows.rows
        .iter()
        .map(|row| {
            Ok(SessionRecord {
                id: int_at(row, 0, "sessions.id")?,
                principal: PrincipalId::parse(text_at(row, 1, "sessions.principal")?)
                    .map_err(|err| StoreError::Corrupt(err.to_string()))?,
                provider: text_at(row, 2, "sessions.provider")?,
                created_at: int_at(row, 3, "sessions.created_at")?,
                expires_at: int_at(row, 4, "sessions.expires_at")?,
                rotated_from: optional_text_at(row, 5),
            })
        })
        .collect()
}

/// Delete a principal and everything hanging off them.
///
/// Ordered so that sessions die first: a deletion interrupted after the user row
/// but before the sessions would leave live tokens pointing at a principal that
/// no longer exists, which [`resolve_session`] would report as
/// [`SessionRejection::PrincipalGone`] — recoverable, but the wrong order is the
/// one that leaves a window where the account is gone and the session is not.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn delete_principal(db: &dyn DataSubstrate, principal: &PrincipalId) -> Result<()> {
    let tx = db.begin().await?;
    for table in [SESSIONS, CREDENTIALS, ACCOUNTS, USERS] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE principal = ?1"),
            &[SqlValue::Text(principal.as_str().to_string())],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// What a provider told us about a human, already projected out of its own
/// claim shape by the provider's `claimMap`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderProfile {
    /// Best-known email. Not an identifier.
    pub email: Option<String>,
    /// Display name.
    pub name: Option<String>,
    /// Avatar URL.
    pub image: Option<String>,
    /// The provider's raw payload, carried onto the principal.
    pub claims: JsonMap<String, JsonValue>,
}

async fn lookup_account(
    db: &dyn DataSubstrate,
    provider: &str,
    subject: &str,
) -> Result<Option<PrincipalId>> {
    let rows = db
        .query(
            &format!("SELECT principal FROM {ACCOUNTS} WHERE provider = ?1 AND subject = ?2"),
            &[
                SqlValue::Text(provider.to_string()),
                SqlValue::Text(subject.to_string()),
            ],
        )
        .await?;
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let id = PrincipalId::parse(text_at(row, 0, "accounts.principal")?)
        .map_err(|err| StoreError::Corrupt(err.to_string()))?;
    Ok(Some(id))
}

fn build_principal(id: PrincipalId, provider: &str, profile: &ProviderProfile) -> Principal {
    Principal {
        id,
        email: profile.email.clone(),
        name: profile.name.clone(),
        image: profile.image.clone(),
        provider: provider.to_string(),
        claims: profile.claims.clone(),
    }
}

fn optional_param(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, |value| SqlValue::Text(value.to_string()))
}

fn text_at(row: &Row, idx: usize, what: &str) -> Result<String> {
    row.get(idx)
        .and_then(SqlValue::as_str)
        .map(str::to_string)
        .ok_or_else(|| StoreError::Corrupt(format!("{what} is not text")))
}

fn optional_text_at(row: &Row, idx: usize) -> Option<String> {
    row.get(idx).and_then(SqlValue::as_str).map(str::to_string)
}

fn int_at(row: &Row, idx: usize, what: &str) -> Result<i64> {
    match row.get(idx) {
        Some(SqlValue::Integer(value)) => Ok(*value),
        _ => Err(StoreError::Corrupt(format!("{what} is not an integer"))),
    }
}
