use dom_render_compiler::hydration::payload::HydrationPayload;
use dom_render_compiler::hydration::plan::HydrationTrigger;
use dom_render_compiler::hydration::script::{
    HYDRATION_BOOTSTRAP_ELEMENT_ID, HYDRATION_PAYLOAD_ELEMENT_ID,
};
use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier,
};
use dom_render_compiler::runtime::engine::BootstrapPayload;
use dom_render_compiler::runtime::quickjs_engine::{compile_client_island_module, QuickJsEngine};
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
                host_json: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    assert_eq!(result.html, "<main><p>hello</p></main>");
    assert_eq!(result.hydration_payload, "{}");
    assert!(result.head_tags.is_empty());
}

// A3.2 — a Tier-C route now ships the client runtime plus a self-registering
// component script in its head tags, and seeds the entry island's props into
// the hydration payload.
#[test]
fn test_hydration_tier_c_route_injects_client_runtime_and_props() {
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-17T00:00:00Z".to_string(),
        components: vec![component(
            50,
            "Widget",
            "components/widget",
            Tier::C,
            HydrationMode::OnInteraction,
            Vec::new(),
        )],
        parallel_batches: vec![vec![50]],
        critical_path: vec![50],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = HashMap::new();
    sources.insert(
        "components/widget".to_string(),
        "(props) => '<button>' + props.label + '</button>'".to_string(),
    );
    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let result = renderer
        .render_route_with_manifest_hydration(
            &RouteRenderRequest {
                entry: "components/widget".to_string(),
                props_json: r#"{"label":"go"}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
                host_json: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    // The shared client runtime is served, and the island self-registers.
    assert!(
        result
            .head_tags
            .iter()
            .any(|tag| tag.contains("/_albedo/client.js")),
        "Tier-C route must load the client runtime"
    );
    assert!(
        result
            .head_tags
            .iter()
            .any(|tag| tag.contains("registerComponent(\"50\"")),
        "the island must self-register with the client runtime"
    );

    // The entry island's props are seeded into the payload for client render.
    let payload: HydrationPayload =
        serde_json::from_str(&result.hydration_payload).expect("payload should deserialize");
    let entry = payload
        .islands
        .iter()
        .find(|island| island.component_id == 50)
        .expect("entry island should be present");
    assert_eq!(entry.props["label"], "go");
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
                host_json: None,
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
                host_json: None,
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

// A3.3 — the ENDGAME verification line.
//
// A Tier-C island is rendered server-side. The resulting HTML carries
// `data-albedo-island="{id}"` on the root element. The client runtime
// (`albedo-client.js`) is loaded, the island self-registers, the bootstrap
// fires on the first click (interaction trigger), hydration adopts the server
// node, and the second click advances local state — all with zero network.
//
// This is the full SSR → bootstrap → hydrateIsland → setState cycle driven
// under QuickJS with a compact DOM shim, proving the three slices (A3.1
// runtime, A3.2 compiler+serving, A3.3 SSR marker) compose correctly.

const CLIENT_RUNTIME_A33: &str = include_str!("../assets/albedo-client.js");

// Reuse the DOM shim from client_hydration.rs verbatim — same contract, same
// shim. Inlined here so the integration tests have no cross-file const dep.
const DOM_SHIM_A33: &str = r#"
globalThis.__net = 0;
globalThis.fetch = function () { globalThis.__net++; return {}; };
globalThis.queueMicrotask = function (fn) { fn(); };
globalThis.console = { error: function () {} };

function makeText(text) {
  return { nodeType: 3, nodeValue: text, parentNode: null };
}

function makeElement(tag) {
  var node = {
    nodeType: 1,
    tagName: tag.toUpperCase(),
    nodeName: tag.toUpperCase(),
    childNodes: [],
    attributes: {},
    listeners: {},
    parentNode: null,
  };
  node.appendChild = function (child) {
    child.parentNode = node;
    node.childNodes.push(child);
    return child;
  };
  node.removeChild = function (child) {
    var i = node.childNodes.indexOf(child);
    if (i >= 0) { node.childNodes.splice(i, 1); }
    child.parentNode = null;
    return child;
  };
  node.replaceChild = function (newChild, oldChild) {
    var i = node.childNodes.indexOf(oldChild);
    if (i >= 0) { node.childNodes[i] = newChild; newChild.parentNode = node; oldChild.parentNode = null; }
    return oldChild;
  };
  node.insertBefore = function (newChild, ref) {
    var i = node.childNodes.indexOf(ref);
    if (i < 0) { i = node.childNodes.length; }
    node.childNodes.splice(i, 0, newChild);
    newChild.parentNode = node;
    return newChild;
  };
  node.setAttribute = function (k, v) { node.attributes[k] = String(v); };
  node.removeAttribute = function (k) { delete node.attributes[k]; };
  node.getAttribute = function (k) {
    return Object.prototype.hasOwnProperty.call(node.attributes, k) ? node.attributes[k] : null;
  };
  node.addEventListener = function (t, fn) { (node.listeners[t] || (node.listeners[t] = [])).push(fn); };
  node.removeEventListener = function (t, fn) {
    var l = node.listeners[t];
    if (l) { var i = l.indexOf(fn); if (i >= 0) { l.splice(i, 1); } }
  };
  node.__dispatch = function (t) {
    var l = (node.listeners[t] || []).slice();
    var ev = { type: t, target: node };
    for (var i = 0; i < l.length; i++) { l[i](ev); }
  };
  Object.defineProperty(node, 'firstChild', {
    get: function () { return node.childNodes.length ? node.childNodes[0] : null; },
  });
  return node;
}

globalThis.document = {
  createElement: makeElement,
  createTextNode: makeText,
  getElementById: function (id) {
    if (id === '__ALBEDO_HYDRATION_PAYLOAD__' && globalThis.__payloadText) {
      return { textContent: globalThis.__payloadText };
    }
    return null;
  },
  querySelector: function (sel) {
    var m = /^\[data-albedo-island="(.+)"\]$/.exec(sel);
    if (!m || !globalThis.__domRoot) { return null; }
    var want = m[1];
    var stack = [globalThis.__domRoot];
    while (stack.length) {
      var n = stack.pop();
      if (n.nodeType === 1) {
        if (n.getAttribute('data-albedo-island') === want) { return n; }
        for (var i = n.childNodes.length - 1; i >= 0; i--) { stack.push(n.childNodes[i]); }
      }
    }
    return null;
  },
};
"#;

// The TSX island. No sub-imports, so compile_client_island_module succeeds.
// The server-side source is a plain JS function registered separately — the
// SSR and client representations are always two different artefacts.
const A33_COUNTER_TSX: &str = r#"
import { useState } from "react";
export default function Counter(props) {
  const [n, setN] = useState(props.start || 0);
  return <button onClick={() => setN(n + 1)}>{"count: " + n}</button>;
}
"#;

#[test]
fn test_a3_3_ssr_stamps_island_marker_and_hydrates_end_to_end() {
    const ISLAND_ID: u64 = 77;

    // --- server side ----------------------------------------------------------
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-17T00:00:00Z".to_string(),
        components: vec![component(
            ISLAND_ID,
            "Counter",
            "components/counter",
            Tier::C,
            HydrationMode::OnInteraction,
            Vec::new(),
        )],
        parallel_batches: vec![vec![ISLAND_ID]],
        critical_path: vec![ISLAND_ID],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = std::collections::HashMap::new();
    // SSR source: plain JS function the renderer evals server-side.
    sources.insert(
        "components/counter".to_string(),
        "(props) => '<button>count: ' + (props.start || 0) + '</button>'".to_string(),
    );
    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let result = renderer
        .render_route_with_manifest_hydration(
            &RouteRenderRequest {
                entry: "components/counter".to_string(),
                props_json: r#"{"start":5}"#.to_string(),
                module_order: Vec::new(),
                hydration_payload: None,
                host_json: None,
            },
            &manifest,
        )
        .expect("render should succeed");

    // A3.3 core assertion: the island marker is in the SSR HTML.
    let expected_marker = format!("data-albedo-island=\"{}\"", ISLAND_ID);
    assert!(
        result.html.contains(&expected_marker),
        "SSR HTML must carry the island marker for the bootstrap to find it: got {:?}",
        result.html
    );
    // Sanity: text content is still there.
    assert!(result.html.contains("count: 5"), "SSR HTML must contain rendered content");

    // --- client side (full e2e under QuickJS) ---------------------------------
    let island_script = compile_client_island_module("components/counter", A33_COUNTER_TSX, ISLAND_ID)
        .expect("counter island should compile to a self-registering browser module");

    // Extract the bootstrap script from the head tags.
    let bootstrap_tag = result
        .head_tags
        .iter()
        .find(|tag| tag.contains(HYDRATION_BOOTSTRAP_ELEMENT_ID))
        .expect("bootstrap script tag must be present");
    let bootstrap_script = script_body(bootstrap_tag);

    let payload_json = serde_json::to_string(&result.hydration_payload)
        .expect("hydration payload should be JSON-serialisable");

    let runtime = Runtime::new().expect("quickjs runtime should initialise");
    let context = Context::full(&runtime).expect("quickjs context should initialise");

    let summary: String = context.with(|ctx| {
        ctx.eval::<(), _>(DOM_SHIM_A33).expect("DOM shim should evaluate");
        ctx.eval::<(), _>(CLIENT_RUNTIME_A33).expect("client runtime should evaluate");
        ctx.eval::<(), _>(island_script.as_str()).expect("island script should evaluate");

        // Wire the payload so the bootstrap can verify the checksum.
        let setup = format!("globalThis.__payloadText = {};", payload_json);
        ctx.eval::<(), _>(setup.as_str()).expect("payload setup should evaluate");

        // Build the DOM tree that matches the SSR output (button with island marker).
        let dom_setup = format!(
            r#"
var body = document.createElement('div');
var button = document.createElement('button');
button.setAttribute('data-albedo-island', '{id}');
button.appendChild(document.createTextNode('count: 5'));
body.appendChild(button);
globalThis.__domRoot = body;
globalThis.__button = button;
"#,
            id = ISLAND_ID
        );
        ctx.eval::<(), _>(dom_setup.as_str()).expect("DOM setup should evaluate");

        // Run the bootstrap. For OnInteraction islands, it wires a click
        // listener on the island root that fires __ALBEDO_HYDRATE_ISLAND.
        ctx.eval::<(), _>(bootstrap_script).expect("bootstrap should evaluate");

        // First click → bootstrap listener fires → hydrateIslandDescriptor →
        // adopts server button, wires component's onClick.
        ctx.eval::<(), _>("globalThis.__button.__dispatch('click');")
            .expect("first click should dispatch");

        // Second click → component's onClick fires → setN(5+1) → reconcile →
        // text node patches to "count: 6".
        ctx.eval::<(), _>("globalThis.__button.__dispatch('click');")
            .expect("second click should dispatch");

        ctx.eval::<String, _>(r#"JSON.stringify({
  hydrated: globalThis.__button.getAttribute('data-albedo-hydrated'),
  text: globalThis.__button.firstChild.nodeValue,
  sameNode: globalThis.__domRoot.firstChild === globalThis.__button,
  network: globalThis.__net,
})"#)
            .expect("summary should serialize")
    });

    let value: serde_json::Value =
        serde_json::from_str(&summary).expect("summary should be valid JSON");

    // The bootstrap marked the root hydrated on first click.
    assert_eq!(value["hydrated"], "true", "island root must be marked hydrated");
    // The second click advanced local state — no server round-trip.
    assert_eq!(value["text"], "count: 6", "client state must advance on click");
    // The server DOM node was adopted, not recreated.
    assert_eq!(value["sameNode"], true, "server node must be patched in place");
    // Zero network calls — the whole interaction was local.
    assert_eq!(value["network"], 0, "Tier-C interaction must not round-trip");
}
