//! FORGE's collection delta: **what changed**, as a z-set over records.
//!
//! [`crate::forge::write::apply_writes`] used to answer a write with the whole
//! collection — rematerialise, fan the JSON out, let the client repaint. That
//! is correct and it is `O(|view|)`: appending one guestbook row re-ships every
//! row, and a keyed list has no way to tell which `<li>` is new, so it rebuilds
//! all of them and loses the DOM node identity (and focus, and scroll, and any
//! in-flight animation) of rows that never changed.
//!
//! This module answers the same write with `O(|Δ|)`: the signed multiset of
//! records that differ between the collection's previous value and its new one.
//! `+1` inserts a row, `−1` retracts one, and an *update* is both under one key.
//!
//! # Two layers, on purpose
//!
//! - [`diff_records`] is **renderer-free**. It speaks records and keys, knows nothing about HTML,
//!   and is a pure function of two JSON arrays — which is what lets the whole delta path be tested
//!   without a renderer, a browser, or a substrate.
//! - [`RowProjector`] is the seam where the *collection* becomes rendered rows. Only the render
//!   path can implement it (the row template lives in the author's TSX), so FORGE takes it as a
//!   parameter and never learns what a `<li>` is.
//!
//! # Why the projector renders the whole view, not one row
//!
//! The obvious seam is "render this record" — and it is wrong. A row template
//! is not a pure function of its record: `map((row, i) => …)` closes over the
//! index, a row may read the collection's length, and the rendered markup is
//! whatever the surrounding component computed. Rendering one record in
//! isolation would produce a row that *almost* matches what a reload produces,
//! and "almost" here means a page that silently disagrees with the server.
//!
//! So the projector renders the post-write collection **exactly as SSR would**
//! and returns its rows keyed by `data-albedo-key`; this module then ships only
//! the rows the diff says changed. The render stays `O(|view|)` — it always was,
//! every request pays it — while the wire and the DOM reconciliation, where the
//! cost actually bites, become `O(|Δ|)`.
//!
//! # Fail-safe, not best-effort
//!
//! Every uncertainty here resolves to **no delta**, never to a guessed one.
//! A collection whose rows have no usable key, duplicate keys, a previous value
//! that isn't an array (the `b"null"` placeholder a topic is registered with),
//! or a row the projector declines to render — each yields `None`, and the
//! caller falls back to shipping the snapshot alone. That path is slower and
//! always right. A *wrong* delta, by contrast, is silent and permanent: the
//! client applies it, diverges from server truth, and shows a page that no
//! reload-free interaction will ever correct. The asymmetry is total, so the
//! rule is total: emit a delta only when it is provably the same transition the
//! snapshot describes.

use crate::ir::opcode::{ReconcileRow, RowKey, SlotChange};
use crate::transforms::shared_slot_lists::RowProjection;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::ops::Index;

/// The rendered rows of one collection, in **render order**: `RowKey → the
/// row's outer HTML`, but ordered the way the markup lays them out rather than
/// sorted by key.
///
/// Keys are byte-identical to the `data-albedo-key` the markup carries, because
/// they were read back out of it — that is what lets a delta name a row the
/// client can actually find. Order matters because
/// [`Instruction::ReconcileList`](crate::ir::opcode::Instruction::ReconcileList)
/// places rows in the order it receives them: a key-sorted map would silently
/// reorder the page. A plain map threw that order away; this newtype keeps it
/// while still answering `get`/`contains_key` in O(n) (row counts are small and
/// the lookups are per-delta, not per-frame).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedRows {
    rows: Vec<(String, String)>,
}

impl RenderedRows {
    #[must_use]
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }

    /// Append `value` under `key`, or overwrite it in place if `key` is already
    /// present (keeping its position). Rows are unique by `data-albedo-key`, so
    /// the overwrite branch is defensive rather than expected.
    pub fn insert(&mut self, key: String, value: String) {
        if let Some(slot) = self.rows.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.rows.push((key, value));
        }
    }

    #[must_use]
    pub fn get(&self, key: &str) -> Option<&String> {
        self.rows.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.rows.iter().any(|(k, _)| k == key)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rows in render order, `(key, html)`.
    pub fn iter(&self) -> std::slice::Iter<'_, (String, String)> {
        self.rows.iter()
    }
}

impl Index<&str> for RenderedRows {
    type Output = String;
    fn index(&self, key: &str) -> &String {
        self.get(key)
            .unwrap_or_else(|| panic!("no rendered row for key {key:?}"))
    }
}

impl FromIterator<(String, String)> for RenderedRows {
    fn from_iter<I: IntoIterator<Item = (String, String)>>(iter: I) -> Self {
        let mut rows = Self::new();
        for (key, value) in iter {
            rows.insert(key, value);
        }
        rows
    }
}

impl IntoIterator for RenderedRows {
    type Item = (String, String);
    type IntoIter = std::vec::IntoIter<(String, String)>;
    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

/// Renders a materialised collection into its keyed rows.
///
/// Implemented by the render path, which owns the row template; FORGE holds it
/// as a `&dyn` so the write loop can produce wire-ready row payloads without
/// depending on a renderer.
///
/// **Must not read the collection's current broadcast value.** The value is
/// passed in explicitly, and for a good reason: the caller invokes this while
/// preparing a topic write, and a projector that reached back into the registry
/// would both render the pre-write state and deadlock on the topic's own
/// linearization lock. Render what you are given.
#[async_trait]
pub trait RowProjector: Send + Sync {
    /// The rows `collection` renders to when its value is `value` (the
    /// materialised JSON bytes), keyed by `data-albedo-key`.
    ///
    /// `None` when this projector cannot speak for the collection — no
    /// template, an ambiguous one, a render failure, or markup with no keyed
    /// list anchor in it. `None` suppresses the delta entirely; it never
    /// degrades to a partial one.
    async fn project_rows(&self, collection: &str, value: &[u8]) -> Option<RenderedRows>;

    /// The compile-time incrementalisation class of `collection`'s row template
    /// (see [`RowProjection`]). It decides whether a single-record write may be
    /// answered by rendering one row over a singleton collection (`PerRecord`,
    /// `O(1)`) or must re-render the whole view.
    ///
    /// Defaults to the always-correct [`RowProjection::WholeView`]: a projector
    /// that has not classified its templates simply never takes the fast path.
    /// The pooled render projector overrides it with the class the transpile
    /// pre-pass derived from the `.map()` callback.
    fn projection_class(&self, _collection: &str) -> RowProjection {
        RowProjection::WholeView
    }
}

/// One record-level change, before rendering. The renderer-free half of a
/// [`SlotChange`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordChange {
    /// `+1` inserts, `−1` retracts. Aggregations produce other integers; the
    /// diff below only ever emits ±1, since a materialised collection is a set
    /// of distinct keys rather than a bag.
    pub weight: i32,
    /// Reconciliation identity — the stringified key column, byte-identical to
    /// the `data-albedo-key` SSR stamped on the row.
    pub key: String,
    /// The record this change carries. For a retraction it is the *old* record,
    /// so the client can compare payloads and recognise an update.
    pub record: Value,
}

/// The z-set delta taking `previous` to `next`, keyed by `key_column`.
///
/// Emission order is the order the client will apply in, and the client's list
/// sink appends inserts at the end, so:
///
/// 1. **Retractions first**, in `previous` order — freeing keys before they can be re-inserted
///    keeps a "delete row 2, insert a new row 2" batch from looking like an update of a row that is
///    on its way out.
/// 2. **Then `next` order** — an update as `−old, +new` under one key (the pair the client folds
///    into an in-place patch), an insertion as a lone `+`.
///
/// Returns `None` when the delta cannot be trusted; see the module docs. Note
/// that `None` is *not* an error — it is the caller's cue to ship the snapshot
/// alone, which is always correct.
///
/// Reordering alone is deliberately **not** expressed here: two collections with
/// the same records in a different order produce an empty delta, because the
/// wire has no "move" and a synthesised remove/insert pair would destroy the
/// node identity this path exists to preserve. Position is instead carried by
/// [`is_tail_append`] + [`ReconcileList`](crate::ir::opcode::Instruction::ReconcileList):
/// a transition this delta cannot reproduce (a reorder, a mid-list insert) is
/// classified as non-tail-append and ships the full ordered set, which does have
/// position. This function stays the `O(|Δ|)` fast path for the tail-append case.
#[must_use]
pub fn diff_records(previous: &Value, next: &Value, key_column: &str) -> Option<Vec<RecordChange>> {
    let previous = index_by_key(previous.as_array()?, key_column)?;
    let next_rows = next.as_array()?;
    let next_index = index_by_key(next_rows, key_column)?;

    // Membership by lookup, not by scan. Both sides are already deduplicated by
    // `index_by_key`, so a map over them loses nothing — and it is what keeps
    // this linear. The scan-inside-a-loop this replaced was O(N²): at a couple
    // of thousand rows it cost more than the render it exists to feed, and it
    // was paid twice per write (the changed-set guess and the authoritative
    // classification under the topic lock).
    let previous_by_key: HashMap<&str, &Value> = previous
        .iter()
        .map(|(key, record)| (key.as_str(), *record))
        .collect();
    let next_by_key: HashMap<&str, ()> = next_index
        .iter()
        .map(|(key, _)| (key.as_str(), ()))
        .collect();

    let mut changes = Vec::new();

    // 1. Retractions, in previous order.
    for (key, record) in &previous {
        if !next_by_key.contains_key(key.as_str()) {
            changes.push(RecordChange {
                weight: -1,
                key: key.clone(),
                record: (*record).clone(),
            });
        }
    }

    // 2. Insertions and updates, in next order.
    for (key, record) in &next_index {
        match previous_by_key.get(key.as_str()) {
            Some(before) if before == record => {}
            Some(before) => {
                // An update is the retraction of the old row AND the insertion
                // of the new one under the same key. Both halves must reach the
                // wire — see `coalesce_changes`, which keys on (key, payload)
                // precisely so this pair cannot cancel itself out.
                changes.push(RecordChange {
                    weight: -1,
                    key: key.clone(),
                    record: (*before).clone(),
                });
                changes.push(RecordChange {
                    weight: 1,
                    key: key.clone(),
                    record: (*record).clone(),
                });
            }
            None => changes.push(RecordChange {
                weight: 1,
                key: key.clone(),
                record: (*record).clone(),
            }),
        }
    }

    Some(changes)
}

/// Pair every [`RecordChange`] with the rendered row it refers to, producing
/// the wire [`SlotChange`]s.
///
/// `rows` is the projection of the collection's **new** value, so a positive
/// change always finds its markup there. A retraction refers to a row that the
/// new value no longer contains, so it cannot: it is emitted with an empty
/// payload, which is exactly what the client's sink needs — a removal is
/// executed by key, and the payload of a `−` is only ever read as part of the
/// `(key, payload)` coalescing identity, where "the empty row" is a distinct
/// record from any insertion under the same key and therefore still forces the
/// `−`/`+` pair of an update through to the wire.
///
/// All-or-nothing on the positive side: one insertion whose key is missing from
/// `rows` collapses the whole delta to `None`. That means the render and the
/// diff disagree about what the collection contains — one of them is wrong, we
/// cannot tell which, and shipping the renderable subset would fan out a
/// transition that does not match the snapshot beside it.
#[must_use]
pub fn project_changes(changes: &[RecordChange], rows: &RenderedRows) -> Option<Vec<SlotChange>> {
    changes
        .iter()
        .map(|change| {
            let payload = if change.weight < 0 {
                Some(Vec::new())
            } else {
                rows.get(&change.key).map(|html| html.clone().into_bytes())
            }?;
            Some(SlotChange {
                weight: change.weight,
                key: RowKey(change.key.clone()),
                payload,
            })
        })
        .collect()
}

/// Whether `previous → next` is expressible as a tail-appending
/// [`SlotChange`] delta, or must ship the full ordered set as a
/// [`ReconcileList`](crate::ir::opcode::Instruction::ReconcileList).
///
/// A `SlotDelta`'s inserts land at the tail in emission order, so a delta
/// reproduces `next` **only** when two things hold:
///
/// 1. the rows surviving the transition keep their relative order (no reorder), and
/// 2. every inserted row comes after all survivors in `next` (no mid-list insert).
///
/// When either fails — a reorder (which [`diff_records`] reports as an empty
/// delta, since membership didn't change) or an insert into the middle — the
/// tail-append model would render the wrong order, and the caller falls back to
/// a full reconcile. Untrustworthy inputs (a non-array `previous`, unkeyable
/// rows) return `false`: the full set is the fail-safe, exactly as elsewhere.
#[must_use]
pub fn is_tail_append(previous: &Value, next: &Value, key_column: &str) -> bool {
    let (Some(prev), Some(next)) = (previous.as_array(), next.as_array()) else {
        return false;
    };
    let (Some(prev_keys), Some(next_keys)) = (
        index_by_key(prev, key_column),
        index_by_key(next, key_column),
    ) else {
        return false;
    };
    let prev_set: HashMap<&str, ()> = prev_keys.iter().map(|(k, _)| (k.as_str(), ())).collect();
    let next_set: HashMap<&str, ()> = next_keys.iter().map(|(k, _)| (k.as_str(), ())).collect();

    // (1) survivors keep their relative order between prev and next.
    let survivors_prev = prev_keys
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| next_set.contains_key(k));
    let survivors_next = next_keys
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| prev_set.contains_key(k));
    if !survivors_prev.eq(survivors_next) {
        return false;
    }

    // (2) once an inserted key appears in next order, no survivor may follow it.
    let mut seen_insert = false;
    for (key, _) in &next_keys {
        if prev_set.contains_key(key.as_str()) {
            if seen_insert {
                return false;
            }
        } else {
            seen_insert = true;
        }
    }
    true
}

/// The rows `next` appends to `previous`, decided on **bytes** — or `None` when
/// `next` is not `previous` plus a tail.
///
/// The intent path's shortcut. Both buffers are `serde_json::to_vec` of the same
/// collection shape: no whitespace, and a record that did not change serialises
/// to the same bytes it did last time. So "`next`'s text begins with `previous`'s
/// text, minus its closing bracket" is a **stronger** fact than the record diff
/// establishes — byte equality implies value equality, in the same order — and
/// it costs one `memcmp` instead of parsing and indexing every row on both sides.
/// What remains between that prefix and `next`'s closing bracket is exactly what
/// was appended, and it is the only thing this parses.
///
/// That makes the common write — one row onto the end of a collection — `O(1)`
/// in the collection's size for both the changed-set guess and the authoritative
/// classification, where before each paid a full parse plus a full walk. Anything
/// else (an edit, a deletion, a reorder, a non-tail insert, a first write off the
/// `null` placeholder) fails the prefix test and returns `None`, and the caller
/// takes the parse-and-diff path exactly as before.
///
/// Returning `Some(vec![])` is meaningful and distinct from `None`: `next` is
/// byte-identical to `previous`, so nothing changed at all.
#[must_use]
pub fn appended_rows(
    previous: &[u8],
    next: &[u8],
    key_column: &str,
) -> Option<Vec<(String, Value)>> {
    // `[` … `]` on both sides, with room for the brackets themselves. The
    // `b"null"` / empty placeholder a topic is registered with fails here.
    if previous.len() < 2
        || previous.first() != Some(&b'[')
        || previous.last() != Some(&b']')
        || next.last() != Some(&b']')
    {
        return None;
    }
    let prefix = &previous[..previous.len() - 1];
    if !next.starts_with(prefix) {
        return None;
    }

    let tail = &next[prefix.len()..next.len() - 1];
    if tail.is_empty() {
        return Some(Vec::new());
    }
    // The appended members, re-bracketed into an array to parse. A non-empty
    // `previous` leaves the separating comma at the front of the tail; an empty
    // one (`[]`, whose prefix is just `[`) does not.
    let body = if tail[0] == b',' { &tail[1..] } else { tail };
    let mut json = Vec::with_capacity(body.len() + 2);
    json.push(b'[');
    json.extend_from_slice(body);
    json.push(b']');
    let records: Vec<Value> = serde_json::from_slice(&json).ok()?;

    // Uphold `index_by_key`'s refusal to reconcile against a key that does not
    // identify. The slow path proves uniqueness by indexing every row; doing that
    // here would cost the parse this function exists to avoid, so instead look for
    // the appended key's serialised fragment in the previous bytes. A hit can be
    // incidental — the same text inside some other field — so it only ever costs a
    // fallback; but a genuine duplicate cannot hide from it.
    let previous_text = std::str::from_utf8(previous).ok()?;
    let mut out = Vec::with_capacity(records.len());
    let mut seen: HashMap<String, ()> = HashMap::with_capacity(records.len());
    for record in records {
        let key_value = record.get(key_column)?;
        let key = row_key(key_value)?;
        if seen.insert(key.clone(), ()).is_some() {
            return None;
        }
        let fragment = format!(
            "{}:{}",
            serde_json::to_string(key_column).ok()?,
            serde_json::to_string(key_value).ok()?
        );
        if previous_text.contains(&fragment) {
            return None;
        }
        out.push((key, record));
    }
    Some(out)
}

/// One contiguous run of newly inserted rows, and the row it lands ahead of —
/// the record-level shape of
/// [`Instruction::SlotInsert`](crate::ir::opcode::Instruction::SlotInsert).
///
/// `before` is the key of the surviving row the run is inserted before; `None`
/// means the run is at the tail. `keys` are the inserted keys in `next` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionedInsert {
    pub before: Option<String>,
    pub keys: Vec<String>,
}

/// Whether `previous → next` is a **pure positioned insert**: rows added at one
/// place, with nothing else touched.
///
/// The rung between [`is_tail_append`] and the whole-set fallback. A tail append
/// ships as a `SlotDelta`; anything else used to ship the entire ordered view,
/// because the wire had no way to say *where* a row goes. `SlotInsert` says it,
/// so this classifier finds the transitions that opcode can reproduce exactly:
///
/// 1. **Nothing retracted** — every previous key survives.
/// 2. **Nothing updated** — every surviving key carries a byte-identical record. An edit is a `−`/`+`
///    pair; `SlotInsert` upserts but retracts nothing, so a transition containing one is not this
///    shape.
/// 3. **Survivors keep their relative order** — the op moves no existing row.
/// 4. **The inserted keys form a single contiguous run** — one op names one anchor. Two runs at
///    different positions would need two ops, and splitting them here would let a partial apply
///    land; the whole set is the honest answer instead.
///
/// The anchor is read off `next`'s **own order**, not recomputed from the
/// collection's `ORDER BY`: `next` is the freshly materialised snapshot, so the
/// substrate has already done the ordering, exactly and with its own collation.
/// Re-deriving position from [`ForgeCollection::sort`](crate::forge::skeleton::ForgeCollection)
/// would mean reimplementing the substrate's comparison semantics — mixed types,
/// NULL placement, text collation — in Rust, and being subtly wrong there
/// produces a row stranded at the wrong index rather than an error. `sort` earns
/// its keep when the whole array stops being materialised; while we have it, it
/// is the more trustworthy source.
///
/// Returns `None` for anything that is not this shape, including "nothing was
/// inserted" — same fail-safe rule as the rest of the module: the caller ships
/// the full set, which is always correct.
#[must_use]
pub fn classify_positioned_insert(
    previous: &Value,
    next: &Value,
    key_column: &str,
) -> Option<PositionedInsert> {
    let prev_rows = index_by_key(previous.as_array()?, key_column)?;
    let next_rows = index_by_key(next.as_array()?, key_column)?;

    let prev_index: HashMap<&str, &Value> = prev_rows
        .iter()
        .map(|(key, record)| (key.as_str(), *record))
        .collect();
    let next_keys: HashMap<&str, ()> = next_rows
        .iter()
        .map(|(key, _)| (key.as_str(), ()))
        .collect();

    // (1) nothing retracted.
    if prev_rows
        .iter()
        .any(|(key, _)| !next_keys.contains_key(key.as_str()))
    {
        return None;
    }

    // One walk of `next` settles (2), (4) and the anchor; (3) is checked after,
    // against the order the survivors came out in.
    let mut keys: Vec<String> = Vec::new();
    let mut before: Option<String> = None;
    let mut run_closed = false;
    let mut survivors: Vec<&str> = Vec::with_capacity(prev_rows.len());

    for (key, record) in &next_rows {
        match prev_index.get(key.as_str()) {
            Some(previous_record) => {
                // (2) a survivor whose record changed is an update, not an insert.
                if *previous_record != *record {
                    return None;
                }
                // The first survivor after the run began is the anchor.
                if !keys.is_empty() && !run_closed {
                    before = Some(key.clone());
                    run_closed = true;
                }
                survivors.push(key.as_str());
            }
            None => {
                // (4) a new key after the run closed is a second run.
                if run_closed {
                    return None;
                }
                keys.push(key.clone());
            }
        }
    }

    if keys.is_empty() {
        return None;
    }

    // (3) every previous row survived, so the survivors in `next` order must be
    // the previous order exactly.
    if !prev_rows
        .iter()
        .map(|(key, _)| key.as_str())
        .eq(survivors.into_iter())
    {
        return None;
    }

    Some(PositionedInsert { before, keys })
}

/// Pair a [`PositionedInsert`]'s keys with their rendered markup, in run order.
///
/// All-or-nothing, for the same reason [`project_changes`] is: a key the render
/// never produced means the render and the classifier disagree about what was
/// inserted, and shipping the renderable subset would place rows the snapshot
/// beside it does not describe.
///
/// Note this reads only the *inserted* rows, which is what lets the positioned
/// insert ride the `PerRecord` fast path — `rows` there holds just the records
/// the write changed, never the whole view.
#[must_use]
pub fn project_inserted_rows(
    insert: &PositionedInsert,
    rows: &RenderedRows,
) -> Option<Vec<ReconcileRow>> {
    insert
        .keys
        .iter()
        .map(|key| {
            Some(ReconcileRow {
                key: RowKey(key.clone()),
                payload: rows.get(key)?.clone().into_bytes(),
            })
        })
        .collect()
}

/// `[(key, record)]` in source order, or `None` if the rows cannot be keyed.
///
/// Duplicate keys are refused rather than deduplicated: two rows claiming one
/// reconciliation identity means the key column is not a key, and every apply
/// after that would be guesswork. Better to ship snapshots for that collection
/// forever than to reconcile against an identity that doesn't identify.
fn index_by_key<'a>(rows: &'a [Value], key_column: &str) -> Option<Vec<(String, &'a Value)>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut seen: HashMap<String, ()> = HashMap::with_capacity(rows.len());
    for row in rows {
        let key = row_key(row.get(key_column)?)?;
        if seen.insert(key.clone(), ()).is_some() {
            return None;
        }
        out.push((key, row));
    }
    Some(out)
}

/// Stringify a key column value the same way the render path stamps
/// `data-albedo-key`, so the delta's key matches the DOM's.
///
/// Scalars only. A null, object, or array key is refused: JSON has no canonical
/// text for those, so any rendering here would be this module's invention, and
/// the SSR stamp would be the renderer's — two inventions that only have to
/// disagree once to strand a row.
fn row_key(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Null | Value::Object(_) | Value::Array(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Stands in for the render path's projection of a whole guestbook: each
    /// record's `<li>`, keyed the way SSR keys it.
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

    fn rows(entries: &[(i64, &str)]) -> Value {
        Value::Array(
            entries
                .iter()
                .map(|(id, author)| json!({ "id": id, "author": author }))
                .collect(),
        )
    }

    #[test]
    fn an_append_is_one_positive_change() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        )
        .expect("keyed rows diff");

        assert_eq!(changes.len(), 1, "appending one row must cost one change");
        assert_eq!(changes[0].weight, 1);
        assert_eq!(changes[0].key, "3");
    }

    #[test]
    fn an_edit_is_a_retraction_and_an_insertion_under_one_key() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada"), (2, "turing")]),
            "id",
        )
        .unwrap();

        assert_eq!(changes.len(), 2);
        assert_eq!((changes[0].weight, changes[0].key.as_str()), (-1, "2"));
        assert_eq!((changes[1].weight, changes[1].key.as_str()), (1, "2"));
        assert_eq!(
            changes[0].record["author"], "alan",
            "retraction carries the OLD row"
        );
        assert_eq!(changes[1].record["author"], "turing");
    }

    #[test]
    fn a_deletion_is_one_retraction() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada")]),
            "id",
        )
        .unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!((changes[0].weight, changes[0].key.as_str()), (-1, "2"));
    }

    #[test]
    fn an_unchanged_collection_produces_no_changes() {
        let same = rows(&[(1, "ada"), (2, "alan")]);
        assert!(diff_records(&same, &same, "id").unwrap().is_empty());
    }

    #[test]
    fn retractions_precede_insertions_so_a_freed_key_can_be_reused() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada"), (3, "grace")]),
            "id",
        )
        .unwrap();
        assert_eq!(changes[0].weight, -1, "the removal of key 2 comes first");
        assert_eq!(changes[0].key, "2");
        assert_eq!(changes[1].key, "3");
    }

    /// Reordering is not a delta this wire can express; saying so beats
    /// synthesising remove/insert churn that would throw away node identity.
    #[test]
    fn a_pure_reorder_is_deliberately_not_expressed() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(2, "alan"), (1, "ada")]),
            "id",
        )
        .unwrap();
        assert!(
            changes.is_empty(),
            "ordering is rung 3, and it needs an insert position"
        );
    }

    #[test]
    fn a_tail_append_is_expressible_as_a_delta() {
        assert!(is_tail_append(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        ));
        // A retraction with the survivors' order intact is still tail-shaped.
        assert!(is_tail_append(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada")]),
            "id",
        ));
        // The empty-to-populated first write is all tail inserts.
        assert!(is_tail_append(&rows(&[]), &rows(&[(1, "ada"), (2, "alan")]), "id"));
    }

    #[test]
    fn a_mid_list_insert_is_not_a_tail_append() {
        assert!(!is_tail_append(
            &rows(&[(1, "ada"), (3, "grace")]),
            &rows(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        ));
    }

    #[test]
    fn a_reorder_is_not_a_tail_append_even_though_the_delta_is_empty() {
        // `diff_records` sees no membership change and returns []; the order
        // check is the only thing that catches this, so it must.
        let before = rows(&[(1, "ada"), (2, "alan")]);
        let after = rows(&[(2, "alan"), (1, "ada")]);
        assert!(diff_records(&before, &after, "id").unwrap().is_empty());
        assert!(!is_tail_append(&before, &after, "id"));
    }

    #[test]
    fn a_null_placeholder_previous_is_not_a_tail_append() {
        // First write after a `b"null"` registration: no trustworthy prev order,
        // so the caller ships the full set rather than guessing appends.
        assert!(!is_tail_append(&Value::Null, &rows(&[(1, "ada")]), "id"));
    }

    #[test]
    fn an_untrustworthy_collection_yields_no_delta_rather_than_a_guess() {
        let good = rows(&[(1, "ada")]);

        // Previous value is the `b"null"` placeholder a topic registers with.
        assert!(diff_records(&Value::Null, &good, "id").is_none());
        // The key column is absent.
        assert!(diff_records(&json!([]), &json!([{ "author": "ada" }]), "id").is_none());
        // The key column is not a scalar.
        assert!(diff_records(&json!([]), &json!([{ "id": { "a": 1 } }]), "id").is_none());
        // Two rows claim one identity.
        assert!(diff_records(&json!([]), &rows(&[(1, "ada"), (1, "alan")]), "id").is_none());
    }

    #[test]
    fn projection_stamps_the_key_the_dom_will_be_holding() {
        let changes = diff_records(&rows(&[]), &rows(&[(3, "grace")]), "id").unwrap();
        let projected = project_changes(&changes, &rendered(&[(3, "grace")])).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].key, RowKey("3".to_string()));
        assert_eq!(
            String::from_utf8(projected[0].payload.clone()).unwrap(),
            "<li data-albedo-key=\"3\">grace</li>",
            "the payload's key attribute must match the SlotChange key, or the \
             client reconciles a row it can never find again"
        );
    }

    /// A row the diff insists was inserted but the render never produced means
    /// the two disagree about the collection. Neither can be trusted over the
    /// other, so nothing row-shaped ships.
    #[test]
    fn an_insertion_missing_from_the_render_suppresses_the_whole_delta() {
        let changes = diff_records(&rows(&[]), &rows(&[(3, "grace"), (4, "hopper")]), "id").unwrap();
        assert!(
            project_changes(&changes, &rendered(&[(3, "grace")])).is_none(),
            "a partial delta would disagree with the snapshot shipped beside it"
        );
    }

    /// A retraction names a row the new render no longer contains — by
    /// definition. It still has to reach the wire, carrying the empty payload
    /// the sink ignores for removals.
    #[test]
    fn a_retraction_projects_without_markup_it_could_never_have() {
        let changes =
            diff_records(&rows(&[(1, "ada"), (2, "alan")]), &rows(&[(1, "ada")]), "id").unwrap();
        let projected = project_changes(&changes, &rendered(&[(1, "ada")])).unwrap();

        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].weight, -1);
        assert_eq!(projected[0].key, RowKey("2".to_string()));
        assert!(projected[0].payload.is_empty());
    }

    /// The update pair must survive projection AND coalescing together: the
    /// retraction's empty payload keeps it a distinct record from the insertion
    /// under the same key, so `(key, payload)` coalescing cannot cancel them.
    #[test]
    fn an_update_survives_projection_and_coalescing_as_a_pair() {
        let changes = diff_records(
            &rows(&[(1, "ada"), (2, "alan")]),
            &rows(&[(1, "ada"), (2, "turing")]),
            "id",
        )
        .unwrap();
        let projected = project_changes(&changes, &rendered(&[(1, "ada"), (2, "turing")])).unwrap();
        let coalesced = crate::runtime::broadcast::coalesce_changes(projected);

        assert_eq!(coalesced.len(), 2, "the -/+ pair IS the update");
        assert_eq!(coalesced[0].weight, -1);
        assert_eq!(coalesced[1].weight, 1);
        assert_eq!(
            String::from_utf8(coalesced[1].payload.clone()).unwrap(),
            "<li data-albedo-key=\"2\">turing</li>"
        );
    }

    // ── Byte-level append shortcut (3.3) ─────────────────────────────
    //
    // The intent path. Every rejection here falls back to parse-and-diff, which
    // is always correct — so these pin that the shortcut fires when it should,
    // and above all that it does NOT fire on anything but a tail append.

    fn bytes(entries: &[(i64, &str)]) -> Vec<u8> {
        serde_json::to_vec(&rows(entries)).unwrap()
    }

    #[test]
    fn one_appended_row_is_read_off_the_bytes() {
        let appended = appended_rows(
            &bytes(&[(1, "ada"), (2, "alan")]),
            &bytes(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        )
        .expect("a tail append is byte-provable");

        assert_eq!(appended.len(), 1);
        assert_eq!(appended[0].0, "3");
        assert_eq!(appended[0].1["author"], "grace");
    }

    #[test]
    fn several_appended_rows_come_back_in_order() {
        let appended = appended_rows(
            &bytes(&[(1, "ada")]),
            &bytes(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        )
        .unwrap();
        assert_eq!(
            appended.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["2", "3"]
        );
    }

    #[test]
    fn the_first_rows_into_an_empty_collection_are_an_append() {
        let appended = appended_rows(b"[]", &bytes(&[(1, "ada")]), "id").unwrap();
        assert_eq!(appended.len(), 1, "the `[` prefix has no trailing comma");
        assert_eq!(appended[0].0, "1");
    }

    #[test]
    fn an_unchanged_collection_appends_nothing_but_is_still_provable() {
        // `Some(empty)` is distinct from `None`: nothing changed, as opposed to
        // "cannot tell". The caller ships no list op rather than falling back.
        let same = bytes(&[(1, "ada")]);
        assert_eq!(appended_rows(&same, &same, "id"), Some(Vec::new()));
    }

    #[test]
    fn an_edit_is_not_an_append() {
        assert_eq!(
            appended_rows(
                &bytes(&[(1, "ada"), (2, "alan")]),
                &bytes(&[(1, "ada"), (2, "turing")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn an_edit_to_the_last_row_plus_an_append_is_not_an_append() {
        // The prefix diverges inside the final surviving record, so the byte test
        // must reject even though the collection did grow at the tail.
        assert_eq!(
            appended_rows(
                &bytes(&[(1, "ada"), (2, "alan")]),
                &bytes(&[(1, "ada"), (2, "turing"), (3, "grace")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_deletion_is_not_an_append() {
        assert_eq!(
            appended_rows(
                &bytes(&[(1, "ada"), (2, "alan")]),
                &bytes(&[(1, "ada")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_head_insert_is_not_an_append() {
        // The reverse-chron case: it must reach `classify_positioned_insert`, not
        // be mistaken for a tail append.
        assert_eq!(
            appended_rows(
                &bytes(&[(2, "alan"), (1, "ada")]),
                &bytes(&[(3, "grace"), (2, "alan"), (1, "ada")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_reorder_is_not_an_append() {
        assert_eq!(
            appended_rows(
                &bytes(&[(1, "ada"), (2, "alan")]),
                &bytes(&[(2, "alan"), (1, "ada")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn the_null_placeholder_is_not_an_append() {
        // The bytes a topic is registered with, before any materialisation.
        assert_eq!(appended_rows(b"null", &bytes(&[(1, "ada")]), "id"), None);
        assert_eq!(appended_rows(b"", &bytes(&[(1, "ada")]), "id"), None);
    }

    #[test]
    fn a_record_without_the_key_column_is_refused() {
        let previous = bytes(&[(1, "ada")]);
        let mut next: Vec<Value> = rows(&[(1, "ada")]).as_array().unwrap().clone();
        next.push(json!({ "author": "keyless" }));
        let next = serde_json::to_vec(&Value::Array(next)).unwrap();
        assert_eq!(appended_rows(&previous, &next, "id"), None);
    }

    #[test]
    fn an_appended_key_that_already_exists_is_refused() {
        // Upholds `index_by_key`'s rule: a key column that does not identify makes
        // every later apply guesswork. The shortcut cannot index the previous rows
        // without the parse it exists to avoid, so it looks for the key's
        // serialised fragment in the previous bytes instead.
        let previous = bytes(&[(1, "ada"), (2, "alan")]);
        let mut next: Vec<Value> = rows(&[(1, "ada"), (2, "alan")]).as_array().unwrap().clone();
        next.push(json!({ "id": 1, "author": "duplicate" }));
        let next = serde_json::to_vec(&Value::Array(next)).unwrap();
        assert_eq!(appended_rows(&previous, &next, "id"), None);
    }

    #[test]
    fn two_appended_rows_sharing_a_key_are_refused() {
        let previous = bytes(&[(1, "ada")]);
        let mut next: Vec<Value> = rows(&[(1, "ada")]).as_array().unwrap().clone();
        next.push(json!({ "id": 7, "author": "first" }));
        next.push(json!({ "id": 7, "author": "second" }));
        let next = serde_json::to_vec(&Value::Array(next)).unwrap();
        assert_eq!(appended_rows(&previous, &next, "id"), None);
    }

    /// The shortcut and the diff must agree about what changed — the shortcut is
    /// an optimisation, not a second opinion.
    #[test]
    fn the_shortcut_agrees_with_the_diff_it_replaces() {
        let previous = bytes(&[(1, "ada"), (2, "alan")]);
        let next = bytes(&[(1, "ada"), (2, "alan"), (3, "grace")]);

        let shortcut: Vec<String> = appended_rows(&previous, &next, "id")
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();

        let diffed: Vec<String> = diff_records(
            &serde_json::from_slice(&previous).unwrap(),
            &serde_json::from_slice(&next).unwrap(),
            "id",
        )
        .unwrap()
        .into_iter()
        .filter(|change| change.weight > 0)
        .map(|change| change.key)
        .collect();

        assert_eq!(shortcut, diffed);
    }

    // ── Positioned insert (C3) ───────────────────────────────────────
    //
    // The shape between `is_tail_append` and the whole-set fallback. Every
    // rejection below is a transition `SlotInsert` cannot reproduce, and the
    // caller ships the full ordered set for it — a false positive here strands a
    // row at the wrong index permanently, so these lean hard on rejection.

    #[test]
    fn a_head_insert_names_the_old_head_as_its_anchor() {
        // The reverse-chron case: `created_at DESC` puts every new row first.
        let insert = classify_positioned_insert(
            &rows(&[(2, "alan"), (1, "ada")]),
            &rows(&[(3, "grace"), (2, "alan"), (1, "ada")]),
            "id",
        )
        .expect("a pure head insert is a positioned insert");

        assert_eq!(insert.keys, vec!["3"]);
        assert_eq!(insert.before.as_deref(), Some("2"));
    }

    #[test]
    fn a_mid_list_insert_names_the_row_it_lands_before() {
        let insert = classify_positioned_insert(
            &rows(&[(1, "ada"), (3, "grace")]),
            &rows(&[(1, "ada"), (2, "alan"), (3, "grace")]),
            "id",
        )
        .unwrap();

        assert_eq!(insert.keys, vec!["2"]);
        assert_eq!(insert.before.as_deref(), Some("3"));
    }

    #[test]
    fn a_contiguous_run_keeps_its_order_under_one_anchor() {
        let insert = classify_positioned_insert(
            &rows(&[(1, "ada"), (9, "zoe")]),
            &rows(&[(1, "ada"), (2, "alan"), (3, "grace"), (9, "zoe")]),
            "id",
        )
        .unwrap();

        assert_eq!(insert.keys, vec!["2", "3"], "run order is `next` order");
        assert_eq!(insert.before.as_deref(), Some("9"));
    }

    #[test]
    fn a_tail_insert_has_no_anchor() {
        // `row_update` catches this as a tail append first, so the `None` arm is
        // not reached in production — but the classifier is a pure function and
        // must still be right on its own terms.
        let insert = classify_positioned_insert(
            &rows(&[(1, "ada")]),
            &rows(&[(1, "ada"), (2, "alan")]),
            "id",
        )
        .unwrap();

        assert_eq!(insert.keys, vec!["2"]);
        assert_eq!(insert.before, None);
    }

    #[test]
    fn two_runs_at_different_positions_are_refused() {
        // One op names one anchor. Splitting this into two would let a partial
        // apply land; the whole set is the honest answer.
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (5, "zoe")]),
                &rows(&[(0, "grace"), (1, "ada"), (2, "alan"), (5, "zoe")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn an_insert_alongside_a_retraction_is_refused() {
        // `SlotInsert` retracts nothing, so the dropped row would linger.
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (2, "alan")]),
                &rows(&[(3, "grace"), (2, "alan")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn an_insert_alongside_an_edit_is_refused() {
        // An edit is a `−`/`+` pair; this op upserts but cannot retract, and the
        // edited row is not part of the run.
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (2, "alan")]),
                &rows(&[(3, "grace"), (1, "ada"), (2, "turing")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_reorder_of_survivors_is_refused() {
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (2, "alan")]),
                &rows(&[(3, "grace"), (2, "alan"), (1, "ada")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_pure_reorder_inserts_nothing_and_is_refused() {
        // No new key: not this shape. Must not be mistaken for an empty insert.
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (2, "alan")]),
                &rows(&[(2, "alan"), (1, "ada")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn an_unchanged_collection_is_refused() {
        assert_eq!(
            classify_positioned_insert(
                &rows(&[(1, "ada"), (2, "alan")]),
                &rows(&[(1, "ada"), (2, "alan")]),
                "id",
            ),
            None,
        );
    }

    #[test]
    fn a_non_array_previous_is_refused() {
        // The `null` placeholder a topic is registered with — no pre-state to
        // position against.
        assert_eq!(
            classify_positioned_insert(&Value::Null, &rows(&[(1, "ada")]), "id"),
            None,
        );
    }

    #[test]
    fn duplicate_keys_are_refused() {
        let previous = rows(&[(1, "ada")]);
        let next = Value::Array(vec![
            json!({ "id": 2, "author": "alan" }),
            json!({ "id": 1, "author": "ada" }),
            json!({ "id": 1, "author": "ada" }),
        ]);
        assert_eq!(classify_positioned_insert(&previous, &next, "id"), None);
    }

    #[test]
    fn the_first_insert_into_an_empty_collection_has_no_anchor() {
        let insert =
            classify_positioned_insert(&Value::Array(vec![]), &rows(&[(1, "ada")]), "id").unwrap();
        assert_eq!(insert.keys, vec!["1"]);
        assert_eq!(insert.before, None);
    }

    #[test]
    fn projection_reads_only_the_inserted_rows() {
        // The property that lets this ride the PerRecord fast path: `rows` holds
        // ONLY the changed record, as a partial render produces, and projection
        // still succeeds.
        let insert = classify_positioned_insert(
            &rows(&[(2, "alan"), (1, "ada")]),
            &rows(&[(3, "grace"), (2, "alan"), (1, "ada")]),
            "id",
        )
        .unwrap();

        let projected =
            project_inserted_rows(&insert, &rendered(&[(3, "grace")])).expect("partial render");
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].key, RowKey("3".to_string()));
        assert_eq!(
            String::from_utf8(projected[0].payload.clone()).unwrap(),
            "<li data-albedo-key=\"3\">grace</li>"
        );
    }

    #[test]
    fn projection_is_all_or_nothing_when_a_row_is_missing() {
        // The render and the classifier disagree about what was inserted —
        // placing the renderable subset would assert rows the snapshot doesn't.
        let insert = classify_positioned_insert(
            &rows(&[(9, "zoe")]),
            &rows(&[(2, "alan"), (3, "grace"), (9, "zoe")]),
            "id",
        )
        .unwrap();

        assert_eq!(project_inserted_rows(&insert, &rendered(&[(2, "alan")])), None);
    }
}
