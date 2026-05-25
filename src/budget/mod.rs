//! Phase O.1 · CHAIN REACTION — tier budget API.
//!
//! Reads a `tier-budget.toml` at the project root and evaluates the
//! current [`crate::manifest::schema::RenderManifestV2`] against it.
//! Build paths fail when any ceiling is exceeded; the per-violation
//! breakdown carries the top contributing components so the fix is
//! obvious from CI output alone.
//!
//! ## Why this is a gate, not a hint
//!
//! Tier classification (`A` / `B` / `C`) is the framework's load-bearing
//! contract — Tier-A ships zero JS, Tier-B ships an island, Tier-C
//! streams. Without a ceiling, a careless import quietly promotes
//! a component up the tier ladder, and the cost shows up in
//! production. The budget makes the promotion visible at commit
//! time.

pub mod config;
pub mod format;
pub mod report;

pub use config::{
    load_budget_from_dir, BudgetDefaults, BudgetLoadError, RouteBudget, TierBudget,
    TIER_BUDGET_FILENAME,
};
pub use format::format_report_pretty;
pub use report::{
    evaluate_budget, BudgetReport, BudgetViolation, ComponentContribution, ViolationKind,
};
