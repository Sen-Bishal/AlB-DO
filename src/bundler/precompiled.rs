use crate::manifest::schema::{
    PrecompiledRuntimeModuleEntry, PrecompiledRuntimeModuleSkip, PrecompiledRuntimeModulesArtifact,
    RenderManifestV2,
};
use crate::runtime::engine::stable_source_hash;
use crate::runtime::quickjs_engine::compile_module_script_for_quickjs;
use std::collections::HashMap;

pub fn build_precompiled_runtime_modules_artifact(
    manifest: &RenderManifestV2,
    module_sources: &HashMap<String, String>,
) -> PrecompiledRuntimeModulesArtifact {
    let mut components = manifest.components.clone();
    components.sort_by_key(|component| component.id);

    let mut modules = Vec::new();
    let mut skipped = Vec::new();

    for component in components {
        let Some(source) = module_sources.get(&component.module_path) else {
            skipped.push(PrecompiledRuntimeModuleSkip {
                component_id: component.id,
                module_path: component.module_path.clone(),
                source_hash: 0,
                reason: "missing_source".to_string(),
            });
            continue;
        };

        let source_hash = stable_source_hash(source);
        match compile_module_script_for_quickjs(&component.module_path, source) {
            Ok(compiled_script) => modules.push(PrecompiledRuntimeModuleEntry {
                component_id: component.id,
                module_path: component.module_path.clone(),
                source_hash,
                compiled_script,
            }),
            Err(err) => skipped.push(PrecompiledRuntimeModuleSkip {
                component_id: component.id,
                module_path: component.module_path.clone(),
                source_hash,
                reason: err.to_string(),
            }),
        }
    }

    PrecompiledRuntimeModulesArtifact {
        version: PrecompiledRuntimeModulesArtifact::VERSION.to_string(),
        engine: PrecompiledRuntimeModulesArtifact::ENGINE_QUICKJS.to_string(),
        manifest_schema_version: manifest.schema_version.clone(),
        manifest_generated_at: manifest.generated_at.clone(),
        modules,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{ComponentManifestEntry, HydrationMode, Tier};

    fn component(id: u64, module_path: &str) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id,
            name: format!("C{id}"),
            module_path: module_path.to_string(),
            tier: Tier::A,
            weight_bytes: 1024,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer: false,
            hydration_mode: HydrationMode::None,
        }
    }

    #[test]
    fn test_build_precompiled_runtime_modules_artifact_captures_compiled_and_skipped() {
        let manifest = RenderManifestV2 {
            schema_version: "2.0".to_string(),
            generated_at: "2026-02-22T00:00:00Z".to_string(),
            components: vec![
                component(1, "components/ok"),
                component(2, "components/imported"),
                component(3, "components/missing"),
            ],
            parallel_batches: vec![vec![1, 2, 3]],
            critical_path: vec![1],
            vendor_chunks: Vec::new(),
            ..RenderManifestV2::legacy_defaults()
        };

        let mut sources = HashMap::new();
        sources.insert(
            "components/ok".to_string(),
            "export default function App(props){return '<main>'+props.name+'</main>';}".to_string(),
        );
        sources.insert(
            "components/imported".to_string(),
            "import x from 'pkg'; export default function App(){return String(x);}".to_string(),
        );

        let artifact = build_precompiled_runtime_modules_artifact(&manifest, &sources);
        assert_eq!(artifact.modules.len(), 2);
        assert_eq!(artifact.modules[0].component_id, 1);
        assert!(!artifact.modules[0].compiled_script.is_empty());
        assert!(artifact
            .modules
            .iter()
            .any(|module| module.module_path == "components/imported"));

        assert_eq!(artifact.skipped.len(), 1);
        assert!(artifact
            .skipped
            .iter()
            .any(|skip| skip.module_path == "components/missing"));
    }
}
