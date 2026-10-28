//! Markdown pages end to end through the compiler: strict scan → tier →
//! manifest → the HTML a route ships.
//!
//! `src/markdown.rs` pins the lowering. These pin that nothing between the `.md`
//! file and the page drops it, re-tiers it, or escapes it twice — the class of
//! failure a unit test on the lowering cannot see.

use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::scanner::{ProjectScanner, ScanMode};
use std::path::Path;

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

const LAYOUT: &str = r#"export default function RootLayout({ route }) {
  return <main data-route={route}><children /></main>;
}
"#;

#[test]
fn a_long_markdown_page_ships_as_escaped_tier_a_markup_inside_its_layout() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    write(&src, "routes/layout.tsx", LAYOUT);
    // 🪤 Far past `tier_c_split_min_bytes` (40 KB) of source, on purpose. Today a
    // page cannot trip the size rule, because `WeightEstimator` never reads the
    // source — a component's "weight" is derived from its NAME's length (the
    // fabricated input `TODO.md` names under T4). The day that estimate becomes
    // real source bytes, every docs page past 8 KB leaves Tier A — B re-renders
    // it through QuickJS per request, C ships its text as JavaScript — and this
    // is the test that says so.
    let filler = "Prose that only makes the page long. ".repeat(40) + "\n\n";
    write(
        &src,
        "routes/docs/intro.md",
        &format!(
            "---\ntitle: Intro\n---\n# Intro\n\nWhen a < b && c > d, write `<T,>() => {{}}`.\n\n{}",
            filler.repeat(40)
        ),
    );

    let scanner = ProjectScanner::new();
    let report = scanner
        .scan_directory_with_mode(&src, ScanMode::Strict)
        .expect("a valid page scans under strict mode");
    let compiler = scanner.build_compiler(report.components).with_project_root(dir.path());
    let (manifest, tiers) = compiler
        .optimize_manifest_v2_with_tier_report()
        .expect("manifest");

    let page = tiers
        .components
        .iter()
        .find(|c| c.name == "MdDocsIntro")
        .expect("the page is a component named after its route");
    assert_eq!(page.tier, Tier::A, "{page:?}");

    let route = manifest.routes.get("/docs/intro").expect("the page is a route");
    assert!(route.shell.doctype_and_head.contains("<title>Intro</title>"));
    assert!(
        route.shell.body_open.contains(r#"data-route="/docs/intro""#),
        "the layout wraps the page and knows its route: {}",
        route.shell.body_open
    );

    let html = &route
        .tier_a_root
        .iter()
        .find(|node| node.component_id == "MdDocsIntro")
        .expect("the page renders at Tier A")
        .html;
    // Escaped exactly once: `&lt;`, never `&amp;lt;` and never a bare `<`.
    assert!(html.contains("When a &lt; b &amp;&amp; c &gt; d, write "), "{html}");
    assert!(html.contains("&lt;T,&gt;() =&gt; {}</code>"), "{html}");
    assert!(!html.contains("&amp;lt;"), "double-escaped: {html}");
    assert!(html.contains(r#"<h1 id="intro""#), "{html}");
}

#[test]
fn a_broken_page_fails_a_strict_scan_with_its_file_and_line() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    write(&src, "routes/docs/intro.md", "# Intro\n\nSee [the rest](./rest.md).\n");

    let err = ProjectScanner::new()
        .scan_directory_with_mode(&src, ScanMode::Strict)
        .expect_err("a dead link is a build failure")
        .to_string();
    assert!(err.contains("intro.md:3: links to `./rest.md`, which does not exist"), "{err}");
}
