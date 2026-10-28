//! What an action body reads about the request that ran it, beside its payload.
//!
//! One struct rather than one parameter per name, because every name added here
//! has to reach BOTH executors — the pure-Rust interpreter's environment and the
//! QuickJS handler script — through the same chain of calls, and the two must
//! bind it identically or a body works under one and not the other. A new
//! request-scoped name is a field here and one line in each executor, not a
//! parameter threaded through every `invoke_action_*` again.
//!
//! Every field is ALWAYS bound — to `null` when absent — so no module constant
//! or prop of the same name can stand in for it. See `REQUEST_BOUND_NAMES` in
//! [`crate::runtime::bridge`].

use serde_json::Value;

/// The request-scoped names an action body reads: `user` and `origin`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RequestBindings {
    /// `user` — [`Principal::user_value`](crate::auth::Principal::user_value).
    /// `None` is an anonymous request, and binds `null`.
    pub user: Option<Value>,
    /// `origin` — where this app is reached, as a bare `scheme://host[:port]`,
    /// for building an absolute URL an outside service sends people back to (a
    /// payment provider's return URL). From configuration only:
    /// `ALBEDO_PUBLIC_ORIGIN`, the ACME domain, or localhost in development —
    /// **never the request's `Host` header**, which the caller controls. `None`
    /// when nothing configures one, and binds `null`.
    pub origin: Option<String>,
}

impl RequestBindings {
    /// No principal and no origin — the bindings of a request nothing is known
    /// about. A `'static` reference, so a literal can borrow it anywhere.
    #[must_use]
    pub fn anonymous() -> &'static Self {
        static ANONYMOUS: RequestBindings = RequestBindings {
            user: None,
            origin: None,
        };
        &ANONYMOUS
    }

    /// `origin` as the JSON value both executors bind.
    #[must_use]
    pub fn origin_value(&self) -> Value {
        self.origin.clone().map_or(Value::Null, Value::String)
    }
}
