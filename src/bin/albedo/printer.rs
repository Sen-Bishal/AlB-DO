use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::types::TierReport;

// Palette — "Halation" (matches src/bin/albedo.rs). Warm champagne gold on ink.
const ACCENT: u8 = 179; // champagne gold
const ACCENT_SOFT: u8 = 223; // pale gold / cream
const MUTED: u8 = 245; // warm-neutral gray

// Luminance ramp (A+B blend, "instrument for light"): a tier reads as brightness
// — Tier A (static, settled) is deep gold, Tier C (live island) burns brightest.
const TIER_LUMEN: [u8; 3] = [137, 179, 222];

pub fn print_tier_report(report: &TierReport, root: &str) {
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
        &format!(
            "hydrated → {} payload",
            dim(&format!("{:.1} kB", report.tier_b_hydration_bytes as f64 / 1024.0))
        ),
    );
    print_tier_summary(
        2,
        report.tier_c_count,
        total,
        "C",
        &format!("streamed → {}", dim("non-blocking")),
    );
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
