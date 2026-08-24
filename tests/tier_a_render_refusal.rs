//! A Tier-A render that fails must be **named and absent**, never scraped.
//!
//! ## What was broken
//!
//! `ManifestBuilder::render_static` treated a failed render as a formatting
//! problem. When `render_static_component_html` returned `None` it called
//! `best_effort_static_content`, which read the component's own `.tsx` off disk,
//! kept every character that fell outside a `<`…`>` pair, and shipped the first
//! 160 of them inside a `<section data-albedo-static>`.
//!
//! On a real route — `<main><h1>asChild</h1><SlotDemo /></main>`, where
//! `SlotDemo` imported `@radix-ui/react-slot` — that produced exactly this:
//!
//! ```html
//! <section data-albedo-static="SlotRoute" data-component-id="10">asChild );}</section>
//! ```
//!
//! Two failures in one string. Every tag is gone, `<main>` included. And `);}`
//! is the closing brace of the route's own source file, rendered to the browser.
//! The build was green, the console clean, and nothing was logged — the one
//! `tracing::warn!` on the path reaches nobody, because the shipped CLI installs
//! a subscriber only under `RUST_LOG`.
//!
//! The QuickJS renderer has refused this class since it was written: its `h`
//! shim throws on `typeof type !== 'string'` precisely because visible
//! corruption is the one outcome worse than a named failure. The pure-Rust path
//! had no equivalent guard.
//!
//! ## What these assert
//!
//! Two independent halves, because fixing either says nothing about the other:
//!
//! 1. **The refusal** — a Tier-A render that raises emits no source text and
//!    records a `StaticRenderFailure` that reaches the manifest (and from there
//!    the `BootReport`).
//! 2. **The tiering** — a component importing npm never reaches that path at
//!    all, because it is no longer eligible for Tier A. It is served from Tier
//!    B, which renders through QuickJS, which has the npm graph.

use dom_render_compiler::effects::{decide_tier_and_hydration, TieringInputs, TieringReason};
use dom_render_compiler::manifest::schema::{HydrationMode, RenderManifestV2, Tier};
use dom_render_compiler::manifest::ManifestOptions;
use dom_render_compiler::scanner::ProjectScanner;
use std::path::{Path, PathBuf};

fn fixture_root(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Build the manifest the way `albedo build` does — through the scanner, so the
/// parser sets the facts the tiering cascade reads. Hand-registering
/// `Component`s skips the parse and lands everything in Tier A, which would make
/// the second half of this file assert nothing.
fn build_manifest(name: &str) -> RenderManifestV2 {
    let root = fixture_root(name);
    let scanner = ProjectScanner::new();
    let compiler = scanner.scan_and_build(&root).expect("scan");
    let result = compiler.optimize().expect("optimize");
    dom_render_compiler::manifest::build_render_manifest_v2(
        compiler.graph(),
        &result,
        &ManifestOptions::default(),
    )
}

fn all_tier_a_html(manifest: &RenderManifestV2) -> String {
    manifest
        .routes
        .values()
        .flat_map(|route| route.tier_a_root.iter())
        .map(|node| node.html.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tier_of(manifest: &RenderManifestV2, name: &str) -> Tier {
    manifest
        .components
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("no component named '{name}' in the manifest"))
        .tier
}

// ─────────────────────────────────────────────────────────────────────────────
// 1 · the refusal
// ─────────────────────────────────────────────────────────────────────────────

/// **The bug, stated as an assertion.** Nothing the author wrote in their source
/// file may appear in the served HTML except as rendered markup.
///
/// The literals below are drawn from the fixture's own text, and none of them is
/// reachable through any legitimate render of it: `);}` and `export default` are
/// syntax, and `data-albedo-static` was the wrapper the scrape invented.
#[test]
fn a_failed_static_render_leaks_no_source_text() {
    let manifest = build_manifest("tier_a_refusal");
    let html = all_tier_a_html(&manifest);

    for leaked in [");}", "export default", "function SpreadRoute", "const rest"] {
        assert!(
            !html.contains(leaked),
            "the served HTML carries source text {leaked:?} — this is the scrape \
             the refusal replaced.\nHTML: {html}"
        );
    }
    assert!(
        !html.contains("data-albedo-static"),
        "`data-albedo-static` was the scrape's own wrapper and has no other \
         producer; its presence means the fallback is back.\nHTML: {html}"
    );
}

/// Absent is not enough — it has to be *named*, and to somebody who will see it.
/// The manifest is the only channel that survives `albedo build` → `albedo
/// serve` as two processes, which is why the failure rides there rather than
/// through `tracing`.
#[test]
fn a_failed_static_render_is_recorded_for_the_boot_report() {
    let manifest = build_manifest("tier_a_refusal");

    let failure = manifest
        .static_render_failures
        .iter()
        .find(|failure| failure.component == "SpreadRoute")
        .unwrap_or_else(|| {
            panic!(
                "SpreadRoute failed to render and nothing recorded it: {:?}",
                manifest.static_render_failures
            )
        });

    assert!(
        failure.module_path.contains("index.tsx"),
        "the record must name the file to open; got {:?}",
        failure.module_path
    );
    assert!(
        !failure.error.trim().is_empty(),
        "the renderer's own message is the most useful thing anyone can be told \
         here, and it is lost entirely if summarised away"
    );
}

/// One broken component, one line — not one line per route that happens to
/// render it. A shared component is re-rendered once per route, and reporting
/// the same import failure five times reads as five problems.
#[test]
fn one_broken_component_is_reported_once() {
    let manifest = build_manifest("tier_a_refusal");
    let count = manifest
        .static_render_failures
        .iter()
        .filter(|failure| failure.component == "SpreadRoute")
        .count();
    assert_eq!(count, 1, "{:?}", manifest.static_render_failures);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2 · the tiering
// ─────────────────────────────────────────────────────────────────────────────

/// A pure component that imports npm is **not Tier A**, and the reason says so.
///
/// Tier A has no `node_modules` — `ProjectScanner` never ingests one, so
/// `resolve_import` cannot hit — and every other signal the cascade reads
/// (`hooks`, `io`, `is_interactive`) is false for this component. Without this
/// rule it takes the Tier-A branch, the bake raises, and the failure lands on
/// the *route*, whose markup goes with it.
#[test]
fn a_pure_component_that_imports_npm_is_never_tier_a() {
    let manifest = build_manifest("tier_a_npm");

    assert_eq!(
        tier_of(&manifest, "Wrapper"),
        Tier::B,
        "Wrapper imports @radix-ui/react-slot; the renderer that bakes Tier A \
         cannot resolve it"
    );
}

/// **And it ships no JavaScript for the privilege.**
///
/// Tier C is where npm demonstrably works — adding a `useState` to a broken
/// Radix component was all it took to make it render — but Tier C is also the
/// tier that sends the component to the browser. This component asked for none
/// of that. Tier B reaches the same QuickJS renderer from the server, which is
/// the only thing actually missing at Tier A.
#[test]
fn tiering_up_for_npm_does_not_ship_an_island() {
    let manifest = build_manifest("tier_a_npm");
    let entry = manifest
        .components
        .iter()
        .find(|entry| entry.name == "Wrapper")
        .expect("Wrapper");

    assert_eq!(
        entry.hydration_mode,
        HydrationMode::None,
        "nothing in this component can be driven from the client, so hydrating \
         it would re-invoke it in the browser and clobber the server markup"
    );
    assert!(
        manifest
            .routes
            .values()
            .flat_map(|route| route.tier_c.iter())
            .all(|node| node.component_id != "Wrapper"),
        "a component with no hooks, no effects and no handlers must not become \
         a client island"
    );
}

/// The consequence the whole change is for: **the route around it survives.**
///
/// A Tier-A render is one call over the entire subtree, so before this rule an
/// unresolvable import in a leaf did not blank the leaf — it raised, and every
/// ancestor's markup was replaced by the scrape's `<section>`. `<main>` and
/// `<h1>` were gone from a page that never touched npm itself.
#[test]
fn the_route_that_nests_an_npm_component_still_renders_its_own_markup() {
    let manifest = build_manifest("tier_a_npm");
    let html = all_tier_a_html(&manifest);

    assert!(
        html.contains("<main"),
        "the route's own `<main>` must survive its child's tier; got {html}"
    );
    assert!(
        html.contains("<h1") && html.contains("asChild"),
        "the route's own `<h1>asChild</h1>` must survive; got {html}"
    );
    assert!(
        manifest.static_render_failures.is_empty(),
        "nothing should have failed to render at all — the npm component was \
         kept off the path that cannot run it: {:?}",
        manifest.static_render_failures
    );
}

/// The rule stated directly against the decision function, so the boundary is
/// pinned even if the fixture above stops exercising it.
#[test]
fn the_npm_fact_is_what_moves_the_tier() {
    let inputs = TieringInputs {
        tier_a_inline_max_bytes: 8 * 1024,
        tier_c_split_min_bytes: 40 * 1024,
        tier_b_mode: HydrationMode::OnIdle,
        tier_c_mode: HydrationMode::OnVisible,
    };
    let profile = dom_render_compiler::effects::EffectProfile::pure();

    let without = decide_tier_and_hydration(
        profile, false, false, false, false, false, false, 1024, inputs,
    );
    assert_eq!(without.tier, Tier::A);
    assert_eq!(without.reason, TieringReason::PureStaticEligible);

    let with = decide_tier_and_hydration(
        profile, false, false, false, false, true, false, 1024, inputs,
    );
    assert_eq!(with.tier, Tier::B);
    assert_eq!(with.reason, TieringReason::NpmDependency);
    assert_eq!(with.hydration_mode, HydrationMode::None);
}

/// Type-only imports are erased before they are ever a dependency, so
/// `import type { Props } from "@radix-ui/react-slot"` must not cost a tier.
/// The exclusion lives in `visit_import_decl`; this is the assertion that it
/// keeps applying to the fact derived from it.
#[test]
fn a_type_only_npm_import_does_not_move_the_tier() {
    let parsed = dom_render_compiler::parser::ComponentParser::new()
        .parse_source(
            r#"
            import type { SlotProps } from "@radix-ui/react-slot";

            export default function Label(props: SlotProps) {
                return <span>{props.title}</span>;
            }
            "#,
            "Label.tsx",
        )
        .expect("parse");
    let component = parsed.first().expect("one component");

    assert!(
        !dom_render_compiler::parser::imports_unresolvable_specifier(&component.import_sources),
        "a type-only import creates no runtime module dependency: {:?}",
        component.import_sources
    );
}

/// Relative specifiers are exactly what the evaluator *can* resolve, so they
/// must not trip the rule — otherwise every component in the project tiers up
/// and Tier A ceases to exist.
#[test]
fn relative_imports_are_still_tier_a_eligible() {
    let sources: Vec<String> = ["./Button", "../lib/format", "./styles.module.css"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert!(
        !dom_render_compiler::parser::imports_unresolvable_specifier(&sources),
        "relative specifiers resolve against the project tree"
    );
}

/// **The exemption this rule cannot live without**, and the one that was missing
/// on the first attempt.
///
/// `import React from "react"` heads a very large fraction of all `.jsx` ever
/// written. The evaluator compiles JSX itself, so the binding is never in value
/// position and the import costs nothing — `render_component_ref` already carves
/// `"react"` out for exactly this reason. Without the same carve-out here the
/// rule is not a narrow npm guard, it **empties Tier A**: `tests/fixtures/
/// components` is ordinary classic JSX and nothing else, and it moved wholesale
/// to Tier B and blew the default per-route Tier-B budget. That failing budget
/// test is how this was found, which is the argument for keeping it asserted
/// here too rather than resting on a budget number that could be raised.
#[test]
fn the_classic_jsx_pragma_does_not_cost_a_tier() {
    let sources: Vec<String> = ["react", "react/jsx-runtime", "./Button"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert!(
        !dom_render_compiler::parser::imports_unresolvable_specifier(&sources),
        "`import React from \"react\"` is a no-op to the evaluator, not a load"
    );
}

/// The exemption is `react`, not everything that begins with those five letters.
/// A `starts_with("react")` test would silently readmit `react-dom` and
/// `react-router` — real npm packages the evaluator genuinely cannot resolve.
#[test]
fn the_react_exemption_does_not_leak_to_react_prefixed_packages() {
    for specifier in ["react-dom", "react-router", "react-remove-scroll"] {
        assert!(
            dom_render_compiler::parser::imports_unresolvable_specifier(&[specifier.to_string()]),
            "{specifier} is npm, not the JSX pragma"
        );
    }
}

/// Guard against the fixture silently ceasing to be a fixture.
#[test]
fn the_fixtures_exist() {
    for name in ["tier_a_refusal", "tier_a_npm"] {
        let root: &Path = &fixture_root(name);
        assert!(root.join("routes").is_dir(), "{} has no routes/", root.display());
    }
}
