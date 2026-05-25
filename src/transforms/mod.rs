//! Phase K + Phase L — compile-time JSX transforms.
//!
//! The runtime is fully wired: Phase H ships [`SlotStore`], Phase G
//! ships [`ActionHandler`], bakabox already handles `BindEvent`,
//! `SetTextRef`, and `SlotSet`. What Phase K + L supply is the
//! compile-time bridge from user-authored `useState` + JSX `on*`
//! handlers + `<form action="action:NAME">` + `<Link href>` to those
//! wire primitives.
//!
//! Architectural choice: Phase J's evaluator is an AST walker. Rather
//! than rewriting the AST source-to-source (a full SWC `VisitMut`
//! pass), this module **extracts metadata** from the parsed AST that
//! the renderer interprets at render time to emit the same opcodes a
//! source-to-source compiler would have. The observable wire contract
//! is identical; the implementation reuses the existing evaluator.
//!
//! See `src/runtime/compiled.rs` for the wrapper that combines the
//! extractor results with the render/dispatch entry points.
//!
//! [`SlotStore`]: crate::runtime::slot_store::SlotStore
//! [`ActionHandler`]: ../../../crates/albedo-server/src/actions.rs

pub mod css_modules;
pub mod events;
pub mod form;
pub mod hooks;
pub mod link;
pub mod shared_slots;

pub use css_modules::{is_css_module_path, scope_module_css, ScopedCssModule};
pub use events::{
    collect_free_idents_in_handler_body, extract_handlers_in_function, HandlerBody, HandlerExtract,
};
pub use form::{
    allocate_field_error_id, allocate_form_action_id, extract_forms_in_function, FormExtract,
    FormField, FormFieldKind, FormMethod, FORM_ACTION_PREFIX,
};
pub use hooks::{extract_use_state_hooks, HookBinding, HookExtractError};
pub use link::{extract_links_in_function, LinkExtract};
pub use shared_slots::{
    extract_shared_slot_hooks, SharedSlotBinding, SharedSlotExtractError,
};
