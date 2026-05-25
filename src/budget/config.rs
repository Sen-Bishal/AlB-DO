//! Budget configuration — `tier-budget.toml` parsing + built-in defaults.
//!
//! The file lives at the project root (next to `albedo.config.ts`).
//! Missing file → built-in defaults; present file → defaults overlaid
//! by `[defaults]` keys; `[routes."/some/path"]` blocks override on a
//! per-route basis.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Canonical filename the loader searches for at the project root.
pub const TIER_BUDGET_FILENAME: &str = "tier-budget.toml";

/// Built-in ceiling for Tier-A components per route. Generous enough
/// for landing pages with many static cards; tight enough to flag the
/// "I accidentally rendered the entire catalog" mistake.
pub const DEFAULT_TIER_A_MAX_COMPONENTS_PER_ROUTE: u32 = 50;

/// Built-in ceiling for total Tier-B hydration weight per route, in KB.
pub const DEFAULT_TIER_B_MAX_KB_PER_ROUTE: u32 = 8;

/// Built-in ceiling for a single Tier-B component's hydration weight,
/// in KB. Catches the "one giant island" anti-pattern even when the
/// per-route sum is still under budget.
pub const DEFAULT_TIER_B_MAX_KB_PER_COMPONENT: u32 = 4;

/// Built-in ceiling for the number of Tier-C nodes a single route may
/// stream in parallel. Higher numbers can saturate the WT patches lane.
pub const DEFAULT_TIER_C_MAX_CONCURRENT_FETCHES_PER_ROUTE: u32 = 10;

/// Phase O.3 · built-in ceiling for the *emitted* per-component
/// Tier-B wrapper bundle, in KB. This is the bytes the client's
/// browser actually downloads to hydrate one island, distinct from
/// the source-weight estimate in [`Self::tier_b_max_kb_per_component`].
/// 1 KB is the sprint-plan default — "Counter" with no dependencies
/// compiles well under it; importing `lodash` blows past instantly.
pub const DEFAULT_TIER_B_BUNDLE_MAX_KB_PER_COMPONENT: u32 = 1;

/// Fully-resolved budget shape after defaults + file overlay. Pass
/// this to [`crate::budget::evaluate_budget`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TierBudget {
    #[serde(default)]
    pub defaults: BudgetDefaults,
    #[serde(default)]
    pub routes: BTreeMap<String, RouteBudget>,
}

impl Default for TierBudget {
    fn default() -> Self {
        Self {
            defaults: BudgetDefaults::default(),
            routes: BTreeMap::new(),
        }
    }
}

impl TierBudget {
    /// Returns the resolved budget for `route_path`, layering any
    /// per-route override on top of the defaults.
    pub fn for_route(&self, route_path: &str) -> BudgetDefaults {
        let Some(override_block) = self.routes.get(route_path) else {
            return self.defaults.clone();
        };
        BudgetDefaults {
            tier_a_max_components_per_route: override_block
                .tier_a_max_components_per_route
                .unwrap_or(self.defaults.tier_a_max_components_per_route),
            tier_b_max_kb_per_route: override_block
                .tier_b_max_kb_per_route
                .unwrap_or(self.defaults.tier_b_max_kb_per_route),
            tier_b_max_kb_per_component: override_block
                .tier_b_max_kb_per_component
                .unwrap_or(self.defaults.tier_b_max_kb_per_component),
            tier_c_max_concurrent_fetches_per_route: override_block
                .tier_c_max_concurrent_fetches_per_route
                .unwrap_or(self.defaults.tier_c_max_concurrent_fetches_per_route),
            tier_b_bundle_max_kb_per_component: override_block
                .tier_b_bundle_max_kb_per_component
                .unwrap_or(self.defaults.tier_b_bundle_max_kb_per_component),
        }
    }
}

/// Ceiling values applied when no per-route override is present.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetDefaults {
    #[serde(default = "default_tier_a_max_components_per_route")]
    pub tier_a_max_components_per_route: u32,
    #[serde(default = "default_tier_b_max_kb_per_route")]
    pub tier_b_max_kb_per_route: u32,
    #[serde(default = "default_tier_b_max_kb_per_component")]
    pub tier_b_max_kb_per_component: u32,
    #[serde(default = "default_tier_c_max_concurrent_fetches_per_route")]
    pub tier_c_max_concurrent_fetches_per_route: u32,
    /// Phase O.3 · ceiling for the *emitted* Tier-B wrapper bundle
    /// per component, in KB. Distinct key from
    /// `tier_b_max_kb_per_component` (source-weight estimate) so a
    /// project can ship either or both gates. The bundle gate runs
    /// post-emit and is the metric that matches what the user's
    /// browser downloads.
    #[serde(default = "default_tier_b_bundle_max_kb_per_component")]
    pub tier_b_bundle_max_kb_per_component: u32,
}

impl Default for BudgetDefaults {
    fn default() -> Self {
        Self {
            tier_a_max_components_per_route: DEFAULT_TIER_A_MAX_COMPONENTS_PER_ROUTE,
            tier_b_max_kb_per_route: DEFAULT_TIER_B_MAX_KB_PER_ROUTE,
            tier_b_max_kb_per_component: DEFAULT_TIER_B_MAX_KB_PER_COMPONENT,
            tier_c_max_concurrent_fetches_per_route:
                DEFAULT_TIER_C_MAX_CONCURRENT_FETCHES_PER_ROUTE,
            tier_b_bundle_max_kb_per_component: DEFAULT_TIER_B_BUNDLE_MAX_KB_PER_COMPONENT,
        }
    }
}

/// Per-route override block. Every field is optional; absent fields
/// fall through to [`BudgetDefaults`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RouteBudget {
    #[serde(default)]
    pub tier_a_max_components_per_route: Option<u32>,
    #[serde(default)]
    pub tier_b_max_kb_per_route: Option<u32>,
    #[serde(default)]
    pub tier_b_max_kb_per_component: Option<u32>,
    #[serde(default)]
    pub tier_c_max_concurrent_fetches_per_route: Option<u32>,
    #[serde(default)]
    pub tier_b_bundle_max_kb_per_component: Option<u32>,
}

/// Error surface for [`load_budget_from_dir`]. Distinct from IO
/// failures of the manifest evaluator so the CLI can report
/// "your budget file is malformed" separately from "your manifest
/// blew up".
#[derive(Debug)]
pub enum BudgetLoadError {
    Io {
        path: PathBuf,
        message: String,
    },
    Parse {
        path: PathBuf,
        message: String,
    },
}

impl std::fmt::Display for BudgetLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => {
                write!(f, "failed to read '{}': {message}", path.display())
            }
            Self::Parse { path, message } => {
                write!(f, "failed to parse '{}': {message}", path.display())
            }
        }
    }
}

impl std::error::Error for BudgetLoadError {}

/// Look for `tier-budget.toml` in `project_dir`. Returns `Ok(None)`
/// when the file is absent (callers fall back to built-in defaults
/// only when *explicitly* doing so — auto-fallback hides typos).
pub fn load_budget_from_dir(project_dir: &Path) -> Result<Option<TierBudget>, BudgetLoadError> {
    let path = project_dir.join(TIER_BUDGET_FILENAME);
    if !path.is_file() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(&path).map_err(|err| BudgetLoadError::Io {
        path: path.clone(),
        message: err.to_string(),
    })?;
    let parsed: TierBudget = toml::from_str(&contents).map_err(|err| BudgetLoadError::Parse {
        path,
        message: err.to_string(),
    })?;
    Ok(Some(parsed))
}

const fn default_tier_a_max_components_per_route() -> u32 {
    DEFAULT_TIER_A_MAX_COMPONENTS_PER_ROUTE
}

const fn default_tier_b_max_kb_per_route() -> u32 {
    DEFAULT_TIER_B_MAX_KB_PER_ROUTE
}

const fn default_tier_b_max_kb_per_component() -> u32 {
    DEFAULT_TIER_B_MAX_KB_PER_COMPONENT
}

const fn default_tier_c_max_concurrent_fetches_per_route() -> u32 {
    DEFAULT_TIER_C_MAX_CONCURRENT_FETCHES_PER_ROUTE
}

const fn default_tier_b_bundle_max_kb_per_component() -> u32 {
    DEFAULT_TIER_B_BUNDLE_MAX_KB_PER_COMPONENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_file_yields_none() {
        let temp = tempdir().unwrap();
        let result = load_budget_from_dir(temp.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn builtin_defaults_match_documented_values() {
        let defaults = BudgetDefaults::default();
        assert_eq!(defaults.tier_a_max_components_per_route, 50);
        assert_eq!(defaults.tier_b_max_kb_per_route, 8);
        assert_eq!(defaults.tier_b_max_kb_per_component, 4);
        assert_eq!(defaults.tier_c_max_concurrent_fetches_per_route, 10);
        assert_eq!(defaults.tier_b_bundle_max_kb_per_component, 1);
    }

    #[test]
    fn empty_file_uses_serde_defaults() {
        let temp = tempdir().unwrap();
        std::fs::write(temp.path().join(TIER_BUDGET_FILENAME), "[defaults]\n").unwrap();
        let budget = load_budget_from_dir(temp.path()).unwrap().unwrap();
        assert_eq!(budget.defaults, BudgetDefaults::default());
        assert!(budget.routes.is_empty());
    }

    #[test]
    fn defaults_block_overlays_individual_fields() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join(TIER_BUDGET_FILENAME),
            "[defaults]\ntier_b_max_kb_per_route = 16\n",
        )
        .unwrap();
        let budget = load_budget_from_dir(temp.path()).unwrap().unwrap();
        assert_eq!(budget.defaults.tier_b_max_kb_per_route, 16);
        // Other defaults untouched.
        assert_eq!(budget.defaults.tier_b_max_kb_per_component, 4);
    }

    #[test]
    fn route_override_resolves_correctly() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join(TIER_BUDGET_FILENAME),
            "[defaults]\n\
             tier_b_max_kb_per_route = 8\n\
             \n\
             [routes.\"/dashboard\"]\n\
             tier_b_max_kb_per_route = 24\n",
        )
        .unwrap();
        let budget = load_budget_from_dir(temp.path()).unwrap().unwrap();
        let resolved = budget.for_route("/dashboard");
        assert_eq!(resolved.tier_b_max_kb_per_route, 24);
        // Other fields fall through to defaults.
        assert_eq!(resolved.tier_b_max_kb_per_component, 4);

        let unrelated = budget.for_route("/about");
        assert_eq!(unrelated.tier_b_max_kb_per_route, 8);
    }

    #[test]
    fn malformed_toml_surfaces_parse_error() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join(TIER_BUDGET_FILENAME),
            "[defaults\ntier_b_max_kb_per_route = oops",
        )
        .unwrap();
        let err = load_budget_from_dir(temp.path()).unwrap_err();
        assert!(matches!(err, BudgetLoadError::Parse { .. }));
    }
}
