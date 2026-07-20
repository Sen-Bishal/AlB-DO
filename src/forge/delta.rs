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

use crate::ir::opcode::{RowKey, SlotChange};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// The rendered rows of one collection: `RowKey → the row's outer HTML`.
///
/// Keys are byte-identical to the `data-albedo-key` the markup carries, because
/// they were read back out of it — that is what lets a delta name a row the
/// client can actually find.
pub type RenderedRows = BTreeMap<String, String>;

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
/// Reordering alone is deliberately **not** expressed: two collections with the
/// same records in a different order produce an empty delta, because the wire
/// has no "move" and a synthesised remove/insert pair would destroy the node
/// identity this path exists to preserve. Ordering is rung 3 (`sort`) on the
/// operator ladder and needs an insert-position on the wire to be done right.
#[must_use]
pub fn diff_records(previous: &Value, next: &Value, key_column: &str) -> Option<Vec<RecordChange>> {
    let previous = index_by_key(previous.as_array()?, key_column)?;
    let next_rows = next.as_array()?;
    let next_index = index_by_key(next_rows, key_column)?;

    let mut changes = Vec::new();

    // 1. Retractions, in previous order.
    for (key, record) in &previous {
        if !next_index.iter().any(|(next_key, _)| next_key == key) {
            changes.push(RecordChange {
                weight: -1,
                key: key.clone(),
                record: (*record).clone(),
            });
        }
    }

    // 2. Insertions and updates, in next order.
    for (key, record) in &next_index {
        match previous.iter().find(|(prev_key, _)| prev_key == key) {
            Some((_, before)) if before == record => {}
            Some((_, before)) => {
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
}
