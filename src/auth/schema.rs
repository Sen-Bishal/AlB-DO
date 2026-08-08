//! AUTH · P0 — the tables, emitted rather than adapted.
//!
//! `AUTH.md` § 1: **the schema problem that killed Lucia is a non-problem here.**
//! Lucia's database adapters existed to abstract over a schema the library did
//! not control, the maintenance burden that abstraction created is the stated
//! reason it deprecated itself into a tutorial — and ALBEDO does not need an
//! adapter for your database *because it emitted your database*. This module is
//! that claim, cashed: four tables, generated from the same
//! [`ForgeCollection`](crate::forge::skeleton::ForgeCollection) machinery an
//! app's own `forge` block goes through.
//!
//! ## Why these are built as collections and not as loose DDL
//!
//! A session has to be a **row in a FORGE collection**, not a row in a side
//! table, because that is what makes `AUTH.md` § 5 true: a row change is already
//! a delta on the existing wire, so revoking a session on one device drops every
//! other tab's live lane in the same frame as any other write. Instant global
//! logout is not a feature built here — it is the delta kernel, already shipped,
//! pointed at [`SESSIONS`].
//!
//! ## Why they are hand-built rather than declared
//!
//! An app's `forge` block lowers through
//! [`CollectionDecl`](crate::forge::declare::CollectionDecl), which emits one
//! table, one optional partition index, and no uniqueness constraints. Auth
//! needs uniqueness in two places where its absence is a *correctness* bug
//! rather than a slow query:
//!
//! - `accounts (provider, subject)` — without it, two concurrent first logins for the same human
//!   create two principals, and the second one silently owns nothing.
//! - `sessions (token_hash)` — the lookup key for every authenticated request.
//!
//! Widening the app-facing declaration vocabulary to express those would be a
//! bigger change than building four fixed collections, and it would widen it for
//! a reason no app has yet asked for. So these are constructed directly, and the
//! `forge` block keeps the shape it has.
//!
//! ## The reservation
//!
//! Every table here lives under [`RESERVED_PREFIX`], and an app declaring a
//! collection with that prefix is a build error ([`is_reserved`]). Two distinct
//! reasons, and only the first is obvious:
//!
//! 1. An app that declares `albedo_sessions` would otherwise *replace* the session table, taking
//!    the DDL with it.
//! 2. A collection name is a **topic**, and a topic is readable. `useSharedSlot(albedo_users)` on
//!    an unpartitioned users table is every user's row. [`SESSIONS`] is partitioned by principal
//!    precisely so that P1's rule — a session that is not `u_7f3a` cannot name `…:u_7f3a` — is what
//!    protects it, but until P1 lands the reservation is the only thing standing there. It stays
//!    afterwards regardless.
//!
//! ## What is deliberately not stored
//!
//! **The session token itself.** [`SESSIONS`] stores `token_hash`; the bearer
//! value exists in the cookie and nowhere else on our side. A database read —
//! backup, log, replica, `SELECT *` in a support session — must not yield
//! anything that can be replayed as a login. This is `AUTH.md` invariant 2.6
//! applied to the one credential we mint ourselves.

use crate::forge::skeleton::{ForgeCollection, ForgeSchema, ForgeSchemaError};

/// Every table this module emits starts with this.
///
/// Deliberately `albedo_` and not `_albedo_`: a leading underscore is legal in
/// SQLite but reads as "private by convention", and this is private by
/// *enforcement* ([`is_reserved`]). The prefix is also what makes the whole auth
/// surface greppable in a database a stranger opens.
pub const RESERVED_PREFIX: &str = "albedo_";

/// The mirror: one row per human, holding the id we minted.
pub const USERS: &str = "albedo_users";
/// The `(provider, subject) → principal` map. What makes the id ours.
pub const ACCOUNTS: &str = "albedo_accounts";
/// Live sessions. Partitioned by principal so revocation is a delta.
pub const SESSIONS: &str = "albedo_sessions";
/// Passkey public keys and password hashes.
pub const CREDENTIALS: &str = "albedo_credentials";

/// Every reserved collection name, for error messages that list them.
pub const RESERVED_COLLECTIONS: &[&str] = &[USERS, ACCOUNTS, SESSIONS, CREDENTIALS];

/// Whether a collection name belongs to AUTH.
///
/// Checked against the **prefix**, not the four names, so adding a fifth table
/// later cannot silently un-reserve a name an app has meanwhile started using.
#[must_use]
pub fn is_reserved(name: &str) -> bool {
    name.starts_with(RESERVED_PREFIX)
}

/// The four collections, in a stable order.
///
/// Built fresh on each call rather than held in a `static` because
/// [`ForgeCollection`] owns its strings; the cost is four small allocations at
/// boot, once.
#[must_use]
pub fn collections() -> Vec<ForgeCollection> {
    vec![users(), accounts(), sessions(), credentials()]
}

/// Add the auth tables to an app's schema.
///
/// Rebuilt through [`ForgeSchema::build`] rather than pushed onto the existing
/// one, so the auth collections pass exactly the gates an app's own do —
/// identifier safety, and no two topics sharing a wire slot. That last one is
/// the reason this cannot be a simple concatenation: `albedo_sessions` hashing
/// onto the same slot as somebody's collection is a one-in-four-billion event
/// that would otherwise cross-deliver two topics' rows, and the only place it is
/// visible is where the whole set is.
///
/// # Errors
/// [`ForgeSchemaError`] from the rebuild.
pub fn augment(schema: &ForgeSchema) -> Result<ForgeSchema, ForgeSchemaError> {
    let mut merged: Vec<ForgeCollection> = schema.collections().to_vec();
    merged.extend(collections());
    ForgeSchema::build(merged)
}

/// One row per human. **The mirror side of `AUTH.md` R3.**
///
/// A delegated provider's users are not our rows — so we make a row beside
/// theirs. `principal` is the id we minted; the provider's own subject lives on
/// [`accounts`], never here. That is what lets a team move from Clerk to
/// first-party without every foreign key in their app changing meaning.
#[must_use]
pub fn users() -> ForgeCollection {
    let mut collection = ForgeCollection::new(
        USERS,
        USERS,
        format!(
            "SELECT id, created_at, email, image, name, principal, updated_at FROM {USERS} \
             ORDER BY id"
        ),
        "id",
        Box::new([
            format!(
                "CREATE TABLE IF NOT EXISTS {USERS} (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 principal TEXT NOT NULL, \
                 email TEXT, \
                 name TEXT, \
                 image TEXT, \
                 created_at TIMESTAMP NOT NULL, \
                 updated_at TIMESTAMP NOT NULL)"
            ),
            // The principal is the identity every other auth table joins on and
            // every principal-keyed topic is named after. Two rows claiming one
            // principal is not a slow query, it is two different humans behind
            // one id.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {USERS}_principal ON {USERS} (principal)"
            ),
            // Email is *not* unique. Two providers can report the same address
            // for what may or may not be the same human, and deciding that
            // question is account linking — a policy, not a constraint. A
            // UNIQUE here would turn "sign in with GitHub after signing up with
            // Google" into a crash, which is exactly the pre-verification
            // account-takeover shape that makes email-as-identity a bad idea.
            format!("CREATE INDEX IF NOT EXISTS {USERS}_email ON {USERS} (email)"),
        ]),
        Box::new([]),
    );
    collection.partition_by = Some("principal".to_string());
    collection
}

/// `(provider, subject) → principal`. The indirection that keeps the id ours.
#[must_use]
pub fn accounts() -> ForgeCollection {
    let mut collection = ForgeCollection::new(
        ACCOUNTS,
        ACCOUNTS,
        format!(
            "SELECT id, created_at, principal, provider, subject FROM {ACCOUNTS} ORDER BY id"
        ),
        "id",
        Box::new([
            format!(
                "CREATE TABLE IF NOT EXISTS {ACCOUNTS} (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 principal TEXT NOT NULL, \
                 provider TEXT NOT NULL, \
                 subject TEXT NOT NULL, \
                 created_at TIMESTAMP NOT NULL)"
            ),
            // **The constraint that makes first-login idempotent.** Without it,
            // two requests racing on the same new user both miss the lookup and
            // both insert, producing two principals for one human — and the
            // loser's rows become unreachable, because the id that owns them is
            // no longer the id their session carries. A race that corrupts
            // ownership silently is exactly the kind the database should refuse
            // rather than the application remember to avoid.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {ACCOUNTS}_identity \
                 ON {ACCOUNTS} (provider, subject)"
            ),
            // Account linking reads the other direction: every provider one
            // human has connected.
            format!(
                "CREATE INDEX IF NOT EXISTS {ACCOUNTS}_principal ON {ACCOUNTS} (principal)"
            ),
        ]),
        Box::new([]),
    );
    collection.partition_by = Some("principal".to_string());
    collection
}

/// Live sessions.
///
/// Partitioned by `principal`, which is the whole revocation story: the topic is
/// `albedo_sessions:u_7f3a`, a session that is not `u_7f3a` cannot name it, and
/// deleting a row fans out on the wire that already exists.
#[must_use]
pub fn sessions() -> ForgeCollection {
    let mut collection = ForgeCollection::new(
        SESSIONS,
        SESSIONS,
        format!(
            "SELECT id, created_at, expires_at, principal, provider, rotated_from, token_hash \
             FROM {SESSIONS} ORDER BY id"
        ),
        "id",
        Box::new([
            format!(
                "CREATE TABLE IF NOT EXISTS {SESSIONS} (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 principal TEXT NOT NULL, \
                 token_hash TEXT NOT NULL, \
                 provider TEXT NOT NULL, \
                 created_at TIMESTAMP NOT NULL, \
                 expires_at TIMESTAMP NOT NULL, \
                 rotated_from TEXT)"
            ),
            // The lookup key for every authenticated request, so it is indexed;
            // UNIQUE because a token that resolves to two sessions is a token
            // that resolves to whichever row the planner reached first.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {SESSIONS}_token ON {SESSIONS} (token_hash)"
            ),
            // `(principal, expires_at)` rather than `(principal)`: the two reads
            // are "this principal's live sessions" (a device list, and the
            // revocation fan-out) and "expire the stale ones", and both want the
            // second column.
            format!(
                "CREATE INDEX IF NOT EXISTS {SESSIONS}_principal \
                 ON {SESSIONS} (principal, expires_at)"
            ),
        ]),
        Box::new([]),
    );
    collection.partition_by = Some("principal".to_string());
    collection
}

/// Passkey public keys and password hashes.
///
/// One table for both because they are the same relation — *material this
/// principal can authenticate with* — and separating them would mean two lookups
/// on a login path that does not yet know which kind it is holding.
#[must_use]
pub fn credentials() -> ForgeCollection {
    let mut collection = ForgeCollection::new(
        CREDENTIALS,
        CREDENTIALS,
        format!(
            "SELECT id, created_at, credential_id, principal, provider, public_key, secret_hash, \
             sign_count FROM {CREDENTIALS} ORDER BY id"
        ),
        "id",
        Box::new([
            format!(
                "CREATE TABLE IF NOT EXISTS {CREDENTIALS} (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 principal TEXT NOT NULL, \
                 provider TEXT NOT NULL, \
                 credential_id TEXT, \
                 public_key TEXT, \
                 secret_hash TEXT, \
                 sign_count INTEGER, \
                 created_at TIMESTAMP NOT NULL)"
            ),
            // A WebAuthn credential id is globally unique and arrives from the
            // authenticator, so it is the lookup key for a passkey assertion.
            //
            // SQLite treats NULLs as distinct in a UNIQUE index, which is the
            // behaviour this relies on rather than tolerates: a password row has
            // no credential id, and every one of them must be able to coexist.
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {CREDENTIALS}_credential \
                 ON {CREDENTIALS} (credential_id)"
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {CREDENTIALS}_principal ON {CREDENTIALS} (principal)"
            ),
        ]),
        Box::new([]),
    );
    collection.partition_by = Some("principal".to_string());
    collection
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_emitted_collection_is_reserved() {
        for collection in collections() {
            assert!(
                is_reserved(&collection.topic),
                "`{}` is emitted by AUTH but an app could declare it",
                collection.topic
            );
            assert!(is_reserved(&collection.table));
        }
    }

    #[test]
    fn the_named_constants_match_what_is_emitted() {
        let emitted: HashSet<String> = collections()
            .into_iter()
            .map(|collection| collection.topic)
            .collect();
        let named: HashSet<String> = RESERVED_COLLECTIONS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(emitted, named);
    }

    #[test]
    fn an_apps_own_collection_name_is_not_reserved() {
        for name in ["guestbook", "todos", "messages", "albedoodles"] {
            assert!(!is_reserved(name), "`{name}` must stay available to apps");
        }
        // …but anything wearing the prefix is, whether we emit it today or not.
        assert!(is_reserved("albedo_anything"));
    }

    /// The four tables all key their rows by principal, which is what lets every
    /// one of them be read as a per-principal partition rather than a table
    /// scan — and, for [`SESSIONS`], what makes revocation a delta.
    #[test]
    fn every_collection_is_partitioned_by_principal() {
        for collection in collections() {
            assert_eq!(
                collection.partition_by.as_deref(),
                Some("principal"),
                "`{}` must be readable one principal at a time",
                collection.topic
            );
        }
    }

    /// Topics hash to wire slot ids, and a collision is undetectable at the
    /// wire. `ForgeSchema::build` checks this across the whole set, but these
    /// four ship together in every app, so they are checked on their own too.
    #[test]
    fn the_auth_collections_do_not_collide_on_wire_slots() {
        let mut seen = HashSet::new();
        for collection in collections() {
            assert!(
                seen.insert(collection.slot_id),
                "`{}` collides on slot {:#010x}",
                collection.topic,
                collection.slot_id.0
            );
        }
    }

    /// **Invariant 2.6.** The bearer token exists in the cookie and nowhere
    /// else; a database read must not yield anything replayable as a login.
    #[test]
    fn the_session_table_stores_a_hash_and_never_the_token() {
        let sessions = sessions();
        let ddl = sessions.migrations.join(" ");
        assert!(ddl.contains("token_hash"), "the hash column must exist");
        // A column literally called `token` would be the mistake this guards.
        assert!(
            !ddl.contains(" token TEXT") && !ddl.contains("(token "),
            "the raw token must have no column: {ddl}"
        );
        assert!(
            !sessions.query.contains(" token,") && !sessions.query.contains(" token "),
            "the raw token must not be selectable: {}",
            sessions.query
        );
    }

    /// The constraint that makes a first login idempotent under a race. Losing
    /// it turns concurrent first logins into two principals for one human.
    #[test]
    fn accounts_are_unique_on_provider_and_subject() {
        let ddl = accounts().migrations.join(" ");
        assert!(
            ddl.contains("CREATE UNIQUE INDEX") && ddl.contains("(provider, subject)"),
            "missing the uniqueness that makes first login idempotent: {ddl}"
        );
    }

    /// Email is deliberately *not* unique — see [`users`]. Asserted because the
    /// obvious "improvement" is to add it, and doing so breaks signing in with a
    /// second provider.
    #[test]
    fn user_email_is_indexed_but_not_unique() {
        let ddl = users().migrations.join(" ");
        assert!(ddl.contains("_email ON"), "email must be indexed: {ddl}");
        assert!(
            !ddl.contains("CREATE UNIQUE INDEX IF NOT EXISTS albedo_users_email"),
            "email must not be unique — two providers may report one address: {ddl}"
        );
    }

    /// Every migration must be re-runnable, because they run on every boot.
    #[test]
    fn all_migrations_are_idempotent() {
        for collection in collections() {
            for statement in collection.migrations.iter() {
                assert!(
                    statement.contains("IF NOT EXISTS"),
                    "`{}` has a migration that cannot survive a second boot: {statement}",
                    collection.topic
                );
            }
        }
    }

    /// The query's column list is what the wire carries, so it must name
    /// exactly the columns the DDL creates — a mismatch is a runtime error on
    /// the first read rather than a build failure.
    #[test]
    fn every_selected_column_exists_in_the_ddl() {
        for collection in collections() {
            let create = collection
                .migrations
                .first()
                .expect("first migration is the CREATE TABLE")
                .clone();
            let select = collection
                .query
                .split(" FROM ")
                .next()
                .expect("query has a FROM")
                .trim_start_matches("SELECT ")
                .to_string();
            for column in select.split(',') {
                let column = column.trim();
                assert!(
                    create.contains(column),
                    "`{}` selects `{column}`, which the DDL does not create",
                    collection.topic
                );
            }
        }
    }
}
