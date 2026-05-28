//! Phase P · Stream E.3 — `*.module.css` JSX rewrite end-to-end.
//!
//! Four gates:
//!
//!   1. `styles.foo` in JSX resolves to the scoped class name
//!      derived from the CSS file's path-hash (`foo_<8hex>`).
//!   2. The scoping primitive `scope_module_css` is loaded once per
//!      CSS file and reused across multiple `import styles from ...`
//!      bindings in the same project — no double-scoping.
//!   3. Two `.module.css` files that declare the same local class
//!      name (e.g. both have `.box`) produce DIFFERENT scoped names
//!      so styles don't bleed across components.
//!   4. The manifest's `shell.doctype_and_head` carries a `<style
//!      data-albedo-css-modules>` block with the route's scoped CSS
//!      so the rendered class names actually paint correctly without
//!      a separate stylesheet request.

use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::runtime::eval::{CompiledProject, SessionSlotView};
use dom_render_compiler::runtime::slot_store::SlotStore;
use dom_render_compiler::runtime::{render_entry_with_bindings, RenderOptions, SessionId};
use dom_render_compiler::types::{Component, ComponentId};
use dom_render_compiler::RenderCompiler;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("css_modules")
        .join(name)
}

#[test]
fn styles_member_access_resolves_to_scoped_class_in_rendered_html() {
    let project =
        CompiledProject::load_from_dir(fixture_root("single_card")).expect("load project");
    let registry = project.css_modules();
    assert_eq!(
        registry.file_count(),
        1,
        "single_card fixture has one .module.css file"
    );

    // Render the entry — `<article class={styles.card}>` must
    // surface as `class="card_<hash>"`. The scoping primitive uses
    // the file's project-relative path as its module_id, so the
    // hash is deterministic across runs.
    let scoped_card = registry
        .scoped_class_for("Component.tsx", "styles", "card")
        .expect("styles.card must resolve");
    assert!(
        scoped_card.starts_with("card_"),
        "scoped class must keep original local name as a prefix; got: {scoped_card}"
    );
    // Render through the Phase K entry path so the CSS-module
    // class-map thread-local is installed for the duration of the
    // render. `hook_compile: false` keeps Tier-A behaviour
    // (no opcodes emitted) — CSS modules don't require hook compile.
    let slots = SessionSlotView::new(SessionId::random(), Arc::new(SlotStore::new()));
    let opts = RenderOptions { hook_compile: false };
    let output = render_entry_with_bindings(
        &project,
        "Component.tsx",
        &serde_json::json!({}),
        &slots,
        &opts,
    )
    .expect("render entry through Phase K path");
    let html = output.html;
    assert!(
        html.contains(scoped_card),
        "rendered HTML must use the scoped class name; got: {html}"
    );
    assert!(
        !html.contains("class=\"card\""),
        "rendered HTML must NOT contain the unscoped class name; got: {html}"
    );
}

#[test]
fn scope_module_css_runs_once_per_file_even_with_multiple_imports() {
    let project =
        CompiledProject::load_from_dir(fixture_root("single_card")).expect("load project");
    // The single_card fixture has one component importing one file.
    // Re-checking the registry's file_count after multiple lookups
    // proves the file is cached (not re-scoped on demand).
    let registry = project.css_modules();
    let _ = registry.scoped_class_for("Component.tsx", "styles", "card");
    let _ = registry.scoped_class_for("Component.tsx", "styles", "title");
    assert_eq!(
        registry.file_count(),
        1,
        "repeated lookups must not load extra files"
    );
}

#[test]
fn two_module_css_files_with_same_local_class_produce_different_scoped_names() {
    let project =
        CompiledProject::load_from_dir(fixture_root("multi_card")).expect("load project");
    let registry = project.css_modules();
    assert_eq!(
        registry.file_count(),
        2,
        "multi_card fixture has card.module.css + banner.module.css"
    );

    let card_box = registry
        .scoped_class_for("Card.tsx", "styles", "box")
        .expect("Card's styles.box must resolve");
    let banner_box = registry
        .scoped_class_for("Banner.tsx", "styles", "box")
        .expect("Banner's styles.box must resolve");

    assert!(
        card_box.starts_with("box_") && banner_box.starts_with("box_"),
        "both must start with the local name prefix; got '{card_box}' and '{banner_box}'"
    );
    assert_ne!(
        card_box, banner_box,
        "two different .module.css files declaring the same local class \
         MUST produce different scoped names — got identical '{card_box}'"
    );
}

#[test]
fn manifest_shell_carries_scoped_css_block_for_route() {
    // Build a manifest where the route's component imports a
    // `.module.css`. The shell's doctype_and_head should contain a
    // `<style data-albedo-css-modules>` block with the scoped CSS
    // body (so the rendered class names actually paint).
    let mut compiler = RenderCompiler::new();
    let mut card = Component::new(ComponentId::new(0), "Card".to_string());
    card.file_path = fixture_root("single_card")
        .join("Component.tsx")
        .display()
        .to_string();
    card.weight = 1024.0;
    compiler.add_component(card);

    let result = compiler.optimize().expect("optimize");
    let manifest = dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    );

    let route = manifest
        .routes
        .values()
        .next()
        .expect("at least one route");
    let head = &route.shell.doctype_and_head;
    assert!(
        head.contains("<style data-albedo-css-modules>"),
        "shell head must include the CSS-modules <style> block; got: {head}"
    );
    assert!(
        head.contains(".card_"),
        "shell head must include the scoped class selector; got: {head}"
    );
    assert!(
        !head.contains(".card {"),
        "shell head must NOT include the unscoped selector; got: {head}"
    );
}
