use dom_render_compiler::hydration::payload::HydrationPayload;
use dom_render_compiler::hydration::plan::HydrationTrigger;
use dom_render_compiler::hydration::script::{
    HYDRATION_BOOTSTRAP_ELEMENT_ID, HYDRATION_PAYLOAD_ELEMENT_ID,
};
use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier,
};
use dom_render_compiler::runtime::engine::BootstrapPayload;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::renderer::{RouteRenderRequest, ServerRenderer};
use rquickjs::{Context, Runtime};
use std::collections::HashMap;

fn create_renderer() -> ServerRenderer<QuickJsEngine> {
    let engine = QuickJsEngine::new();
    let bootstrap = BootstrapPayload::default();
    ServerRenderer::new(engine, &bootstrap).expect("renderer should initialize")
}

fn component(
    id: u64,
    name: &str,
    module_path: &str,
    tier: Tier,
    hydration_mode: HydrationMode,
    dependencies: Vec<u64>,
) -> ComponentManifestEntry {
    ComponentManifestEntry {
        id,
        name: name.to_string(),
        module_path: module_path.to_string(),
        tier,
        weight_bytes: 4096,
        priority: 1.0,
        dependencies,
        can_defer: true,
        hydration_mode,
    }
}

fn script_body(tag: &str) -> &str {
    let start = tag.find('>').expect("script tag should contain '>'") + 1;
    let end = tag
        .rfind("</script>")
        .expect("script tag should contain closing tag");
    &tag[start..end]
}

#[test]
fn test_hydration_no_js_for_tier_a_only_route() {
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-17T00:00:00Z".to_string(),
        components: vec![
            component(
                1,
                "Leaf",
                "components/leaf",
                Tier::A,
                HydrationMode::None,
                Vec::new(),
            ),
            component(
                2,
                "Entry",
                "routes/home",
                Tier::A,
                HydrationMode::None,
                vec![1],
            ),
        ],
        parallel_batches: vec![vec![1, 2]],
        critical_path: vec![2],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = HashMap::new();
    sources.insert(
        "components/leaf".to_string(),
        "(props) => '<p>' + props.message + '</p>'".to_string(),
    );
    sources.insert(
        "routes/home".to_string(),
        "(props, require) => '<main>' + require('components/leaf')({message: props.message}) + '</main>'".to_string(),
    );
    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let result = renderer
        .render_route_with_manifest_hydration(
            &RouteRenderRequest {
                entry: "routes/home".to_string(),
                props_json: r#"{"message":"hello"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    assert_eq!(result.html, "<main><p>hello</p></main>");
    assert_eq!(result.hydration_payload, "{}");
    assert!(result.head_tags.is_empty());
}

#[test]
fn test_hydration_tier_c_uses_trigger_gated_islands() {
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-17T00:00:00Z".to_string(),
        components: vec![
            component(
                10,
                "VisiblePanel",
                "components/visible",
                Tier::C,
                HydrationMode::OnVisible,
                Vec::new(),
            ),
            component(
                11,
                "InteractivePanel",
                "components/interactive",
                Tier::C,
                HydrationMode::OnInteraction,
                Vec::new(),
            ),
            component(
                12,
                "IdlePanel",
                "components/idle",
                Tier::B,
                HydrationMode::OnIdle,
                Vec::new(),
            ),
            component(
                20,
                "Entry",
                "routes/home",
                Tier::C,
                HydrationMode::OnInteraction,
                vec![10, 11, 12],
            ),
        ],
        parallel_batches: vec![vec![10, 11, 12], vec![20]],
        critical_path: vec![20],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = HashMap::new();
    sources.insert(
        "components/visible".to_string(),
        "(props) => '<section>' + props.visible + '</section>'".to_string(),
    );
    sources.insert(
        "components/interactive".to_string(),
        "(props) => '<button>' + props.cta + '</button>'".to_string(),
    );
    sources.insert(
        "components/idle".to_string(),
        "(props) => '<aside>' + props.note + '</aside>'".to_string(),
    );
    sources.insert(
        "routes/home".to_string(),
        "(props, require) => '<main>' + require('components/visible')({visible: props.visible}) + require('components/interactive')({cta: props.cta}) + require('components/idle')({note: props.note}) + '</main>'".to_string(),
    );
    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let result = renderer
        .render_route_with_manifest_hydration(
            &RouteRenderRequest {
                entry: "routes/home".to_string(),
                props_json: r#"{"visible":"hero","cta":"click","note":"later"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    let payload: HydrationPayload =
        serde_json::from_str(&result.hydration_payload).expect("payload should deserialize");

    let trigger_by_component: HashMap<u64, HydrationTrigger> = payload
        .islands
        .iter()
        .map(|island| (island.component_id, island.trigger))
        .collect();

    assert_eq!(
        trigger_by_component.get(&10),
        Some(&HydrationTrigger::Visible)
    );
    assert_eq!(
        trigger_by_component.get(&11),
        Some(&HydrationTrigger::Interaction)
    );
    assert_eq!(trigger_by_component.get(&12), Some(&HydrationTrigger::Idle));
    assert_eq!(
        trigger_by_component.get(&20),
        Some(&HydrationTrigger::Interaction)
    );

    assert!(result
        .head_tags
        .iter()
        .any(|tag| tag.contains(HYDRATION_PAYLOAD_ELEMENT_ID)));
    assert!(result
        .head_tags
        .iter()
        .any(|tag| tag.contains(HYDRATION_BOOTSTRAP_ELEMENT_ID)));
}

#[test]
fn test_hydration_island_failure_does_not_break_other_islands() {
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-17T00:00:00Z".to_string(),
        components: vec![
            component(
                32,
                "Child",
                "components/child",
                Tier::B,
                HydrationMode::OnIdle,
                Vec::new(),
            ),
            component(
                31,
                "Entry",
                "routes/home",
                Tier::B,
                HydrationMode::OnIdle,
                vec![32],
            ),
        ],
        parallel_batches: vec![vec![32], vec![31]],
        critical_path: vec![31],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = HashMap::new();
    sources.insert(
        "components/child".to_string(),
        "(props) => '<p>' + props.value + '</p>'".to_string(),
    );
    sources.insert(
        "routes/home".to_string(),
        "(props, require) => '<main>' + require('components/child')({value: props.value}) + '</main>'".to_string(),
    );
    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let result = renderer
        .render_route_with_manifest_hydration(
            &RouteRenderRequest {
                entry: "routes/home".to_string(),
                props_json: r#"{"value":"ok"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    let bootstrap_tag = result
        .head_tags
        .iter()
        .find(|tag| tag.contains(HYDRATION_BOOTSTRAP_ELEMENT_ID))
        .expect("bootstrap tag should be present");
    let bootstrap_script = script_body(bootstrap_tag);

    let runtime = Runtime::new().expect("quickjs runtime should initialize");
    let context = Context::full(&runtime).expect("quickjs context should initialize");
    context.with(|ctx| {
        let payload_literal = serde_json::to_string(&result.hydration_payload).unwrap();
        let setup = format!(
            "globalThis.__payload={payload_literal};\
globalThis.__calls=[];\
globalThis.__errors=[];\
globalThis.requestIdleCallback=function(cb){{cb();}};\
globalThis.setTimeout=function(cb){{cb();}};\
globalThis.console={{error:function(){{globalThis.__errors.push('err');}}}};\
globalThis.document={{\
getElementById:function(id){{if(id==='{payload_id}'){{return {{textContent:globalThis.__payload}};}}return null;}},\
querySelector:function(){{return {{addEventListener:function(_ev,cb){{cb();}}}};}}\
}};\
globalThis.__ALBEDO_HYDRATE_ISLAND=function(island){{if(island.component_id===31){{throw new Error('boom');}}globalThis.__calls.push(island.component_id);}};",
            payload_id = HYDRATION_PAYLOAD_ELEMENT_ID
        );

        ctx.eval::<(), _>(setup.as_str())
            .expect("setup script should execute");
        ctx.eval::<(), _>(bootstrap_script)
            .expect("bootstrap script should execute");

        let calls_json: String = ctx
            .eval("JSON.stringify(globalThis.__calls)")
            .expect("calls should serialize");
        let error_count: i32 = ctx
            .eval("globalThis.__errors.length")
            .expect("error count should be readable");

        assert_eq!(calls_json, "[32]");
        assert!(error_count >= 1);
    });
}
