use crate::manifest::schema::{
    RenderManifestV2, StaticSliceArtifactEntry, StaticSliceArtifactManifest, Tier,
};
use crate::runtime::engine::stable_source_hash;
use std::collections::HashMap;

const STATIC_SLICE_HOOK_MARKERS: &[&str] = &[
    "useState(",
    "useEffect(",
    "useLayoutEffect(",
    "useReducer(",
    "useMemo(",
    "useCallback(",
    "useRef(",
    "useContext(",
    "useTransition(",
    "useDeferredValue(",
    "useSyncExternalStore(",
    "useOptimistic(",
    "useActionState(",
];

const STATIC_SLICE_ASYNC_DATA_MARKERS: &[&str] = &[
    "async function",
    "await ",
    "fetch(",
    "Promise.",
    ".then(",
    ".catch(",
];

const STATIC_SLICE_NON_DETERMINISTIC_MARKERS: &[&str] =
    &["Date.now(", "new Date(", "Math.random(", "performance.now("];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticSliceEligibilityKind {
    Eligible,
    NotTierA,
    MissingSource,
    UsesHooks,
    UsesAsyncData,
    NonDeterministic,
}

impl StaticSliceEligibilityKind {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Eligible => None,
            Self::NotTierA => Some("component_tier_is_not_a"),
            Self::MissingSource => Some("missing_source"),
            Self::UsesHooks => Some("hooks_detected"),
            Self::UsesAsyncData => Some("async_data_detected"),
            Self::NonDeterministic => Some("nondeterministic_source_detected"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticSliceEligibility {
    pub kind: StaticSliceEligibilityKind,
    pub source_hash: u64,
}

pub fn evaluate_static_slice_eligibility(source: &str) -> StaticSliceEligibility {
    let source_hash = stable_source_hash(source);
    let trimmed = source.trim();

    if trimmed.is_empty() {
        return StaticSliceEligibility {
            kind: StaticSliceEligibilityKind::MissingSource,
            source_hash,
        };
    }

    if contains_any(trimmed, STATIC_SLICE_HOOK_MARKERS) {
        return StaticSliceEligibility {
            kind: StaticSliceEligibilityKind::UsesHooks,
            source_hash,
        };
    }

    if contains_any(trimmed, STATIC_SLICE_ASYNC_DATA_MARKERS) {
        return StaticSliceEligibility {
            kind: StaticSliceEligibilityKind::UsesAsyncData,
            source_hash,
        };
    }

    if contains_any(trimmed, STATIC_SLICE_NON_DETERMINISTIC_MARKERS) {
        return StaticSliceEligibility {
            kind: StaticSliceEligibilityKind::NonDeterministic,
            source_hash,
        };
    }

    StaticSliceEligibility {
        kind: StaticSliceEligibilityKind::Eligible,
        source_hash,
    }
}

pub fn build_static_slice_manifest(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
) -> StaticSliceArtifactManifest {
    let mut components = manifest.components.clone();
    components.sort_by_key(|component| component.id);

    let mut slices = Vec::new();
    for component in components {
        let (eligible, source_hash, reason) = if component.tier != Tier::A {
            (
                false,
                0_u64,
                StaticSliceEligibilityKind::NotTierA
                    .reason()
                    .map(str::to_string),
            )
        } else if let Some(source) = module_sources.get(&component.module_path) {
            let result = evaluate_static_slice_eligibility(source);
            let eligible = result.kind == StaticSliceEligibilityKind::Eligible;
            (
                eligible,
                result.source_hash,
                result.kind.reason().map(str::to_string),
            )
        } else {
            (
                false,
                0_u64,
                StaticSliceEligibilityKind::MissingSource
                    .reason()
                    .map(str::to_string),
            )
        };

        slices.push(StaticSliceArtifactEntry {
            component_id: component.id,
            module_path: component.module_path,
            source_hash,
            eligible,
            ineligibility_reason: reason,
        });
    }

    StaticSliceArtifactManifest {
        version: StaticSliceArtifactManifest::VERSION.to_string(),
        manifest_schema_version: manifest.schema_version.clone(),
        manifest_generated_at: manifest.generated_at.clone(),
        entry_component_id: manifest.critical_path.last().copied(),
        slices,
    }
}

fn contains_any(source: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| source.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode};

    fn component(id: u64, module_path: &str, tier: Tier) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: format!("C{id}"),
            module_path: module_path.to_string(),
            tier,
            weight_bytes: 1024,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer: false,
            hydration_mode: HydrationMode::None,
        }
    }

    #[test]
    fn test_static_slice_eligibility_rejects_hooks_async_and_nondeterminism() {
        let hook = evaluate_static_slice_eligibility(
            "export default function App() { const [v] = useState(1); return '<p>'+v+'</p>'; }",
        );
        assert_eq!(hook.kind, StaticSliceEligibilityKind::UsesHooks);

        let async_data = evaluate_static_slice_eligibility(
            "export default async function App() { const x = await fetch('/api'); return String(x); }",
        );
        assert_eq!(async_data.kind, StaticSliceEligibilityKind::UsesAsyncData);

        let nondeterministic = evaluate_static_slice_eligibility(
            "export default function App() { return String(Date.now()); }",
        );
        assert_eq!(
            nondeterministic.kind,
            StaticSliceEligibilityKind::NonDeterministic
        );
    }

    #[test]
    fn test_build_static_slice_manifest_filters_to_tier_a_eligibility() {
        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-22T00:00:00Z".to_string(),
            components: vec![
                component(1, "components/eligible", Tier::A),
                component(2, "components/non-tier-a", Tier::B),
                component(3, "components/hook", Tier::A),
            ],
            parallel_batches: vec![vec![1, 2, 3]],
            critical_path: vec![1],
            vendor_chunks: Vec::new(),
        };

        let mut sources = HashMap::new();
        sources.insert(
            "components/eligible".to_string(),
            "export default function App(props){return '<main>'+props.title+'</main>';}"
                .to_string(),
        );
        sources.insert(
            "components/hook".to_string(),
            "export default function Hooky(){const [v]=useState(0);return String(v);}".to_string(),
        );

        let static_manifest = build_static_slice_manifest(&manifest, &sources);
        assert_eq!(static_manifest.slices.len(), 3);

        let eligible = static_manifest
            .slices
            .iter()
            .find(|slice| slice.component_id == 1)
            .unwrap();
        assert!(eligible.eligible);
        assert!(eligible.ineligibility_reason.is_none());
        assert_ne!(eligible.source_hash, 0);

        let non_tier_a = static_manifest
            .slices
            .iter()
            .find(|slice| slice.component_id == 2)
            .unwrap();
        assert!(!non_tier_a.eligible);
        assert_eq!(
            non_tier_a.ineligibility_reason.as_deref(),
            Some("component_tier_is_not_a")
        );

        let hook = static_manifest
            .slices
            .iter()
            .find(|slice| slice.component_id == 3)
            .unwrap();
        assert!(!hook.eligible);
        assert_eq!(hook.ineligibility_reason.as_deref(), Some("hooks_detected"));
    }
}
