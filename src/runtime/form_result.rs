//! P6 · project an action's return value onto a form's error slots.
//!
//! A server `action()` runs in userland JS and (for a form) returns a plain
//! result object. ALBEDO does not prescribe a validation library — zod, valibot,
//! a hand-rolled check all work — it prescribes only the *shape* of the return
//! and then performs a **compile-time-anchored projection** of that shape onto
//! the DOM slots the form already reserved at build time:
//!
//! ```ts
//! export const subscribe = action(({ event }) => {
//!   const parsed = Schema.safeParse(event);
//!   if (!parsed.success) return { error: { email: "Enter a real address." } };
//!   return { ok: "You're subscribed." }; // fills the success sink, clears every error
//! });
//! ```
//!
//! The projection is a **reconciliation against the form's declared field set**
//! (surfaced at compile time as `FormExtract.fields`): every declared field is
//! visited exactly once, so a field named in `error` is filled and a field
//! *absent* from it is cleared. A re-submit therefore never leaves a stale
//! message behind, and the runtime never guesses which spans exist — the form's
//! field manifest is the schema. Only `SetText` opcodes targeting the
//! compile-time `fnv1a_32("form-error:{action}:{field}")` ids
//! ([`crate::transforms::form::allocate_field_error_id`]) are emitted, so the
//! client's existing slot table lines up with zero per-render coordination.
//!
//! This runs at the one point action semantics become wire opcodes
//! ([`crate::runtime::compiled::CompiledProject::invoke_action_quickjs`]),
//! alongside the `setState` / `broadcast` effect lowering — the value channel is
//! untouched; projection only *appends* the field-error opcodes.

use crate::ir::opcode::{Instruction, StableId};
use crate::transforms::form::{allocate_field_error_id, allocate_form_ok_id};
use serde_json::Value;

/// Reconcile an action's return `result` against a form's `declared_fields`,
/// emitting the `SetText` opcodes that fill or clear each field's
/// `data-albedo-error` span.
///
/// Contract on `result`:
/// - `{ "error": { field: message, … } }` → each declared field named here is
///   filled with its message; every other declared field is cleared.
/// - `{ "ok": message }` → the form's success sink is filled — **only if no
///   declared field failed** in the same result; a result is one or the other.
/// - any other shape (`{}`, `{ ok: true }`, `null`, a non-object) → every error
///   sink and the success sink are cleared.
///
/// So the form's whole feedback state is a function of its *last* result, and
/// one pass reconciles all of it: a failing re-submit clears a stale "You're on
/// the list", a succeeding one clears a stale error. No second wave.
///
/// Non-string messages and keys the form never declared are ignored: the form's
/// field set is authoritative, so a typo in userland can't smuggle text into an
/// arbitrary slot. One opcode per declared field, then the success sink's.
///
/// A form that declares **no** fields gets the success opcode only when there
/// is a message to write. That keeps the pre-`ok` behaviour — no opcodes at all
/// — for every field-less form whose action doesn't use `ok`, which matters
/// because a form rendered client-side has no reserved sinks, and bakabox drops
/// a whole frame on an opcode whose node is missing. A field-less form has no
/// failure state to clear the success sink *from*, so nothing is lost.
pub fn project_form_result(
    action_name: &str,
    result: &Value,
    declared_fields: &[String],
) -> Vec<Instruction> {
    let errors = result.get("error").and_then(Value::as_object);
    let mut failed = false;

    let mut ops: Vec<Instruction> = declared_fields
        .iter()
        .map(|field| {
            // A declared field is filled iff `error` carries a string message
            // for it; otherwise it is cleared (empty text). This single pass IS
            // the reconcile — no separate "clear stale" wave needed.
            let text = errors
                .and_then(|map| map.get(field))
                .and_then(Value::as_str)
                .unwrap_or("");
            failed |= !text.is_empty();
            Instruction::SetText {
                stable_id: StableId(allocate_field_error_id(action_name, field)),
                text: text.as_bytes().to_vec(),
            }
        })
        .collect();

    let ok = if failed { "" } else { result.get("ok").and_then(Value::as_str).unwrap_or("") };
    if !declared_fields.is_empty() || !ok.is_empty() {
        ops.push(Instruction::SetText {
            stable_id: StableId(allocate_form_ok_id(action_name)),
            text: ok.as_bytes().to_vec(),
        });
    }

    ops
}

/// `{ redirect: "/account" }` — or an allowlisted `https://…` — as the
/// `Navigate` that sends the browser on.
///
/// Nothing when the same result reports an error: a failing result keeps the
/// person on the form that shows why. Any non-empty `error` object counts, so a
/// validation failure cannot be skipped past by also returning a redirect.
///
/// Not validated HERE: the allowlist is the app's config, which the server
/// holds, so the server checks every `Navigate` an action emits before it is
/// sent — both transports, one place. See `redirects` in the dev contract.
#[must_use]
pub fn redirect_of(result: &Value) -> Option<Instruction> {
    let failed = result
        .get("error")
        .and_then(Value::as_object)
        .is_some_and(|errors| !errors.is_empty());
    if failed {
        return None;
    }
    result
        .get("redirect")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(|url| Instruction::Navigate {
            url: url.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fields(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn text_for<'a>(ops: &'a [Instruction], action: &str, field: &str) -> Option<&'a [u8]> {
        let want = StableId(allocate_field_error_id(action, field));
        ops.iter().find_map(|op| match op {
            Instruction::SetText { stable_id, text } if *stable_id == want => {
                Some(text.as_slice())
            }
            _ => None,
        })
    }

    #[test]
    fn a_redirect_result_navigates_and_an_error_result_never_does() {
        assert!(matches!(
            redirect_of(&json!({ "redirect": "/account/welcome" })),
            Some(Instruction::Navigate { url }) if url == "/account/welcome"
        ));
        assert_eq!(
            redirect_of(&json!({ "error": { "email": "No." }, "redirect": "/elsewhere" })),
            None,
            "a failing result keeps the person on the form"
        );
        assert!(redirect_of(&json!({ "error": {}, "redirect": "/ok" })).is_some());
        assert_eq!(redirect_of(&json!({ "ok": "done" })), None);
        assert_eq!(redirect_of(&json!({ "redirect": "" })), None);
        assert_eq!(redirect_of(&Value::Null), None);
    }

    #[test]
    fn fills_named_field_and_clears_the_rest() {
        let result = json!({ "error": { "email": "Enter a real address." } });
        let declared = fields(&["email", "name"]);
        let ops = project_form_result("subscribe", &result, &declared);

        // One opcode per declared field, plus the success sink's.
        assert_eq!(ops.len(), 3);
        assert_eq!(
            text_for(&ops, "subscribe", "email"),
            Some(b"Enter a real address.".as_slice())
        );
        // The untouched field is cleared, not left stale.
        assert_eq!(text_for(&ops, "subscribe", "name"), Some(b"".as_slice()));
    }

    fn ok_text<'a>(ops: &'a [Instruction], action: &str) -> Option<&'a [u8]> {
        let want = StableId(allocate_form_ok_id(action));
        ops.iter().find_map(|op| match op {
            Instruction::SetText { stable_id, text } if *stable_id == want => Some(text.as_slice()),
            _ => None,
        })
    }

    #[test]
    fn an_ok_message_fills_the_success_sink_and_clears_every_error() {
        let result = json!({ "ok": "You're on the list." });
        let ops = project_form_result("join", &result, &fields(&["email"]));
        assert_eq!(ok_text(&ops, "join"), Some(b"You're on the list.".as_slice()));
        assert_eq!(text_for(&ops, "join", "email"), Some(b"".as_slice()));
    }

    #[test]
    fn a_failing_result_clears_the_success_sink_even_if_it_says_ok() {
        // One or the other: a result that names a failing field is a failure,
        // and must also wipe the success message a previous submit left.
        let result = json!({ "ok": "nope", "error": { "email": "bad" } });
        let ops = project_form_result("join", &result, &fields(&["email"]));
        assert_eq!(ok_text(&ops, "join"), Some(b"".as_slice()));
        assert_eq!(text_for(&ops, "join", "email"), Some(b"bad".as_slice()));
    }

    #[test]
    fn a_fieldless_form_gets_the_success_opcode_only_when_there_is_a_message() {
        // No sinks may exist for it (a client-rendered form), and a missing node
        // costs the whole frame — so no opcode unless `ok` has something to say.
        assert!(project_form_result("resend", &json!({}), &[]).is_empty());
        let ops = project_form_result("resend", &json!({ "ok": "Sent." }), &[]);
        assert_eq!(ops.len(), 1);
        assert_eq!(ok_text(&ops, "resend"), Some(b"Sent.".as_slice()));
    }

    #[test]
    fn success_clears_every_declared_field() {
        // `ok: true` is a success with nothing to say: every sink is cleared.
        let ops = project_form_result("subscribe", &json!({ "ok": true }), &fields(&["email", "name"]));
        assert_eq!(ops.len(), 3);
        assert_eq!(ok_text(&ops, "subscribe"), Some(b"".as_slice()));
        for field in ["email", "name"] {
            assert_eq!(
                text_for(&ops, "subscribe", field),
                Some(b"".as_slice()),
                "field {field} should be cleared on success"
            );
        }
    }

    #[test]
    fn null_result_clears_every_declared_field() {
        let ops = project_form_result("subscribe", &Value::Null, &fields(&["email"]));
        assert_eq!(text_for(&ops, "subscribe", "email"), Some(b"".as_slice()));
    }

    #[test]
    fn undeclared_error_keys_are_ignored() {
        // A body returning an error for a field the form never declared must not
        // fabricate an opcode — the declared set is authoritative.
        let result = json!({ "error": { "ghost": "nope", "email": "bad" } });
        let ops = project_form_result("subscribe", &result, &fields(&["email"]));
        assert_eq!(ops.len(), 2);
        assert_eq!(text_for(&ops, "subscribe", "email"), Some(b"bad".as_slice()));
        assert!(text_for(&ops, "subscribe", "ghost").is_none());
    }

    #[test]
    fn non_string_message_is_treated_as_no_error() {
        // 42 isn't a message → email has no error → cleared.
        let ops = project_form_result("subscribe", &json!({ "error": { "email": 42 } }), &fields(&["email"]));
        assert_eq!(text_for(&ops, "subscribe", "email"), Some(b"".as_slice()));
    }

    #[test]
    fn no_declared_fields_is_empty() {
        let result = json!({ "error": { "email": "bad" } });
        assert!(project_form_result("subscribe", &result, &[]).is_empty());
    }
}
