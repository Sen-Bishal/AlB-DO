pub mod payload;
pub mod plan;
pub mod script;

use crate::manifest::schema::RenderManifestV2;
use payload::{build_hydration_payload, serialize_hydration_payload, HydrationPayload};
use plan::{build_hydration_plan, HydrationPlan};
use script::{build_bootstrap_script_tag, build_payload_script_tag};

#[derive(Debug, Clone)]
pub struct HydrationArtifacts {
    pub plan: HydrationPlan,
    pub payload: HydrationPayload,
    pub payload_json: String,
    pub payload_script_tag: String,
    pub bootstrap_script_tag: String,
}

pub fn build_hydration_artifacts(
    manifest: &RenderManifestV2,
    entry: &str,
) -> Result<Option<HydrationArtifacts>, serde_json::Error> {
    let plan = build_hydration_plan(manifest, entry);
    if plan.islands.is_empty() {
        return Ok(None);
    }

    let payload = build_hydration_payload(manifest, &plan)?;
    let payload_json = serialize_hydration_payload(&payload)?;
    let payload_script_tag = build_payload_script_tag(
        &payload_json,
        payload.checksum.as_str(),
        payload.version.as_str(),
    );
    let bootstrap_script_tag =
        build_bootstrap_script_tag(payload.checksum.as_str(), payload.version.as_str());

    Ok(Some(HydrationArtifacts {
        plan,
        payload,
        payload_json,
        payload_script_tag,
        bootstrap_script_tag,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode, Tier};

    #[test]
    fn test_build_hydration_artifacts_returns_none_for_tier_a_only_entry() {
        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-17T00:00:00Z".to_string(),
            components: vec![ComponentManifestEntry {
                id: 1,
                name: "Entry".to_string(),
                module_path: "routes/entry".to_string(),
                tier: Tier::A,
                weight_bytes: 1024,
                priority: 1.0,
                dependencies: Vec::new(),
                can_defer: false,
                hydration_mode: HydrationMode::None,
            }],
            parallel_batches: vec![vec![1]],
            critical_path: vec![1],
            vendor_chunks: Vec::new(),
            ..RenderManifestV2::legacy_defaults()
        };

        let artifacts = build_hydration_artifacts(&manifest, "routes/entry").unwrap();
        assert!(artifacts.is_none());
    }
}
