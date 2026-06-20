pub mod builder;
pub mod metadata;
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
                component.is_client_interactive,
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

    // Phase P · Stream E.1 — `layout.tsx` (and, ahead of Stream E.2,
    // `error.tsx` / `loading.tsx`) are convention files, not routes.
    // They live in `<routes>/` so the manifest builder's
    // `render_layout_html` can resolve them by component name, but
    // they MUST NOT become route entries — otherwise a phantom
    // `/layout` URL appears alongside the real routes. Filter by
    // the basename of the source file (with extension stripped).
    let stripped = Path::new(route_hint.as_str())
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    let last_segment = stripped.rsplit('/').next().unwrap_or("");
    if matches!(last_segment, "layout" | "error" | "loading") {
        return None;
    }

    let mut route = stripped;
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

    // ── Phase P · Stream B tests ────────────────────────────────────

    /// Phase P · build a manifest from a real on-disk Tier-B component
    /// (Counter with useState) and confirm the manifest carries real
    /// HTML + opcodes, not a placeholder. This is the gate test for
    /// Stream B.
    #[test]
    fn stream_b_tier_b_node_carries_real_html_and_opcode_frame() {
        let mut compiler = RenderCompiler::new();
        let mut counter = Component::new(ComponentId::new(0), "Counter".to_string());
        // The hook-compile fixture under `tests/fixtures/hook_compile/counter/`
        // ships a useState component the manifest builder can render
        // through Phase K end-to-end.
        counter.file_path = "tests/fixtures/hook_compile/counter/Component.tsx".to_string();
        counter.weight = 2048.0;
        counter.effect_profile.hooks = true;
        compiler.add_component(counter);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        // Counter compiles to Tier-B because of useState.
        let counter_entry = manifest
            .components
            .iter()
            .find(|c| c.name == "Counter")
            .expect("Counter present");
        assert_eq!(counter_entry.tier, Tier::B);

        // The route's Tier-B list must carry the pre-rendered HTML
        // and a non-empty opcode frame (BindEvent + SetTextRef).
        let route = manifest.routes.values().next().expect("at least one route");
        let tier_b = route
            .tier_b
            .iter()
            .find(|n| n.component_id == "Counter")
            .expect("Counter is in tier_b");
        let html = tier_b
            .initial_html
            .as_deref()
            .expect("Stream B fills initial_html for compilable Tier-B nodes");
        assert!(
            html.contains("<button"),
            "expected rendered <button>; got: {html}"
        );
        assert!(
            html.contains(">0</button>"),
            "expected initial useState value '0' inline; got: {html}"
        );
        assert!(
            !tier_b.initial_opcode_frame.is_empty(),
            "expected non-empty opcode frame for hook-compiled Counter"
        );
    }

    /// Phase P · the embedded opcode frame must round-trip through
    /// `decode_frame` and contain the expected BindEvent + SetTextRef
    /// opcodes Phase K emits for a useState counter.
    #[test]
    fn stream_b_tier_b_opcode_frame_round_trips_and_carries_phase_k_opcodes() {
        use crate::ir::opcode::Instruction;
        use crate::ir::wire::decode_frame;

        let mut compiler = RenderCompiler::new();
        let mut counter = Component::new(ComponentId::new(0), "Counter".to_string());
        counter.file_path = "tests/fixtures/hook_compile/counter/Component.tsx".to_string();
        counter.weight = 2048.0;
        counter.effect_profile.hooks = true;
        compiler.add_component(counter);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let tier_b = manifest
            .routes
            .values()
            .next()
            .unwrap()
            .tier_b
            .iter()
            .find(|n| n.component_id == "Counter")
            .unwrap();

        let (frame, _) = decode_frame(&tier_b.initial_opcode_frame)
            .expect("frame round-trips through decode_frame");
        let has_bind_event = frame
            .instructions
            .iter()
            .any(|op| matches!(op, Instruction::BindEvent { .. }));
        let has_set_text_ref = frame
            .instructions
            .iter()
            .any(|op| matches!(op, Instruction::SetTextRef { .. }));
        assert!(
            has_bind_event,
            "expected BindEvent in counter opcode frame; got {:?}",
            frame.instructions
        );
        assert!(
            has_set_text_ref,
            "expected SetTextRef in counter opcode frame; got {:?}",
            frame.instructions
        );
    }

    /// Phase P · Tier-A components remain unaffected. The new fields
    /// default to None/empty for Tier-A routes (Tier-A renders to HTML
    /// inline, no opcode payload needed).
    #[test]
    fn stream_b_tier_a_only_route_has_empty_initial_opcode_metadata() {
        let mut compiler = RenderCompiler::new();
        let mut static_hero = Component::new(ComponentId::new(0), "Hero".to_string());
        static_hero.file_path = "src/Hero.tsx".to_string();
        static_hero.is_above_fold = true;
        static_hero.weight = 512.0;
        compiler.add_component(static_hero);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().expect("route exists");
        assert!(
            route.tier_b.is_empty(),
            "Tier-A only route should not produce Tier-B nodes"
        );
        // Schema defaults: empty per-route metadata for projects
        // without file-based routes or TS actions.
        assert!(route.shared_slot_topics.is_empty());
        assert!(route.action_ids.is_empty());
        assert!(route.layout_chain.is_empty());
        assert!(route.error_component.is_none());
        assert!(route.loading_component.is_none());
    }

    /// Phase P · falling back gracefully when the component file
    /// doesn't exist on disk. The pre-render path returns None and
    /// the manifest stays usable (fallback_html still populated).
    #[test]
    fn stream_b_falls_back_to_placeholder_when_source_is_missing() {
        let mut compiler = RenderCompiler::new();
        let mut ghost = Component::new(ComponentId::new(0), "Ghost".to_string());
        ghost.file_path = "src/nonexistent/Ghost.tsx".to_string();
        ghost.weight = 1024.0;
        ghost.effect_profile.hooks = true;
        compiler.add_component(ghost);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().unwrap();
        let tier_b = route
            .tier_b
            .iter()
            .find(|n| n.component_id == "Ghost")
            .expect("Ghost still emitted as Tier-B placeholder");
        // No disk source → no inline HTML, no opcodes. But the
        // placeholder fallback still ships so the streaming handler
        // has SOMETHING to render.
        assert!(tier_b.initial_html.is_none());
        assert!(tier_b.initial_opcode_frame.is_empty());
        assert!(tier_b.fallback_html.is_some());
    }

    /// Phase P · two consecutive manifest builds against identical
    /// inputs produce byte-identical opcode frames. Determinism is
    /// load-bearing for the budget gate + cache invalidation.
    #[test]
    fn stream_b_manifest_build_is_deterministic_across_runs() {
        let build = || {
            let mut compiler = RenderCompiler::new();
            let mut counter = Component::new(ComponentId::new(0), "Counter".to_string());
            counter.file_path = "tests/fixtures/hook_compile/counter/Component.tsx".to_string();
            counter.weight = 2048.0;
            counter.effect_profile.hooks = true;
            compiler.add_component(counter);
            let result = compiler.optimize().unwrap();
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default())
        };

        let first = build();
        let second = build();

        let first_tier_b = &first.routes.values().next().unwrap().tier_b[0];
        let second_tier_b = &second.routes.values().next().unwrap().tier_b[0];
        assert_eq!(first_tier_b.initial_html, second_tier_b.initial_html);
        assert_eq!(
            first_tier_b.initial_opcode_frame, second_tier_b.initial_opcode_frame,
            "opcode frame bytes must match across rebuilds"
        );
    }

    /// Phase P · the shell's body_open should reference the Tier-B
    /// placeholder by its stable id so the streaming handler can
    /// slot the rendered HTML in. The pre-render machinery is
    /// orthogonal to placeholder generation; this confirms we didn't
    /// regress the existing wire shape.
    #[test]
    fn stream_b_shell_still_anchors_tier_b_placeholders() {
        let mut compiler = RenderCompiler::new();
        let mut counter = Component::new(ComponentId::new(0), "Counter".to_string());
        counter.file_path = "tests/fixtures/hook_compile/counter/Component.tsx".to_string();
        counter.weight = 2048.0;
        counter.effect_profile.hooks = true;
        compiler.add_component(counter);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().unwrap();
        let placeholder = &route.tier_b[0].placeholder_id;
        assert!(
            route.shell.body_open.contains(placeholder),
            "shell.body_open should anchor the Tier-B placeholder id; \
             got body_open='{}', placeholder='{}'",
            route.shell.body_open,
            placeholder
        );
    }

    /// Gate 2 · B (slice 1) — a route that exports `const metadata`
    /// drives the shell `<head>`: the authored title replaces the
    /// `ALBEDO {route}` fallback and description / OG / twitter tags are
    /// emitted. The resolved metadata also rides the `RouteManifest`.
    #[test]
    fn shell_head_reflects_static_metadata_export() {
        let mut compiler = RenderCompiler::new();
        let mut page = Component::new(ComponentId::new(0), "Page".to_string());
        page.file_path = "tests/fixtures/metadata/Page.tsx".to_string();
        page.weight = 1024.0;
        compiler.add_component(page);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().unwrap();
        let head = &route.shell.doctype_and_head;

        assert!(
            head.contains("<title>Home — ALBEDO</title>"),
            "author title must replace the fallback; head={head}"
        );
        assert!(
            !head.contains("<title>ALBEDO "),
            "the ALBEDO fallback title must not also appear; head={head}"
        );
        assert!(
            head.contains("<meta name=\"description\" content=\"The fastest way to ship.\">"),
            "head={head}"
        );
        assert!(
            head.contains("property=\"og:title\" content=\"Home OG\""),
            "head={head}"
        );
        assert!(
            head.contains("name=\"twitter:card\" content=\"summary_large_image\""),
            "head={head}"
        );

        // The structured metadata is also carried on the manifest for
        // consumers other than the shell string.
        assert_eq!(route.metadata.title.as_deref(), Some("Home — ALBEDO"));
        assert_eq!(
            route.metadata.description.as_deref(),
            Some("The fastest way to ship.")
        );
    }

    /// Gate 2 · B (slice 2) — JSX-rendered `<title>`/`<meta>` are
    /// hoisted out of the body into the shell `<head>` (React-19 style)
    /// and override the static `export const metadata` base (JSX wins).
    #[test]
    fn shell_head_hoists_jsx_title_and_meta_over_static() {
        let mut compiler = RenderCompiler::new();
        let mut page = Component::new(ComponentId::new(0), "Page".to_string());
        page.file_path = "tests/fixtures/metadata/JsxHead.tsx".to_string();
        page.weight = 1024.0;
        compiler.add_component(page);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().unwrap();
        let head = &route.shell.doctype_and_head;
        // The rendered component HTML (where the JSX head tags lived) is
        // carried on the tier-A node and injected into the body at serve
        // time; the slice-2 hoist strips the head tags from it in place.
        let node_html = route
            .tier_a_root
            .iter()
            .map(|n| n.html.as_str())
            .find(|html| html.contains("Body content"))
            .expect("rendered page HTML should be on a tier-A node");

        // JSX <title> overrode the static "Static Title".
        assert!(
            head.contains("<title>JSX Title Wins</title>"),
            "JSX title must win; head={head}"
        );
        assert!(
            !head.contains("Static Title"),
            "static title must be overridden; head={head}"
        );
        // JSX <meta> hoisted into the head.
        assert!(
            head.contains("property=\"og:title\" content=\"JSX OG Title\""),
            "head={head}"
        );
        // Static description (not overridden by JSX) survives.
        assert!(
            head.contains("<meta name=\"description\" content=\"static description\">"),
            "head={head}"
        );
        // The hoisted tags are stripped from the rendered body HTML; real
        // content stays.
        assert!(
            !node_html.contains("<title"),
            "title stripped from body; node={node_html}"
        );
        assert!(
            !node_html.contains("og:title"),
            "meta stripped from body; node={node_html}"
        );
        assert!(
            node_html.contains("Body content"),
            "body retains real content; node={node_html}"
        );

        // The resolved metadata on the manifest reflects the merge.
        assert_eq!(route.metadata.title.as_deref(), Some("JSX Title Wins"));
    }

    /// A route with no `metadata` export keeps the historical
    /// `ALBEDO {route}` fallback title and emits no extra head tags —
    /// proving the feature is opt-in and byte-compatible for untouched
    /// routes.
    #[test]
    fn shell_head_keeps_fallback_without_metadata_export() {
        let mut compiler = RenderCompiler::new();
        let mut counter = Component::new(ComponentId::new(0), "Counter".to_string());
        counter.file_path = "tests/fixtures/hook_compile/counter/Component.tsx".to_string();
        counter.weight = 2048.0;
        counter.effect_profile.hooks = true;
        compiler.add_component(counter);

        let result = compiler.optimize().unwrap();
        let manifest =
            build_render_manifest_v2(compiler.graph(), &result, &ManifestOptions::default());

        let route = manifest.routes.values().next().unwrap();
        assert!(
            route.shell.doctype_and_head.contains("<title>ALBEDO "),
            "no metadata export → ALBEDO fallback title; head={}",
            route.shell.doctype_and_head
        );
        assert!(route.metadata.is_empty());
    }
}
