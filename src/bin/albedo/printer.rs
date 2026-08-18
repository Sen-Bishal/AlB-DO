use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::types::TierReport;

// Palette — "Halation" (matches src/bin/albedo.rs). Warm champagne gold on ink.
const ACCENT: u8 = 179; // champagne gold
const ACCENT_SOFT: u8 = 223; // pale gold / cream
const MUTED: u8 = 245; // warm-neutral gray

// Luminance ramp (A+B blend, "instrument for light"): a tier reads as brightness
// — Tier A (static, settled) is deep gold, Tier C (live island) burns brightest.
const TIER_LUMEN: [u8; 3] = [137, 179, 222];

/// Client bytes the build actually produced, as opposed to estimated.
///
/// `TierReport::tier_b_hydration_bytes` — which this replaces on the summary
/// line — is a sum of `WeightEstimator::estimate`, i.e.
/// `500 + imports*100 + name.len()*50 + …`. It is a proxy for source size and
/// has no relationship to any byte the browser downloads. Worse, it was printed
/// against Tier B, which ships **no** component JavaScript at all: a Tier-B
/// component is rendered on the server and delivered as markup.
///
/// Every field here is measured from the build's own output, so a reader can
/// check each one with `curl … | wc -c`.
#[derive(Debug, Clone, Default)]
pub struct MeasuredBytes {
    /// Sum of the compiled client-island module bytes across Tier-C
    /// components — the real payload an island costs, straight from
    /// `compile_client_island_module`, which is the same lowering the serve
    /// path ships.
    pub tier_c_island_bytes: u64,
    /// Tier-C components whose island compiled and was measured.
    pub tier_c_measured: usize,
    /// Framework client runtime emitted to `_albedo/` — the shared cost every
    /// live page pays regardless of how its components tiered.
    pub runtime_bytes: u64,
    /// Tier-C components whose island FAILED to compile, as
    /// `(component name, why)`.
    ///
    /// This exists because the failure used to be unobservable. Both call
    /// sites wrote `if let Ok(iife) = compile_client_island_module(…)` and
    /// dropped a `RuntimeError` that already carried an exact diagnostic — so
    /// a component could be classified Tier C, printed as "ships a client
    /// island", and then ship nothing at all. The build stayed green, the
    /// placeholder rendered as an empty `<div data-albedo-tier="c">`, and the
    /// only signal anywhere was the words "no island compiled" in the summary.
    ///
    /// A Tier-C component that produces no island is a BUILD FAILURE of the
    /// same kind as a type error: the author asked for interactivity and did
    /// not get it. Printing the reason is the minimum; see `print_tier_report`.
    pub tier_c_failures: Vec<(String, String)>,
    /// Tier C · Phase 2 — the npm packages Tier-C islands pull into the browser,
    /// as `(package, bytes)`, one entry per emitted content-hashed chunk.
    ///
    /// Counted separately from `tier_c_island_bytes` because it is a different
    /// cost with a different fix: an island's own bytes shrink by writing less
    /// component, and these shrink by importing less package (or by the host
    /// providing it). Folding them into one number would hide which lever
    /// applies — and hiding it is how 157 kB of transitive `react` sat inside a
    /// "3.5 kB icon" for a whole phase.
    pub npm_chunks: Vec<(String, u64)>,
}

pub fn print_tier_report(report: &TierReport, root: &str, measured: &MeasuredBytes) {
    println!();
    println!(
        "  {} {}  {}",
        style_256("▸", ACCENT, true),
        style("tiers", "1"),
        style(&format!("— {}", root), "2")
    );

    if report.components.is_empty() {
        println!(
            "    {}  {}",
            style("!", "1;33"),
            style("no components discovered.", "2")
        );
        return;
    }

    let mut rows = report.components.clone();
    rows.sort_by(|left, right| {
        tier_rank(left.tier)
            .cmp(&tier_rank(right.tier))
            .then_with(|| left.name.cmp(&right.name))
    });

    let name_width = rows
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or(9)
        .max("component".len())
        + 2;

    for row in &rows {
        // Pad the PLAIN name, then colorize — ANSI escapes have zero display
        // width, so padding a styled string skews the reason column.
        let pad = name_width.saturating_sub(row.name.chars().count());
        println!(
            "    {} {}{}  {}",
            tier_badge(row.tier),
            style_256(&row.name, ACCENT_SOFT, true),
            " ".repeat(pad),
            style(&row.reason, "2"),
        );
    }

    println!();
    let total = report.tier_a_count + report.tier_b_count + report.tier_c_count;
    print_tier_summary(
        0,
        report.tier_a_count,
        total,
        "A",
        &format!("zero JS → {}", dim("static")),
    );
    print_tier_summary(
        1,
        report.tier_b_count,
        total,
        "B",
        &format!("server-rendered → {}", dim("zero JS")),
    );
    // Tier C is the only tier that ships component code, so it is the only one
    // that gets a byte count.
    //
    // Reported as a ceiling (`<=`), and that is not hedging. A Tier-C component
    // reaches the browser one of two ways, and only the server knows which:
    // fully compiled as an island module, or — when the analysis proves it
    // driveable from bindings alone — as the far smaller inline reactive
    // payload. `build_reactive_blocks` makes that choice at boot, so at build
    // time the compiled module is the honest upper bound. An island that failed
    // to compile is excluded rather than estimated, so the figure never
    // includes a guess.
    let tier_c_hint = if measured.tier_c_measured == 0 {
        format!("island → {}", dim("no island compiled"))
    } else if measured.tier_c_measured < report.tier_c_count {
        format!(
            "island → {}",
            dim(&format!(
                "≤ {:.1} kB across {}/{} compiled",
                measured.tier_c_island_bytes as f64 / 1024.0,
                measured.tier_c_measured,
                report.tier_c_count
            ))
        )
    } else {
        format!(
            "island → {}",
            dim(&format!(
                "≤ {:.1} kB compiled",
                measured.tier_c_island_bytes as f64 / 1024.0
            ))
        )
    };
    print_tier_summary(2, report.tier_c_count, total, "C", &tier_c_hint);

    // Tier C · Phase 2 — what npm costs, per package, so the number is
    // actionable rather than a total to shrug at. Each chunk is content-hashed
    // and shared across every route that needs it, so this is a whole-site cost
    // paid once, not a per-page one.
    if !measured.npm_chunks.is_empty() {
        let total_npm: u64 = measured.npm_chunks.iter().map(|(_, bytes)| bytes).sum();
        println!();
        println!(
            "  {} {}  {}",
            style_256("▸", ACCENT, true),
            style("npm in the browser", "1"),
            style(
                &format!(
                    "— {:.1} kB across {} chunk{}",
                    total_npm as f64 / 1024.0,
                    measured.npm_chunks.len(),
                    if measured.npm_chunks.len() == 1 { "" } else { "s" }
                ),
                "2"
            )
        );
        let width = measured
            .npm_chunks
            .iter()
            .map(|(package, _)| package.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        for (package, bytes) in &measured.npm_chunks {
            let pad = width.saturating_sub(package.chars().count());
            println!(
                "    {}{}  {}",
                style_256(package, ACCENT_SOFT, true),
                " ".repeat(pad),
                style(&format!("{:.1} kB", *bytes as f64 / 1024.0), "2")
            );
        }
    }

    // An island that did not compile is not a footnote. The component was
    // classified Tier C — the author wrote hooks and handlers and expects them
    // to run — and what actually ships is an empty placeholder div. Silence
    // here is how a whole navigation bar goes missing from every page of a
    // site without one line of output saying so.
    //
    // The reason strings come straight from `compile_client_island_module`,
    // which already names the exact cause (a missing default export, an import
    // that is not client-bundled). They were being discarded, not missing.
    if !measured.tier_c_failures.is_empty() {
        println!();
        println!(
            "  {} {}",
            style("▸", "1;33"),
            style("islands that did not compile", "1")
        );
        for (name, reason) in &measured.tier_c_failures {
            println!(
                "  {} {}  {}",
                style("!", "1;33"),
                style_256(name, ACCENT, true),
                style(reason, "2")
            );
        }
        println!(
            "    {}",
            style(
                "these components render as an empty placeholder and never hydrate.",
                "2"
            )
        );
    }

    // The shared cost. Leaving it out is what made the per-tier numbers
    // misleading even when they were right: a page can ship zero component
    // bytes and still load the framework.
    if measured.runtime_bytes > 0 {
        println!();
        println!(
            "    {} {}  {}",
            style_256("·", MUTED, false),
            style("runtime", "1"),
            dim(&format!(
                "{:.1} kB framework client, shared by every route (gzipped on the wire)",
                measured.runtime_bytes as f64 / 1024.0
            )),
        );
    }
    println!();
}

fn tier_badge(tier: Tier) -> String {
    let color = TIER_LUMEN[tier_rank(tier) as usize];
    style_256(tier.as_str(), color, true)
}

/// Summary line with a luminance bar (A+B blend): the tier's share of all
/// components rendered as brightness — a build's tier mix reads at a glance as
/// how much of it is settled static light vs. live interactive light.
fn print_tier_summary(tier_idx: usize, count: usize, total: usize, tier: &str, hint: &str) {
    let color = TIER_LUMEN[tier_idx];
    let width = 12usize;
    let filled = if total == 0 {
        0
    } else {
        ((count * width) as f64 / total as f64).round() as usize
    }
    .min(width);
    let bar = format!(
        "{}{}",
        style_256(&"█".repeat(filled), color, true),
        style_256(&"░".repeat(width - filled), MUTED, false),
    );
    println!(
        "    {} {}  {}  {:>3} {}  {}",
        style_256("◆", color, true),
        style_256(tier, color, true),
        bar,
        count,
        style(pluralize(count), "2"),
        hint
    );
}

fn dim(value: &str) -> String {
    style_256(value, MUTED, false)
}

fn tier_rank(tier: Tier) -> u8 {
    match tier {
        Tier::A => 0,
        Tier::B => 1,
        Tier::C => 2,
    }
}

fn pluralize(count: usize) -> &'static str {
    if count == 1 {
        "component"
    } else {
        "components"
    }
}

trait TierLabel {
    fn as_str(self) -> &'static str;
}

impl TierLabel for Tier {
    fn as_str(self) -> &'static str {
        match self {
            Tier::A => "A",
            Tier::B => "B",
            Tier::C => "C",
        }
    }
}

fn style(value: &str, code: &str) -> String {
    if !supports_color() {
        return value.to_string();
    }
    format!("\u{1b}[{code}m{value}\u{1b}[0m")
}

fn style_256(value: &str, color: u8, bold: bool) -> String {
    if !supports_color() {
        return value.to_string();
    }
    if bold {
        format!("\u{1b}[1;38;5;{color}m{value}\u{1b}[0m")
    } else {
        format!("\u{1b}[38;5;{color}m{value}\u{1b}[0m")
    }
}

fn supports_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}
