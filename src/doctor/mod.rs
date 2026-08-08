//! `albedo doctor` — what the build already knows about itself.
//!
//! *`TODO.md` item 8 (trust polish) and `AUTH.md` § 9 P5.*
//!
//! Every check here is a **derivation, not a document**. That is the whole
//! organising rule: doctor is allowed to report things the compiler established
//! for its own reasons, and is not allowed to carry a list somebody maintains by
//! hand. A hand-maintained list drifts from the system silently, which is the
//! failure mode the tool exists to remove — so shipping one inside the tool
//! would be the joke telling itself.
//!
//! What that buys is that doctor cannot be *wrong* about a built project, only
//! *incomplete*. It is incomplete in one large way today, stated at the top of
//! [`matrix`]: nothing can be keyed by the session's principal until AUTH item
//! 5's P1 lands, so the matrix's identity column is empty by construction rather
//! than by omission.

pub mod matrix;

pub use matrix::{Finding, Matrix, ReachKey, Read, ReadSubject, RouteReach};
