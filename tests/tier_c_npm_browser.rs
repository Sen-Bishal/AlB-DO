//! Tier C · Phase 2 — npm reaches the browser, end to end, on synthetic
//! packages that run everywhere.
//!
//! The corpus tests in `tests/npm_tree_shaking.rs` measure real packages and are
//! `#[ignore]`d because they need a `node_modules` on disk. **These do not**:
//! they build a package in a tempdir, run the whole pipeline over it — demand
//! scan → shake → chunk → island lowering — and assert on the JavaScript that
//! would be served. The rule that produced them is the one the `action_ids` stub
//! cost us: *at least one test per mechanism must start from what the thing
//! actually produces*, and for a browser bundle that is the emitted script text.

use dom_render_compiler::bundler::client_npm::{
    build_browser_npm_runtime_script, build_client_npm_graph, ClientIsland,
    CLIENT_NPM_CHUNK_PREFIX,
};
use dom_render_compiler::runtime::quickjs_engine::compile_client_island_module_with_npm;
use std::collections::HashMap;
use std::path::Path;

/// A `sideEffects: false` package whose component imports `react` — the exact
/// shape `lucide-react` publishes, and the one that used to drag a second copy
/// of React into every page.
fn write_icon_package(root: &Path) {
    let pkg = root.join("node_modules").join("icons");
    std::fs::create_dir_all(pkg.join("esm")).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "icons", "version": "1.0.0", "module": "esm/index.js",
             "sideEffects": false }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("esm").join("index.js"),
        "export { default as Check } from './check';\n\
         export { default as Cross } from './cross';",
    )
    .unwrap();
    for icon in ["check", "cross"] {
        std::fs::write(
            pkg.join("esm").join(format!("{icon}.js")),
            format!(
                "import {{ forwardRef, createElement }} from 'react';\n\
                 const {icon} = forwardRef(function (props, ref) {{\n\
                 \x20 return createElement('svg', {{ ref: ref, 'data-icon': '{icon}' }});\n\
                 }});\n\
                 export default {icon};"
            ),
        )
        .unwrap();
    }
}

fn island(source: &str) -> ClientIsland<'_> {
    ClientIsland {
        module_path: "src/components/Toolbar.tsx",
        source,
    }
}

const TOOLBAR: &str = r#"
    import { Check } from "icons";
    export default function Toolbar() {
        return <div><Check /></div>;
    }
"#;

/// The headline: a package's `import 'react'` costs **zero bytes**, because the
/// host provides it. Without this, every React component library ships a second
/// React — the finding that made up 97.8% of Phase 1's remaining payload.
#[test]
fn a_packages_react_import_never_reaches_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let graph = build_client_npm_graph(dir.path(), &[island(TOOLBAR)]);
    assert!(
        graph.failures().is_empty(),
        "nothing should fail: {:?}",
        graph.failures()
    );

    let all: String = graph
        .chunks()
        .iter()
        .map(|chunk| chunk.script.as_str())
        .collect();
    assert!(
        !all.contains("npm:react@"),
        "no react record may be registered by a chunk"
    );
    assert!(
        all.contains("albedo:host/react"),
        "the icon's react import must resolve to the host record"
    );
    assert_eq!(
        graph.chunks().len(),
        1,
        "one package in, one chunk out: {:?}",
        graph.chunks().iter().map(|c| &c.package).collect::<Vec<_>>()
    );
}

/// Only the demanded icon ships, and the barrel that named it does not.
#[test]
fn only_the_demanded_export_is_chunked() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let graph = build_client_npm_graph(dir.path(), &[island(TOOLBAR)]);
    let chunk = &graph.chunks()[0];
    assert!(chunk.script.contains("esm/check.js"), "{}", chunk.script);
    assert!(
        !chunk.script.contains("esm/cross.js"),
        "an undemanded sibling must not ship"
    );
    assert!(
        !chunk.script.contains("esm/index.js"),
        "the barrel is resolved through, never emitted"
    );
}

/// The island binds against the chunk's record table rather than inlining the
/// package — which is the whole design, and the thing the depth-8 inliner could
/// never have done.
#[test]
fn the_island_binds_to_the_record_table() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let graph = build_client_npm_graph(dir.path(), &[island(TOOLBAR)]);
    let bindings = graph
        .bindings_for("src/components/Toolbar.tsx")
        .expect("the island has bindings");

    let compiled = compile_client_island_module_with_npm(
        "src/components/Toolbar.tsx",
        TOOLBAR,
        7,
        &HashMap::new(),
        bindings,
    )
    .expect("the island compiles");

    assert!(
        compiled.contains("__albedo_require_record"),
        "the import must lower to a record lookup: {compiled}"
    );
    assert!(
        compiled.contains("esm/check.js"),
        "bound past the barrel to the defining record: {compiled}"
    );
    assert!(
        !compiled.contains("forwardRef"),
        "the package body must NOT be inlined into the island: {compiled}"
    );
}

/// The refusal is preserved for an island whose package did not resolve: no
/// binding, no silent `undefined`, a named error.
#[test]
fn an_unresolvable_package_still_refuses_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
        import { Thing } from "not-installed";
        export default function Widget() { return <Thing />; }
    "#;

    let graph = build_client_npm_graph(dir.path(), &[island(source)]);
    assert_eq!(graph.failures().len(), 1);
    assert_eq!(graph.failures()[0].specifier, "not-installed");
    assert!(graph.chunks().is_empty());

    let err = compile_client_island_module_with_npm(
        "src/components/Toolbar.tsx",
        source,
        7,
        &HashMap::new(),
        &Default::default(),
    )
    .expect_err("an unbundled package must not compile silently");
    assert!(
        err.to_string().contains("not-installed"),
        "the error must name the specifier: {err}"
    );
}

/// A package importing a name the host does not have is a **build** error
/// naming what is available — not an `undefined` discovered at first render.
#[test]
fn a_host_name_that_does_not_exist_is_a_build_error() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("node_modules").join("fancy");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "fancy", "version": "1.0.0", "module": "index.js",
             "sideEffects": false }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        "import { useSyncExternalStore } from 'react';\n\
         export const useStore = useSyncExternalStore;",
    )
    .unwrap();

    let source = r#"
        import { useStore } from "fancy";
        export default function Widget() { return <div>{useStore()}</div>; }
    "#;
    let graph = build_client_npm_graph(dir.path(), &[island(source)]);
    assert_eq!(graph.failures().len(), 1, "{:?}", graph.failures());
    let reason = &graph.failures()[0].reason;
    assert!(reason.contains("useSyncExternalStore"), "{reason}");
    assert!(
        reason.contains("useState"),
        "the error must list what the host does provide: {reason}"
    );
}

/// `react-dom` is refused with its reason rather than stubbed into a runtime
/// throw, because a blank island is a worse answer than a failed build.
#[test]
fn a_package_reaching_for_react_dom_is_refused_at_build() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("node_modules").join("portalish");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::write(
        pkg.join("package.json"),
        r#"{ "name": "portalish", "version": "1.0.0", "module": "index.js",
             "sideEffects": false }"#,
    )
    .unwrap();
    std::fs::write(
        pkg.join("index.js"),
        "import { createPortal } from 'react-dom';\nexport const portal = createPortal;",
    )
    .unwrap();

    let graph = build_client_npm_graph(
        dir.path(),
        &[island(
            r#"import { portal } from "portalish";
               export default function W() { return <div>{portal}</div>; }"#,
        )],
    );
    assert_eq!(graph.failures().len(), 1);
    assert!(
        graph.failures()[0].reason.contains("createPortal"),
        "{}",
        graph.failures()[0].reason
    );
}

/// Two islands importing the same package share one chunk, and a package
/// reached from two others is emitted once for the whole build.
#[test]
fn packages_are_deduplicated_across_islands() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let one = r#"import { Check } from "icons"; export default () => <Check />;"#;
    let two = r#"import { Cross } from "icons"; export default () => <Cross />;"#;
    let graph = build_client_npm_graph(
        dir.path(),
        &[
            ClientIsland {
                module_path: "a.tsx",
                source: one,
            },
            ClientIsland {
                module_path: "b.tsx",
                source: two,
            },
        ],
    );

    assert_eq!(graph.chunks().len(), 1, "one package, one chunk");
    let chunk = &graph.chunks()[0];
    assert!(chunk.script.contains("esm/check.js"));
    assert!(chunk.script.contains("esm/cross.js"));
    // Both islands point at the same URL, so the browser fetches it once.
    assert_eq!(
        graph.chunk_urls_for(["a.tsx", "b.tsx"]),
        vec![chunk.url.as_str()]
    );
}

/// A chunk URL carries a hash of its exact bytes, which is what makes the
/// `immutable` cache header the handler sets honest.
#[test]
fn chunk_urls_are_content_addressed_and_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    write_icon_package(dir.path());

    let first = build_client_npm_graph(dir.path(), &[island(TOOLBAR)]);
    let again = build_client_npm_graph(dir.path(), &[island(TOOLBAR)]);
    assert_eq!(first.chunks()[0].url, again.chunks()[0].url);
    assert!(first.chunks()[0].url.starts_with(CLIENT_NPM_CHUNK_PREFIX));

    // A different demand is different bytes, and must be a different URL — a
    // stale immutable cache is the one failure this scheme must not have.
    let wider = build_client_npm_graph(
        dir.path(),
        &[island(
            r#"import { Check, Cross } from "icons";
               export default () => <div><Check /><Cross /></div>;"#,
        )],
    );
    assert_ne!(first.chunks()[0].url, wider.chunks()[0].url);
}

/// The generated browser runtime has to *parse*. It is assembled from Rust
/// string fragments, and a typo in one of them would otherwise surface as a
/// blank page with a syntax error in the console.
#[test]
fn the_browser_npm_runtime_is_valid_javascript() {
    use swc_common::{sync::Lrc, FileName, SourceMap};
    use swc_ecma_parser::{EsSyntax, Parser, StringInput, Syntax};

    let script = build_browser_npm_runtime_script();
    let source_map: Lrc<SourceMap> = Lrc::default();
    let file = source_map.new_source_file(
        FileName::Custom("npm-runtime.js".to_string()).into(),
        script.clone(),
    );
    let parsed = Parser::new(
        Syntax::Es(EsSyntax::default()),
        StringInput::from(&*file),
        None,
    )
    .parse_script();
    assert!(
        parsed.is_ok(),
        "the emitted runtime must parse: {:?}",
        parsed.err()
    );
}

/// The linker the browser gets and the linker QuickJS installs are the same
/// string. Two hand-written copies of a module linker is the *"three paint-rule
/// implementations"* shape this codebase has already paid for once.
#[test]
fn the_browser_and_server_linkers_are_one_implementation() {
    let browser = build_browser_npm_runtime_script();
    let linker = dom_render_compiler::runtime::quickjs_engine::npm_record_linker_script();
    assert!(
        browser.contains(linker.trim()),
        "the browser runtime must embed the shared linker verbatim"
    );
}
