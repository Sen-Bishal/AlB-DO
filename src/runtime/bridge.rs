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
    /// Pre-write snapshot of broadcast topic values (topic → current JSON
    /// value), used to resolve updater-form `broadcast(topic, fn)` calls inside
    /// JS: the builtin reads the current value here, applies `fn`, and writes
    /// the new value back into the snapshot so a later updater for the same
    /// topic in the same body chains correctly. A topic absent from the map is
    /// treated as `null` (first-call default). Empty for value-only handlers.
    pub broadcast_current: &'a Map<String, Value>,
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
}

#[derive(Debug, Deserialize)]
struct HandlerEnvelope {
    ok: bool,
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
    script.push_str("try{\n");
    script.push_str("const __albedo_effects=[];\n");

    // Pre-write snapshot of topic values, so updater-form `broadcast(topic, fn)`
    // can read the current value. Always defined (at least `{}`) since the
    // builtin references it. A strict-JSON object literal is valid JS.
    script.push_str(&format!(
        "const __albedo_topic_current={};\n",
        js_literal(&Value::Object(inv.broadcast_current.clone()))?
    ));

    // `broadcast(topic, value)` records a broadcast effect. The second argument
    // may be a plain value (value form) or an updater function (React
    // `setState(fn)` form): for a function we read the current topic value from
    // the snapshot (defaulting to `null` for an unseen topic), apply the
    // updater, and write the result back so a subsequent updater for the same
    // topic in this body sees it — matching the pure-Rust read-modify-write.
    // The setter helpers push raw values; the outer JSON.stringify of the whole
    // array encodes them once.
    script.push_str(
        "const broadcast=function(topic,value){var __t=String(topic);var __v;if(typeof value==='function'){var __cur=Object.prototype.hasOwnProperty.call(__albedo_topic_current,__t)?__albedo_topic_current[__t]:null;__v=value(__cur);}else{__v=value;}if(__v===undefined)__v=null;__albedo_topic_current[__t]=__v;__albedo_effects.push({kind:'broadcast',topic:__t,value:__v});};\n",
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
        "const append=function(collection,record){if(record===null||typeof record!=='object'||Array.isArray(record)){throw new TypeError('append(collection, record): record must be an object');}__albedo_effects.push({kind:'forge_append',topic:String(collection),value:record});return null;};\n",
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
        "const update=function(collection,key,fields){if(fields===null||typeof fields!=='object'||Array.isArray(fields)){throw new TypeError('update(collection, key, fields): fields must be an object');}__albedo_effects.push({kind:'forge_update',topic:String(collection),key:__albedo_forge_key('update',key),value:fields});return null;};\n",
    );
    script.push_str(
        "const remove=function(collection,key){__albedo_effects.push({kind:'forge_delete',topic:String(collection),key:__albedo_forge_key('remove',key)});return null;};\n",
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
            "const {name}=function(v){{__albedo_effects.push({{kind:'slot',slot_id:{},value:(v===undefined?null:v)}});}};\n",
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
    script.push_str(
        "return JSON.stringify({ok:true,value:JSON.stringify(__albedo_effects),result:JSON.stringify(__albedo_result===undefined?null:__albedo_result)});\n",
    );
    script.push_str(
        "}catch(err){const message=(err&&typeof err.message==='string')?err.message:String(err);return JSON.stringify({ok:false,error:message});}\n",
    );
    script.push_str("})()");
    Ok(script)
}

/// Decodes the engine's raw envelope string into effects plus the body's return
/// value, mapping a JS throw to a loud [`RuntimeError`]. `entry` is only used
/// for the error message.
pub(crate) fn decode_handler_envelope(
    entry: &str,
    envelope_json: &str,
) -> RuntimeResult<HandlerOutcome> {
    let envelope: HandlerEnvelope = serde_json::from_str(envelope_json).map_err(|err| {
        RuntimeError::render(format!(
            "failed to decode handler effect envelope for '{entry}': {err}"
        ))
    })?;

    if !envelope.ok {
        let message = envelope
            .error
            .unwrap_or_else(|| "unknown handler runtime error".to_string());
        return Err(RuntimeError::render(format!(
            "handler '{entry}' threw: {message}"
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

    Ok(HandlerOutcome { effects, result })
}

fn lower_effect(entry: &str, raw: RawEffect) -> RuntimeResult<HandlerEffect> {
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

    #[test]
    fn script_seeds_bindings_setters_and_event() {
        let env = env(&[("count", Value::from(41))]);
        let setters = vec![("setCount".to_string(), SlotId(7))];
        let bc = Map::new();
        let inv = HandlerInvocation {
            body: "setCount(count + 1)",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &setters,
            event_json: None,
            broadcast_current: &bc,
        };
        let script = build_handler_script(&inv).unwrap();
        assert!(script.contains("let count=41;"));
        assert!(script.contains("const setCount=function(v)"));
        assert!(script.contains("slot_id:7"));
        assert!(script.contains("(setCount(count + 1));"));
        assert!(script.contains("const event=null;"));
    }

    #[test]
    fn raw_bindings_seed_engine_trusted_expressions() {
        let env = Map::new();
        let raw = vec![("count".to_string(), "1 + 2".to_string())];
        let bc = Map::new();
        let inv = HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &raw,
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        };
        let script = build_handler_script(&inv).unwrap();
        assert!(script.contains("let count=(1 + 2);"));
    }

    #[test]
    fn invalid_binding_name_is_rejected_loudly() {
        let env = env(&[("not-an-ident", Value::Null)]);
        let bc = Map::new();
        let inv = HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        };
        let err = build_handler_script(&inv).unwrap_err();
        assert!(err
            .to_string()
            .contains("not a valid JavaScript identifier"));
    }

    #[test]
    fn script_seeds_broadcast_snapshot_and_updater_handling() {
        let env = Map::new();
        let mut bc = Map::new();
        bc.insert("count".to_string(), Value::from(5));
        let inv = HandlerInvocation {
            body: "broadcast(\"count\", n => n + 1)",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
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

        let outcome = decode_handler_envelope("routes/x", &envelope).unwrap();
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

    #[test]
    fn decode_surfaces_a_thrown_error_loudly() {
        let envelope = serde_json::json!({ "ok": false, "error": "boom" }).to_string();
        let err = decode_handler_envelope("routes/x", &envelope).unwrap_err();
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
        let bc = Map::new();
        let script = build_handler_script(&HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        })
        .unwrap();
        assert!(script.contains("const append=function(collection,record)"));
        assert!(script.contains("kind:'forge_append'"));
        assert!(script.contains("const form="));
    }

    /// The mutation trio must all be in the script, and the delete builtin must
    /// be named `remove` — `delete` is a JS reserved word that cannot parse as a
    /// call, so a script defining `delete` would be unreachable from a body.
    #[test]
    fn the_script_defines_the_full_mutation_trio_with_remove_not_delete() {
        let env = env(&[]);
        let bc = Map::new();
        let script = build_handler_script(&HandlerInvocation {
            body: "0",
            is_block: false,
            env: &env,
            raw_bindings: &[],
            setters: &[],
            event_json: None,
            broadcast_current: &bc,
        })
        .unwrap();
        assert!(script.contains("const update=function(collection,key,fields)"));
        assert!(script.contains("kind:'forge_update'"));
        assert!(script.contains("const remove=function(collection,key)"));
        assert!(script.contains("kind:'forge_delete'"));
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
        let outcome = decode_handler_envelope("routes/x", &envelope).unwrap();

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
        let err = decode_handler_envelope("routes/x", &envelope).unwrap_err();
        assert!(err.to_string().contains("scalar key"));
    }

    /// `form` is only bound for an object payload: a click carries `null` and a
    /// typed input carries a string, and binding either as `form` would let
    /// `form.field` read `undefined` instead of failing at the mistake.
    #[test]
    fn the_form_alias_is_only_bound_for_object_payloads() {
        let env = env(&[]);
        let bc = Map::new();
        let build = |event_json| {
            build_handler_script(&HandlerInvocation {
                body: "0",
                is_block: false,
                env: &env,
                raw_bindings: &[],
                setters: &[],
                event_json,
                broadcast_current: &bc,
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
