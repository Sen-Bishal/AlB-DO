use dom_render_compiler::manifest::schema::{
    ComponentManifestEntry, HydrationMode, RenderManifestV2, Tier,
};
use dom_render_compiler::runtime::engine::BootstrapPayload;
use dom_render_compiler::runtime::quickjs_engine::QuickJsEngine;
use dom_render_compiler::runtime::renderer::{
    FsRouteRenderRequest, RouteRenderRequest, ServerRenderer,
};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("visual")
        .join(name)
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(fixture_path(name)).expect("fixture should exist")
}

fn normalize_html(value: &str) -> String {
    value.replace("\r\n", "\n").trim().to_string()
}

fn assert_matches_fixture(actual: &str, fixture_name: &str) {
    let expected = read_fixture(fixture_name);
    assert_eq!(normalize_html(actual), normalize_html(&expected));
}

fn create_renderer() -> ServerRenderer<QuickJsEngine> {
    let engine = QuickJsEngine::new();
    let bootstrap = BootstrapPayload::default();
    ServerRenderer::new(engine, &bootstrap).expect("renderer should initialize")
}

#[test]
fn test_visual_snapshot_showcase_route() {
    let mut renderer = create_renderer();

    renderer.register_module(
        "ui/pill",
        "(props) => '<span class=\"pill\">' + props.label + '</span>'",
    );
    renderer.register_module(
        "ui/stat-card",
        "(props) => '<article class=\"stat-card\"><h3>' + props.label + '</h3><p>' + props.value + '</p></article>'",
    );
    renderer.register_module(
        "sections/activity-feed",
        "(props) => '<ul class=\"activity-feed\">' + props.items.map(function(item) { return '<li>' + item + '</li>'; }).join('') + '</ul>'",
    );
    renderer.register_module_with_metadata(
        "sections/hero",
        "(props, require) => '<section class=\"hero\"><h1>' + props.headline + '</h1><p>' + props.subhead + '</p>' + require('ui/pill')({label: props.tier}) + '</section>'",
        vec!["ui/pill".to_string()],
        vec!["<meta name=\"albedo-engine\" content=\"quickjs\" />".to_string()],
    );
    renderer.register_module_with_dependencies(
        "sections/metrics",
        "(props, require) => '<section class=\"metrics\">' + props.metrics.map(function(metric) { return require('ui/stat-card')({label: metric.label, value: metric.value}); }).join('') + '</section>'",
        vec!["ui/stat-card".to_string()],
    );
    renderer.register_module_with_metadata(
        "routes/showcase",
        "(props, require) => '<main class=\"showcase\"><header class=\"masthead\"><h2>Project Alpha</h2><p>Server-rendered preview</p></header>' + require('sections/hero')(props) + require('sections/metrics')({metrics: props.metrics}) + '<section class=\"activity\"><h3>Execution Track</h3>' + require('sections/activity-feed')({items: props.activities}) + '</section></main>'",
        vec![
            "sections/hero".to_string(),
            "sections/metrics".to_string(),
            "sections/activity-feed".to_string(),
        ],
        vec![
            "<meta name=\"albedo-engine\" content=\"quickjs\" />".to_string(),
            "<meta name=\"albedo-route\" content=\"showcase\" />".to_string(),
        ],
    );

    let props = json!({
        "headline": "ALBEDO Phase 2",
        "subhead": "Independent SSR renderer with graph-aware module loading.",
        "tier": "Tier C Ready",
        "metrics": [
            { "label": "SSR p95", "value": "42ms" },
            { "label": "Hydration JS", "value": "-38%" },
            { "label": "LCP", "value": "1.7s" }
        ],
        "activities": [
            "Manifest v2",
            "QuickJS runtime",
            "Route renderer",
            "Visual snapshots"
        ]
    });

    let result = renderer
        .render_route(&RouteRenderRequest {
            entry: "routes/showcase".to_string(),
            props_json: props.to_string(),
            module_order: Vec::new(),
            hydration_payload: None,
        })
        .expect("route render should succeed");

    assert_matches_fixture(&result.html, "showcase_route.html");
    assert_eq!(
        result.head_tags,
        vec![
            "<meta name=\"albedo-engine\" content=\"quickjs\" />".to_string(),
            "<meta name=\"albedo-route\" content=\"showcase\" />".to_string()
        ]
    );
    assert_eq!(result.hydration_payload, "{}");
    assert!(result.timings.total_ms >= result.timings.render_ms);
}

#[test]
fn test_visual_snapshot_manifest_driven_case_study_route() {
    let mut renderer = create_renderer();

    let manifest = RenderManifestV2 {
        schema_version: "2.0".to_string(),
        generated_at: "2026-02-12T00:00:00Z".to_string(),
        components: vec![
            ComponentManifestEntry {
                id: 11,
                name: "TitleBlock".to_string(),
                module_path: "components/title-block".to_string(),
                tier: Tier::A,
                weight_bytes: 2048,
                priority: 10.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: HydrationMode::None,
            },
            ComponentManifestEntry {
                id: 12,
                name: "KpiRow".to_string(),
                module_path: "components/kpi-row".to_string(),
                tier: Tier::B,
                weight_bytes: 4096,
                priority: 9.0,
                dependencies: vec![],
                can_defer: false,
                hydration_mode: HydrationMode::OnIdle,
            },
            ComponentManifestEntry {
                id: 13,
                name: "CaseStudyRoute".to_string(),
                module_path: "routes/case-study".to_string(),
                tier: Tier::B,
                weight_bytes: 8192,
                priority: 11.0,
                dependencies: vec![11, 12],
                can_defer: false,
                hydration_mode: HydrationMode::OnIdle,
            },
        ],
        parallel_batches: vec![vec![11, 12], vec![13]],
        critical_path: vec![11, 13],
        vendor_chunks: Vec::new(),
        ..RenderManifestV2::legacy_defaults()
    };

    let mut sources = HashMap::new();
    sources.insert(
        "components/title-block".to_string(),
        "(props) => '<header class=\"title-block\"><h1>' + props.title + '</h1><p>' + props.summary + '</p></header>'".to_string(),
    );
    sources.insert(
        "components/kpi-row".to_string(),
        "(props) => '<section class=\"kpi-row\">' + props.kpis.map(function(kpi) { return '<article class=\"kpi\"><h3>' + kpi.name + '</h3><p>' + kpi.value + '</p></article>'; }).join('') + '</section>'".to_string(),
    );
    sources.insert(
        "routes/case-study".to_string(),
        "(props, require) => '<main class=\"case-study\">' + require('components/title-block')({title: props.title, summary: props.summary}) + require('components/kpi-row')({kpis: props.kpis}) + '<footer class=\"note\">Generated by ALBEDO Runtime</footer></main>'".to_string(),
    );

    renderer
        .register_manifest_modules(&manifest, &sources)
        .expect("manifest registration should succeed");

    let props = json!({
        "title": "Benchmarked Route",
        "summary": "Measured p50/p95 from native SSR runtime.",
        "kpis": [
            { "name": "TTFB", "value": "79ms" },
            { "name": "INP", "value": "120ms" },
            { "name": "SSR p95", "value": "42ms" }
        ]
    });

    let result = renderer
        .render_route(&RouteRenderRequest {
            entry: "routes/case-study".to_string(),
            props_json: props.to_string(),
            module_order: Vec::new(),
            hydration_payload: Some("{\"route\":\"case-study\",\"islands\":0}".to_string()),
        })
        .expect("manifest-driven route render should succeed");

    assert_matches_fixture(&result.html, "case_study_route.html");
    assert_eq!(
        result.hydration_payload,
        "{\"route\":\"case-study\",\"islands\":0}"
    );
}

#[test]
fn test_visual_snapshot_client_demo_route_for_non_technical_audience() {
    let mut renderer = create_renderer();

    renderer.register_module(
        "ui/chip",
        r#"(props) => '<span class="chip">' + props.label + '</span>'"#,
    );
    renderer.register_module(
        "ui/section-title",
        r#"(props) => '<header class="section-title"><h2>' + props.title + '</h2><p>' + props.subtitle + '</p></header>'"#,
    );
    renderer.register_module_with_dependencies(
        "sections/hero-banner",
        r#"(props, require) => '<section class="hero-banner"><p class="eyebrow">' + props.eyebrow + '</p><h1>' + props.headline + '</h1><p class="lead">' + props.lead + '</p><div class="chip-row">' + props.tags.map(function(tag) { return require('ui/chip')({label: tag}); }).join('') + '</div></section>'"#,
        vec!["ui/chip".to_string()],
    );
    renderer.register_module_with_dependencies(
        "sections/business-value",
        r#"(props, require) => '<section class="business-value">' + require('ui/section-title')({title: 'Business Outcomes', subtitle: 'What your team gets without reading technical details.'}) + '<div class="value-grid">' + props.outcomes.map(function(item) { return '<article class="value-card"><h3>' + item.title + '</h3><p>' + item.description + '</p><strong>' + item.impact + '</strong></article>'; }).join('') + '</div></section>'"#,
        vec!["ui/section-title".to_string()],
    );
    renderer.register_module_with_dependencies(
        "sections/before-after",
        r#"(props, require) => '<section class="before-after">' + require('ui/section-title')({title: 'Before and After', subtitle: 'Clear comparison for decision makers.'}) + '<div class="comparison"><article><h3>Before ALBEDO</h3><ul>' + props.before.map(function(item) { return '<li>' + item + '</li>'; }).join('') + '</ul></article><article><h3>With ALBEDO</h3><ul>' + props.after.map(function(item) { return '<li>' + item + '</li>'; }).join('') + '</ul></article></div></section>'"#,
        vec!["ui/section-title".to_string()],
    );
    renderer.register_module_with_dependencies(
        "sections/release-plan",
        r#"(props, require) => '<section class="release-plan">' + require('ui/section-title')({title: 'Delivery Timeline', subtitle: 'What happens each week.'}) + '<ol class="timeline">' + props.plan.map(function(step) { return '<li><h4>' + step.week + '</h4><p>' + step.deliverable + '</p></li>'; }).join('') + '</ol></section>'"#,
        vec!["ui/section-title".to_string()],
    );
    renderer.register_module_with_dependencies(
        "sections/faq",
        r#"(props, require) => '<section class="faq">' + require('ui/section-title')({title: 'Frequently Asked Questions', subtitle: 'Answers stakeholders usually ask first.'}) + '<div class="faq-list">' + props.faq.map(function(item) { return '<details><summary>' + item.q + '</summary><p>' + item.a + '</p></details>'; }).join('') + '</div></section>'"#,
        vec!["ui/section-title".to_string()],
    );
    renderer.register_module_with_metadata(
        "routes/client-demo",
        r#"(props, require) => '<main class="client-demo"><style>.client-demo{font-family:"Segoe UI",Arial,sans-serif;line-height:1.45;color:#0f172a;background:linear-gradient(180deg,#f8fafc 0%,#eef2ff 100%);padding:28px}.hero-banner{background:#0f172a;color:#f8fafc;padding:28px;border-radius:16px;box-shadow:0 20px 40px rgba(15,23,42,.18)}.eyebrow{letter-spacing:.08em;text-transform:uppercase;font-size:12px;opacity:.8}.lead{max-width:64ch}.chip-row{display:flex;gap:10px;flex-wrap:wrap;margin-top:14px}.chip{display:inline-block;background:#1d4ed8;color:#fff;padding:6px 10px;border-radius:999px;font-size:12px}.section-title h2{margin-bottom:4px}.section-title p{margin-top:0;color:#334155}.business-value,.before-after,.release-plan,.faq{margin-top:24px;background:#fff;border-radius:14px;padding:20px;box-shadow:0 10px 24px rgba(15,23,42,.08)}.value-grid{display:grid;grid-template-columns:repeat(3,minmax(0,1fr));gap:14px}.value-card{background:#f8fafc;border:1px solid #dbeafe;border-radius:10px;padding:14px}.comparison{display:grid;grid-template-columns:1fr 1fr;gap:14px}.comparison article{background:#f8fafc;border-radius:10px;padding:12px}.timeline{padding-left:18px}.timeline li{margin-bottom:10px}.faq-list details{background:#f8fafc;border-radius:10px;padding:10px;margin-bottom:10px}@media (max-width:900px){.value-grid,.comparison{grid-template-columns:1fr}}</style><section class="intro-strip"><p><strong>Client Preview:</strong> This page is fully server-rendered by the ALBEDO runtime and mirrors production layout fidelity.</p></section>' + require('sections/hero-banner')({eyebrow: props.eyebrow, headline: props.headline, lead: props.lead, tags: props.tags}) + require('sections/business-value')({outcomes: props.outcomes}) + require('sections/before-after')({before: props.before, after: props.after}) + require('sections/release-plan')({plan: props.plan}) + require('sections/faq')({faq: props.faq}) + '<footer class="footnote"><p><strong>Confidence score:</strong> ' + props.confidence + '</p><p>Generated at request-time by ALBEDO SSR Renderer.</p></footer></main>'"#,
        vec![
            "sections/hero-banner".to_string(),
            "sections/business-value".to_string(),
            "sections/before-after".to_string(),
            "sections/release-plan".to_string(),
            "sections/faq".to_string(),
        ],
        vec![
            "<meta name=\"albedo-demo\" content=\"client-facing\" />".to_string(),
            "<meta name=\"albedo-audience\" content=\"non-technical\" />".to_string(),
        ],
    );

    let props = json!({
        "eyebrow": "Executive Review Deck",
        "headline": "Faster Pages, Less Risk, Clear Rollout",
        "lead": "ALBEDO prioritizes what users see first, delays the rest intelligently, and gives leadership measurable outcomes.",
        "tags": ["No Rewrite Needed", "Progressive Rollout", "Measured KPIs"],
        "outcomes": [
            {
                "title": "Lower Wait Time",
                "description": "Users get meaningful content on screen earlier.",
                "impact": "Expected 20-35% TTFB/LCP improvement"
            },
            {
                "title": "Controlled Change",
                "description": "Rollout can happen route-by-route with fallbacks.",
                "impact": "Reduced release risk and easier rollback"
            },
            {
                "title": "Less Client Work",
                "description": "Hydration is scoped to only necessary islands.",
                "impact": "Lower browser CPU on mid-range devices"
            }
        ],
        "before": [
            "All components hydrated immediately",
            "Heavy scripts loaded up front",
            "Difficult to explain performance tradeoffs"
        ],
        "after": [
            "Critical components render first",
            "Deferred hydration by intent and priority",
            "Clear, reportable latency and vitals metrics"
        ],
        "plan": [
            { "week": "Week 1", "deliverable": "Baseline route profiling and manifest generation" },
            { "week": "Week 2", "deliverable": "Native runtime SSR pilot on one high-traffic route" },
            { "week": "Week 3", "deliverable": "Hydration policy tuning and KPI validation" },
            { "week": "Week 4", "deliverable": "Production rollout decision with confidence report" }
        ],
        "faq": [
            {
                "q": "Will this require rewriting the app?",
                "a": "No. ALBEDO integrates with your existing components and introduces optimization progressively."
            },
            {
                "q": "How do we measure success?",
                "a": "We track TTFB, LCP, INP, hydration CPU, and route-level latency deltas against baseline."
            },
            {
                "q": "What if a route has issues?",
                "a": "Fallback paths are preserved so traffic can revert to baseline behavior immediately."
            }
        ],
        "confidence": "High (based on staged benchmarks + fallback coverage)"
    });

    let result = renderer
        .render_route(&RouteRenderRequest {
            entry: "routes/client-demo".to_string(),
            props_json: props.to_string(),
            module_order: Vec::new(),
            hydration_payload: Some(
                "{\"route\":\"client-demo\",\"mode\":\"showcase\"}".to_string(),
            ),
        })
        .expect("client demo route render should succeed");

    assert_matches_fixture(&result.html, "client_demo_route.html");
    assert_eq!(
        result.head_tags,
        vec![
            "<meta name=\"albedo-demo\" content=\"client-facing\" />".to_string(),
            "<meta name=\"albedo-audience\" content=\"non-technical\" />".to_string()
        ]
    );
    assert_eq!(
        result.hydration_payload,
        "{\"route\":\"client-demo\",\"mode\":\"showcase\"}"
    );
}

#[test]
fn test_visual_snapshot_test_app_components_directory_render() {
    let mut renderer = create_renderer();
    let components_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("components");

    let result = renderer
        .render_route_from_component_dir(&FsRouteRenderRequest {
            components_root,
            entry_module: "App.jsx".to_string(),
            props_json: "{}".to_string(),
            hydration_payload: Some("{\"route\":\"test-app\"}".to_string()),
        })
        .expect("test-app filesystem render should succeed");

    assert_matches_fixture(&result.html, "test_app_components_route.html");
    assert_eq!(result.hydration_payload, "{\"route\":\"test-app\"}");
}
