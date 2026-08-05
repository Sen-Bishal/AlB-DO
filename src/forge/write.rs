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

use crate::forge::delta::{
    appended_rows, classify_positioned_insert, diff_records, is_tail_append, project_changes,
    project_inserted_rows, RenderedRows, RowProjector,
};
use crate::transforms::shared_slot_lists::RowProjection;
use crate::forge::skeleton::{materialize_slot, ForgeCollection, ForgeSchema, SortDir};
use crate::forge::substrate::DataSubstrate;
use crate::forge::value::{Result, SqlValue, SubstrateError};
use crate::ir::opcode::{ReconcileRow, RowKey, SlotChange};
use crate::runtime::broadcast::{BroadcastRegistry, ListUpdate, TopicTransition};
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};

/// One durable mutation requested by an action body.
///
/// The three variants are the *same* loop (mutate → rematerialise → fan out)
/// differing only in the statement built — which is the whole point of the z-set
/// delta path: an `Update` diffs to `−old, +new` under one key (an in-place
/// patch on the wire), a `Delete` to a lone `−` (a keyed removal), and neither
/// needs machinery beyond the row-level diff [`crate::forge::delta`] already
/// performs. `key` identifies the row by the collection's `key_column` (see
/// [`ForgeCollection`]); the column *name* never crosses from
/// userland — it is resolved from the allowlist here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeWrite {
    /// Append a record to a persistent collection. `collection` is the topic
    /// key the component reads via `useSharedSlot`.
    Append {
        collection: String,
        record: Map<String, Value>,
    },
    /// Update the row identified by `key`, setting the columns in `fields`.
    /// `fields` is a partial record — only the columns it names change.
    Update {
        collection: String,
        key: Value,
        fields: Map<String, Value>,
    },
    /// Delete the row identified by `key`.
    Delete { collection: String, key: Value },
}

impl ForgeWrite {
    /// The collection this write targets — the topic that must be
    /// rematerialised once it commits.
    #[must_use]
    pub fn collection(&self) -> &str {
        match self {
            Self::Append { collection, .. }
            | Self::Update { collection, .. }
            | Self::Delete { collection, .. } => collection.as_str(),
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
///
/// `pub(crate)` so [`crate::forge::skeleton::ForgeSchema::build`] can apply the
/// same rule to app-declared table/key-column names at schema-build time — one
/// definition of "safe SQL identifier" for the whole FORGE plane.
pub(crate) fn is_safe_identifier(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

/// [`build_append`] with a `RETURNING` list matching the collection's own
/// `SELECT`, so the insert hands back the row it actually persisted.
///
/// This is the piece that makes a zero-query write possible. An append's key is
/// assigned by the database (`INTEGER PRIMARY KEY AUTOINCREMENT`), so before
/// this the only way to learn the persisted row was to re-read the collection —
/// which is precisely the query PRISM wants to stop running. `RETURNING` is the
/// right tool *here* (unlike on `UPDATE`, where it reports post-update values
/// and loses the origin partition — see [`read_partition`]): an insert has no
/// "before", so the returned row is unambiguous.
fn build_append_returning(
    table: &str,
    key_column: &str,
    record: &Map<String, Value>,
) -> Result<(String, Vec<SqlValue>, Vec<String>)> {
    let (sql, params) = build_append(table, record)?;
    // Same column set the materialising query selects: the key, then every
    // written column. Byte-agreement with `materialize_slot` depends on the
    // *names* matching, not the order — `serde_json::Map` is a `BTreeMap`, so
    // both sides serialize alphabetically regardless.
    let mut returned: Vec<String> = vec![key_column.to_string()];
    returned.extend(record.keys().cloned());
    Ok((
        format!("{sql} RETURNING {}", returned.join(", ")),
        params,
        returned,
    ))
}

/// Turn one `RETURNING` row into the JSON object shape the materialised array
/// carries, so a spliced row and a queried row are indistinguishable.
///
/// `fields` are the collection's declared types, and passing them is what keeps
/// that promise true for a `bool`: the full materialisation renders it `true`,
/// so a row spliced in by this path has to as well, or the same record would
/// look different depending on whether it arrived by write or by re-read.
fn returned_record(
    columns: &[String],
    row: &crate::forge::value::Row,
    fields: &std::collections::BTreeMap<String, crate::forge::declare::FieldSpec>,
) -> Option<Value> {
    let mut object = Map::new();
    for (index, column) in columns.iter().enumerate() {
        let stored = row.get(index)?;
        // Blobs have no JSON form and no declared type that produces them;
        // refusing keeps the fast path from inventing one.
        if matches!(stored, SqlValue::Blob(_)) {
            return None;
        }
        let value = crate::forge::skeleton::column_value_to_json(
            stored,
            fields.get(column).map(|spec| spec.ty),
        );
        object.insert(column.clone(), value);
    }
    Some(Value::Object(object))
}

/// Lower a row-identity value to a bound [`SqlValue`], refusing the shapes that
/// can't identify a row.
///
/// A null key would compile to `WHERE k = NULL`, which SQL never matches — so an
/// update or delete with a null key is a silent no-op, the worst outcome for a
/// mutation. Reject it, and objects/arrays, loudly instead.
fn key_to_sqlvalue(collection: &str, key_column: &str, key: &Value) -> Result<SqlValue> {
    match key {
        Value::Null => Err(SubstrateError::Backend(format!(
            "FORGE write: '{collection}' key ('{key_column}') is null; \
             a mutation must identify exactly one row"
        ))),
        scalar => json_to_sqlvalue(key_column, scalar),
    }
}

/// Build the `UPDATE` for one row, with both the new field values and the key
/// bound. `key_column` comes from the collection's [`ForgeCollection`], never from
/// userland.
fn build_update(
    collection: &str,
    key_column: &str,
    key: &Value,
    fields: &Map<String, Value>,
) -> Result<(String, Vec<SqlValue>)> {
    if fields.is_empty() {
        return Err(SubstrateError::Backend(format!(
            "FORGE update: no fields to set for '{collection}'; nothing to change"
        )));
    }

    let mut assignments = Vec::with_capacity(fields.len());
    let mut params = Vec::with_capacity(fields.len() + 1);
    for (index, (column, value)) in fields.iter().enumerate() {
        if !is_safe_identifier(column) {
            return Err(SubstrateError::Backend(format!(
                "FORGE update: '{column}' is not a valid column name"
            )));
        }
        // Refuse an update that would rewrite the row's identity: the delta path
        // keys on `key_column`, so changing it under an action would strand the
        // row on every client (its DOM node is addressed by the old key). A key
        // change is a delete + append, and the author should say so.
        if column == key_column {
            return Err(SubstrateError::Backend(format!(
                "FORGE update: cannot change the key column '{key_column}' of '{collection}'; \
                 delete and re-append instead"
            )));
        }
        assignments.push(format!("{column} = ?{}", index + 1));
        params.push(json_to_sqlvalue(column, value)?);
    }
    params.push(key_to_sqlvalue(collection, key_column, key)?);

    let sql = format!(
        "UPDATE {collection} SET {} WHERE {key_column} = ?{}",
        assignments.join(", "),
        params.len()
    );
    Ok((sql, params))
}

/// Build the `DELETE` for one row, with the key bound.
fn build_delete(
    collection: &str,
    key_column: &str,
    key: &Value,
) -> Result<(String, Vec<SqlValue>)> {
    let params = vec![key_to_sqlvalue(collection, key_column, key)?];
    let sql = format!("DELETE FROM {collection} WHERE {key_column} = ?1");
    Ok((sql, params))
}

/// One channel a write touches: a collection, and — when that collection is
/// partitioned — which partition of it.
///
/// A write used to touch a *collection*; it now touches a **channel**. Keeping
/// both here matters because they are read by different things: the row template
/// belongs to the collection (`projection_class`, `project_rows` key off
/// `slot.topic`), while the subscriber set belongs to the partition.
struct Touched<'a> {
    slot: &'a ForgeCollection,
    /// The bound partition key, `None` for an unpartitioned collection.
    partition: Option<String>,
    /// The broadcast channel name — `slot.topic`, or the minted partition name.
    channel: String,
    /// Rows this action appended to this channel, exactly as the database
    /// persisted them (`INSERT … RETURNING`), in insertion order.
    ///
    /// When this is the *whole* story for the channel — appends only, nothing
    /// updated or deleted — the new value can be spliced from the previous one
    /// and no query is needed at all.
    appended: Vec<Value>,
    /// Set when anything other than an append touched this channel. An update or
    /// a delete rewrites rows in place, which a tail/head splice cannot express,
    /// so the channel falls back to re-reading its partition.
    mutated: bool,
}

/// Splice appended rows onto the previous materialised value, without parsing
/// either side.
///
/// Correct only when the collection's ordering places a new row at one end and
/// the rows arrive in that order — which is exactly what `sort` proves. `None`
/// means "cannot place these", and the caller re-reads instead.
///
/// The output must be **byte-identical** to what `materialize_slot` would have
/// produced, or an unpartitioned collection would observe P2 — that equality is
/// asserted directly by a test.
fn splice_appended(previous: &[u8], records: &[Value], at_head: bool) -> Option<Vec<u8>> {
    if records.is_empty() {
        return Some(previous.to_vec());
    }
    // A topic registered but never materialised holds `null`; there is no array
    // to splice onto, so let the caller materialise.
    if previous.len() < 2 || previous.first() != Some(&b'[') || previous.last() != Some(&b']') {
        return None;
    }
    let inner = &previous[1..previous.len() - 1];

    let mut encoded = Vec::with_capacity(records.len());
    for record in records {
        encoded.push(serde_json::to_vec(record).ok()?);
    }
    let joined = encoded.join(&b',');

    let mut out = Vec::with_capacity(previous.len() + joined.len() + 1);
    out.push(b'[');
    if inner.is_empty() {
        out.extend_from_slice(&joined);
    } else if at_head {
        out.extend_from_slice(&joined);
        out.push(b',');
        out.extend_from_slice(inner);
    } else {
        out.extend_from_slice(inner);
        out.push(b',');
        out.extend_from_slice(&joined);
    }
    out.push(b']');
    Some(out)
}

/// Where the collection's ordering puts a freshly inserted row, when that is
/// knowable at all.
///
/// `Some(true)` = head, `Some(false)` = tail, `None` = anywhere (so a splice
/// would be a guess). Only an ordering whose leading key is the row-identity
/// column qualifies: `id` ascending appends at the tail, `id DESC` at the head.
/// An ordering on any other column — `score desc` — places a row by its value,
/// which is the reorder case a splice cannot express.
fn insert_at_head(slot: &ForgeCollection) -> Option<bool> {
    let first = slot.sort.first()?;
    if first.column != slot.key_column {
        return None;
    }
    Some(matches!(first.dir, SortDir::Desc))
}

/// The partition value an `Append` lands in, read off the record it is inserting.
fn append_partition(slot: &ForgeCollection, record: &Map<String, Value>) -> Result<Option<String>> {
    let Some(column) = &slot.partition_by else {
        return Ok(None);
    };
    let value = record.get(column).ok_or_else(|| {
        SubstrateError::Backend(format!(
            "FORGE append: collection '{}' is partitioned by '{column}', but the record does not \
             set it — an append with no partition would be invisible to every reader",
            slot.topic
        ))
    })?;
    Ok(Some(partition_value_to_key(&slot.topic, column, value)?))
}

/// Render a partition value as the key that names its channel.
///
/// Text and integers only: those are what a URL segment and a declared column
/// can both express, and the key has to survive a round trip through a topic
/// name unchanged.
fn partition_value_to_key(topic: &str, column: &str, value: &Value) -> Result<String> {
    let key = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) if number.is_i64() => number.to_string(),
        other => {
            return Err(SubstrateError::Backend(format!(
                "FORGE write: partition column '{column}' of '{topic}' must be text or an \
                 integer; got {other}"
            )))
        }
    };
    if !crate::runtime::broadcast::is_valid_partition_key(&key) {
        return Err(SubstrateError::Backend(format!(
            "FORGE write: partition key {key:?} for '{topic}' is outside the permitted \
             alphabet ([A-Za-z0-9_-], 1..=64)"
        )));
    }
    Ok(key)
}

/// Read the partition a row currently sits in, **before** it is changed.
///
/// Deliberately a `SELECT` rather than `RETURNING`, which is what PRISM's draft
/// called for. `RETURNING` on an `UPDATE` yields the row's **post**-update
/// values, so for the one case that actually needs care — an update that moves a
/// row between partitions — it reports the destination and the origin is lost.
/// The origin is exactly what the old partition's subscribers need in order to
/// be told the row left. One indexed read by primary key inside the transaction
/// buys correctness for both `Update` and `Delete` with no substrate-capability
/// dependency, which is also why the `RETURNING`-not-honoured fallback
/// (`reserve.rs:266`) is moot here.
async fn read_partition(
    tx: &dyn crate::forge::substrate::Transaction,
    slot: &ForgeCollection,
    key: &Value,
) -> Result<Option<String>> {
    let Some(column) = &slot.partition_by else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {column} FROM {} WHERE {} = ?1",
        slot.table, slot.key_column
    );
    let bound = key_to_sqlvalue(&slot.topic, &slot.key_column, key)?;
    let rows = tx.query(&sql, &[bound]).await?;
    let Some(row) = rows.rows.first() else {
        // The row is gone (or never existed). Not an error: the mutation below
        // will simply affect nothing, and there is no partition to notify.
        return Ok(None);
    };
    match row.get(0) {
        Some(SqlValue::Text(text)) => Ok(Some(partition_value_to_key(
            &slot.topic,
            column,
            &Value::String(text.clone()),
        )?)),
        Some(SqlValue::Integer(number)) => Ok(Some(partition_value_to_key(
            &slot.topic,
            column,
            &Value::Number((*number).into()),
        )?)),
        other => Err(SubstrateError::Backend(format!(
            "FORGE write: partition column '{column}' of '{}' read back as {other:?}, which is \
             not a usable key",
            slot.topic
        ))),
    }
}

/// Record a channel this action touched, minting the partition name through the
/// single resolver so the write path cannot invent a name the render and
/// subscribe paths would not produce.
fn note_touched<'a>(
    touched: &mut Vec<Touched<'a>>,
    slot: &'a ForgeCollection,
    partition: Option<String>,
) -> Result<()> {
    let channel = match (&slot.partition_by, &partition) {
        (Some(_), Some(key)) => {
            crate::runtime::broadcast::partition_topic_name(&slot.topic, key).ok_or_else(|| {
                SubstrateError::Backend(format!(
                    "FORGE write: partition key {key:?} for '{}' cannot name a channel",
                    slot.topic
                ))
            })?
        }
        // A partitioned write whose row vanished: nothing to notify.
        (Some(_), None) => return Ok(()),
        _ => slot.topic.clone(),
    };
    if !touched.iter().any(|known| known.channel == channel) {
        touched.push(Touched {
            slot,
            partition,
            channel,
            appended: Vec::new(),
            mutated: false,
        });
    }
    Ok(())
}

/// Record what a write did to its channel, so the fan-out knows whether the new
/// value can be spliced or has to be re-read.
fn record_outcome(touched: &mut [Touched<'_>], channel: &str, appended: Option<Value>) {
    if let Some(entry) = touched.iter_mut().find(|known| known.channel == channel) {
        match appended {
            Some(record) => entry.appended.push(record),
            None => entry.mutated = true,
        }
    }
}

/// Build the statement for one write against its resolved slot. Dispatches on
/// the variant; the slot supplies the `&'static` collection name and key column,
/// so no userland string reaches the SQL as an identifier.
fn build_statement(
    slot: &ForgeCollection,
    write: &ForgeWrite,
) -> Result<(String, Vec<SqlValue>)> {
    match write {
        // `slot.table`, not `slot.topic`. They coincide unless the declaration
        // overrides `table:`, which `CollectionDecl` explicitly allows — and
        // until now every write targeted a table named after the collection, so
        // an override produced statements against a table that does not exist.
        // The read path always used `table` (it is baked into `query`), so only
        // writes were affected.
        ForgeWrite::Append { record, .. } => build_append(&slot.table, record),
        ForgeWrite::Update { key, fields, .. } => {
            build_update(&slot.table, &slot.key_column, key, fields)
        }
        ForgeWrite::Delete { key, .. } => build_delete(&slot.table, &slot.key_column, key),
    }
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
    schema: &ForgeSchema,
    writes: &[ForgeWrite],
    projector: Option<&dyn RowProjector>,
) -> Result<()> {
    if writes.is_empty() {
        return Ok(());
    }

    // Temporary write-path instrumentation (env-gated, off by default): times the
    // four phases per touched slot to `stderr`. Gated so it costs nothing in the
    // hot path unless `ALBEDO_FORGE_TIMING` is set. Remove once the design call is
    // made from the numbers.
    let timing = std::env::var_os("ALBEDO_FORGE_TIMING").is_some();

    // Resolve every collection against the schema BEFORE opening the
    // transaction: an unknown collection is an authoring error, and it should
    // not cost a write lock or leave a half-built transaction to roll back.
    let mut slots: Vec<&ForgeCollection> = Vec::new();
    for write in writes {
        let slot = schema.slot_for_topic(write.collection()).ok_or_else(|| {
            SubstrateError::Backend(format!(
                "FORGE write: '{}' is not a FORGE-backed collection",
                write.collection()
            ))
        })?;
        if !slots.iter().any(|known| known.topic == slot.topic) {
            slots.push(slot);
        }
    }

    // Channels touched by this action, accumulated as the writes are applied.
    // An `Append` knows its partition from its own record; `Update`/`Delete`
    // have to read it out of the row before they change it, so this cannot be
    // fully resolved before the transaction opens.
    let mut touched: Vec<Touched<'_>> = Vec::new();

    let t_commit = std::time::Instant::now();
    let tx = substrate.begin().await?;
    for write in writes {
        // The slot is borrowed from the schema, resolved in `touched` above —
        // the collection name and key column reach SQL from here, never from the
        // userland string.
        let slot = slots
            .iter()
            .find(|slot| slot.topic == write.collection())
            .copied()
            .expect("collection was resolved above");

        // Learn which channel(s) this write moves, before it moves them.
        let resolved = match write {
            ForgeWrite::Append { record, .. } => append_partition(slot, record).map(|p| vec![p]),
            ForgeWrite::Update { key, fields, .. } => match read_partition(tx.as_ref(), slot, key).await {
                // A partition-CHANGING update touches two channels: the row
                // leaves one and arrives in the other, and each side's
                // subscribers need to hear about their half. Naming it here is
                // the difference between "the row moved" and "the row vanished".
                Ok(before) => match (&slot.partition_by, before) {
                    (Some(column), before) => match fields.get(column) {
                        Some(moved) => partition_value_to_key(&slot.topic, column, moved)
                            .map(|after| vec![before, Some(after)]),
                        None => Ok(vec![before]),
                    },
                    (None, _) => Ok(vec![None]),
                },
                Err(err) => Err(err),
            },
            ForgeWrite::Delete { key, .. } => {
                read_partition(tx.as_ref(), slot, key).await.map(|p| vec![p])
            }
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(err) => {
                let _ = tx.rollback().await;
                return Err(err);
            }
        };
        for partition in resolved {
            if let Err(err) = note_touched(&mut touched, slot, partition) {
                let _ = tx.rollback().await;
                return Err(err);
            }
        }

        // The channel(s) this write just resolved onto. An append records the row
        // the database persisted; anything else marks the channel as mutated,
        // which forces the fan-out to re-read rather than splice.
        let channels: Vec<String> = touched
            .iter()
            .filter(|known| known.slot.topic == slot.topic)
            .map(|known| known.channel.clone())
            .collect();

        match write {
            ForgeWrite::Append { record, .. } => {
                let (sql, params, columns) =
                    match build_append_returning(&slot.table, &slot.key_column, record) {
                        Ok(built) => built,
                        Err(err) => {
                            let _ = tx.rollback().await;
                            return Err(err);
                        }
                    };
                let rows = match tx.query(&sql, &params).await {
                    Ok(rows) => rows,
                    Err(err) => {
                        let _ = tx.rollback().await;
                        return Err(err);
                    }
                };
                // A substrate that does not honour `RETURNING` yields no row.
                // That is not an error — it only costs the zero-query path, and
                // the fan-out falls back to re-reading (`reserve.rs:266` records
                // the same defensive posture).
                let persisted = rows
                    .rows
                    .first()
                    .and_then(|row| returned_record(&columns, row, &slot.fields));
                for channel in &channels {
                    match &persisted {
                        Some(record) => {
                            record_outcome(&mut touched, channel, Some(record.clone()))
                        }
                        None => record_outcome(&mut touched, channel, None),
                    }
                }
            }
            _ => {
                let (sql, params) = match build_statement(slot, write) {
                    Ok(built) => built,
                    Err(err) => {
                        // Drop the whole action's writes: one malformed mutation
                        // must not leave earlier ones committed.
                        let _ = tx.rollback().await;
                        return Err(err);
                    }
                };
                if let Err(err) = tx.execute(&sql, &params).await {
                    let _ = tx.rollback().await;
                    return Err(err);
                }
                for channel in &channels {
                    record_outcome(&mut touched, channel, None);
                }
            }
        }
    }
    tx.commit().await?;
    let commit_el = t_commit.elapsed();

    // Durable now — safe to tell the world.
    for Touched {
        slot,
        partition,
        channel,
        appended,
        mutated,
    } in touched
    {
        // Both awaits happen HERE, outside the topic's critical section:
        // `write_topic_delta`'s closure runs under that topic's lock, so
        // awaiting in it would serialise every writer behind the slowest query
        // — and a projector that reached for the topic's value would deadlock
        // on the very lock it is running under. What survives into the closure
        // is only the diff: pure, bounded by the collection size, and the one
        // step that genuinely needs the pre-state it is replacing.
        let t_mat = std::time::Instant::now();
        // PRISM § 6.2 · the write path stops re-running the query.
        //
        // An append already knows its record (from `RETURNING`) and its
        // partition, and `PerRecord` proves the row's markup is a function of
        // that record alone. So when nothing but appends touched this channel
        // and the ordering places a new row at one end, the new value is the old
        // one with the rows spliced in — a memcpy — and the rows are rendered
        // from the records directly. Cost becomes O(1) in the partition size
        // instead of O(rows in it).
        //
        // Every other shape (an update, a delete, an ordering that places rows
        // by value, a substrate without `RETURNING`, a topic never materialised)
        // falls back to the query. The fallback is always correct, so this can
        // only ever cost time, never truth.
        let splice_at_head = insert_at_head(slot);
        let can_splice = !mutated && !appended.is_empty() && splice_at_head.is_some();
        let spliced = if can_splice {
            let at_head = splice_at_head.unwrap_or(false);
            broadcast.get(&channel).and_then(|topic| {
                // Read the cached bytes in place. Cloning them first would copy
                // the whole partition just to derive the next version of it —
                // the splice is exactly the operation that does not need to own
                // its input.
                topic.with_value(|previous| splice_appended(previous, &appended, at_head))
            })
        } else {
            None
        };
        let queried = spliced.is_none();
        let bytes = match spliced {
            Some(bytes) => bytes,
            None => materialize_slot(substrate, slot, partition.as_deref()).await?,
        };
        let mat_el = t_mat.elapsed();

        // Choose what to render. A `PerRecord` collection renders only the rows
        // this write changed — each over a singleton collection, `O(1)` in the
        // view size — because its row template is a proven function of its record
        // alone (the transpile pre-pass classified it; see `RowProjection`). Every
        // other class renders the whole view exactly as before. `partial` marks a
        // render that covers only the changed keys: a reconcile needs every row,
        // so if the classified fast path can't satisfy one it re-renders after the
        // lock (`needs_whole` below).
        let t_proj = std::time::Instant::now();
        let (rows, partial) = match projector {
            Some(p) if p.projection_class(&slot.topic) == RowProjection::PerRecord => {
                match render_changed_rows(p, slot, &channel, partition.as_deref(), broadcast, &bytes).await {
                    // Changed rows attributed and rendered: the fast path.
                    Some(changed) => (Some(changed), true),
                    // No prior value to diff against, or unkeyable rows: fall back
                    // to the whole view, which is always renderable and correct.
                    None => (p.project_rows(&slot.topic, partition.as_deref(), &bytes).await, false),
                }
            }
            Some(p) => (p.project_rows(&slot.topic, partition.as_deref(), &bytes).await, false),
            None => (None, false),
        };
        let proj_el = t_proj.elapsed();
        let row_count = rows.as_ref().map(|r| r.len()).unwrap_or(0);

        // Dev correctness gate: prove the singleton-rendered rows are byte-identical
        // to the whole-view render's slice of them. A `PerRecord` misclassification
        // would surface here loudly rather than as a stranded row in production.
        if partial && std::env::var_os("ALBEDO_FORGE_VERIFY").is_some() {
            if let (Some(p), Some(changed)) = (projector, rows.as_ref()) {
                if let Some(whole) = p.project_rows(&slot.topic, partition.as_deref(), &bytes).await {
                    for (key, html) in changed.iter() {
                        match whole.get(key) {
                            Some(expected) if expected == html => {}
                            other => eprintln!(
                                "[forge-verify] DIVERGENCE topic={} key={} \
                                 singleton={:?} whole={:?}",
                                slot.topic, key, html, other
                            ),
                        }
                    }
                }
            }
        }

        let t_fan = std::time::Instant::now();
        // The channel, not the collection: a partitioned write fans out on its
        // own partition's topic. `projection_class` / `project_rows` above stay
        // keyed by `slot.topic`, because the row TEMPLATE belongs to the
        // collection while the subscriber set belongs to the partition.
        // Register the channel if this is the first anyone has heard of it. The
        // `initial` argument is ignored when the topic already exists, so
        // cloning the value unconditionally allocated a full copy of the
        // partition and dropped it again on **every** write to a warm topic —
        // the common case, and pure waste.
        if broadcast.get(&channel).is_none() {
            broadcast.topic(channel.clone(), bytes.clone());
        }
        // `needs_whole` is raised inside the closure when a partial render can't
        // express the transition (a reorder, a mid-list insert, or a race that
        // moved `previous` out from under the changed-set guess). The reconcile is
        // then shipped after the lock, off the critical section.
        let needs_whole = Cell::new(false);
        let _ = broadcast.write_topic_delta(&channel, |previous| {
            let update = match rows.as_ref() {
                Some(rows) => row_update(slot, previous, &bytes, rows, partial, &needs_whole),
                None => ListUpdate::None,
            };
            TopicTransition {
                value: bytes.clone(),
                update,
            }
        });
        let fan_el = t_fan.elapsed();

        // Rare, off-lock: the changed-only render could not satisfy a reconcile.
        // Render the whole view now and ship the full ordered set so keyed anchors
        // reach the new order. The `SlotSet` value already went out above, so a
        // reload or late joiner is already correct; this repairs live rows.
        if needs_whole.get() {
            if let Some(p) = projector {
                if let Some(whole) = p.project_rows(&slot.topic, partition.as_deref(), &bytes).await {
                    let _ = broadcast.write_topic_delta(&channel, |_previous| TopicTransition {
                        value: bytes.clone(),
                        update: ListUpdate::Reconcile(reconcile_rows(&whole)),
                    });
                }
            }
        }

        if timing {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
            eprintln!(
                "[forge-timing] channel={} rows={} partial={} value={} commit={:.3}ms                  materialize={:.3}ms project={:.3}ms fanout={:.3}ms",
                channel,
                row_count,
                partial,
                // The number this instrumentation exists to move: `spliced` means
                // the write never touched the database to learn its new value.
                if queried { "queried" } else { "spliced" },
                ms(commit_el),
                ms(mat_el),
                ms(proj_el),
                ms(fan_el),
            );
        }
    }

    // PRISM § 8 · the one cap. Swept here rather than inside the registry
    // because this is where the bytes grew, and because the sweep has to run
    // with no topic lock held — `write_topic_delta` holds one across its whole
    // critical section, and reading every topic's length from inside that would
    // deadlock against itself.
    //
    // Reclaiming an idle partition costs the next reader one query and nothing
    // else: the substrate is the truth and the value is a cache. That is the
    // whole licence for this, and it is why static topics are excluded.
    broadcast.enforce_byte_budget(crate::runtime::broadcast::DEFAULT_TOPIC_VALUE_BUDGET);

    Ok(())
}

/// Render only the records a write changed, each over a **singleton** collection
/// `[record]`, keyed as the whole-view render would key them.
///
/// The changed set is derived by diffing the topic's current broadcast value
/// against `next` (the freshly materialised bytes). That read is racy — a
/// concurrent write may have advanced the value since — but it is only a *guess*
/// at what to pre-render: the authoritative diff runs later under the topic lock
/// ([`row_update`]), and any changed key this guess failed to render collapses
/// that classification to a whole-view reconcile. So the guess can only cost a
/// fallback, never a wrong row.
///
/// Returns `None` when the changed set can't be attributed — no prior value to
/// diff against (the topic isn't registered yet, or holds the `null`
/// placeholder), or rows that can't be keyed — leaving the caller to render the
/// whole view.
async fn render_changed_rows(
    projector: &dyn RowProjector,
    slot: &ForgeCollection,
    channel: &str,
    partition: Option<&str>,
    broadcast: &BroadcastRegistry,
    next: &[u8],
) -> Option<RenderedRows> {
    // The previous value belongs to the CHANNEL — this partition's rows, not the
    // whole collection's. Diffing against the collection would attribute every
    // other partition's rows as "changed".
    let previous = broadcast.get(channel)?.current_value();

    // Intent-path shortcut: when `next` is provably `previous` plus a tail, the
    // appended records ARE the changed set. No parse of either full array, no
    // diff — the guess costs `O(|Δ|)` instead of `O(|view|)`.
    if let Some(appended) = appended_rows(&previous, next, &slot.key_column) {
        let mut rows = RenderedRows::new();
        for (key, record) in &appended {
            let singleton = serde_json::to_vec(&Value::Array(vec![record.clone()])).ok()?;
            let rendered = projector.project_rows(&slot.topic, partition, &singleton).await?;
            rows.insert(key.clone(), rendered.get(key)?.clone());
        }
        return Some(rows);
    }

    let previous: Value = serde_json::from_slice(&previous).ok()?;
    let next_json: Value = serde_json::from_slice(next).ok()?;
    let changes = diff_records(&previous, &next_json, &slot.key_column)?;

    let mut rows = RenderedRows::new();
    for change in &changes {
        // A retraction carries no row to render — its wire payload is empty, and
        // `project_changes` never reads a rendered row for a `−` change.
        if change.weight < 0 {
            continue;
        }
        // The row template applied to this record alone. For a `PerRecord`
        // template the singleton render is byte-identical to the whole-view
        // render's slice of this row (the classifier's guarantee).
        let singleton = serde_json::to_vec(&Value::Array(vec![change.record.clone()])).ok()?;
        let rendered = projector.project_rows(&slot.topic, partition, &singleton).await?;
        let html = rendered.get(&change.key)?.clone();
        rows.insert(change.key.clone(), html);
    }
    Some(rows)
}

/// The full desired row set as [`ReconcileRow`]s, in render order — the payload
/// of a [`ListUpdate::Reconcile`].
fn reconcile_rows(rows: &RenderedRows) -> Vec<ReconcileRow> {
    rows.iter()
        .map(|(key, html)| ReconcileRow {
            key: RowKey(key.clone()),
            payload: html.clone().into_bytes(),
        })
        .collect()
}

/// The list half of one collection's fan-out: classify the previous
/// materialised value against the new one and choose the cheapest wire shape
/// that reproduces it.
///
/// An order-preserving tail append ships as an `O(|Δ|)` [`ListUpdate::Delta`];
/// anything a tail append cannot express — a reorder, a mid-list insert, or a
/// first write off a non-array `previous` (the `b"null"` placeholder) — ships
/// the full ordered set as a [`ListUpdate::Reconcile`]. The reconcile is
/// `O(|view|)` on the wire but the only shape that carries position, and it is
/// always correct, so it is the fallback whenever the delta cannot be trusted.
///
/// `rows` is the projection of `next`, taken before the lock. When `partial` is
/// false it holds every row in render order, so it *is* the reconcile payload,
/// no re-parse. When `partial` is true it holds only the rows the write changed
/// (the `PerRecord` fast path): a `Delta` — which references only changed keys —
/// is served from it directly, but a reconcile, which needs every row, cannot
/// be. In that case this raises `needs_whole` and returns [`ListUpdate::None`];
/// the caller renders the whole view off the lock and ships the reconcile then.
///
/// In the window between that render and this classification another action may
/// have committed; then `previous` is *its* value, the delta describes `their
/// state → our (older) materialisation`, and the snapshot beside it still agrees,
/// so no client diverges and the next write re-converges everyone. Making the
/// whole commit-materialise-fan-out sequence atomic per collection is the real
/// fix and belongs with the substrate, not here.
fn row_update(
    slot: &ForgeCollection,
    previous: &[u8],
    next: &[u8],
    rows: &RenderedRows,
    partial: bool,
    needs_whole: &Cell<bool>,
) -> ListUpdate {
    // The always-correct fallback: the full desired set in render order. When
    // `rows` is only the changed subset it cannot express this, so defer to the
    // caller's off-lock whole-view render instead.
    let reconcile = || {
        if partial {
            needs_whole.set(true);
            return ListUpdate::None;
        }
        ListUpdate::Reconcile(
            rows.iter()
                .map(|(key, html)| ReconcileRow {
                    key: RowKey(key.clone()),
                    payload: html.clone().into_bytes(),
                })
                .collect(),
        )
    };

    // Intent-path shortcut, before either array is parsed: a byte-proven tail
    // append is a tail append by construction, inserting exactly these keys and
    // touching nothing else. This is the common write, and it is the whole point
    // of item 3.3 — the classification below re-derives from two full arrays what
    // the append already knew.
    if let Some(appended) = appended_rows(previous, next, &slot.key_column) {
        if appended.is_empty() {
            // `next` is byte-identical to `previous` — nothing to reconcile.
            return ListUpdate::None;
        }
        let mut changes = Vec::with_capacity(appended.len());
        for (key, _record) in &appended {
            let Some(html) = rows.get(key) else {
                // The render never produced an appended row: same disagreement
                // `project_changes` refuses on, same answer.
                return reconcile();
            };
            changes.push(SlotChange {
                weight: 1,
                key: RowKey(key.clone()),
                payload: html.clone().into_bytes(),
            });
        }
        return ListUpdate::Delta(changes);
    }

    let (Ok(previous), Ok(next)) = (
        serde_json::from_slice::<Value>(previous),
        serde_json::from_slice::<Value>(next),
    ) else {
        // `previous` is the empty-bytes placeholder — no trustworthy pre-state,
        // so establish the rows with a full reconcile.
        return reconcile();
    };

    if !is_tail_append(&previous, &next, &slot.key_column) {
        // Not a tail append — but a run of rows inserted at one place, with
        // nothing else touched, is still `O(|Δ|)` now that the wire can name the
        // anchor. This is the reverse-chron case: a `created_at DESC` feed puts
        // every new row at the head, which used to re-assert the whole view on
        // every single write.
        //
        // Deliberately reachable from the `PerRecord` fast path: it reads only
        // the inserted rows, which is exactly what a partial render holds, so it
        // never raises `needs_whole`. That is what makes a head insert O(1) to
        // render *and* O(|Δ|) on the wire.
        if let Some(insert) = classify_positioned_insert(&previous, &next, &slot.key_column) {
            if let Some(rows) = project_inserted_rows(&insert, rows) {
                return ListUpdate::Insert {
                    before: insert.before.map(RowKey),
                    rows,
                };
            }
        }
        // A reorder, or an update tangled with an insert: only the full ordered
        // set carries this.
        return reconcile();
    }

    match diff_records(&previous, &next, &slot.key_column) {
        Some(changes) if changes.is_empty() => ListUpdate::None,
        Some(changes) => match project_changes(&changes, rows) {
            Some(slot_changes) => ListUpdate::Delta(slot_changes),
            // The diff insists on a row the render never produced: either the
            // whole-view render and the diff disagree, or the changed-set guess
            // missed a key. Ship the whole set (or defer to it) rather than a
            // partial delta.
            None => reconcile(),
        },
        None => reconcile(),
    }
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
        assert!(record_forge_write(append(
            "guestbook",
            json!({ "author": "ada" })
        )));
        assert!(record_forge_write(append(
            "guestbook",
            json!({ "author": "alan" })
        )));

        let writes = collector.take();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].collection(), "guestbook");
        match &writes[0] {
            ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "ada"),
            other => panic!("expected Append, got {other:?}"),
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
        assert!(record_forge_write(append(
            "guestbook",
            json!({ "author": "outer" })
        )));
        {
            let inner = install_forge_write_collector();
            assert!(record_forge_write(append(
                "guestbook",
                json!({ "author": "inner" })
            )));
            let inner_writes = inner.take();
            assert_eq!(inner_writes.len(), 1);
            match &inner_writes[0] {
                ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "inner"),
                other => panic!("expected Append, got {other:?}"),
            }
        }
        let outer_writes = outer.take();
        assert_eq!(
            outer_writes.len(),
            1,
            "outer writes survived the nested dispatch"
        );
        match &outer_writes[0] {
            ForgeWrite::Append { record, .. } => assert_eq!(record["author"], "outer"),
            other => panic!("expected Append, got {other:?}"),
        }
    }

    #[test]
    fn append_binds_values_and_never_inlines_them() {
        let record = json!({ "author": "ada", "message": "first light" });
        let (sql, params) = build_append("guestbook", record.as_object().unwrap()).unwrap();

        assert_eq!(
            sql, "INSERT INTO guestbook (author, message) VALUES (?1, ?2)",
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
        assert!(
            !sql.contains("DROP"),
            "the value must never reach the statement text"
        );
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
        assert!(
            format!("{err}").contains("nested"),
            "unexpected error: {err}"
        );
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

    // ── Update / Delete statement builders ──────────────────────────────

    #[test]
    fn update_sets_fields_and_binds_the_key_last() {
        let fields = json!({ "author": "grace", "message": "edited" });
        let (sql, params) =
            build_update("guestbook", "id", &json!(3), fields.as_object().unwrap()).unwrap();

        assert_eq!(
            sql, "UPDATE guestbook SET author = ?1, message = ?2 WHERE id = ?3",
            "fields bind first in key order, the row key binds last"
        );
        assert_eq!(
            params,
            vec![
                SqlValue::Text("grace".to_string()),
                SqlValue::Text("edited".to_string()),
                SqlValue::Integer(3),
            ]
        );
    }

    #[test]
    fn delete_binds_only_the_key() {
        let (sql, params) = build_delete("guestbook", "id", &json!(7)).unwrap();
        assert_eq!(sql, "DELETE FROM guestbook WHERE id = ?1");
        assert_eq!(params, vec![SqlValue::Integer(7)]);
    }

    /// A hostile value in an update's fields or key must bind, never interpolate.
    #[test]
    fn update_and_delete_bind_hostile_values() {
        let hostile = "'); DROP TABLE guestbook;--";
        let (usql, uparams) = build_update(
            "guestbook",
            "id",
            &json!(hostile),
            json!({ "author": hostile }).as_object().unwrap(),
        )
        .unwrap();
        assert!(!usql.contains("DROP"));
        assert_eq!(
            uparams,
            vec![
                SqlValue::Text(hostile.into()),
                SqlValue::Text(hostile.into())
            ]
        );

        let (dsql, dparams) = build_delete("guestbook", "id", &json!(hostile)).unwrap();
        assert!(!dsql.contains("DROP"));
        assert_eq!(dparams, vec![SqlValue::Text(hostile.into())]);
    }

    #[test]
    fn a_field_column_that_is_not_an_identifier_is_refused() {
        let mut fields = Map::new();
        fields.insert(
            "author) VALUES ('x'); DROP TABLE guestbook;--".to_string(),
            json!("x"),
        );
        assert!(build_update("guestbook", "id", &json!(1), &fields).is_err());
    }

    #[test]
    fn an_update_with_no_fields_is_refused() {
        assert!(build_update("guestbook", "id", &json!(1), &Map::new()).is_err());
    }

    /// Changing the key column would strand the row's DOM node on every client
    /// (addressed by the OLD key); the author must delete + re-append instead.
    #[test]
    fn an_update_that_rewrites_the_key_column_is_refused() {
        let fields = json!({ "id": 99, "author": "grace" });
        let err =
            build_update("guestbook", "id", &json!(3), fields.as_object().unwrap()).unwrap_err();
        assert!(format!("{err}").contains("key column"), "unexpected: {err}");
    }

    // ── row_update: choosing the wire shape (C3) ────────────────────────

    /// A reverse-chron collection — the feed shape whose every write is a head
    /// insert, and which before `SlotInsert` re-asserted the whole view each time.
    fn reverse_chron() -> ForgeCollection {
        ForgeCollection::new(
            "guestbook",
            "guestbook",
            "SELECT id, author FROM guestbook ORDER BY id DESC",
            "id",
            Box::new([]),
            Box::new([]),
        )
    }

    fn json_bytes(entries: &[(i64, &str)]) -> Vec<u8> {
        let rows: Vec<Value> = entries
            .iter()
            .map(|(id, author)| json!({ "id": id, "author": author }))
            .collect();
        serde_json::to_vec(&Value::Array(rows)).unwrap()
    }

    fn rendered(entries: &[(i64, &str)]) -> RenderedRows {
        entries
            .iter()
            .map(|(id, author)| {
                (
                    id.to_string(),
                    format!("<li data-albedo-key=\"{id}\">{author}</li>"),
                )
            })
            .collect()
    }

    #[test]
    fn a_head_insert_ships_a_positioned_insert_not_a_whole_reconcile() {
        let needs_whole = Cell::new(false);
        let update = row_update(
            &reverse_chron(),
            &json_bytes(&[(2, "alan"), (1, "ada")]),
            &json_bytes(&[(3, "grace"), (2, "alan"), (1, "ada")]),
            &rendered(&[(3, "grace"), (2, "alan"), (1, "ada")]),
            false,
            &needs_whole,
        );

        match update {
            ListUpdate::Insert { before, rows } => {
                assert_eq!(before, Some(RowKey("2".to_string())));
                assert_eq!(rows.len(), 1, "one row on the wire, not the whole view");
                assert_eq!(rows[0].key, RowKey("3".to_string()));
            }
            other => panic!("expected a positioned insert, got {other:?}"),
        }
    }

    /// The C3 payoff: a head insert stays on the `PerRecord` fast path. The
    /// partial render holds only the new record, and that is all the positioned
    /// insert reads — so it must NOT raise `needs_whole` and drag the write back
    /// into an `O(|view|)` render.
    #[test]
    fn a_head_insert_rides_the_partial_render_without_demanding_the_whole_view() {
        let needs_whole = Cell::new(false);
        let update = row_update(
            &reverse_chron(),
            &json_bytes(&[(2, "alan"), (1, "ada")]),
            &json_bytes(&[(3, "grace"), (2, "alan"), (1, "ada")]),
            &rendered(&[(3, "grace")]), // partial: only the changed record
            true,
            &needs_whole,
        );

        assert!(
            matches!(update, ListUpdate::Insert { .. }),
            "expected a positioned insert off a partial render, got {update:?}"
        );
        assert!(
            !needs_whole.get(),
            "a positioned insert must never demand the whole view"
        );
    }

    /// Regression: the cheapest shape still wins. A tail append is classified
    /// before the positioned insert and stays a `SlotDelta`.
    #[test]
    fn a_tail_append_still_ships_as_a_delta() {
        let needs_whole = Cell::new(false);
        let update = row_update(
            &reverse_chron(),
            &json_bytes(&[(1, "ada")]),
            &json_bytes(&[(1, "ada"), (2, "alan")]),
            &rendered(&[(1, "ada"), (2, "alan")]),
            false,
            &needs_whole,
        );
        assert!(
            matches!(update, ListUpdate::Delta(_)),
            "expected a delta, got {update:?}"
        );
    }

    /// A reorder is not an insert; only the full ordered set carries it. With a
    /// partial render it must still defer to the off-lock whole-view reconcile.
    #[test]
    fn a_reorder_still_falls_back_to_the_whole_set() {
        let needs_whole = Cell::new(false);
        let update = row_update(
            &reverse_chron(),
            &json_bytes(&[(1, "ada"), (2, "alan")]),
            &json_bytes(&[(2, "alan"), (1, "ada")]),
            &rendered(&[(2, "alan"), (1, "ada")]),
            false,
            &needs_whole,
        );
        assert!(
            matches!(update, ListUpdate::Reconcile(_)),
            "expected a reconcile, got {update:?}"
        );

        let needs_whole = Cell::new(false);
        let partial = row_update(
            &reverse_chron(),
            &json_bytes(&[(1, "ada"), (2, "alan")]),
            &json_bytes(&[(2, "alan"), (1, "ada")]),
            &rendered(&[(2, "alan")]),
            true,
            &needs_whole,
        );
        assert!(matches!(partial, ListUpdate::None));
        assert!(needs_whole.get(), "must defer to the off-lock whole render");
    }

    /// An insert tangled with an edit cannot ship as a positioned insert — the
    /// op retracts nothing, so the stale row would survive.
    #[test]
    fn an_insert_alongside_an_edit_falls_back_to_the_whole_set() {
        let needs_whole = Cell::new(false);
        let update = row_update(
            &reverse_chron(),
            &json_bytes(&[(2, "alan"), (1, "ada")]),
            &json_bytes(&[(3, "grace"), (2, "turing"), (1, "ada")]),
            &rendered(&[(3, "grace"), (2, "turing"), (1, "ada")]),
            false,
            &needs_whole,
        );
        assert!(
            matches!(update, ListUpdate::Reconcile(_)),
            "expected a reconcile, got {update:?}"
        );
    }

    /// A null key would compile to `WHERE k = NULL`, which matches nothing — a
    /// silent no-op, the worst outcome for a mutation.
    #[test]
    fn a_null_key_is_refused_for_update_and_delete() {
        assert!(build_delete("guestbook", "id", &Value::Null).is_err());
        assert!(build_update(
            "guestbook",
            "id",
            &Value::Null,
            json!({ "a": 1 }).as_object().unwrap()
        )
        .is_err());
    }
}

/// The loop against a real backend. `build_append` above proves the statement;
/// these prove the *whole* write path — that a durable row lands AND the topic
/// a component renders from reflects it afterwards.
#[cfg(all(test, feature = "forge"))]
mod substrate_tests {
    /// **The safety argument for the whole zero-query path, asserted directly.**
    ///
    /// A spliced value must be byte-identical to what the query would have
    /// returned. If it is not, an unpartitioned collection — every app that
    /// exists today — observes P2, and the difference would surface as a
    /// mismatched hash or a stale row rather than as an error.
    #[tokio::test]
    async fn a_spliced_value_is_byte_identical_to_a_queried_one() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        let mut record = serde_json::Map::new();
        record.insert("author".into(), Value::String("grace".into()));
        record.insert("message".into(), Value::String("spliced".into()));
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Append {
                collection: "guestbook".into(),
                record,
            }],
            None,
        )
        .await
        .unwrap();

        let spliced = broadcast.get("guestbook").unwrap().current_value();
        let collection = schema.slot_for_topic("guestbook").unwrap();
        let queried = materialize_slot(&db, collection, None).await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&spliced),
            String::from_utf8_lossy(&queried),
            "splice and query must agree byte for byte"
        );
    }

    /// Several appends in one action splice in insertion order.
    #[tokio::test]
    async fn multiple_appends_in_one_action_splice_in_order() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        let entry = |author: &str| {
            let mut record = serde_json::Map::new();
            record.insert("author".into(), Value::String(author.into()));
            record.insert("message".into(), Value::String("m".into()));
            ForgeWrite::Append {
                collection: "guestbook".into(),
                record,
            }
        };
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[entry("first"), entry("second")],
            None,
        )
        .await
        .unwrap();

        let value = broadcast.get("guestbook").unwrap().current_value();
        let queried = materialize_slot(&db, schema.slot_for_topic("guestbook").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&value), String::from_utf8_lossy(&queried));

        let rows: Vec<Value> = serde_json::from_slice(&value).unwrap();
        let authors: Vec<&str> = rows.iter().filter_map(|r| r["author"].as_str()).collect();
        assert_eq!(&authors[authors.len() - 2..], &["first", "second"]);
    }

    /// An update cannot be expressed as a splice, so the channel falls back to
    /// the query — and must still land on the right value.
    #[tokio::test]
    async fn an_update_falls_back_to_the_query_and_stays_correct() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = ForgeSchema::guestbook_default();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        let mut fields = serde_json::Map::new();
        fields.insert("message".into(), Value::String("edited".into()));
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Update {
                collection: "guestbook".into(),
                key: Value::Number(1.into()),
                fields,
            }],
            None,
        )
        .await
        .unwrap();

        let value = broadcast.get("guestbook").unwrap().current_value();
        let queried = materialize_slot(&db, schema.slot_for_topic("guestbook").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&value), String::from_utf8_lossy(&queried));
        assert!(String::from_utf8_lossy(&value).contains("edited"));
    }

    /// Item 6 · the two JSON conversions must agree about a declared type.
    ///
    /// A write splices its row in through `returned_record` (the zero-query
    /// `RETURNING` path); a re-read builds the same row through `rows_to_json`.
    /// If only one of them consults the declared type, an appended `true` shows
    /// as `true` until the next full materialisation and then turns into `1` —
    /// or the reverse — and both are valid JSON, so nothing would ever throw.
    /// Byte equality across the two is the only assertion that catches it.
    #[tokio::test]
    async fn a_bool_is_identical_whether_spliced_or_queried() {
        use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
        use std::collections::BTreeMap;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("done".to_string(), FieldSpec::new(FieldType::Bool));
        fields.insert("note".to_string(), FieldSpec::nullable(FieldType::Text));
        let mut declarations = BTreeMap::new();
        declarations.insert(
            "todos".to_string(),
            CollectionDecl {
                fields,
                ..CollectionDecl::default()
            },
        );
        let schema = ForgeSchema::from_declarations(&declarations).unwrap();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        for (done, note) in [(true, Value::String("first".into())), (false, Value::Null)] {
            let mut record = serde_json::Map::new();
            record.insert("done".into(), Value::Bool(done));
            record.insert("note".into(), note);
            apply_writes(
                &db,
                &broadcast,
                &schema,
                &[ForgeWrite::Append {
                    collection: "todos".into(),
                    record,
                }],
                None,
            )
            .await
            .unwrap();
        }

        let spliced = broadcast.get("todos").unwrap().current_value();
        let queried = materialize_slot(&db, schema.slot_for_topic("todos").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&spliced),
            String::from_utf8_lossy(&queried),
            "the spliced row and the queried row must be byte-identical"
        );

        // And the shared answer is the declared one, not the stored one.
        let rows: Vec<Value> = serde_json::from_slice(&spliced).unwrap();
        assert_eq!(rows[0]["done"], Value::Bool(true), "{rows:?}");
        assert_eq!(rows[1]["done"], Value::Bool(false), "{rows:?}");
        assert_eq!(rows[0]["note"], Value::String("first".into()), "{rows:?}");
        assert_eq!(rows[1]["note"], Value::Null, "{rows:?}");
    }

    /// A `DESC` ordering puts a new row at the head. Splicing it at the tail
    /// would put every new row in the wrong place — silently, since both are
    /// valid JSON.
    #[tokio::test]
    async fn a_descending_collection_splices_at_the_head() {
        use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
        use std::collections::BTreeMap;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("body".to_string(), FieldSpec::new(FieldType::Text));
        let mut declarations = BTreeMap::new();
        declarations.insert(
            "feed".to_string(),
            CollectionDecl {
                fields,
                order_by: Some("id desc".to_string()),
                ..CollectionDecl::default()
            },
        );
        let schema = ForgeSchema::from_declarations(&declarations).unwrap();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        for body in ["older", "newer"] {
            let mut record = serde_json::Map::new();
            record.insert("body".into(), Value::String(body.into()));
            apply_writes(
                &db,
                &broadcast,
                &schema,
                &[ForgeWrite::Append {
                    collection: "feed".into(),
                    record,
                }],
                None,
            )
            .await
            .unwrap();
        }

        let value = broadcast.get("feed").unwrap().current_value();
        let queried = materialize_slot(&db, schema.slot_for_topic("feed").unwrap(), None)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&value),
            String::from_utf8_lossy(&queried),
            "head splice must match the DESC query"
        );
        let rows: Vec<Value> = serde_json::from_slice(&value).unwrap();
        assert_eq!(rows[0]["body"], "newer", "newest first: {rows:?}");
    }

    /// `table:` is documented as overridable, but every write builder used to
    /// target the *collection* name — so an override wrote to a table that does
    /// not exist. Pre-existing, found while wiring the zero-query path.
    #[tokio::test]
    async fn a_collection_whose_table_differs_from_its_name_is_writable() {
        use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
        use std::collections::BTreeMap;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let mut fields = BTreeMap::new();
        fields.insert("body".to_string(), FieldSpec::new(FieldType::Text));
        let mut declarations = BTreeMap::new();
        declarations.insert(
            "notes".to_string(),
            CollectionDecl {
                fields,
                table: Some("note_rows".to_string()),
                ..CollectionDecl::default()
            },
        );
        let schema = ForgeSchema::from_declarations(&declarations).unwrap();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &schema).await.unwrap();

        let mut record = serde_json::Map::new();
        record.insert("body".into(), Value::String("into the real table".into()));
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Append {
                collection: "notes".into(),
                record,
            }],
            None,
        )
        .await
        .expect("a write must reach the declared table, not one named after the collection");

        let rows = db.query("SELECT body FROM note_rows", &[]).await.unwrap();
        assert_eq!(rows.rows.len(), 1, "the row landed in `note_rows`");
    }

    /// A partitioned collection, declared the way an app would.
    fn partitioned_schema() -> ForgeSchema {
        use crate::forge::declare::{CollectionDecl, FieldSpec, FieldType};
        use std::collections::BTreeMap;

        let mut fields = BTreeMap::new();
        fields.insert("room".to_string(), FieldSpec::new(FieldType::Text));
        fields.insert("body".to_string(), FieldSpec::new(FieldType::Text));
        let mut declarations = BTreeMap::new();
        declarations.insert(
            "messages".to_string(),
            CollectionDecl {
                fields,
                partition_by: Some("room".to_string()),
                ..CollectionDecl::default()
            },
        );
        ForgeSchema::from_declarations(&declarations).expect("schema")
    }

    fn msg(room: &str, body: &str) -> ForgeWrite {
        let mut record = serde_json::Map::new();
        record.insert("room".into(), Value::String(room.into()));
        record.insert("body".into(), Value::String(body.into()));
        ForgeWrite::Append {
            collection: "messages".into(),
            record,
        }
    }

    fn rows_of(broadcast: &BroadcastRegistry, channel: &str) -> Vec<Value> {
        broadcast
            .get(channel)
            .map(|topic| {
                serde_json::from_slice::<Value>(&topic.current_value())
                    .ok()
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }

    /// An append fans out on **its own partition's channel** and nowhere else.
    /// Without this, one room's message lands in every room.
    #[tokio::test]
    async fn an_append_fans_out_only_on_its_own_partition() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        apply_writes(&db, &broadcast, &schema, &[msg("a", "hello")], None)
            .await
            .unwrap();
        apply_writes(&db, &broadcast, &schema, &[msg("b", "other")], None)
            .await
            .unwrap();

        let room_a = rows_of(&broadcast, "messages:a");
        let room_b = rows_of(&broadcast, "messages:b");
        assert_eq!(room_a.len(), 1, "room a: {room_a:?}");
        assert_eq!(room_a[0]["body"], "hello");
        assert_eq!(room_b.len(), 1, "room b: {room_b:?}");
        assert_eq!(room_b[0]["body"], "other");
        assert!(
            broadcast.get("messages").is_none(),
            "a partitioned collection has no whole-collection channel to fan out on"
        );
    }

    /// The steady-state partitioned append — the case the `O(1)`-in-partition-size
    /// claim actually rests on.
    ///
    /// The FIRST append to a room necessarily queries: the channel does not exist
    /// yet, so there is no previous value to splice onto. Every append after that
    /// splices, and must still agree byte-for-byte with the query it replaced.
    #[tokio::test]
    async fn a_second_append_to_a_partition_splices_and_still_matches_the_query() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        apply_writes(&db, &broadcast, &schema, &[msg("a", "first")], None)
            .await
            .unwrap();
        apply_writes(&db, &broadcast, &schema, &[msg("a", "second")], None)
            .await
            .unwrap();

        let live = broadcast.get("messages:a").unwrap().current_value();
        let queried = materialize_slot(&db, schema.slot_for_topic("messages").unwrap(), Some("a"))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&live),
            String::from_utf8_lossy(&queried),
            "a spliced partition must equal its query"
        );
        let rows: Vec<Value> = serde_json::from_slice(&live).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1]["body"], "second");
    }

    /// The edge PRISM named up front: moving a row between partitions touches
    /// **two** channels. The origin has to hear that the row left, or it shows a
    /// message that is no longer there until someone reloads.
    #[tokio::test]
    async fn a_partition_changing_update_notifies_both_sides() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        apply_writes(&db, &broadcast, &schema, &[msg("a", "movable")], None)
            .await
            .unwrap();
        assert_eq!(rows_of(&broadcast, "messages:a").len(), 1);

        let mut fields = serde_json::Map::new();
        fields.insert("room".into(), Value::String("b".into()));
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Update {
                collection: "messages".into(),
                key: Value::Number(1.into()),
                fields,
            }],
            None,
        )
        .await
        .unwrap();

        assert!(
            rows_of(&broadcast, "messages:a").is_empty(),
            "the origin must be told the row left"
        );
        let arrived = rows_of(&broadcast, "messages:b");
        assert_eq!(arrived.len(), 1, "the destination gains it: {arrived:?}");
        assert_eq!(arrived[0]["body"], "movable");
    }

    /// A delete has to learn the partition from the row before removing it —
    /// afterwards there is nothing left to ask.
    #[tokio::test]
    async fn a_delete_fans_out_on_the_partition_the_row_was_in() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        apply_writes(&db, &broadcast, &schema, &[msg("a", "doomed")], None)
            .await
            .unwrap();
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Delete {
                collection: "messages".into(),
                key: Value::Number(1.into()),
            }],
            None,
        )
        .await
        .unwrap();

        assert!(rows_of(&broadcast, "messages:a").is_empty());
    }

    /// An append that omits the partition column would be written but invisible
    /// to every reader — refused rather than silently orphaned.
    #[tokio::test]
    async fn an_append_missing_its_partition_column_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        let mut record = serde_json::Map::new();
        record.insert("body".into(), Value::String("orphan".into()));
        let err = apply_writes(
            &db,
            &broadcast,
            &schema,
            &[ForgeWrite::Append {
                collection: "messages".into(),
                record,
            }],
            None,
        )
        .await
        .expect_err("refused");
        assert!(format!("{err}").contains("partitioned by"), "{err}");
    }

    /// A key outside the topic alphabet cannot name a channel, so it is refused
    /// at the write rather than producing an unreachable partition.
    #[tokio::test]
    async fn a_partition_key_outside_the_alphabet_is_refused() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        let err = apply_writes(&db, &broadcast, &schema, &[msg("a:b", "sneaky")], None)
            .await
            .expect_err("refused");
        assert!(format!("{err}").contains("alphabet"), "{err}");
    }

    use super::*;
    use crate::forge::skeleton::{bootstrap_schema, hydrate_topics};
    use crate::forge::substrate::Transaction;
    use crate::forge::LibSqlSubstrate;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    /// A substrate that counts the reads passing through it.
    ///
    /// PRISM § 11 asks for the zero-query claim to be asserted, **not
    /// benchmarked** — and the distinction is the whole point. A benchmark says
    /// the write got faster, which stays true if the query merely moved
    /// somewhere cheaper; a count says the query is *gone*. The competitive
    /// sentence is "a reactive backend that never re-runs your query", and a
    /// number is what makes that sentence checkable.
    struct CountingSubstrate {
        inner: LibSqlSubstrate,
        queries: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl DataSubstrate for CountingSubstrate {
        async fn migrate(&self, ddl: &str) -> crate::forge::value::Result<()> {
            self.inner.migrate(ddl).await
        }
        async fn query(
            &self,
            sql: &str,
            params: &[crate::forge::value::SqlValue],
        ) -> crate::forge::value::Result<crate::forge::value::Rows> {
            self.queries.fetch_add(1, AtomicOrdering::Relaxed);
            self.inner.query(sql, params).await
        }
        async fn execute(
            &self,
            sql: &str,
            params: &[crate::forge::value::SqlValue],
        ) -> crate::forge::value::Result<u64> {
            self.inner.execute(sql, params).await
        }
        async fn begin(&self) -> crate::forge::value::Result<Box<dyn Transaction>> {
            // Transaction-scoped reads are counted too: an `INSERT … RETURNING`
            // is the write itself, but a `SELECT` smuggled inside the
            // transaction would be exactly the re-query this asserts against.
            Ok(Box::new(CountingTransaction {
                inner: self.inner.begin().await?,
                queries: self.queries.clone(),
            }))
        }
    }

    struct CountingTransaction {
        inner: Box<dyn Transaction>,
        queries: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Transaction for CountingTransaction {
        async fn query(
            &self,
            sql: &str,
            params: &[crate::forge::value::SqlValue],
        ) -> crate::forge::value::Result<crate::forge::value::Rows> {
            // `INSERT`/`UPDATE … RETURNING` runs through `query` because it
            // returns rows, but it IS the mutation, not a re-read. Counting it
            // would make the assertion meaningless.
            if !sql.trim_start().get(..6).is_some_and(|head| {
                head.eq_ignore_ascii_case("select")
            }) {
                return self.inner.query(sql, params).await;
            }
            self.queries.fetch_add(1, AtomicOrdering::Relaxed);
            self.inner.query(sql, params).await
        }
        async fn execute(
            &self,
            sql: &str,
            params: &[crate::forge::value::SqlValue],
        ) -> crate::forge::value::Result<u64> {
            self.inner.execute(sql, params).await
        }
        async fn commit(self: Box<Self>) -> crate::forge::value::Result<()> {
            self.inner.commit().await
        }
        async fn rollback(self: Box<Self>) -> crate::forge::value::Result<()> {
            self.inner.rollback().await
        }
    }

    /// **§ 6.2, asserted.** An append to a warm partition runs *no* reads.
    ///
    /// The row is rendered from the record the `INSERT … RETURNING` handed back
    /// and spliced into the cached bytes, so the cost is O(1) in the partition
    /// size rather than O(rows in it). This is the claim that is not parity with
    /// anything, and it is the one most likely to regress silently — a fallback
    /// added for some edge case would restore the query and leave every test
    /// passing.
    #[tokio::test]
    async fn an_append_to_a_warm_partition_runs_zero_queries() {
        let queries = Arc::new(AtomicUsize::new(0));
        let db = CountingSubstrate {
            inner: LibSqlSubstrate::open_ephemeral().await.unwrap(),
            queries: queries.clone(),
        };
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        // Warm the partition the way the read-through path does — this read is
        // expected and is what the counter is reset after.
        let collection = schema.slot_for_topic("messages").unwrap();
        let bytes = materialize_slot(&db, collection, Some("a")).await.unwrap();
        broadcast
            .try_topic_partition("messages:a".to_string(), "messages".into(), "a".into(), bytes)
            .unwrap();

        queries.store(0, AtomicOrdering::Relaxed);
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[append(
                "messages",
                json!({ "room": "a", "body": "hello" }),
            )],
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            queries.load(AtomicOrdering::Relaxed),
            0,
            "a warm-partition append must re-run no query; § 6.2 is the claim \
             and this count is the proof"
        );
    }

    /// The converse, so the test above cannot pass by counting nothing: a
    /// **cold** partition has no cached bytes to splice into, so the same write
    /// legitimately falls back to the query. Correct and slower — never wrong.
    #[tokio::test]
    async fn an_append_to_a_cold_partition_falls_back_to_the_query() {
        let queries = Arc::new(AtomicUsize::new(0));
        let db = CountingSubstrate {
            inner: LibSqlSubstrate::open_ephemeral().await.unwrap(),
            queries: queries.clone(),
        };
        let schema = partitioned_schema();
        bootstrap_schema(&db, &schema).await.unwrap();
        let broadcast = BroadcastRegistry::new();

        queries.store(0, AtomicOrdering::Relaxed);
        apply_writes(
            &db,
            &broadcast,
            &schema,
            &[append(
                "messages",
                json!({ "room": "a", "body": "hello" }),
            )],
            None,
        )
        .await
        .unwrap();

        assert!(
            queries.load(AtomicOrdering::Relaxed) > 0,
            "a cold partition must re-materialise; if this is also zero the \
             counter is measuring nothing"
        );
    }

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
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();
        assert_eq!(topic_rows(&broadcast, "guestbook").len(), 2, "seeded rows");

        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
        assert_eq!(
            materialised.len(),
            3,
            "topic rematerialised after the write"
        );
        assert_eq!(materialised[2]["author"], "grace");
        assert_eq!(materialised[2]["message"], "found the bug");
    }

    /// A value that looks like SQL must land as text, not execute. The unit test
    /// proves the statement is parameterised; this proves the backend agrees.
    #[tokio::test]
    async fn a_hostile_value_is_stored_as_data_and_the_table_survives() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let hostile = "'); DROP TABLE guestbook;--";
        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let mut bad = serde_json::Map::new();
        bad.insert(
            "author) VALUES ('x'); DROP TABLE guestbook;--".to_string(),
            json!("x"),
        );

        let err = apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let err = apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
    ///
    /// S5 tightened this from "one row *beside* the snapshot" to "one row,
    /// alone": the snapshot stays in the topic (asserted below, since a reload
    /// must still show three rows) but no longer rides the wire behind every
    /// delta. That is the difference between a 179-byte append and a 126KB one.
    #[tokio::test]
    async fn an_append_fans_out_one_row_delta_and_nothing_else() {
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
                _partition: Option<&str>,
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
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
            [Instruction::SlotDelta { changes, .. }] => {
                // The delta is one row, not three — and it is the whole frame.
                assert_eq!(changes.len(), 1, "an append must cost ONE row on the wire");
                assert_eq!(changes[0].weight, 1);
                assert_eq!(changes[0].key, RowKey("3".to_string()));
                assert_eq!(
                    String::from_utf8(changes[0].payload.clone()).unwrap(),
                    "<li data-albedo-key=\"3\">grace</li>"
                );
            }
            other => panic!("expected [SlotDelta] alone, got {other:?}"),
        }

        // The snapshot is still the truth a reload would show — suppressing it
        // on the wire must not stop the topic from advancing.
        assert_eq!(
            topic_rows(&broadcast, "guestbook").len(),
            3,
            "the stored snapshot carries the appended row"
        );
    }

    /// The fast path. A `PerRecord` collection answers an append by rendering
    /// only the appended row — over a *singleton* collection — never the whole
    /// view. The wire result is byte-identical to the whole-view path (asserted
    /// just above); this test asserts the projector was never handed more than
    /// one row, which is the whole point: the render stops being `O(|view|)`.
    #[tokio::test]
    async fn a_per_record_append_renders_only_the_new_row() {
        use crate::ir::opcode::Instruction;
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;
        use crate::transforms::shared_slot_lists::RowProjection;

        /// Records the size of every collection it is asked to render, and
        /// declares itself `PerRecord` so the singleton fast path engages.
        struct CountingGuestbook {
            render_sizes: std::sync::Mutex<Vec<usize>>,
        }

        #[async_trait::async_trait]
        impl crate::forge::delta::RowProjector for CountingGuestbook {
            async fn project_rows(
                &self,
                collection: &str,
                _partition: Option<&str>,
                value: &[u8],
            ) -> Option<crate::forge::delta::RenderedRows> {
                if collection != "guestbook" {
                    return None;
                }
                let records: Value = serde_json::from_slice(value).ok()?;
                let arr = records.as_array()?;
                self.render_sizes.lock().unwrap().push(arr.len());
                let mut html = String::from("<ul data-albedo-list-slot=\"guestbook\">");
                for record in arr {
                    html.push_str(&format!(
                        "<li data-albedo-key=\"{}\">{}</li>",
                        record.get("id")?,
                        record.get("author")?.as_str()?
                    ));
                }
                html.push_str("</ul>");
                crate::transforms::shared_slot_lists::extract_keyed_rows(&html, collection)
            }

            fn projection_class(&self, collection: &str) -> RowProjection {
                if collection == "guestbook" {
                    RowProjection::PerRecord
                } else {
                    RowProjection::WholeView
                }
            }
        }

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        let projector = CountingGuestbook { render_sizes: std::sync::Mutex::new(Vec::new()) };
        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
            &[append(
                "guestbook",
                json!({ "author": "grace", "message": "found the bug" }),
            )],
            Some(&projector),
        )
        .await
        .unwrap();

        // Identical wire result to the whole-view path: a lone one-row delta.
        let payload = rx.recv().await.expect("the write reaches the subscriber");
        let (frame, _) = decode_frame(&payload).unwrap();
        match frame.instructions.as_slice() {
            [Instruction::SlotDelta { changes, .. }] => {
                assert_eq!(changes.len(), 1, "an append is still ONE row on the wire");
                assert_eq!(changes[0].weight, 1);
            }
            other => panic!("expected [SlotDelta] alone, got {other:?}"),
        }

        // The proof of `O(1)`: the projector was only ever asked to render a
        // single row — never the whole, post-write, three-row collection.
        let sizes = projector.render_sizes.lock().unwrap();
        assert!(!sizes.is_empty(), "the projector was invoked");
        assert!(
            sizes.iter().all(|&n| n == 1),
            "every render must be a singleton; got sizes {sizes:?}"
        );
    }

    /// Module-scope stand-in for the render path, shared by the update/delete
    /// delta tests: renders the whole guestbook to keyed `<li>`s and reads the
    /// rows back through the real markup extractor.
    struct Guestbook;

    #[async_trait::async_trait]
    impl crate::forge::delta::RowProjector for Guestbook {
        async fn project_rows(
            &self,
            collection: &str,
            _partition: Option<&str>,
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

    fn update(collection: &str, key: Value, fields: Value) -> ForgeWrite {
        ForgeWrite::Update {
            collection: collection.to_string(),
            key,
            fields: fields.as_object().expect("fields is an object").clone(),
        }
    }

    fn delete(collection: &str, key: Value) -> ForgeWrite {
        ForgeWrite::Delete {
            collection: collection.to_string(),
            key,
        }
    }

    /// An update must persist AND reach subscribers as an in-place patch — the
    /// `−old, +new` pair under one key that the client folds into a single node
    /// replacement, not a repaint. This is the retraction/patch half of the
    /// delta engine, unreachable until Update existed.
    #[tokio::test]
    async fn an_update_persists_and_fans_out_a_keyed_patch() {
        use crate::ir::opcode::{Instruction, RowKey};
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        // Row id=1 is "ada" from the seed; rename its author to "turing".
        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
            &[update("guestbook", json!(1), json!({ "author": "turing" }))],
            Some(&Guestbook),
        )
        .await
        .unwrap();

        // Durable: the row changed, the count did not.
        let rows = db
            .query("SELECT author FROM guestbook WHERE id = 1", &[])
            .await
            .unwrap();
        assert_eq!(
            rows.rows[0].get(0).and_then(SqlValue::as_str),
            Some("turing")
        );
        assert_eq!(
            topic_rows(&broadcast, "guestbook").len(),
            2,
            "an update changes no count"
        );

        let payload = rx.recv().await.expect("the update reaches the subscriber");
        let (frame, _) = decode_frame(&payload).unwrap();
        match frame.instructions.as_slice() {
            [Instruction::SlotDelta { changes, .. }] => {
                assert_eq!(changes.len(), 2, "an update is a -/+ pair under one key");
                assert_eq!(
                    (changes[0].weight, &changes[0].key),
                    (-1, &RowKey("1".to_string()))
                );
                assert_eq!(
                    (changes[1].weight, &changes[1].key),
                    (1, &RowKey("1".to_string()))
                );
                assert_eq!(
                    String::from_utf8(changes[1].payload.clone()).unwrap(),
                    "<li data-albedo-key=\"1\">turing</li>"
                );
            }
            other => panic!("expected [SlotDelta] alone, got {other:?}"),
        }
    }

    /// A delete must persist AND fan out as a lone retraction — a `−` the client
    /// removes by key. This is the other half the delta engine could express but
    /// nothing could produce.
    #[tokio::test]
    async fn a_delete_persists_and_fans_out_a_retraction() {
        use crate::ir::opcode::{Instruction, RowKey};
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
            &[delete("guestbook", json!(2))],
            Some(&Guestbook),
        )
        .await
        .unwrap();

        assert_eq!(topic_rows(&broadcast, "guestbook").len(), 1, "one row gone");
        let remaining = db.query("SELECT id FROM guestbook", &[]).await.unwrap();
        assert_eq!(remaining.rows.len(), 1);
        assert_eq!(remaining.rows[0].get(0).and_then(SqlValue::as_i64), Some(1));

        let payload = rx.recv().await.expect("the delete reaches the subscriber");
        let (frame, _) = decode_frame(&payload).unwrap();
        match frame.instructions.as_slice() {
            [Instruction::SlotDelta { changes, .. }] => {
                assert_eq!(changes.len(), 1, "a delete is one retraction");
                assert_eq!(changes[0].weight, -1);
                assert_eq!(changes[0].key, RowKey("2".to_string()));
            }
            other => panic!("expected [SlotDelta] alone, got {other:?}"),
        }
    }

    /// Update and delete share the append path's atomicity: a malformed second
    /// write rolls back the good first one.
    #[tokio::test]
    async fn a_bad_write_in_a_batch_rolls_back_the_good_ones() {
        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        // A good delete, then an update with a null key (refused before execute).
        let err = apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
            &[
                delete("guestbook", json!(1)),
                update("guestbook", Value::Null, json!({ "author": "x" })),
            ],
            None,
        )
        .await
        .expect_err("a null key must fail the action");
        assert!(format!("{err}").contains("null"), "unexpected: {err}");
        assert_eq!(
            topic_rows(&broadcast, "guestbook").len(),
            2,
            "the good delete rolled back"
        );
    }

    /// No projector (today's serve path) must behave exactly as it did before
    /// the delta lane existed: snapshot only, nothing row-shaped on the wire.
    #[tokio::test]
    async fn without_a_projector_the_write_falls_back_to_a_snapshot() {
        use crate::ir::wire::decode_frame;
        use crate::runtime::session::SessionId;

        let db = LibSqlSubstrate::open_ephemeral().await.unwrap();
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        broadcast
            .subscribe(SessionId::random(), "guestbook", tx)
            .unwrap();

        apply_writes(
            &db,
            &broadcast,
            &ForgeSchema::guestbook_default(),
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
            bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
            let broadcast = BroadcastRegistry::new();
            hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();
            apply_writes(
                &db,
                &broadcast,
            &ForgeSchema::guestbook_default(),
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
        bootstrap_schema(&db, &ForgeSchema::guestbook_default()).await.unwrap();
        let broadcast = BroadcastRegistry::new();
        hydrate_topics(&db, &broadcast, &ForgeSchema::guestbook_default()).await.unwrap();

        let rows = topic_rows(&broadcast, "guestbook");
        assert_eq!(rows.len(), 3, "the appended row survived the restart");
        assert_eq!(rows[2]["message"], "again");
    }
}
