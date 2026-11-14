//! T-0045: same-name CSS lookup must not alter an unrelated route's appearance.

use dom_render_compiler::manifest::{build_render_manifest_v2, ManifestOptions};
use dom_render_compiler::scanner::ProjectScanner;
use std::fs;
use std::path::Path;

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn duplicate_page_names_do_not_inject_sibling_global_css() {
    let dir = tempfile::tempdir().unwrap();
    for (group, css) in [
        ("a", ".label { color: red; }"),
        ("b", ".label { color: blue; } body { display: none; }"),
    ] {
        write(dir.path(), &format!("routes/{group}.module.css"), css);
        write(
            dir.path(),
            &format!("routes/{group}.tsx"),
            &format!("import styles from './{group}.module.css';\nexport default function Page() {{ return <p className={{styles.label}}>page-{group}</p>; }}"),
        );
    }
    let compiler = ProjectScanner::new().scan_and_build(dir.path()).unwrap();
    let optimized = compiler.optimize().unwrap();
    let manifest = build_render_manifest_v2(
        compiler.graph(),
        &optimized,
        &ManifestOptions {
            project_root: Some(dir.path().to_path_buf()),
            ..ManifestOptions::default()
        },
    );
    let a = &manifest.routes["/a"].shell.doctype_and_head;
    let b = &manifest.routes["/b"].shell.doctype_and_head;
    assert!(a.contains("color: red"), "route a lost its CSS: {a}");
    assert!(b.contains("display: none"), "route b lost its own CSS: {b}");
    assert!(
        !a.contains("display: none"),
        "route a must not be hidden by the unrelated same-named page's CSS: {a}"
    );
}
