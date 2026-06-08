pub mod affinity;
pub mod arena;
pub mod bridge;
pub mod broadcast;
pub mod compiled;
pub mod dirty_bitmap;
pub mod emitter;
pub mod engine;
pub mod eval;
pub mod frame;
pub mod highway;
pub mod hot_set;
pub mod pi_arch;
pub mod pipeline;
pub mod quickjs_engine;
pub mod render_observer;
pub mod renderer;
pub mod scheduler;
pub mod session;
pub mod slot_store;
pub mod static_slice;
pub mod webtransport;

pub use bridge::{HandlerEffect, HandlerInvocation};
pub use broadcast::{
    broadcast_slot_id, BroadcastDelivery, BroadcastError, BroadcastRegistry, BroadcastSender,
    BroadcastTopic,
};
pub use compiled::{
    allocate_proxy_id, allocate_slot_id, render_entry_with_bindings, render_entry_with_broadcast,
    CompiledComponent, CompiledProject, RenderOptions, RenderOutput, ResolvedHandler,
};
pub use eval::{render_from_components_dir, ComponentProject, PatchReport};
pub use session::SessionId;
pub use slot_store::{SessionSlotView, SlotStore};
