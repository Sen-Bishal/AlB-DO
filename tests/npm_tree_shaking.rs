//! Tier C · export-level tree shaking, measured against real packages.
//!
//! The number this exists to move: `import { Check } from "lucide-react"` walks
//! an entry that is `export * from './icons'`, whose index is 854 lines of
//! `export { default as X } from './x'`. Bundling from the entry — correct for
//! the server, which cannot know what an action will reach — lowers all 854 to
//! ship one icon. In a browser that is the "minimal JavaScript" claim being
//! false.
//!
//! These tests are `#[ignore]`d because they need a real `node_modules` on disk
//! (`C:/Development/albedo-corpus`, rebuilt by item 9.0). The unit tests in
//! `bundler::npm` cover the algorithm against synthetic fixtures and run
//! everywhere; **this file is the one that starts from what a real package
//! actually publishes**, which is the discipline the `action_ids` stub and the
//! CSRF seam both cost us before it was a rule.
//!
//! Run with:
//! ```text
//! ALBEDO_SHAKE_CORPUS=C:/Development/albedo-corpus/shadcn-taxonomy \
//!   cargo test --test npm_tree_shaking -- --ignored --nocapture
//! ```

use dom_render_compiler::bundler::client_npm::{
    build_client_npm_graph, client_shake_options, ClientIsland,
};
use dom_render_compiler::bundler::npm::{
    bundle_npm_dependency, bundle_npm_dependency_for_demand, NpmArtifact, ShakeOptions,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

fn corpus() -> Option<PathBuf> {
    let dir = PathBuf::from(
        std::env::var("ALBEDO_SHAKE_CORPUS")
            .unwrap_or_else(|_| "C:/Development/albedo-corpus/shadcn-taxonomy".to_string()),
    );
    dir.join("node_modules").is_dir().then_some(dir)
}

fn demand(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// The headline. One icon must not cost the whole icon set.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn one_lucide_icon_does_not_drag_in_the_whole_set() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let whole = bundle_npm_dependency(&dir, "lucide-react").expect("bundles whole");
    let shaken = bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check"]), &ShakeOptions::default())
        .expect("bundles shaken");

    eprintln!(
        "lucide-react: whole = {} artifacts, shaken for `Check` = {}",
        whole.artifacts.len(),
        shaken.artifacts.len()
    );
    // 🔑 **Bytes, not just files.** A file count is a proxy; what a user
    // downloads is bytes, and reporting the proxy is how a saving gets
    // overstated. Split by origin too — the package's own code versus what its
    // dependencies drag in — because those have different fixes.
    let bytes = |artifacts: &[dom_render_compiler::bundler::npm::NpmArtifact]| -> usize {
        artifacts.iter().map(|a| a.script.len()).sum()
    };
    let own: Vec<_> = shaken
        .artifacts
        .iter()
        .filter(|a| a.key.contains("lucide-react@"))
        .cloned()
        .collect();
    eprintln!(
        "    bytes: whole {} · shaken {} · of which lucide's own {} ({} files)",
        bytes(&whole.artifacts),
        bytes(&shaken.artifacts),
        bytes(&own),
        own.len()
    );
    for artifact in &shaken.artifacts {
        eprintln!("    · {:>8} B  {}", artifact.script.len(), artifact.key);
    }

    assert!(!shaken.taken_whole, "lucide-react declares sideEffects:false");
    assert!(
        shaken.artifacts.len() * 20 < whole.artifacts.len(),
        "shaking must be an order-of-magnitude saving, not a rounding error: \
         {} vs {}",
        shaken.artifacts.len(),
        whole.artifacts.len()
    );

    // The binding must point past both barrels at the file that defines it,
    // which is what makes the barrels unnecessary to emit at all.
    let check = shaken.bindings.get("Check").expect("Check resolved");
    assert!(
        check.record_key.contains("check"),
        "expected the defining record, got {}",
        check.record_key
    );
    assert!(
        !shaken
            .artifacts
            .iter()
            .any(|a| a.key.ends_with("icons/index.js")),
        "the 854-line barrel must not be emitted — nothing points at it"
    );
}

/// Two icons share their helper: the graph is a set, not a per-name copy.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn two_icons_share_one_graph() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let one = bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check"]), &ShakeOptions::default())
        .expect("bundles");
    let two =
        bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check", "ArrowLeft"]), &ShakeOptions::default())
            .expect("bundles");

    eprintln!(
        "lucide-react: 1 icon = {} artifacts, 2 icons = {}",
        one.artifacts.len(),
        two.artifacts.len()
    );
    assert!(
        two.artifacts.len() > one.artifacts.len(),
        "a second icon must add its own file"
    );
    assert!(
        two.artifacts.len() < one.artifacts.len() * 2,
        "the shared helper must not be duplicated per icon"
    );
}

/// `date-fns` is the other shape: a deep `esm/` tree behind a flat barrel.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn one_date_fns_function_does_not_drag_in_the_library() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let whole = bundle_npm_dependency(&dir, "date-fns").expect("bundles whole");
    let shaken = bundle_npm_dependency_for_demand(&dir, "date-fns", &demand(&["format"]), &ShakeOptions::default())
        .expect("bundles shaken");

    eprintln!(
        "date-fns: whole = {} artifacts, shaken for `format` = {}",
        whole.artifacts.len(),
        shaken.artifacts.len()
    );
    assert!(
        shaken.artifacts.len() < whole.artifacts.len(),
        "shaking must remove something"
    );
}

/// A package that does **not** declare `sideEffects: false` is taken whole, and
/// says so. Silently dropping a file that installs a polyfill on import is the
/// failure mode this refusal exists to prevent.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn a_package_without_a_side_effect_declaration_is_taken_whole() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    // `clsx` ships no `sideEffects` field.
    let shaken =
        bundle_npm_dependency_for_demand(&dir, "clsx", &demand(&["default"]), &ShakeOptions::default()).expect("bundles");
    assert!(
        shaken.taken_whole,
        "without the declaration the package must not be shaken"
    );
    assert!(shaken.bindings.contains_key("default"));
}

/// Asking for a name the package does not export fails loudly at build, rather
/// than shipping a chunk whose binding is `undefined` at runtime.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn an_unexported_name_is_a_build_error() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let err = bundle_npm_dependency_for_demand(
        &dir,
        "lucide-react",
        &demand(&["NoSuchIconExistsAnywhere"]),
        &ShakeOptions::default(),
    )
    .expect_err("an unexported name must not resolve");
    let message = err.to_string();
    assert!(
        message.contains("NoSuchIconExistsAnywhere"),
        "the error must name the binding: {message}"
    );
}


// ── Tier C · Phase 2 ────────────────────────────────────────────────────
//
// Phase 1 shook `lucide-react` from 848 231 B to 156 991 B for one icon and
// then measured **3 507 B of that as lucide's own code**. The remaining 97.8%
// was `react` (both the development *and* the production build), `prop-types`,
// `react-is` and `object-assign` — all of it dragged in by the *package's* own
// `import … from 'react'`, which, unlike a project's, bound to nothing.
//
// These are the tests that hold that number down. They start from what the
// packages actually publish, which is the rule the `action_ids` stub cost us.

fn total_bytes(artifacts: &[NpmArtifact]) -> usize {
    artifacts.iter().map(|artifact| artifact.script.len()).sum()
}

/// The Phase 2 headline: externalising React and folding `NODE_ENV` must take
/// the shaken payload from ~157 kB to the order of lucide's own code.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn externalising_react_collapses_the_client_payload() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let server = bundle_npm_dependency_for_demand(
        &dir,
        "lucide-react",
        &demand(&["Check"]),
        &ShakeOptions::default(),
    )
    .expect("server-shaped bundle");
    let client = bundle_npm_dependency_for_demand(
        &dir,
        "lucide-react",
        &demand(&["Check"]),
        &client_shake_options(),
    )
    .expect("client-shaped bundle");

    eprintln!(
        "lucide-react `Check`: server-shaped {} B / {} files → client-shaped {} B / {} files",
        total_bytes(&server.artifacts),
        server.artifacts.len(),
        total_bytes(&client.artifacts),
        client.artifacts.len()
    );
    for artifact in &client.artifacts {
        eprintln!("    · {:>8} B  {}", artifact.script.len(), artifact.key);
    }

    let keys: Vec<&str> = client.artifacts.iter().map(|a| a.key.as_str()).collect();
    assert!(
        !keys.iter().any(|key| key.starts_with("npm:react@")),
        "react must be externalised, not bundled: {keys:?}"
    );
    assert!(
        !keys.iter().any(|key| key.contains("react.development")),
        "the development build must never reach a browser: {keys:?}"
    );
    assert!(
        !keys.iter().any(|key| key.starts_with("npm:react-is@")),
        "`react-is` arrives only through prop-types' development arm, which \
         NODE_ENV folding removes: {keys:?}"
    );
    assert!(
        total_bytes(&client.artifacts) * 4 < total_bytes(&server.artifacts),
        "externalisation must be a step change, not a trim: {} vs {}",
        total_bytes(&client.artifacts),
        total_bytes(&server.artifacts)
    );
}

/// `prop-types` is the second `NODE_ENV` fork, and the one that proves the fold
/// is doing the work rather than externalisation alone: nothing about it is a
/// host module, yet its 20 kB checking factory must not ship.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn the_node_env_fold_drops_the_development_arm() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let client = bundle_npm_dependency_for_demand(
        &dir,
        "lucide-react",
        &demand(&["Check"]),
        &client_shake_options(),
    )
    .expect("client-shaped bundle");

    let keys: Vec<&str> = client.artifacts.iter().map(|a| a.key.as_str()).collect();
    if keys.iter().any(|key| key.starts_with("npm:prop-types@")) {
        assert!(
            !keys.iter().any(|key| key.contains("factoryWithTypeCheckers")),
            "the checking factory is the development arm: {keys:?}"
        );
    }
}

/// The whole-build view: what a page actually downloads, per package.
#[test]
#[ignore = "needs a real node_modules; see the module docs"]
fn the_client_graph_emits_one_chunk_per_package() {
    let Some(dir) = corpus() else {
        eprintln!("SKIP: no corpus on disk");
        return;
    };

    let island = r#"
        import { Check, ArrowLeft } from "lucide-react";
        import clsx from "clsx";
        export default function Toolbar() {
            return <div className={clsx("bar")}><Check /><ArrowLeft /></div>;
        }
    "#;
    let graph = build_client_npm_graph(
        &dir,
        &[ClientIsland {
            module_path: "src/components/Toolbar.tsx",
            source: island,
        }],
    );

    for failure in graph.failures() {
        eprintln!(
            "    FAILED {} · {}: {}",
            failure.module_path, failure.specifier, failure.reason
        );
    }
    let total: usize = graph.chunks().iter().map(|chunk| chunk.bytes()).sum();
    eprintln!("client npm: {} B across {} chunks", total, graph.chunks().len());
    for chunk in graph.chunks() {
        eprintln!("    · {:>8} B  {}  {}", chunk.bytes(), chunk.package, chunk.url);
    }

    assert!(graph.failures().is_empty(), "nothing should fail to resolve");
    assert!(
        !graph.chunks().is_empty(),
        "two packages were imported; chunks must be emitted"
    );
    assert!(
        graph
            .chunks()
            .iter()
            .all(|chunk| chunk.package != "react"),
        "react is a host module and must never be a chunk"
    );

    let bindings = graph
        .bindings_for("src/components/Toolbar.tsx")
        .expect("the island has npm bindings");
    let check = bindings
        .get("lucide-react", "Check")
        .expect("Check is bound");
    assert!(
        check.record_key.contains("check"),
        "bound past the barrel to the defining record, got {}",
        check.record_key
    );

    // Content-hashed and stable: the same demand must produce the same URL, or
    // every deploy invalidates every cache for no reason.
    let again = build_client_npm_graph(
        &dir,
        &[ClientIsland {
            module_path: "src/components/Toolbar.tsx",
            source: island,
        }],
    );
    let first: Vec<&str> = graph.chunks().iter().map(|c| c.url.as_str()).collect();
    let second: Vec<&str> = again.chunks().iter().map(|c| c.url.as_str()).collect();
    assert_eq!(first, second, "chunk URLs must be deterministic");
}
