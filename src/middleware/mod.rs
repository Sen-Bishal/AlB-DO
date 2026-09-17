//! MIDDLEWARE — `TODO.md` item 15.6.
//!
//! ## The sentence
//!
//! **A middleware is one `src/middleware.ts` whose matcher is compiled and whose
//! body runs in the render pool — and it can refuse, redirect or rewrite a
//! request, but it can never grant one.**
//!
//! That last clause is the design. Identity is resolved before the middleware
//! runs and handed to it; the route's own `export const auth` gate runs *after*
//! it, against whatever route the request finally lands on. So a middleware that
//! is wrong can only make an app stricter than intended or send someone to the
//! wrong page — it cannot open a gated route, and a rewrite onto one is gated
//! like a direct request.
//!
//! ## Module map
//!
//! | module | holds |
//! |---|---|
//! | [`declare`] | finding the file, lowering `config.matcher`, the load order |
//! | [`matcher`] | the compiled path patterns, checked in Rust per request |
//! | [`outcome`] | what the body returned, validated before it touches a response |
//!
//! The dispatch itself lives in `albedo_server::middleware`, the half that needs
//! a request — the split [`crate::upload`] and [`crate::auth::oauth`] use.
//!
//! ## What is deliberately not here
//!
//! No request-body access, no request-header mutation, no chaining of several
//! middleware files. Item 15's rule for a table-stakes surface is to build it
//! cheaply and not innovate.
//!
//! ## `fetch()`
//!
//! Supported, and **not** by holding an engine across the round trip — that is
//! the design APERTURE's gate 5 measured at 403.9 ms against 52.7 ms. A call the
//! journal cannot answer returns a promise that never settles on that pass; the
//! host sees the queue stall with requests staged, releases the engine, resolves
//! them through the same `aperture::resolve_pending` an action uses, and runs the
//! body again. A promise rather than the action path's thrown sentinel, because a
//! middleware is an ordinary module the compiler does not rewrite: a sentinel
//! thrown into a userland `try/catch` would be swallowed.
//!
//! Every call is charged to the visitor's **outbound** bucket before it leaves,
//! and must be awaited — a call still pending when the body returns is an error,
//! not a background request.

pub mod declare;
pub mod matcher;
pub mod outcome;

pub use declare::{declaration, module_graph, MiddlewareDecl};
pub use matcher::Matcher;
pub use outcome::Outcome;

/// The import the helpers come from: `import { redirect } from "albedo/middleware"`.
///
/// One spelling, consulted by every place that decides what a framework
/// specifier is — the npm scanner must not try to bundle it and the engine must
/// bind it to its shims.
pub const MIDDLEWARE_MODULE: &str = "albedo/middleware";
