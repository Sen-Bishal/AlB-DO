pub mod component;
pub mod core;
pub mod expr;

pub use core::{render_from_components_dir, ComponentProject, PatchReport};
pub use expr::{ComponentFunction, ImportBinding, ParamBinding, ParsedModule};

// Phase K re-exports — the corpus and any downstream consumer import
// the hook-compile surface from `runtime::eval` to match Phase J's
// import shape. The real definitions live in `runtime::compiled`.
pub use crate::runtime::compiled::{
    allocate_proxy_id, allocate_slot_id, render_entry_with_bindings, CompiledComponent,
    CompiledProject, RenderOptions, RenderOutput, ResolvedHandler,
};
pub use crate::runtime::slot_store::SessionSlotView;
