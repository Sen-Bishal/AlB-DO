//! T-0045 adversarial checks at the scanner-to-manifest seam.

use dom_render_compiler::manifest::schema::{
    qualified_component_name, split_qualified_component_name, RenderManifestV2,
};
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
    let result = compiler.optimize().unwrap();
    build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions {
            project_root: Some(root.to_path_buf()),
            ..ManifestOptions::default()
        },
    )
}

fn counter(label: &str) -> String {
    format!(
        "import {{ useState }} from 'albedo';\nexport default function Counter({{ seed }}) {{ const [n, setN] = useState(seed); return <button onClick={{() => setN(n + 1)}}>{label} {{n}}</button>; }}\n"
    )
}

#[test]
fn duplicate_layouts_keep_deep_order_with_crlf_and_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("project with spaces");
    for (folder, label) in [
        ("", "outer"),
        ("admin/", "middle"),
        ("admin/tools/", "inner"),
    ] {
        write(
            &root,
            &format!("routes/{folder}layout.tsx"),
            &format!("export default function Layout() {{\r\nreturn <div data-layer='{label}'><children /></div>;\r\n}}\r\n"),
        );
    }
    write(
        &root,
        "routes/index.tsx",
        "export default function Page() { return <p>home-leaf</p>; }",
    );
    write(
        &root,
        "routes/admin/tools/index.tsx",
        "export default function Page() { return <p>tools-leaf</p>; }",
    );
    for _ in 0..4 {
        let built = manifest(&root);
        let nested = &built.routes["/admin/tools"].shell.body_open;
        let positions: Vec<_> = ["outer", "middle", "inner"]
            .iter()
            .map(|label| {
                nested
                    .find(label)
                    .unwrap_or_else(|| panic!("missing {label}: {nested}"))
            })
            .collect();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{nested}"
        );
        assert_eq!(
            built.routes["/admin/tools"].layout_chain,
            ["Layout", "Layout", "Layout"]
        );
        let home = &built.routes["/"].shell.body_open;
        assert!(home.contains("outer"), "{home}");
        assert!(
            !home.contains("middle") && !home.contains("inner"),
            "{home}"
        );
        assert!(
            built.static_render_failures.is_empty(),
            "{:?}",
            built.static_render_failures
        );
    }
}

#[test]
fn duplicate_boundaries_and_metadata_carry_their_exact_module_ids() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(dir.path(), &format!("routes/{group}/[id].tsx"), "export function generateMetadata({ params }) { return { title: params.id }; }\nexport default function Page({ params }) { return <p>{params.id}</p>; }");
        for (file, name) in [("error", "Error"), ("loading", "Loading")] {
            write(
                dir.path(),
                &format!("routes/{group}/{file}.tsx"),
                &format!("export default function {name}() {{ return <p>{group}-{file}</p>; }}"),
            );
        }
    }
    let built = manifest(dir.path());
    for group in ["a", "b"] {
        let route = &built.routes[&format!("/{group}/[id]")];
        for (stored, file, name) in [
            (route.error_component.as_deref(), "error", "Error"),
            (route.loading_component.as_deref(), "loading", "Loading"),
            (route.dynamic_metadata.as_deref(), "[id]", "Page"),
        ] {
            let component = built
                .components
                .iter()
                .find(|entry| {
                    entry
                        .module_path
                        .replace('\\', "/")
                        .ends_with(&format!("routes/{group}/{file}.tsx"))
                        && entry.name == name
                })
                .unwrap();
            let stored = stored.unwrap();
            assert_eq!(stored, qualified_component_name(name, Some(component.id)));
            assert_eq!(
                split_qualified_component_name(stored),
                (name, Some(component.id))
            );
        }
    }
}

#[test]
fn same_named_islands_on_separate_routes_keep_distinct_chunks_and_edges() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group),
        );
        write(dir.path(), &format!("routes/{group}.tsx"), &format!("import Counter from '../components/{group}/Counter';\nexport default function Page() {{ return <main><Counter seed={{7}} /></main>; }}"));
    }
    let built = manifest(dir.path());
    let mut paths = Vec::new();
    for group in ["a", "b"] {
        let route = &built.routes[&format!("/{group}")];
        assert_eq!(route.tier_c.len(), 1, "{route:?}");
        let node = &route.tier_c[0];
        let own = built
            .components
            .iter()
            .find(|entry| {
                entry
                    .module_path
                    .replace('\\', "/")
                    .ends_with(&format!("components/{group}/Counter.tsx"))
            })
            .unwrap();
        let page = built
            .components
            .iter()
            .find(|entry| {
                entry
                    .module_path
                    .replace('\\', "/")
                    .ends_with(&format!("routes/{group}.tsx"))
            })
            .unwrap();
        assert_eq!(page.dependencies, [own.id]);
        assert_eq!(node.component_key, Some(own.id));
        assert_eq!(
            built.assets.chunks[&qualified_component_name("Counter", Some(own.id))],
            node.bundle_path
        );
        paths.push(node.bundle_path.clone());
    }
    assert_ne!(paths[0], paths[1]);
}

fn side_by_side_island_manifest() -> RenderManifestV2 {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group),
        );
        let (name, seed) = if group == "a" {
            ("PanelA", 11)
        } else {
            ("PanelB", 29)
        };
        write(dir.path(), &format!("components/{group}/{name}.tsx"), &format!("import Counter from './Counter';\nexport default function {name}() {{ return <section><Counter seed={{{seed}}} /></section>; }}"));
    }
    write(dir.path(), "routes/index.tsx", "import PanelA from '../components/a/PanelA';\nimport PanelB from '../components/b/PanelB';\nexport default function Home() { return <main><PanelA /><PanelB /></main>; }");
    manifest(dir.path())
}

#[test]
fn two_same_named_islands_on_one_route_each_have_one_inline_anchor_and_own_props() {
    let built = side_by_side_island_manifest();
    let route = &built.routes["/"];
    assert_eq!(route.tier_c.len(), 2, "{route:?}");
    let html: String = std::iter::once(route.shell.body_open.as_str())
        .chain(route.tier_a_root.iter().map(|node| node.html.as_str()))
        .collect();
    assert!(
        !html.contains("did not render"),
        "the static wrappers must render before anchor checks: {html}\n{:?}",
        built.static_render_failures
    );
    for node in &route.tier_c {
        assert_eq!(
            html.matches(&format!("id=\"{}\"", node.placeholder_id))
                .count(),
            1,
            "each module needs its own anchor: {node:?}\n{html}"
        );
        let component = built
            .components
            .iter()
            .find(|entry| Some(entry.id) == node.component_key)
            .unwrap();
        let seed = if component
            .module_path
            .replace('\\', "/")
            .contains("components/a/")
        {
            11
        } else {
            29
        };
        assert_eq!(
            node.initial_props["seed"].as_f64(),
            Some(f64::from(seed)),
            "{node:?}"
        );
    }
}

#[test]
fn two_same_named_islands_capture_props_for_their_own_module() {
    let built = side_by_side_island_manifest();
    let route = &built.routes["/"];
    assert_eq!(route.tier_c.len(), 2, "{route:?}");
    let mut actual = std::collections::BTreeMap::new();
    for node in &route.tier_c {
        let component = built
            .components
            .iter()
            .find(|entry| Some(entry.id) == node.component_key)
            .unwrap();
        actual.insert(
            component.module_path.replace('\\', "/"),
            node.initial_props["seed"].clone(),
        );
    }
    assert_eq!(
        actual,
        std::collections::BTreeMap::from([
            (
                "components/a/Counter.tsx".to_string(),
                serde_json::json!(11.0)
            ),
            (
                "components/b/Counter.tsx".to_string(),
                serde_json::json!(29.0)
            ),
        ]),
        "each island must hydrate from the props passed to its own module"
    );
}

#[test]
fn same_named_islands_in_root_and_nested_layouts_have_distinct_anchors() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group),
        );
    }
    write(dir.path(), "routes/layout.tsx", "import Counter from '../components/a/Counter';\nexport default function Layout() { return <div><Counter seed={11} /><children /></div>; }");
    write(dir.path(), "routes/admin/layout.tsx", "import Counter from '../../components/b/Counter';\nexport default function Layout() { return <section><Counter seed={29} /><children /></section>; }");
    write(
        dir.path(),
        "routes/index.tsx",
        "export default function Home() { return <p>home</p>; }",
    );
    write(
        dir.path(),
        "routes/admin/index.tsx",
        "export default function Admin() { return <p>admin</p>; }",
    );
    let built = manifest(dir.path());
    let home = &built.routes["/"];
    assert_eq!(home.tier_c.len(), 1, "{home:?}");
    let admin = &built.routes["/admin"];
    assert_eq!(admin.tier_c.len(), 2, "{admin:?}");
    for node in &admin.tier_c {
        assert_eq!(
            admin
                .shell
                .body_open
                .matches(&format!("id=\"{}\"", node.placeholder_id))
                .count(),
            1,
            "each layout's module needs its own anchor: {node:?}\n{}",
            admin.shell.body_open
        );
    }
}

#[test]
fn same_named_page_css_is_not_dropped_and_unique_boundaries_stay_unqualified() {
    let dir = tempfile::tempdir().unwrap();
    for (group, color) in [("a", "red"), ("b", "blue")] {
        write(
            dir.path(),
            &format!("routes/{group}.module.css"),
            &format!(".label {{ color: {color}; }}"),
        );
        write(dir.path(), &format!("routes/{group}.tsx"), &format!("import styles from './{group}.module.css';\nexport default function Page() {{ return <p className={{styles.label}}>{group}</p>; }}"));
    }
    write(
        dir.path(),
        "routes/error.tsx",
        "export default function RootError() { return <p>error</p>; }",
    );
    write(
        dir.path(),
        "routes/loading.tsx",
        "export default function RootLoading() { return <p>loading</p>; }",
    );
    let built = manifest(dir.path());
    for (group, color) in [("a", "red"), ("b", "blue")] {
        let route = &built.routes[&format!("/{group}")];
        assert!(
            route.shell.doctype_and_head.contains(color),
            "{}",
            route.shell.doctype_and_head
        );
        assert_eq!(route.error_component.as_deref(), Some("RootError"));
        assert_eq!(route.loading_component.as_deref(), Some("RootLoading"));
        let encoded = serde_json::to_string(route).unwrap();
        let decoded =
            serde_json::from_str::<dom_render_compiler::manifest::schema::RouteManifest>(&encoded)
                .unwrap();
        assert_eq!(encoded, serde_json::to_string(&decoded).unwrap());
    }
}
