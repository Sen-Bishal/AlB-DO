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
}

impl HandlerEffect {
    /// Lowers this effect to the opcode the action dispatcher ships. Both
    /// variants become a `SlotSet` carrying the JSON value bytes.
    #[must_use]
    pub fn into_instruction(self) -> Instruction {
        match self {
            HandlerEffect::SlotSet { slot_id, value } => Instruction::SlotSet { slot_id, value },
            HandlerEffect::Broadcast { slot_id, value, .. } => {
                Instruction::SlotSet { slot_id, value }
            }
        }
    }

    /// The slot id this effect writes, regardless of variant.
    #[must_use]
    pub fn slot_id(&self) -> SlotId {
        match self {
            HandlerEffect::SlotSet { slot_id, .. }
            | HandlerEffect::Broadcast { slot_id, .. } => *slot_id,
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
    value: Value,
}

#[derive(Debug, Deserialize)]
struct HandlerEnvelope {
    ok: bool,
    /// On success: a JSON-encoded `Vec<RawEffect>` (double-encoded so the outer
    /// render envelope stays a flat `{ok, value, error}` shape).
    value: Option<String>,
    error: Option<String>,
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
        RuntimeError::render(format!("failed to encode handler binding as JS literal: {err}"))
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

    // Run the body. A block runs as-is; an expression is evaluated for its
    // effects (its value is discarded, matching the pure-Rust handler path).
    if inv.is_block {
        script.push_str(inv.body);
        script.push('\n');
    } else {
        script.push_str(&format!("({});\n", inv.body));
    }

    script.push_str(
        "return JSON.stringify({ok:true,value:JSON.stringify(__albedo_effects)});\n",
    );
    script.push_str(
        "}catch(err){const message=(err&&typeof err.message==='string')?err.message:String(err);return JSON.stringify({ok:false,error:message});}\n",
    );
    script.push_str("})()");
    Ok(script)
}

/// Decodes the engine's raw envelope string into effects, mapping a JS throw to
/// a loud [`RuntimeError`]. `entry` is only used for the error message.
pub(crate) fn decode_handler_envelope(
    entry: &str,
    envelope_json: &str,
) -> RuntimeResult<Vec<HandlerEffect>> {
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

    let effects_json = envelope.value.ok_or_else(|| {
        RuntimeError::render(format!("handler '{entry}' returned success without effects"))
    })?;
    let raw: Vec<RawEffect> = serde_json::from_str(&effects_json).map_err(|err| {
        RuntimeError::render(format!(
            "failed to decode handler effect list for '{entry}': {err}"
        ))
    })?;

    raw.into_iter()
        .map(|effect| lower_effect(entry, effect))
        .collect()
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
        other => Err(RuntimeError::render(format!(
            "handler '{entry}' produced an unknown effect kind '{other}'"
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
        assert!(err.to_string().contains("not a valid JavaScript identifier"));
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
        let envelope =
            serde_json::json!({ "ok": true, "value": effects_json }).to_string();

        let effects = decode_handler_envelope("routes/x", &envelope).unwrap();
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[0],
            HandlerEffect::SlotSet {
                slot_id: SlotId(7),
                value: b"42".to_vec()
            }
        );
        match &effects[1] {
            HandlerEffect::Broadcast { topic, slot_id, value } => {
                assert_eq!(topic, "chat:room");
                assert_eq!(*slot_id, broadcast_slot_id("chat:room"));
                assert_eq!(value, b"\"hi\"");
            }
            other => panic!("expected broadcast, got {other:?}"),
        }
    }

    #[test]
    fn decode_surfaces_a_thrown_error_loudly() {
        let envelope =
            serde_json::json!({ "ok": false, "error": "boom" }).to_string();
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
            Instruction::SlotSet {
                slot_id: SlotId(3),
                value: b"1".to_vec()
            }
        );
    }
}
