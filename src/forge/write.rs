//! FORGE's write loop: **mutate → rematerialize → fan out**.
//!
//! The read loop ([`crate::forge::skeleton`]) makes a persistent collection
//! visible: a `BroadcastRegistry` topic whose value is materialised from the
//! substrate. This closes the circle — a TSX `action()` body appends a record,
//! and every subscriber sees the collection's new value.
//!
//! # Why writes are recorded, not executed
//!
//! An action body is evaluated **synchronously**
//! ([`CompiledProject::invoke_action_with_broadcast`] returns
//! `Result<Vec<Instruction>>`, not a future), while [`DataSubstrate`] is
//! **async**. A builtin called from inside that evaluation therefore cannot
//! await a write, and blocking on one from inside the async runtime's own
//! worker would deadlock.
//!
//! So the builtin *records an intent* onto a thread-local, exactly as
//! `useState` setters record slot writes, and the async action adapter drains
//! and applies them once the body has run. That ordering is not a workaround —
//! it is the seam a durable/resumable action log hooks into later: the intents
//! are precisely what such a log would need to persist before executing.
//!
//! # Why the fan-out happens after commit
//!
//! [`apply_writes`] rematerialises and broadcasts only once the transaction has
//! committed. Broadcasting from inside the transaction would let subscribers
//! observe a collection state that a failed commit then erases — a value that
//! never existed. Fan-out is therefore strictly a report of durable state.
//!
//! # Why the fan-out carries a delta
//!
//! Rematerialisation answers "what does this collection look like now"; the
//! page needs "what changed". Those are the same information at very different
//! prices — one is `O(|view|)` and forces a keyed list to rebuild every row
//! (losing the DOM identity of rows that did not change), the other is
//! `O(|Δ|)`. So the post-commit step ships both: the snapshot as the
//! authoritative value, and the z-set delta ([`crate::forge::delta`]) that
//! takes the previous value to it. The delta is computed *inside* the topic's
//! critical section against the value that is actually being replaced — never
//! against a read that a concurrent action may have already invalidated.
//!
//! Rendering rows is not FORGE's business, so it takes a
//! [`RowProjector`] from the render path. Without one — or when the diff
//! cannot be trusted — the write degenerates to exactly the pre-S4 snapshot
//! fan-out, which is slower and always correct.
//!
//! [`CompiledProject::invoke_action_with_broadcast`]: crate::runtime::CompiledProject::invoke_action_with_broadcast

use crate::forge::delta::{diff_records, project_changes, RowProjector};
use crate::forge::skeleton::{materialize_slot, slot_for_topic};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Result, SqlValue, SubstrateError};
use crate::runtime::broadcast::{BroadcastRegistry, TopicTransition};
use serde_json::{Map, Value};
use std::cell::RefCell;

/// One durable mutation requested by an action body.
///
/// An enum with a single variant on purpose: update and delete are the *same*
/// loop (mutate → rematerialize → fan out) differing only in the statement
/// built, so they land as additional arms here rather than as new machinery.
/// Append is what the walking skeleton needs to prove the loop closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeWrite {
    /// Append a record to a persistent collection. `collection` is the topic
    /// key the component reads via `useSharedSlot`.
    Append {
        collection: String,
        record: Map<String, Value>,
    },
}

impl ForgeWrite {
    /// The collection this write targets — the topic that must be
    /// rematerialised once it commits.
    #[must_use]
    pub fn collection(&self) -> &str {
        match self {
            Self::Append { collection, .. } => collection.as_str(),
        }
    }
}

thread_local! {
    /// Writes recorded by the action body currently being evaluated. `None`
    /// means no collector is installed, which is how the `append()` builtin
    /// knows it is being called outside an action and can say so instead of
    /// silently dropping a write.
    static FORGE_WRITES: RefCell<Option<Vec<ForgeWrite>>> = const { RefCell::new(None) };
}

/// Collects the writes an action body records, and restores whatever collector
/// was installed before it on drop.
///
/// Mirrors `install_phase_k_broadcast`'s guard discipline: nested dispatch on
/// one thread must not have an inner action steal an outer action's writes.
pub struct ForgeWriteCollector {
    previous: Option<Vec<ForgeWrite>>,
}

impl ForgeWriteCollector {
    /// The writes recorded since installation, in call order.
    #[must_use]
    pub fn take(&self) -> Vec<ForgeWrite> {
        FORGE_WRITES.with(|cell| {
            cell.borrow_mut()
                .as_mut()
                .map_or_else(Vec::new, std::mem::take)
        })
    }
}

impl Drop for ForgeWriteCollector {
    fn drop(&mut self) {
        FORGE_WRITES.with(|cell| *cell.borrow_mut() = self.previous.take());
    }
}

/// Install a collector for the duration of one action dispatch. Hold the guard
/// across the (synchronous) body evaluation, then [`ForgeWriteCollector::take`]
/// the intents to apply.
#[must_use]
pub fn install_forge_write_collector() -> ForgeWriteCollector {
    let previous = FORGE_WRITES.with(|cell| cell.borrow_mut().replace(Vec::new()));
    ForgeWriteCollector { previous }
}

/// Record one write from a builtin. Returns `false` when no collector is
/// installed — the caller must surface that rather than pretend the write
/// happened.
pub(crate) fn record_forge_write(write: ForgeWrite) -> bool {
    FORGE_WRITES.with(|cell| {
        let mut slot = cell.borrow_mut();
        match slot.as_mut() {
            Some(writes) => {
                writes.push(write);
                true
            }
            None => false,
        }
    })
}

/// A SQL identifier this module is willing to emit.
///
/// Identifiers cannot be bound as parameters, so a column name arriving from a
/// TSX object literal would otherwise be concatenated into SQL verbatim. In
/// practice these are compile-time literals, but "in practice" is not a
/// security boundary: anything that is not a plain `[A-Za-z_][A-Za-z0-9_]*`
/// identifier is refused. Values never take this path — they bind.
fn is_safe_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Lower one JSON value to a substrate-neutral [`SqlValue`].
///
/// Objects and arrays are refused rather than silently stringified: a nested
/// value in an append is a modelling question (a column? a relation?) that the
/// skeleton has no answer for, and guessing would persist something the author
/// did not ask for.
fn json_to_sqlvalue(column: &str, value: &Value) -> Result<SqlValue> {
    match value {
        Value::Null => Ok(SqlValue::Null),
        Value::Bool(b) => Ok(SqlValue::Integer(i64::from(*b))),
        Value::String(s) => Ok(SqlValue::Text(s.clone())),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(SqlValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(SqlValue::Real(f))
            } else {
                Err(SubstrateError::Backend(format!(
                    "FORGE append: column '{column}' has a number that is neither i64 nor f64"
                )))
            }
        }
        Value::Object(_) | Value::Array(_) => Err(SubstrateError::Backend(format!(
            "FORGE append: column '{column}' is a nested object/array; \
             append takes flat records of scalars"
        ))),
    }
}

/// Build the `INSERT` for one append, with values bound rather than inlined.
fn build_append(collection: &str, record: &Map<String, Value>) -> Result<(String, Vec<SqlValue>)> {
    if record.is_empty() {
        return Err(SubstrateError::Backend(format!(
            "FORGE append: record for '{collection}' is empty; nothing to write"
        )));
    }

    let mut columns = Vec::with_capacity(record.len());
    let mut params = Vec::with_capacity(record.len());
    for (column, value) in record {
        if !is_safe_identifier(column) {
            return Err(SubstrateError::Backend(format!(
                "FORGE append: '{column}' is not a valid column name"
            )));
        }
        columns.push(column.as_str());
        params.push(json_to_sqlvalue(column, value)?);
    }

    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "INSERT INTO {collection} ({}) VALUES ({placeholders})",
        columns.join(", ")
    );
    Ok((sql, params))
}

/// Apply every recorded write, then rematerialise and fan out each collection
/// they touched.
///
/// **Atomic**: all the writes of one action commit together or not at all, so an
/// action that appends twice can never half-happen. Reuses the transaction seam
/// proven by the kill harness.
///
/// **Fan-out is post-commit** (see module docs) and best-effort: a broadcast
/// failure means subscribers missed a notification, not that the write is in
/// doubt — the data is already durable, and the next render reads it. Failing
/// the action there would tell the author their write was lost when it was not.
///
/// `projector` is the render path's row template. Pass `None` to fan out
/// snapshots only — the pre-S4 behaviour, and the automatic fallback whenever a
/// delta cannot be proven equivalent to the snapshot beside it.
///
/// # Errors
/// Returns [`SubstrateError`] when a collection is unknown, a record is not a
/// flat scalar record, or the transaction fails. Nothing is committed in any of
/// those cases.
pub async fn apply_writes(
    substrate: &dyn DataSubstrate,
    broadcast: &BroadcastRegistry,
    writes: &[ForgeWrite],
    projector: Option<&dyn RowProjector>,
) -> Result<()> {
    if writes.is_empty() {
        return Ok(());
    }

    // Resolve every collection against the allowlist BEFORE opening the
    // transaction: an unknown collection is an authoring error, and it should
    // not cost a write lock or leave a half-built transaction to roll back.
    let mut touched: Vec<&'static crate::forge::skeleton::ForgeSlot> = Vec::new();
    for write in writes {
        let slot = slot_for_topic(write.collection()).ok_or_else(|| {
            SubstrateError::Backend(format!(
                "FORGE append: '{}' is not a FORGE-backed collection",
                write.collection()
            ))
        })?;
        if !touched.iter().any(|known| known.topic == slot.topic) {
            touched.push(slot);
        }
    }

    let tx = substrate.begin().await?;
    for write in writes {
        match write {
            ForgeWrite::Append { collection, record } => {
                // `collection` is a `&'static str` from FORGE_SLOTS by way of
                // the allowlist above — never the userland string itself.
                let slot_name = touched
                    .iter()
                    .find(|slot| slot.topic == collection.as_str())
                    .expect("collection was resolved above")
                    .topic;
                let (sql, params) = match build_append(slot_name, record) {
                    Ok(built) => built,
                    Err(err) => {
                        // Drop the whole action's writes: a malformed record
                        // must not leave earlier appends committed.
                        let _ = tx.rollback().await;
                        return Err(err);
                    }
                };
                if let Err(err) = tx.execute(&sql, &params).await {
                    let _ = tx.rollback().await;
                    return Err(err);
                }
            }
        }
    }
    tx.commit().await?;

    // Durable now — safe to tell the world.
    for slot in touched {
        // Both awaits happen HERE, outside the topic's critical section:
        // `write_topic_delta`'s closure runs under that topic's lock, so
        // awaiting in it would serialise every writer behind the slowest query
        // — and a projector that reached for the topic's value would deadlock
        // on the very lock it is running under. What survives into the closure
        // is only the diff: pure, bounded by the collection size, and the one
        // step that genuinely needs the pre-state it is replacing.
        let bytes = materialize_slot(substrate, slot).await?;
        let rows = match projector {
            Some(projector) => projector.project_rows(slot.topic, &bytes).await,
            None => None,
        };
        broadcast.topic(slot.topic, bytes.clone());
        let _ = broadcast.write_topic_delta(slot.topic, |previous| {
            let changes = rows
                .as_ref()
                .and_then(|rows| row_changes(slot, previous, &bytes, rows))
                .unwrap_or_default();
            TopicTransition {
                value: bytes,
                changes,
            }
        });
    }

    Ok(())
}

/// The delta half of one collection's fan-out: diff the previous materialised
/// value against the new one and pair the changes with their rendered rows.
///
/// `None` at any step means "no delta" — the snapshot alone still ships and is
/// still correct. Both JSON inputs are the *same* materialisation shape by
/// construction ([`materialize_slot`] is the single definition), so a parse
/// failure here means the previous value predates hydration (the `b"null"`
/// placeholder), not that the two disagree.
///
/// `rows` is the projection of `next`, taken before the lock. In the window
/// between that render and this diff another action may have committed and
/// fanned out; then `previous` is *its* value, and this diff describes
/// `their state → our (older) materialisation`. That is still internally
/// consistent — the delta and the snapshot beside it agree, so no client
/// diverges — and the next write re-converges everyone. Making the whole
/// commit-materialise-fan-out sequence atomic per collection is the real fix
/// and belongs with the substrate, not here.
fn row_changes(
    slot: &crate::forge::skeleton::ForgeSlot,
    previous: &[u8],
    next: &[u8],
    rows: &crate::forge::delta::RenderedRows,
) -> Option<Vec<crate::ir::opcode::SlotChange>> {
    let previous: Value = serde_json::from_slice(previous).ok()?;
    let next: Value = serde_json::from_slice(next).ok()?;
    let changes = diff_records(&previous, &next, slot.key_column)?;
    if changes.is_empty() {
        return None;
    }
    project_changes(&changes, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn append(collection: &str, record: Value) -> ForgeWrite {
        ForgeWrite::Append {
            collection: collection.to_string(),
            record: record.as_object().expect("record is an object").clone(),
        }
    }

    #[test]
    fn a_write_recorded_without_a_collector_is_refused_not_swallowed() {
        assert!(
            !record_forge_write(append("guestbook", json!({ "author": "ada" }))),
            "no collector installed => the builtin must be told the write went nowhere"
        );
    }

    #[test]
    fn the_collector_captures_writes_in_call_order() {
        let collector = install_forge_write_collector();
        assert!(record_forge_write(append("guestbook", json!({ "author": "ada" }))));
        assert!(record_forge_write(append("guestbook", json!({ "author": "alan" }))));

        let writes = collector.take();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].collection(), "guestbook");
        match &writes[0] {
            ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "ada"),
        }
    }

    #[test]
    fn dropping_the_collector_uninstalls_it() {
        {
            let _collector = install_forge_write_collector();
        }
        assert!(
            !record_forge_write(append("guestbook", json!({ "author": "ada" }))),
            "the collector must not outlive its dispatch"
        );
    }

    /// Nested dispatch on one thread must not let an inner action swallow an
    /// outer action's writes.
    #[test]
    fn a_nested_collector_restores_the_outer_one() {
        let outer = install_forge_write_collector();
        assert!(record_forge_write(append("guestbook", json!({ "author": "outer" }))));
        {
            let inner = install_forge_write_collector();
            assert!(record_forge_write(append("guestbook", json!({ "author": "inner" }))));
            let inner_writes = inner.take();
            assert_eq!(inner_writes.len(), 1);
            match &inner_writes[0] {
                ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "inner"),
            }
        }
        let outer_writes = outer.take();
        assert_eq!(outer_writes.len(), 1, "outer writes survived the nested dispatch");
        match &outer_writes[0] {
            ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "outer"),
        }
    }

    #[test]
    fn append_binds_values_and_never_inlines_them() {
        let record = json!({ "author": "ada", "message": "first light" });
        let (sql, params) = build_append("guestbook", record.as_object().unwrap()).unwrap();

        assert_eq!(
            sql,
            "INSERT INTO guestbook (author, message) VALUES (?1, ?2)",
            "column order follows the record's (BTreeMap) key order"
        );
        assert_eq!(
            params,
            vec![
                SqlValue::Text("ada".to_string()),
                SqlValue::Text("first light".to_string())
            ]
        );
    }

    /// A value that looks like SQL is data, and must stay data.
    #[test]
    fn a_value_containing_sql_is_bound_not_interpreted() {
        let record = json!({ "author": "'); DROP TABLE guestbook;--" });
        let (sql, params) = build_append("guestbook", record.as_object().unwrap()).unwrap();

        assert_eq!(sql, "INSERT INTO guestbook (author) VALUES (?1)");
        assert!(!sql.contains("DROP"), "the value must never reach the statement text");
        assert_eq!(
            params,
            vec![SqlValue::Text("'); DROP TABLE guestbook;--".to_string())]
        );
    }

    /// Column names CANNOT be bound, so they are the real injection surface.
    #[test]
    fn a_column_name_that_is_not_an_identifier_is_refused() {
        for hostile in [
            "author, message) VALUES ('x','y'); DROP TABLE guestbook;--",
            "author\"",
            "has space",
            "",
            "1leading_digit",
        ] {
            let mut record = Map::new();
            record.insert(hostile.to_string(), json!("x"));
            assert!(
                build_append("guestbook", &record).is_err(),
                "must refuse column name: {hostile:?}"
            );
        }
    }

    #[test]
    fn identifier_rules_accept_ordinary_columns() {
        for ok in ["author", "message", "_private", "col_1", "A"] {
            assert!(is_safe_identifier(ok), "should accept {ok:?}");
        }
    }

    #[test]
    fn a_nested_value_is_refused_rather_than_guessed_at() {
        let record = json!({ "author": { "name": "ada" } });
        let err = build_append("guestbook", record.as_object().unwrap()).unwrap_err();
        assert!(format!("{err}").contains("nested"), "unexpected error: {err}");
    }

    #[test]
    fn an_empty_record_is_refused() {
        assert!(build_append("guestbook", &Map::new()).is_err());
    }

    #[test]
    fn scalars_lower_to_their_sql_shapes() {
        let record = json!({ "a": 1, "b": 1.5, "c": true, "d": Value::Null, "e": "x" });
        let (_, params) = build_append("guestbook", record.as_object().unwrap()).unwrap();
        assert_eq!(
            params,
            vec![
                SqlValue::Integer(1),
                SqlValue::Real(1.5),
                SqlValue::Integer(1),
                SqlValue::Null,
                SqlValue::Text("x".to_string()),
            ]
        );
    }
}

/// The loop against a real backend. `build_append` above proves the statement;
/// these prove the *whole* write path — that a durable row lands AND the topic
/// a component renders from reflects it afterwards.
#[cfg(all(test, feature = "forge"))]
mod substrate_tests {
    use super::*;
    use crate::forge::skeleton::{bootstrap_schema, hydrate_topics};
    use crate::forge::LibSqlSubstrate;
    use serde_json::json;

    fn append(collection: &str, record: Value) -> ForgeWrite {
        ForgeWrite::Append {
            collection: collection.to_string(),
            record: record.as_object().expect("record is an object").clone(),
        }
    }

    fn topic_rows(broadcast: &BroadcastRegistry, topic: &str) -> Vec<Value> {
        let bytes = broadcast
            .get(topic)
            .expect("topic is registered")
            .current_value();
        serde_json::from_slice::<Value>(&bytes)
            .expect("topic value is JSON")
            .as_array()
            .expect("collection materialises to an array")
            .clone()
    }

    /// THE write loop: append → the row is durable → the topic a component reads
    /// carries it. Without the rematerialise step the row would be in the
    /// database and invisible on the page, which is the failure this guards.
    #[tokio::test]
    async fn an_append_persists_and_rematerialises_the_topic() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();
        assert_eq!(topic_rows(&broadcast, "guestbook").len(), 2, "seeded rows");

        apply_writes(
            &db,
            &broadcast,
            &[append(
                "guestbook",
                json!({ "author": "grace", "message": "found the bug" }),
            )],
            None,
        )
        .await
        .unwrap();

        // Durable in the substrate…
        let rows = db
            .query("SELECT author, message FROM guestbook ORDER BY id", &[])
            .await
            .unwrap();
        assert_eq!(rows.rows.len(), 3);

        // …and visible in the topic the page renders from.
        let materialised = topic_rows(&broadcast, "guestbook");
        assert_eq!(materialised.len(), 3, "topic rematerialised after the write");
        assert_eq!(materialised[2]["author"], "grace");
        assert_eq!(materialised[2]["message"], "found the bug");
    }

    /// A value that looks like SQL must land as text, not execute. The unit test
    /// proves the statement is parameterised; this proves the backend agrees.
    #[tokio::test]
    async fn a_hostile_value_is_stored_as_data_and_the_table_survives() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let hostile = "'); DROP TABLE guestbook;--";
        apply_writes(
            &db,
            &broadcast,
            &[append(
                "guestbook",
                json!({ "author": hostile, "message": "x" }),
            )],
            None,
        )
        .await
        .unwrap();

        let rows = topic_rows(&broadcast, "guestbook");
        assert_eq!(rows.len(), 3, "table still exists and took the row");
        assert_eq!(rows[2]["author"], hostile, "stored verbatim as data");
    }

    /// One action's writes commit together. The second append is malformed, so
    /// the first must not survive on its own.
    #[tokio::test]
    async fn a_failed_write_rolls_back_the_whole_action() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let mut bad = serde_json::Map::new();
        bad.insert(
            "author) VALUES ('x'); DROP TABLE guestbook;--".to_string(),
            json!("x"),
        );

        let err = apply_writes(
            &db,
            &broadcast,
            &[
                append("guestbook", json!({ "author": "ok", "message": "first" })),
                ForgeWrite::Append {
                    collection: "guestbook".to_string(),
                    record: bad,
                },
            ],
            None,
        )
        .await
        .expect_err("a malformed record must fail the action");
        assert!(format!("{err}").contains("not a valid column name"));

        let rows = db.query("SELECT id FROM guestbook", &[]).await.unwrap();
        assert_eq!(
            rows.rows.len(),
            2,
            "the good append rolled back with the bad one"
        );
    }

    /// An unknown collection is refused before any lock is taken, and nothing
    /// about the store changes.
    #[tokio::test]
    async fn an_unknown_collection_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let err = apply_writes(
            &db,
            &broadcast,
            &[append("not_a_collection", json!({ "a": 1 }))],
            None,
        )
        .await
        .expect_err("only FORGE-backed collections are writable");
        assert!(format!("{err}").contains("not a FORGE-backed collection"));
        assert_eq!(
            topic_rows(&broadcast, "guestbook").len(),
            2,
            "store untouched"
        );
    }

    /// S4 · the beam, minus the transport. A subscriber must learn about an
    /// append as ONE row, not as a repainted collection — and the row it gets
    /// must be the row SSR would have rendered, keyed the way SSR keyed it.
    #[tokio::test]
    async fn an_append_fans_out_one_row_delta_beside_the_snapshot() {
        use crate::ir::opcode::{Instruction, RowKey};
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;

        /// The render path's stand-in: renders the whole collection the way SSR
        /// would, then hands back its keyed rows — including the round trip
        /// through the real markup reader, so this test exercises the same
        /// extraction the pooled projector uses.
        struct GuestbookRows;

        #[async_trait::async_trait]
        impl crate::forge::delta::RowProjector for GuestbookRows {
            async fn project_rows(
                &self,
                collection: &str,
                value: &[u8],
            ) -> Option<crate::forge::delta::RenderedRows> {
                if collection != "guestbook" {
                    return None;
                }
                let records: Value = serde_json::from_slice(value).ok()?;
                let mut html = String::from("<ul data-albedo-list-slot=\"guestbook\">");
                for record in records.as_array()? {
                    html.push_str(&format!(
                        "<li data-albedo-key=\"{}\">{}</li>",
                        record.get("id")?,
                        record.get("author")?.as_str()?
                    ));
                }
                html.push_str("</ul>");
                crate::transforms::shared_slot_lists::extract_keyed_rows(&html, collection)
            }
        }

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        apply_writes(
            &db,
            &broadcast,
            &[append(
                "guestbook",
                json!({ "author": "grace", "message": "found the bug" }),
            )],
            Some(&GuestbookRows),
        )
        .await
        .unwrap();

        let payload = rx.recv().await.expect("the write reaches the subscriber");
        let (frame, _) = decode_frame(&payload).unwrap();
        match frame.instructions.as_slice() {
            [Instruction::SlotSet { value, .. }, Instruction::SlotDelta { changes, .. }] => {
                // The snapshot is still the truth a reload would show…
                let snapshot: Value = serde_json::from_slice(value).unwrap();
                assert_eq!(snapshot.as_array().unwrap().len(), 3);

                // …and the delta is one row, not three.
                assert_eq!(changes.len(), 1, "an append must cost ONE row on the wire");
                assert_eq!(changes[0].weight, 1);
                assert_eq!(changes[0].key, RowKey("3".to_string()));
                assert_eq!(
                    String::from_utf8(changes[0].payload.clone()).unwrap(),
                    "<li data-albedo-key=\"3\">grace</li>"
                );
            }
            other => panic!("expected [SlotSet, SlotDelta], got {other:?}"),
        }
    }

    /// No projector (today's serve path) must behave exactly as it did before
    /// the delta lane existed: snapshot only, nothing row-shaped on the wire.
    #[tokio::test]
    async fn without_a_projector_the_write_falls_back_to_a_snapshot() {
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        apply_writes(
            &db,
            &broadcast,
            &[append(
                "guestbook",
                json!({ "author": "grace", "message": "x" }),
            )],
            None,
        )
        .await
        .unwrap();

        let payload = rx.recv().await.unwrap();
        let (frame, _) = decode_frame(&payload).unwrap();
        assert_eq!(frame.instructions.len(), 1, "snapshot-only fan-out");
    }

    /// Writes survive a reopen — the point of durability, and the property the
    /// idempotent seed exists to protect.
    #[tokio::test]
    async fn an_appended_row_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forge.db");

        {
            let db = LibSqlSubstrate::open_local(&path).await.unwrap();
            bootstrap_schema(&db).await.unwrap();
            let broadcast = BroadcastRegistry::new();
            hydrate_topics(&db, &broadcast).await.unwrap();
            apply_writes(
                &db,
                &broadcast,
                &[append(
                    "guestbook",
                    json!({ "author": "ada", "message": "again" }),
                )],
                None,
            )
            .await
            .unwrap();
        }

        // Fresh process-shaped boot: reopen, re-bootstrap (must not re-seed),
        // rehydrate — the appended row has to come back.
        let db = LibSqlSubstrate::open_local(&path).await.unwrap();
        bootstrap_schema(&db).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast).await.unwrap();

        let rows = topic_rows(&broadcast, "guestbook");
        assert_eq!(rows.len(), 3, "the appended row survived the restart");
        assert_eq!(rows[2]["message"], "again");
    }
}
