//! Schema evolution — apply what is additive, refuse the rest loudly.
//!
//! FORGE emits `CREATE TABLE IF NOT EXISTS` and `CREATE INDEX IF NOT EXISTS` and
//! **nothing else**. Against a fresh database that is a complete migration
//! system. Against an existing one it is a **no-op that looks like success**: an
//! author adds a field to the `forge` block of `albedo.config.ts`, restarts, and
//! the table is unchanged — reads come back without the column, writes naming it
//! fail, and nothing anywhere said why. On the path the pre-cohort gate actually
//! measures (*edit the schema, reload*) that reads as data loss, and it is the
//! kind of thing that ends a weekend.
//!
//! [`evolve_schema`] closes that hole in two moves. It compares the schema about
//! to be served against the tables that exist, and then:
//!
//! - **a new nullable column is added**, with `ALTER TABLE … ADD COLUMN`, so the
//!   headline edit — *append a field, reload* — simply works;
//! - **everything else refuses to boot with a message naming the field**. Drops,
//!   renames, type changes and nullability flips stay refusals permanently,
//!   because there is no answer a compiler can pick on the author's behalf. A
//!   loud refusal was already a complete fix for the failure mode — silence is
//!   the defect, not the absence of `ALTER`.
//!
//! A *required* new column is in the second group and always will be: SQLite
//! cannot add a `NOT NULL` column without a default, and inventing values for
//! rows that predate the field is exactly the decision a compiler must not make.
//! That refusal names the one-character fix (`"text"` → `"text?"`).
//!
//! ## Nothing is applied until everything is planned
//!
//! Every collection is diffed before the first `ALTER` runs, and one refusal
//! anywhere cancels every addition everywhere — including additions in other,
//! perfectly evolvable collections. A half-migrated database is a worse thing to
//! hand back than an unmigrated one: the author reverts the edit that was
//! refused, reboots, and now the *applied* half is drift in the other direction.
//! The database only ever moves between shapes the declaration described.
//!
//! ## Indexes are already additive, and deliberately unchecked
//!
//! `CREATE INDEX IF NOT EXISTS` applies to an existing table, so adding
//! `partition_by` to a live collection already works end to end. A *stale* index
//! left behind by a removed partition costs a little write throughput and
//! nothing else. Neither is drift, so neither is reported: the check is about
//! shapes a read can silently disagree with.
//!
//! ## Where the "before" comes from
//!
//! The live tables, via `PRAGMA table_info` — not a recorded copy of the last
//! schema. A metadata table would only describe databases created after it
//! shipped, which is precisely the wrong set: every `forge.db` that exists today
//! predates it, and those are the ones about to drift. Introspection also stays
//! honest about a table edited outside ALBEDO.

use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

use crate::forge::skeleton::{ForgeCollection, ForgeSchema};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{SqlValue, SubstrateError};
use crate::forge::write::is_safe_identifier;

/// One column, as either the DDL declares it or the database has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnShape {
    /// The declared SQL type, upper-cased. SQLite stores a column's declared
    /// type verbatim, so `TEXT` round-trips through `PRAGMA table_info` exactly.
    pub sql_type: String,
    /// Whether the column carries `NOT NULL`.
    pub not_null: bool,
}

/// A table's shape: its primary key and its other columns.
///
/// Produced two ways that must agree — parsed from the `CREATE TABLE` a
/// collection *would* run, and read back from the table that *exists*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableShape {
    /// Physical table name.
    pub table: String,
    /// The primary-key column. `None` for a live table that has none, which our
    /// own DDL can never produce.
    pub key: Option<String>,
    /// Non-key columns by name. A `BTreeMap` so a drift report lists fields in
    /// the same order on every machine.
    pub columns: BTreeMap<String, ColumnShape>,
}

/// One difference between the declaration and the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Declared, but the table does not have it. The common case: someone added
    /// a field to their `forge` block — and the only one FORGE can apply, which
    /// is why this variant carries the nullability the decision turns on.
    FieldAdded {
        field: String,
        sql_type: String,
        /// `true` when the column was declared with a trailing `?`. Only a
        /// nullable column can join a table that already has rows.
        nullable: bool,
    },
    /// In the table, but no longer declared.
    FieldRemoved { field: String, sql_type: String },
    /// Same name, different type.
    TypeChanged {
        field: String,
        from: String,
        to: String,
    },
    /// Same name and type, different nullability.
    NullabilityChanged { field: String, now_required: bool },
    /// The row-identity column is not the one the table was built with.
    KeyChanged { from: String, to: String },
}

impl Change {
    /// Whether FORGE can apply this change to a table that already holds rows.
    ///
    /// Exactly one kind can be: a **new nullable column**. Every existing row
    /// gets `null` for it, which is precisely what the declaration says the
    /// column may hold, so no value is invented on the author's behalf. That
    /// last clause is the whole test, and nothing else passes it.
    #[must_use]
    pub const fn is_additive(&self) -> bool {
        matches!(self, Self::FieldAdded { nullable: true, .. })
    }
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldAdded {
                field,
                sql_type,
                nullable: true,
            } => write!(
                f,
                "field '{field}' ({sql_type}) is declared, but the table does not have it"
            ),
            // The one refusal with a one-character fix, so it says the fix
            // rather than describing the constraint. `sql_type` is the SQL name
            // (`INTEGER`); the author writes the declaration name (`int?`), and
            // quoting the SQL one here would send them looking for a spelling
            // that does not exist in their config.
            Self::FieldAdded {
                field,
                sql_type,
                nullable: false,
            } => write!(
                f,
                "field '{field}' ({sql_type}) is declared required, but the table already exists — \
                 a required column cannot be added to it; declare the field nullable \
                 (a trailing '?', as in \"text?\") and existing rows will read it as null"
            ),
            Self::FieldRemoved { field, sql_type } => write!(
                f,
                "field '{field}' ({sql_type}) is in the table, but is no longer declared"
            ),
            Self::TypeChanged { field, from, to } => {
                write!(f, "field '{field}' is {from} in the table, declared as {to}")
            }
            Self::NullabilityChanged {
                field,
                now_required: true,
            } => write!(f, "field '{field}' is nullable in the table, declared required"),
            Self::NullabilityChanged {
                field,
                now_required: false,
            } => write!(f, "field '{field}' is required in the table, declared nullable"),
            Self::KeyChanged { from, to } => {
                write!(f, "the row key is '{from}' in the table, declared as '{to}'")
            }
        }
    }
}

/// Every difference found for one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionDrift {
    /// The `useSharedSlot` topic — what the author names in their code.
    pub topic: String,
    /// The physical table — what they would open in a SQL shell.
    pub table: String,
    /// At least one; a `CollectionDrift` is never constructed empty.
    pub changes: Vec<Change>,
}

/// Every drifted collection in a schema.
///
/// Its `Display` is the whole deliverable of this module — the message an
/// author reads instead of watching their edit do nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDrift {
    /// At least one; a `SchemaDrift` is never constructed empty.
    pub collections: Vec<CollectionDrift>,
}

impl fmt::Display for SchemaDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "the declared schema no longer matches the database (forge.db)."
        )?;
        for collection in &self.collections {
            writeln!(f)?;
            writeln!(
                f,
                "  collection '{}' (table '{}')",
                collection.topic, collection.table
            )?;
            for change in &collection.changes {
                writeln!(f, "    · {change}")?;
            }
        }
        writeln!(f)?;
        // Say plainly that nothing was lost. An author who sees a startup
        // failure mentioning their database assumes the worst, and the whole
        // point of this check is that it fires *before* anything is touched.
        //
        // "applied NOTHING" is stated flatly because it is the one thing an
        // author cannot check by reading the list: some of the lines above may
        // well be additive, and so may changes in a collection this report does
        // not mention at all. Every one of them is held back.
        writeln!(
            f,
            "FORGE adds new nullable columns by itself. It cannot apply everything listed\n\
             above, so it applied NOTHING — here or anywhere else in this schema; a\n\
             half-migrated database is worse to hand back than an unmigrated one. Your rows\n\
             are intact and untouched: this is a refusal to serve a shape that disagrees\n\
             with them, not a loss."
        )?;
        writeln!(f)?;
        write!(
            f,
            "Either revert the `forge` block in albedo.config.ts, or delete forge.db to\n\
             rebuild from the declaration (which discards the rows it holds)."
        )
    }
}

/// One column [`evolve_schema`] added to a live table.
///
/// Returned rather than merely logged so the boot path can say what it changed
/// in the author's database. Altering someone's storage is not a thing to do
/// quietly, even when the alteration is the correct one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Addition {
    /// The `useSharedSlot` topic — what the author names in their code.
    pub topic: String,
    /// The physical table the column joined.
    pub table: String,
    /// The new column.
    pub field: String,
    /// Its declared SQL type.
    pub sql_type: String,
}

impl Addition {
    /// The statement that applies it.
    ///
    /// No `NOT NULL`, by construction — [`Change::is_additive`] admits only
    /// nullable columns, and this is the reason it does.
    ///
    /// Every interpolated fragment is an identifier the strict parser in
    /// [`parse_create_table`] already validated, out of DDL this crate emitted.
    fn ddl(&self) -> String {
        format!(
            "ALTER TABLE {} ADD COLUMN {} {}",
            self.table, self.field, self.sql_type
        )
    }
}

impl fmt::Display for Addition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "added column '{}' ({}) to collection '{}' (table '{}')",
            self.field, self.sql_type, self.topic, self.table
        )
    }
}

/// Why [`evolve_schema`] could not certify the database.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The introspection read itself failed.
    #[error(transparent)]
    Substrate(#[from] SubstrateError),
    /// The database and the declaration disagree.
    #[error("{0}")]
    Drift(SchemaDrift),
}

/// Reconcile every collection's declared shape with the table that exists:
/// apply the additive differences, refuse the rest.
///
/// Call this **before** [`bootstrap_schema`](crate::forge::skeleton::bootstrap_schema),
/// which is where the silent no-op would otherwise happen — and, less obviously,
/// because a `partition_by` added in the same edit as its column needs the
/// `ALTER` to have run before `CREATE INDEX` names it.
///
/// Absent tables are not drift — `CREATE TABLE IF NOT EXISTS` is about to create
/// them correctly, which is the first-run path and by far the common one.
///
/// Returns what it applied, in schema order; empty when the database already
/// agreed, which is every boot but the one after an edit.
///
/// # Errors
/// [`VerifyError::Drift`] when any collection differs in a way FORGE cannot
/// apply — in which case **nothing** was applied, for any collection;
/// [`VerifyError::Substrate`] if the introspection read or an `ALTER` fails.
pub async fn evolve_schema(
    substrate: &dyn DataSubstrate,
    schema: &ForgeSchema,
) -> Result<Vec<Addition>, VerifyError> {
    let mut additions = Vec::new();
    let mut drifted = Vec::new();

    // Phase 1 — plan. Reads only; the database is not touched until the whole
    // schema has been accounted for.
    for collection in schema.collections() {
        // A collection whose DDL this module cannot prove it understands is
        // skipped rather than guessed at — the same discipline `parse_order_by`
        // uses next door, and for the same reason: a confident wrong answer
        // here refuses a boot that should have succeeded.
        let Some(expected) = expected_shape(collection) else {
            continue;
        };
        let Some(live) = read_table_shape(substrate, &collection.table).await? else {
            continue;
        };
        let Some(drift) = diff(collection, &expected, &live) else {
            continue;
        };

        // All or nothing per collection, and then per schema. A collection that
        // gains a column *and* renames another is not two independent edits to
        // be half-honoured; it is one edit whose meaning FORGE cannot determine.
        if drift.changes.iter().all(Change::is_additive) {
            additions.extend(drift.changes.iter().filter_map(|change| {
                let Change::FieldAdded {
                    field, sql_type, ..
                } = change
                else {
                    return None;
                };
                Some(Addition {
                    topic: collection.topic.clone(),
                    table: collection.table.clone(),
                    field: field.clone(),
                    sql_type: sql_type.clone(),
                })
            }));
        } else {
            drifted.push(drift);
        }
    }

    if !drifted.is_empty() {
        return Err(VerifyError::Drift(SchemaDrift {
            collections: drifted,
        }));
    }

    // Phase 2 — apply. Each `ALTER TABLE ADD COLUMN` is atomic in SQLite and
    // they are independent of one another, so no transaction spans them: a
    // failure part-way leaves earlier columns added, which is a state the next
    // boot plans from correctly (they are simply no longer missing).
    for addition in &additions {
        substrate.migrate(&addition.ddl()).await?;
    }

    Ok(additions)
}

/// The shape this collection's own `CREATE TABLE` would produce.
///
/// Read out of the migration string rather than off a typed field, so the check
/// compares against *the statement that actually runs*. That also makes it work
/// identically for all three collection sources — the built-in default's
/// hand-written DDL, a declared `forge` block, and inference — without any of
/// them growing a second, drift-prone description of themselves.
fn expected_shape(collection: &ForgeCollection) -> Option<TableShape> {
    collection
        .migrations
        .iter()
        .filter_map(|ddl| parse_create_table(ddl))
        .find(|shape| shape.table == collection.table)
}

/// Parse one `CREATE TABLE` in the exact grammar FORGE emits, or give up.
///
/// Deliberately strict. It accepts `CREATE TABLE [IF NOT EXISTS] <t> (<key>
/// INTEGER PRIMARY KEY AUTOINCREMENT, <col> <TYPE> [NOT NULL], …)` and nothing
/// else — no `CHECK`, no `DEFAULT`, no `REFERENCES`, no quoted identifiers.
/// Anything outside that yields `None` and the collection goes unchecked, which
/// costs a diagnosis; mis-parsing it would cost a false refusal, and a false
/// refusal on boot is worse than the silence this replaces.
fn parse_create_table(ddl: &str) -> Option<TableShape> {
    // Identifiers here are `is_safe_identifier`-clean, so they never contain
    // whitespace and collapsing runs of it is lossless. This is what lets the
    // built-in default's `\`-continued literal parse the same as a generated
    // single-line one.
    let norm = ddl.split_whitespace().collect::<Vec<_>>().join(" ");
    // `to_ascii_lowercase` is length-preserving, so offsets are interchangeable
    // between the two.
    let lower = norm.to_ascii_lowercase();

    let mut at = if lower.starts_with("create table ") {
        "create table ".len()
    } else {
        return None;
    };
    if lower[at..].starts_with("if not exists ") {
        at += "if not exists ".len();
    }

    let open = at + norm.get(at..)?.find('(')?;
    let table = norm.get(at..open)?.trim();
    if !is_safe_identifier(table) {
        return None;
    }

    let body = norm.strip_suffix(')')?.get(open + 1..)?;
    // No column definition in our grammar is parenthesised, so a paren in the
    // body means a construct this parser does not model and must not pretend to.
    if body.contains('(') || body.contains(')') {
        return None;
    }

    let mut key: Option<String> = None;
    let mut columns = BTreeMap::new();
    for part in body.split(',') {
        let tokens: Vec<&str> = part.split_whitespace().collect();
        let (name, rest) = tokens.split_first()?;
        if !is_safe_identifier(name) {
            return None;
        }
        let owned: Vec<String> = rest.iter().map(|t| t.to_ascii_uppercase()).collect();
        let upper: Vec<&str> = owned.iter().map(String::as_str).collect();
        match upper.as_slice() {
            ["INTEGER", "PRIMARY", "KEY", "AUTOINCREMENT"] => {
                // Two primary keys is not our grammar; refuse rather than pick.
                if key.replace((*name).to_string()).is_some() {
                    return None;
                }
            }
            [sql_type] | [sql_type, "NOT", "NULL"] => {
                // The type name is re-emitted verbatim into `ALTER TABLE … ADD
                // COLUMN`, so it is held to the same standard as an identifier.
                // Our own emitters only ever produce `TEXT`/`INTEGER`/`REAL`/
                // `BOOLEAN`/`TIMESTAMP`; declining anything else keeps that a
                // fact this parser enforces rather than one it assumes.
                if !is_safe_identifier(sql_type) {
                    return None;
                }
                let not_null = upper.len() == 3;
                columns.insert(
                    (*name).to_string(),
                    ColumnShape {
                        sql_type: (*sql_type).to_string(),
                        not_null,
                    },
                );
            }
            _ => return None,
        }
    }

    // Our DDL always names a key. A parse that found none read something else.
    key.as_ref()?;
    Some(TableShape {
        table: table.to_string(),
        key,
        columns,
    })
}

/// Read a table's shape back out of the database, or `None` if it does not
/// exist yet.
///
/// `PRAGMA` takes no bound parameters, so the table name is interpolated —
/// safely, because `ForgeSchema::build` identifier-validates every `table`
/// before a collection can exist. That is the same guarantee the seed path's
/// `SELECT COUNT(*) FROM {table}` probe already runs on.
async fn read_table_shape(
    substrate: &dyn DataSubstrate,
    table: &str,
) -> Result<Option<TableShape>, SubstrateError> {
    let rows = substrate
        .query(&format!("PRAGMA table_info({table})"), &[])
        .await?;
    // No rows means no such table. It also means a substrate that does not
    // interpret SQL (the recording double) reports "nothing to compare", which
    // is the right answer for one that cannot have drifted.
    if rows.rows.is_empty() {
        return Ok(None);
    }

    // Address the PRAGMA's columns by name. Their order has changed across
    // SQLite versions; their names have not.
    let column_at = |name: &str| rows.columns.iter().position(|c| c == name);
    let (Some(name_at), Some(type_at), Some(notnull_at), Some(pk_at)) = (
        column_at("name"),
        column_at("type"),
        column_at("notnull"),
        column_at("pk"),
    ) else {
        return Ok(None);
    };

    let mut key = None;
    let mut columns = BTreeMap::new();
    for row in &rows.rows {
        let Some(name) = row.get(name_at).and_then(SqlValue::as_str) else {
            return Ok(None);
        };
        let sql_type = row
            .get(type_at)
            .and_then(SqlValue::as_str)
            .unwrap_or_default()
            .to_ascii_uppercase();
        let not_null = row.get(notnull_at).and_then(SqlValue::as_i64).unwrap_or(0) != 0;
        // `pk` is the column's 1-based position within the primary key, 0 when
        // it is not part of one.
        if row.get(pk_at).and_then(SqlValue::as_i64).unwrap_or(0) != 0 {
            key = Some(name.to_string());
            continue;
        }
        columns.insert(name.to_string(), ColumnShape { sql_type, not_null });
    }

    Ok(Some(TableShape {
        table: table.to_string(),
        key,
        columns,
    }))
}

/// Compare one collection's expected shape against its live one.
fn diff(
    collection: &ForgeCollection,
    expected: &TableShape,
    live: &TableShape,
) -> Option<CollectionDrift> {
    let mut changes = Vec::new();

    // The key is compared by *name* only. SQLite reports `INTEGER PRIMARY KEY`
    // with `notnull = 0` even though it cannot hold NULL, so comparing its
    // nullability the way an ordinary column's is compared would report drift
    // against every table FORGE has ever created.
    if expected.key != live.key {
        if let Some(to) = &expected.key {
            changes.push(Change::KeyChanged {
                from: live.key.clone().unwrap_or_else(|| "(none)".to_string()),
                to: to.clone(),
            });
        }
    }

    for (field, want) in &expected.columns {
        match live.columns.get(field) {
            None => changes.push(Change::FieldAdded {
                field: field.clone(),
                sql_type: want.sql_type.clone(),
                nullable: !want.not_null,
            }),
            // One change reported per field. When the type moved, that is the
            // edit to reckon with and a nullability note underneath it is noise.
            Some(have) if have.sql_type != want.sql_type => changes.push(Change::TypeChanged {
                field: field.clone(),
                from: have.sql_type.clone(),
                to: want.sql_type.clone(),
            }),
            Some(have) if have.not_null != want.not_null => {
                changes.push(Change::NullabilityChanged {
                    field: field.clone(),
                    now_required: want.not_null,
                });
            }
            Some(_) => {}
        }
    }

    for (field, have) in &live.columns {
        if !expected.columns.contains_key(field) {
            changes.push(Change::FieldRemoved {
                field: field.clone(),
                sql_type: have.sql_type.clone(),
            });
        }
    }

    if changes.is_empty() {
        return None;
    }
    Some(CollectionDrift {
        topic: collection.topic.clone(),
        table: collection.table.clone(),
        changes,
    })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parses_the_ddl_a_declaration_emits() {
        let shape = parse_create_table(
            "CREATE TABLE IF NOT EXISTS guestbook (id INTEGER PRIMARY KEY AUTOINCREMENT, \
             author TEXT NOT NULL, message TEXT NOT NULL)",
        )
        .expect("the emitted grammar parses");

        assert_eq!(shape.table, "guestbook");
        assert_eq!(shape.key.as_deref(), Some("id"));
        assert_eq!(shape.columns.len(), 2, "the key is not a column");
        assert_eq!(shape.columns["author"].sql_type, "TEXT");
        assert!(shape.columns["author"].not_null);
    }

    /// The built-in default writes its DDL as a `\`-continued literal, so it
    /// arrives with runs of whitespace the generated form never has. Both must
    /// parse to the same shape or the default would report drift against itself.
    #[test]
    fn the_built_in_default_parses_to_the_same_shape_as_a_single_line() {
        let multiline = parse_create_table(
            "CREATE TABLE IF NOT EXISTS guestbook (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 author TEXT NOT NULL, \
                 message TEXT NOT NULL)",
        )
        .expect("parses");
        let single = parse_create_table(
            "CREATE TABLE IF NOT EXISTS guestbook (id INTEGER PRIMARY KEY AUTOINCREMENT, author TEXT NOT NULL, message TEXT NOT NULL)",
        )
        .expect("parses");
        assert_eq!(multiline, single);
    }

    #[test]
    fn a_nullable_column_is_distinguished_from_a_required_one() {
        let shape = parse_create_table(
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a TEXT NOT NULL, b TEXT)",
        )
        .expect("parses");
        assert!(shape.columns["a"].not_null);
        assert!(!shape.columns["b"].not_null);
    }

    /// An index migration must not be mistaken for a table definition — every
    /// partitioned collection ships one in the same migration list.
    #[test]
    fn an_index_migration_is_not_a_table() {
        assert!(parse_create_table("CREATE INDEX IF NOT EXISTS idx_m_room ON m(room, id)").is_none());
    }

    /// `reserve.rs` owns DDL outside this grammar. It never reaches the check,
    /// but the parser must *decline* it rather than produce a partial shape that
    /// would refuse a perfectly good boot.
    #[test]
    fn ddl_outside_the_emitted_grammar_is_declined_not_guessed() {
        for exotic in [
            "CREATE TABLE forge_resource (key TEXT PRIMARY KEY, capacity INTEGER NOT NULL CHECK (capacity >= 0))",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')))",
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, r TEXT NOT NULL REFERENCES other(key))",
            "CREATE TABLE t (\"quoted name\" TEXT NOT NULL)",
            "CREATE TABLE t (a TEXT NOT NULL)",
            // The type name is re-emitted into `ALTER TABLE … ADD COLUMN`, so
            // one that is not a plain identifier is declined at the parse.
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a TEXT-ISH NOT NULL)",
            "SELECT 1",
        ] {
            assert!(
                parse_create_table(exotic).is_none(),
                "must decline rather than half-parse: {exotic}"
            );
        }
    }
}

#[cfg(all(test, feature = "forge"))]
mod tests {
    use super::*;
    use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
    use crate::forge::skeleton::bootstrap_schema;
    use crate::forge::LibSqlSubstrate;

    /// Build a schema the way the config path does.
    fn schema_of(fields: &[(&str, FieldType)], partition_by: Option<&str>) -> ForgeSchema {
        let mut declarations = BTreeMap::new();
        declarations.insert(
            "messages".to_string(),
            CollectionDecl {
                fields: fields
                    .iter()
                    .map(|(name, ty)| ((*name).to_string(), FieldSpec::new(*ty)))
                    .collect(),
                partition_by: partition_by.map(str::to_string),
                ..CollectionDecl::default()
            },
        );
        ForgeSchema::from_declarations(&declarations).expect("schema")
    }

    /// As [`schema_of`], but with per-field nullability.
    fn spec_schema(fields: &[(&str, FieldSpec)]) -> ForgeSchema {
        spec_schema_of(&[("messages", fields, None)])
    }

    /// The general form: any number of collections, each with its own fields and
    /// optional `partition_by`. Needed because the all-or-nothing property is
    /// about what one collection's refusal does to *another* collection.
    fn spec_schema_of(collections: &[(&str, &[(&str, FieldSpec)], Option<&str>)]) -> ForgeSchema {
        let mut declarations = BTreeMap::new();
        for (topic, fields, partition_by) in collections {
            declarations.insert(
                (*topic).to_string(),
                CollectionDecl {
                    fields: fields
                        .iter()
                        .map(|(name, spec)| ((*name).to_string(), *spec))
                        .collect(),
                    partition_by: partition_by.map(str::to_string),
                    ..CollectionDecl::default()
                },
            );
        }
        ForgeSchema::from_declarations(&declarations).expect("schema")
    }

    /// Whether a live table has a column — the fact every "was it applied?"
    /// assertion actually rests on.
    async fn has_column(db: &LibSqlSubstrate, table: &str, column: &str) -> bool {
        read_table_shape(db, table)
            .await
            .expect("introspection")
            .is_some_and(|shape| shape.columns.contains_key(column))
    }

    fn drift_of(err: VerifyError) -> SchemaDrift {
        match err {
            VerifyError::Drift(drift) => drift,
            VerifyError::Substrate(e) => panic!("expected drift, got substrate error: {e}"),
        }
    }

    /// The false-positive guard, and the most important test here. A boot that
    /// changed nothing must be certified clean — including the built-in default,
    /// whose `INTEGER PRIMARY KEY` SQLite reports as nullable.
    #[tokio::test]
    async fn a_freshly_bootstrapped_schema_has_no_drift() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();

        let applied = evolve_schema(&db, &schema)
            .await
            .expect("a schema just applied cannot have drifted from itself");
        assert!(applied.is_empty(), "nothing to evolve: {applied:?}");
    }

    /// The same property for a declared, partitioned collection — the shape a
    /// real app has.
    #[tokio::test]
    async fn a_declared_partitioned_schema_has_no_drift() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = schema_of(
            &[("room", FieldType::Text), ("body", FieldType::Text)],
            Some("room"),
        );
        bootstrap_schema(&db, &schema).await.unwrap();

        let applied = evolve_schema(&db, &schema).await.expect("no drift");
        assert!(applied.is_empty(), "nothing to evolve: {applied:?}");
    }

    /// First run: nothing exists, and `CREATE TABLE IF NOT EXISTS` is about to
    /// do exactly the right thing. Refusing here would break every new project.
    #[tokio::test]
    async fn an_absent_table_is_not_drift() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();

        let applied = evolve_schema(&db, &schema)
            .await
            .expect("a database with no tables is a first run, not a conflict");
        assert!(
            applied.is_empty(),
            "a table that does not exist yet is created, not altered: {applied:?}"
        );
    }

    /// **The headline case.** The author appends a nullable field to their
    /// `forge` block and restarts. This is the edit the on-ramp advertises, it
    /// is what a stranger does in their first hour, and until now it applied as
    /// silence and then as a refusal. It has to just work.
    ///
    /// Asserted all the way down to the rows: the column exists, the rows that
    /// predate it read `null` rather than having been touched, and the value
    /// they *did* have is still there.
    #[tokio::test]
    async fn a_new_nullable_field_is_added_to_a_table_that_already_has_rows() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[("body", FieldSpec::new(FieldType::Text))]);
        bootstrap_schema(&db, &before).await.unwrap();
        db.execute("INSERT INTO messages (body) VALUES (?1)", &["hello".into()])
            .await
            .unwrap();

        let after = spec_schema(&[
            ("body", FieldSpec::new(FieldType::Text)),
            ("nickname", FieldSpec::nullable(FieldType::Text)),
        ]);
        let applied = evolve_schema(&db, &after).await.expect("additive, so applied");

        assert_eq!(
            applied,
            vec![Addition {
                topic: "messages".to_string(),
                table: "messages".to_string(),
                field: "nickname".to_string(),
                sql_type: "TEXT".to_string(),
            }]
        );

        let rows = db
            .query("SELECT body, nickname FROM messages", &[])
            .await
            .expect("the read the app is about to run must now succeed");
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0].get(0).and_then(SqlValue::as_str), Some("hello"));
        assert_eq!(
            rows.rows[0].get(1),
            Some(&SqlValue::Null),
            "a row that predates the column reads null, not a value invented for it"
        );
    }

    /// The boot after the one that migrated. An `ALTER` that leaves the table in
    /// a shape the next diff still calls drift would refuse every restart from
    /// then on — the migration has to converge, not oscillate.
    #[tokio::test]
    async fn the_boot_after_an_addition_is_clean() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[("body", FieldSpec::new(FieldType::Text))]);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = spec_schema(&[
            ("body", FieldSpec::new(FieldType::Text)),
            ("nickname", FieldSpec::nullable(FieldType::Text)),
        ]);
        assert_eq!(evolve_schema(&db, &after).await.unwrap().len(), 1);

        for _ in 0..3 {
            let again = evolve_schema(&db, &after)
                .await
                .expect("the applied shape is the declared shape");
            assert!(again.is_empty(), "applied twice: {again:?}");
            bootstrap_schema(&db, &after).await.unwrap();
        }
    }

    /// A *required* new field. SQLite cannot add a `NOT NULL` column without a
    /// default, and no value FORGE picked for the existing rows would be the
    /// author's — so this stays a refusal permanently. It is also the single
    /// most likely refusal anyone will hit, because `"text"` is shorter to type
    /// than `"text?"`, which is why the message has to carry the fix.
    #[tokio::test]
    async fn a_required_new_field_is_refused_and_names_the_one_character_fix() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = schema_of(&[("body", FieldType::Text)], None);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = schema_of(
            &[("body", FieldType::Text), ("created_at", FieldType::Int)],
            None,
        );
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(drift.collections.len(), 1);
        assert_eq!(drift.collections[0].topic, "messages");
        assert_eq!(
            drift.collections[0].changes,
            vec![Change::FieldAdded {
                field: "created_at".to_string(),
                sql_type: "INTEGER".to_string(),
                nullable: false,
            }]
        );
        assert!(
            !drift.collections[0].changes[0].is_additive(),
            "a required column is never additive"
        );
        assert!(
            !has_column(&db, "messages", "created_at").await,
            "a refused change must not have been applied anyway"
        );

        // The message is the deliverable — it must name the field, hand over the
        // fix, and say the rows are safe, or an author reads a startup crash as
        // a wipe.
        let message = drift.to_string();
        assert!(message.contains("created_at"), "names the field: {message}");
        assert!(message.contains("messages"), "names the collection: {message}");
        assert!(message.contains("text?"), "names the fix: {message}");
        assert!(message.contains("intact"), "says the data survived: {message}");
    }

    #[tokio::test]
    async fn a_removed_field_is_refused_and_named() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = schema_of(&[("body", FieldType::Text), ("nickname", FieldType::Text)], None);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = schema_of(&[("body", FieldType::Text)], None);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(
            drift.collections[0].changes,
            vec![Change::FieldRemoved {
                field: "nickname".to_string(),
                sql_type: "TEXT".to_string(),
            }]
        );
    }

    /// A type change is the one that corrupts quietly: the column keeps its
    /// name, reads keep succeeding, and the values come back in the old
    /// representation. This stays a refusal even after additive `ALTER` lands.
    #[tokio::test]
    async fn a_changed_type_is_refused_and_named() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = schema_of(&[("count", FieldType::Text)], None);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = schema_of(&[("count", FieldType::Int)], None);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(
            drift.collections[0].changes,
            vec![Change::TypeChanged {
                field: "count".to_string(),
                from: "TEXT".to_string(),
                to: "INTEGER".to_string(),
            }]
        );
    }

    /// A renamed key is a rebuild, not a migration — and it silently orphans
    /// every `data-albedo-key` on the wire.
    #[tokio::test]
    async fn a_renamed_key_column_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let mut declarations = BTreeMap::new();
        let mut fields = BTreeMap::new();
        fields.insert("body".to_string(), FieldSpec::new(FieldType::Text));
        declarations.insert(
            "messages".to_string(),
            CollectionDecl {
                fields: fields.clone(),
                ..CollectionDecl::default()
            },
        );
        let before = ForgeSchema::from_declarations(&declarations).unwrap();
        bootstrap_schema(&db, &before).await.unwrap();

        declarations.insert(
            "messages".to_string(),
            CollectionDecl {
                fields,
                key: Some("message_id".to_string()),
                ..CollectionDecl::default()
            },
        );
        let after = ForgeSchema::from_declarations(&declarations).unwrap();
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(
            drift.collections[0].changes,
            vec![Change::KeyChanged {
                from: "id".to_string(),
                to: "message_id".to_string(),
            }]
        );
    }

    /// Item 6 made this variant reachable. Until nullability could be declared,
    /// every column was `NOT NULL` and `NullabilityChanged` was unconstructible
    /// — the check was written for a type system that did not exist yet.
    ///
    /// Tightening a nullable column to required is the dangerous direction: the
    /// rows already holding `null` do not satisfy the new declaration, and no
    /// `ALTER` can invent values for them.
    #[tokio::test]
    async fn making_a_nullable_field_required_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[("nickname", FieldSpec::nullable(FieldType::Text))]);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = spec_schema(&[("nickname", FieldSpec::new(FieldType::Text))]);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));
        assert_eq!(
            drift.collections[0].changes,
            vec![Change::NullabilityChanged {
                field: "nickname".to_string(),
                now_required: true,
            }]
        );
        assert!(
            drift.to_string().contains("nickname"),
            "{}",
            drift.to_string()
        );
    }

    /// And the loosening direction, which is equally undetectable at runtime:
    /// the column keeps working, it just silently keeps rejecting the `null` the
    /// declaration now promises is allowed.
    #[tokio::test]
    async fn making_a_required_field_nullable_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[("nickname", FieldSpec::new(FieldType::Text))]);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = spec_schema(&[("nickname", FieldSpec::nullable(FieldType::Text))]);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));
        assert_eq!(
            drift.collections[0].changes,
            vec![Change::NullabilityChanged {
                field: "nickname".to_string(),
                now_required: false,
            }]
        );
    }

    /// `bool` and `int` are both integers in storage. Emitting the declared type
    /// name is what keeps them distinguishable here — otherwise retyping a field
    /// between them would change what the wire carries (`true` vs `1`) while the
    /// schema check saw nothing at all.
    #[tokio::test]
    async fn retyping_a_bool_to_an_int_is_caught_despite_identical_storage() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &schema_of(&[("done", FieldType::Bool)], None))
            .await
            .unwrap();

        let after = schema_of(&[("done", FieldType::Int)], None);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));
        assert_eq!(
            drift.collections[0].changes,
            vec![Change::TypeChanged {
                field: "done".to_string(),
                from: "BOOLEAN".to_string(),
                to: "INTEGER".to_string(),
            }]
        );
    }

    /// A typed schema must certify clean against the table it just created —
    /// the false-positive guard extended to the new type names.
    #[tokio::test]
    async fn a_freshly_bootstrapped_typed_schema_has_no_drift() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = spec_schema(&[
            ("done", FieldSpec::new(FieldType::Bool)),
            ("created_at", FieldSpec::new(FieldType::Timestamp)),
            ("nickname", FieldSpec::nullable(FieldType::Text)),
            ("score", FieldSpec::nullable(FieldType::Real)),
        ]);
        bootstrap_schema(&db, &schema).await.unwrap();
        evolve_schema(&db, &schema)
            .await
            .expect("a typed schema just applied cannot have drifted from itself");
    }

    /// Adding `partition_by` emits a new index and no column change.
    /// `CREATE INDEX IF NOT EXISTS` already applies to a live table, so this is
    /// a migration that genuinely works today and must not be refused — which
    /// is the whole reason index differences are left out of the diff.
    ///
    /// Asserted by **existence**, not by `EXPLAIN QUERY PLAN`. The plan is
    /// asserted where it belongs, in `skeleton::the_partitioned_read_uses_its_composite_index`,
    /// and it is the wrong instrument here: on this substrate the reader is a
    /// separate connection from the writer that runs the DDL, and its first
    /// statement after a cross-connection `CREATE INDEX` plans against the
    /// schema it already had — so the same query reports `SCAN` or the index
    /// depending only on whether some other read went first. Existence is the
    /// fact this test needs and it does not depend on planner state.
    #[tokio::test]
    async fn adding_a_partition_is_not_drift() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let fields = [("room", FieldType::Text), ("body", FieldType::Text)];
        bootstrap_schema(&db, &schema_of(&fields, None)).await.unwrap();

        let partitioned = schema_of(&fields, Some("room"));
        evolve_schema(&db, &partitioned)
            .await
            .expect("an added index is additive and already applies");

        bootstrap_schema(&db, &partitioned).await.unwrap();
        let listed = db
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages'",
                &[],
            )
            .await
            .unwrap();
        let names: Vec<String> = listed
            .rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqlValue::as_str).map(str::to_string))
            .collect();
        assert!(
            names.iter().any(|n| n == "idx_messages_room_id"),
            "the index must exist on the live table: {names:?}"
        );
    }

    /// **The all-or-nothing property, across collections.** One collection can
    /// be evolved; another refuses. Applying the first anyway is the trap: the
    /// author reverts the edit that was refused, restarts, and now the applied
    /// column is drift in the *other* direction against a config that no longer
    /// mentions it — a database moved to a shape no declaration ever described.
    #[tokio::test]
    async fn a_refusal_in_one_collection_holds_back_an_addition_in_another() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema_of(&[
            ("notes", &[("body", FieldSpec::new(FieldType::Text))], None),
            ("counters", &[("hits", FieldSpec::new(FieldType::Int))], None),
        ]);
        bootstrap_schema(&db, &before).await.unwrap();

        // `notes` gains a nullable column — additive on its own. `counters`
        // retypes a field — never applicable.
        let after = spec_schema_of(&[
            (
                "notes",
                &[
                    ("body", FieldSpec::new(FieldType::Text)),
                    ("nickname", FieldSpec::nullable(FieldType::Text)),
                ],
                None,
            ),
            ("counters", &[("hits", FieldSpec::new(FieldType::Text))], None),
        ]);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(
            drift.collections.len(),
            1,
            "only the collection that cannot be evolved is reported: {drift}"
        );
        assert_eq!(drift.collections[0].topic, "counters");
        assert!(
            !has_column(&db, "notes", "nickname").await,
            "the evolvable collection must be left exactly as it was"
        );
        // The report cannot list `notes`, so the prose has to carry the fact
        // that its column did not go through on its own.
        assert!(
            drift.to_string().contains("anywhere else"),
            "must say the hold-back is schema-wide: {drift}"
        );
    }

    /// The same property inside one collection. Adding a column *and* renaming
    /// another is one edit whose meaning FORGE cannot determine, not two
    /// independent ones to half-honour.
    #[tokio::test]
    async fn a_collection_mixing_an_addition_with_a_refusal_applies_neither() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[
            ("body", FieldSpec::new(FieldType::Text)),
            ("old", FieldSpec::new(FieldType::Text)),
        ]);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = spec_schema(&[
            ("body", FieldSpec::new(FieldType::Text)),
            ("nickname", FieldSpec::nullable(FieldType::Text)),
        ]);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        assert_eq!(
            drift.collections[0].changes,
            vec![
                Change::FieldAdded {
                    field: "nickname".to_string(),
                    sql_type: "TEXT".to_string(),
                    nullable: true,
                },
                Change::FieldRemoved {
                    field: "old".to_string(),
                    sql_type: "TEXT".to_string(),
                },
            ],
            "both halves are reported, so the author sees the whole edit"
        );
        assert!(
            !has_column(&db, "messages", "nickname").await,
            "the additive half must not be applied beside a refusal"
        );
    }

    /// A column and the `partition_by` that indexes it, added in the same edit.
    /// This is why `evolve_schema` runs *before* `bootstrap_schema` rather than
    /// after: `CREATE INDEX` names a column that only the `ALTER` puts there.
    #[tokio::test]
    async fn a_column_and_its_new_partition_index_land_in_one_boot() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = spec_schema(&[("body", FieldSpec::new(FieldType::Text))]);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = spec_schema_of(&[(
            "messages",
            &[
                ("body", FieldSpec::new(FieldType::Text)),
                ("room", FieldSpec::nullable(FieldType::Text)),
            ],
            Some("room"),
        )]);
        assert_eq!(evolve_schema(&db, &after).await.unwrap().len(), 1);
        bootstrap_schema(&db, &after)
            .await
            .expect("the index DDL must find the column the ALTER just added");

        let listed = db
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages'",
                &[],
            )
            .await
            .unwrap();
        let names: Vec<String> = listed
            .rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqlValue::as_str).map(str::to_string))
            .collect();
        assert!(
            names.iter().any(|n| n == "idx_messages_room_id"),
            "the index must exist on the live table: {names:?}"
        );
    }

    /// Every difference is reported at once. Restarting five times to discover
    /// five renamed fields is its own kind of silence.
    #[tokio::test]
    async fn every_change_is_reported_in_one_pass() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let before = schema_of(&[("body", FieldType::Text), ("old", FieldType::Text)], None);
        bootstrap_schema(&db, &before).await.unwrap();

        let after = schema_of(&[("body", FieldType::Int), ("new", FieldType::Text)], None);
        let drift = drift_of(evolve_schema(&db, &after).await.expect_err("must refuse"));

        let message = drift.to_string();
        for expected in ["body", "old", "new"] {
            assert!(message.contains(expected), "reports '{expected}': {message}");
        }
        assert_eq!(drift.collections[0].changes.len(), 3);
    }

    /// Restarting an unchanged app must stay clean however many times it boots —
    /// the counterpart to `seed_is_idempotent_across_reboots`.
    #[tokio::test]
    async fn repeated_boots_of_an_unchanged_schema_stay_clean() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = schema_of(&[("body", FieldType::Text)], Some("body"));
        for _ in 0..3 {
            evolve_schema(&db, &schema).await.expect("no drift");
            bootstrap_schema(&db, &schema).await.unwrap();
        }
    }
}
