//! A page of static markup ships no JavaScript.
//!
//! 🔴 Until 2026-10-07 every route's shell carried `runtime.js`, `link-forms.js`,
//! the inject stub and two inline globals — a route whose every component the
//! build had just reported as "Tier A · zero JS → static" included. On such a
//! page the runtime scanned for anchors it would never find.
//!
//! The other direction matters as much: a route that DOES have something for
//! the scripts to act on must keep them. Both are pinned here, through the scan
//! → manifest path `albedo build` runs.

use dom_render_compiler::manifest::schema::{RenderManifestV2, RouteManifest};
use dom_render_compiler::scanner::{ProjectScanner, ScanMode};
use std::path::Path;

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn build(files: &[(&str, &str)]) -> RenderManifestV2 {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    for (rel, source) in files {
        write(&src, rel, source);
    }
    let scanner = ProjectScanner::new();
    let report = scanner
        .scan_directory_with_mode(&src, ScanMode::Strict)
        .expect("scans");
    let compiler = scanner.build_compiler(report.components).with_project_root(dir.path());
    compiler.optimize_manifest_v2().expect("manifest")
}

fn route<'m>(manifest: &'m RenderManifestV2, path: &str) -> &'m RouteManifest {
    manifest
        .routes
        .get(path)
        .unwrap_or_else(|| panic!("no route {path}: {:?}", manifest.routes.keys().collect::<Vec<_>>()))
}

/// Every script a route's shell can carry, as the server writes it out.
fn ships_script(route: &RouteManifest) -> bool {
    route.shell.shim_script.contains("<script")
        || route.shell.doctype_and_head.contains("<script")
        || route.shell.body_open.contains("<script")
}

const LAYOUT: &str = r#"export default function RootLayout() {
  return <div><nav><a href="/">home</a></nav><children /></div>;
}
"#;

#[test]
fn a_route_of_static_markup_ships_no_script() {
    let manifest = build(&[
        ("routes/layout.tsx", LAYOUT),
        (
            "routes/about.tsx",
            "export default function About() {\n  return <main><h1>About</h1><a href=\"/x\">plain link</a></main>;\n}\n",
        ),
    ]);
    let about = route(&manifest, "/about");
    assert!(!ships_script(about), "a static page shipped a script: {:?}", about.shell);
}

#[test]
fn a_route_the_scripts_act_on_keeps_them() {
    let cases = [
        (
            "a <Link>",
            "import { Link } from \"albedo\";\nexport default function About() {\n  return <main><Link href=\"/x\">spa</Link></main>;\n}\n",
        ),
        (
            "a plain POST form",
            "export default function About() {\n  return <form method=\"post\" action=\"/_albedo/auth/password/logout\"><button>out</button></form>;\n}\n",
        ),
        (
            "a client island",
            "import { useState } from \"react\";\nexport default function About() {\n  const [n, setN] = useState(0);\n  return <button onClick={() => setN(n + 1)}>{n}</button>;\n}\n",
        ),
    ];
    for (what, source) in cases {
        let manifest = build(&[("routes/layout.tsx", LAYOUT), ("routes/about.tsx", source)]);
        let about = route(&manifest, "/about");
        assert!(
            about.shell.shim_script.contains("/_albedo/runtime.js")
                && about.shell.shim_script.contains("/_albedo/link-forms.js"),
            "a route with {what} must keep the client runtime: {:?}",
            about.shell
        );
    }
}
