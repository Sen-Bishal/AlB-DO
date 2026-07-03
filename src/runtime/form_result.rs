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
//!   return {};                       // success — every error slot is cleared
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
use crate::transforms::form::allocate_field_error_id;
use serde_json::Value;

/// Reconcile an action's return `result` against a form's `declared_fields`,
/// emitting the `SetText` opcodes that fill or clear each field's
/// `data-albedo-error` span.
///
/// Contract on `result`:
/// - `{ "error": { field: message, … } }` → each declared field named here is
///   filled with its message; every other declared field is cleared.
/// - any other shape (a success object, `null`, a non-object) → all declared
///   error spans are cleared.
///
/// Non-string messages and keys the form never declared are ignored: the form's
/// field set is authoritative, so a typo in userland can't smuggle text into an
/// arbitrary slot. One opcode per declared field; empty when the form declares
/// none.
pub fn project_form_result(
    action_name: &str,
    result: &Value,
    declared_fields: &[String],
) -> Vec<Instruction> {
    let errors = result.get("error").and_then(Value::as_object);

    declared_fields
        .iter()
        .map(|field| {
            // A declared field is filled iff `error` carries a string message
            // for it; otherwise it is cleared (empty text). This single pass IS
            // the reconcile — no separate "clear stale" wave needed.
            let text = errors
                .and_then(|map| map.get(field))
                .and_then(Value::as_str)
                .unwrap_or("")
                .as_bytes()
                .to_vec();
            Instruction::SetText {
                stable_id: StableId(allocate_field_error_id(action_name, field)),
                text,
            }
        })
        .collect()
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
    fn fills_named_field_and_clears_the_rest() {
        let result = json!({ "error": { "email": "Enter a real address." } });
        let declared = fields(&["email", "name"]);
        let ops = project_form_result("subscribe", &result, &declared);

        // Exactly one opcode per declared field.
        assert_eq!(ops.len(), 2);
        assert_eq!(
            text_for(&ops, "subscribe", "email"),
            Some(b"Enter a real address.".as_slice())
        );
        // The untouched field is cleared, not left stale.
        assert_eq!(text_for(&ops, "subscribe", "name"), Some(b"".as_slice()));
    }

    #[test]
    fn success_clears_every_declared_field() {
        let ops = project_form_result("subscribe", &json!({ "ok": true }), &fields(&["email", "name"]));
        assert_eq!(ops.len(), 2);
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
        assert_eq!(ops.len(), 1);
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
