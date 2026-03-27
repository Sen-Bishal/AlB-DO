use super::plan::{HydrationPlan, HydrationTrigger};
use crate::manifest::schema::RenderManifestV2;
use serde::{Deserialize, Serialize};

pub const HYDRATION_PAYLOAD_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HydrationIslandPayload {
    pub component_id: u64,
    pub module_path: String,
    pub trigger: HydrationTrigger,
    pub dependencies: Vec<u64>,
}
//This is a nvim setup test!
//I think it works.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HydrationPayload {
    pub version: String,
    pub checksum: String,
    pub route_entry: String,
    pub manifest_schema_version: String,
    pub manifest_generated_at: String,
    pub islands: Vec<HydrationIslandPayload>,
}

pub fn build_hydration_payload(
    manifest: &RenderManifestV2,
    plan: &HydrationPlan,
) -> Result<HydrationPayload, serde_json::Error> {
    let islands = plan
        .islands
        .iter()
        .map(|island| HydrationIslandPayload {
            component_id: island.component_id,
            module_path: island.module_path.clone(),
            trigger: island.trigger,
            dependencies: island.dependencies.clone(),
        })
        .collect();

    let mut payload = HydrationPayload {
        version: HYDRATION_PAYLOAD_VERSION.to_string(),
        checksum: String::new(),
        route_entry: plan.entry.clone(),
        manifest_schema_version: manifest.schema_version.clone(),
        manifest_generated_at: manifest.generated_at.clone(),
        islands,
    };
    payload.checksum = compute_payload_checksum(&payload)?;
    Ok(payload)
}

pub fn serialize_hydration_payload(
    payload: &HydrationPayload,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(payload)
}

pub fn payload_checksum_matches(payload: &HydrationPayload) -> Result<bool, serde_json::Error> {
    Ok(compute_payload_checksum(payload)? == payload.checksum)
}

fn compute_payload_checksum(payload: &HydrationPayload) -> Result<String, serde_json::Error> {
    #[derive(Serialize)]
    struct ChecksumBasis<'a> {
        version: &'a str,
        route_entry: &'a str,
        manifest_schema_version: &'a str,
        manifest_generated_at: &'a str,
        islands: &'a [HydrationIslandPayload],
    }

    let basis = ChecksumBasis {
        version: payload.version.as_str(),
        route_entry: payload.route_entry.as_str(),
        manifest_schema_version: payload.manifest_schema_version.as_str(),
        manifest_generated_at: payload.manifest_generated_at.as_str(),
        islands: payload.islands.as_slice(),
    };
    let serialized = serde_json::to_string(&basis)?;
    Ok(fnv1a_64_hex(serialized.as_bytes()))
}

fn fnv1a_64_hex(input: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }

    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydration::plan::{
        HydrationIslandPlan, HydrationPlan, HydrationTrigger, HYDRATION_PLAN_VERSION,
    };
    use crate::manifest::schema::RenderManifestV2;

    fn manifest() -> RenderManifestV2 {
        RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-17T00:00:00Z".to_string(),
            components: Vec::new(),
            parallel_batches: Vec::new(),
            critical_path: Vec::new(),
            vendor_chunks: Vec::new(),
            ..RenderManifestV2::legacy_defaults()
        }
    }

    #[test]
    fn test_build_hydration_payload_sets_checksum() {
        let plan = HydrationPlan {
            version: HYDRATION_PLAN_VERSION.to_string(),
            entry: "routes/home".to_string(),
            islands: vec![HydrationIslandPlan {
                component_id: 11,
                module_path: "routes/home".to_string(),
                trigger: HydrationTrigger::Idle,
                dependencies: vec![1],
            }],
        };

        let payload = build_hydration_payload(&manifest(), &plan).unwrap();
        assert!(!payload.checksum.is_empty());
        assert!(payload_checksum_matches(&payload).unwrap());
    }

    #[test]
    fn test_checksum_changes_when_payload_changes() {
        let plan_a = HydrationPlan {
            version: HYDRATION_PLAN_VERSION.to_string(),
            entry: "routes/home".to_string(),
            islands: vec![HydrationIslandPlan {
                component_id: 11,
                module_path: "routes/home".to_string(),
                trigger: HydrationTrigger::Idle,
                dependencies: vec![1],
            }],
        };
        let plan_b = HydrationPlan {
            version: HYDRATION_PLAN_VERSION.to_string(),
            entry: "routes/home".to_string(),
            islands: vec![HydrationIslandPlan {
                component_id: 12,
                module_path: "routes/home".to_string(),
                trigger: HydrationTrigger::Visible,
                dependencies: vec![1],
            }],
        };

        let payload_a = build_hydration_payload(&manifest(), &plan_a).unwrap();
        let payload_b = build_hydration_payload(&manifest(), &plan_b).unwrap();
        assert_ne!(payload_a.checksum, payload_b.checksum);
    }
}
