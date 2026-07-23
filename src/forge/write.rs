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
use crate::forge::skeleton::{materialize_slot, ForgeCollection, ForgeSchema};
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

/// Build the statement for one write against its resolved slot. Dispatches on
/// the variant; the slot supplies the `&'static` collection name and key column,
/// so no userland string reaches the SQL as an identifier.
fn build_statement(
    slot: &ForgeCollection,
    write: &ForgeWrite,
) -> Result<(String, Vec<SqlValue>)> {
    match write {
        ForgeWrite::Append { record, .. } => build_append(&slot.topic, record),
        ForgeWrite::Update { key, fields, .. } => {
            build_update(&slot.topic, &slot.key_column, key, fields)
        }
        ForgeWrite::Delete { key, .. } => build_delete(&slot.topic, &slot.key_column, key),
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
    let mut touched: Vec<&ForgeCollection> = Vec::new();
    for write in writes {
        let slot = schema.slot_for_topic(write.collection()).ok_or_else(|| {
            SubstrateError::Backend(format!(
                "FORGE write: '{}' is not a FORGE-backed collection",
                write.collection()
            ))
        })?;
        if !touched.iter().any(|known| known.topic == slot.topic) {
            touched.push(slot);
        }
    }

    let t_commit = std::time::Instant::now();
    let tx = substrate.begin().await?;
    for write in writes {
        // The slot is borrowed from the schema, resolved in `touched` above —
        // the collection name and key column reach SQL from here, never from the
        // userland string.
        let slot = touched
            .iter()
            .find(|slot| slot.topic == write.collection())
            .expect("collection was resolved above");
        let (sql, params) = match build_statement(slot, write) {
            Ok(built) => built,
            Err(err) => {
                // Drop the whole action's writes: one malformed mutation must
                // not leave earlier ones committed.
                let _ = tx.rollback().await;
                return Err(err);
            }
        };
        if let Err(err) = tx.execute(&sql, &params).await {
            let _ = tx.rollback().await;
            return Err(err);
        }
    }
    tx.commit().await?;
    let commit_el = t_commit.elapsed();

    // Durable now — safe to tell the world.
    for slot in touched {
        // Both awaits happen HERE, outside the topic's critical section:
        // `write_topic_delta`'s closure runs under that topic's lock, so
        // awaiting in it would serialise every writer behind the slowest query
        // — and a projector that reached for the topic's value would deadlock
        // on the very lock it is running under. What survives into the closure
        // is only the diff: pure, bounded by the collection size, and the one
        // step that genuinely needs the pre-state it is replacing.
        let t_mat = std::time::Instant::now();
        let bytes = materialize_slot(substrate, slot).await?;
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
                match render_changed_rows(p, slot, broadcast, &bytes).await {
                    // Changed rows attributed and rendered: the fast path.
                    Some(changed) => (Some(changed), true),
                    // No prior value to diff against, or unkeyable rows: fall back
                    // to the whole view, which is always renderable and correct.
                    None => (p.project_rows(&slot.topic, &bytes).await, false),
                }
            }
            Some(p) => (p.project_rows(&slot.topic, &bytes).await, false),
            None => (None, false),
        };
        let proj_el = t_proj.elapsed();
        let row_count = rows.as_ref().map(|r| r.len()).unwrap_or(0);

        // Dev correctness gate: prove the singleton-rendered rows are byte-identical
        // to the whole-view render's slice of them. A `PerRecord` misclassification
        // would surface here loudly rather than as a stranded row in production.
        if partial && std::env::var_os("ALBEDO_FORGE_VERIFY").is_some() {
            if let (Some(p), Some(changed)) = (projector, rows.as_ref()) {
                if let Some(whole) = p.project_rows(&slot.topic, &bytes).await {
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
        broadcast.topic(slot.topic.clone(), bytes.clone());
        // `needs_whole` is raised inside the closure when a partial render can't
        // express the transition (a reorder, a mid-list insert, or a race that
        // moved `previous` out from under the changed-set guess). The reconcile is
        // then shipped after the lock, off the critical section.
        let needs_whole = Cell::new(false);
        let _ = broadcast.write_topic_delta(&slot.topic, |previous| {
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
                if let Some(whole) = p.project_rows(&slot.topic, &bytes).await {
                    let _ = broadcast.write_topic_delta(&slot.topic, |_previous| TopicTransition {
                        value: bytes.clone(),
                        update: ListUpdate::Reconcile(reconcile_rows(&whole)),
                    });
                }
            }
        }

        if timing {
            let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
            eprintln!(
                "[forge-timing] topic={} rows={} partial={} commit={:.3}ms materialize={:.3}ms \
                 project={:.3}ms fanout={:.3}ms",
                slot.topic,
                row_count,
                partial,
                ms(commit_el),
                ms(mat_el),
                ms(proj_el),
                ms(fan_el),
            );
        }
    }

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
    broadcast: &BroadcastRegistry,
    next: &[u8],
) -> Option<RenderedRows> {
    let previous = broadcast.get(&slot.topic)?.current_value();

    // Intent-path shortcut: when `next` is provably `previous` plus a tail, the
    // appended records ARE the changed set. No parse of either full array, no
    // diff — the guess costs `O(|Δ|)` instead of `O(|view|)`.
    if let Some(appended) = appended_rows(&previous, next, &slot.key_column) {
        let mut rows = RenderedRows::new();
        for (key, record) in &appended {
            let singleton = serde_json::to_vec(&Value::Array(vec![record.clone()])).ok()?;
            let rendered = projector.project_rows(&slot.topic, &singleton).await?;
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
        let rendered = projector.project_rows(&slot.topic, &singleton).await?;
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
