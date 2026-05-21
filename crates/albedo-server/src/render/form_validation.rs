//! Phase L · form-validation patches.
//!
//! Emits `Instruction::SetText` opcodes that populate the
//! `<span data-albedo-error="FIELD">` slots a form renders for each
//! of its declared fields. The stable id for the span is
//! `fnv1a_32("form-error:{action_name}:{field}")` — matches the id
//! `dom_render_compiler::transforms::form::allocate_field_error_id`
//! produces at compile time, so the client's slot table lines up
//! without any per-render coordination.

use dom_render_compiler::ir::opcode::{Instruction, StableId};
use dom_render_compiler::transforms::form::allocate_field_error_id;
use std::collections::HashMap;

/// Build one `SetText` opcode per `(field, message)` pair, targeting
/// the form's declared `data-albedo-error` span for that field.
///
/// Field order in the returned vec follows `errors.iter()` which is
/// `HashMap` iteration order — non-deterministic by design. The wire
/// contract does not depend on order because each opcode is
/// self-contained; callers that want a stable order should collect
/// from a `BTreeMap` instead.
pub fn validation_error_text_opcodes(
    action_name: &str,
    errors: &HashMap<String, String>,
) -> Vec<Instruction> {
    let mut out = Vec::with_capacity(errors.len());
    for (field, message) in errors {
        let stable_id = StableId(allocate_field_error_id(action_name, field));
        out.push(Instruction::SetText {
            stable_id,
            text: message.clone().into_bytes(),
        });
    }
    out
}

/// Clear every error span for a form by emitting empty-text `SetText`
/// opcodes for each declared field. Useful after a successful submit
/// path that wants to wipe stale messages before a redirect, or as
/// the first wave of opcodes a fresh validation pass emits before
/// writing the new errors.
pub fn clear_validation_error_opcodes(
    action_name: &str,
    field_names: &[String],
) -> Vec<Instruction> {
    field_names
        .iter()
        .map(|field| Instruction::SetText {
            stable_id: StableId(allocate_field_error_id(action_name, field)),
            text: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_one_set_text_per_field() {
        let mut errs = HashMap::new();
        errs.insert("user".to_string(), "required".to_string());
        errs.insert("pass".to_string(), "too short".to_string());
        let ops = validation_error_text_opcodes("submit_login", &errs);
        assert_eq!(ops.len(), 2);
        for op in &ops {
            match op {
                Instruction::SetText { text, .. } => assert!(!text.is_empty()),
                other => panic!("unexpected opcode: {other:?}"),
            }
        }
    }

    #[test]
    fn clear_emits_empty_text_for_every_declared_field() {
        let ops = clear_validation_error_opcodes(
            "submit_login",
            &["user".to_string(), "pass".to_string()],
        );
        assert_eq!(ops.len(), 2);
        for op in &ops {
            match op {
                Instruction::SetText { text, .. } => assert!(text.is_empty()),
                other => panic!("expected SetText, got {other:?}"),
            }
        }
    }

    #[test]
    fn stable_id_matches_compiler_side_allocation() {
        let mut errs = HashMap::new();
        errs.insert("user".to_string(), "x".to_string());
        let ops = validation_error_text_opcodes("submit_login", &errs);
        let expected = StableId(allocate_field_error_id("submit_login", "user"));
        match &ops[0] {
            Instruction::SetText { stable_id, .. } => assert_eq!(*stable_id, expected),
            _ => panic!("not a SetText"),
        }
    }
}
