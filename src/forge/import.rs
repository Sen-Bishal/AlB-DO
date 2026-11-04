//! `albedo import` — rows from somewhere else, written the way an append writes
//! them, with their authors.
//!
//! ## Why this exists
//!
//! A row is changed only by the signed-in user who added it unless its
//! collection says `edit: "anyone"` (ALB-02). Who added it is a row in
//! `albedo_row_authors`, written by the append in the same transaction. A row
//! inserted any other way — a migration script, `sqlite3`, a restore — has no
//! author, and so **no request can ever update or remove it**. For an app moving
//! its existing data in, that is every row it has.
//!
//! So the import is an append, minus the request: each row goes in through
//! [`build_append_returning`](crate::forge::write) — the statement an action's
//! `append` builds — and its author is recorded through the same
//! `record_author`, in one transaction for the whole file. A file that fails
//! anywhere writes nothing.
//!
//! ## What a row may carry
//!
//! The collection's declared fields, and its key (`id`) to keep the source's
//! ids — which is what keeps references between collections pointing at the
//! right rows. Anything else is refused by name with its line number, because a
//! column the declaration does not know is either a typo or data the app will
//! never read. Values are checked against the declared type, and a `timestamp`
//! takes epoch milliseconds or an RFC 3339 string (what most exports write),
//! stored as milliseconds like every other timestamp.

use crate::auth::PrincipalId;
use crate::forge::declare::{FieldSpec, FieldType};
use crate::forge::skeleton::{ForgeCollection, ForgeSchema};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::SubstrateError;
use crate::forge::write::{build_append_returning, record_author};
use serde_json::{Map, Value};
use std::fmt;

/// One row to import.
#[derive(Debug, Clone)]
pub struct ImportRow {
    /// Where it came from (1-based), for errors.
    pub line: usize,
    /// The columns to write.
    pub record: Map<String, Value>,
    /// Who it belongs to. `None` leaves it with no author — changeable by nobody
    /// unless the collection says `edit: "anyone"`.
    pub author: Option<PrincipalId>,
}

/// What an import wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ImportSummary {
    /// Rows written.
    pub rows: usize,
    /// Of those, rows with a recorded author.
    pub authored: usize,
}

/// Why an import wrote nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The collection is not declared, or is framework-owned.
    Collection(String),
    /// A row is not what the declaration says; `line` is 1-based.
    Row {
        /// The line in the input.
        line: usize,
        /// What is wrong with it.
        reason: String,
    },
    /// The database refused; nothing was committed.
    Substrate(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Collection(reason) | Self::Substrate(reason) => f.write_str(reason),
            Self::Row { line, reason } => write!(f, "line {line}: {reason}"),
        }
    }
}

impl std::error::Error for ImportError {}

/// Write `rows` into `collection`, all or nothing.
///
/// # Errors
/// [`ImportError`] for an unknown or reserved collection, any row that does not
/// fit the declaration, or a refused write. Nothing is committed in any case.
pub async fn import_rows(
    substrate: &dyn DataSubstrate,
    schema: &ForgeSchema,
    collection: &str,
    rows: &[ImportRow],
) -> Result<ImportSummary, ImportError> {
    if crate::auth::schema::is_reserved(collection) {
        return Err(ImportError::Collection(format!(
            "`{collection}` is a framework table; `albedo import` writes the app's own collections"
        )));
    }
    let slot = schema.slot_for_topic(collection).ok_or_else(|| {
        let declared = schema
            .collections()
            .iter()
            .filter(|known| !crate::auth::schema::is_reserved(&known.topic))
            .map(|known| format!("`{}`", known.topic))
            .collect::<Vec<_>>();
        ImportError::Collection(format!(
            "no collection called `{collection}` is declared in albedo.config.ts{}",
            if declared.is_empty() {
                String::new()
            } else {
                format!(" — the declared ones are {}", declared.join(", "))
            }
        ))
    })?;

    // Every row is checked before the transaction opens: a bad line is the
    // author's to fix, and it should not cost a write lock to find out.
    let mut prepared = Vec::with_capacity(rows.len());
    for row in rows {
        let record = prepare(slot, row).map_err(|reason| ImportError::Row {
            line: row.line,
            reason,
        })?;
        prepared.push((row, record));
    }

    let substrate_err = |line: Option<usize>, err: SubstrateError| match line {
        Some(line) => ImportError::Row {
            line,
            reason: err.to_string(),
        },
        None => ImportError::Substrate(err.to_string()),
    };
    let tx = substrate
        .begin()
        .await
        .map_err(|err| substrate_err(None, err))?;
    let mut summary = ImportSummary::default();
    for (row, record) in &prepared {
        let written = async {
            let (sql, params, _columns) =
                build_append_returning(&slot.table, &slot.key_column, record)?;
            let returned = tx.query(&sql, &params).await?;
            if let Some(author) = &row.author {
                let key = returned.rows.first().and_then(|first| first.get(0).cloned());
                record_author(tx.as_ref(), slot, key, author).await?;
            }
            Ok::<_, SubstrateError>(())
        }
        .await;
        if let Err(err) = written {
            let _ = tx.rollback().await;
            return Err(substrate_err(Some(row.line), err));
        }
        summary.rows += 1;
        summary.authored += usize::from(row.author.is_some());
    }
    tx.commit().await.map_err(|err| substrate_err(None, err))?;
    Ok(summary)
}

/// Check one row against the declaration and lower its values to what an append
/// writes.
fn prepare(slot: &ForgeCollection, row: &ImportRow) -> Result<Map<String, Value>, String> {
    if row.record.is_empty() {
        return Err("the row is empty".to_string());
    }
    let mut out = Map::new();
    for (column, value) in &row.record {
        if column == &slot.key_column {
            match value {
                Value::Number(number) if number.as_i64().is_some_and(|id| id > 0) => {}
                Value::Null => continue,
                other => {
                    return Err(format!(
                        "`{column}` is the row's key and must be a positive whole number, not {other}"
                    ))
                }
            }
            out.insert(column.clone(), value.clone());
            continue;
        }
        let spec = if slot.fields.is_empty() {
            // An undeclared collection converts structurally, as its reads do.
            None
        } else {
            Some(slot.fields.get(column).ok_or_else(|| {
                format!(
                    "`{column}` is not a field of `{}` — its fields are {}",
                    slot.topic,
                    slot.fields
                        .keys()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?)
        };
        out.insert(column.clone(), lower(column, value, spec)?);
    }
    for (name, spec) in &slot.fields {
        if !spec.nullable && out.get(name).is_none_or(Value::is_null) {
            return Err(format!(
                "`{name}` is required (declared `{}`) and the row has no value for it",
                type_name(spec.ty)
            ));
        }
    }
    if let Some(partition) = &slot.partition_by {
        if out.get(partition).is_none_or(Value::is_null) {
            return Err(format!(
                "`{}` is partitioned by `{partition}`, and the row has no `{partition}`",
                slot.topic
            ));
        }
    }
    Ok(out)
}

fn lower(column: &str, value: &Value, spec: Option<&FieldSpec>) -> Result<Value, String> {
    let Some(spec) = spec else {
        return match value {
            Value::Array(_) | Value::Object(_) => Err(format!(
                "`{column}` holds {}; a column holds one value",
                if value.is_array() { "a list" } else { "an object" }
            )),
            other => Ok(other.clone()),
        };
    };
    if value.is_null() {
        return if spec.nullable {
            Ok(Value::Null)
        } else {
            Err(format!("`{column}` is required and is null"))
        };
    }
    let wrong = || {
        format!(
            "`{column}` is declared `{}`, and {value} is not one",
            type_name(spec.ty)
        )
    };
    match spec.ty {
        FieldType::Text => value.is_string().then(|| value.clone()).ok_or_else(wrong),
        FieldType::Int => value
            .as_i64()
            .map(Value::from)
            .ok_or_else(wrong),
        FieldType::Real => value.as_f64().map(Value::from).ok_or_else(wrong),
        FieldType::Bool => value.as_bool().map(Value::from).ok_or_else(wrong),
        FieldType::Timestamp => match value {
            Value::Number(number) => number.as_i64().map(Value::from).ok_or_else(wrong),
            Value::String(text) => chrono::DateTime::parse_from_rfc3339(text)
                .map(|moment| Value::from(moment.timestamp_millis()))
                .map_err(|_| {
                    format!(
                        "`{column}` is a timestamp: epoch milliseconds, or an RFC 3339 string \
                         like 2026-10-30T09:15:00Z — not {value}"
                    )
                }),
            _ => Err(wrong()),
        },
    }
}

fn type_name(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Text => "text",
        FieldType::Int => "int",
        FieldType::Real => "real",
        FieldType::Bool => "bool",
        FieldType::Timestamp => "timestamp",
    }
}

#[cfg(all(test, feature = "forge"))]
mod tests {
    use super::*;
    use crate::forge::declare::CollectionDecl;
    use crate::forge::write::{apply_writes, ForgeWrite};
    use crate::forge::LibSqlSubstrate;
    use crate::runtime::BroadcastRegistry;
    use std::collections::BTreeMap;

    fn schema() -> ForgeSchema {
        let decls: BTreeMap<String, CollectionDecl> = serde_json::from_value(serde_json::json!({
            "invoices": {
                "fields": { "title": "text", "amount": "real", "paid": "bool", "due": "timestamp", "note": "text?" }
            }
        }))
        .unwrap();
        ForgeSchema::from_declarations(&decls).unwrap()
    }

    fn row(line: usize, json: serde_json::Value, author: Option<&PrincipalId>) -> ImportRow {
        ImportRow {
            line,
            record: json.as_object().unwrap().clone(),
            author: author.cloned(),
        }
    }

    fn update(key: i64) -> ForgeWrite {
        let mut fields = Map::new();
        fields.insert("title".to_string(), Value::from("changed"));
        ForgeWrite::Update {
            collection: "invoices".to_string(),
            key: Value::from(key),
            fields,
        }
    }

    /// The point of the module: an imported row is its author's to change, like
    /// one they appended — and only theirs.
    #[tokio::test]
    async fn an_imported_row_is_changed_by_its_author_and_nobody_else() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = schema();
        crate::forge::skeleton::bootstrap_schema(&db, &schema).await.unwrap();
        let ada = PrincipalId::mint();
        let bob = PrincipalId::mint();

        let summary = import_rows(
            &db,
            &schema,
            "invoices",
            &[
                row(1, serde_json::json!({ "id": 41, "title": "rent", "amount": 1200.5, "paid": false, "due": "2026-11-01T00:00:00Z" }), Some(&ada)),
                row(2, serde_json::json!({ "id": 42, "title": "orphan", "amount": 1, "paid": true, "due": 0 }), None),
            ],
        )
        .await
        .expect("imports");
        assert_eq!(summary, ImportSummary { rows: 2, authored: 1 });

        // The source's ids survive, and the timestamp is stored as milliseconds.
        let stored = db
            .query("SELECT id, due FROM invoices ORDER BY id", &[])
            .await
            .unwrap();
        assert_eq!(stored.rows.len(), 2);
        assert_eq!(stored.rows[0].get(0), Some(&crate::forge::value::SqlValue::Integer(41)));
        assert_eq!(
            stored.rows[0].get(1),
            Some(&crate::forge::value::SqlValue::Integer(1_793_491_200_000))
        );

        let broadcast = BroadcastRegistry::new();
        let as_ = |who: &PrincipalId, key| {
            let writes = vec![update(key)];
            let schema = schema.clone();
            let who = who.clone();
            let db = &db;
            let broadcast = &broadcast;
            async move { apply_writes(db, broadcast, &schema, &writes, None, Some(&who)).await }
        };
        assert!(as_(&bob, 41).await.is_err(), "another user changed ada's imported row");
        assert!(as_(&ada, 41).await.is_ok(), "ada could not change her own imported row");
        assert!(as_(&ada, 42).await.is_err(), "an authorless row was changed");
    }

    /// One bad line and the file writes nothing, with the line named.
    #[tokio::test]
    async fn a_bad_line_writes_nothing_and_says_where() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = schema();
        crate::forge::skeleton::bootstrap_schema(&db, &schema).await.unwrap();
        let ok = || serde_json::json!({ "title": "a", "amount": 1, "paid": false, "due": 0 });

        for (bad, says) in [
            (serde_json::json!({ "title": "a", "amount": 1, "paid": false, "due": 0, "colour": "red" }), "`colour` is not a field"),
            (serde_json::json!({ "title": "a", "amount": "lots", "paid": false, "due": 0 }), "declared `real`"),
            (serde_json::json!({ "amount": 1, "paid": false, "due": 0 }), "`title` is required"),
            (serde_json::json!({ "title": "a", "amount": 1, "paid": false, "due": "next tuesday" }), "RFC 3339"),
            (serde_json::json!({ "id": "x7", "title": "a", "amount": 1, "paid": false, "due": 0 }), "positive whole number"),
        ] {
            let error = import_rows(&db, &schema, "invoices", &[row(1, ok(), None), row(2, bad, None)])
                .await
                .expect_err("refused");
            let message = error.to_string();
            assert!(message.starts_with("line 2:") && message.contains(says), "{message}");
        }
        let count = db.query("SELECT COUNT(*) FROM invoices", &[]).await.unwrap();
        assert_eq!(
            count.rows[0].get(0),
            Some(&crate::forge::value::SqlValue::Integer(0)),
            "a refused file wrote rows"
        );

        // A key that collides fails at the database — and still writes nothing.
        let colliding = [
            row(1, serde_json::json!({ "id": 7, "title": "a", "amount": 1, "paid": false, "due": 0 }), None),
            row(2, serde_json::json!({ "id": 7, "title": "b", "amount": 1, "paid": false, "due": 0 }), None),
        ];
        let error = import_rows(&db, &schema, "invoices", &colliding).await.expect_err("refused");
        assert!(error.to_string().starts_with("line 2:"), "{error}");
        let count = db.query("SELECT COUNT(*) FROM invoices", &[]).await.unwrap();
        assert_eq!(count.rows[0].get(0), Some(&crate::forge::value::SqlValue::Integer(0)));
    }

    #[tokio::test]
    async fn framework_tables_and_undeclared_collections_are_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = schema();
        let reserved = import_rows(&db, &schema, "albedo_users", &[]).await.unwrap_err();
        assert!(reserved.to_string().contains("framework table"), "{reserved}");
        let unknown = import_rows(&db, &schema, "invoice", &[]).await.unwrap_err();
        assert!(unknown.to_string().contains("`invoices`"), "{unknown}");
    }
}
