//! R1a · roles against a real substrate: granting, revoking, listing, and the
//! roles a resolved session carries.
//!
//! Feature-gated on `forge` because it needs the libSQL backend.

#![cfg(feature = "forge")]

use dom_render_compiler::auth::principal::PrincipalId;
use dom_render_compiler::auth::session::SessionToken;
use dom_render_compiler::auth::store::{
    create_session, delete_principal, grant_role, load_principal, principals_with_email,
    resolve_session, revoke_role, role_grants, upsert_principal, GrantOutcome, ProviderProfile,
};
use dom_render_compiler::auth::{schema, Principal};
use dom_render_compiler::forge::skeleton::{bootstrap_schema, ForgeSchema};
use dom_render_compiler::forge::value::SqlValue;
use dom_render_compiler::forge::{DataSubstrate, LibSqlSubstrate};

const NOW: i64 = 1_754_400_000_000;
const DAY_MS: i64 = 86_400_000;

async fn booted() -> LibSqlSubstrate {
    let db = LibSqlSubstrate::open_ephemeral().await.expect("open");
    let schema = schema::augment(&ForgeSchema::build(Vec::new()).expect("empty schema"))
        .expect("auth tables");
    bootstrap_schema(&db, &schema).await.expect("bootstrap");
    db
}

async fn person(db: &LibSqlSubstrate, subject: &str, email: &str) -> (Principal, SessionToken) {
    let profile = ProviderProfile {
        email: Some(email.to_string()),
        name: None,
        image: None,
        claims: serde_json::Map::new(),
    };
    let principal = upsert_principal(db, "password", subject, &profile, NOW)
        .await
        .expect("upsert");
    let token = create_session(db, &principal.id, "password", NOW, 30 * DAY_MS)
        .await
        .expect("session");
    (principal, token)
}

async fn resolved_roles(db: &LibSqlSubstrate, token: &SessionToken) -> Vec<String> {
    resolve_session(db, token, NOW + 1)
        .await
        .expect("resolve")
        .principal()
        .cloned()
        .expect("a live session")
        .roles
}

/// The roles ride the session resolution every authenticated request makes,
/// sorted, and the job path (`load_principal`) sees the same list.
#[tokio::test]
async fn granted_roles_reach_the_resolved_session_and_the_job_path() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada", "ada@example.com").await;
    assert!(resolved_roles(&db, &token).await.is_empty());

    for role in ["payroll", "admin"] {
        assert_eq!(
            grant_role(&db, &ada.id, role, Some("cli"), NOW).await.expect("grant"),
            GrantOutcome::Granted
        );
    }
    assert_eq!(resolved_roles(&db, &token).await, ["admin", "payroll"]);

    let loaded = load_principal(&db, &ada.id)
        .await
        .expect("load")
        .expect("exists");
    assert_eq!(loaded.roles, ["admin", "payroll"]);
}

/// A second grant of a held role changes nothing — the unique index makes it a
/// no-op rather than a duplicate a revoke would have to find twice.
#[tokio::test]
async fn granting_a_held_role_again_is_a_no_op() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada", "ada@example.com").await;
    grant_role(&db, &ada.id, "admin", Some("cli"), NOW).await.expect("grant");
    assert_eq!(
        grant_role(&db, &ada.id, "admin", Some("cli"), NOW).await.expect("grant"),
        GrantOutcome::AlreadyHeld
    );
    assert_eq!(resolved_roles(&db, &token).await, ["admin"]);
}

/// A grant to nobody must not leave an orphan row that a later principal with
/// a recycled id — or a restore — would inherit.
#[tokio::test]
async fn granting_to_an_unknown_principal_is_refused() {
    let db = booted().await;
    let stranger = PrincipalId::parse("u_doesnotexist").expect("valid id");
    assert_eq!(
        grant_role(&db, &stranger, "admin", Some("cli"), NOW).await.expect("grant"),
        GrantOutcome::NoSuchPrincipal
    );
    assert!(role_grants(&db, None).await.expect("list").is_empty());
}

/// Revocation takes effect on the very next resolution — there is no cache to
/// wait out, because there is no principal cache at all.
#[tokio::test]
async fn a_revoked_role_is_gone_at_the_next_resolution() {
    let db = booted().await;
    let (ada, token) = person(&db, "ada", "ada@example.com").await;
    grant_role(&db, &ada.id, "admin", Some("cli"), NOW).await.expect("grant");
    grant_role(&db, &ada.id, "payroll", Some("cli"), NOW).await.expect("grant");

    assert!(revoke_role(&db, &ada.id, "payroll").await.expect("revoke"));
    assert_eq!(resolved_roles(&db, &token).await, ["admin"]);
    assert!(
        !revoke_role(&db, &ada.id, "payroll").await.expect("revoke"),
        "revoking a role not held reports that nothing changed"
    );
}

/// The listing the CLI prints: every grant, or one role's, with the email
/// beside the id so an operator can read it.
#[tokio::test]
async fn role_grants_list_every_grant_or_one_roles() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada", "ada@example.com").await;
    let (bob, _) = person(&db, "bob", "bob@example.com").await;
    grant_role(&db, &ada.id, "admin", Some("cli"), NOW).await.expect("grant");
    grant_role(&db, &bob.id, "payroll", Some("cli"), NOW).await.expect("grant");
    grant_role(&db, &ada.id, "payroll", Some("cli"), NOW).await.expect("grant");

    let all = role_grants(&db, None).await.expect("list");
    assert_eq!(all.len(), 3);

    let payroll = role_grants(&db, Some("payroll")).await.expect("list");
    let mut emails: Vec<_> = payroll.iter().filter_map(|g| g.email.clone()).collect();
    emails.sort();
    assert_eq!(emails, ["ada@example.com", "bob@example.com"]);
    assert!(payroll.iter().all(|g| g.role == "payroll" && g.granted_by.as_deref() == Some("cli")));
}

/// The CLI accepts an email. Email is not unique (two providers may report one
/// address), so the lookup returns every match and the caller refuses when
/// there is more than one.
#[tokio::test]
async fn an_email_can_name_several_principals() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada", "ada@example.com").await;
    assert_eq!(
        principals_with_email(&db, "ADA@example.com").await.expect("lookup"),
        [ada.id.clone()],
        "email matching ignores case"
    );
    let (other, _) = person(&db, "ada-2", "ada@example.com").await;
    let mut found = principals_with_email(&db, "ada@example.com").await.expect("lookup");
    found.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    let mut expected = vec![ada.id, other.id];
    expected.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(found, expected);
    assert!(principals_with_email(&db, "nobody@example.com")
        .await
        .expect("lookup")
        .is_empty());
}

/// Erasing a person erases their grants. A role row outliving its principal is
/// personal data the erasure promised to remove.
#[tokio::test]
async fn deleting_a_principal_deletes_its_roles() {
    let db = booted().await;
    let (ada, _) = person(&db, "ada", "ada@example.com").await;
    grant_role(&db, &ada.id, "admin", Some("cli"), NOW).await.expect("grant");
    delete_principal(&db, &ada.id).await.expect("delete");
    let left = db
        .query(
            &format!("SELECT COUNT(*) FROM {} WHERE principal = ?1", schema::ROLES),
            &[SqlValue::Text(ada.id.as_str().to_string())],
        )
        .await
        .expect("count");
    assert_eq!(left.rows[0].get(0), Some(&SqlValue::Integer(0)));
}
