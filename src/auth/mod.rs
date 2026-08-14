//! AUTH — identity as a key source (`development-plan/AUTH.md`, TODO #1 item 5).
//!
//! ## The sentence
//!
//! **A principal is a compiler-known value, so an authorization policy is never
//! written — it is derived from the read that needs it.**
//!
//! Two corollaries, and they are the whole design:
//!
//! - **Identity is one more key source, beside `params`.** `user.id` mints a topic identity exactly
//!   as `params.id` already does, so `todos.where({ owner: user.id })` is per-user live data with no
//!   authorization code anywhere in the app. A session that is not `u_7f3a` cannot *name*
//!   `todos:owner=u_7f3a`, which is the same guarantee PRISM already gives two chat rooms —
//!   inherited, not rebuilt.
//! - **A provider is one more declaration, beside `sources`.** Ours and a third party's produce the
//!   same [`Principal`], so app code cannot tell them apart and a provider swap is a config edit.
//!
//! ## Module map
//!
//! | module | holds |
//! |---|---|
//! | [`principal`] | [`Principal`], [`PrincipalId`] — the one shape every provider lands on |
//! | [`declare`] | the `auth` block: providers, presets, validation, egress derivation |
//! | [`schema`] | the four tables, emitted through FORGE rather than adapted to it |
//!
//! ## Why the id is ours, and what it settled
//!
//! [`PrincipalId`] is minted here, never adopted from a provider. That is forced
//! rather than chosen: an id becomes a topic namespace the moment a component
//! reads `user.id`, and Auth0 subjects (`google-oauth2|…`) and email-keyed ids
//! are outside the partition-key alphabet. The consequence is that `AUTH.md`'s
//! open question R3 — *mirror or reference?* — resolves to **mirror**, which is
//! also what makes joins and § 5's instant global logout possible. See
//! [`principal`] for the full argument.
//!
//! ## Why a fourth provider kind exists that `AUTH.md` does not mention
//!
//! The design doc caps the provider list at five presets plus generic OIDC
//! discovery. That covers the compliant long tail but not an app that already
//! has auth, or one whose identity provider is something we would never think to
//! name. [`declare::ProviderKind::Custom`] closes that: author-supplied code
//! returns a [`Principal`], and because it returns *that* type, everything
//! derived from a principal keeps working over an implementation we have never
//! seen. It is the only kind whose correctness we cannot argue for, which is why
//! it is last and why it is explicit.

pub mod declare;
pub mod password;
pub mod principal;
pub mod schema;
pub mod session;
pub mod store;

pub use declare::{
    is_valid_provider_name, preset_for, AuthDeclaration, AuthRegistry, AuthSchemaError, Endpoints,
    Preset, ProviderDecl, ProviderKind, ResolvedProvider, SecretDecl, SessionDecl,
    DEFAULT_SESSION_COOKIE, DEFAULT_SESSION_TTL, PRESETS,
};
pub use password::{
    absorb_timing, hash_password, normalize_email, verify_password, PasswordError,
    MAX_PASSWORD_BYTES, MIN_PASSWORD_BYTES,
};
pub use principal::{Principal, PrincipalId, PrincipalIdError, PRINCIPAL_ID_PREFIX};
pub use session::{
    clear_cookie_value, cookie_entries, read_cookie, set_cookie_value, SessionRecord,
    SessionRejection, SessionToken, TokenHash,
};
pub use store::{PasswordCredential, ProviderProfile, Resolved, StoreError};
