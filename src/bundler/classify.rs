use crate::manifest::schema::{ComponentManifestEntry, Tier};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BundleClass {
    Entry,
    Critical,
    Deferred,
}

pub fn classify_component(
    component: &ComponentManifestEntry,
    entry_component_id: Option<u64>,
    critical_ids: &BTreeSet<u64>,
) -> BundleClass {
    if entry_component_id == Some(component.id) {
        return BundleClass::Entry;
    }

    if critical_ids.contains(&component.id) {
        return BundleClass::Critical;
    }

    match component.tier {
        Tier::A => BundleClass::Critical,
        Tier::B => {
            if component.can_defer {
                BundleClass::Deferred
            } else {
                BundleClass::Critical
            }
        }
        Tier::C => BundleClass::Deferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::HydrationMode;

    fn fixture_component(id: u64, tier: Tier, can_defer: bool) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: format!("C{id}"),
            module_path: format!("components/c{id}.tsx"),
            tier,
            weight_bytes: 1024,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer,
            hydration_mode: HydrationMode::None,
        }
    }

    #[test]
    fn test_classify_component_prefers_entry() {
        let component = fixture_component(10, Tier::C, true);
        let mut critical_ids = BTreeSet::new();
        critical_ids.insert(10);
        assert_eq!(
            classify_component(&component, Some(10), &critical_ids),
            BundleClass::Entry
        );
    }

    #[test]
    fn test_classify_component_marks_tier_c_as_deferred() {
        let component = fixture_component(20, Tier::C, false);
        let critical_ids = BTreeSet::new();
        assert_eq!(
            classify_component(&component, None, &critical_ids),
            BundleClass::Deferred
        );
    }
}
