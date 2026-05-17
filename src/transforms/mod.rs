//! Phase K — compile-time JSX transforms.
//!
//! The runtime is fully wired: Phase H ships [`SlotStore`], Phase G
//! ships [`ActionHandler`], bakabox already handles `BindEvent`,
//! `SetTextRef`, and `SlotSet`. What Phase K supplies is the
//! compile-time bridge from user-authored `useState` + JSX `on*`
//! handlers to those wire primitives.
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

pub mod events;
pub mod hooks;

pub use events::{extract_handlers_in_function, HandlerExtract};
pub use hooks::{extract_use_state_hooks, HookBinding, HookExtractError};
