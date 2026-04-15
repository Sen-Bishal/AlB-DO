use dom_render_compiler::manifest::schema::Tier;
use dom_render_compiler::types::TierReport;

pub fn print_tier_report(report: &TierReport, root: &str) {
    println!();
    println!(
        "  {}  compiling {} ...",
        style("AlBDO", "1;36"),
        style(root, "2")
    );
    println!();

    if report.components.is_empty() {
        println!(
            "  {}",
            style("No components discovered for tier analysis.", "1;33")
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
        .max("Component".len())
        + 2;
    let tier_width = "Tier".len() + 3;

    println!(
        "  {:<name_width$}{:<tier_width$}{}",
        style("Component", "1"),
        style("Tier", "1"),
        style("Why", "1"),
        name_width = name_width,
        tier_width = tier_width
    );
    println!("  {}", "-".repeat((name_width + tier_width + 48).max(42)));
    for row in &rows {
        println!(
            "  {:<name_width$}{:<tier_width$}{}",
            row.name,
            row.tier.as_str(),
            row.reason,
            name_width = name_width,
            tier_width = tier_width
        );
    }
    println!();

    println!(
        "  Tier A  {} {} -> zero JS sent to client",
        report.tier_a_count,
        pluralize(report.tier_a_count)
    );
    println!(
        "  Tier B  {} {} -> {:.1} kB hydration payload",
        report.tier_b_count,
        pluralize(report.tier_b_count),
        report.tier_b_hydration_bytes as f64 / 1024.0
    );
    println!(
        "  Tier C  {} {} -> streamed, no blocking",
        report.tier_c_count,
        pluralize(report.tier_c_count)
    );
    println!();
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

fn supports_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}
