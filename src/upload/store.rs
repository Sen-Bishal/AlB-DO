//! UPLOADS · 15.1 — where the bytes go, and the row that remembers them.
//!
//! ## Bytes on disk, a row pointing at them
//!
//! Not a blob column. SQLite will happily hold a 5 MiB blob, and the cost shows
//! up everywhere else: the page cache fills with image data competing with the
//! rows a live query actually reads, every `SELECT *` that forgets to exclude
//! the column drags it across, and the backup story becomes "copy a database
//! that grew 40× because somebody uploaded holiday photos". The row here is
//! metadata only; the bytes are a file.
//!
//! ## Content-addressed, and what that buys
//!
//! The id **is** the SHA-256 of the contents, so:
//!
//! - **dedup is free** — the same file uploaded twice is one file on disk, and the second write is a
//!   rename onto a path that already exists;
//! - **the id is a perfect `ETag`**, because it cannot survive a change to the bytes, so a browser
//!   that already has a file revalidates for a `304`. (Not `immutable`: that let shared caches
//!   answer for the app, past any middleware gating the read.)
//! - **the id is not a capability.** It is derived from content anyone holding the file already has,
//!   so it must not be treated as an unguessable token. Every read is checked against the bucket's
//!   declared [`ReadRule`](crate::upload::ReadRule) — the uploader only, unless the bucket says
//!   `"signed-in"` or `"public"` — exactly as a FORGE row is checked against its author. This is
//!   written down because the opposite assumption — "the URL is unguessable, therefore it is
//!   private" — is the standard way upload stores leak, and this store made it until 2026-10-30.
//!
//! The bytes are shared; the records are not. Two people who upload the same
//! file get one file on disk and one record each, so each keeps their own name,
//! type and ownership of it.
//!
//! The hash is computed **while streaming**, so nothing is buffered to hash it.
//!
//! ## Why the layout has a fan-out directory
//!
//! `uploads/ab/abcd…` rather than `uploads/abcd…`. Not premature: a directory
//! with a hundred thousand entries is slow to list on every filesystem and
//! pathological on some, and the fix is unavailable later without moving every
//! file that already exists. Two hex characters is 256 buckets, which is the
//! cheapest version of this that works.

use crate::forge::substrate::DataSubstrate;
use crate::forge::value::SqlValue;
use std::fmt;
use std::path::{Path, PathBuf};

/// The table recording what was stored.
///
/// Under `albedo_`, the reserved prefix, for the same reason the four auth
/// tables are: an app that could declare `albedo_uploads` could rewrite the
/// record of what is on its own disk.
pub const UPLOADS: &str = "albedo_uploads";

/// Directory under the project root that holds the bytes.
///
/// Project-relative, deliberately — the same fix `forge.db` needed. An absolute
/// or working-directory-relative path makes a shipped binary write its uploads
/// wherever it happened to be launched from, which is the bug class deployment
/// Tier 0 closed.
pub const UPLOAD_DIR: &str = "uploads";

/// Length of a hex SHA-256.
const ID_LEN: usize = 64;

/// What went wrong storing or reading an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UploadStoreError {
    /// The substrate refused.
    Substrate(String),
    /// The filesystem refused.
    Io(String),
    /// An id that is not one of ours reached a path that would have opened a
    /// file with it.
    MalformedId,
}

impl fmt::Display for UploadStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Substrate(reason) => write!(f, "the upload record could not be written: {reason}"),
            Self::Io(reason) => write!(f, "the upload could not be stored: {reason}"),
            Self::MalformedId => write!(f, "that is not an upload id"),
        }
    }
}

impl std::error::Error for UploadStoreError {}

type Result<T> = std::result::Result<T, UploadStoreError>;

/// One stored file, as the row remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUpload {
    /// Hex SHA-256 of the contents. The id, and the path.
    pub id: String,
    /// Which declared bucket accepted it.
    pub bucket: String,
    /// The media type **we accepted it as** — checked against the bucket's
    /// `accept` list at upload time, and the type it is served back with.
    pub content_type: String,
    /// Size in bytes.
    pub byte_size: u64,
    /// The filename the browser sent, sanitised. Display only: it is
    /// attacker-supplied and is never part of a path.
    pub original_name: String,
    /// Who uploaded it, when anyone was signed in.
    pub principal: Option<String>,
}

/// Is this a well-formed upload id?
///
/// **The path guard.** Every filesystem path in this module is built from an id,
/// so an id that is not exactly 64 lowercase hex characters must never reach
/// one: `..`, a separator and an absolute path are all excluded by the alphabet
/// rather than by a blocklist, which is the version of this check that cannot
/// be incomplete.
#[must_use]
pub fn is_upload_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Where an id's bytes live, under `project_dir`.
///
/// # Errors
/// [`UploadStoreError::MalformedId`] for anything [`is_upload_id`] refuses —
/// returned rather than sanitised, because a caller that handed us a bad id has
/// a bug and quietly reading a different file is the worst way to report it.
pub fn path_for(project_dir: &Path, id: &str) -> Result<PathBuf> {
    if !is_upload_id(id) {
        return Err(UploadStoreError::MalformedId);
    }
    let (fan_out, rest) = id.split_at(2);
    Ok(project_dir.join(UPLOAD_DIR).join(fan_out).join(rest))
}

/// Reduce a browser-supplied filename to something safe to store and show.
///
/// Never used to build a path — [`path_for`] does that from the id alone — so
/// this is about display and `Content-Disposition`, not traversal. Kept anyway
/// because a name is echoed back into HTML and into a header, and both have
/// their own injection shapes.
#[must_use]
pub fn sanitise_name(raw: &str) -> String {
    let trimmed = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(120)
        .collect::<String>();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_string()
    } else {
        trimmed
    }
}

/// The collection the uploads table is emitted as.
#[must_use]
pub fn collection() -> crate::forge::ForgeCollection {
    crate::forge::ForgeCollection::new(
        UPLOADS,
        UPLOADS,
        format!(
            "SELECT id, created_at, upload_id, bucket, content_type, byte_size, original_name, \
             principal FROM {UPLOADS} ORDER BY id"
        ),
        "id",
        Box::new([
            format!(
                "CREATE TABLE IF NOT EXISTS {UPLOADS} (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 upload_id TEXT NOT NULL, \
                 bucket TEXT NOT NULL, \
                 content_type TEXT NOT NULL, \
                 byte_size INTEGER NOT NULL, \
                 original_name TEXT NOT NULL, \
                 principal TEXT, \
                 created_at TIMESTAMP NOT NULL)"
            ),
            // One row per (bytes, bucket, uploader). Not `(upload_id)` alone:
            // the same file legitimately belongs to two buckets with different
            // accept lists, read rules and lifetimes. And not `(upload_id,
            // bucket)`, which this was until 2026-10-30: under `INSERT OR
            // IGNORE` the second person to upload the same bytes got no row at
            // all, so they could neither read their own file under an owner
            // rule nor attach it to an email, and their filename and type were
            // never kept. The bytes on disk are still shared — dedup is the
            // file, not the record.
            format!("DROP INDEX IF EXISTS {UPLOADS}_identity"),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {UPLOADS}_owner_identity \
                 ON {UPLOADS} (upload_id, bucket, COALESCE(principal, ''))"
            ),
            // The read every serve does.
            format!("CREATE INDEX IF NOT EXISTS {UPLOADS}_upload_id ON {UPLOADS} (upload_id)"),
        ]),
        Box::new([]),
    )
}

/// Add the uploads table to an app's schema.
///
/// Goes through [`ForgeSchema::build`] rather than pushing onto the collection
/// list, for the reason `auth::schema::augment` does: that constructor is the
/// single funnel where slot collisions and unsafe identifiers are caught, and a
/// collection that skipped it would be the one nobody validated.
///
/// # Errors
/// [`crate::forge::ForgeSchemaError`] on a wire-slot collision with a
/// collection the app declared.
pub fn augment(
    schema: &crate::forge::ForgeSchema,
) -> std::result::Result<crate::forge::ForgeSchema, crate::forge::ForgeSchemaError> {
    let mut merged = schema.collections().to_vec();
    merged.push(collection());
    crate::forge::ForgeSchema::build(merged)
}

/// Record a stored file. Idempotent under the unique index.
///
/// # Errors
/// [`UploadStoreError::Substrate`] if the write fails.
pub async fn record(
    db: &dyn DataSubstrate,
    upload: &StoredUpload,
    now_ms: i64,
) -> Result<()> {
    // `INSERT OR IGNORE` rather than a read-then-write: the same bytes uploaded
    // twice is the *expected* case for a content-addressed store, not a race to
    // resolve, and the unique index already states the rule.
    db.execute(
        &format!(
            "INSERT OR IGNORE INTO {UPLOADS} \
             (upload_id, bucket, content_type, byte_size, original_name, principal, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"
        ),
        &[
            SqlValue::Text(upload.id.clone()),
            SqlValue::Text(upload.bucket.clone()),
            SqlValue::Text(upload.content_type.clone()),
            SqlValue::Integer(i64::try_from(upload.byte_size).unwrap_or(i64::MAX)),
            SqlValue::Text(upload.original_name.clone()),
            upload
                .principal
                .clone()
                .map_or(SqlValue::Null, SqlValue::Text),
            SqlValue::Integer(now_ms),
        ],
    )
    .await
    .map_err(|err| UploadStoreError::Substrate(err.to_string()))?;
    Ok(())
}

/// Every record of a stored file, oldest first — one per bucket and uploader.
///
/// All of them, because who may have a file depends on which record they can
/// claim: a reader's own record, or one in a bucket open to them. This was a
/// `LIMIT 1` with no `ORDER BY`, which made the answer to "whose file is this"
/// whichever row SQLite happened to return.
///
/// # Errors
/// [`UploadStoreError::Substrate`] if the read fails;
/// [`UploadStoreError::MalformedId`] before touching the substrate if the id is
/// not one of ours.
pub async fn records(db: &dyn DataSubstrate, id: &str) -> Result<Vec<StoredUpload>> {
    if !is_upload_id(id) {
        return Err(UploadStoreError::MalformedId);
    }
    let rows = db
        .query(
            &format!(
                "SELECT upload_id, bucket, content_type, byte_size, original_name, principal \
                 FROM {UPLOADS} WHERE upload_id = ?1 ORDER BY id"
            ),
            &[SqlValue::Text(id.to_string())],
        )
        .await
        .map_err(|err| UploadStoreError::Substrate(err.to_string()))?;

    Ok(rows
        .rows
        .iter()
        .map(|row| {
            let text = |index: usize| -> String {
                row.get(index)
                    .and_then(|value| match value {
                        SqlValue::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default()
            };
            StoredUpload {
                id: text(0),
                bucket: text(1),
                content_type: text(2),
                byte_size: row
                    .get(3)
                    .and_then(|value| match value {
                        SqlValue::Integer(n) => u64::try_from(*n).ok(),
                        _ => None,
                    })
                    .unwrap_or(0),
                original_name: text(4),
                principal: match row.get(5) {
                    Some(SqlValue::Text(text)) => Some(text.clone()),
                    _ => None,
                },
            }
        })
        .collect())
}

/// The record `reader` may read this file through, under `registry`'s rules —
/// their own record first, so they get back the name and type they uploaded it
/// with; otherwise the oldest record whose bucket lets them read it.
///
/// `None` when no record does. A caller answers that exactly as it answers a
/// file that was never stored, so the route does not say which it was.
#[must_use]
pub fn readable_by<'a>(
    records: &'a [StoredUpload],
    registry: &crate::upload::UploadRegistry,
    reader: Option<&str>,
) -> Option<&'a StoredUpload> {
    let permitted = |record: &&StoredUpload| {
        registry
            .read_rule(&record.bucket)
            .permits(reader, record.principal.as_deref())
    };
    records
        .iter()
        .filter(permitted)
        .find(|record| reader.is_some() && record.principal.as_deref() == reader)
        .or_else(|| records.iter().find(permitted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_exactly_a_lowercase_hex_sha256() {
        let good = "a".repeat(64);
        assert!(is_upload_id(&good));
        for bad in [
            "",
            &"a".repeat(63),
            &"a".repeat(65),
            &"A".repeat(64),
            &format!("{}..", "a".repeat(62)),
            &format!("{}/x", "a".repeat(62)),
            &"g".repeat(64),
        ] {
            assert!(!is_upload_id(bad), "`{bad}` must not be an id");
        }
    }

    /// The traversal guard, stated as the property rather than as a blocklist:
    /// nothing that is not an id can produce a path at all.
    #[test]
    fn a_path_can_only_be_built_from_a_real_id() {
        let root = Path::new("/srv/app");
        for bad in ["../../etc/passwd", "..", "/etc/passwd", "", "a/b"] {
            assert_eq!(path_for(root, bad), Err(UploadStoreError::MalformedId));
        }
        let id = format!("ab{}", "c".repeat(62));
        let path = path_for(root, &id).expect("a real id");
        assert!(path.ends_with(Path::new("uploads").join("ab").join("c".repeat(62))));
    }

    #[test]
    fn a_filename_is_reduced_to_something_safe_to_echo() {
        assert_eq!(sanitise_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitise_name(r"C:\Users\me\photo.png"), "photo.png");
        assert_eq!(sanitise_name(""), "file");
        assert_eq!(sanitise_name(".."), "file");
        assert_eq!(sanitise_name("a\"b.txt"), "ab.txt");
        assert_eq!(sanitise_name("with\nnewline.txt"), "withnewline.txt");
        assert_eq!(sanitise_name(&"x".repeat(500)).len(), 120);
    }

    /// The same bytes in two buckets are two records with two lifetimes, and
    /// the same bytes from two uploaders are two records with two owners; the
    /// same bytes from one uploader into one bucket are one.
    #[test]
    fn the_uniqueness_rule_is_bytes_bucket_and_uploader() {
        let ddl = collection().migrations.join(" ");
        assert!(
            ddl.contains("(upload_id, bucket, COALESCE(principal, ''))"),
            "the unique index must name the uploader: {ddl}"
        );
        // The old pair index is dropped, or it would keep refusing the second
        // uploader's row on every database created before the change.
        assert!(ddl.contains("DROP INDEX IF EXISTS albedo_uploads_identity"), "{ddl}");
    }

    fn stored(bucket: &str, principal: Option<&str>, name: &str) -> StoredUpload {
        StoredUpload {
            id: "a".repeat(64),
            bucket: bucket.to_string(),
            content_type: "application/json".to_string(),
            byte_size: 2,
            original_name: name.to_string(),
            principal: principal.map(str::to_string),
        }
    }

    fn registry(json: serde_json::Value) -> crate::upload::UploadRegistry {
        crate::upload::UploadRegistry::from_declarations(
            &serde_json::from_value(json).expect("parses"),
        )
        .expect("lowers")
    }

    #[test]
    fn a_reader_gets_their_own_record_or_one_they_may_read_or_nothing() {
        let registry = registry(serde_json::json!({
            "docs": {},
            "team": { "read": "signed-in" },
            "pictures": { "read": "public" },
        }));

        // Two people uploaded the same bytes into an owner-only bucket.
        let shared = [
            stored("docs", Some("u_ada"), "ada.json"),
            stored("docs", Some("u_bob"), "bob.json"),
        ];
        let name = |reader| readable_by(&shared, &registry, reader).map(|r| r.original_name.as_str());
        assert_eq!(name(Some("u_ada")), Some("ada.json"));
        assert_eq!(name(Some("u_bob")), Some("bob.json"));
        assert_eq!(name(Some("u_eve")), None);
        assert_eq!(name(None), None);

        // Signed-in and public buckets, oldest record first.
        let team = [stored("team", Some("u_ada"), "plan.json")];
        assert!(readable_by(&team, &registry, Some("u_bob")).is_some());
        assert!(readable_by(&team, &registry, None).is_none());
        let pictures = [stored("pictures", None, "cat.json")];
        assert!(readable_by(&pictures, &registry, None).is_some());

        // A file in a public bucket and an owner bucket is readable through the
        // public record — but its owner still gets their own name back.
        let both = [
            stored("pictures", Some("u_bob"), "public-copy.json"),
            stored("docs", Some("u_ada"), "private-copy.json"),
        ];
        assert_eq!(
            readable_by(&both, &registry, Some("u_ada")).map(|r| r.original_name.as_str()),
            Some("private-copy.json")
        );
        assert_eq!(
            readable_by(&both, &registry, None).map(|r| r.original_name.as_str()),
            Some("public-copy.json")
        );

        // A bucket the config no longer declares is read as owner-only.
        let orphaned = [stored("removed", Some("u_ada"), "old.json")];
        assert!(readable_by(&orphaned, &registry, None).is_none());
        assert!(readable_by(&orphaned, &registry, Some("u_ada")).is_some());
    }

    /// The table is framework-owned, so it must sit under the reserved prefix
    /// an app cannot declare into.
    #[test]
    fn the_table_is_reserved() {
        assert!(crate::auth::schema::is_reserved(UPLOADS));
    }
}
