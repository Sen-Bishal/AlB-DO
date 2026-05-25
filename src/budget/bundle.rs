//! Phase O.3 · bundle-byte attribution.
//!
//! Walks a [`crate::bundler::emit::BundleEmitReport`] and pairs every
//! emitted wrapper module against the bundle runtime map to attribute
//! actual emitted bytes back to the originating component plus its
//! tier. The output drives the tier-B-bundle ceiling in
//! [`crate::budget::config::TierBudget`] — what O.1 estimates with
//! `ComponentManifestEntry.weight_bytes`, O.3 measures from the bytes
//! the user's browser actually downloads.
//!
//! ## Attribution rules
//!
//! - Wrapper module bytes → the owning `component_id`'s tier.
//! - Wrapper source-map bytes (`*.mjs.map`) → the same component as
//!   their paired wrapper. Source maps inflate dev artefacts without
//!   reflecting hot-path size; the formatter calls them out
//!   separately so an oversized map doesn't masquerade as oversized
//!   JS.
//! - Vendor chunks → tracked as a separate "vendor" line, not
//!   per-component, since they're shared across the components that
//!   import them. Per-component ceilings would otherwise double-count
//!   a vendor shared by ten components.
//! - Everything else (manifests, plan JSON, static slices) →
//!   classified as `BundleArtifactClass::Infrastructure` and
//!   excluded from per-component ceilings.

use crate::bundler::emit::{build_bundle_runtime_map, BundleEmitReport, BundleRuntimeMap};
use crate::bundler::plan::BundlePlan;
use crate::manifest::schema::{RenderManifestV2, Tier};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One artifact's attribution within the bundle. The same component
/// can produce multiple entries (one wrapper + one map); the
/// evaluator sums them when checking the per-component ceiling.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleAttribution {
    pub relative_path: String,
    pub bytes: u64,
    pub class: BundleArtifactClass,
    /// Component this artifact belongs to. `None` for vendor chunks
    /// (shared) and infrastructure files (per-build metadata).
    #[serde(default)]
    pub component_id: Option<u64>,
    /// Mirrors `component_id` but stringified for the formatter.
    /// `None` when `component_id` is `None`.
    #[serde(default)]
    pub component_name: Option<String>,
    /// Tier of `component_id`'s component, when known. `None` for
    /// unattributed artifacts.
    #[serde(default)]
    pub tier: Option<Tier>,
}

/// Coarse classification driving how the ceiling is applied.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BundleArtifactClass {
    /// Per-component wrapper module (the JS bakabox loads on hydrate).
    /// This is the bytes the user's browser pays for interactivity —
    /// the primary target of the per-component bundle ceiling.
    Wrapper,
    /// `*.mjs.map` peer of a Wrapper. Counted separately so the diff
    /// can show "the wrapper is 8 KB but the map adds another 12 KB"
    /// without inflating the budget metric.
    SourceMap,
    /// Vendor chunk shared by multiple components. Not attributed to
    /// any single component; surfaced as a totals row.
    Vendor,
    /// Bundle plan / runtime map / static slices / manifest JSON /
    /// precompiled module artefacts. Always emitted regardless of
    /// component composition; excluded from per-component ceilings.
    Infrastructure,
}

/// Aggregated per-component bundle weight + per-class subtotals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BundleByteReport {
    pub attributions: Vec<BundleAttribution>,
    pub per_component: HashMap<u64, ComponentBundleSummary>,
    pub vendor_total_bytes: u64,
    pub infrastructure_total_bytes: u64,
}

/// Per-component summary the evaluator consults to check the
/// per-component bundle ceiling. `wrapper_bytes` is the hot-path
/// metric; `source_map_bytes` is informational.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentBundleSummary {
    pub component_id: u64,
    pub component_name: String,
    pub tier: Tier,
    pub wrapper_bytes: u64,
    pub source_map_bytes: u64,
}

impl ComponentBundleSummary {
    /// Bytes counted against the per-component bundle ceiling.
    /// Maps are excluded — they ride alongside the wrapper in dev
    /// but are off the critical hydration path in production builds
    /// (the user's browser fetches them lazily on demand from
    /// DevTools, not on first paint).
    pub fn budget_bytes(&self) -> u64 {
        self.wrapper_bytes
    }
}

/// Pair an emit report with its source plan + manifest to produce
/// the attribution report. Pure function — no IO, deterministic
/// given the same inputs.
#[must_use]
pub fn compute_bundle_byte_report(
    emit_report: &BundleEmitReport,
    plan: &BundlePlan,
    manifest: &RenderManifestV2,
) -> BundleByteReport {
    let runtime_map = build_bundle_runtime_map(plan);
    let wrapper_index = build_wrapper_index(&runtime_map);
    let vendor_paths = build_vendor_path_set(&runtime_map);
    let component_index = build_component_index(manifest);

    let mut report = BundleByteReport::default();
    let mut summaries: HashMap<u64, ComponentBundleSummary> = HashMap::new();

    for artifact in &emit_report.artifacts {
        let class = classify_artifact(&artifact.relative_path, &wrapper_index, &vendor_paths);
        let bytes = u64::try_from(artifact.bytes).unwrap_or(u64::MAX);
        let (component_id, component_name, tier) = match class {
            BundleArtifactClass::Wrapper => {
                let id = wrapper_index.get(artifact.relative_path.as_str()).copied();
                let meta = id.and_then(|id| component_index.get(&id));
                (id, meta.map(|(name, _)| name.clone()), meta.map(|(_, tier)| *tier))
            }
            BundleArtifactClass::SourceMap => {
                // Map files inherit their wrapper's attribution by
                // stripping the trailing `.map` from the relative
                // path and looking up the resulting wrapper.
                let wrapper_path = artifact.relative_path.trim_end_matches(".map");
                let id = wrapper_index.get(wrapper_path).copied();
                let meta = id.and_then(|id| component_index.get(&id));
                (id, meta.map(|(name, _)| name.clone()), meta.map(|(_, tier)| *tier))
            }
            BundleArtifactClass::Vendor | BundleArtifactClass::Infrastructure => (None, None, None),
        };

        match class {
            BundleArtifactClass::Wrapper => {
                if let (Some(id), Some(name), Some(tier)) = (component_id, &component_name, tier) {
                    let entry = summaries.entry(id).or_insert_with(|| ComponentBundleSummary {
                        component_id: id,
                        component_name: name.clone(),
                        tier,
                        wrapper_bytes: 0,
                        source_map_bytes: 0,
                    });
                    entry.wrapper_bytes = entry.wrapper_bytes.saturating_add(bytes);
                }
            }
            BundleArtifactClass::SourceMap => {
                if let (Some(id), Some(name), Some(tier)) = (component_id, &component_name, tier) {
                    let entry = summaries.entry(id).or_insert_with(|| ComponentBundleSummary {
                        component_id: id,
                        component_name: name.clone(),
                        tier,
                        wrapper_bytes: 0,
                        source_map_bytes: 0,
                    });
                    entry.source_map_bytes = entry.source_map_bytes.saturating_add(bytes);
                }
            }
            BundleArtifactClass::Vendor => {
                report.vendor_total_bytes = report.vendor_total_bytes.saturating_add(bytes);
            }
            BundleArtifactClass::Infrastructure => {
                report.infrastructure_total_bytes =
                    report.infrastructure_total_bytes.saturating_add(bytes);
            }
        }

        report.attributions.push(BundleAttribution {
            relative_path: artifact.relative_path.clone(),
            bytes,
            class,
            component_id,
            component_name,
            tier,
        });
    }

    report.attributions.sort_by(|left, right| {
        left.relative_path
            .cmp(&right.relative_path)
            .then_with(|| left.bytes.cmp(&right.bytes))
    });
    report.per_component = summaries;
    report
}

fn build_wrapper_index(runtime_map: &BundleRuntimeMap) -> HashMap<String, u64> {
    runtime_map
        .modules
        .iter()
        .map(|module| (module.wrapper_module.clone(), module.component_id))
        .collect()
}

fn build_vendor_path_set(runtime_map: &BundleRuntimeMap) -> std::collections::HashSet<String> {
    runtime_map
        .modules
        .iter()
        .flat_map(|module| module.vendor_chunks.iter().cloned())
        .collect()
}

fn build_component_index(manifest: &RenderManifestV2) -> HashMap<u64, (String, Tier)> {
    manifest
        .components
        .iter()
        .map(|component| (component.id, (component.name.clone(), component.tier)))
        .collect()
}

fn classify_artifact(
    relative_path: &str,
    wrapper_index: &HashMap<String, u64>,
    vendor_paths: &std::collections::HashSet<String>,
) -> BundleArtifactClass {
    if relative_path.ends_with(".map") {
        return BundleArtifactClass::SourceMap;
    }
    if wrapper_index.contains_key(relative_path) {
        return BundleArtifactClass::Wrapper;
    }
    if vendor_paths.contains(relative_path) {
        return BundleArtifactClass::Vendor;
    }
    BundleArtifactClass::Infrastructure
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundler::classify::BundleClass;
    use crate::bundler::emit::EmittedArtifact;
    use crate::bundler::plan::{BundleModulePlan, BundlePlan};
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode};
    use std::path::PathBuf;

    fn entry(id: u64, name: &str, tier: Tier, weight_bytes: u64) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: name.to_string(),
            module_path: format!("src/{name}.tsx"),
            tier,
            weight_bytes,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer: false,
            hydration_mode: HydrationMode::None,
        }
    }

    fn bundle_module(id: u64, name: &str, class: BundleClass) -> BundleModulePlan {
        BundleModulePlan {
            component_id: id,
            module_path: format!("src/{name}.tsx"),
            class,
            dependency_ids: Vec::new(),
            wrapper_module_path: format!("__albedo__/wrappers/{name}.mjs"),
            dom_position: None,
        }
    }

    fn plan(modules: Vec<BundleModulePlan>) -> BundlePlan {
        BundlePlan {
            version: "1.0".to_string(),
            manifest_schema_version: "2.0".to_string(),
            manifest_generated_at: String::new(),
            entry_component_id: modules.first().map(|m| m.component_id),
            modules,
            vendor_chunks: Vec::new(),
            rewrite_actions: Vec::new(),
        }
    }

    fn artifact(path: &str, bytes: usize) -> EmittedArtifact {
        EmittedArtifact {
            relative_path: path.to_string(),
            bytes,
        }
    }

    fn manifest_for(components: Vec<ComponentManifestEntry>) -> RenderManifestV2 {
        let mut m = RenderManifestV2::legacy_defaults();
        m.components = components;
        m
    }

    #[test]
    fn wrapper_bytes_attribute_to_owning_component() {
        let manifest = manifest_for(vec![entry(1, "Counter", Tier::B, 0)]);
        let plan = plan(vec![bundle_module(1, "Counter", BundleClass::Critical)]);
        let emit_report = BundleEmitReport {
            output_dir: PathBuf::from(""),
            artifacts: vec![artifact("__albedo__/wrappers/Counter.mjs", 4096)],
        };

        let report = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        let summary = report.per_component.get(&1).expect("Counter present");
        assert_eq!(summary.wrapper_bytes, 4096);
        assert_eq!(summary.source_map_bytes, 0);
        assert_eq!(summary.tier, Tier::B);
        assert_eq!(summary.component_name, "Counter");
    }

    #[test]
    fn source_map_attributes_to_its_paired_wrapper() {
        let manifest = manifest_for(vec![entry(2, "Hero", Tier::B, 0)]);
        let plan = plan(vec![bundle_module(2, "Hero", BundleClass::Critical)]);
        let emit_report = BundleEmitReport {
            output_dir: PathBuf::from(""),
            artifacts: vec![
                artifact("__albedo__/wrappers/Hero.mjs", 1024),
                artifact("__albedo__/wrappers/Hero.mjs.map", 8000),
            ],
        };

        let report = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        let summary = report.per_component.get(&2).unwrap();
        assert_eq!(summary.wrapper_bytes, 1024);
        assert_eq!(summary.source_map_bytes, 8000);
        // Budget bytes exclude the source map.
        assert_eq!(summary.budget_bytes(), 1024);
    }

    #[test]
    fn infrastructure_artifacts_do_not_attribute_to_a_component() {
        let manifest = manifest_for(vec![entry(3, "Lone", Tier::B, 0)]);
        let plan = plan(vec![bundle_module(3, "Lone", BundleClass::Critical)]);
        let emit_report = BundleEmitReport {
            output_dir: PathBuf::from(""),
            artifacts: vec![
                artifact("bundle-plan.json", 2048),
                artifact("__albedo__/wrappers/Lone.mjs", 512),
                artifact("static-slices.json", 600),
            ],
        };

        let report = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        assert_eq!(report.infrastructure_total_bytes, 2048 + 600);
        assert_eq!(report.per_component.get(&3).unwrap().wrapper_bytes, 512);
    }

    #[test]
    fn multiple_wrappers_summed_independently_per_component() {
        let manifest = manifest_for(vec![
            entry(10, "A", Tier::B, 0),
            entry(11, "B", Tier::B, 0),
        ]);
        let plan = plan(vec![
            bundle_module(10, "A", BundleClass::Critical),
            bundle_module(11, "B", BundleClass::Critical),
        ]);
        let emit_report = BundleEmitReport {
            output_dir: PathBuf::from(""),
            artifacts: vec![
                artifact("__albedo__/wrappers/A.mjs", 4000),
                artifact("__albedo__/wrappers/B.mjs", 9000),
            ],
        };

        let report = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        assert_eq!(report.per_component[&10].wrapper_bytes, 4000);
        assert_eq!(report.per_component[&11].wrapper_bytes, 9000);
    }

    #[test]
    fn output_is_deterministic_across_calls() {
        let manifest = manifest_for(vec![entry(1, "X", Tier::B, 0)]);
        let plan = plan(vec![bundle_module(1, "X", BundleClass::Critical)]);
        let emit_report = BundleEmitReport {
            output_dir: PathBuf::from(""),
            artifacts: vec![artifact("__albedo__/wrappers/X.mjs", 100)],
        };

        let a = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        let b = compute_bundle_byte_report(&emit_report, &plan, &manifest);
        assert_eq!(a, b);
    }
}
