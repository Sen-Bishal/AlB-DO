//! App-declared FORGE schema — the `forge` block in `albedo.config.ts`.
//!
//! Phase 1 gave FORGE a real registry ([`ForgeSchema`]) but only one way to fill
//! it: the built-in [`ForgeSchema::guestbook_default`]. An app could not say what
//! its own data was. This module is the second way in — a declaration an author
//! writes, lowered to the same [`ForgeCollection`]s the default produces and
//! funnelled through the same [`ForgeSchema::build`], so every invariant that
//! held for the built-in (safe identifiers, no wire-slot collisions) holds for an
//! app's schema without a second check.
//!
//! # Declarative, not SQL
//!
//! The author declares *shape* — fields and their scalar types — and this module
//! emits the DDL, the materialising `SELECT`, and the seed `INSERT`s. That is the
//! whole point of the "no backend" wedge: writing `CREATE TABLE` by hand is the
//! thing ALBEDO exists to remove. It also keeps the generated query inside the
//! narrow grammar the write path already understands — a plain column list with a
//! plain `ORDER BY` — which is exactly what
//! [`parse_order_by`](crate::forge::skeleton::parse_order_by) needs to derive the
//! structured sort, and what the incremental-projection fast paths assume.
//!
//! # The conformance property
//!
//! **The built-in guestbook default must be expressible in this language, byte
//! for byte** — same DDL string, same query string, same seed statements. A
//! declaration language that cannot reproduce the thing it is replacing is too
//! weak, and the test that pins this (`guestbook_declaration_reproduces_the_builtin_default`)
//! is the one that would catch a divergence between the two paths.
//!
//! # Everything reaching SQL is generated, never passed through
//!
//! No string from the config is interpolated into SQL as-is. Table, key and field
//! names are identifier-validated; `order_by` is *parsed* into structured
//! [`SortKey`](crate::forge::skeleton::SortKey)s and then re-emitted from that
//! parse, so a hostile ordering clause cannot survive the round trip. Seed values
//! bind as parameters, never as text.

use super::skeleton::{parse_order_by, ForgeCollection, ForgeSchema, ForgeSchemaError, SeedRow};
use super::value::SqlValue;
use crate::forge::skeleton::SortDir;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The scalar shapes a declared field can take.
///
/// Deliberately few. These are the types that lower to a substrate column, a JSON
/// value, and a bound parameter without ambiguity; anything richer (enums,
/// relations) is a later rung and should not be faked with `TEXT` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Text,
    Int,
    Real,
    /// `true`/`false`. Stored as `0`/`1` — SQLite has no boolean — but it is a
    /// distinct *declared* type, which is what keeps it distinguishable from an
    /// `Int` both on the wire and in a schema diff.
    Bool,
    /// A moment in time, as **milliseconds since the Unix epoch**.
    ///
    /// Integer rather than ISO-8601 text because a timestamp's whole job is to
    /// be compared and ordered: `ORDER BY created_at` and `expires_at > ?` are
    /// exact on an integer and lexical accidents on a string. Milliseconds
    /// rather than seconds because the consumer is JavaScript — `Date.now()`
    /// and `new Date(n)` are both milliseconds, so a round trip through the
    /// wire needs no unit conversion and no place to forget one.
    ///
    /// (`reserve.rs`'s internal `created_at` uses `strftime('%s')` seconds. It
    /// is not user-facing and not read through this type; the two never meet.)
    Timestamp,
}

impl FieldType {
    /// The SQL column type as it is *declared*.
    ///
    /// `BOOLEAN` and `TIMESTAMP` both carry NUMERIC affinity in SQLite and store
    /// the integers they are given, so the storage is identical to `INTEGER` —
    /// but the declared name survives in `PRAGMA table_info`, which is what lets
    /// [`drift`](crate::forge::drift) catch a `bool` retyped to an `int` and
    /// what makes the table legible to someone opening it in a SQL shell.
    fn sql_type(self) -> &'static str {
        match self {
            FieldType::Text => "TEXT",
            FieldType::Int => "INTEGER",
            FieldType::Real => "REAL",
            FieldType::Bool => "BOOLEAN",
            FieldType::Timestamp => "TIMESTAMP",
        }
    }

    /// The spelling an author writes in the `forge` block.
    fn as_str(self) -> &'static str {
        match self {
            FieldType::Text => "text",
            FieldType::Int => "int",
            FieldType::Real => "real",
            FieldType::Bool => "bool",
            FieldType::Timestamp => "timestamp",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "text" => FieldType::Text,
            "int" => FieldType::Int,
            "real" => FieldType::Real,
            "bool" => FieldType::Bool,
            "timestamp" => FieldType::Timestamp,
            _ => return None,
        })
    }
}

/// A declared field: its type, and whether it accepts `null`.
///
/// Written as a single string in the config, with a trailing `?` for nullable —
/// `{ author: "text", nickname: "text?" }`. That spelling is deliberate: it is
/// what a TypeScript author already means by optional, it keeps `fields` a flat
/// string map rather than growing a nested object for one boolean, and it makes
/// the *required* case the shorter one to write, which is the one that should be
/// the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldSpec {
    pub ty: FieldType,
    /// `true` when the column omits `NOT NULL` and the wire may carry `null`.
    pub nullable: bool,
}

impl FieldSpec {
    #[must_use]
    pub fn new(ty: FieldType) -> Self {
        Self { ty, nullable: false }
    }

    #[must_use]
    pub fn nullable(ty: FieldType) -> Self {
        Self { ty, nullable: true }
    }

    /// The declaration string this spec round-trips through.
    fn as_declared(self) -> String {
        let mut out = self.ty.as_str().to_string();
        if self.nullable {
            out.push('?');
        }
        out
    }
}

/// Every field is required unless it says otherwise, so a bare [`FieldType`]
/// is a complete spec. Keeps the many `FieldType::Text` construction sites
/// (tests, the built-in default) reading as they did.
impl From<FieldType> for FieldSpec {
    fn from(ty: FieldType) -> Self {
        Self::new(ty)
    }
}

impl Serialize for FieldSpec {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_declared())
    }
}

impl<'de> Deserialize<'de> for FieldSpec {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let (name, nullable) = match raw.strip_suffix('?') {
            Some(base) => (base, true),
            None => (raw.as_str(), false),
        };
        let ty = FieldType::parse(name).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown field type '{raw}' — expected one of text, int, real, bool, timestamp, \
                 each optionally suffixed with '?' to allow null"
            ))
        })?;
        Ok(Self { ty, nullable })
    }
}

/// One app-declared collection, as it appears in the config's `forge` block.
///
/// ```ts
/// forge: {
///   guestbook: {
///     fields: { author: "text", message: "text" },
///     seed: [{ author: "ada", message: "first light" }],
///   },
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CollectionDecl {
    /// Physical table name. Defaults to the collection's own name, which is what
    /// an author almost always wants; kept overridable so a collection can front
    /// a table it does not share a name with.
    #[serde(default)]
    pub table: Option<String>,
    /// The row-identity column. Emitted as `INTEGER PRIMARY KEY AUTOINCREMENT`,
    /// which is what makes an append's key DB-assigned and monotonic. Defaults
    /// to `id`.
    #[serde(default)]
    pub key: Option<String>,
    /// Non-key columns, by name.
    ///
    /// A `BTreeMap`, so column order is alphabetical and therefore identical on
    /// every machine and every build. That order becomes the `SELECT` list and
    /// the seed `INSERT` column list, both of which are part of the bytes the
    /// wire carries — deriving it from a hash map's iteration order, or from
    /// whether `serde_json`'s `preserve_order` feature happens to be on, would
    /// make the rendered page depend on the build.
    pub fields: BTreeMap<String, FieldSpec>,
    /// The collection's ordering, e.g. `"id"` or `"created_at desc, id"`.
    /// Defaults to the key column ascending. Parsed and re-emitted, never
    /// interpolated.
    #[serde(default)]
    pub order_by: Option<String>,
    /// PRISM · the column this collection is partitioned by, when it is read one
    /// partition at a time: `useSharedSlot(messages.where({ room: params.id }))`.
    ///
    /// Must name a declared field (never the key). Declared, never inferred —
    /// guessing which column a key refers to has a silent wrong answer.
    ///
    /// **P0 records it and validates it; it does not yet change the query.** The
    /// `WHERE` clause and the matching composite index land together in P2, and
    /// they must land together: a partitioned read without
    /// `(partition, sort)` indexed is a full scan plus a sort on every read.
    #[serde(default)]
    pub partition_by: Option<String>,
    /// One-time seed rows, applied only when the table is empty. Keys must be
    /// declared fields; values bind as parameters.
    #[serde(default)]
    pub seed: Vec<BTreeMap<String, serde_json::Value>>,
}

impl CollectionDecl {
    /// Lower this declaration into the [`ForgeCollection`] the runtime uses.
    ///
    /// `topic` is the collection's name — the `useSharedSlot(topic)` key and the
    /// broadcast topic — taken from the declaration's key in the `forge` block.
    ///
    /// # Errors
    /// [`ForgeSchemaError::InvalidIdentifier`] for a table, key or field name
    /// that is not a plain SQL identifier; [`ForgeSchemaError::EmptyDeclaration`]
    /// for a collection with no fields; [`ForgeSchemaError::KeyIsAlsoAField`]
    /// when the key column is redeclared as a field;
    /// [`ForgeSchemaError::InvalidOrderBy`] for an ordering outside the plain
    /// grammar; [`ForgeSchemaError::UnknownSeedColumn`] /
    /// [`ForgeSchemaError::UnsupportedSeedValue`] for a seed row that does not
    /// match the declared fields.
    pub fn lower(&self, topic: &str) -> Result<ForgeCollection, ForgeSchemaError> {
        let table = self.table.clone().unwrap_or_else(|| topic.to_string());
        let key = self.key.clone().unwrap_or_else(|| "id".to_string());

        // Identifier-check everything that will be *generated into* SQL. The
        // schema's own `build` re-checks table and key; fields are only visible
        // here, so this is their one gate.
        for (field, value) in [("table", &table), ("key_column", &key)] {
            Self::require_identifier(topic, field, value)?;
        }
        if self.fields.is_empty() {
            return Err(ForgeSchemaError::EmptyDeclaration {
                topic: topic.to_string(),
            });
        }
        for column in self.fields.keys() {
            Self::require_identifier(topic, "field", column)?;
            if *column == key {
                return Err(ForgeSchemaError::KeyIsAlsoAField {
                    topic: topic.to_string(),
                    key: key.clone(),
                });
            }
        }

        // PRISM · a partition column must be a real, declared field. The key is
        // excluded on purpose: partitioning by row identity would make every
        // partition exactly one row, which is never what anyone means and is
        // silently useless rather than loudly wrong.
        if let Some(column) = &self.partition_by {
            Self::require_identifier(topic, "partition_by", column)?;
            if !self.fields.contains_key(column) {
                return Err(ForgeSchemaError::UnknownPartitionColumn {
                    topic: topic.to_string(),
                    column: column.clone(),
                    declared: self.fields.keys().cloned().collect(),
                });
            }
        }

        let columns: Vec<&str> = self.fields.keys().map(String::as_str).collect();

        // ── DDL ──
        // The key is the AUTOINCREMENT primary key; every declared field is a
        // NOT NULL column of its scalar type. `IF NOT EXISTS` because migrations
        // re-run on every boot.
        let mut ddl = format!("CREATE TABLE IF NOT EXISTS {table} ({key} INTEGER PRIMARY KEY AUTOINCREMENT");
        for (column, spec) in &self.fields {
            // A required field keeps the `NOT NULL` it always had — which is
            // what preserves the built-in default's DDL byte for byte. Only an
            // explicitly nullable one drops it.
            let required = if spec.nullable { "" } else { " NOT NULL" };
            ddl.push_str(&format!(", {column} {}{required}", spec.ty.sql_type()));
        }
        ddl.push(')');

        // ── The materialising query ──
        //
        // A partitioned collection is read one partition at a time, so the query
        // carries a `WHERE` and takes a bound parameter. The column name comes
        // from the schema and was identifier-checked above; the *value* is only
        // ever bound, never interpolated, so there is no path from a URL segment
        // to SQL text.
        let order_by = self.order_by_sql(topic, &key)?;
        let select = format!("SELECT {key}, {} FROM {table}", columns.join(", "));
        let query = match &self.partition_by {
            Some(column) => format!("{select} WHERE {column} = ?1 ORDER BY {order_by}"),
            None => format!("{select} ORDER BY {order_by}"),
        };

        // ── The index that makes the `WHERE` worth having ──
        //
        // Emitted in the same breath as the clause it serves, deliberately.
        // Without `(partition, sort…)` indexed, every partitioned read is a full
        // table scan plus a sort — indistinguishable from the unpartitioned
        // version at one partition, and unusable at a thousand. Shipping the
        // `WHERE` without the index would be a performance cliff that only
        // appears under real data.
        let mut migrations = vec![ddl];
        if let Some(column) = &self.partition_by {
            let mut index_columns = vec![column.clone()];
            let mut name_parts = vec![table.clone(), column.clone()];
            for sort in parse_order_by(&query).iter() {
                if &sort.column == column {
                    continue;
                }
                name_parts.push(sort.column.clone());
                index_columns.push(match sort.dir {
                    SortDir::Asc => sort.column.clone(),
                    SortDir::Desc => format!("{} DESC", sort.column),
                });
            }
            migrations.push(format!(
                "CREATE INDEX IF NOT EXISTS idx_{} ON {table}({})",
                name_parts.join("_"),
                index_columns.join(", ")
            ));
        }

        // ── Seeds ──
        let mut seed = Vec::with_capacity(self.seed.len());
        for row in &self.seed {
            seed.push(self.lower_seed_row(topic, &table, row)?);
        }

        Ok(ForgeCollection::new(
            topic,
            table,
            query,
            key,
            migrations.into_boxed_slice(),
            seed.into_boxed_slice(),
        )
        .with_partition_by(self.partition_by.clone())
        // Carried onto the runtime collection so the read path can render a
        // `bool` as `true` rather than the `1` it is stored as.
        .with_fields(self.fields.clone()))
    }

    /// The `ORDER BY` clause, re-emitted from a *parse* rather than passed
    /// through. `parse_order_by` accepts only a comma-separated list of plain
    /// columns with optional directions, so anything it cannot prove — an
    /// expression, a function call, `NULLS LAST` — yields an empty parse and is
    /// refused here rather than reaching SQL.
    ///
    /// Ascending emits bare (`ORDER BY id`, not `ORDER BY id ASC`): it is the SQL
    /// default, it is what an author writes, and it is what the built-in default
    /// uses — which is what lets a declaration reproduce it byte for byte.
    fn order_by_sql(&self, topic: &str, key: &str) -> Result<String, ForgeSchemaError> {
        let Some(raw) = self.order_by.as_deref() else {
            return Ok(key.to_string());
        };
        // `parse_order_by` reads a whole query, so give it one to chew on.
        let parsed = parse_order_by(&format!("SELECT 1 ORDER BY {raw}"));
        if parsed.is_empty() {
            return Err(ForgeSchemaError::InvalidOrderBy {
                topic: topic.to_string(),
                order_by: raw.to_string(),
            });
        }
        // Every parsed column must be one this collection actually has, or the
        // query would fail at materialisation — a boot error is better than a
        // dead topic.
        let mut terms = Vec::with_capacity(parsed.len());
        for term in parsed.iter() {
            if term.column != key && !self.fields.contains_key(&term.column) {
                return Err(ForgeSchemaError::InvalidOrderBy {
                    topic: topic.to_string(),
                    order_by: format!("{raw} (no such field '{}')", term.column),
                });
            }
            terms.push(match term.dir {
                SortDir::Asc => term.column.clone(),
                SortDir::Desc => format!("{} DESC", term.column),
            });
        }
        Ok(terms.join(", "))
    }

    /// One seed row as a bound `INSERT`. Columns come from the row's own keys in
    /// `BTreeMap` order, so the statement is stable; every one must be a declared
    /// field, and every value must be a scalar.
    fn lower_seed_row(
        &self,
        topic: &str,
        table: &str,
        row: &BTreeMap<String, serde_json::Value>,
    ) -> Result<SeedRow, ForgeSchemaError> {
        let mut columns = Vec::with_capacity(row.len());
        let mut params = Vec::with_capacity(row.len());
        for (column, value) in row {
            let Some(spec) = self.fields.get(column) else {
                return Err(ForgeSchemaError::UnknownSeedColumn {
                    topic: topic.to_string(),
                    column: column.clone(),
                });
            };
            columns.push(column.as_str());
            params.push(Self::seed_value(topic, column, *spec, value)?);
        }
        if columns.is_empty() {
            return Err(ForgeSchemaError::UnknownSeedColumn {
                topic: topic.to_string(),
                column: "<empty seed row>".to_string(),
            });
        }
        let placeholders = (1..=columns.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(SeedRow {
            sql: format!(
                "INSERT INTO {table} ({}) VALUES ({placeholders})",
                columns.join(", ")
            ),
            params: params.into_boxed_slice(),
        })
    }

    /// Lower one seed value to a bound parameter, **checked against the column's
    /// declared type**.
    ///
    /// The check is the point. A seed is the one place an author writes data in
    /// the same file where they declared its shape, so a mismatch there is a
    /// typo they can see and fix — `{ done: "yes" }` against a `bool` is worth a
    /// boot error naming the column, not a silent `TEXT` in a `BOOLEAN` column
    /// that reads back as a string forever after. (SQLite would happily store
    /// it: affinity converts what it can and keeps the rest as-is.)
    ///
    /// Nested shapes are refused for the same reason `append` refuses them: a
    /// collection is a table of scalars.
    fn seed_value(
        topic: &str,
        column: &str,
        spec: FieldSpec,
        value: &serde_json::Value,
    ) -> Result<SqlValue, ForgeSchemaError> {
        use serde_json::Value;

        let mismatch = || ForgeSchemaError::UnsupportedSeedValue {
            topic: topic.to_string(),
            column: column.to_string(),
        };

        if value.is_null() {
            // A null in a required column would fail the `NOT NULL` at insert
            // time, on boot, with SQLite's wording instead of ours.
            return if spec.nullable { Ok(SqlValue::Null) } else { Err(mismatch()) };
        }

        match (spec.ty, value) {
            (FieldType::Text, Value::String(s)) => Ok(SqlValue::Text(s.clone())),
            (FieldType::Bool, Value::Bool(b)) => Ok(SqlValue::Integer(i64::from(*b))),
            // A timestamp is epoch milliseconds; there is no date parsing here,
            // and a string that looks like a date is refused rather than
            // guessed at across formats and time zones.
            (FieldType::Int | FieldType::Timestamp, Value::Number(n)) => {
                n.as_i64().map(SqlValue::Integer).ok_or_else(mismatch)
            }
            // Reals accept whole numbers too — `1` for a ratio is not a typo,
            // and JSON has no way to write `1.0` distinctly.
            (FieldType::Real, Value::Number(n)) => n.as_f64().map(SqlValue::Real).ok_or_else(mismatch),
            _ => Err(mismatch()),
        }
    }

    fn require_identifier(
        topic: &str,
        field: &'static str,
        value: &str,
    ) -> Result<(), ForgeSchemaError> {
        if crate::forge::write::is_safe_identifier(value) {
            Ok(())
        } else {
            Err(ForgeSchemaError::InvalidIdentifier {
                topic: topic.to_string(),
                field,
                value: value.to_string(),
            })
        }
    }
}

impl ForgeSchema {
    /// Build a schema from an app's declarations — the `forge` block of
    /// `albedo.config.ts`.
    ///
    /// Lowers each declaration to a [`ForgeCollection`] and hands the set to
    /// [`ForgeSchema::build`], so a declared schema passes through exactly the
    /// gates the built-in default does: safe identifiers, and no two collections
    /// sharing a wire slot.
    ///
    /// # Errors
    /// Any [`ForgeSchemaError`] from lowering a declaration or from `build`.
    pub fn from_declarations(
        declarations: &BTreeMap<String, CollectionDecl>,
    ) -> Result<Self, ForgeSchemaError> {
        let mut collections = Vec::with_capacity(declarations.len());
        for (topic, decl) in declarations {
            collections.push(decl.lower(topic)?);
        }
        Self::build(collections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(fields: &[(&str, FieldType)]) -> CollectionDecl {
        CollectionDecl {
            fields: fields
                .iter()
                .map(|(name, ty)| ((*name).to_string(), FieldSpec::new(*ty)))
                .collect(),
            ..CollectionDecl::default()
        }
    }

    #[test]
    fn a_declared_partition_column_is_carried_onto_the_collection() {
        let mut d = decl(&[("room", FieldType::Text), ("body", FieldType::Text)]);
        d.partition_by = Some("room".to_string());
        let collection = d.lower("messages").expect("lowers");
        assert_eq!(collection.partition_by.as_deref(), Some("room"));

        // P2 · the read is now per-partition, and the key is BOUND, never
        // interpolated — the column name comes from the schema, the value from
        // a parameter.
        assert_eq!(
            collection.query,
            "SELECT id, body, room FROM messages WHERE room = ?1 ORDER BY id"
        );

        // …and the index that makes it worth having ships in the same
        // migration set. A `WHERE` without `(partition, sort)` indexed is a full
        // scan plus a sort on every read: identical at one partition, unusable
        // at a thousand. These two must never land apart.
        let index = collection
            .migrations
            .iter()
            .find(|sql| sql.contains("CREATE INDEX"))
            .expect("a partitioned collection emits its composite index");
        assert_eq!(
            index,
            "CREATE INDEX IF NOT EXISTS idx_messages_room_id ON messages(room, id)"
        );
    }

    /// A DESC ordering has to be mirrored in the index, or the index serves the
    /// filter but the sort still happens in memory.
    #[test]
    fn a_descending_order_is_carried_into_the_partition_index() {
        let mut d = decl(&[("room", FieldType::Text), ("body", FieldType::Text)]);
        d.partition_by = Some("room".to_string());
        d.order_by = Some("id desc".to_string());
        let collection = d.lower("messages").expect("lowers");
        let index = collection
            .migrations
            .iter()
            .find(|sql| sql.contains("CREATE INDEX"))
            .expect("index");
        assert!(index.contains("(room, id DESC)"), "{index}");
    }

    /// An unpartitioned collection is untouched by all of this — the property
    /// that keeps P2 invisible to every app that exists today.
    #[test]
    fn an_unpartitioned_collection_gets_no_where_and_no_index() {
        let collection = decl(&[("author", FieldType::Text)])
            .lower("guestbook")
            .expect("lowers");
        assert_eq!(
            collection.query,
            "SELECT id, author FROM guestbook ORDER BY id"
        );
        assert!(
            !collection.migrations.iter().any(|sql| sql.contains("CREATE INDEX")),
            "no partition, no index: {:?}",
            collection.migrations
        );
    }

    #[test]
    fn a_partition_column_that_is_not_a_declared_field_is_refused() {
        let mut d = decl(&[("room", FieldType::Text)]);
        d.partition_by = Some("channel".to_string());
        let err = d.lower("messages").expect_err("refused");
        let message = err.to_string();
        assert!(message.contains("channel"), "{message}");
        assert!(
            message.contains("room"),
            "the message must list what IS declared — the cause is almost always a typo: {message}"
        );
    }

    /// Partitioning by row identity gives one row per partition: useless, and
    /// silently so. The key is not a declared field, so the same check catches it.
    #[test]
    fn partitioning_by_the_key_column_is_refused() {
        let mut d = decl(&[("room", FieldType::Text)]);
        d.partition_by = Some("id".to_string());
        assert!(d.lower("messages").is_err());
    }

    #[test]
    fn an_unpartitioned_collection_lowers_exactly_as_before() {
        let collection = decl(&[("author", FieldType::Text)]).lower("guestbook").expect("lowers");
        assert_eq!(collection.partition_by, None);
    }

    fn seed_row(pairs: &[(&str, serde_json::Value)]) -> BTreeMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// **The conformance property.** A declaration language that cannot express
    /// the schema it replaces is too weak — so the built-in guestbook must come
    /// out of this path byte-identical to the hand-written constant.
    #[test]
    fn guestbook_declaration_reproduces_the_builtin_default() {
        let declared = CollectionDecl {
            fields: [
                ("author".to_string(), FieldSpec::new(FieldType::Text)),
                ("message".to_string(), FieldSpec::new(FieldType::Text)),
            ]
            .into_iter()
            .collect(),
            seed: vec![
                seed_row(&[
                    ("author", serde_json::json!("ada")),
                    ("message", serde_json::json!("first light")),
                ]),
                seed_row(&[
                    ("author", serde_json::json!("alan")),
                    ("message", serde_json::json!("the machine stirs")),
                ]),
            ],
            ..CollectionDecl::default()
        }
        .lower("guestbook")
        .expect("the guestbook declaration must lower");

        let builtin = ForgeSchema::guestbook_default();
        let builtin = builtin
            .slot_for_topic("guestbook")
            .expect("the default has a guestbook");

        assert_eq!(declared.table, builtin.table);
        assert_eq!(declared.key_column, builtin.key_column);
        assert_eq!(declared.query, builtin.query, "materialising query");
        assert_eq!(*declared.migrations, *builtin.migrations, "DDL");
        assert_eq!(declared.slot_id, builtin.slot_id, "wire identity");
        assert_eq!(*declared.sort, *builtin.sort, "derived structured sort");
        assert_eq!(declared.seed.len(), builtin.seed.len());
        for (got, want) in declared.seed.iter().zip(builtin.seed.iter()) {
            assert_eq!(got.sql, want.sql);
            assert_eq!(got.params, want.params);
        }
    }

    #[test]
    fn table_and_key_default_to_the_topic_and_id() {
        let lowered = decl(&[("body", FieldType::Text)]).lower("notes").unwrap();
        assert_eq!(lowered.table, "notes");
        assert_eq!(lowered.key_column, "id");
        assert_eq!(lowered.query, "SELECT id, body FROM notes ORDER BY id");
    }

    #[test]
    fn a_custom_table_and_key_are_honoured() {
        let lowered = CollectionDecl {
            table: Some("note_rows".to_string()),
            key: Some("note_id".to_string()),
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes")
        .unwrap();

        assert_eq!(lowered.table, "note_rows");
        assert_eq!(lowered.key_column, "note_id");
        assert_eq!(
            lowered.query,
            "SELECT note_id, body FROM note_rows ORDER BY note_id"
        );
        assert_eq!(
            lowered.migrations[0],
            "CREATE TABLE IF NOT EXISTS note_rows (note_id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT NOT NULL)"
        );
    }

    #[test]
    fn every_scalar_type_lowers_to_its_sql_column() {
        let lowered = decl(&[
            ("amount", FieldType::Real),
            ("label", FieldType::Text),
            ("quantity", FieldType::Int),
        ])
        .lower("orders")
        .unwrap();

        assert_eq!(
            lowered.migrations[0],
            "CREATE TABLE IF NOT EXISTS orders (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             amount REAL NOT NULL, label TEXT NOT NULL, quantity INTEGER NOT NULL)"
        );
    }

    /// Reverse-chron — the ordering the positioned-insert path exists for. It
    /// must survive declaration *and* produce the structured sort C1 derives.
    #[test]
    fn a_descending_order_by_lowers_and_derives_its_sort() {
        let lowered = CollectionDecl {
            order_by: Some("id desc".to_string()),
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("feed")
        .unwrap();

        assert_eq!(lowered.query, "SELECT id, body FROM feed ORDER BY id DESC");
        assert_eq!(lowered.sort.len(), 1);
        assert_eq!(lowered.sort[0].column, "id");
        assert_eq!(lowered.sort[0].dir, SortDir::Desc);
    }

    #[test]
    fn a_multi_term_order_by_round_trips_through_the_parse() {
        let lowered = CollectionDecl {
            order_by: Some("rank desc, id".to_string()),
            ..decl(&[("rank", FieldType::Int)])
        }
        .lower("board")
        .unwrap();
        assert_eq!(
            lowered.query,
            "SELECT id, rank FROM board ORDER BY rank DESC, id"
        );
    }

    #[test]
    fn an_order_by_outside_the_plain_grammar_is_refused() {
        // Not passed through — `parse_order_by` cannot prove it, so it never
        // reaches SQL.
        for hostile in ["lower(name)", "id DESC NULLS LAST", "1; DROP TABLE t--"] {
            let result = CollectionDecl {
                order_by: Some(hostile.to_string()),
                ..decl(&[("body", FieldType::Text)])
            }
            .lower("notes");
            assert!(result.is_err(), "must refuse order_by {hostile:?}");
        }
    }

    #[test]
    fn an_order_by_naming_an_undeclared_field_is_refused() {
        let result = CollectionDecl {
            order_by: Some("created_at desc".to_string()),
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes");
        assert!(result.is_err(), "ordering by a column the SELECT lacks");
    }

    #[test]
    fn a_collection_with_no_fields_is_refused() {
        assert!(decl(&[]).lower("empty").is_err());
    }

    #[test]
    fn redeclaring_the_key_as_a_field_is_refused() {
        // It would be emitted twice in the DDL.
        assert!(decl(&[("id", FieldType::Int)]).lower("notes").is_err());
    }

    #[test]
    fn hostile_identifiers_are_refused_everywhere_they_could_reach_sql() {
        let hostile = "body); DROP TABLE notes;--";
        assert!(decl(&[(hostile, FieldType::Text)]).lower("notes").is_err());
        assert!(CollectionDecl {
            table: Some(hostile.to_string()),
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes")
        .is_err());
        assert!(CollectionDecl {
            key: Some(hostile.to_string()),
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes")
        .is_err());
    }

    #[test]
    fn seed_values_bind_and_are_never_inlined() {
        let hostile = "'); DROP TABLE notes;--";
        let lowered = CollectionDecl {
            seed: vec![seed_row(&[("body", serde_json::json!(hostile))])],
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes")
        .unwrap();

        assert_eq!(lowered.seed[0].sql, "INSERT INTO notes (body) VALUES (?1)");
        assert!(!lowered.seed[0].sql.contains("DROP"));
        assert_eq!(*lowered.seed[0].params, [SqlValue::Text(hostile.to_string())]);
    }

    #[test]
    fn seed_scalars_lower_to_their_sql_shapes() {
        let lowered = CollectionDecl {
            seed: vec![seed_row(&[
                ("amount", serde_json::json!(1.5)),
                ("done", serde_json::json!(true)),
                ("quantity", serde_json::json!(3)),
                ("placed_at", serde_json::json!(1_754_180_000_000_i64)),
            ])],
            ..decl(&[
                ("amount", FieldType::Real),
                ("done", FieldType::Bool),
                ("quantity", FieldType::Int),
                ("placed_at", FieldType::Timestamp),
            ])
        }
        .lower("orders")
        .unwrap();

        // A bool binds as 0/1 and a timestamp as epoch milliseconds — both
        // integers in the substrate. The declared type is what tells them apart
        // again on the way out; see `column_value_to_json`.
        assert_eq!(
            *lowered.seed[0].params,
            [
                SqlValue::Real(1.5),
                SqlValue::Integer(1),
                SqlValue::Integer(1_754_180_000_000),
                SqlValue::Integer(3),
            ],
            "params bind in the record's BTreeMap order: amount, done, placed_at, quantity"
        );
    }

    /// Previously this was accepted — `seed_value` read the JSON shape and never
    /// consulted the declaration, so `true` in an `int` column silently became
    /// `1`. It is a typo with a plausible-looking result, which is the worst
    /// kind, and the author is looking right at the declaration when they make
    /// it.
    #[test]
    fn a_seed_value_that_contradicts_its_declared_type_is_refused() {
        for (ty, bad) in [
            (FieldType::Int, serde_json::json!(true)),
            (FieldType::Bool, serde_json::json!(1)),
            (FieldType::Bool, serde_json::json!("yes")),
            (FieldType::Text, serde_json::json!(3)),
            // A timestamp is epoch millis; a date string is not parsed, guessed
            // at, or accepted.
            (FieldType::Timestamp, serde_json::json!("2026-08-03T00:00:00Z")),
        ] {
            let result = CollectionDecl {
                seed: vec![seed_row(&[("field", bad.clone())])],
                ..decl(&[("field", ty)])
            }
            .lower("orders");
            assert!(
                matches!(result, Err(ForgeSchemaError::UnsupportedSeedValue { .. })),
                "{ty:?} must refuse {bad}"
            );
        }
    }

    /// Null is a declaration-level decision, not a per-value one.
    #[test]
    fn null_seeds_only_where_the_field_is_declared_nullable() {
        let nullable = CollectionDecl {
            seed: vec![seed_row(&[("nickname", serde_json::json!(null))])],
            fields: [("nickname".to_string(), FieldSpec::nullable(FieldType::Text))]
                .into_iter()
                .collect(),
            ..CollectionDecl::default()
        }
        .lower("people")
        .expect("a nullable field takes null");
        assert_eq!(*nullable.seed[0].params, [SqlValue::Null]);

        let required = CollectionDecl {
            seed: vec![seed_row(&[("nickname", serde_json::json!(null))])],
            ..decl(&[("nickname", FieldType::Text)])
        }
        .lower("people");
        assert!(
            matches!(required, Err(ForgeSchemaError::UnsupportedSeedValue { .. })),
            "a required field must refuse null here rather than fail NOT NULL at boot"
        );
    }

    /// The `forge` block is JSON by the time it reaches here, so the declaration
    /// language *is* this string grammar. `?` is the whole of the nullability
    /// syntax.
    #[test]
    fn a_field_spec_round_trips_through_its_declaration_string() {
        for (written, expected) in [
            ("text", FieldSpec::new(FieldType::Text)),
            ("int", FieldSpec::new(FieldType::Int)),
            ("real", FieldSpec::new(FieldType::Real)),
            ("bool", FieldSpec::new(FieldType::Bool)),
            ("timestamp", FieldSpec::new(FieldType::Timestamp)),
            ("text?", FieldSpec::nullable(FieldType::Text)),
            ("bool?", FieldSpec::nullable(FieldType::Bool)),
            ("timestamp?", FieldSpec::nullable(FieldType::Timestamp)),
        ] {
            let parsed: FieldSpec = serde_json::from_value(serde_json::json!(written))
                .unwrap_or_else(|err| panic!("'{written}' must parse: {err}"));
            assert_eq!(parsed, expected, "parsing '{written}'");
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                serde_json::json!(written),
                "'{written}' must serialize back to itself"
            );
        }
    }

    /// A typo in a type name is a config error the author can fix in seconds if
    /// the message says what was expected — and an unfixable mystery if it just
    /// says "invalid".
    #[test]
    fn an_unknown_field_type_names_what_was_expected() {
        let err = serde_json::from_value::<FieldSpec>(serde_json::json!("boolean"))
            .expect_err("'boolean' is not the spelling");
        let message = err.to_string();
        assert!(message.contains("boolean"), "quotes the input: {message}");
        assert!(message.contains("bool"), "lists the real names: {message}");
        assert!(message.contains("timestamp"), "{message}");
        assert!(message.contains('?'), "mentions the nullable suffix: {message}");
    }

    /// The conformance property in this module's docs, extended: a declaration
    /// with no nullable fields must still emit exactly the DDL it always did.
    #[test]
    fn required_fields_keep_the_not_null_they_always_had() {
        let ddl = &decl(&[("author", FieldType::Text)])
            .lower("guestbook")
            .unwrap()
            .migrations[0];
        assert_eq!(
            **ddl,
            *"CREATE TABLE IF NOT EXISTS guestbook (id INTEGER PRIMARY KEY AUTOINCREMENT, \
              author TEXT NOT NULL)"
        );
    }

    #[test]
    fn a_seed_column_that_is_not_a_declared_field_is_refused() {
        let result = CollectionDecl {
            seed: vec![seed_row(&[("nope", serde_json::json!("x"))])],
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes");
        assert!(result.is_err());
    }

    #[test]
    fn a_nested_seed_value_is_refused_rather_than_guessed_at() {
        let result = CollectionDecl {
            seed: vec![seed_row(&[("body", serde_json::json!({ "a": 1 }))])],
            ..decl(&[("body", FieldType::Text)])
        }
        .lower("notes");
        assert!(result.is_err());
    }

    // ── Whole-schema assembly ────────────────────────────────────────

    #[test]
    fn several_declared_collections_build_one_schema() {
        let declarations: BTreeMap<String, CollectionDecl> = [
            ("guestbook".to_string(), decl(&[("author", FieldType::Text)])),
            ("todos".to_string(), decl(&[("title", FieldType::Text)])),
            ("chat".to_string(), decl(&[("body", FieldType::Text)])),
        ]
        .into_iter()
        .collect();

        let schema = ForgeSchema::from_declarations(&declarations).expect("three collections");
        assert_eq!(schema.len(), 3);
        for topic in ["guestbook", "todos", "chat"] {
            assert!(
                schema.slot_for_topic(topic).is_some(),
                "{topic} must resolve"
            );
        }
    }

    #[test]
    fn a_declaration_error_fails_the_whole_schema() {
        // Boot-time and total: one bad collection must not leave a half-built
        // registry that silently drops a topic.
        let declarations: BTreeMap<String, CollectionDecl> = [
            ("ok".to_string(), decl(&[("body", FieldType::Text)])),
            ("bad".to_string(), decl(&[])),
        ]
        .into_iter()
        .collect();
        assert!(ForgeSchema::from_declarations(&declarations).is_err());
    }

    #[test]
    fn an_empty_forge_block_builds_an_empty_schema() {
        let schema = ForgeSchema::from_declarations(&BTreeMap::new()).unwrap();
        assert!(schema.is_empty());
    }
}
