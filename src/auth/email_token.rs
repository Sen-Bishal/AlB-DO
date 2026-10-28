//! AUTH × EMAIL — single-use tokens mailed to an address, and the record that
//! an address was proven.
//!
//! Three flows share one table: password reset, email verification, and magic
//! link sign-in. Each mails a link carrying a token; each redeems it once.
//!
//! ## Properties, and the mechanism that provides each
//!
//! - **Only the hash is stored.** The table holds `SHA-256(token)`, as
//!   `albedo_sessions` does — a database read, backup or `SELECT *` yields
//!   nothing that opens a reset form. The raw value exists in the email and, until
//!   the send completes, in that job's queue row, which the runner scrubs
//!   (`email::QueuedEmail::sensitive`).
//! - **Single use** is one transaction: the `SELECT` and the `DELETE` of the same
//!   primary key commit together, so two concurrent redemptions cannot both see
//!   the row. The second finds nothing, which is the same answer as a wrong token.
//! - **Newest wins.** Issuing a token deletes every outstanding token for the
//!   same `(purpose, provider, email)` first. Three "forgot password" clicks
//!   leave one working link, not three, so the one in the inbox that the person
//!   opens last is the only one that can matter.
//! - **A reset ends every session.** [`reset_password`] rewrites the hash, deletes
//!   the principal's sessions and every other reset token in one transaction.
//!   Somebody resetting a password is very often somebody who thinks another
//!   person has it; leaving that person signed in would make the reset
//!   decorative.
//!
//! ## Why these are plain tables and not FORGE collections
//!
//! A FORGE collection is a topic, and a topic is readable. Nothing here should
//! ever be subscribed to, and neither table needs to reach a browser. The
//! `albedo_` prefix still reserves both names, so an app cannot declare them.

use crate::auth::principal::PrincipalId;
use crate::auth::schema::{ACCOUNTS, CREDENTIALS, SESSIONS, USERS};
use crate::auth::session::SessionToken;
use crate::auth::store::{optional_text_at, text_at, StoreError};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::SqlValue;

/// Outstanding mailed tokens.
pub const EMAIL_TOKENS: &str = "albedo_email_tokens";
/// Addresses a principal has proven it receives mail at.
pub const EMAIL_VERIFIED: &str = "albedo_email_verified";

type Result<T> = std::result::Result<T, StoreError>;

/// What a mailed token is for. Part of the lookup, so a verification token can
/// never be redeemed as a reset — same table, disjoint keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Choose a new password.
    PasswordReset,
    /// Prove a password account's address.
    VerifyEmail,
    /// Sign in with no password.
    MagicLink,
}

impl Purpose {
    /// As stored.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PasswordReset => "reset",
            Self::VerifyEmail => "verify",
            Self::MagicLink => "magiclink",
        }
    }

    /// How long a link lasts, in minutes.
    ///
    /// A reset and a sign-in link are live credentials and last minutes; a
    /// verification link proves a mailbox and grants nothing on its own, and
    /// people confirm sign-ups the next morning.
    #[must_use]
    pub const fn ttl_minutes(self) -> u64 {
        match self {
            Self::PasswordReset => 30,
            Self::MagicLink => 15,
            Self::VerifyEmail => 24 * 60,
        }
    }

    /// [`Self::ttl_minutes`] in milliseconds.
    #[must_use]
    pub const fn ttl_ms(self) -> i64 {
        // Bounded by the constants above; cannot overflow.
        (self.ttl_minutes() * 60_000) as i64
    }
}

/// A redeemed token: whose it was, and where to send them next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    /// The provider it was issued under.
    pub provider: String,
    /// The principal, for reset and verify. `None` for a magic link to an
    /// address with no account yet — redeeming it creates one.
    pub principal: Option<PrincipalId>,
    /// The address it was mailed to, already normalised.
    pub email: String,
    /// The validated return path captured when it was issued.
    pub next: Option<String>,
}

/// The statements that create the tables. Idempotent.
#[must_use]
pub fn ddl() -> Vec<String> {
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {EMAIL_TOKENS} (\
             token_hash TEXT PRIMARY KEY, \
             purpose TEXT NOT NULL, \
             provider TEXT NOT NULL, \
             principal TEXT, \
             email TEXT NOT NULL, \
             next TEXT, \
             expires_at INTEGER NOT NULL, \
             created_at INTEGER NOT NULL)"
        ),
        // Supersession deletes by this on every issue.
        format!(
            "CREATE INDEX IF NOT EXISTS {EMAIL_TOKENS}_address \
             ON {EMAIL_TOKENS} (purpose, provider, email)"
        ),
        // The hourly sweep.
        format!("CREATE INDEX IF NOT EXISTS {EMAIL_TOKENS}_expiry ON {EMAIL_TOKENS} (expires_at)"),
        // Keyed by the pair, not by principal: verification is of an ADDRESS.
        // If a principal's email changes, the new one is unproven — a boolean on
        // the user row would silently carry the old proof over.
        format!(
            "CREATE TABLE IF NOT EXISTS {EMAIL_VERIFIED} (\
             principal TEXT NOT NULL, \
             email TEXT NOT NULL, \
             verified_at INTEGER NOT NULL, \
             PRIMARY KEY (principal, email))"
        ),
        // See `admit_mail`. The address is the key; `tat` is all the state GCRA
        // needs.
        format!(
            "CREATE TABLE IF NOT EXISTS {MAIL_BUDGET} (\
             address TEXT PRIMARY KEY, \
             tat INTEGER NOT NULL)"
        ),
    ]
}

/// Mint a token for `email` and store its hash, superseding any outstanding
/// token for the same purpose, provider and address.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn issue(
    db: &dyn DataSubstrate,
    purpose: Purpose,
    provider: &str,
    principal: Option<&PrincipalId>,
    email: &str,
    next: Option<&str>,
    now_ms: i64,
) -> Result<SessionToken> {
    let token = SessionToken::mint();
    let tx = db.begin().await?;
    tx.execute(
        &format!(
            "DELETE FROM {EMAIL_TOKENS} WHERE purpose = ?1 AND provider = ?2 AND email = ?3"
        ),
        &[
            SqlValue::Text(purpose.as_str().to_string()),
            SqlValue::Text(provider.to_string()),
            SqlValue::Text(email.to_string()),
        ],
    )
    .await?;
    tx.execute(
        &format!(
            "INSERT INTO {EMAIL_TOKENS} \
             (token_hash, purpose, provider, principal, email, next, expires_at, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        ),
        &[
            SqlValue::Text(token.hash().as_str().to_string()),
            SqlValue::Text(purpose.as_str().to_string()),
            SqlValue::Text(provider.to_string()),
            principal.map_or(SqlValue::Null, |p| SqlValue::Text(p.as_str().to_string())),
            SqlValue::Text(email.to_string()),
            next.map_or(SqlValue::Null, |n| SqlValue::Text(n.to_string())),
            SqlValue::Integer(now_ms.saturating_add(purpose.ttl_ms())),
            SqlValue::Integer(now_ms),
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(token)
}

/// Whether `token` is live for `purpose`, without spending it.
///
/// For the `GET` that renders a form: a link a mail scanner prefetches must not
/// be consumed by the prefetch, but the person should still learn *before*
/// typing a new password that the link has expired.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn peek(
    db: &dyn DataSubstrate,
    purpose: Purpose,
    token: &SessionToken,
    now_ms: i64,
) -> Result<bool> {
    let rows = db
        .query(
            &format!(
                "SELECT 1 FROM {EMAIL_TOKENS} \
                 WHERE token_hash = ?1 AND purpose = ?2 AND expires_at > ?3"
            ),
            &[
                SqlValue::Text(token.hash().as_str().to_string()),
                SqlValue::Text(purpose.as_str().to_string()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await?;
    Ok(!rows.rows.is_empty())
}

/// Redeem `token` for `purpose`. `Ok(None)` for unknown, expired, already used,
/// or issued for a different purpose — one answer for all four.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn consume(
    db: &dyn DataSubstrate,
    purpose: Purpose,
    token: &SessionToken,
    now_ms: i64,
) -> Result<Option<Redeemed>> {
    let hash = token.hash();
    let tx = db.begin().await?;
    let rows = tx
        .query(
            &format!(
                "SELECT provider, principal, email, next FROM {EMAIL_TOKENS} \
                 WHERE token_hash = ?1 AND purpose = ?2 AND expires_at > ?3"
            ),
            &[
                SqlValue::Text(hash.as_str().to_string()),
                SqlValue::Text(purpose.as_str().to_string()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await?;
    let Some(row) = rows.rows.first() else {
        tx.rollback().await?;
        return Ok(None);
    };
    let principal = optional_text_at(row, 1)
        .map(|raw| PrincipalId::parse(&raw).map_err(|err| StoreError::Corrupt(err.to_string())))
        .transpose()?;
    let redeemed = Redeemed {
        provider: text_at(row, 0, "email_tokens.provider")?,
        principal,
        email: text_at(row, 2, "email_tokens.email")?,
        next: optional_text_at(row, 3),
    };
    let deleted = tx
        .execute(
            &format!("DELETE FROM {EMAIL_TOKENS} WHERE token_hash = ?1"),
            &[SqlValue::Text(hash.as_str().to_string())],
        )
        .await?;
    if deleted != 1 {
        // Somebody else spent it between our read and our delete. Unreachable
        // under a single-writer substrate, and correct under any other.
        tx.rollback().await?;
        return Ok(None);
    }
    tx.commit().await?;
    Ok(Some(redeemed))
}

/// Set a password, end every session, and void every other reset token for the
/// principal — one transaction.
///
/// Two shapes, one call:
///
/// - **A reset.** The principal has a password credential under `provider`; its hash is replaced.
/// - **A first password.** The principal has none — it signed up by magic link — and proved
///   `email` by clicking this link. The `(provider, email)` account and the credential are
///   created, so the same person can now sign in either way and lands on one principal.
///
/// Returns `false` when neither applies: the principal no longer exists, or
/// `(provider, email)` already belongs to somebody else.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn reset_password(
    db: &dyn DataSubstrate,
    principal: &PrincipalId,
    provider: &str,
    email: &str,
    secret_hash: &str,
    now_ms: i64,
) -> Result<bool> {
    let who = SqlValue::Text(principal.as_str().to_string());
    let tx = db.begin().await?;
    let updated = tx
        .execute(
            &format!(
                "UPDATE {CREDENTIALS} SET secret_hash = ?1 \
                 WHERE principal = ?2 AND provider = ?3 AND secret_hash IS NOT NULL"
            ),
            &[
                SqlValue::Text(secret_hash.to_string()),
                who.clone(),
                SqlValue::Text(provider.to_string()),
            ],
        )
        .await?;
    if updated == 0 {
        let exists = !tx
            .query(
                &format!("SELECT 1 FROM {USERS} WHERE principal = ?1"),
                std::slice::from_ref(&who),
            )
            .await?
            .rows
            .is_empty();
        // The account row is what makes `email` sign in as this principal; if
        // it already names someone else, adding a credential here would hand
        // this principal a password for another person's login.
        let mut claimed = false;
        if exists {
            tx.execute(
                &format!(
                    "INSERT OR IGNORE INTO {ACCOUNTS} (principal, provider, subject, created_at) \
                     VALUES (?1, ?2, ?3, ?4)"
                ),
                &[
                    who.clone(),
                    SqlValue::Text(provider.to_string()),
                    SqlValue::Text(email.to_string()),
                    SqlValue::Integer(now_ms),
                ],
            )
            .await?;
            // Whoever holds the row now — ours just inserted, ours left behind
            // with no credential, or somebody else's. Only the first two may
            // gain a password here.
            let owner = tx
                .query(
                    &format!("SELECT principal FROM {ACCOUNTS} WHERE provider = ?1 AND subject = ?2"),
                    &[
                        SqlValue::Text(provider.to_string()),
                        SqlValue::Text(email.to_string()),
                    ],
                )
                .await?;
            claimed = owner
                .rows
                .first()
                .and_then(|row| row.get(0))
                .and_then(SqlValue::as_str)
                == Some(principal.as_str());
        }
        if !claimed {
            tx.rollback().await?;
            return Ok(false);
        }
        tx.execute(
            &format!(
                "INSERT INTO {CREDENTIALS} (principal, provider, secret_hash, created_at) \
                 VALUES (?1, ?2, ?3, ?4)"
            ),
            &[
                who.clone(),
                SqlValue::Text(provider.to_string()),
                SqlValue::Text(secret_hash.to_string()),
                SqlValue::Integer(now_ms),
            ],
        )
        .await?;
    }
    tx.execute(
        &format!("DELETE FROM {SESSIONS} WHERE principal = ?1"),
        &[SqlValue::Text(principal.as_str().to_string())],
    )
    .await?;
    tx.execute(
        &format!("DELETE FROM {EMAIL_TOKENS} WHERE principal = ?1 AND purpose = ?2"),
        &[
            SqlValue::Text(principal.as_str().to_string()),
            SqlValue::Text(Purpose::PasswordReset.as_str().to_string()),
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// The principal that has proven `email`, if any — the earliest, if several.
///
/// **The linking key.** Two providers reporting one address are not one human
/// (`schema::users` says why the email column is not unique); two providers
/// whose owner *proved* the address by clicking a link sent to it are.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn verified_principal(db: &dyn DataSubstrate, email: &str) -> Result<Option<PrincipalId>> {
    let rows = db
        .query(
            &format!(
                "SELECT principal FROM {EMAIL_VERIFIED} WHERE email = ?1 \
                 ORDER BY verified_at, principal LIMIT 1"
            ),
            &[SqlValue::Text(email.to_string())],
        )
        .await?;
    rows.rows
        .first()
        .map(|row| {
            PrincipalId::parse(&text_at(row, 0, "email_verified.principal")?)
                .map_err(|err| StoreError::Corrupt(err.to_string()))
        })
        .transpose()
}

/// Who a redeemed magic link signs in as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagicLinked {
    /// The principal.
    pub principal: PrincipalId,
    /// An unverified password account for this address was taken over — its
    /// password deleted and its sessions ended. See [`resolve_magic_link`].
    pub took_over: bool,
}

/// Resolve the principal a magic link for `email` signs in as, creating or
/// linking the magic-link account as needed. In order:
///
/// 1. **This address already signs in by magic link** → that principal.
/// 2. **A principal has proven this address** (a verified password account) → link to it: one
///    human, two ways in, one principal.
/// 3. **An *unverified* password account holds this address** → link to it, and **delete its
///    password and end its sessions.** This is the *pre-account-takeover* defence (Sudhodanan &
///    Paverd, 2022): an attacker registers the victim's address with a password they know and
///    waits; the victim later signs in by magic link and, if the two were merely linked, the
///    attacker's password still opens the victim's account. Whoever clicked this link owns the
///    mailbox; whoever set that password never proved they did. The owner loses nothing they can
///    prove was theirs, and can set a password through "forgot password". Every other unproven
///    way in goes too — credentials, account rows, sessions, links.
///
///    **Why the claimant inherits nothing the squatter made**: this branch is reachable only
///    where the password provider declares `verify: "required"` —
///    [`AuthDeclaration::lower`](crate::auth::declare::AuthDeclaration::lower) refuses
///    `magiclink` beside a password provider without it — and under `verify` an unproven account
///    can never sign in. It has no sessions, so it has no rows: the principal claimed here is
///    an empty shell. (The one exception is an app that switched `verify` on after users had
///    already signed in unverified; their data is theirs, and the claimant is usually them.)
/// 4. **Nobody** → a new principal.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn resolve_magic_link(
    db: &dyn DataSubstrate,
    magic_provider: &str,
    password_provider: Option<&str>,
    email: &str,
    now_ms: i64,
) -> Result<MagicLinked> {
    use crate::auth::store::{lookup_account, upsert_principal, ProviderProfile};

    if let Some(principal) = lookup_account(db, magic_provider, email).await? {
        return Ok(MagicLinked { principal, took_over: false });
    }

    let (target, took_over) = match verified_principal(db, email).await? {
        Some(principal) => (Some(principal), false),
        None => match password_provider {
            Some(provider) => (lookup_account(db, provider, email).await?, true),
            None => (None, false),
        },
    };

    let Some(target) = target else {
        let profile = ProviderProfile {
            email: Some(email.to_string()),
            ..ProviderProfile::default()
        };
        let principal = upsert_principal(db, magic_provider, email, &profile, now_ms).await?;
        return Ok(MagicLinked { principal: principal.id, took_over: false });
    };

    let who = SqlValue::Text(target.as_str().to_string());
    let tx = db.begin().await?;
    tx.execute(
        &format!(
            "INSERT OR IGNORE INTO {ACCOUNTS} (principal, provider, subject, created_at) \
             VALUES (?1, ?2, ?3, ?4)"
        ),
        &[
            who.clone(),
            SqlValue::Text(magic_provider.to_string()),
            SqlValue::Text(email.to_string()),
            SqlValue::Integer(now_ms),
        ],
    )
    .await?;
    if took_over {
        // Every way in that nobody proved goes: each credential (password,
        // passkey), each other account row (a "trojan identifier" a squatter
        // could have attached so the account still opens for them afterwards),
        // each session, each outstanding link. What remains is exactly the
        // address the claimant just proved.
        tx.execute(
            &format!("DELETE FROM {CREDENTIALS} WHERE principal = ?1"),
            std::slice::from_ref(&who),
        )
        .await?;
        tx.execute(
            &format!("DELETE FROM {ACCOUNTS} WHERE principal = ?1 AND provider <> ?2"),
            &[who.clone(), SqlValue::Text(magic_provider.to_string())],
        )
        .await?;
        tx.execute(
            &format!("DELETE FROM {SESSIONS} WHERE principal = ?1"),
            std::slice::from_ref(&who),
        )
        .await?;
        tx.execute(
            &format!("DELETE FROM {EMAIL_TOKENS} WHERE principal = ?1"),
            std::slice::from_ref(&who),
        )
        .await?;
    }
    tx.commit().await?;

    // Re-read rather than trust the insert: under a race the other request's
    // row is the one that stands, and it is the one to sign in as.
    let principal = lookup_account(db, magic_provider, email).await?.ok_or_else(|| {
        StoreError::Corrupt("a magic-link account was linked but cannot be read back".into())
    })?;
    Ok(MagicLinked {
        took_over: took_over && principal == target,
        principal,
    })
}

/// Record that `principal` receives mail at `email`. Idempotent.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn mark_verified(
    db: &dyn DataSubstrate,
    principal: &PrincipalId,
    email: &str,
    now_ms: i64,
) -> Result<()> {
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO {EMAIL_VERIFIED} (principal, email, verified_at) \
             VALUES (?1, ?2, ?3)"
        ),
        &[
            SqlValue::Text(principal.as_str().to_string()),
            SqlValue::Text(email.to_string()),
            SqlValue::Integer(now_ms),
        ],
    )
    .await?;
    Ok(())
}

/// Whether `principal` has proven `email`.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn is_verified(db: &dyn DataSubstrate, principal: &PrincipalId, email: &str) -> Result<bool> {
    let rows = db
        .query(
            &format!("SELECT 1 FROM {EMAIL_VERIFIED} WHERE principal = ?1 AND email = ?2"),
            &[
                SqlValue::Text(principal.as_str().to_string()),
                SqlValue::Text(email.to_string()),
            ],
        )
        .await?;
    Ok(!rows.rows.is_empty())
}

/// Delete expired tokens and fully refilled mail budgets. Housekeeping — an
/// expired token is already refused, and a budget whose `tat` has passed is
/// indistinguishable from no row.
///
/// # Errors
/// [`StoreError`] on substrate failure.
pub async fn purge_expired(db: &dyn DataSubstrate, now_ms: i64) -> Result<u64> {
    let tokens = db
        .execute(
            &format!("DELETE FROM {EMAIL_TOKENS} WHERE expires_at <= ?1"),
            &[SqlValue::Integer(now_ms)],
        )
        .await?;
    let budgets = db
        .execute(
            &format!("DELETE FROM {MAIL_BUDGET} WHERE tat <= ?1"),
            &[SqlValue::Integer(now_ms)],
        )
        .await?;
    Ok(tokens + budgets)
}

// ── the per-mailbox budget ───────────────────────────────────────────────

/// One row per address that has been mailed a link recently.
pub const MAIL_BUDGET: &str = "albedo_mail_budget";

/// Links one address may receive at once.
pub const MAIL_BURST: i64 = 3;

/// Milliseconds for one link of budget to come back — ten an hour.
pub const MAIL_INTERVAL_MS: i64 = 6 * 60 * 1000;

/// Charge one emailed link to `address`. `false` means the address has had
/// enough mail from this app for now, and this one must not be sent.
///
/// ## Why this is a row and not SHUTTER
///
/// Every "email me a link" form lets a stranger make this app mail an address
/// they do not own, so the budget has to belong to the **inbox**, whoever asks
/// and whichever process answers. In memory it was per process — N processes
/// admitted N× — and it could not be split among them either: a burst of three
/// divided by four rounds to zero. In the database every process sharing
/// `forge.db` charges the same row, and a restart does not refill anybody's
/// budget. That is exact for the only multi-process topology this framework
/// serves (13.2 refuses the multi-machine one). The cost is one write per mail
/// request, on a path SHUTTER already rations to ten a minute per caller.
///
/// ## The algorithm
///
/// GCRA in one statement: the row holds the *theoretical arrival time*; a
/// charge moves it to `max(tat, now) + interval`, and is admitted only if that
/// stays within `burst × interval` of now. One `INSERT … ON CONFLICT DO UPDATE …
/// WHERE` is a single atomic write, so two processes charging at once cannot
/// both spend the last unit: SQLite serialises the writes, and the second sees
/// the first's `tat`. The update's `WHERE` is the admission test — a refused
/// charge changes nothing, so refusals do not push the refill further away.
///
/// # Errors
/// [`StoreError`] on substrate failure. Callers fail closed: no budget
/// answer, no mail.
pub async fn admit_mail(db: &dyn DataSubstrate, address: &str, now_ms: i64) -> Result<bool> {
    let changed = db
        .execute(
            &format!(
                "INSERT INTO {MAIL_BUDGET} (address, tat) VALUES (?1, ?2 + ?3) \
                 ON CONFLICT(address) DO UPDATE SET tat = MAX({MAIL_BUDGET}.tat, ?2) + ?3 \
                 WHERE MAX({MAIL_BUDGET}.tat, ?2) + ?3 - ?2 <= ?4"
            ),
            &[
                SqlValue::Text(address.to_string()),
                SqlValue::Integer(now_ms),
                SqlValue::Integer(MAIL_INTERVAL_MS),
                SqlValue::Integer(MAIL_BURST * MAIL_INTERVAL_MS),
            ],
        )
        .await?;
    Ok(changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::password::hash_password;
    use crate::auth::store::{self, ProviderProfile};
    use crate::forge::libsql::LibSqlSubstrate;

    const NOW: i64 = 1_789_600_000_000;

    async fn db() -> LibSqlSubstrate {
        let db = LibSqlSubstrate::open_ephemeral().await.expect("opens");
        for collection in crate::auth::schema::collections() {
            for statement in collection.migrations.iter() {
                db.migrate(statement).await.expect("auth ddl");
            }
        }
        for statement in ddl() {
            db.migrate(&statement).await.expect("email ddl");
        }
        db
    }

    async fn account(db: &LibSqlSubstrate, email: &str) -> PrincipalId {
        store::create_credential_account(
            db,
            "password",
            email,
            &ProviderProfile { email: Some(email.into()), ..ProviderProfile::default() },
            &hash_password("old password 1").expect("hashes"),
            NOW,
        )
        .await
        .expect("writes")
        .expect("new account")
        .id
    }

    #[tokio::test]
    async fn a_token_redeems_exactly_once() {
        let db = db().await;
        let who = account(&db, "ada@x.dev").await;
        let token = issue(&db, Purpose::PasswordReset, "password", Some(&who), "ada@x.dev", Some("/home"), NOW)
            .await
            .expect("issues");

        let first = consume(&db, Purpose::PasswordReset, &token, NOW + 1).await.expect("reads");
        assert_eq!(
            first,
            Some(Redeemed {
                provider: "password".into(),
                principal: Some(who),
                email: "ada@x.dev".into(),
                next: Some("/home".into()),
            })
        );
        assert_eq!(consume(&db, Purpose::PasswordReset, &token, NOW + 2).await.expect("reads"), None);
    }

    /// Only the hash is stored: nothing in the table opens a reset form.
    #[tokio::test]
    async fn the_raw_token_is_never_written() {
        let db = db().await;
        let token = issue(&db, Purpose::MagicLink, "magiclink", None, "a@x.dev", None, NOW)
            .await
            .expect("issues");
        let rows = db
            .query(&format!("SELECT * FROM {EMAIL_TOKENS}"), &[])
            .await
            .expect("reads");
        let dump = format!("{:?}", rows.rows);
        assert!(!dump.contains(token.expose_for_cookie()), "{dump}");
        assert!(dump.contains(token.hash().as_str()));
    }

    #[tokio::test]
    async fn expiry_and_purpose_both_refuse() {
        let db = db().await;
        let token = issue(&db, Purpose::MagicLink, "magiclink", None, "a@x.dev", None, NOW)
            .await
            .expect("issues");
        assert!(!peek(&db, Purpose::PasswordReset, &token, NOW).await.expect("reads"));
        assert_eq!(consume(&db, Purpose::VerifyEmail, &token, NOW).await.expect("reads"), None);
        let after = NOW + Purpose::MagicLink.ttl_ms();
        assert!(!peek(&db, Purpose::MagicLink, &token, after).await.expect("reads"));
        assert_eq!(consume(&db, Purpose::MagicLink, &token, after).await.expect("reads"), None);
        assert!(peek(&db, Purpose::MagicLink, &token, NOW).await.expect("still there"));
        assert_eq!(purge_expired(&db, after).await.expect("purges"), 1);
    }

    #[tokio::test]
    async fn a_newer_link_voids_the_older_one() {
        let db = db().await;
        let who = account(&db, "ada@x.dev").await;
        let old = issue(&db, Purpose::PasswordReset, "password", Some(&who), "ada@x.dev", None, NOW)
            .await
            .expect("issues");
        let new = issue(&db, Purpose::PasswordReset, "password", Some(&who), "ada@x.dev", None, NOW + 5)
            .await
            .expect("issues");
        assert_eq!(consume(&db, Purpose::PasswordReset, &old, NOW + 6).await.expect("reads"), None);
        assert!(consume(&db, Purpose::PasswordReset, &new, NOW + 6).await.expect("reads").is_some());
    }

    /// The reset contract: new hash, no sessions, no other reset links.
    #[tokio::test]
    async fn a_reset_ends_every_session_and_voids_other_links() {
        let db = db().await;
        let who = account(&db, "ada@x.dev").await;
        store::create_session(&db, &who, "password", NOW, 60_000).await.expect("session");
        store::create_session(&db, &who, "password", NOW, 60_000).await.expect("session");
        let other = issue(&db, Purpose::PasswordReset, "password", Some(&who), "ada@x.dev", None, NOW)
            .await
            .expect("issues");

        let new_hash = hash_password("brand new password").expect("hashes");
        assert!(reset_password(&db, &who, "password", "ada@x.dev", &new_hash, NOW).await.expect("writes"));

        assert!(store::sessions_for(&db, &who, NOW).await.expect("reads").is_empty());
        assert!(!peek(&db, Purpose::PasswordReset, &other, NOW).await.expect("reads"));
        let credential = store::lookup_credential(&db, "password", "ada@x.dev")
            .await
            .expect("reads")
            .expect("exists");
        assert!(crate::auth::password::verify_password(&credential.secret_hash, "brand new password"));
        assert!(!crate::auth::password::verify_password(&credential.secret_hash, "old password 1"));
    }

    #[tokio::test]
    async fn a_reset_for_a_principal_with_no_password_changes_nothing() {
        let db = db().await;
        let stranger = PrincipalId::mint();
        assert!(!reset_password(&db, &stranger, "password", "nobody@x.dev", "x", NOW).await.expect("reads"));
    }

    /// Case 4 then case 1: a new address makes a principal, and the next link
    /// for it signs in as the same one.
    #[tokio::test]
    async fn a_magic_link_creates_once_and_then_signs_in() {
        let db = db().await;
        let first = resolve_magic_link(&db, "magiclink", Some("password"), "new@x.dev", NOW)
            .await
            .expect("resolves");
        assert!(!first.took_over);
        let again = resolve_magic_link(&db, "magiclink", Some("password"), "new@x.dev", NOW + 1)
            .await
            .expect("resolves");
        assert_eq!(again.principal, first.principal);
    }

    /// Case 2: a verified password account and a magic link are one human.
    #[tokio::test]
    async fn a_magic_link_joins_the_principal_that_proved_the_address() {
        let db = db().await;
        let who = account(&db, "ada@x.dev").await;
        mark_verified(&db, &who, "ada@x.dev", NOW).await.expect("writes");
        store::create_session(&db, &who, "password", NOW, 60_000).await.expect("session");

        let linked = resolve_magic_link(&db, "magiclink", Some("password"), "ada@x.dev", NOW)
            .await
            .expect("resolves");
        assert_eq!(linked.principal, who);
        assert!(!linked.took_over);
        assert!(
            store::lookup_credential(&db, "password", "ada@x.dev").await.expect("reads").is_some(),
            "a proven password is kept"
        );
        assert_eq!(store::sessions_for(&db, &who, NOW).await.expect("reads").len(), 1);
    }

    /// Case 3 — the pre-account-takeover defence. Someone registered the
    /// address with a password and never proved it; the mailbox owner signs in
    /// by link. Their password stops working and their sessions end.
    #[tokio::test]
    async fn a_mailbox_gets_its_burst_then_one_link_per_interval() {
        let db = db().await;
        for _ in 0..MAIL_BURST {
            assert!(admit_mail(&db, "ada@x.dev", NOW).await.expect("charges"));
        }
        assert!(!admit_mail(&db, "ada@x.dev", NOW).await.expect("charges"), "burst spent");
        assert!(admit_mail(&db, "bob@x.dev", NOW).await.expect("charges"), "another inbox is untouched");
        // Refusals must not push the refill further away: hammering the form
        // cannot keep the owner locked out.
        for _ in 0..50 {
            assert!(!admit_mail(&db, "ada@x.dev", NOW + 1).await.expect("charges"));
        }
        assert!(admit_mail(&db, "ada@x.dev", NOW + MAIL_INTERVAL_MS).await.expect("charges"));
        assert!(!admit_mail(&db, "ada@x.dev", NOW + MAIL_INTERVAL_MS).await.expect("charges"));
        // Fully refilled rows are swept.
        assert!(purge_expired(&db, NOW + 10 * MAIL_BURST * MAIL_INTERVAL_MS).await.expect("purges") >= 2);
    }

    /// 🔑 The point of moving it out of memory: two processes on one
    /// `forge.db` — here, two independent connections to one file, which is what
    /// two processes are to SQLite — spend ONE budget between them, including
    /// when they race.
    #[tokio::test]
    async fn two_processes_on_one_database_share_one_mailbox_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("forge.db");
        let a = LibSqlSubstrate::open_local(&path).await.expect("opens");
        for statement in ddl() {
            a.migrate(&statement).await.expect("ddl");
        }
        let b = LibSqlSubstrate::open_local(&path).await.expect("opens again");

        let charges = (0..12).map(|i| {
            let handle = if i % 2 == 0 { &a } else { &b };
            async move { admit_mail(handle, "ada@x.dev", NOW).await.expect("charges") }
        });
        let admitted = futures_util::future::join_all(charges)
            .await
            .into_iter()
            .filter(|admitted| *admitted)
            .count();
        assert_eq!(
            admitted, MAIL_BURST as usize,
            "twelve concurrent charges from two processes admit exactly the burst"
        );
    }

    /// A squatter who managed to attach other ways in — another credential, an
    /// OAuth identity — loses all of them when the mailbox owner claims the
    /// address. Only the proven identity remains.
    #[tokio::test]
    async fn a_takeover_strips_every_unproven_way_in() {
        let db = db().await;
        let squatter = account(&db, "ada@x.dev").await;
        let who = SqlValue::Text(squatter.as_str().to_string());
        db.execute(
            &format!(
                "INSERT INTO {ACCOUNTS} (principal, provider, subject, created_at) \
                 VALUES (?1, 'github', 'squatter-gh-id', 0)"
            ),
            std::slice::from_ref(&who),
        )
        .await
        .expect("trojan identifier");
        db.execute(
            &format!(
                "INSERT INTO {CREDENTIALS} (principal, provider, credential_id, public_key, created_at) \
                 VALUES (?1, 'passkey', 'squatter-key', 'pk', 0)"
            ),
            std::slice::from_ref(&who),
        )
        .await
        .expect("planted passkey");
        issue(&db, Purpose::PasswordReset, "password", Some(&squatter), "ada@x.dev", None, NOW)
            .await
            .expect("an outstanding link");

        let linked = resolve_magic_link(&db, "magiclink", Some("password"), "ada@x.dev", NOW)
            .await
            .expect("resolves");
        assert!(linked.took_over);

        let count = |table: &'static str| {
            let db = &db;
            let who = who.clone();
            async move {
                db.query(&format!("SELECT COUNT(*) FROM {table} WHERE principal = ?1"), &[who])
                    .await
                    .expect("reads")
                    .rows[0]
                    .get(0)
                    .cloned()
                    .expect("a count")
            }
        };
        assert_eq!(count(CREDENTIALS).await, SqlValue::Integer(0), "no credential survives");
        assert_eq!(count(EMAIL_TOKENS).await, SqlValue::Integer(0), "no outstanding link survives");
        let accounts = db
            .query(
                &format!("SELECT provider FROM {ACCOUNTS} WHERE principal = ?1"),
                std::slice::from_ref(&who),
            )
            .await
            .expect("reads");
        let providers: Vec<_> = accounts.rows.iter().filter_map(|r| r.get(0).cloned()).collect();
        assert_eq!(providers, vec![SqlValue::Text("magiclink".into())], "only the proven identity");

        // …and the owner can still set a first password afterwards.
        mark_verified(&db, &linked.principal, "ada@x.dev", NOW).await.expect("writes");
        let hash = hash_password("owners own password").expect("hashes");
        assert!(reset_password(&db, &linked.principal, "password", "ada@x.dev", &hash, NOW)
            .await
            .expect("writes"));
    }

    #[tokio::test]
    async fn a_magic_link_takes_over_an_unproven_password_account() {
        let db = db().await;
        let squatter = account(&db, "ada@x.dev").await;
        store::create_session(&db, &squatter, "password", NOW, 60_000).await.expect("session");

        let linked = resolve_magic_link(&db, "magiclink", Some("password"), "ada@x.dev", NOW)
            .await
            .expect("resolves");
        assert_eq!(linked.principal, squatter);
        assert!(linked.took_over);
        assert!(
            store::lookup_credential(&db, "password", "ada@x.dev").await.expect("reads").is_none(),
            "the unproven password must not survive"
        );
        assert!(store::sessions_for(&db, &squatter, NOW).await.expect("reads").is_empty());
    }

    /// A magic-link user can set a first password through a reset link, and it
    /// lands on the same principal.
    #[tokio::test]
    async fn a_reset_link_gives_a_magic_link_user_a_first_password() {
        let db = db().await;
        let linked = resolve_magic_link(&db, "magiclink", Some("password"), "bo@x.dev", NOW)
            .await
            .expect("resolves");
        mark_verified(&db, &linked.principal, "bo@x.dev", NOW).await.expect("writes");
        assert_eq!(verified_principal(&db, "bo@x.dev").await.expect("reads"), Some(linked.principal.clone()));

        let hash = hash_password("a first password").expect("hashes");
        assert!(reset_password(&db, &linked.principal, "password", "bo@x.dev", &hash, NOW)
            .await
            .expect("writes"));
        let credential = store::lookup_credential(&db, "password", "bo@x.dev")
            .await
            .expect("reads")
            .expect("created");
        assert_eq!(credential.principal, linked.principal);
    }

    /// The first-password path must not hand a principal somebody else's login.
    #[tokio::test]
    async fn a_first_password_cannot_claim_an_address_another_principal_holds() {
        let db = db().await;
        let owner = account(&db, "ada@x.dev").await;
        let other = resolve_magic_link(&db, "magiclink", None, "zed@x.dev", NOW)
            .await
            .expect("resolves")
            .principal;
        assert_ne!(owner, other);
        assert!(!reset_password(&db, &other, "password", "ada@x.dev", "h", NOW)
            .await
            .expect("reads"));
        let credential = store::lookup_credential(&db, "password", "ada@x.dev")
            .await
            .expect("reads")
            .expect("still the owner's");
        assert_eq!(credential.principal, owner);
    }

    #[tokio::test]
    async fn verification_is_of_an_address_not_of_a_principal() {
        let db = db().await;
        let who = account(&db, "ada@x.dev").await;
        assert!(!is_verified(&db, &who, "ada@x.dev").await.expect("reads"));
        mark_verified(&db, &who, "ada@x.dev", NOW).await.expect("writes");
        mark_verified(&db, &who, "ada@x.dev", NOW + 1).await.expect("idempotent");
        assert!(is_verified(&db, &who, "ada@x.dev").await.expect("reads"));
        assert!(!is_verified(&db, &who, "ada@y.dev").await.expect("reads"), "a new address is unproven");
    }
}
