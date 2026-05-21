//! Phase L · typed `register_form_action::<T>(...)`.
//!
//! Registration shorthand for a server-side form handler. Wraps the
//! Phase-G `ActionHandler` trait so userland sees a typed struct
//! `T: FromFormPayload` instead of raw `ActionEnvelope.payload:
//! Vec<u8>`.
//!
//! Wire shape: the client serializes `FormData` into a JSON object
//! `{ "field1": "value1", "field2": "value2", ... }` and writes those
//! bytes into `ActionEnvelope.payload`. The server-side wrapper
//! deserializes those bytes into `T` via [`FromFormPayload`]; the
//! blanket impl for `T: serde::de::DeserializeOwned` covers the
//! common case.
//!
//! Validation failure path: when `from_form_payload` returns
//! `FormDecodeError::Validation { errors }`, the wrapper emits
//! `SetText` opcodes targeting `<span data-albedo-error="FIELD">`
//! elements via [`super::form_validation::validation_error_text_opcodes`]
//! before returning early — no application handler runs.

use crate::actions::{ActionHandler, SessionSlots};
use crate::error::RuntimeError;
use crate::lifecycle::RequestContext;
use crate::render::form_validation::validation_error_text_opcodes;
use async_trait::async_trait;
use dom_render_compiler::ir::action::ActionEnvelope;
use dom_render_compiler::ir::opcode::Instruction;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::Arc;

/// Decode-time failure modes a typed form action can hit. The wrapper
/// converts each into a recoverable response — a 400 for the malformed
/// case, a patch-only response (no Navigate, no app handler) for the
/// validation case.
#[derive(Debug, thiserror::Error)]
pub enum FormDecodeError {
    /// JSON parse failed or the payload didn't match the target
    /// shape. Surfaced to the dispatcher as a
    /// `RuntimeError::RequestHandling`, which the dispatcher renders
    /// as a 400 Bad Request.
    #[error("malformed form payload: {0}")]
    Malformed(String),
    /// Application-level validation failed. The `errors` map carries
    /// per-field messages keyed by form field name; the wrapper emits
    /// one `SetText` opcode per entry targeting the corresponding
    /// `data-albedo-error` span.
    #[error("form validation failed for {} field(s)", .errors.len())]
    Validation { errors: HashMap<String, String> },
}

/// Trait implemented by every struct a typed form action can decode
/// into.
///
/// The default blanket impl decodes the payload bytes as JSON via
/// `serde_json::from_slice`. Custom impls can layer in
/// application-level validation that returns
/// `FormDecodeError::Validation { errors }` for per-field messages.
pub trait FromFormPayload: Sized + Send {
    /// Attempt to construct `Self` from the raw payload bytes
    /// supplied by the client. Either yields a value or one of the
    /// declared error variants.
    fn from_form_payload(payload: &[u8]) -> Result<Self, FormDecodeError>;
}

// Note: we intentionally do NOT add a blanket
// `impl<T: DeserializeOwned> FromFormPayload for T` because it would
// collide with userland custom impls that wrap a `DeserializeOwned`
// value to add validation. Users wire one of two paths:
//
//   1. Their type derives `serde::Deserialize` AND they hand it to
//      [`form_action_handler_json`] which performs JSON decode under
//      the hood.
//   2. Their type implements `FromFormPayload` directly with custom
//      validation that emits `FormDecodeError::Validation`.

/// The adapter the dispatcher actually sees. Wraps a typed user
/// closure as a Phase-G `ActionHandler`. Construct via
/// [`form_action_handler`] and register through the normal
/// `register_action(action_id, handler)` API on
/// `AlbedoServerBuilder`.
pub struct TypedFormActionHandler<T, F, Fut>
where
    T: FromFormPayload + 'static,
    F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send + 'static,
{
    action_name: String,
    inner: F,
    _phantom_payload: PhantomData<fn() -> T>,
    _phantom_fut: PhantomData<fn() -> Fut>,
}

impl<T, F, Fut> TypedFormActionHandler<T, F, Fut>
where
    T: FromFormPayload + 'static,
    F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send + 'static,
{
    /// Constructs a typed handler bound to `action_name`. The action
    /// name must match the suffix of the JSX form's
    /// `action="action:NAME"` attribute so the `action_id`s line up
    /// on both sides.
    pub fn new(action_name: impl Into<String>, handler: F) -> Self {
        Self {
            action_name: action_name.into(),
            inner: handler,
            _phantom_payload: PhantomData,
            _phantom_fut: PhantomData,
        }
    }

    /// Returns the action name this handler was constructed with.
    /// Exposed for diagnostics and for the server builder to derive
    /// the matching action_id at registration time.
    pub fn action_name(&self) -> &str {
        &self.action_name
    }
}

#[async_trait]
impl<T, F, Fut> ActionHandler for TypedFormActionHandler<T, F, Fut>
where
    T: FromFormPayload + 'static,
    F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send + 'static,
{
    async fn handle(
        &self,
        ctx: &RequestContext,
        envelope: &ActionEnvelope,
        slots: SessionSlots,
    ) -> Result<Vec<Instruction>, RuntimeError> {
        match T::from_form_payload(&envelope.payload) {
            Ok(decoded) => (self.inner)(ctx.clone(), decoded, slots).await,
            Err(FormDecodeError::Validation { errors }) => {
                // Pure-validation failure: ship `SetText` opcodes that
                // populate `<span data-albedo-error="...">` elements
                // for each failing field. No application handler runs,
                // no Navigate emitted — the client stays on the form
                // page with errors rendered in-place.
                Ok(validation_error_text_opcodes(&self.action_name, &errors))
            }
            Err(err) => Err(RuntimeError::RequestHandling(err.to_string())),
        }
    }
}

/// Convenience constructor returning an `Arc<dyn ActionHandler>`
/// ready to drop into
/// `AlbedoServerBuilder::register_action(action_id, ...)`. Pair with
/// [`form_action_id`] to derive the matching `action_id`.
///
/// Use this when your target type implements [`FromFormPayload`]
/// directly (typically with custom validation). For the simpler JSON
/// decode path, use [`form_action_handler_json`] instead.
pub fn form_action_handler<T, F, Fut>(
    action_name: impl Into<String>,
    handler: F,
) -> Arc<dyn ActionHandler>
where
    T: FromFormPayload + 'static,
    F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send + 'static,
{
    Arc::new(TypedFormActionHandler::<T, F, Fut>::new(
        action_name, handler,
    ))
}

/// Wrapper that adapts a `serde::Deserialize` type into a
/// [`FromFormPayload`] without manual validation. Equivalent to
/// `form_action_handler::<JsonFormPayload<T>, _, _>` but hides the
/// wrapping noise at the call site.
///
/// Errors from `serde_json` are surfaced as
/// `FormDecodeError::Malformed` and bubble up to the dispatcher as a
/// 400; this path performs no field-level validation. For per-field
/// error messages, implement `FromFormPayload` manually.
pub fn form_action_handler_json<T, F, Fut>(
    action_name: impl Into<String>,
    handler: F,
) -> Arc<dyn ActionHandler>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(RequestContext, T, SessionSlots) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Vec<Instruction>, RuntimeError>> + Send + 'static,
{
    // Wrap T in `JsonFormPayload<T>` which provides the
    // `FromFormPayload` impl via JSON decoding, then unwrap before
    // calling the user closure.
    let action_name = action_name.into();
    let adapter = move |ctx: RequestContext, payload: JsonFormPayload<T>, slots: SessionSlots| {
        let fut = handler(ctx, payload.0, slots);
        fut
    };
    Arc::new(TypedFormActionHandler::<JsonFormPayload<T>, _, _>::new(
        action_name,
        adapter,
    ))
}

/// Newtype that gives any `DeserializeOwned` type a
/// [`FromFormPayload`] impl through `serde_json::from_slice`. Used
/// internally by [`form_action_handler_json`]; exposed because tests
/// occasionally find it convenient.
pub struct JsonFormPayload<T>(pub T);

impl<T> FromFormPayload for JsonFormPayload<T>
where
    T: serde::de::DeserializeOwned + Send,
{
    fn from_form_payload(payload: &[u8]) -> Result<Self, FormDecodeError> {
        serde_json::from_slice(payload)
            .map(JsonFormPayload)
            .map_err(|err| FormDecodeError::Malformed(err.to_string()))
    }
}

/// FNV-1a-32 of an action name — equal to
/// `dom_render_compiler::transforms::form::allocate_form_action_id`.
/// Re-exported here so server code that registers handlers does not
/// need to import from the compiler crate just to derive an id.
pub fn form_action_id(action_name: &str) -> u32 {
    dom_render_compiler::transforms::form::allocate_form_action_id(action_name)
}
