//! T-0045 revision 3: deferred identities must not steal static modules or props.

use dom_render_compiler::manifest::schema::RenderManifestV2;
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
    "import { useState } from 'albedo';\r\nexport default function Counter({ seed }) { const [n, setN] = useState(seed); return <button onClick={() => setN(n + 1)}>{n}</button>; }\r\n"
}

#[test]
fn static_same_named_module_with_longer_path_is_not_a_deferred_suffix_match() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "components/Counter.tsx", counter());
    write(
        dir.path(),
        "nested/components/Counter.tsx",
        "export default function Counter({ seed }) { return <p>static-counter {seed}</p>; }",
    );
    write(dir.path(), "routes/index.tsx", "import Interactive from '../components/Counter';\nimport Static from '../nested/components/Counter';\nexport default function Home() { return <main><Interactive seed={11} /><Static seed={99} /></main>; }");
    let built = manifest(dir.path());
    let route = &built.routes["/"];
    let html: String = std::iter::once(route.shell.body_open.as_str())
        .chain(route.tier_a_root.iter().map(|node| node.html.as_str()))
        .collect();
    assert!(
        html.contains("static-counter 99"),
        "the static module must render, even when its path ends in the deferred module's path: {html}"
    );
    assert_eq!(route.tier_c.len(), 1, "{route:?}");
    let node = &route.tier_c[0];
    assert_eq!(node.initial_props["seed"], serde_json::json!(11.0));
    assert_eq!(
        html.matches(&format!("id=\"{}\"", node.placeholder_id))
            .count(),
        1
    );
}

#[test]
fn repeated_builds_drain_layout_props_without_overwriting_route_props() {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["layout", "page"] {
        write(
            dir.path(),
            &format!("components/{folder}/Counter.tsx"),
            counter(),
        );
    }
    write(dir.path(), "routes/layout.tsx", "import Counter from '../components/layout/Counter';\r\nexport default function Layout() { return <section><Counter seed={71} /><children /></section>; }");
    for (route, seed) in [("index", 13), ("a", 29), ("z", 47)] {
        write(dir.path(), &format!("routes/{route}.tsx"), &format!("import Counter from '../components/page/Counter';\nexport default function Page() {{ return <main><Counter seed={{{seed}}} /></main>; }}"));
    }
    for _ in 0..4 {
        let built = manifest(dir.path());
        assert!(
            built.static_render_failures.is_empty(),
            "{:?}",
            built.static_render_failures
        );
        for (route_name, seed) in [("/", 13), ("/a", 29), ("/z", 47)] {
            let route = &built.routes[route_name];
            assert_eq!(route.tier_c.len(), 2, "{route:?}");
            for node in &route.tier_c {
                let component = built
                    .components
                    .iter()
                    .find(|entry| Some(entry.id) == node.component_key)
                    .unwrap();
                let expected = if component
                    .module_path
                    .replace('\\', "/")
                    .contains("/layout/")
                {
                    71
                } else {
                    seed
                };
                assert_eq!(
                    node.initial_props["seed"],
                    serde_json::json!(f64::from(expected)),
                    "{route_name}: {node:?}"
                );
            }
        }
    }
}

#[test]
fn duplicate_layout_islands_keep_large_structured_props_under_unicode_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project with spaces 日本語");
    let label = "<&>\"日本語".repeat(1024);
    let encoded = serde_json::to_string(&label).unwrap();
    for (group, seed) in [("a", 11), ("b", 37)] {
        write(&root, &format!("components/{group}/Counter.tsx"), counter());
        write(&root, &format!("routes/{group}/layout.tsx"), &format!("import Counter from '../../components/{group}/Counter';\r\nexport default function Layout() {{ return <section><Counter seed={{{seed}}} label={{{encoded}}} config={{{{ group: '{group}', flags: [true, false], absent: null }}}} /><children /></section>; }}\r\n"));
        write(
            &root,
            &format!("routes/{group}/index.tsx"),
            "export default function Page() { return <p>leaf</p>; }",
        );
    }
    let built = manifest(&root);
    assert!(
        built.static_render_failures.is_empty(),
        "{:?}",
        built.static_render_failures
    );
    for (group, seed) in [("a", 11), ("b", 37)] {
        let route = &built.routes[&format!("/{group}")];
        assert_eq!(route.tier_c.len(), 1, "{route:?}");
        let props = &route.tier_c[0].initial_props;
        assert_eq!(props["seed"], serde_json::json!(f64::from(seed)));
        assert_eq!(props["label"], label);
        assert_eq!(
            props["config"],
            serde_json::json!({"group": group, "flags": [true, false], "absent": null})
        );
        let encoded = serde_json::to_string(route).unwrap();
        let decoded: dom_render_compiler::manifest::schema::RouteManifest =
            serde_json::from_str(&encoded).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
    }
}

#[test]
fn unique_layout_island_props_follow_each_route_without_identity_fields() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "components/Counter.tsx", counter());
    write(dir.path(), "routes/layout.tsx", "import Counter from '../components/Counter';\r\nexport default function Layout({ route }) { return <section><Counter seed={route === '/a' ? 11 : 29} /><children /></section>; }");
    for (route, name) in [("a", "Alpha"), ("b", "Beta")] {
        write(
            dir.path(),
            &format!("routes/{route}.tsx"),
            &format!("export default function {name}() {{ return <p>{route}</p>; }}"),
        );
    }
    for _ in 0..3 {
        let built = manifest(dir.path());
        assert!(
            built.static_render_failures.is_empty(),
            "{:?}",
            built.static_render_failures
        );
        for (route, seed) in [("/a", 11), ("/b", 29)] {
            let nodes = &built.routes[route].tier_c;
            assert_eq!(nodes.len(), 1);
            assert_eq!(nodes[0].component_key, None);
            assert_eq!(
                nodes[0].initial_props["seed"],
                serde_json::json!(f64::from(seed))
            );
            assert!(serde_json::to_value(&nodes[0])
                .unwrap()
                .get("component_key")
                .is_none());
        }
    }
}

#[cfg(windows)]
#[test]
fn verbatim_project_root_beyond_max_path_preserves_nested_layout_islands() {
    use std::os::windows::ffi::OsStrExt;

    let dir = tempfile::tempdir().unwrap();
    let mut root = fs::canonicalize(dir.path()).unwrap();
    assert!(root.to_string_lossy().starts_with(r"\\?\"));
    for index in 0..4 {
        root.push(format!("segment_{index}_{}", "long-path-".repeat(7)));
    }
    assert!(root.as_os_str().encode_wide().count() > 260);
    for group in ["a", "b"] {
        write(&root, &format!("components/{group}/Counter.tsx"), counter());
    }
    write(&root, "routes/layout.tsx", "import Counter from '../components/a/Counter';\r\nexport default function Layout() { return <div data-layer='root-layout'><Counter seed={17} /><children /></div>; }\r\n");
    write(&root, "routes/admin/layout.tsx", "import Counter from '../../components/b/Counter';\r\nexport default function Layout() { return <section data-layer='admin-layout'><Counter seed={43} /><children /></section>; }\r\n");
    write(
        &root,
        "routes/index.tsx",
        "export default function Page() { return <p>home</p>; }",
    );
    write(
        &root,
        "routes/admin/index.tsx",
        "export default function Page() { return <p>admin</p>; }",
    );

    let built = manifest(&root);
    assert!(
        built.static_render_failures.is_empty(),
        "{:?}",
        built.static_render_failures
    );
    let home = &built.routes["/"];
    assert!(home.shell.body_open.contains("root-layout"));
    assert!(!home.shell.body_open.contains("admin-layout"));
    assert_eq!(home.tier_c.len(), 1);
    assert_eq!(
        home.tier_c[0].initial_props["seed"],
        serde_json::json!(17.0)
    );

    let admin = &built.routes["/admin"];
    let html = &admin.shell.body_open;
    assert!(
        html.find("root-layout").unwrap() < html.find("admin-layout").unwrap(),
        "{html}"
    );
    assert_eq!(admin.tier_c.len(), 2, "{admin:?}");
    assert_ne!(
        admin.tier_c[0].placeholder_id,
        admin.tier_c[1].placeholder_id
    );
    for (group, seed) in [("a", 17), ("b", 43)] {
        let component = built
            .components
            .iter()
            .find(|entry| {
                entry
                    .module_path
                    .replace('\\', "/")
                    .ends_with(&format!("components/{group}/Counter.tsx"))
            })
            .unwrap();
        let node = admin
            .tier_c
            .iter()
            .find(|node| node.component_key == Some(component.id))
            .unwrap();
        assert_eq!(
            node.initial_props["seed"],
            serde_json::json!(f64::from(seed))
        );
        assert_eq!(
            html.matches(&format!("id=\"{}\"", node.placeholder_id))
                .count(),
            1,
            "{html}"
        );
    }
}
