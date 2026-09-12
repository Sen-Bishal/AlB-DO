//! UPLOADS — `TODO.md` item 15.1.
//!
//! ## The sentence
//!
//! **An upload is a declared bucket with a bound, and what an action handler
//! receives is a reference — never bytes.**
//!
//! Two corollaries, and they are the whole design:
//!
//! - **The bound is knowable before the body is read.** `transforms::form`'s foreclosure note said
//!   streaming uploads were impossible because the server could not know what an action was until
//!   it had buffered the body. Half of that was fixed when the action's *name* moved into the
//!   request line; [`declare`] is the other half, so the caps are a map lookup away at the moment
//!   the headers arrive.
//! - **Bytes never enter the JS engine.** A multipart file part streams to disk, hashed as it goes,
//!   and the action body sees the resulting id as the field's value — exactly as a text field's
//!   value is its text. The action ABI does not change, no new JS type exists, and a 5 MiB photo
//!   never touches the QuickJS arena.
//!
//! ## Module map
//!
//! | module | holds |
//! |---|---|
//! | [`declare`] | the `uploads` block: buckets, accept lists, size ceilings |
//! | [`store`] | the content-addressed layout and the `albedo_uploads` row |
//!
//! The streaming itself lives in `albedo_server::uploads`, because it is the
//! half that needs a request body — the same split [`crate::auth::oauth`] uses.
//!
//! ## What is deliberately not here
//!
//! No image transform pipeline, no thumbnailing, no CDN, no signed URLs. Item
//! 15.9 names image optimisation as deferred *by decision rather than by
//! oversight*, and item 15's rule for a table-stakes surface is to build it
//! cheaply and not innovate. The one thing worth saying out loud is in
//! [`store`]: **a content-addressed id is not a capability**, so a private file
//! needs an authorization check on the read like any other row.

pub mod declare;
pub mod store;

pub use declare::{
    is_valid_bucket_name, ResolvedBucket, UploadDecl, UploadRegistry, UploadSchemaError,
    DEFAULT_MAX_BYTES, MAX_DECLARABLE_BYTES,
};
pub use store::{
    is_upload_id, path_for, sanitise_name, StoredUpload, UploadStoreError, UPLOADS, UPLOAD_DIR,
};
