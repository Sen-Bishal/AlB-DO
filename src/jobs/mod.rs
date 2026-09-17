//! JOBS — `TODO.md` item 15.5.
//!
//! ## The sentence
//!
//! **A job is one export in `src/jobs.ts` that the server runs on a schedule or
//! on demand — for a principal, never as the system.**
//!
//! ## What owning this lets the compiler say
//!
//! Item 15's rule is *for each surface, does owning it let the compiler say
//! something nobody else can?* Background work is table stakes and most of this
//! module is deliberately small. Two things are not:
//!
//! **1 · Exactly-once firing is a property of the schema.** A schedule is a pure
//! function from an instant to the next instant ([`schedule`]), so every process
//! computes the same fire times; the fire's row id is derived from one
//! ([`queue::fire_id`]); and the id is the primary key. Two processes racing the
//! 03:00 tick therefore produce one row, and the loser's `INSERT OR IGNORE` is a
//! no-op. **That holds on the multi-process topology `TODO.md` item 13.2
//! refuses, and across a restart mid-tick.** A uuid would have bought nothing
//! and cost exactly this.
//!
//! **2 · A per-user job exists without a god-mode principal existing.** The
//! usual way a framework delivers "email every user their digest" is an
//! identity that sees every row. This one expands the tick into one run per
//! principal ([`declare::FanOut`]), each carrying exactly one — so the
//! capability arrives and the number of partition bypasses in this codebase
//! stays zero. There is no privileged principal to leak, to forget to check, or
//! to inherit by accident.
//!
//! Because the manifest already records which collections are partitioned by
//! identity (`manifest::schema::PartitionKeySource::Identity`), a scheduled job
//! that reads one without a principal to read it under is a **build error**, not
//! a runtime surprise. A runtime can warn; a compiler can refuse.
//!
//! ## Module map
//!
//! | module | holds |
//! |---|---|
//! | [`declare`] | finding `src/jobs.ts`, lowering each `job({ … }, handler)` |
//! | [`schedule`] | cron / `@alias` / `every N`, and the next instant |
//! | [`queue`] | the `albedo_jobs` table, the lease, backoff, the fan-out cursor |
//!
//! The runner itself lives in `albedo_server::jobs` — the half that needs a
//! process, a clock and an engine pool. The same split [`crate::upload`],
//! [`crate::middleware`] and [`crate::auth::oauth`] use.
//!
//! ## What is deliberately not here
//!
//! No distributed queue, no priorities, no job chaining or dependency graph, no
//! per-job concurrency limits, and no cancellation of an already-running job.
//! Each is a real feature and none is table stakes; every one of them is
//! cheaper to add later than to remove.

pub mod builtin;
pub mod collect;
pub mod declare;
pub mod queue;
pub mod schedule;

pub use builtin::{Builtin, BUILTIN_PREFIX};
pub use collect::{install_enqueue_collector, EnqueueCollector, EnqueueIntent};
pub use declare::{FanOut, JobDecl, JobsDecl, JOBS_MODULE};
pub use queue::{ClaimedJob, Enqueue, JobState};
pub use schedule::Schedule;
