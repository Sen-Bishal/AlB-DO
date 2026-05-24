//! Phase M · server-side developer-tooling surfaces.
//!
//! Modules in here only run in dev builds or when the server is
//! explicitly configured for development. Production code paths must
//! not depend on them; the inspector lives under
//! [`crate::inspector`] for that reason.

pub mod error_overlay;
pub mod hmr;

pub use error_overlay::{
    DevError, DevErrorRegistry, ErrorKind, OverlayEvent, SharedErrorRegistry,
};
pub use hmr::{HmrEvent, HmrPayload, HmrRegistry, SharedHmrRegistry};
