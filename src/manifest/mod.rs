pub mod builder;
pub mod schema;

use crate::effects::{decide_tier_and_hydration, TieringInputs};
use crate::graph::ComponentGraph;
use crate::types::{Component, ComponentId, OptimizationResult};
use builder::{ComponentTierMetadata, ManifestBuilder};
use schema::{ComponentManifestEntry, HydrationMode, RenderManifestV2};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ManifestOptions {
    pub tier_a_inline_max_bytes: u64,
    pub tier_c_split_min_bytes: u64,
    pub tier_b_mode: HydrationMode,
    pub tier_c_mode: HydrationMode,
    pub tier_b_timeout_ms: u64,
}

impl Default for ManifestOptions {
    fn default() -> Self {
        Self {
            tier_a_inline_max_bytes: 8 * 1024,
            tier_c_split_min_bytes: 40 * 1024,
            tier_b_mode: HydrationMode::OnIdle,
            tier_c_mode: HydrationMode::OnVisible,
            tier_b_timeout_ms: 2000,
        }
    }
}

pub fn build_render_manifest_v2(
    graph: &ComponentGraph,
    result: &OptimizationResult,
    options: &ManifestOptions,
) -> RenderManifestV2 {
    let critical_index = build_critical_path_index(result);
    let batch_index = build_batch_index(result);

    let mut components: Vec<Component> = graph.components();
    components.sort_by_key(|component| component.id.as_u64());

    let mut tier_metadata: HashMap<ComponentId, ComponentTierMetadata> = HashMap::new();
    let mut component_entries: Vec<ComponentManifestEntry> = components
        .iter()
        .map(|component| {
            let weight_bytes = component.weight.max(0.0).round() as u64;
            let decision = decide_tier_and_hydration(
                component.effect_profile,
                component.is_interactive,
                component.is_above_fold,
                weight_bytes,
                tiering_inputs_from_options(options),
            );
            tier_metadata.insert(
                component.id,
                ComponentTierMetadata {
                    tier: decision.tier,
                    hydration_mode: decision.hydration_mode,
                    effect_profile: component.effect_profile,
                },
            );

            let mut dependencies: Vec<u64> = graph
                .get_dependencies(&component.id)
                .into_iter()
                .map(|id| id.as_u64())
                .collect();
            dependencies.sort_unstable();

            ComponentManifestEntry {
                id: component.id.as_u64(),
                name: component.name.clone(),
                module_path: component.file_path.clone(),
                tier: decision.tier,
                weight_bytes,
                priority: compute_priority(component, &critical_index, &batch_index),
                dependencies,
                can_defer: !component.is_above_fold && !component.is_lcp_candidate,
                hydration_mode: decision.hydration_mode,
            }
        })
        .collect();
    component_entries.sort_by_key(|entry| entry.id);

    let mut batches = result.parallel_batches.clone();
    batches.sort_by_key(|batch| batch.level);
    let parallel_batches: Vec<Vec<u64>> = batches
        .into_iter()
        .map(|batch| {
            let mut ids: Vec<u64> = batch.components.into_iter().map(|id| id.as_u64()).collect();
            ids.sort_unstable();
            ids
        })
        .collect();

    let critical_path = result
        .critical_path
        .iter()
        .map(|id| id.as_u64())
        .collect::<Vec<u64>>();

    let manifest_builder = ManifestBuilder::new(graph, tier_metadata, options.tier_b_timeout_ms);
    let assets = manifest_builder.build_assets_manifest();
    let build_id = manifest_builder.build_build_id();
    let wt_streams = manifest_builder.build_wt_stream_slots();
    let mut routes = HashMap::new();

    for (route_path, root_component) in entry_components_for_routes(graph, result) {
        let route =
            manifest_builder.build_route_manifest(route_path.as_str(), root_component, &assets);
        routes.insert(route.route.clone(), route);
    }

    if routes.is_empty() {
        if let Some(root_component) = entry_component_for_route(graph, result) {
            let route = manifest_builder.build_route_manifest("/", root_component, &assets);
            routes.insert(route.route.clone(), route);
        }
    }

    RenderManifestV2 {
        version: RenderManifestV2::VERSION,
        build_id,
        routes,
        assets,
        schema_version: RenderManifestV2::SCHEMA_VERSION.to_string(),
        generated_at: result.generated_at.clone(),
        components: component_entries,
        parallel_batches,
        critical_path,
        vendor_chunks: Vec::new(),
        wt_streams,
    }
}

fn build_critical_path_index(result: &OptimizationResult) -> HashMap<u64, usize> {
    result
        .critical_path
        .iter()
        .enumerate()
        .map(|(idx, id)| (id.as_u64(), idx))
        .collect()
}

fn build_batch_index(result: &OptimizationResult) -> HashMap<u64, usize> {
    let mut map = HashMap::new();
    for (batch_idx, batch) in result.parallel_batches.iter().enumerate() {
        for id in &batch.components {
            map.entry(id.as_u64()).or_insert(batch_idx);
        }
    }
    map
}

fn tiering_inputs_from_options(options: &ManifestOptions) -> TieringInputs {
    TieringInputs {
        tier_a_inline_max_bytes: options.tier_a_inline_max_bytes,
        tier_c_split_min_bytes: options.tier_c_split_min_bytes,
        tier_b_mode: options.tier_b_mode,
        tier_c_mode: options.tier_c_mode,
    }
}

fn entry_component_for_route(
    graph: &ComponentGraph,
    result: &OptimizationResult,
) -> Option<ComponentId> {
    result.critical_path.last().copied().or_else(|| {
        let mut ids = graph.component_ids();
        ids.sort_unstable_by_key(|id| id.as_u64());
        ids.first().copied()
    })
}

fn entry_components_for_routes(
    graph: &ComponentGraph,
    result: &OptimizationResult,
) -> Vec<(String, ComponentId)> {
    let mut route_map: BTreeMap<String, ComponentId> = BTreeMap::new();

    let mut component_ids = graph.component_ids();
    component_ids.sort_unstable_by_key(|id| id.as_u64());

    for id in component_ids {
        if !graph.get_dependents(&id).is_empty() {
            continue;
        }

        let Some(component) = graph.get(&id) else {
            continue;
        };
        let Some(route_path) = route_path_from_component(component.file_path.as_str()) else {
            continue;
        };

        route_map.entry(route_path).or_insert(id);
    }

    if route_map.is_empty() {
        if let Some(entry) = entry_component_for_route(graph, result) {
            route_map.insert("/".to_string(), entry);
        }
    }

    route_map.into_iter().collect()
}

fn route_path_from_component(file_path: &str) -> Option<String> {
    let normalized = file_path.replace('\\', "/");
    let route_hint = normalized
        .split_once("/routes/")
        .map(|(_, tail)| tail.to_string())
        .or_else(|| normalized.strip_prefix("routes/").map(str::to_string))?;

    let mut route = Path::new(route_hint.as_str())
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    route = route.trim_matches('/').to_string();

    if route.ends_with("/index") {
        route = route
            .trim_end_matches("/index")
            .trim_matches('/')
            .to_string();
    }

    if route.is_empty() || route == "index" || route == "home" || route == "app" {
        return Some("/".to_string());
    }

    Some(format!("/{}", route))
}

fn compute_priority(
    component: &Component,
    critical_index: &HashMap<u64, usize>,
    batch_index: &HashMap<u64, usize>,
) -> f64 {
    let id = component.id.as_u64();
    let critical_score = critical_index
        .get(&id)
        .map(|idx| 1000.0 - (*idx as f64))
        .unwrap_or(0.0);
    let batch_score = batch_index
        .get(&id)
        .map(|idx| 100.0 - (*idx as f64))
        .unwrap_or(0.0);
    let fold_bonus = if component.is_above_fold { 20.0 } else { 0.0 };
    let interaction_bonus = if component.is_interactive { 10.0 } else { 0.0 };

    critical_score + batch_score + fold_bonus + interaction_bonus
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::Tier;
    use crate::types::{Component, ComponentId};
    use crate::RenderCompiler;

    #[test]
    fn test_build_render_manifest_v2_shape() {
        let mut compiler = RenderCompiler::new();

        let mut app = Component::new(ComponentId::new(0), "App".to_string());
        app.weight = 4096.0;
        app.file_path = "src/App.tsx".to_string();
        app.is_above_fold = true;

        let mut widget = Component::new(ComponentId::new(0), "Widget".to_string());
        widget.weight = 65536.0;
        widget.file_path = "src/Widget.tsx".to_string();
        widget.is_interactive = true;

        let app_id = compiler.add_component(app);
        let widget_id = compiler.add_component(widget);
        compiler.add_dependency(app_id, widget_id).unwrap();

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        assert_eq!(manifest.schema_version, "2.0");
        assert_eq!(manifest.components.len(), 2);
        assert_eq!(
            manifest.parallel_batches.len(),
            result.parallel_batches.len()
        );
        assert_eq!(manifest.critical_path.len(), result.critical_path.len());
        assert!(manifest
            .components
            .iter()
            .any(|entry| entry.tier == Tier::A));
        assert!(manifest
            .components
            .iter()
            .any(|entry| entry.tier == Tier::C));
        assert_eq!(manifest.wt_streams.len(), 2);
        assert!(manifest
            .wt_streams
            .iter()
            .all(|slot| slot.slot == 1 || slot.slot == 2));
    }

    #[test]
    fn test_effect_contract_promotes_hook_component_out_of_tier_a() {
        let mut compiler = RenderCompiler::new();

        let mut hook_component = Component::new(ComponentId::new(0), "HookWidget".to_string());
        hook_component.file_path = "src/HookWidget.tsx".to_string();
        hook_component.weight = 1024.0;
        hook_component.effect_profile.hooks = true;

        compiler.add_component(hook_component);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let entry = manifest
            .components
            .iter()
            .find(|component| component.name == "HookWidget")
            .expect("hook component should exist");

        assert_eq!(entry.tier, Tier::B);
        assert_eq!(entry.hydration_mode, HydrationMode::OnIdle);

        let shell_slot = manifest
            .wt_streams
            .iter()
            .find(|slot| slot.slot == 1)
            .expect("shell slot should exist");
        let patch_slot = manifest
            .wt_streams
            .iter()
            .find(|slot| slot.slot == 2)
            .expect("patch slot should exist");

        assert_eq!(shell_slot.component_ids, vec![entry.id]);
        assert_eq!(patch_slot.component_ids, vec![entry.id]);
    }

    #[test]
    fn test_build_render_manifest_v2_registers_multiple_routes() {
        let mut compiler = RenderCompiler::new();

        let mut home = Component::new(ComponentId::new(0), "Home".to_string());
        home.file_path = "src/routes/home.tsx".to_string();
        home.is_above_fold = true;
        home.weight = 1024.0;

        let mut about = Component::new(ComponentId::new(0), "About".to_string());
        about.file_path = "src/routes/about.tsx".to_string();
        about.is_above_fold = true;
        about.weight = 1024.0;

        compiler.add_component(home);
        compiler.add_component(about);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        assert!(manifest.routes.contains_key("/"));
        assert!(manifest.routes.contains_key("/about"));
    }
}
