//! T-0045: boundary plans from a real build keep their module identity when
//! loading/error names collide, including after a boundary itself throws.

use albedo_server::engine_pool::QuickJsEnginePool;
use albedo_server::render::tier_b::{PooledTierBRenderRegistry, TierBRenderRegistry};
use albedo_server::renderer_runtime::RendererRuntime;
use dom_render_compiler::manifest::schema::{
    qualified_component_name, split_qualified_component_name,
};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

fn command(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_albedo"));
    command
        .current_dir(cwd)
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env("NO_COLOR", "1")
        .env("ALBEDO_NO_TUI", "1");
    command
}

#[test]
fn boundary_identity_encoding_round_trips_extremes_and_rejects_invalid_suffixes() {
    for key in [0, 1, u64::MAX] {
        let encoded = qualified_component_name("Loading", Some(key));
        assert_eq!(
            split_qualified_component_name(&encoded),
            ("Loading", Some(key))
        );
    }
    assert_eq!(split_qualified_component_name("Loading"), ("Loading", None));
    for invalid in [
        "Loading#",
        "Loading#-1",
        "Loading#18446744073709551616",
        "Loading#x",
    ] {
        assert_eq!(split_qualified_component_name(invalid), (invalid, None));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_build_duplicate_loading_and_error_plans_survive_pool_reuse_and_errors() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let initialized = command(home.path(), work.path())
        .args(["init", "app"])
        .output()
        .unwrap();
    assert!(
        initialized.status.success(),
        "{}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    let app = work.path().join("app");
    for which in ["a", "b"] {
        let routes = app.join(format!("src/routes/side{which}"));
        fs::create_dir_all(&routes).unwrap();
        fs::write(routes.join("[id].tsx"), format!(
            "export default function Page({{params}}) {{ return <p>page-{which} {{params.id}}</p>; }}"
        )).unwrap();
        fs::write(
            routes.join("loading.tsx"),
            format!("const Loading = () => <p>loading-{which}</p>; export default Loading;"),
        )
        .unwrap();
        fs::write(routes.join("error.tsx"), format!(
            "export default function Error({{fail}}) {{ if (fail) throw 'boundary-failed-{which}'; return <p>boundary-{which}</p>; }}"
        )).unwrap();
    }
    let built = command(home.path(), &app)
        .args(["build", "."])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&built.stdout),
        String::from_utf8_lossy(&built.stderr)
    );
    let runtime = RendererRuntime::from_artifacts_dir(app.join(".albedo/dist")).unwrap();
    let keys: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|which| {
            let route = &runtime.manifest().routes[&format!("/side{which}/[id]")];
            (
                route.loading_component.clone().unwrap(),
                route.error_component.clone().unwrap(),
            )
        })
        .collect();
    assert_ne!(
        keys[0].0, keys[1].0,
        "loading boundaries must have separate keys"
    );
    assert_ne!(
        keys[0].1, keys[1].1,
        "error boundaries must have separate keys"
    );
    let registry = PooledTierBRenderRegistry::new(
        Arc::new(QuickJsEnginePool::with_size(1)),
        runtime.build_tier_b_render_plan(None),
        Arc::new(dom_render_compiler::runtime::BroadcastRegistry::new()),
        serde_json::Map::new(),
        Arc::new(albedo_server::topics::NoTopicWarmer),
    );
    // One engine alternates all four plans, so a retained module cannot hide
    // a key collision or poison the next boundary after a failed render.
    for _ in 0..3 {
        for (index, which) in ["a", "b"].into_iter().enumerate() {
            let other = if which == "a" { "b" } else { "a" };
            for (key, prefix) in [(&keys[index].0, "loading"), (&keys[index].1, "boundary")] {
                let html = registry
                    .call(key, &serde_json::json!({"fail": false}), &HashMap::new())
                    .await
                    .unwrap();
                assert!(html.contains(&format!("{prefix}-{which}")), "{html}");
                assert!(!html.contains(&format!("{prefix}-{other}")), "{html}");
            }
        }
        assert!(registry
            .call(
                &keys[1].1,
                &serde_json::json!({"fail": true}),
                &HashMap::new()
            )
            .await
            .is_err());
    }
}
