//! T-0045 revision checks for import bindings and overlapping module paths.

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

fn counter(label: &str, default: bool) -> String {
    let export = if default { "export default" } else { "export" };
    format!(
        "import {{ useState }} from 'albedo';\r\n{export} function Counter({{ seed }}) {{ const [n, setN] = useState(seed); return <button onClick={{() => setN(n + 1)}}>{label} {{n}}</button>; }}\r\n"
    )
}

fn assert_island_anchors_and_props(
    built: &RenderManifestV2,
    route_name: &str,
    expected: &[(&str, i32)],
) {
    let route = &built.routes[route_name];
    assert_eq!(route.tier_c.len(), expected.len(), "{route:?}");
    let html: String = std::iter::once(route.shell.body_open.as_str())
        .chain(route.tier_a_root.iter().map(|node| node.html.as_str()))
        .collect();
    assert!(
        !html.contains("did not render"),
        "{html}\n{:?}",
        built.static_render_failures
    );
    for (module, seed) in expected {
        let component = built
            .components
            .iter()
            .find(|entry| entry.module_path.replace('\\', "/") == *module)
            .unwrap_or_else(|| panic!("missing module {module}: {:?}", built.components));
        let node = route
            .tier_c
            .iter()
            .find(|node| node.component_key == Some(component.id))
            .unwrap_or_else(|| panic!("missing island for {module}: {route:?}"));
        assert_eq!(
            html.matches(&format!("id=\"{}\"", node.placeholder_id))
                .count(),
            1,
            "{module} must have exactly one inline anchor: {html}"
        );
        assert_eq!(
            node.initial_props["seed"].as_f64(),
            Some(f64::from(*seed)),
            "props must follow the resolved import for {module}: {node:?}"
        );
    }
}

#[test]
fn duplicate_default_island_names_resolve_two_aliases_in_the_same_parent() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group, true),
        );
    }
    write(dir.path(), "routes/index.tsx", "import First from '../components/a/Counter';\r\nimport Second from '../components/b/Counter';\r\nexport default function Home() { return <main><First seed={13} /><Second seed={37} /></main>; }\r\n");
    let built = manifest(dir.path());
    assert_island_anchors_and_props(
        &built,
        "/",
        &[
            ("components/a/Counter.tsx", 13),
            ("components/b/Counter.tsx", 37),
        ],
    );
}

#[test]
fn duplicate_named_islands_resolve_aliased_named_imports() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group, false),
        );
    }
    write(dir.path(), "routes/index.tsx", "import { Counter as First } from '../components/a/Counter';\nimport { Counter as Second } from '../components/b/Counter';\nexport default function Home() { return <main><First seed={17} /><Second seed={41} /></main>; }\n");
    let built = manifest(dir.path());
    assert_island_anchors_and_props(
        &built,
        "/",
        &[
            ("components/a/Counter.tsx", 17),
            ("components/b/Counter.tsx", 41),
        ],
    );
}

#[test]
fn island_identity_prefers_exact_module_over_another_modules_path_suffix() {
    let dir = tempfile::tempdir().unwrap();
    // Both are project-relative paths. The second also ends with the first;
    // treating every suffix match as identity can steal its anchor and props.
    write(
        dir.path(),
        "components/Counter.tsx",
        &counter("short-path", true),
    );
    write(
        dir.path(),
        "nested/components/Counter.tsx",
        &counter("long-path", true),
    );
    write(dir.path(), "routes/index.tsx", "import First from '../components/Counter';\nimport Second from '../nested/components/Counter';\nexport default function Home() { return <main><First seed={19} /><Second seed={43} /></main>; }\n");
    let built = manifest(dir.path());
    assert_island_anchors_and_props(
        &built,
        "/",
        &[
            ("components/Counter.tsx", 19),
            ("nested/components/Counter.tsx", 43),
        ],
    );
}

#[test]
fn duplicate_layout_islands_capture_props_after_their_layouts_render() {
    let dir = tempfile::tempdir().unwrap();
    for group in ["a", "b"] {
        write(
            dir.path(),
            &format!("components/{group}/Counter.tsx"),
            &counter(group, true),
        );
    }
    write(dir.path(), "routes/layout.tsx", "import Counter from '../components/a/Counter';\r\nexport default function Layout() { return <div><Counter seed={11} /><children /></div>; }\r\n");
    write(dir.path(), "routes/admin/layout.tsx", "import Counter from '../../components/b/Counter';\r\nexport default function Layout() { return <section><Counter seed={29} /><children /></section>; }\r\n");
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
    assert_island_anchors_and_props(
        &built,
        "/admin",
        &[
            ("components/a/Counter.tsx", 11),
            ("components/b/Counter.tsx", 29),
        ],
    );
}

#[test]
fn concurrent_builds_keep_many_shared_island_identities_and_props_isolated() {
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || {
                let dir = tempfile::tempdir().unwrap();
                let mut imports = String::new();
                let mut children = String::new();
                let mut expected = Vec::new();
                for index in 0..32 {
                    let module = format!("components/group-{index}/Counter.tsx");
                    let seed = worker * 1000 + index;
                    write(dir.path(), &module, &counter("isolated", true));
                    imports.push_str(&format!(
                        "import Island{index} from '../components/group-{index}/Counter';\n"
                    ));
                    children.push_str(&format!("<Island{index} seed={{{seed}}} />"));
                    expected.push((module, seed));
                }
                write(dir.path(), "routes/index.tsx", &format!(
                    "{imports}export default function Home() {{ return <main>{children}</main>; }}\n"
                ));
                let expected: Vec<_> = expected
                    .iter()
                    .map(|(module, seed)| (module.as_str(), *seed))
                    .collect();
                assert_island_anchors_and_props(&manifest(dir.path()), "/", &expected);
            });
        }
    });
}
