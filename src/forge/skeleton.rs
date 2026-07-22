//! FORGE's collection registry — the `topic → (query, schema)` map.
//!
//! This was a single hand-authored `&'static` slot (one hardcoded guestbook).
//! It is now an owned, boot-built [`ForgeSchema`] — the seam every source of
//! collections funnels through:
//!
//! - **today** — [`ForgeSchema::guestbook_default`], the built-in default that
//!   reproduces the walking-skeleton guestbook byte-for-byte, so nothing that
//!   depended on it changed;
//! - **next (Phase 2)** — an app-declared schema (a `forge` block in the app's
//!   config, or a schema file) parsed into [`ForgeCollection`]s;
//! - **endgame (Pillar 1)** — escape analysis emitting the same
//!   [`ForgeCollection`]s from the component's own `useSharedSlot` reads.
//!
//! All three produce a `Vec<ForgeCollection>` and hand it to
//! [`ForgeSchema::build`]; the *runtime* never learns which source it came from.
//!
//! # Why the schema is keyed by the wire's `SlotId`
//!
//! A collection has exactly one identity that already travels everywhere: its
//! broadcast [`SlotId`], `fnv1a_32("broadcast::{topic}")`
//! ([`broadcast_slot_id`]). The registry, the wire, and the client all address
//! it by that 32-bit number. So the schema keys on the *same* number rather than
//! inventing a second identity — a write's `slot_for_topic` and a fan-out's
//! slot-id land in the one lookup, and a hash collision (two topics sharing a
//! wire slot — already a latent wire bug the old `const` never caught) is
//! promoted to a **boot-time** invariant: [`ForgeSchema::build`] refuses it.
//!
//! The lookup itself is a **struct-of-arrays**: a dense, sorted `Box<[u32]>`
//! search spine kept *separate* from the fat [`ForgeCollection`] payload. A
//! per-write resolve is a branch-predictable binary search over ~16 ids per
//! cache line that dereferences the payload exactly once, at the found index —
//! not a linear pointer-chase through owned `String`s.

use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Rows, SqlValue};
use crate::ir::opcode::SlotId;
use crate::runtime::broadcast::{broadcast_slot_id, BroadcastRegistry};

/// One row of a collection's one-time seed: a statement plus its bound params,
/// run once when the backing table is empty (so runtime writes survive restart).
/// Params **bind** — they never reach SQL as text — so seed data is not an
/// identifier hazard the way a table name is.
#[derive(Debug, Clone)]
pub struct SeedRow {
    pub sql: String,
    pub params: Box<[SqlValue]>,
}

/// Sort direction of one `ORDER BY` term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// One `ORDER BY` term: a column and its direction. The structured form of the
/// query's ordering, so the write path can decide where a new row lands (tail,
/// head, or before some sibling key) without re-parsing SQL or comparing whole
/// arrays at write time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    pub column: String,
    pub dir: SortDir,
}

/// One FORGE-backed collection: a broadcast topic materialised from a substrate
/// query, plus the schema (DDL + optional seed) that makes the query answerable.
/// The owned successor of the old `&'static ForgeSlot` — the hand-authored
/// ancestor of an inferred `(collection, query, key)` triple.
#[derive(Debug, Clone)]
pub struct ForgeCollection {
    /// The `useSharedSlot(topic)` key the component reads, and the broadcast
    /// topic the write fans out on.
    pub topic: String,
    /// Physical table name. Kept explicit (rather than parsed out of `query`)
    /// so the empty-check probe and future introspection have a first-class
    /// fact, and so it can be identifier-validated once at build.
    pub table: String,
    /// The read that materialises the topic's value.
    pub query: String,
    /// Column that identifies a row across two materialisations — the
    /// reconciliation identity a `SlotDelta`/`ReconcileList` is keyed by, and the
    /// value the author's `key={row.id}` stamps into `data-albedo-key`.
    pub key_column: String,
    /// Precomputed wire identity, `broadcast_slot_id(&topic)`. Set once at
    /// construction; it is the schema's lookup key and the wire's slot id, one
    /// and the same. See the module docs.
    pub slot_id: SlotId,
    /// Idempotent DDL (`CREATE TABLE IF NOT EXISTS …`, indexes, …) run at every
    /// boot. Stands in for Pillar-3 auto-migration.
    pub migrations: Box<[String]>,
    /// One-time seed rows, applied only when `table` is empty.
    pub seed: Box<[SeedRow]>,
    /// Structured ordering, derived from the query's `ORDER BY` at construction
    /// (see [`parse_order_by`]). Empty when the ordering can't be proven a simple
    /// column list.
    ///
    /// Not yet read by the write path. Insert position is currently taken from
    /// the freshly materialised array's own order (see
    /// [`classify_positioned_insert`](crate::forge::delta::classify_positioned_insert)),
    /// which is exact because the substrate did the ordering. This becomes the
    /// source of position when the whole array stops being materialised — an
    /// `INSERT … RETURNING` knows the new record but not where it landed, and
    /// these keys are what will place it.
    pub sort: Box<[SortKey]>,
}

impl ForgeCollection {
    /// Assemble a collection, precomputing its wire [`SlotId`] from `topic`.
    #[must_use]
    pub fn new(
        topic: impl Into<String>,
        table: impl Into<String>,
        query: impl Into<String>,
        key_column: impl Into<String>,
        migrations: Box<[String]>,
        seed: Box<[SeedRow]>,
    ) -> Self {
        let topic = topic.into();
        let slot_id = broadcast_slot_id(&topic);
        let query = query.into();
        let sort = parse_order_by(&query);
        Self {
            topic,
            table: table.into(),
            query,
            key_column: key_column.into(),
            slot_id,
            migrations,
            seed,
            sort,
        }
    }
}

/// Extract the structured ordering from a query's `ORDER BY` clause.
///
/// Deliberately strict: it accepts only a comma-separated list of **plain column
/// identifiers**, each with an optional `ASC`/`DESC`. Anything it can't prove is
/// a simple column list — an expression (`lower(name)`), a `COLLATE`, a
/// `NULLS FIRST`, a subquery, no `ORDER BY` at all — yields an **empty** result,
/// and an empty `sort` sends the write path to its whole-array classification,
/// which is always correct. So a parse that gives up costs the fast position
/// computation, never correctness. The query text stays the single source of
/// truth for ordering; this is just its structured shadow.
#[must_use]
pub fn parse_order_by(query: &str) -> Box<[SortKey]> {
    let lower = query.to_ascii_lowercase();
    let Some(at) = lower.rfind("order by") else {
        return Box::new([]);
    };
    // The clause runs from after `order by` to the first trailing clause that can
    // follow it, or the end of the statement.
    let tail = &query[at + "order by".len()..];
    let tail_lower = &lower[at + "order by".len()..];
    let end = ["limit", "offset", ";", ")"]
        .iter()
        .filter_map(|kw| tail_lower.find(kw))
        .min()
        .unwrap_or(tail.len());
    let clause = tail[..end].trim();
    if clause.is_empty() {
        return Box::new([]);
    }

    let mut keys = Vec::new();
    for term in clause.split(',') {
        let mut parts = term.split_whitespace();
        let Some(column) = parts.next() else {
            return Box::new([]);
        };
        // Only a plain identifier can be reasoned about positionally; anything
        // else (a function call, a quoted expr) fails the whole parse.
        if !crate::forge::write::is_safe_identifier(column) {
            return Box::new([]);
        }
        let dir = match parts.next() {
            None => SortDir::Asc,
            Some(word) if word.eq_ignore_ascii_case("asc") => SortDir::Asc,
            Some(word) if word.eq_ignore_ascii_case("desc") => SortDir::Desc,
            // `NULLS FIRST`, a trailing `COLLATE`, a second unexpected token:
            // give up rather than guess.
            Some(_) => return Box::new([]),
        };
        // A term with more than `<col> <dir>` (e.g. `col desc nulls last`) is
        // more than we can model.
        if parts.next().is_some() {
            return Box::new([]);
        }
        keys.push(SortKey { column: column.to_string(), dir });
    }
    keys.into_boxed_slice()
}

/// Why a set of [`ForgeCollection`]s cannot form a valid [`ForgeSchema`].
#[derive(Debug, thiserror::Error)]
pub enum ForgeSchemaError {
    /// Two topics hash to the same wire [`SlotId`]. Undetectable at the wire
    /// (both would drive the same broadcast slot), so it is refused here where
    /// the whole collection set is visible at once.
    #[error(
        "FORGE schema: topics {a:?} and {b:?} collide on wire slot {slot:#010x}; \
         rename one collection"
    )]
    SlotCollision { a: String, b: String, slot: u32 },
    /// A `table` or `key_column` is not a plain SQL identifier — it would reach
    /// SQL as text, and identifiers cannot bind. Refused at build, not at write.
    #[error("FORGE schema: {field} {value:?} for topic {topic:?} is not a safe SQL identifier")]
    InvalidIdentifier {
        topic: String,
        field: &'static str,
        value: String,
    },
}

/// The FORGE collection registry: an immutable, boot-built lookup from a
/// collection's wire [`SlotId`] to its [`ForgeCollection`].
///
/// Layout is struct-of-arrays (see module docs): `ids` is the dense sorted
/// search spine, `collections[i]` its parallel payload. Cheap to `Arc`-share and
/// hold on the live runtime across a dev reload.
#[derive(Debug, Clone)]
pub struct ForgeSchema {
    /// `slot_id.0` for every collection, sorted ascending. The binary-search
    /// spine — contiguous `u32`s, packed ~16 to a cache line.
    ids: Box<[u32]>,
    /// Parallel to `ids`, same order: `collections[i].slot_id.0 == ids[i]`.
    collections: Box<[ForgeCollection]>,
}

impl ForgeSchema {
    /// Build a schema from a collection set, sorting by wire [`SlotId`] and
    /// rejecting slot collisions and unsafe identifiers.
    ///
    /// The single funnel every collection source (default, config, inference)
    /// passes through, so the invariants hold no matter where the collections
    /// came from.
    ///
    /// # Errors
    /// [`ForgeSchemaError::SlotCollision`] on a shared wire slot;
    /// [`ForgeSchemaError::InvalidIdentifier`] on a `table`/`key_column` that
    /// isn't a plain identifier.
    pub fn build(mut collections: Vec<ForgeCollection>) -> Result<Self, ForgeSchemaError> {
        for c in &collections {
            for (field, value) in [("table", &c.table), ("key_column", &c.key_column)] {
                if !crate::forge::write::is_safe_identifier(value) {
                    return Err(ForgeSchemaError::InvalidIdentifier {
                        topic: c.topic.clone(),
                        field,
                        value: value.clone(),
                    });
                }
            }
        }

        // Sort by the 32-bit wire id so the spine is a searchable dense array
        // and any collision lands as an adjacent duplicate.
        collections.sort_unstable_by_key(|c| c.slot_id.0);
        for pair in collections.windows(2) {
            if pair[0].slot_id == pair[1].slot_id {
                return Err(ForgeSchemaError::SlotCollision {
                    a: pair[0].topic.clone(),
                    b: pair[1].topic.clone(),
                    slot: pair[0].slot_id.0,
                });
            }
        }

        let ids = collections.iter().map(|c| c.slot_id.0).collect();
        Ok(Self {
            ids,
            collections: collections.into_boxed_slice(),
        })
    }

    /// The collection driving wire slot `slot_id`, if any. `O(log n)` binary
    /// search over the dense id spine; the payload is touched once.
    #[must_use]
    pub fn resolve(&self, slot_id: SlotId) -> Option<&ForgeCollection> {
        self.ids
            .binary_search(&slot_id.0)
            .ok()
            .map(|i| &self.collections[i])
    }

    /// The collection named `topic`, if any. The allowlist that keeps a
    /// collection name arriving from userland from ever reaching SQL as an
    /// identifier — resolves through the wire id, so it shares the one lookup.
    #[must_use]
    pub fn slot_for_topic(&self, topic: &str) -> Option<&ForgeCollection> {
        self.resolve(broadcast_slot_id(topic))
    }

    /// Every collection, in wire-id order.
    #[must_use]
    pub fn collections(&self) -> &[ForgeCollection] {
        &self.collections
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.collections.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.collections.len()
    }

    /// The built-in default: the walking-skeleton guestbook, reproducing the old
    /// `FORGE_SLOTS` const exactly (same query, same key, same DDL + ada/alan
    /// seed). Used until an app declares its own schema.
    #[must_use]
    pub fn guestbook_default() -> Self {
        let guestbook = ForgeCollection::new(
            "guestbook",
            "guestbook",
            "SELECT id, author, message FROM guestbook ORDER BY id",
            "id",
            Box::new([
                "CREATE TABLE IF NOT EXISTS guestbook (\
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     author TEXT NOT NULL, \
                     message TEXT NOT NULL)"
                    .to_string(),
            ]),
            Box::new([
                SeedRow {
                    sql: "INSERT INTO guestbook (author, message) VALUES (?1, ?2)".to_string(),
                    params: Box::new(["ada".into(), "first light".into()]),
                },
                SeedRow {
                    sql: "INSERT INTO guestbook (author, message) VALUES (?1, ?2)".to_string(),
                    params: Box::new(["alan".into(), "the machine stirs".into()]),
                },
            ]),
        );
        Self::build(vec![guestbook]).expect("the single-collection default cannot collide")
    }
}

impl Default for ForgeSchema {
    fn default() -> Self {
        Self::guestbook_default()
    }
}

/// Run every collection's migrations, then seed each one that is still empty.
///
/// Idempotent: migrations are `IF NOT EXISTS` and a seed runs only against an
/// empty table, so a restart preserves rows written at runtime (the Gate-3
/// durability property). Stands in for Pillar-3 auto-migration.
///
/// # Errors
/// Propagates any [`SubstrateError`](crate::forge::value::SubstrateError) from a
/// migration, the empty-check probe, or a seed statement.
pub async fn bootstrap_schema(
    substrate: &dyn DataSubstrate,
    schema: &ForgeSchema,
) -> crate::forge::value::Result<()> {
    for collection in schema.collections() {
        for ddl in collection.migrations.iter() {
            substrate.migrate(ddl).await?;
        }
        if collection.seed.is_empty() {
            continue;
        }
        // `table` is identifier-validated at `ForgeSchema::build`, so this
        // interpolation is safe.
        let probe = format!("SELECT COUNT(*) FROM {}", collection.table);
        let existing = substrate.query(&probe, &[]).await?;
        let count = existing
            .rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqlValue::as_i64)
            .unwrap_or(0);
        if count == 0 {
            for row in collection.seed.iter() {
                substrate.execute(&row.sql, &row.params).await?;
            }
        }
    }
    Ok(())
}

/// Materialise every collection into `broadcast`. Registers the topic if absent,
/// then forces the materialised value over the register-time `b"null"`
/// placeholder. At boot there are no subscribers, so the `write_topic` fan-out
/// just sets the stored value.
///
/// # Errors
/// Propagates any read error from the substrate. A broadcast write failure is
/// non-fatal (no subscribers at boot) and is intentionally swallowed.
pub async fn hydrate_topics(
    substrate: &dyn DataSubstrate,
    broadcast: &BroadcastRegistry,
    schema: &ForgeSchema,
) -> crate::forge::value::Result<()> {
    for (topic, bytes) in materialize_seeds(substrate, schema).await? {
        broadcast.topic(topic.as_str(), bytes.clone());
        let _ = broadcast.write_topic(topic.as_str(), bytes);
    }
    Ok(())
}

/// Materialise every collection to `(topic, JSON bytes)` without touching a
/// broadcast registry. Used at **build time**: the manifest builder seeds a
/// fresh registry with these before Stream-B pre-renders each Tier-B island, so
/// the baked HTML carries persisted rows rather than the `b"null"` placeholder.
/// Same materialisation [`hydrate_topics`] uses at serve-boot, factored out so
/// both paths agree byte-for-byte.
///
/// # Errors
/// Propagates any read error from the substrate.
pub async fn materialize_seeds(
    substrate: &dyn DataSubstrate,
    schema: &ForgeSchema,
) -> crate::forge::value::Result<Vec<(String, Vec<u8>)>> {
    let mut seeds = Vec::with_capacity(schema.len());
    for collection in schema.collections() {
        seeds.push((
            collection.topic.clone(),
            materialize_slot(substrate, collection).await?,
        ));
    }
    Ok(seeds)
}

/// Materialise ONE collection to the JSON bytes its topic carries.
///
/// The single definition of "what this collection currently looks like", shared
/// by boot hydration, the build-time bake, and post-write rematerialisation. If
/// these diverged, a write would fan out a value shaped differently from the one
/// SSR rendered.
///
/// # Errors
/// Propagates any read error from the substrate.
pub async fn materialize_slot(
    substrate: &dyn DataSubstrate,
    collection: &ForgeCollection,
) -> crate::forge::value::Result<Vec<u8>> {
    let rows = substrate.query(collection.query.as_str(), &[]).await?;
    Ok(serde_json::to_vec(&rows_to_json(&rows)).unwrap_or_else(|_| b"[]".to_vec()))
}

/// Map a substrate result set to a JSON array of column-keyed objects —
/// the shape a component reading `data.map(row => …)` expects, and the
/// contract the inferred query (#2) must later satisfy.
fn rows_to_json(rows: &Rows) -> serde_json::Value {
    let records = rows
        .rows
        .iter()
        .map(|row| {
            let object = rows
                .columns
                .iter()
                .enumerate()
                .map(|(idx, column)| {
                    let value = row.get(idx).map_or(serde_json::Value::Null, sqlvalue_to_json);
                    (column.clone(), value)
                })
                .collect::<serde_json::Map<String, serde_json::Value>>();
            serde_json::Value::Object(object)
        })
        .collect();
    serde_json::Value::Array(records)
}

/// Lower one neutral [`SqlValue`] into JSON.
fn sqlvalue_to_json(value: &SqlValue) -> serde_json::Value {
    use serde_json::Value;
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => Value::from(*i),
        SqlValue::Real(r) => serde_json::Number::from_f64(*r).map_or(Value::Null, Value::Number),
        SqlValue::Text(t) => Value::String(t.clone()),
        SqlValue::Blob(b) => Value::Array(b.iter().map(|byte| Value::from(*byte)).collect()),
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    fn coll(topic: &str, table: &str, key: &str) -> ForgeCollection {
        ForgeCollection::new(
            topic,
            table,
            format!("SELECT {key} FROM {table}"),
            key,
            Box::new([]),
            Box::new([]),
        )
    }

    fn key(column: &str, dir: SortDir) -> SortKey {
        SortKey { column: column.to_string(), dir }
    }

    #[test]
    fn order_by_a_bare_column_is_ascending() {
        assert_eq!(
            *parse_order_by("SELECT id, author FROM guestbook ORDER BY id"),
            [key("id", SortDir::Asc)],
        );
    }

    #[test]
    fn order_by_reads_explicit_direction_case_insensitively() {
        assert_eq!(
            *parse_order_by("SELECT * FROM t ORDER BY created_at DESC"),
            [key("created_at", SortDir::Desc)],
        );
        assert_eq!(
            *parse_order_by("select * from t order by ID desc"),
            [key("ID", SortDir::Desc)],
        );
    }

    #[test]
    fn order_by_handles_a_multi_column_list() {
        assert_eq!(
            *parse_order_by("SELECT * FROM t ORDER BY rank DESC, id ASC"),
            [key("rank", SortDir::Desc), key("id", SortDir::Asc)],
        );
    }

    #[test]
    fn a_trailing_limit_does_not_leak_into_the_sort() {
        assert_eq!(
            *parse_order_by("SELECT * FROM t ORDER BY id DESC LIMIT 10"),
            [key("id", SortDir::Desc)],
        );
    }

    #[test]
    fn no_order_by_yields_no_sort() {
        assert!(parse_order_by("SELECT id FROM t").is_empty());
    }

    #[test]
    fn an_expression_order_is_not_modelled_and_yields_no_sort() {
        // A function or expression can't be reasoned about positionally, so the
        // whole parse gives up — the write path then classifies whole-array.
        assert!(parse_order_by("SELECT * FROM t ORDER BY lower(name)").is_empty());
        assert!(parse_order_by("SELECT * FROM t ORDER BY id DESC NULLS LAST").is_empty());
    }

    #[test]
    fn guestbook_default_sorts_by_id_ascending() {
        let schema = ForgeSchema::guestbook_default();
        let guestbook = schema.slot_for_topic("guestbook").unwrap();
        assert_eq!(*guestbook.sort, [key("id", SortDir::Asc)]);
    }

    #[test]
    fn resolve_and_slot_for_topic_agree_with_the_wire_id() {
        let schema = ForgeSchema::build(vec![
            coll("guestbook", "guestbook", "id"),
            coll("todos", "todos", "id"),
        ])
        .unwrap();

        let gb = schema.slot_for_topic("guestbook").expect("known topic");
        assert_eq!(gb.topic, "guestbook");
        // The lookup key IS the wire id, so resolving by it lands the same row.
        assert_eq!(
            schema.resolve(broadcast_slot_id("guestbook")).map(|c| &c.topic),
            Some(&"guestbook".to_string())
        );
        assert!(schema.slot_for_topic("unknown").is_none());
    }

    #[test]
    fn the_id_spine_is_sorted_and_parallel_to_the_payload() {
        let schema =
            ForgeSchema::build(vec![coll("zeta", "zeta", "id"), coll("alpha", "alpha", "id")])
                .unwrap();
        assert!(schema.ids.windows(2).all(|w| w[0] < w[1]), "spine sorted");
        for (i, id) in schema.ids.iter().enumerate() {
            assert_eq!(*id, schema.collections[i].slot_id.0, "spine parallel to payload");
        }
    }

    #[test]
    fn a_wire_slot_collision_is_refused_at_build() {
        // Two ForgeCollections claiming the same slot id (constructed by hand to
        // simulate an FNV collision) must not build.
        let mut a = coll("a", "a", "id");
        let mut b = coll("b", "b", "id");
        b.slot_id = a.slot_id; // force the collision
        a.slot_id = SlotId(42);
        b.slot_id = SlotId(42);
        let err = ForgeSchema::build(vec![a, b]).unwrap_err();
        assert!(matches!(err, ForgeSchemaError::SlotCollision { slot: 42, .. }));
    }

    #[test]
    fn an_unsafe_table_or_key_is_refused_at_build() {
        assert!(matches!(
            ForgeSchema::build(vec![coll("t", "drop table users;--", "id")]).unwrap_err(),
            ForgeSchemaError::InvalidIdentifier { field: "table", .. }
        ));
        assert!(matches!(
            ForgeSchema::build(vec![coll("t", "t", "1; DELETE")]).unwrap_err(),
            ForgeSchemaError::InvalidIdentifier { field: "key_column", .. }
        ));
    }

    #[test]
    fn the_default_is_the_guestbook_and_builds_clean() {
        let schema = ForgeSchema::guestbook_default();
        assert_eq!(schema.len(), 1);
        let gb = schema.slot_for_topic("guestbook").unwrap();
        assert_eq!(gb.key_column, "id");
        assert_eq!(gb.query, "SELECT id, author, message FROM guestbook ORDER BY id");
        assert_eq!(gb.seed.len(), 2, "ada + alan");
    }
}

#[cfg(all(test, feature = "forge"))]
mod tests {
    use super::*;
    use crate::forge::LibSqlSubstrate;

    #[tokio::test]
    async fn hydrates_guestbook_topic_from_a_real_substrate() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();

        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        let topic = broadcast
            .get("guestbook")
            .expect("hydration registers the guestbook topic");
        let value: serde_json::Value =
            serde_json::from_slice(&topic.current_value()).expect("topic value is JSON");
        let rows = value.as_array().expect("materialised value is a JSON array");

        assert_eq!(rows.len(), 2, "both seed rows materialised");
        assert_eq!(rows[0]["author"], "ada");
        assert_eq!(rows[0]["message"], "first light");
        assert!(rows[0]["id"].is_number(), "id column carried through as a number");
    }

    #[tokio::test]
    async fn seed_is_idempotent_across_reboots() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();
        // A second bootstrap against the same (now non-empty) table must not
        // re-seed — the Gate-3 property that runtime writes survive restart.
        bootstrap_schema(&db, &schema).await.unwrap();

        let rows = db
            .query("SELECT COUNT(*) FROM guestbook", &[])
            .await
            .unwrap();
        let count = rows.rows[0].get(0).and_then(SqlValue::as_i64).unwrap();
        assert_eq!(count, 2, "seed ran once, not twice");
    }
}
