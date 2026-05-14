//! Phase-G — server-side action handler surface.
//!
//! An [`ActionHandler`] is the inverse of bakabox's `BindEvent`: when
//! a DOM event fires client-side, bakabox POSTs the action envelope
//! to `/_albedo/action`; the dispatcher decodes it, looks up the
//! handler keyed by `action_id`, and runs it. The handler returns
//! `Vec<Instruction>` — the patches the server wants applied in
//! response — which the dispatcher wire-encodes back to the client.
//!
//! No `Instruction` enum change: action invocations reuse `BindEvent`'s
//! `proxy_id` as the `action_id`. The wire stays at
//! `LOCKED_WIRE_VERSION = 1`.

use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use async_trait::async_trait;
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;

/// User-implemented server-side action.
///
/// Receives the in-flight [`RequestContext`] (cookies, session-bearing
/// headers, query string from the originating page if the bakabox
/// dispatcher chose to forward it) and the [`ActionEnvelope`] decoded
/// from the POST body. Returns the opcode patches the server wants
/// applied to the bakabox DOM; the dispatcher encodes them into an
/// [`OpcodeFrame`](dom_render_compiler::ir::opcode::OpcodeFrame) and
/// returns the bytes.
#[async_trait]
pub trait ActionHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: &RequestContext,
        envelope: &ActionEnvelope,
    ) -> Result<Vec<Instruction>, RuntimeError>;
}

/// Blanket impl so any async closure registers directly. Mirrors the
/// ergonomics of [`crate::contract::RouteHandler`] and
/// [`crate::api::ApiHandler`].
#[async_trait]
impl<F, Fut> ActionHandler for F
where
    F: Send + Sync + Fn(RequestContext, ActionEnvelope) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send,
{
    async fn handle(
        &self,
        ctx: &RequestContext,
        envelope: &ActionEnvelope,
    ) -> Result<Vec<Instruction>, RuntimeError> {
        (self)(ctx.clone(), envelope.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::HttpMethod;
    use bytes::Bytes;
    use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};
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

    #[tokio::test]
    async fn closure_impl_receives_envelope_and_returns_instructions() {
        let handler =
            |_ctx: RequestContext, env: ActionEnvelope| async move {
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
        let out = handler.handle(&ctx(), &envelope).await.unwrap();
        assert!(matches!(
            out[0],
            Instruction::Create { stable_id: StableId(99), .. }
        ));
    }
}
