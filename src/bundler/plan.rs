use super::classify::{classify_component, BundleClass};
use super::rewrite::{stable_wrapper_module_path, RewriteAction};
use super::vendor::{infer_package_name, plan_vendor_chunks, VendorChunkPlan, VendorPlanOptions};
use crate::manifest::schema::RenderManifestV2;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const BUNDLE_PLAN_VERSION: &str = "1.0";

#[derive(Debug, Clone)]
pub struct BundlePlanOptions {
    pub vendor: VendorPlanOptions,
}

impl Default for BundlePlanOptions {
    fn default() -> Self {
        Self {
            vendor: VendorPlanOptions::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundleModulePlan {
    pub component_id: u64,
    pub module_path: String,
    pub class: BundleClass,
    pub dependency_ids: Vec<u64>,
    pub wrapper_module_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BundlePlan {
    pub version: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub entry_component_id: Option<u64>,
    pub modules: Vec<BundleModulePlan>,
    pub vendor_chunks: Vec<VendorChunkPlan>,
    pub rewrite_actions: Vec<RewriteAction>,
}

pub fn build_bundle_plan(manifest: &RenderManifestV2, options: &BundlePlanOptions) -> BundlePlan {
    let entry_component_id = manifest.critical_path.last().copied();
    let critical_ids = manifest
        .critical_path
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    let vendor_chunks = plan_vendor_chunks(manifest, &options.vendor);
    let chunk_index = build_vendor_chunk_index(&vendor_chunks);

    let mut components = manifest.components.clone();
    components.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.module_path.cmp(&right.module_path))
    });

    let mut modules = Vec::with_capacity(components.len());
    let mut rewrite_actions = Vec::new();

    for component in components {
        let wrapper_module_path = stable_wrapper_module_path(&component.module_path);
        let class = classify_component(&component, entry_component_id, &critical_ids);
        let mut dependency_ids = component.dependencies.clone();
        dependency_ids.sort_unstable();

        modules.push(BundleModulePlan {
            component_id: component.id,
            module_path: component.module_path.clone(),
            class,
            dependency_ids,
            wrapper_module_path: wrapper_module_path.clone(),
        });

        rewrite_actions.push(RewriteAction::WrapModule {
            component_id: component.id,
            source_module: component.module_path.clone(),
            wrapper_module: wrapper_module_path,
        });

        if let Some(package_name) = infer_package_name(&component.module_path) {
            if let Some(chunk_names) = chunk_index.get(&package_name) {
                for chunk_name in chunk_names {
                    rewrite_actions.push(RewriteAction::LinkVendorChunk {
                        component_id: component.id,
                        chunk_name: chunk_name.clone(),
                    });
                }
            }
        }
    }

    rewrite_actions
        .sort_by(|left, right| rewrite_action_sort_key(left).cmp(&rewrite_action_sort_key(right)));

    BundlePlan {
        version: BUNDLE_PLAN_VERSION.to_string(),
        manifest_schema_version: manifest.schema_version.clone(),
        manifest_generated_at: manifest.generated_at.clone(),
        entry_component_id,
        modules,
        vendor_chunks,
        rewrite_actions,
    }
}

fn build_vendor_chunk_index(chunks: &[VendorChunkPlan]) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for chunk in chunks {
        for package in &chunk.packages {
            index
                .entry(package.clone())
                .or_default()
                .push(chunk.chunk_name.clone());
        }
    }

    for chunk_names in index.values_mut() {
        chunk_names.sort();
        chunk_names.dedup();
    }

    index
}

fn rewrite_action_sort_key(action: &RewriteAction) -> (u64, u8, &str, &str) {
    match action {
        RewriteAction::WrapModule {
            component_id,
            source_module,
            wrapper_module,
        } => (
            *component_id,
            0,
            source_module.as_str(),
            wrapper_module.as_str(),
        ),
        RewriteAction::LinkVendorChunk {
            component_id,
            chunk_name,
        } => (*component_id, 1, chunk_name.as_str(), ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier, VendorChunk,
    };

    fn component(id: u64, module_path: &str, dependencies: Vec<u64>) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: format!("C{id}"),
            module_path: module_path.to_string(),
            tier: Tier::C,
            weight_bytes: 2048,
            priority: 1.0,
            dependencies,
            can_defer: true,
            hydration_mode: HydrationMode::OnVisible,
        }
    }

    fn fixture_manifest_unsorted() -> RenderManifestV2 {
        RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-18T00:00:00Z".to_string(),
            components: vec![
                component(3, "src/routes/home.tsx", vec![1, 2]),
                component(1, "src/components/header.tsx", vec![]),
                component(2, "src/components/hero.tsx", vec![1]),
                component(9, "/repo/node_modules/react/index.js", vec![]),
            ],
            parallel_batches: vec![vec![1, 9], vec![2], vec![3]],
            critical_path: vec![1, 2, 3],
            vendor_chunks: vec![VendorChunk {
                chunk_name: "vendor.core".to_string(),
                packages: vec!["react".to_string()],
            }],
        }
    }

    #[test]
    fn test_build_bundle_plan_is_deterministic_for_unsorted_manifest() {
        let manifest = fixture_manifest_unsorted();
        let options = BundlePlanOptions::default();

        let first = build_bundle_plan(&manifest, &options);
        let second = build_bundle_plan(&manifest, &options);

        assert_eq!(first, second);
        assert_eq!(
            first
                .modules
                .iter()
                .map(|module| module.component_id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 9]
        );
    }

    #[test]
    fn test_build_bundle_plan_generates_wrapper_and_vendor_actions() {
        let manifest = fixture_manifest_unsorted();
        let plan = build_bundle_plan(&manifest, &BundlePlanOptions::default());

        assert!(plan.rewrite_actions.iter().any(|action| matches!(
            action,
            RewriteAction::WrapModule {
                component_id: 3,
                ..
            }
        )));
        assert!(plan.rewrite_actions.iter().any(|action| {
            matches!(
                action,
                RewriteAction::LinkVendorChunk {
                    component_id: 9,
                    chunk_name
                } if chunk_name == "vendor.core"
            )
        }));
    }
}
