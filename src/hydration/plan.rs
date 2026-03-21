use crate::manifest::schema::{ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const HYDRATION_PLAN_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HydrationTrigger {
    Idle,
    Visible,
    Interaction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HydrationIslandPlan {
    pub component_id: u64,
    pub module_path: String,
    pub trigger: HydrationTrigger,
    pub dependencies: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HydrationPlan {
    pub version: String,
    pub entry: String,
    pub islands: Vec<HydrationIslandPlan>,
}

pub fn build_hydration_plan(manifest: &RenderManifestV2, entry: &str) -> HydrationPlan {
    let component_index: BTreeMap<u64, &ComponentManifestEntry> = manifest
        .components
        .iter()
        .map(|component| (component.id, component))
        .collect();

    let mut islands = Vec::new();
    if let Some(entry_component) = manifest
        .components
        .iter()
        .find(|component| component.module_path == entry)
    {
        let reachable_ids = collect_reachable(entry_component.id, &component_index);
        for component_id in reachable_ids {
            if let Some(component) = component_index.get(&component_id) {
                if let Some(trigger) = trigger_from_tier(component.tier, component.hydration_mode) {
                    let mut dependencies = component.dependencies.clone();
                    dependencies.sort_unstable();

                    islands.push(HydrationIslandPlan {
                        component_id: component.id,
                        module_path: component.module_path.clone(),
                        trigger,
                        dependencies,
                    });
                }
            }
        }
    }

    HydrationPlan {
        version: HYDRATION_PLAN_VERSION.to_string(),
        entry: entry.to_string(),
        islands,
    }
}

fn collect_reachable(
    entry_id: u64,
    component_index: &BTreeMap<u64, &ComponentManifestEntry>,
) -> BTreeSet<u64> {
    let mut reachable = BTreeSet::new();
    let mut stack = vec![entry_id];

    while let Some(current_id) = stack.pop() {
        if !reachable.insert(current_id) {
            continue;
        }

        if let Some(component) = component_index.get(&current_id) {
            for dependency in component.dependencies.iter().rev() {
                stack.push(*dependency);
            }
        }
    }

    reachable
}

fn trigger_from_tier(tier: Tier, hydration_mode: HydrationMode) -> Option<HydrationTrigger> {
    match tier {
        Tier::A => None,
        Tier::B => Some(HydrationTrigger::Idle),
        Tier::C => {
            if hydration_mode == HydrationMode::OnInteraction {
                Some(HydrationTrigger::Interaction)
            } else {
                Some(HydrationTrigger::Visible)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier};

    fn fixture_manifest() -> RenderManifestV2 {
        RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-17T00:00:00Z".to_string(),
            components: vec![
                ComponentManifestEntry {
                    id: 1,
                    name: "A".to_string(),
                    module_path: "components/a".to_string(),
                    tier: Tier::A,
                    weight_bytes: 1024,
                    priority: 1.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: HydrationMode::None,
                },
                ComponentManifestEntry {
                    id: 2,
                    name: "B".to_string(),
                    module_path: "components/b".to_string(),
                    tier: Tier::B,
                    weight_bytes: 4096,
                    priority: 2.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: HydrationMode::OnIdle,
                },
                ComponentManifestEntry {
                    id: 3,
                    name: "C".to_string(),
                    module_path: "components/c".to_string(),
                    tier: Tier::C,
                    weight_bytes: 64000,
                    priority: 3.0,
                    dependencies: vec![],
                    can_defer: false,
                    hydration_mode: HydrationMode::OnVisible,
                },
                ComponentManifestEntry {
                    id: 4,
                    name: "Entry".to_string(),
                    module_path: "routes/entry".to_string(),
                    tier: Tier::C,
                    weight_bytes: 64000,
                    priority: 4.0,
                    dependencies: vec![1, 2, 3],
                    can_defer: false,
                    hydration_mode: HydrationMode::OnInteraction,
                },
            ],
            parallel_batches: vec![vec![1, 2], vec![3, 4]],
            critical_path: vec![4],
            vendor_chunks: Vec::new(),
        }
    }

    #[test]
    fn test_build_hydration_plan_filters_tier_a_and_maps_triggers() {
        let manifest = fixture_manifest();
        let plan = build_hydration_plan(&manifest, "routes/entry");

        assert_eq!(plan.version, HYDRATION_PLAN_VERSION);
        assert_eq!(plan.entry, "routes/entry");
        assert_eq!(plan.islands.len(), 3);

        assert_eq!(plan.islands[0].component_id, 2);
        assert_eq!(plan.islands[0].trigger, HydrationTrigger::Idle);

        assert_eq!(plan.islands[1].component_id, 3);
        assert_eq!(plan.islands[1].trigger, HydrationTrigger::Visible);

        assert_eq!(plan.islands[2].component_id, 4);
        assert_eq!(plan.islands[2].trigger, HydrationTrigger::Interaction);
    }

    #[test]
    fn test_build_hydration_plan_returns_empty_for_unknown_entry() {
        let manifest = fixture_manifest();
        let plan = build_hydration_plan(&manifest, "routes/missing");
        assert!(plan.islands.is_empty());
    }
}
