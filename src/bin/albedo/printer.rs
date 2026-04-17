use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::types::TierReport;

const ACCENT: u8 = 81;
const ACCENT_SOFT: u8 = 117;
const MUTED: u8 = 244;

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
        println!(
            "    {} {:<name_width$} {}  {}",
            tier_badge(row.tier),
            style_256(&row.name, ACCENT_SOFT, true),
            style(row.tier.as_str(), "2"),
            style(&row.reason, "2"),
            name_width = name_width,
        );
    }

    println!();
    print_tier_summary(
        "A",
        report.tier_a_count,
        &format!("zero JS → {}", dim("static")),
    );
    print_tier_summary(
        "B",
        report.tier_b_count,
        &format!(
            "hydrated → {} payload",
            dim(&format!("{:.1} kB", report.tier_b_hydration_bytes as f64 / 1024.0))
        ),
    );
    print_tier_summary(
        "C",
        report.tier_c_count,
        &format!("streamed → {}", dim("non-blocking")),
    );
    println!();
}

fn tier_badge(tier: Tier) -> String {
    let (ch, code) = match tier {
        Tier::A => ("A", "1;32"),
        Tier::B => ("B", "1;36"),
        Tier::C => ("C", "1;35"),
    };
    style(ch, code)
}

fn print_tier_summary(tier: &str, count: usize, hint: &str) {
    let (code, sym) = match tier {
        "A" => ("1;32", "●"),
        "B" => ("1;36", "●"),
        _ => ("1;35", "●"),
    };
    println!(
        "    {} {}  {:>3} {}  {}",
        style(sym, code),
        style(tier, code),
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
