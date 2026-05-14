//! Phase-G — HTTP handler that decodes an [`ActionEnvelope`], dispatches
//! to the registered [`ActionHandler`], and ships the resulting opcode
//! patches back as a binary [`OpcodeFrame`].
//!
//! Wire: `POST /_albedo/action`. Body = bincode `ActionEnvelope`.
//! Response = bincode `OpcodeFrame` (the same shape bakabox already
//! decodes for the WT patches stream).

use crate::actions::ActionHandler;
use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use axum::body::Body;
use axum::http::{header, Response, StatusCode};
use bytes::Bytes;
use dom_render_compiler::ir::action::decode_action_envelope;
use dom_render_compiler::ir::opcode::OpcodeFrame;
use dom_render_compiler::ir::wire::encode_frame;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

/// Lookup table from `action_id` → handler. The dispatcher reads from
/// this; the server builder fills it via
/// [`crate::AlbedoServerBuilder::register_action`].
pub type ActionRegistry = HashMap<u32, Arc<dyn ActionHandler>>;

/// Runs the action HTTP path. Decodes the envelope, looks up the
/// handler by `action_id`, invokes it, and wire-encodes the returned
/// instructions as an [`OpcodeFrame`] (no `component_id`).
///
/// Error mapping:
/// - Malformed body → 400 with a short text reason
/// - Unknown `action_id` → 404
/// - Handler error → 500 with the underlying message
///
/// The success response is `200` with
/// `content-type: application/octet-stream`; bakabox's client
/// dispatcher feeds the bytes straight into `applyFrameBytes`.
pub async fn run_action_request(
    registry: &ActionRegistry,
    ctx: RequestContext,
    body: Bytes,
) -> Response<Body> {
    let (envelope, _consumed) = match decode_action_envelope(body.as_ref()) {
        Ok(value) => value,
        Err(err) => {
            warn!(error = %err, "rejecting malformed action envelope");
            return error_response(
                StatusCode::BAD_REQUEST,
                format!("invalid action envelope: {err}"),
            );
        }
    };

    let handler = match registry.get(&envelope.action_id) {
        Some(handler) => handler.clone(),
        None => {
            return error_response(
                StatusCode::NOT_FOUND,
                format!("no handler registered for action_id {}", envelope.action_id),
            );
        }
    };

    let instructions = match handler.handle(&ctx, &envelope).await {
        Ok(out) => out,
        Err(err) => {
            warn!(action_id = envelope.action_id, error = %err, "action handler failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                err.to_string(),
            );
        }
    };

    // Action responses don't share frame_ids with the WT patches
    // stream — each is a self-contained `OpcodeFrame` per the Phase-D
    // wire amendment. The `frame_id` here is `0`: bakabox's
    // `applyFrameBytes` doesn't currently key on frame_id for HTTP
    // responses (no multi-message reassembly), so the value is
    // bookkeeping only.
    let frame = OpcodeFrame {
        frame_id: 0,
        component_id: None,
        instructions,
    };
    let bytes = match encode_frame(&frame) {
        Ok(bytes) => bytes,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to encode action response: {err}"),
            );
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("cache-control", "no-store")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Centralises the error response shape so every failure mode in
/// `run_action_request` looks the same on the wire. Plain-text body
/// keeps debugging cheap; the client surfaces non-200 by logging the
/// status + body and dropping the result.
fn error_response(status: StatusCode, message: String) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Body::from(message))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// Convenience wrapper so the [`crate::error::RuntimeError`] surface
/// stays the canonical failure type for action handlers. Currently
/// unused outside `RuntimeError::Authentication` mapping — the
/// dispatcher converts every error variant uniformly to 500 in this
/// Phase-G MVP.
#[allow(dead_code)]
pub(crate) fn status_for_error(err: &RuntimeError) -> StatusCode {
    match err {
        RuntimeError::Authentication(_) => StatusCode::UNAUTHORIZED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionHandler;
    use axum::body::to_bytes;
    use dom_render_compiler::ir::action::{encode_action_envelope, ActionEnvelope};
    use dom_render_compiler::ir::opcode::{Instruction, StableId, TagId};
    use dom_render_compiler::ir::wire::decode_frame;

    fn ctx() -> RequestContext {
        RequestContext {
            request_id: "t".into(),
            method: crate::routing::HttpMethod::Post,
            path: "/_albedo/action".into(),
            query: Default::default(),
            params: Default::default(),
            headers: Default::default(),
            body: Bytes::new(),
            metadata: Default::default(),
        }
    }

    async fn body_bytes(resp: Response<Body>) -> Bytes {
        to_bytes(resp.into_body(), 1024 * 1024).await.unwrap()
    }

    #[tokio::test]
    async fn dispatches_to_registered_handler_and_returns_wire_encoded_frame() {
        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, env: ActionEnvelope| async move {
                Ok(vec![Instruction::Create {
                    tag_id: TagId(0),
                    stable_id: StableId(env.action_id),
                }])
            },
        );
        registry.insert(42, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 42,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );

        let response = run_action_request(&registry, ctx(), body).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/octet-stream"),
        );

        let bytes = body_bytes(response).await;
        let (frame, consumed) = decode_frame(&bytes).expect("frame decodes");
        assert_eq!(consumed, bytes.len());
        assert!(matches!(
            frame.instructions[0],
            Instruction::Create { stable_id: StableId(42), .. }
        ));
    }

    #[tokio::test]
    async fn malformed_body_returns_400() {
        let registry: ActionRegistry = HashMap::new();
        let response = run_action_request(
            &registry,
            ctx(),
            Bytes::from_static(&[0xff, 0xff, 0xff, 0xff]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_action_id_returns_404() {
        let registry: ActionRegistry = HashMap::new();
        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 7,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );
        let response = run_action_request(&registry, ctx(), body).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handler_error_returns_500() {
        let mut registry: ActionRegistry = HashMap::new();
        let handler: Arc<dyn ActionHandler> = Arc::new(
            |_ctx: RequestContext, _env: ActionEnvelope| async move {
                Err(RuntimeError::RequestHandling("boom".into()))
            },
        );
        registry.insert(1, handler);

        let body = Bytes::from(
            encode_action_envelope(&ActionEnvelope {
                action_id: 1,
                event_kind: 0,
                payload: Vec::new(),
            })
            .unwrap(),
        );
        let response = run_action_request(&registry, ctx(), body).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
