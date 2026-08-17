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

use dom_render_compiler::bundler::npm::{bundle_npm_dependency, bundle_npm_dependency_for_demand};
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
    let shaken = bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check"]))
        .expect("bundles shaken");

    eprintln!(
        "lucide-react: whole = {} artifacts, shaken for `Check` = {}",
        whole.artifacts.len(),
        shaken.artifacts.len()
    );
    // Printed, not just counted: a count that is smaller can still be wrong, and
    // the only way to see the walk pulled nothing gratuitous is to read what it
    // pulled.
    for artifact in &shaken.artifacts {
        eprintln!("    · {}", artifact.key);
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

    let one = bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check"]))
        .expect("bundles");
    let two =
        bundle_npm_dependency_for_demand(&dir, "lucide-react", &demand(&["Check", "ArrowLeft"]))
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
    let shaken = bundle_npm_dependency_for_demand(&dir, "date-fns", &demand(&["format"]))
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
        bundle_npm_dependency_for_demand(&dir, "clsx", &demand(&["default"])).expect("bundles");
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
    )
    .expect_err("an unexported name must not resolve");
    let message = err.to_string();
    assert!(
        message.contains("NoSuchIconExistsAnywhere"),
        "the error must name the binding: {message}"
    );
}
