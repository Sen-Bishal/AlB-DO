//! Walking-skeleton wiring for FORGE **Gate 1** — the read loop.
//!
//! This is the hand-authored stand-in for what escape analysis (Pillar 1)
//! will later infer. It hard-codes one FORGE-backed collection — a
//! guestbook — and does the three things Gate 1 needs:
//!
//! 1. [`bootstrap_schema`] — create the table (and seed it once) so there
//!    is data to render. Stands in for Pillar-3 auto-migration.
//! 2. [`FORGE_SLOTS`] — the `topic → query` map. Stands in for the
//!    inferred `DataSource::DbQuery` a component's reads would produce.
//! 3. [`hydrate_topics`] — run each query through the [`DataSubstrate`],
//!    materialise the rows to JSON, and seed the matching
//!    [`BroadcastRegistry`] topic. The serve path already reads that
//!    topic's value at render time (`useSharedSlot`), so the SSR HTML
//!    comes out carrying the persisted rows with no per-request I/O.
//!
//! Everything here is substrate-agnostic (it speaks only the
//! [`DataSubstrate`] trait), so it is *not* feature-gated: it compiles in
//! the default build and is exercised against libSQL only where the
//! `forge` feature supplies a real backend.

use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Rows, SqlValue};
use crate::runtime::broadcast::BroadcastRegistry;

/// One FORGE-backed shared slot: a broadcast topic whose value is
/// materialised from a substrate query. The hand-authored ancestor of an
/// inferred `(collection, query)` pair.
pub struct ForgeSlot {
    /// The `useSharedSlot(topic)` key the component reads.
    pub topic: &'static str,
    /// The read that materialises the topic's value.
    pub query: &'static str,
}

/// The walking-skeleton `topic → query` map. One entry: the guestbook.
/// Replaced by escape-analysis output in Phase 1 (#2).
pub const FORGE_SLOTS: &[ForgeSlot] = &[ForgeSlot {
    topic: "guestbook",
    query: "SELECT id, author, message FROM guestbook ORDER BY id",
}];

/// DDL for the skeleton table, plus a one-time seed so the first boot has
/// something to render. Idempotent: the table is `IF NOT EXISTS` and the
/// seed only runs when the table is empty, so a restart preserves rows
/// written at runtime (the Gate-3 durability property).
///
/// # Errors
/// Propagates any [`SubstrateError`](crate::forge::value::SubstrateError)
/// from the migration or the seed statements.
pub async fn bootstrap_schema(substrate: &dyn DataSubstrate) -> crate::forge::value::Result<()> {
    substrate
        .migrate(
            "CREATE TABLE IF NOT EXISTS guestbook (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 author TEXT NOT NULL, \
                 message TEXT NOT NULL)",
        )
        .await?;

    let existing = substrate
        .query("SELECT COUNT(*) FROM guestbook", &[])
        .await?;
    let count = existing
        .rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(SqlValue::as_i64)
        .unwrap_or(0);

    if count == 0 {
        for (author, message) in [("ada", "first light"), ("alan", "the machine stirs")] {
            substrate
                .execute(
                    "INSERT INTO guestbook (author, message) VALUES (?1, ?2)",
                    &[author.into(), message.into()],
                )
                .await?;
        }
    }

    Ok(())
}

/// Materialise every [`FORGE_SLOTS`] entry from the substrate into
/// `broadcast`. Registers the topic if absent, then writes the materialised
/// value (overriding the `b"null"` placeholder the serve path seeds shared
/// topics with at registration). At boot there are no subscribers, so the
/// `write_topic` fan-out is a no-op that just sets the stored value.
///
/// # Errors
/// Propagates any read error from the substrate. A broadcast write failure
/// is non-fatal (no subscribers at boot) and is intentionally swallowed.
pub async fn hydrate_topics(
    substrate: &dyn DataSubstrate,
    broadcast: &BroadcastRegistry,
) -> crate::forge::value::Result<()> {
    for (topic, bytes) in materialize_seeds(substrate).await? {
        // Ensure the topic exists (idempotent), then force its value — the
        // register-time `b"null"` seed would otherwise win, since `topic`
        // never overwrites an existing value.
        broadcast.topic(topic.as_str(), bytes.clone());
        let _ = broadcast.write_topic(topic.as_str(), bytes);
    }
    Ok(())
}

/// Materialise every [`FORGE_SLOTS`] entry to `(topic, JSON bytes)` without
/// touching a broadcast registry. Used at **build time**: the manifest builder
/// seeds a fresh registry with these before Stream-B pre-renders each Tier-B
/// island, so the baked HTML carries the persisted rows rather than the empty
/// `b"null"` placeholder. Same materialisation [`hydrate_topics`] uses at
/// serve-boot, factored out so both paths agree byte-for-byte.
///
/// # Errors
/// Propagates any read error from the substrate.
pub async fn materialize_seeds(
    substrate: &dyn DataSubstrate,
) -> crate::forge::value::Result<Vec<(String, Vec<u8>)>> {
    let mut seeds = Vec::with_capacity(FORGE_SLOTS.len());
    for slot in FORGE_SLOTS {
        let rows = substrate.query(slot.query, &[]).await?;
        let bytes = serde_json::to_vec(&rows_to_json(&rows)).unwrap_or_else(|_| b"[]".to_vec());
        seeds.push((slot.topic.to_string(), bytes));
    }
    Ok(seeds)
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

#[cfg(all(test, feature = "forge"))]
mod tests {
    use super::*;
    use crate::forge::LibSqlSubstrate;

    #[tokio::test]
    async fn hydrates_guestbook_topic_from_a_real_substrate() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();

        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

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
        bootstrap_schema(&db).await.unwrap();
        // A second bootstrap against the same (now non-empty) table must not
        // re-seed — the Gate-3 property that runtime writes survive restart.
        bootstrap_schema(&db).await.unwrap();

        let rows = db
            .query("SELECT COUNT(*) FROM guestbook", &[])
            .await
            .unwrap();
        let count = rows.rows[0].get(0).and_then(SqlValue::as_i64).unwrap();
        assert_eq!(count, 2, "seed ran once, not twice");
    }
}
