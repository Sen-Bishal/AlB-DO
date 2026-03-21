use crate::manifest::schema::RenderManifestV2;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
pub struct VendorPlanOptions {
    pub infer_shared_vendor_chunks: bool,
    pub shared_dependency_min_components: usize,
}

impl Default for VendorPlanOptions {
    fn default() -> Self {
        Self {
            infer_shared_vendor_chunks: true,
            shared_dependency_min_components: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VendorChunkPlan {
    pub chunk_name: String,
    pub packages: Vec<String>,
    pub component_ids: Vec<u64>,
}

pub fn plan_vendor_chunks(
    manifest: &RenderManifestV2,
    options: &VendorPlanOptions,
) -> Vec<VendorChunkPlan> {
    let package_usage = build_package_usage_index(manifest);
    let mut normalized_chunks: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for chunk in &manifest.vendor_chunks {
        let packages: BTreeSet<String> = chunk
            .packages
            .iter()
            .map(|pkg| pkg.trim())
            .filter(|pkg| !pkg.is_empty())
            .map(str::to_string)
            .collect();

        if !packages.is_empty() {
            normalized_chunks.insert(chunk.chunk_name.clone(), packages);
        }
    }

    if options.infer_shared_vendor_chunks {
        let mut covered_packages = BTreeSet::new();
        for packages in normalized_chunks.values() {
            for package in packages {
                covered_packages.insert(package.clone());
            }
        }

        for (package, used_by_components) in &package_usage {
            if used_by_components.len() < options.shared_dependency_min_components {
                continue;
            }
            if covered_packages.contains(package) {
                continue;
            }

            let chunk_name = format!("vendor.{}", sanitize_chunk_label(package));
            normalized_chunks
                .entry(chunk_name)
                .or_default()
                .insert(package.clone());
        }
    }

    let mut planned = Vec::new();
    for (chunk_name, packages) in normalized_chunks {
        let component_ids = packages
            .iter()
            .filter_map(|package| package_usage.get(package))
            .flat_map(|ids| ids.iter().copied())
            .collect::<BTreeSet<u64>>()
            .into_iter()
            .collect::<Vec<u64>>();

        planned.push(VendorChunkPlan {
            chunk_name,
            packages: packages.into_iter().collect(),
            component_ids,
        });
    }

    planned
}

pub fn infer_package_name(module_path: &str) -> Option<String> {
    let normalized = module_path.replace('\\', "/");
    let needle = "/node_modules/";
    let (_, tail) = normalized.split_once(needle)?;

    if tail.is_empty() {
        return None;
    }

    let mut segments = tail.split('/');
    let first = segments.next()?;
    if first.is_empty() {
        return None;
    }

    if first.starts_with('@') {
        let second = segments.next()?;
        if second.is_empty() {
            return None;
        }
        return Some(format!("{first}/{second}"));
    }

    Some(first.to_string())
}

fn build_package_usage_index(manifest: &RenderManifestV2) -> BTreeMap<String, BTreeSet<u64>> {
    let mut usage: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();

    for component in &manifest.components {
        if let Some(package) = infer_package_name(&component.module_path) {
            usage.entry(package).or_default().insert(component.id);
        }
    }

    usage
}

fn sanitize_chunk_label(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier};

    fn component(id: u64, module_path: &str) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: format!("C{id}"),
            module_path: module_path.to_string(),
            tier: Tier::C,
            weight_bytes: 1024,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer: true,
            hydration_mode: HydrationMode::OnVisible,
        }
    }

    #[test]
    fn test_infer_package_name_for_scoped_dependency() {
        let package =
            infer_package_name("C:/repo/node_modules/@scope/ui-kit/dist/index.js").unwrap();
        assert_eq!(package, "@scope/ui-kit");
    }

    #[test]
    fn test_plan_vendor_chunks_infers_shared_chunk() {
        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-18T00:00:00Z".to_string(),
            components: vec![
                component(1, "/repo/node_modules/react/index.js"),
                component(2, "/repo/node_modules/react/index.js"),
                component(3, "/repo/src/routes/home.tsx"),
            ],
            parallel_batches: Vec::new(),
            critical_path: Vec::new(),
            vendor_chunks: Vec::new(),
        };

        let chunks = plan_vendor_chunks(&manifest, &VendorPlanOptions::default());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk_name, "vendor.react");
        assert_eq!(chunks[0].packages, vec!["react".to_string()]);
        assert_eq!(chunks[0].component_ids, vec![1, 2]);
    }
}
