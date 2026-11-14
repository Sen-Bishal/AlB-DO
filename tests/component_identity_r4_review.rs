//! T-0045: route-local styles, inherited boundaries, and rebuild identity changes.

use dom_render_compiler::manifest::schema::{qualified_component_name, RenderManifestV2};
use dom_render_compiler::manifest::{build_render_manifest_v2, ManifestOptions};
use dom_render_compiler::scanner::ProjectScanner;
use std::fs;
use std::path::Path;

fn write(root: &Path, relative: &str, source: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, source).unwrap();
}

fn manifest(root: &Path) -> RenderManifestV2 {
    let compiler = ProjectScanner::new().scan_and_build(root).unwrap();
    let optimized = compiler.optimize().unwrap();
    build_render_manifest_v2(
        compiler.graph(),
        &optimized,
        &ManifestOptions {
            project_root: Some(root.to_path_buf()),
            ..ManifestOptions::default()
        },
    )
}

fn counter() -> &'static str {
    "import { useState } from 'albedo';\r\nimport styles from './counter.module.css';\r\nexport default function Counter({ seed }) { const [n, setN] = useState(seed); return <button className={styles.label} onClick={() => setN(n + 1)}>{n}</button>; }\r\n"
}

#[test]
fn sibling_layout_islands_keep_their_own_css_and_props() {
    let dir = tempfile::tempdir().unwrap();
    for (group, color, seed) in [("a", "red", 17), ("b", "blue", 31)] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            counter(),
        );
        write(
            dir.path(),
            &format!("components/{group}/counter.module.css"),
            &format!(".label {{ color: {color}; }}"),
        );
        write(dir.path(), &format!("routes/{group}/layout.tsx"), &format!(
            "import Counter from '../../components/{group}/Counter';\r\nexport default function Layout() {{ return <section><Counter seed={{{seed}}} /><children /></section>; }}\r\n"
        ));
        write(
            dir.path(),
            &format!("routes/{group}/index.tsx"),
            "export default function Page() { return <p>leaf</p>; }",
        );
    }
    let built = manifest(dir.path());
    assert!(
        built.static_render_failures.is_empty(),
        "{:?}",
        built.static_render_failures
    );
    for (group, color, other, seed) in [("a", "red", "blue", 17), ("b", "blue", "red", 31)] {
        let route = &built.routes[&format!("/{group}")];
        assert!(
            route
                .shell
                .doctype_and_head
                .contains(&format!("color: {color}")),
            "{}",
            route.shell.doctype_and_head
        );
        assert!(
            !route
                .shell
                .doctype_and_head
                .contains(&format!("color: {other}")),
            "{}",
            route.shell.doctype_and_head
        );
        assert_eq!(route.tier_c.len(), 1, "{route:?}");
        assert_eq!(
            route.tier_c[0].initial_props["seed"],
            serde_json::json!(f64::from(seed))
        );
        assert_eq!(
            route
                .shell
                .body_open
                .matches(&format!("id=\"{}\"", route.tier_c[0].placeholder_id))
                .count(),
            1
        );
    }
}

#[test]
fn inherited_same_named_boundaries_keep_the_nearest_module_identity() {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["", "branch/"] {
        for (file, name) in [("error", "Error"), ("loading", "Loading")] {
            write(
                dir.path(),
                &format!("routes/{folder}{file}.tsx"),
                &format!("export default function {name}() {{ return <p>{folder}{file}</p>; }}"),
            );
        }
    }
    for route in ["outer/deep", "branch/deep/leaf"] {
        write(
            dir.path(),
            &format!("routes/{route}/[id].tsx"),
            "export default function Page({ params }) { return <p>{params.id}</p>; }",
        );
    }
    let built = manifest(dir.path());
    for (route, folder) in [
        ("/outer/deep/[id]", ""),
        ("/branch/deep/leaf/[id]", "branch/"),
    ] {
        let route = &built.routes[route];
        for (stored, file, name) in [
            (route.error_component.as_deref(), "error", "Error"),
            (route.loading_component.as_deref(), "loading", "Loading"),
        ] {
            let component = built
                .components
                .iter()
                .find(|component| {
                    component.module_path.replace('\\', "/") == format!("routes/{folder}{file}.tsx")
                })
                .unwrap();
            assert_eq!(
                stored,
                Some(qualified_component_name(name, Some(component.id)).as_str())
            );
        }
    }
}

#[test]
fn rebuilding_after_removing_a_name_collision_drops_identity_state() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            counter(),
        );
        write(
            dir.path(),
            &format!("components/{group}/counter.module.css"),
            ".label { color: red; }",
        );
        write(dir.path(), &format!("routes/{group}.tsx"), &format!(
            "import Counter from '../components/{group}/Counter';\r\nexport default function Page() {{ return <main><Counter seed={{23}} /></main>; }}\r\n"
        ));
    }
    let shared = manifest(dir.path());
    assert!(shared.routes["/a"].tier_c[0].component_key.is_some());
    fs::remove_file(dir.path().join("routes/b.tsx")).unwrap();
    fs::remove_file(dir.path().join("components/b/Counter.tsx")).unwrap();
    for _ in 0..3 {
        let rebuilt = manifest(dir.path());
        assert!(
            rebuilt.static_render_failures.is_empty(),
            "{:?}",
            rebuilt.static_render_failures
        );
        assert!(!rebuilt.routes.contains_key("/b"));
        let route = &rebuilt.routes["/a"];
        assert_eq!(route.tier_c.len(), 1, "{route:?}");
        let node = &route.tier_c[0];
        assert_eq!(node.component_key, None);
        assert_eq!(node.initial_props["seed"], serde_json::json!(23.0));
        assert_eq!(
            route
                .tier_a_root
                .iter()
                .map(|node| node.html.as_str())
                .collect::<String>()
                .matches(&format!("id=\"{}\"", node.placeholder_id))
                .count(),
            1
        );
        assert!(!rebuilt
            .assets
            .chunks
            .keys()
            .any(|key| key.starts_with("Counter#")));
    }
}
