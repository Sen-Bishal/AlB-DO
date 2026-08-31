//! A1 · host-object bridge — running TSX **handlers** under QuickJS.
//!
//! SSR already runs through [`crate::runtime::quickjs_engine::QuickJsEngine`]
//! (`ServerRenderer<QuickJsEngine>` is the live `albedo serve` path). What did
//! *not* run through QuickJS were event handlers and server `action()` bodies:
//! those went through the pure-Rust [`crate::runtime::eval`] interpreter, which
//! models only a subset of JS. A handler with a `for`/`while`/`try`, an array
//! method, or any construct the interpreter rejects could not execute.
//!
//! This module promotes handlers to the real engine. The contract is
//! deliberately narrow and **pure** — it knows nothing about the server's
//! `SlotStore` or `BroadcastRegistry`. A handler invocation carries:
//!
//!   * the handler **body** as JS source,
//!   * the in-scope **value bindings** (state/props/captured consts) as JSON,
//!   * the **setter → [`SlotId`]** map (so `setCount(x)` lowers to a slot write),
//!   * an optional **event** payload exposed to the body as `event`.
//!
//! Running it yields a `Vec<`[`HandlerEffect`]`>`: the slot writes and
//! broadcasts the body performed, in source order. Each effect lowers to the
//! exact [`Instruction::SlotSet`] opcode the action dispatcher already drains
//! and ships, so the wire shape is byte-identical to the pure-Rust path. The
//! server-side wiring that maps these effects onto the real `SlotStore` /
//! `BroadcastRegistry` (cross-session fan-out) is a separate, thin layer.
//!
//! ## Why collect effects in JS rather than via host FFI
//!
//! The body pushes into a plain JS array (`__albedo_effects`) which we read back
//! as one JSON string through the same envelope the renderer uses. No
//! `Function::new` host closures, no `Rc<RefCell<…>>` captured across the FFI
//! boundary, no per-call closure lifetime juggling — just code generation plus a
//! single `eval`. The effect ordering the body produced is preserved exactly.

use crate::ir::opcode::{Instruction, SlotId};
use crate::runtime::broadcast::broadcast_slot_id;
use crate::runtime::engine::{RuntimeError, RuntimeResult};
use serde::Deserialize;
use serde_json::{Map, Value};

/// One side effect a handler body produced, in source order.
///
/// Both variants lower to an [`Instruction::SlotSet`] — the client cannot tell
/// a per-session state write from a broadcast write, exactly as the existing
/// broadcast fan-out intends (see [`crate::runtime::broadcast`]). The
/// distinction is preserved here so the server layer can *also* route a
/// [`HandlerEffect::Broadcast`] to the topic registry for cross-session
/// fan-out; the pure-Rust SlotSet lowering is only the current session's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerEffect {
    /// A `setX(value)` call: write the JSON-encoded `value` to `slot_id`.
    SlotSet { slot_id: SlotId, value: Vec<u8> },
    /// A `broadcast(topic, value)` call. `slot_id` is the deterministic
    /// broadcast slot derived from `topic`, so the current session's opcode is
    /// a `SlotSet` on it; the server layer fans the same value out to every
    /// other subscriber of `topic`.
    Broadcast {
        topic: String,
        slot_id: SlotId,
        value: Vec<u8>,
    },
    /// FORGE · an `append(collection, record)` call: a **durable** write.
    ///
    /// Unlike its siblings this carries no `slot_id` and lowers to no opcode.
    /// It cannot: the row is not state this session already holds, it is a
    /// request for the server to change the database. The value a subscriber
    /// eventually sees comes from rematerialising the collection *after* the
    /// write commits (`crate::forge::write`), not from echoing back what the
    /// body passed in — which would announce a row that might never land.
    ForgeAppend {
        collection: String,
        record: serde_json::Map<String, Value>,
    },
    /// FORGE · an `update(collection, key, fields)` call. Like `ForgeAppend`,
    /// durable and opcode-free; the row is identified by `key` (a scalar) and
    /// only the columns in `fields` change.
    ForgeUpdate {
        collection: String,
        key: Value,
        fields: serde_json::Map<String, Value>,
    },
    /// FORGE · a `remove(collection, key)` call: a durable delete of the row
    /// identified by `key`.
    ForgeDelete { collection: String, key: Value },
}

impl HandlerEffect {
    /// Lowers this effect to the opcode the action dispatcher ships, when it has
    /// one. `SlotSet` and `Broadcast` both become a `SlotSet` carrying the JSON
    /// value bytes; [`HandlerEffect::ForgeAppend`] returns `None` — a durable
    /// write is applied server-side and reported by the topic's post-commit
    /// fan-out, so there is nothing to send this session inline.
    #[must_use]
    pub fn into_instruction(self) -> Option<Instruction> {
        match self {
            HandlerEffect::SlotSet { slot_id, value } => {
                Some(Instruction::SlotSet { slot_id, value })
            }
            HandlerEffect::Broadcast { slot_id, value, .. } => {
                Some(Instruction::SlotSet { slot_id, value })
            }
            HandlerEffect::ForgeAppend { .. }
            | HandlerEffect::ForgeUpdate { .. }
            | HandlerEffect::ForgeDelete { .. } => None,
        }
    }

    /// The slot id this effect writes, when it writes one.
    #[must_use]
    pub fn slot_id(&self) -> Option<SlotId> {
        match self {
            HandlerEffect::SlotSet { slot_id, .. } | HandlerEffect::Broadcast { slot_id, .. } => {
                Some(*slot_id)
            }
            HandlerEffect::ForgeAppend { .. }
            | HandlerEffect::ForgeUpdate { .. }
            | HandlerEffect::ForgeDelete { .. } => None,
        }
    }
}

/// A handler ready to run under QuickJS.
///
/// Borrows everything; build one per dispatch. `body` is JS source already
/// stripped of TS/JSX (the same SWC pipeline the engine uses for modules
/// produces it). `is_block` distinguishes a statement block (`{ … }`) from a
/// single expression (`setCount(count + 1)`).
#[derive(Debug, Clone)]
pub struct HandlerInvocation<'a> {
    /// JS source of the handler body.
    pub body: &'a str,
    /// `true` when `body` is a brace-delimited statement block; `false` for a
    /// single expression.
    pub is_block: bool,
    /// In-scope value bindings (state values, captured props, module consts),
    /// name → current JSON value. Seeded as mutable `let`s so a body that
    /// reassigns a local stays valid JS.
    pub env: &'a Map<String, Value>,
    /// Bindings whose seed is an **engine-trusted JS expression** rather than a
    /// JSON value — used for `useState` initials and module constants that come
    /// from the compiler's own codegen of the source AST, not from request
    /// data. Seeded as mutable `let`s like [`Self::env`]; the expression source
    /// is spliced verbatim, so callers must only ever pass code they produced.
    pub raw_bindings: &'a [(String, String)],
    /// Setter name → the slot it writes. `setCount` becomes
    /// `const setCount = v => <push SlotSet(slot, v)>`.
    pub setters: &'a [(String, SlotId)],
    /// Optional event payload exposed to the body as the global `event`.
    pub event_json: Option<&'a str>,
    /// Pre-write snapshot of broadcast topic values as **raw JSON bytes**
    /// (topic → the topic's stored encoding), used to resolve updater-form
    /// `broadcast(topic, fn)` calls inside JS: the builtin reads the current
    /// value here, applies `fn`, and writes the new value back into the snapshot
    /// so a later updater for the same topic in the same body chains correctly.
    /// A topic absent from the list is treated as `null` (first-call default).
    /// Empty for value-only handlers.
    ///
    /// **Bytes, not `Value`, deliberately** — this is exactly what
    /// [`crate::runtime::BroadcastRegistry::snapshot_values`] returns, and a
    /// topic's stored bytes are already its JSON encoding. Materializing them
    /// into a `serde_json::Value` here bought nothing: the only consumer
    /// (`build_handler_script`) immediately re-encodes them back to JSON text to
    /// splice into the script. See `OPTIMIZATIONS.md` § 7.
    pub broadcast_current: &'a [(String, Vec<u8>)],
    /// APERTURE A2 · the workflow journal as
    /// [`crate::aperture::Journal::to_script_value`] encodes it — a dense array
    /// indexed by step.
    ///
    /// `None` seeds an empty log, which is the first pass of every dispatch and
    /// also the permanent state of a body that never calls out. The `fetch`
    /// builtin is installed either way: a body that does not fetch pays one
    /// closure definition, and making its presence conditional would mean the
    /// engine's surface depended on a static analysis that can be wrong.
    pub journal: Option<&'a Value>,
}

/// One outbound call a suspended body is waiting on.
///
/// The body described it; nothing has been sent. That distinction is what makes
/// the compiler's hoisting sound (§ 11 R1.3) — requests are *staged*, so
/// issuing two of them together reorders nothing observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequest {
    /// Journal position. **This is the idempotency key** once paired with the
    /// workflow id (§ 5.3), so it is carried from JS rather than re-derived.
    pub step: u32,
    /// Uppercased HTTP method.
    pub method: String,
    /// The URL exactly as the body wrote it.
    pub url: String,
    /// Request body, if the call carried one.
    pub body: Option<String>,
    /// Headers the body supplied. **Not covered by [`Self::digest`]** — § 11 R6:
    /// credentials must not reach the journal.
    pub headers: Vec<(String, String)>,
    /// Digest of method + URL + body, computed by the body's own pass. A later
    /// pass producing a different digest at the same step has diverged.
    pub digest: String,
}

/// The result of one pass of a handler body.
///
/// A pass either finishes or asks for I/O. Making that an enum rather than an
/// error case is the point: a suspension is an ordinary, expected outcome, and
/// treating it as a failure is how a framework ends up committing the effects of
/// a body that never actually ran to completion.
#[derive(Debug)]
pub enum HandlerRun {
    /// The body ran to completion. Its effects may commit.
    Completed(HandlerOutcome),
    /// The body needs these calls resolved, then wants to be run again.
    ///
    /// **No effects come back with this.** `__albedo_effects` is rebuilt per
    /// pass, so a discarded pass discards its effects — the property § 5.4 leans
    /// on for "effects cannot double-apply". It is not added here; it is what
    /// the existing design already did.
    Suspended {
        /// Everything the body asked for in this pass, in step order.
        pending: Vec<PendingRequest>,
        /// How many steps the body had already read back when it suspended.
        /// A pass that suspends without consuming its whole seeded journal is a
        /// body that took a different path — caught by the driver, not here.
        journal_len: u32,
    },
}

/// Raw shape the generated script emits per effect; decoded then lowered.
#[derive(Debug, Deserialize)]
struct RawEffect {
    kind: String,
    slot_id: Option<u32>,
    topic: Option<String>,
    /// Row key for `forge_update` / `forge_delete`; absent for the others.
    #[serde(default)]
    key: Option<Value>,
    /// Absent for `forge_delete` (a delete carries no value). `#[serde(default)]`
    /// makes it `Value::Null` there rather than a decode error.
    #[serde(default)]
    value: Value,
    /// SANDGATE-B · the module whose factory body was running when this effect
    /// was recorded, or `None` for the ordinary case (the application's own
    /// handler). Stamped from the sealed provenance stack.
    #[serde(default)]
    origin: Option<String>,
}

/// Raw shape of one staged request, as the `fetch` builtin pushes it.
#[derive(Debug, Deserialize)]
struct RawPending {
    step: u32,
    method: String,
    url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    headers: Vec<(String, String)>,
    digest: String,
}

#[derive(Debug, Deserialize)]
struct HandlerEnvelope {
    ok: bool,
    /// A2 · present when the pass suspended: a JSON-encoded `Vec<RawPending>`,
    /// double-encoded for the same reason `value` is.
    #[serde(default)]
    suspend: Option<String>,
    /// A2 · how many journal steps were seeded into the pass that suspended.
    #[serde(default)]
    journal_len: Option<u32>,
    /// On success: a JSON-encoded `Vec<RawEffect>` (double-encoded so the outer
    /// render envelope stays a flat `{ok, value, error}` shape).
    value: Option<String>,
    /// On success: the handler body's *completion value*, JSON-encoded (double-
    /// encoded, same reason as `value`). `null` when the body returns nothing.
    /// Server-side form dispatch projects a `{ error: { field: msg } }` result
    /// onto the form's compile-time `data-albedo-error` slots; other callers
    /// ignore it. Optional so pre-P6 envelopes (no `result` key) still decode.
    result: Option<String>,
    error: Option<String>,
    /// SANDGATE-B · the realm's own answer to *"is anything hooked into
    /// serialisation right now?"*, read through the sealed holder a package
    /// cannot replace. `None` on an envelope from a pre-SANDGATE prelude;
    /// `Some(reason)` means the run is refused. See
    /// [`crate::runtime::confinement::integrity_probe_expression`].
    #[serde(default)]
    integrity: Option<String>,
}

/// What a handler body produced: its side-effects (setter/broadcast calls) and,
/// for form actions, its return value. Effects always drive slot writes; the
/// result is the userland return the server projects onto pre-allocated DOM
/// slots (see `crates/albedo-server/src/render/form_result.rs`).
#[derive(Debug)]
pub struct HandlerOutcome {
    pub effects: Vec<HandlerEffect>,
    pub result: Option<Value>,
}

/// `true` for a valid JS identifier (the binding/setter names we splice into
/// generated source). We refuse anything else loudly rather than risk emitting
/// malformed or injectable source — a non-identifier binding name is a bug in
/// the caller, not user input to tolerate.
fn is_js_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

/// Serialize a JSON value to a JS literal safe to splice into source. We go
/// through `serde_json`, which emits a strict JSON subset of JS expression
/// syntax — valid as a right-hand side. `<`/`>`/`&` etc. inside strings are
/// fine here (this is a `<script>`-free `eval`, not HTML).
fn js_literal(value: &Value) -> RuntimeResult<String> {
    serde_json::to_string(value).map_err(|err| {
        RuntimeError::render(format!(
            "failed to encode handler binding as JS literal: {err}"
        ))
    })
}

/// Builds the self-contained IIFE that seeds bindings, installs setters and the
/// `broadcast` builtin, runs the body, and returns a `{ok, value, error}`
/// envelope whose `value` is the JSON-encoded effect list.
pub(crate) fn build_handler_script(inv: &HandlerInvocation) -> RuntimeResult<String> {
    let mut script = String::new();
    script.push_str("(function(){\n");

    // A2 · the suspend state is declared OUTSIDE the try, because the catch
    // reads it. `let`/`const` are block-scoped, so declaring these beside the
    // effect list — inside the try, where they naturally belong — makes the
    // epilogue's own catch throw a ReferenceError and turns every handler that
    // throws anything at all into an opaque engine exception.
    script.push_str("let __albedo_suspended=false;\n");
    script.push_str("const __albedo_pending=[];\n");
    script.push_str("const __ALBEDO_SUSPEND={__albedo_suspend:true};\n");
    // Recognising the sentinel is a named function because the R3 catch fold
    // calls it from inside every userland `catch` — see `transforms::workflow`.
    // Identity is checked first and the marker property second, so a sentinel
    // that crossed a bundle boundary and lost its identity is still recognised.
    script.push_str(
        "const __albedo_is_suspend=function(e){return e===__ALBEDO_SUSPEND||(e!==null&&typeof e==='object'&&e.__albedo_suspend===true);};\n",
    );
    // SANDGATE-B · the sealed holder is bound OUTSIDE the try for the same
    // reason `__albedo_suspended` is: the catch block encodes its envelope
    // through it, and a `const` declared inside the try is not in scope there —
    // which would turn every handler that throws into an opaque ReferenceError.
    script.push_str("const __albedo_S=globalThis.__albedo_sealed;\n");
    script.push_str("const __albedo_journal=");
    match inv.journal {
        Some(journal) => script.push_str(&js_literal(journal)?),
        None => script.push_str("[]"),
    }
    script.push_str(";\n");

    script.push_str("try{\n");
    // SANDGATE-B · the effect channel is assembled through the SEALED holder,
    // never through the realm's own `JSON` or `Array.prototype`.
    //
    // 🔴 The attack this replaces: a package patches `JSON.stringify`, and the
    // epilogue below used to hand it the whole effect array to encode. The
    // patched function returned text for an array it had appended a
    // `forge_append` to, and the server executed it. Confinement does not close
    // that — the victim route re-imports the package, so the patch is
    // re-applied *before* the handler it attacks (gate 2, row 3).
    //
    // Three properties make the list unforgeable, and all three are needed:
    //  1. every effect is a NULL-PROTOTYPE record, so encoding one cannot pick
    //     up an inherited `toJSON`;
    //  2. each is encoded at push time through the PRISTINE `stringify`
    //     captured before any package ran;
    //  3. the list is assembled by STRING CONCATENATION over those encoded
    //     entries, so no array — and therefore no `Array.prototype` hook and no
    //     second `stringify` call on an object — is on the path at all.
    script.push_str("const __albedo_effect_json=__albedo_S.record();\n");
    script.push_str("let __albedo_effect_n=0;\n");
    // `origin` is the sealed provenance stack's current frame: the npm linker
    // pushes a module key around a factory body, so an effect recorded while
    // third-party top-level code runs is attributable. For an ordinary handler
    // it is `null`, which is the honest answer.
    script.push_str(
        "const __albedo_emit=function(rec){rec.origin=__albedo_S.currentOrigin();__albedo_effect_json[__albedo_effect_n]=__albedo_S.stringify(rec);__albedo_effect_n=__albedo_effect_n+1;};\n",
    );
    script.push_str(
        "const __albedo_rec=function(kind){var r=__albedo_S.record();r.kind=kind;return r;};\n",
    );
    script.push_str(
        "const __albedo_effects_json=function(){var s='[';for(var i=0;i<__albedo_effect_n;i++){if(i>0){s+=',';}s+=__albedo_effect_json[i];}return s+']';};\n",
    );

    // Pre-write snapshot of topic values, so updater-form `broadcast(topic, fn)`
    // can read the current value. Always defined (at least `{}`) since the
    // builtin references it. A strict-JSON object literal is valid JS.
    //
    // Built straight from the stored bytes. A topic's value IS its JSON
    // encoding, so the old `bytes → Value → to_string` round-trip walked the
    // same data three times (parse + allocate a tree, deep-clone the map,
    // re-encode) to arrive at text equivalent to the bytes we started with.
    // Measured at 1.011 ms for a 2,000-row collection on **every** action whose
    // body mentions `broadcast`, paid for topics the body never reads —
    // `OPTIMIZATIONS.md` § 7.
    script.push_str("const __albedo_topic_current={");
    for (index, (topic, bytes)) in inv.broadcast_current.iter().enumerate() {
        if index > 0 {
            script.push(',');
        }
        // The key goes through the JSON encoder: a topic is userland-authored
        // text and may contain quotes or backslashes.
        script.push_str(&js_literal(&Value::String(topic.clone()))?);
        script.push(':');
        // Validate before splicing — unvalidated bytes would turn one corrupt
        // topic into a SyntaxError that fails the whole action, where the old
        // path degraded that topic to `null` and ran everything else.
        // `IgnoredAny` walks the JSON without building a tree, so this keeps the
        // graceful-degradation property at a fraction of the cost. A successful
        // parse also proves the bytes are UTF-8, which is why `from_utf8` below
        // cannot realistically fail.
        let valid = !bytes.is_empty()
            && serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok();
        match valid.then(|| std::str::from_utf8(bytes).ok()).flatten() {
            Some(json) => script.push_str(json),
            None => script.push_str("null"),
        }
    }
    script.push_str("};\n");

    // `broadcast(topic, value)` records a broadcast effect. The second argument
    // may be a plain value (value form) or an updater function (React
    // `setState(fn)` form): for a function we read the current topic value from
    // the snapshot (defaulting to `null` for an unseen topic), apply the
    // updater, and write the result back so a subsequent updater for the same
    // topic in this body sees it — matching the pure-Rust read-modify-write.
    // The setter helpers push raw values; the outer JSON.stringify of the whole
    // array encodes them once.
    script.push_str(
        "const broadcast=function(topic,value){var __t=String(topic);var __v;if(typeof value==='function'){var __cur=Object.prototype.hasOwnProperty.call(__albedo_topic_current,__t)?__albedo_topic_current[__t]:null;__v=value(__cur);}else{__v=value;}if(__v===undefined)__v=null;__albedo_topic_current[__t]=__v;var __r=__albedo_rec('broadcast');__r.topic=__t;__r.value=__v;__albedo_emit(__r);};\n",
    );

    // FORGE · `append(collection, record)` — the durable write builtin, defined
    // beside `broadcast` because it is the same idea: a body describes an effect
    // and the server performs it.
    //
    // It records ONLY. No `__albedo_topic_current` update and no echo of the
    // record: unlike `broadcast`, whose value IS the new state, an append's
    // visible result is whatever the collection looks like once the row commits
    // — which only the server, post-commit, can say. Guessing here would show a
    // row that a failed write never created.
    //
    // Throws on a non-object record so the author hears about it at the call
    // site rather than through a server-side type error later.
    // Emitted in the existing `{kind, topic, value}` shape rather than inventing
    // fields: the collection IS the topic (a persistent collection is a topic
    // materialised from the substrate), and the record is the value.
    script.push_str(
        "const append=function(collection,record){if(record===null||typeof record!=='object'||Array.isArray(record)){throw new TypeError('append(collection, record): record must be an object');}var __r=__albedo_rec('forge_append');__r.topic=String(collection);__r.value=record;__albedo_emit(__r);return null;};\n",
    );
    // `update(collection, key, fields)` and `remove(collection, key)` — the
    // other two durable mutations, same effect-recording discipline as append.
    // `key` must be a scalar (string/number/boolean) that identifies one row;
    // `fields` a partial record. Both throw at the call site on a bad shape so
    // the author hears it there, not through a server type error later. Carried
    // in the same `{kind, topic, value}` envelope, with the key alongside.
    //
    // The delete builtin is named `remove`, not `delete`: `delete` is a JS
    // reserved word (the delete operator), so `delete(coll, key)` cannot parse
    // as a call in either the QuickJS engine or the swc-based pure-Rust
    // evaluator. `remove` is the ergonomic non-reserved name both paths accept.
    script.push_str(
        "const __albedo_forge_key=function(name,key){if(key===null||typeof key==='object'){throw new TypeError(name+'(collection, key): key must be a string, number, or boolean');}return key;};\n",
    );
    script.push_str(
        "const update=function(collection,key,fields){if(fields===null||typeof fields!=='object'||Array.isArray(fields)){throw new TypeError('update(collection, key, fields): fields must be an object');}var __r=__albedo_rec('forge_update');__r.topic=String(collection);__r.key=__albedo_forge_key('update',key);__r.value=fields;__albedo_emit(__r);return null;};\n",
    );
    script.push_str(
        "const remove=function(collection,key){var __r=__albedo_rec('forge_delete');__r.topic=String(collection);__r.key=__albedo_forge_key('remove',key);__albedo_emit(__r);return null;};\n",
    );

    // ── APERTURE A2 · the suspend protocol ───────────────────────────────
    //
    // `fetch()` cannot block: the engine must be released across the round trip
    // (invariant 2.6 — a blocking host function holds an engine, its arena and a
    // worker for the whole RTT, which gate 5 measured at 403.9 ms against
    // 52.7 ms for this design, and no pool size fixes it because the engine is
    // the thing waiting).
    //
    // So a call is a JOURNAL STEP. On a hit the body reads the recorded answer;
    // on a miss it records the intent and throws a sentinel, and Rust resolves
    // every request the pass asked for, appends the outcomes, and runs the body
    // again. Suspend-replay-with-memoisation is event sourcing, which is why the
    // same machinery buys crash recovery and exactly-once (§ 5.1).
    script.push_str("let __albedo_step=0;\n");

    // FNV-1a-32 over the canonical request text. The same hash family the wire
    // slot ids use, and it is a CONSISTENCY check, not a security one: it
    // catches a replay that asks for something different at the same step
    // (§ 10), where the adversary is the author's own nondeterminism. It runs on
    // UTF-16 code units, which is fine for that job and cheap in QuickJS.
    script.push_str(
        "const __albedo_digest=function(s){var h=2166136261;for(var i=0;i<s.length;i++){h^=s.charCodeAt(i);h=Math.imul(h,16777619);}return (h>>>0).toString(16);};\n",
    );

    // Headers travel with the request and are NEVER digested or journaled
    // (§ 11 R6): a journal dump must not be a credential dump.
    script.push_str(
        "const __albedo_headers=function(init){var out=[];if(!init||!init.headers)return out;var h=init.headers;if(Array.isArray(h)){for(var i=0;i<h.length;i++){out.push([String(h[i][0]),String(h[i][1])]);}return out;}for(var k in h){if(Object.prototype.hasOwnProperty.call(h,k)){out.push([String(k),String(h[k])]);}}return out;};\n",
    );

    // The response a recorded step replays as. Shaped like the web platform's
    // because § 5.5 requires copy-pasted vendor code to run verbatim — but
    // `.json()` and `.text()` return values rather than promises, which is
    // invisible to a body whose `await` the compiler already lowered away.
    script.push_str(
        "const __albedo_response=function(rec){var headers=rec.headers||{};return {status:rec.status,ok:rec.status>=200&&rec.status<300,url:rec.url,headers:{get:function(n){var k=String(n).toLowerCase();return Object.prototype.hasOwnProperty.call(headers,k)?headers[k]:null;}},text:function(){return rec.body;},json:function(){return JSON.parse(rec.body);}};};\n",
    );

    // `fetch(url, init)` — one journal step.
    //
    // The step index is taken BEFORE anything can throw, so the same call site
    // occupies the same index on every pass. That is what keeps a derived
    // idempotency key stable across a retry (§ 5.3) and what makes a divergent
    // replay detectable rather than silently re-keyed.
    script.push_str(
        "const fetch=function(url,init){\
var step=__albedo_step++;\
var method=(init&&init.method)?String(init.method).toUpperCase():'GET';\
var target=String(url);\
var body=null;\
if(init&&init.body!==undefined&&init.body!==null){body=(typeof init.body==='string')?init.body:JSON.stringify(init.body);}\
var digest=__albedo_digest(method+'\\n'+target+'\\n'+(body===null?'':body));\
var recorded=(step<__albedo_journal.length)?__albedo_journal[step]:null;\
if(recorded){\
if(recorded.d!==digest){throw new Error('albedo: this action asked for a different request at step '+step+' when it re-ran. A body that calls out must ask for the same things in the same order every time it runs.');}\
if(recorded.ok===true){return __albedo_response(recorded.v);}\
throw new Error(recorded.e);\
}\
__albedo_pending.push({step:step,method:method,url:target,body:body,headers:__albedo_headers(init),digest:digest});\
__albedo_suspended=true;\
throw __ALBEDO_SUSPEND;\
};\n",
    );

    // Seed engine-trusted raw-JS bindings first (useState initials, module
    // constants). A later store-backed JSON binding for the same name shadows
    // the initial, which is correct: a written slot is newer than its initial.
    for (name, expr_src) in inv.raw_bindings {
        if !is_js_identifier(name) {
            return Err(RuntimeError::render(format!(
                "handler binding name '{name}' is not a valid JavaScript identifier"
            )));
        }
        script.push_str(&format!("let {name}=({expr_src});\n"));
    }

    // Seed value bindings as mutable lets so a body may reassign locals.
    for (name, value) in inv.env {
        if !is_js_identifier(name) {
            return Err(RuntimeError::render(format!(
                "handler binding name '{name}' is not a valid JavaScript identifier"
            )));
        }
        script.push_str(&format!("let {name}={};\n", js_literal(value)?));
    }

    // Install setters bound to their slot ids.
    for (name, slot_id) in inv.setters {
        if !is_js_identifier(name) {
            return Err(RuntimeError::render(format!(
                "handler setter name '{name}' is not a valid JavaScript identifier"
            )));
        }
        script.push_str(&format!(
            "const {name}=function(v){{var __r=__albedo_rec('slot');__r.slot_id={};__r.value=(v===undefined?null:v);__albedo_emit(__r);}};\n",
            slot_id.0
        ));
    }

    // Expose the event payload (or `null` when there is none).
    match inv.event_json {
        Some(event) if !event.trim().is_empty() => {
            script.push_str(&format!("const event=({event});\n"));
        }
        _ => script.push_str("const event=null;\n"),
    }

    // `form` — the same payload under the name a form handler actually reads.
    //
    // A submit's payload IS the form's fields, and the action extractor already
    // preserves `action(({ form, broadcast }) => …)` as the authored shape, so a
    // body naming `form` must resolve. `event` stays as the general name (an
    // input/click carries a non-form payload); this is an alias, not a rename,
    // so nothing that reads `event` changes.
    //
    // Only bound for object payloads: a click (`null`) or a typed-input string
    // is not a form, and binding it as one would let `form.author` silently read
    // `undefined` off a string instead of failing where the mistake is.
    // The pure-Rust interpreter binds `form` on the same rule — the two paths
    // must agree or a body works under one executor and not the other.
    script.push_str(
        "const form=(event!==null&&typeof event==='object'&&!Array.isArray(event))?event:undefined;\n",
    );

    // Run the body inside a nested arrow so a userland `return` is CAPTURED as
    // the action's result instead of escaping the effect-collection epilogue.
    // (Splicing a block body directly into this `try` — as before — let an early
    // `return { error: ... }` bail out of the whole wrapper, skipping the effect
    // serialization below: a form action's validation return silently produced
    // no wire output.) A block body runs as its statements; an expression body
    // is the arrow's implicit return. Effects still accumulate via the setter /
    // `broadcast` closures regardless of how the body returns.
    if inv.is_block {
        script.push_str("const __albedo_result=(function(){");
        script.push_str(inv.body);
        script.push_str("})();\n");
    } else {
        script.push_str(&format!("const __albedo_result=({});\n", inv.body));
    }

    // Two lanes: `value` = the effect list (setter/broadcast writes), `result` =
    // the body's return value. Both double-encoded so the outer envelope stays a
    // flat `{ok, value, result, error}` string shape. `undefined` normalizes to
    // `null` so the result lane is always valid JSON.
    // A2 · the suspend envelope, checked on the SUCCESS path too.
    //
    // That is § 11 R3's backstop and it is the whole reason the flag exists
    // beside the sentinel. A userland `try/catch` — or a `catch` inside an npm
    // bundle, which the AST fold cannot reach — can swallow the sentinel, and
    // the body then runs on garbage and "succeeds". Checking the flag here means
    // a swallowed sentinel degrades to *suspend anyway*, never to *commit the
    // effects of a body that never got its data*.
    script.push_str(
        "if(__albedo_suspended){return '{\"ok\":false,\"suspend\":'+__albedo_S.stringify(__albedo_S.stringify(__albedo_pending))+',\"journal_len\":'+__albedo_S.stringify(__albedo_journal.length|0)+'}';}\n",
    );
    script.push_str(
        // 🔑 The envelope itself is CONCATENATED, not `stringify`d from an
        // object literal. An object handed to `stringify` consults `toJSON`
        // through its prototype chain, so a realm carrying
        // `Object.prototype.toJSON` could have rewritten the whole envelope —
        // including any integrity field inside it, which would have made the
        // check below report on itself. Only primitives are encoded here, and
        // `stringify` skips `toJSON` for primitives.
        "var __albedo_result_json=__albedo_S.stringify(__albedo_result===undefined?null:__albedo_result);\n",
    );
    script.push_str(
        "if(typeof __albedo_result_json!=='string'){__albedo_result_json='null';}\n",
    );
    script.push_str(&format!(
        "var __albedo_integrity={probe};\n",
        probe = crate::runtime::confinement::integrity_probe_expression()
    ));
    script.push_str(
        "return '{\"ok\":true,\"value\":'+__albedo_S.stringify(__albedo_effects_json())+',\"result\":'+__albedo_S.stringify(__albedo_result_json)+',\"integrity\":'+__albedo_S.stringify(__albedo_integrity)+'}';\n",
    );
    script.push_str(
        "}catch(err){if(__albedo_suspended||__albedo_is_suspend(err)){return '{\"ok\":false,\"suspend\":'+__albedo_S.stringify(__albedo_S.stringify(__albedo_pending))+',\"journal_len\":'+__albedo_S.stringify(__albedo_journal.length|0)+'}';}const message=(err&&typeof err.message==='string')?err.message:String(err);return '{\"ok\":false,\"error\":'+__albedo_S.stringify(message)+'}';}\n",
    );
    script.push_str("})()");
    Ok(script)
}

/// Decode one pass of a handler body — completion or suspension.
///
/// Decodes the engine's raw envelope string into effects plus the body's return
/// value, mapping a JS throw to a loud [`RuntimeError`]. `entry` is only used
/// for the error message.
///
/// The suspension arm is checked **before** the error arm on purpose. Both are
/// `ok:false` on the wire (the script has one return shape), and reading them in
/// the other order would turn every outbound call into a handler crash.
pub(crate) fn decode_handler_run(
    entry: &str,
    envelope_json: &str,
) -> RuntimeResult<HandlerRun> {
    let envelope: HandlerEnvelope = serde_json::from_str(envelope_json).map_err(|err| {
        RuntimeError::render(format!(
            "failed to decode handler effect envelope for '{entry}': {err}"
        ))
    })?;

    if let Some(raw) = envelope.suspend.as_deref() {
        let staged: Vec<RawPending> = serde_json::from_str(raw).map_err(|err| {
            RuntimeError::render(format!(
                "failed to decode suspended requests for '{entry}': {err}"
            ))
        })?;
        return Ok(HandlerRun::Suspended {
            pending: staged
                .into_iter()
                .map(|p| PendingRequest {
                    step: p.step,
                    method: p.method,
                    url: p.url,
                    body: p.body,
                    headers: p.headers,
                    digest: p.digest,
                })
                .collect(),
            journal_len: envelope.journal_len.unwrap_or(0),
        });
    }

    if !envelope.ok {
        let message = envelope
            .error
            .unwrap_or_else(|| "unknown handler runtime error".to_string());
        return Err(RuntimeError::render(format!(
            "handler '{entry}' threw: {message}"
        )));
    }

    // SANDGATE-B · refuse the run if the realm is carrying a serialisation hook.
    //
    // The effect list itself is unforgeable (null-prototype records, pristine
    // `stringify`, concatenated assembly), but an effect's *payload* is
    // application data serialised as an object, and `Object.prototype.toJSON`
    // would still let a package rewrite it. Rather than defend a value the
    // application legitimately owns, refuse the whole pass: no application
    // plants `toJSON` on `Object.prototype`, so this is an attack signature and
    // not a compatibility hazard.
    //
    // 🔑 This check is only worth anything because the envelope carrying it is
    // built by string concatenation. Had it been an object handed to
    // `stringify`, the same hook it reports would have been able to rewrite the
    // report.
    if let Some(reason) = envelope.integrity.as_deref().filter(|r| !r.is_empty()) {
        return Err(RuntimeError::render(format!(
            "handler '{entry}' refused: the JS realm has '{reason}' installed, which can rewrite              serialised effect payloads. This is realm poisoning — see SANDGATE-B."
        )));
    }

    // The result lane is best-effort: a missing key (pre-P6 envelope) or a
    // decode hiccup degrades to `None` rather than failing an otherwise-good
    // dispatch — the effects still ship.
    let result = envelope
        .result
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());

    let effects_json = envelope.value.ok_or_else(|| {
        RuntimeError::render(format!(
            "handler '{entry}' returned success without effects"
        ))
    })?;
    let raw: Vec<RawEffect> = serde_json::from_str(&effects_json).map_err(|err| {
        RuntimeError::render(format!(
            "failed to decode handler effect list for '{entry}': {err}"
        ))
    })?;

    let effects = raw
        .into_iter()
        .map(|effect| lower_effect(entry, effect))
        .collect::<RuntimeResult<Vec<HandlerEffect>>>()?;

    Ok(HandlerRun::Completed(HandlerOutcome { effects, result }))
}

fn lower_effect(entry: &str, raw: RawEffect) -> RuntimeResult<HandlerEffect> {
    // SANDGATE-B · provenance, enforced rather than merely recorded.
    //
    // The sealed provenance stack carries a frame only while an npm module's
    // **factory body** is executing — the one window in which third-party code
    // runs with the linker on the stack. A handler body runs with that stack
    // empty, so every legitimate effect arrives with `origin: null`.
    //
    // 🔑 This is a **tripwire, not the defence**. Gate 4 showed the effect
    // builtins were never reachable from package code to begin with
    // (`append`/`update`/`remove` are `const`s inside the per-request handler
    // IIFE, not globals), so this should never fire. That is precisely why it
    // is worth having: if it ever does, either a builtin leaked onto
    // `globalThis` or a package found a path back into one, and both are
    // findings that would otherwise surface as a mysterious write.
    if let Some(origin) = raw.origin.as_deref().filter(|o| !o.is_empty()) {
        return Err(RuntimeError::render(format!(
            "handler '{entry}' produced a '{}' effect while the module '{origin}' was              executing. Effects may only be recorded by application code; a package              reached an effect builtin. See SANDGATE-B.",
            raw.kind
        )));
    }

    let value = serde_json::to_vec(&raw.value).map_err(|err| {
        RuntimeError::render(format!(
            "failed to encode handler effect value for '{entry}': {err}"
        ))
    })?;

    match raw.kind.as_str() {
        "slot" => {
            let slot_id = raw.slot_id.ok_or_else(|| {
                RuntimeError::render(format!("slot effect in '{entry}' is missing a slot_id"))
            })?;
            Ok(HandlerEffect::SlotSet {
                slot_id: SlotId(slot_id),
                value,
            })
        }
        "broadcast" => {
            let topic = raw.topic.ok_or_else(|| {
                RuntimeError::render(format!("broadcast effect in '{entry}' is missing a topic"))
            })?;
            let slot_id = broadcast_slot_id(&topic);
            Ok(HandlerEffect::Broadcast {
                topic,
                slot_id,
                value,
            })
        }
        // FORGE · `append(collection, record)`. Carried in the shared
        // `{topic, value}` shape: topic = collection, value = the record.
        "forge_append" => {
            let collection = raw.topic.ok_or_else(|| {
                RuntimeError::render(format!(
                    "forge_append effect in '{entry}' is missing a collection"
                ))
            })?;
            // The shim already rejects a non-object record at the call site;
            // this is the trust boundary for anything that reached us anyway.
            let record = serde_json::from_slice::<Value>(&value)
                .ok()
                .and_then(|parsed| match parsed {
                    Value::Object(map) => Some(map),
                    _ => None,
                })
                .ok_or_else(|| {
                    RuntimeError::render(format!(
                        "forge_append effect in '{entry}' for '{collection}' is not an object record"
                    ))
                })?;
            Ok(HandlerEffect::ForgeAppend { collection, record })
        }
        // FORGE · `update(collection, key, fields)`. `topic` = collection,
        // `key` = the row identity (a scalar), `value` = the partial fields.
        "forge_update" => {
            let collection = raw.topic.ok_or_else(|| {
                RuntimeError::render(format!(
                    "forge_update effect in '{entry}' is missing a collection"
                ))
            })?;
            let key = forge_scalar_key(raw.key, entry, &collection, "forge_update")?;
            let fields = serde_json::from_slice::<Value>(&value)
                .ok()
                .and_then(|parsed| match parsed {
                    Value::Object(map) => Some(map),
                    _ => None,
                })
                .ok_or_else(|| {
                    RuntimeError::render(format!(
                        "forge_update effect in '{entry}' for '{collection}' is not an object of fields"
                    ))
                })?;
            Ok(HandlerEffect::ForgeUpdate {
                collection,
                key,
                fields,
            })
        }
        // FORGE · `remove(collection, key)`. Carries no value — just the key.
        "forge_delete" => {
            let collection = raw.topic.ok_or_else(|| {
                RuntimeError::render(format!(
                    "forge_delete effect in '{entry}' is missing a collection"
                ))
            })?;
            let key = forge_scalar_key(raw.key, entry, &collection, "forge_delete")?;
            Ok(HandlerEffect::ForgeDelete { collection, key })
        }
        other => Err(RuntimeError::render(format!(
            "handler '{entry}' produced an unknown effect kind '{other}'"
        ))),
    }
}

/// The trust-boundary check on a row key that crossed from the engine: present
/// and scalar. The shim already guards the call site; this refuses anything
/// that reached us anyway rather than lowering a key SQL could not match.
fn forge_scalar_key(
    key: Option<Value>,
    entry: &str,
    collection: &str,
    kind: &str,
) -> Result<Value, RuntimeError> {
    match key {
        Some(key @ (Value::String(_) | Value::Number(_) | Value::Bool(_))) => Ok(key),
        _ => Err(RuntimeError::render(format!(
            "{kind} effect in '{entry}' for '{collection}' is missing a scalar key"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, Value)]) -> Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    /// Decode a pass that is expected to have COMPLETED.
    ///
    /// These tests predate A2 and are about effect lowering, so a suspension
    /// here would mean the envelope shape changed under them — worth a panic
    /// rather than a silently different assertion.
    fn decode_completed(entry: &str, envelope: &str) -> RuntimeResult<HandlerOutcome> {
        match decode_handler_run(entry, envelope)? {
            HandlerRun::Completed(outcome) => Ok(outcome),
            HandlerRun::Suspended { .. } => panic!("unexpected suspension in '{entry}'"),
        }
    }

    #[test]
    fn script_seeds_bindings_setters_and_event() {
        let env = env(&[("count", Value::from(41))]);
        let setters = vec![("setCount".to_string(), SlotId(7))];
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let inv = HandlerInvocation {
            body: "setCount(count + 1)",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };
        let script = build_handler_script(&inv).unwrap();
        assert!(script.contains("let count=41;"));
        assert!(script.contains("const setCount=function(v)"));
        assert!(script.contains("__r.slot_id=7;"));
        assert!(script.contains("(setCount(count + 1));"));
        assert!(script.contains("const event=null;"));
    }

    #[test]
    fn raw_bindings_seed_engine_trusted_expressions() {
        let env = Map::new();
        let raw = vec![("count".to_string(), "1 + 2".to_string())];
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let inv = HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &raw,
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };
        let script = build_handler_script(&inv).unwrap();
        assert!(script.contains("let count=(1 + 2);"));
    }

    #[test]
    fn invalid_binding_name_is_rejected_loudly() {
        let env = env(&[("not-an-ident", Value::Null)]);
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let inv = HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };
        let err = build_handler_script(&inv).unwrap_err();
        assert!(err
            .to_string()
            .contains("not a valid JavaScript identifier"));
    }

    #[test]
    fn script_seeds_broadcast_snapshot_and_updater_handling() {
        let env = Map::new();
        let bc = vec![("count".to_string(), b"5".to_vec())];
        let inv = HandlerInvocation {
            body: "broadcast(\"count\", n => n + 1)",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        };
        let script = build_handler_script(&inv).unwrap();
        // The pre-write snapshot is seeded as a JS object literal.
        assert!(script.contains("const __albedo_topic_current="));
        assert!(script.contains("\"count\":5"));
        // The builtin distinguishes updater functions from plain values.
        assert!(script.contains("typeof value==='function'"));
    }

    #[test]
    fn decode_lowers_slot_and_broadcast_effects_in_order() {
        let effects_json = serde_json::to_string(&serde_json::json!([
            { "kind": "slot", "slot_id": 7, "value": 42 },
            { "kind": "broadcast", "topic": "chat:room", "value": "hi" }
        ]))
        .unwrap();
        let envelope = serde_json::json!({ "ok": true, "value": effects_json }).to_string();

        let outcome = decode_completed("routes/x", &envelope).unwrap();
        // No `result` key (pre-P6 envelope shape) → degrades to `None`.
        assert!(outcome.result.is_none());
        let effects = outcome.effects;
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[0],
            HandlerEffect::SlotSet {
                slot_id: SlotId(7),
                value: b"42".to_vec()
            }
        );
        match &effects[1] {
            HandlerEffect::Broadcast {
                topic,
                slot_id,
                value,
            } => {
                assert_eq!(topic, "chat:room");
                assert_eq!(*slot_id, broadcast_slot_id("chat:room"));
                assert_eq!(value, b"\"hi\"");
            }
            other => panic!("expected broadcast, got {other:?}"),
        }
    }

    /// SANDGATE-B · an effect stamped with a module origin is refused.
    ///
    /// Constructed at the envelope rather than through a package, because gate
    /// 4 established that a package *cannot* reach an effect builtin — so the
    /// only way to exercise the tripwire is to forge the condition it watches
    /// for. That is the right shape for a tripwire test: it proves the alarm
    /// works without needing the fire.
    #[test]
    fn an_effect_recorded_while_a_package_was_executing_is_refused() {
        let effects_json = serde_json::to_string(&serde_json::json!([
            { "kind": "forge_append", "topic": "albedo_users",
              "value": { "role": "admin" }, "origin": "npm:evil@1.0.0/index.js" }
        ]))
        .unwrap();
        let envelope = serde_json::json!({ "ok": true, "value": effects_json }).to_string();

        let err = decode_completed("routes/x", &envelope).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("npm:evil@1.0.0/index.js"),
            "the refusal must name the module it caught. Got: {message}"
        );
        assert!(message.contains("SANDGATE-B"), "Got: {message}");
    }

    /// The ordinary case must not trip it: a handler body runs with the
    /// provenance stack empty, so `origin` is absent and every effect lands.
    #[test]
    fn an_effect_with_no_origin_is_the_ordinary_case_and_is_accepted() {
        let effects_json = serde_json::to_string(&serde_json::json!([
            { "kind": "broadcast", "topic": "t", "value": 1, "origin": null }
        ]))
        .unwrap();
        let envelope = serde_json::json!({ "ok": true, "value": effects_json }).to_string();
        let outcome = decode_completed("routes/x", &envelope).expect("accepted");
        assert_eq!(outcome.effects.len(), 1);
    }

    /// SANDGATE-B · a realm reporting a serialisation hook refuses the pass.
    #[test]
    fn an_integrity_violation_refuses_the_whole_pass() {
        let envelope = serde_json::json!({
            "ok": true, "value": "[]", "integrity": "Object.prototype.toJSON"
        })
        .to_string();
        let err = decode_completed("routes/x", &envelope).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Object.prototype.toJSON"), "Got: {message}");
        assert!(message.contains("SANDGATE-B"), "Got: {message}");
    }

    #[test]
    fn decode_surfaces_a_thrown_error_loudly() {
        let envelope = serde_json::json!({ "ok": false, "error": "boom" }).to_string();
        let err = decode_completed("routes/x", &envelope).unwrap_err();
        assert!(err.to_string().contains("threw: boom"));
    }

    #[test]
    fn effect_lowers_to_slot_set_opcode() {
        let effect = HandlerEffect::Broadcast {
            topic: "t".to_string(),
            slot_id: SlotId(3),
            value: b"1".to_vec(),
        };
        assert_eq!(
            effect.into_instruction(),
            Some(Instruction::SlotSet {
                slot_id: SlotId(3),
                value: b"1".to_vec()
            })
        );
    }

    /// A durable write is not this session's state, so it lowers to no opcode:
    /// the rows a subscriber sees come from rematerialising the collection after
    /// the write commits, not from echoing the record back.
    #[test]
    fn a_forge_append_lowers_to_no_opcode() {
        let effect = HandlerEffect::ForgeAppend {
            collection: "guestbook".to_string(),
            record: env(&[("author", Value::String("ada".to_string()))]),
        };
        assert_eq!(effect.slot_id(), None);
        assert_eq!(effect.into_instruction(), None);
    }

    /// The `append()` builtin and its `form` alias must both be in the script a
    /// handler body runs against, or a body calling them dies at runtime — which
    /// is exactly how both were found.
    #[test]
    fn the_script_defines_append_and_the_form_alias() {
        let env = env(&[]);
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let script = build_handler_script(&HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        })
        .unwrap();
        assert!(script.contains("const append=function(collection,record)"));
        assert!(script.contains("__albedo_rec('forge_append')"));
        assert!(script.contains("const form="));
    }

    /// The mutation trio must all be in the script, and the delete builtin must
    /// be named `remove` — `delete` is a JS reserved word that cannot parse as a
    /// call, so a script defining `delete` would be unreachable from a body.
    #[test]
    fn the_script_defines_the_full_mutation_trio_with_remove_not_delete() {
        let env = env(&[]);
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let script = build_handler_script(&HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
            journal: None,
        })
        .unwrap();
        assert!(script.contains("const update=function(collection,key,fields)"));
        assert!(script.contains("__albedo_rec('forge_update')"));
        assert!(script.contains("const remove=function(collection,key)"));
        assert!(script.contains("__albedo_rec('forge_delete')"));
        assert!(
            !script.contains("const delete="),
            "delete is reserved; must be remove"
        );
    }

    #[test]
    fn a_forge_update_lowers_to_no_opcode() {
        let effect = HandlerEffect::ForgeUpdate {
            collection: "guestbook".to_string(),
            key: Value::from(3),
            fields: env(&[("author", Value::String("grace".to_string()))]),
        };
        assert_eq!(effect.slot_id(), None);
        assert_eq!(effect.into_instruction(), None);
    }

    /// A `forge_update` effect decodes topic→collection, key→row identity,
    /// value→fields; a `forge_delete` carries the key and no value.
    #[test]
    fn forge_update_and_delete_effects_decode_from_the_raw_shape() {
        let effects = serde_json::json!([
            { "kind": "forge_update", "topic": "guestbook", "key": 3, "value": { "author": "grace" } },
            { "kind": "forge_delete", "topic": "guestbook", "key": 7 }
        ])
        .to_string();
        let envelope = serde_json::json!({ "ok": true, "value": effects }).to_string();
        let outcome = decode_completed("routes/x", &envelope).unwrap();

        match &outcome.effects[0] {
            HandlerEffect::ForgeUpdate {
                collection,
                key,
                fields,
            } => {
                assert_eq!(collection, "guestbook");
                assert_eq!(*key, Value::from(3));
                assert_eq!(fields["author"], "grace");
            }
            other => panic!("expected ForgeUpdate, got {other:?}"),
        }
        match &outcome.effects[1] {
            HandlerEffect::ForgeDelete { collection, key } => {
                assert_eq!(collection, "guestbook");
                assert_eq!(*key, Value::from(7));
            }
            other => panic!("expected ForgeDelete, got {other:?}"),
        }
    }

    /// A non-scalar key that somehow reached the decoder is refused — the SQL
    /// builder would refuse it too, but a builtin-named error is clearer.
    #[test]
    fn a_forge_delete_with_a_non_scalar_key_is_refused() {
        let effects =
            serde_json::json!([{ "kind": "forge_delete", "topic": "g", "key": { "a": 1 } }])
                .to_string();
        let envelope = serde_json::json!({ "ok": true, "value": effects }).to_string();
        let err = decode_completed("routes/x", &envelope).unwrap_err();
        assert!(err.to_string().contains("scalar key"));
    }

    /// `form` is only bound for an object payload: a click carries `null` and a
    /// typed input carries a string, and binding either as `form` would let
    /// `form.field` read `undefined` instead of failing at the mistake.
    #[test]
    fn the_form_alias_is_only_bound_for_object_payloads() {
        let env = env(&[]);
        let bc: Vec<(String, Vec<u8>)> = Vec::new();
        let build = |event_json| {
            build_handler_script(&HandlerInvocation {
                body: "0",
                is_block: false,
                env: &env,
                raw_bindings: &[],
                setters: &[],
                event_json,
                broadcast_current: &bc,
                journal: None,
            })
            .unwrap()
        };

        // The alias is a runtime guard on `event`'s shape, so assert the guard
        // is present rather than re-deriving what it evaluates to.
        let form_line = "const form=(event!==null&&typeof event==='object'&&!Array.isArray(event))?event:undefined;";
        assert!(build(None).contains(form_line));
        assert!(build(Some(r#"{"author":"ada"}"#)).contains(form_line));
    }
}
