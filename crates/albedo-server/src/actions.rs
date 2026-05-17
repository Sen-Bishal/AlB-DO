//! Phase-G/H — server-side action handler surface.
//!
//! An [`ActionHandler`] is the inverse of bakabox's `BindEvent`: when
//! a DOM event fires client-side, bakabox POSTs the action envelope
//! to `/_albedo/action`; the dispatcher decodes it, looks up the
//! handler keyed by `action_id`, and runs it. The handler returns
//! `Vec<Instruction>` — the patches the server wants applied in
//! response — which the dispatcher wire-encodes back to the client.
//!
//! Phase-H adds [`SessionSlots`]: a session-scoped view onto the
//! shared [`SlotStore`] that handlers use to read and mutate the
//! reactive state behind compiled `useState`/`useEffect` calls. The
//! dispatcher appends any drained `SlotSet` opcodes to the handler's
//! response so the reactive loop closes in one round-trip.
//!
//! No `Instruction` enum change: action invocations reuse `BindEvent`'s
//! `proxy_id` as the `action_id`. The wire stays at
//! `LOCKED_WIRE_VERSION = 1`.

use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use async_trait::async_trait;
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::{Instruction, SlotId};
use dom_render_compiler::runtime::{SessionId, SlotStore};
use std::sync::Arc;

/// Session-scoped slot view passed to every [`ActionHandler::handle`]
/// call. Wraps the shared [`SlotStore`] so handlers can read and write
/// without needing to know the session id explicitly each time.
///
/// `Clone` is cheap (just clones two `Arc`s); pass the view by value
/// into spawned tasks or closures.
#[derive(Clone)]
pub struct SessionSlots {
    session_id: SessionId,
    store: Arc<SlotStore>,
}

impl SessionSlots {
    /// Constructs a view bound to `session_id` against `store`.
    #[must_use]
    pub fn new(session_id: SessionId, store: Arc<SlotStore>) -> Self {
        Self { session_id, store }
    }

    /// Returns the session this view is scoped to.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns a clone of the underlying shared store. Phase K's
    /// `CompiledProjectActionAdapter` uses this to build a
    /// `SessionSlotView` (the dom-render-compiler-side equivalent of
    /// `SessionSlots`) that the compiled handler executes against.
    #[must_use]
    pub fn store(&self) -> &Arc<SlotStore> {
        &self.store
    }

    /// Reads the current value of `slot_id` for this session. Returns
    /// `None` when the slot has never been written.
    #[must_use]
    pub fn read(&self, slot_id: SlotId) -> Option<Vec<u8>> {
        self.store.read(self.session_id, slot_id)
    }

    /// Writes a new value to `slot_id`. The dispatcher drains the
    /// dirty set after the handler returns and ships a `SlotSet`
    /// opcode for every written slot.
    pub fn write(&self, slot_id: SlotId, value: Vec<u8>) {
        self.store.write(self.session_id, slot_id, value);
    }

    /// Drains the dirty set for this session into `SlotSet` opcodes.
    /// The dispatcher calls this after [`ActionHandler::handle`]
    /// returns and appends the result to the handler's response; user
    /// code rarely needs to call it directly.
    pub fn drain_pending(&self) -> Vec<Instruction> {
        self.store.drain_set_instructions(self.session_id)
    }
}

/// User-implemented server-side action.
///
/// Receives the in-flight [`RequestContext`] (cookies, session-bearing
/// headers, query string from the originating page if the bakabox
/// dispatcher chose to forward it), the [`ActionEnvelope`] decoded
/// from the POST body, and a [`SessionSlots`] view for reading and
/// mutating reactive state. Returns the explicit opcode patches the
/// server wants applied; the dispatcher merges in any `SlotSet`
/// emissions driven by `slots.write` before wire-encoding the response.
#[async_trait]
pub trait ActionHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: &RequestContext,
        envelope: &ActionEnvelope,
        slots: SessionSlots,
    ) -> Result<Vec<Instruction>, RuntimeError>;
}

/// Blanket impl so any async closure registers directly. Mirrors the
/// ergonomics of [`crate::contract::RouteHandler`] and
/// [`crate::api::ApiHandler`].
#[async_trait]
impl<F, Fut> ActionHandler for F
where
    F: Send + Sync + Fn(RequestContext, ActionEnvelope, SessionSlots) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send,
{
    async fn handle(
        &self,
        ctx: &RequestContext,
        envelope: &ActionEnvelope,
        slots: SessionSlots,
    ) -> Result<Vec<Instruction>, RuntimeError> {
        (self)(ctx.clone(), envelope.clone(), slots).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::HttpMethod;
    use bytes::Bytes;
    use dom_render_compiler::ir::opcode::{Instruction, SlotId, StableId, TagId};
    use std::collections::BTreeMap;

    fn ctx() -> RequestContext {
        RequestContext {
            request_id: "t".into(),
            method: HttpMethod::Post,
            path: "/_albedo/action".into(),
            query: BTreeMap::new(),
            params: BTreeMap::new(),
            headers: BTreeMap::new(),
            body: Bytes::new(),
            metadata: BTreeMap::new(),
        }
    }

    fn slots() -> SessionSlots {
        SessionSlots::new(SessionId::random(), Arc::new(SlotStore::new()))
    }

    #[tokio::test]
    async fn closure_impl_receives_envelope_and_returns_instructions() {
        let handler =
            |_ctx: RequestContext, env: ActionEnvelope, _slots: SessionSlots| async move {
                assert_eq!(env.action_id, 99);
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(env.action_id),
                }])
            };
        let envelope = ActionEnvelope {
            action_id: 99,
            event_kind: 0,
            payload: Vec::new(),
        };
        let out = handler.handle(&ctx(), &envelope, slots()).await.unwrap();
        assert!(matches!(
            out[0],
            Instruction::Create { stable_id: StableId(99), .. }
        ));
    }

    #[tokio::test]
    async fn closure_can_mutate_slots_and_dispatcher_drains_them() {
        let view = slots();
        let handler =
            |_ctx: RequestContext, _env: ActionEnvelope, slots: SessionSlots| async move {
                slots.write(SlotId(1), b"updated".to_vec());
                Ok(Vec::new())
            };
        let envelope = ActionEnvelope {
            action_id: 1,
            event_kind: 0,
            payload: Vec::new(),
        };
        let out = handler.handle(&ctx(), &envelope, view.clone()).await.unwrap();
        assert!(out.is_empty(), "handler returned no explicit opcodes");

        let drained = view.drain_pending();
        assert_eq!(drained.len(), 1, "the slot.write must surface as one SlotSet");
        assert!(matches!(
            &drained[0],
            Instruction::SlotSet { slot_id: SlotId(1), value } if value == b"updated"
        ));
    }
}
