//! AUTH · P0 — the tables reach a real database, and carry the constraints that
//! make them correct rather than merely present.
//!
//! `src/auth/schema.rs` asserts what the DDL *says*. This asserts what libSQL
//! *does* with it, which is a different question in three places that matter:
//!
//! - A `UNIQUE` index only prevents a duplicate if the database enforces it. The
//!   `(provider, subject)` constraint is what makes a first login idempotent
//!   under a race, and a test that only greps the DDL string cannot tell a real
//!   constraint from a typo'd one.
//! - SQLite treats NULLs as *distinct* in a `UNIQUE` index. `albedo_credentials`
//!   relies on that — every password row has a NULL `credential_id` — and it is
//!   the kind of dialect behaviour that should be pinned by a test rather than
//!   remembered.
//! - The migrations run on every boot, so running them twice must be a no-op.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::schema;
use dom_render_compiler::forge::declare::{CollectionDecl, FieldSpec, FieldType};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};
use std::collections::BTreeMap;

/// An ordinary app schema, so the auth tables are always tested *beside*
/// somebody else's collections rather than alone.
fn app_schema() -> ForgeSchema {
    let mut declarations = BTreeMap::new();
    declarations.insert(
        "todos".to_string(),
        CollectionDecl {
            fields: [
                ("body".to_string(), FieldSpec::from(FieldType::Text)),
                ("owner".to_string(), FieldSpec::from(FieldType::Text)),
            ]
            .into_iter()
            .collect(),
            partition_by: Some("owner".to_string()),
            ..CollectionDecl::default()
        },
    );
    ForgeSchema::from_declarations(&declarations).expect("app schema lowers")
}

async fn booted() -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_ephemeral().await.expect("open");
    let schema = schema::augment(&app_schema()).expect("auth tables join the schema");
    bootstrap_schema(&db, &schema).await.expect("bootstrap");
    db
}

async fn table_exists(db: &LibSqlSubstrate, name: &str) -> bool {
    let rows = db
        .query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
            &[SqlValue::Text(name.to_string())],
        )
        .await
        .expect("query sqlite_master");
    !rows.rows.is_empty()
}

#[tokio::test]
async fn every_auth_table_is_created_beside_the_apps_own() {
    let db = booted().await;

    for name in schema::RESERVED_COLLECTIONS {
        assert!(
            table_exists(&db, name).await,
            "`{name}` was declared by AUTH but never created"
        );
    }
    assert!(
        table_exists(&db, "todos").await,
        "augmenting the schema must not drop the app's own collections"
    );
}

/// **The constraint that makes a first login idempotent.**
///
/// Two requests racing on the same new human both miss the account lookup and
/// both insert. Without this, that produces two principals for one person and
/// the loser's rows become unreachable — silently, because the id that owns
/// them is not the id their session carries.
#[tokio::test]
async fn a_second_account_for_one_provider_subject_is_refused_by_the_database() {
    let db = booted().await;

    let insert = "INSERT INTO albedo_accounts (principal, provider, subject, created_at) \
                  VALUES (?1, ?2, ?3, ?4)";
    let first = db
        .execute(
            insert,
            &[
                SqlValue::Text("u_first".to_string()),
                SqlValue::Text("google".to_string()),
                SqlValue::Text("104829901776232416982".to_string()),
                SqlValue::Integer(1_754_400_000_000),
            ],
        )
        .await;
    assert!(first.is_ok(), "the first login must land: {first:?}");

    let second = db
        .execute(
            insert,
            &[
                // A different principal — which is exactly what the losing side
                // of the race would have minted.
                SqlValue::Text("u_second".to_string()),
                SqlValue::Text("google".to_string()),
                SqlValue::Text("104829901776232416982".to_string()),
                SqlValue::Integer(1_754_400_000_001),
            ],
        )
        .await;
    assert!(
        second.is_err(),
        "the database must refuse a second principal for one (provider, subject)"
    );

    // The same subject at a *different* provider is a different person until
    // somebody links them, so it must still be insertable.
    let other_provider = db
        .execute(
            insert,
            &[
                SqlValue::Text("u_third".to_string()),
                SqlValue::Text("github".to_string()),
                SqlValue::Text("104829901776232416982".to_string()),
                SqlValue::Integer(1_754_400_000_002),
            ],
        )
        .await;
    assert!(
        other_provider.is_ok(),
        "one subject string at two providers is two accounts: {other_provider:?}"
    );
}

/// A session token that resolves to two rows resolves to whichever one the
/// planner reached first.
#[tokio::test]
async fn a_duplicate_session_token_hash_is_refused() {
    let db = booted().await;

    let insert = "INSERT INTO albedo_sessions \
                  (principal, token_hash, provider, created_at, expires_at) \
                  VALUES (?1, ?2, ?3, ?4, ?5)";
    let params = |principal: &str| {
        vec![
            SqlValue::Text(principal.to_string()),
            SqlValue::Text("d0f4…same-hash".to_string()),
            SqlValue::Text("passkey".to_string()),
            SqlValue::Integer(1_754_400_000_000),
            SqlValue::Integer(1_756_992_000_000),
        ]
    };

    assert!(db.execute(insert, &params("u_alice")).await.is_ok());
    assert!(
        db.execute(insert, &params("u_mallory")).await.is_err(),
        "one token hash must never name two sessions"
    );
}

/// SQLite treats NULLs as distinct in a `UNIQUE` index, and
/// `albedo_credentials` depends on it: a password row carries no WebAuthn
/// credential id, and every one of them has to coexist under the unique index
/// that exists for passkeys.
#[tokio::test]
async fn password_rows_coexist_under_the_passkey_unique_index() {
    let db = booted().await;

    let insert = "INSERT INTO albedo_credentials \
                  (principal, provider, credential_id, secret_hash, created_at) \
                  VALUES (?1, ?2, ?3, ?4, ?5)";

    for principal in ["u_alice", "u_bob", "u_carol"] {
        let inserted = db
            .execute(
                insert,
                &[
                    SqlValue::Text(principal.to_string()),
                    SqlValue::Text("password".to_string()),
                    SqlValue::Null,
                    SqlValue::Text(format!("$argon2id$…{principal}")),
                    SqlValue::Integer(1_754_400_000_000),
                ],
            )
            .await;
        assert!(
            inserted.is_ok(),
            "a password row must not collide with another password row: {inserted:?}"
        );
    }

    // …while two authenticators claiming one credential id still collide.
    let passkey = "INSERT INTO albedo_credentials \
                   (principal, provider, credential_id, public_key, created_at) \
                   VALUES (?1, ?2, ?3, ?4, ?5)";
    let params = |principal: &str| {
        vec![
            SqlValue::Text(principal.to_string()),
            SqlValue::Text("passkey".to_string()),
            SqlValue::Text("AQIDBAUGBwgJCgsMDQ4PEA".to_string()),
            SqlValue::Text("pQECAyYgASFYIA".to_string()),
            SqlValue::Integer(1_754_400_000_000),
        ]
    };
    assert!(db.execute(passkey, &params("u_alice")).await.is_ok());
    assert!(
        db.execute(passkey, &params("u_mallory")).await.is_err(),
        "one credential id must never name two principals"
    );
}

/// Two humans can register with the same email address at two providers, and
/// deciding whether they are one person is account linking — a policy, not a
/// constraint. A `UNIQUE` on email would turn "sign in with GitHub after
/// signing up with Google" into a crash.
#[tokio::test]
async fn two_users_may_share_an_email_address() {
    let db = booted().await;

    let insert = "INSERT INTO albedo_users (principal, email, created_at, updated_at) \
                  VALUES (?1, ?2, ?3, ?4)";
    for principal in ["u_alice", "u_alice_at_work"] {
        let inserted = db
            .execute(
                insert,
                &[
                    SqlValue::Text(principal.to_string()),
                    SqlValue::Text("ada@example.com".to_string()),
                    SqlValue::Integer(1_754_400_000_000),
                    SqlValue::Integer(1_754_400_000_000),
                ],
            )
            .await;
        assert!(inserted.is_ok(), "email must not be unique: {inserted:?}");
    }

    // The principal, however, is the identity everything else joins on.
    let duplicate_principal = db
        .execute(
            insert,
            &[
                SqlValue::Text("u_alice".to_string()),
                SqlValue::Text("someone.else@example.com".to_string()),
                SqlValue::Integer(1_754_400_000_001),
                SqlValue::Integer(1_754_400_000_001),
            ],
        )
        .await;
    assert!(
        duplicate_principal.is_err(),
        "one principal must never name two user rows"
    );
}

/// Migrations run on every boot, so the second boot must be a no-op rather than
/// an error — and must not disturb rows the first one left behind.
#[tokio::test]
async fn bootstrapping_twice_is_a_no_op() {
    let db = booted().await;

    db.execute(
        "INSERT INTO albedo_users (principal, created_at, updated_at) VALUES (?1, ?2, ?3)",
        &[
            SqlValue::Text("u_survivor".to_string()),
            SqlValue::Integer(1_754_400_000_000),
            SqlValue::Integer(1_754_400_000_000),
        ],
    )
    .await
    .expect("insert");

    let schema = schema::augment(&app_schema()).expect("augment");
    bootstrap_schema(&db, &schema)
        .await
        .expect("a second boot must not fail");

    let rows = db
        .query(
            "SELECT principal FROM albedo_users WHERE principal = ?1",
            &[SqlValue::Text("u_survivor".to_string())],
        )
        .await
        .expect("query");
    assert_eq!(rows.rows.len(), 1, "re-running migrations dropped a row");
}
