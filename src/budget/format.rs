//! Pretty terminal formatter for [`crate::budget::BudgetReport`].
//!
//! Output stays plain ASCII so it survives copy-paste into PR
//! comments and renders identically across terminals. Colour is the
//! CLI's job — formatters here are pure-string so they round-trip
//! through tests.

use crate::budget::report::{
    BudgetReport, BudgetViolation, RouteSummary, ViolationKind,
};

/// Render the report as the multi-line failure message printed by
/// `albedo budget` and the build/ship gate.
pub fn format_report_pretty(report: &BudgetReport) -> String {
    let mut out = String::new();
    if report.is_ok() {
        format_summary_table(&mut out, &report.route_summaries);
        out.push_str("\nOK · all routes within tier budget\n");
        return out;
    }

    out.push_str("FAIL · tier budget exceeded\n");
    out.push('\n');
    for violation in &report.violations {
        append_violation(&mut out, violation);
        out.push('\n');
    }

    out.push_str("--- current usage ---\n");
    format_summary_table(&mut out, &report.route_summaries);
    out
}

fn append_violation(out: &mut String, violation: &BudgetViolation) {
    let (limit_disp, actual_disp, delta_disp) = format_amounts(violation);
    let route_label = if violation.route.is_empty() {
        "<global>".to_string()
    } else {
        violation.route.clone()
    };
    out.push_str(&format!(
        "* {label} exceeded - {route}\n",
        label = violation.kind.label(),
        route = route_label,
    ));
    out.push_str(&format!("    limit   {limit_disp}\n"));
    out.push_str(&format!(
        "    actual  {actual_disp}  ({delta_disp} over)\n"
    ));
    if !violation.top_contributors.is_empty() {
        out.push_str("    top contributors:\n");
        for contrib in &violation.top_contributors {
            out.push_str(&format!(
                "      - {name:<24} {weight}\n",
                name = contrib.name,
                weight = format_bytes_kb(contrib.weight_bytes),
            ));
        }
    }
}

fn format_amounts(violation: &BudgetViolation) -> (String, String, String) {
    match violation.kind {
        ViolationKind::TierBMaxKbPerRoute | ViolationKind::TierBMaxKbPerComponent => {
            let limit_kb = violation.limit as f64 / 1024.0;
            let actual_kb = violation.actual as f64 / 1024.0;
            let delta_kb = actual_kb - limit_kb;
            (
                format!("{limit_kb:.1} KB"),
                format!("{actual_kb:.1} KB"),
                format!("+{delta_kb:.1} KB"),
            )
        }
        ViolationKind::TierAMaxComponentsPerRoute
        | ViolationKind::TierCMaxConcurrentFetchesPerRoute => {
            let delta = violation.actual.saturating_sub(violation.limit);
            (
                format!("{} {}", violation.limit, violation.kind.unit()),
                format!("{} {}", violation.actual, violation.kind.unit()),
                format!("+{delta}"),
            )
        }
    }
}

fn format_summary_table(out: &mut String, summaries: &[RouteSummary]) {
    if summaries.is_empty() {
        out.push_str("  (no routes in manifest)\n");
        return;
    }
    out.push_str(&format!(
        "  {route:<28}  {a:>10}  {b:>12}  {c:>10}\n",
        route = "route",
        a = "tier-a",
        b = "tier-b",
        c = "tier-c",
    ));
    out.push_str(&format!(
        "  {dash:<28}  {dash:>10}  {dash:>12}  {dash:>10}\n",
        dash = "----",
    ));
    for summary in summaries {
        out.push_str(&format!(
            "  {route:<28}  {a:>10}  {b:>12}  {c:>10}\n",
            route = trim_for_table(&summary.route, 28),
            a = format!("{} cmp", summary.tier_a_component_count),
            b = format_bytes_kb(summary.tier_b_total_bytes),
            c = format!("{} fetch", summary.tier_c_concurrent_fetches),
        ));
    }
}

fn format_bytes_kb(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    format!("{kb:.1} KB")
}

fn trim_for_table(value: &str, width: usize) -> String {
    if value.len() <= width {
        value.to_string()
    } else {
        // Keep last `width-1` chars so dynamic prefixes don't dominate
        // long path collisions; lead with `~` to signal truncation.
        let tail = &value[value.len() - (width - 1)..];
        format!("~{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::report::{
        BudgetReport, BudgetViolation, ComponentContribution, RouteSummary, ViolationKind,
    };

    fn summary(route: &str, a: u32, b_bytes: u64, c: u32) -> RouteSummary {
        RouteSummary {
            route: route.to_string(),
            tier_a_component_count: a,
            tier_b_total_bytes: b_bytes,
            tier_c_concurrent_fetches: c,
        }
    }

    #[test]
    fn ok_report_renders_summary_table_and_ok_footer() {
        let report = BudgetReport {
            violations: Vec::new(),
            route_summaries: vec![summary("/", 3, 2 * 1024, 1)],
        };
        let out = format_report_pretty(&report);
        assert!(out.contains("route"));
        assert!(out.contains("tier-a"));
        assert!(out.contains("3 cmp"));
        assert!(out.contains("2.0 KB"));
        assert!(out.contains("1 fetch"));
        assert!(out.contains("OK"));
    }

    #[test]
    fn violation_report_renders_label_route_limit_actual_and_top_contributors() {
        let report = BudgetReport {
            violations: vec![BudgetViolation {
                route: "/dashboard".to_string(),
                kind: ViolationKind::TierBMaxKbPerRoute,
                limit: 8 * 1024,
                actual: 14 * 1024 + 512,
                top_contributors: vec![
                    ComponentContribution {
                        name: "DashboardChart".to_string(),
                        weight_bytes: 6 * 1024,
                    },
                    ComponentContribution {
                        name: "MetricsTable".to_string(),
                        weight_bytes: 4 * 1024 + 512,
                    },
                ],
            }],
            route_summaries: vec![summary("/dashboard", 0, 14 * 1024 + 512, 0)],
        };
        let out = format_report_pretty(&report);
        assert!(out.contains("FAIL"));
        assert!(out.contains("tier-b route weight exceeded - /dashboard"));
        assert!(out.contains("limit   8.0 KB"));
        assert!(out.contains("actual  14.5 KB"));
        assert!(out.contains("(+6.5 KB over)"));
        assert!(out.contains("DashboardChart"));
        assert!(out.contains("MetricsTable"));
        assert!(out.contains("current usage"));
    }

    #[test]
    fn count_violation_uses_count_units_not_kb() {
        let report = BudgetReport {
            violations: vec![BudgetViolation {
                route: "/cards".to_string(),
                kind: ViolationKind::TierAMaxComponentsPerRoute,
                limit: 50,
                actual: 73,
                top_contributors: Vec::new(),
            }],
            route_summaries: vec![summary("/cards", 73, 0, 0)],
        };
        let out = format_report_pretty(&report);
        assert!(out.contains("limit   50 count"));
        assert!(out.contains("actual  73 count"));
        assert!(out.contains("(+23 over)"));
    }

    #[test]
    fn empty_manifest_yields_friendly_summary() {
        let report = BudgetReport {
            violations: Vec::new(),
            route_summaries: Vec::new(),
        };
        let out = format_report_pretty(&report);
        assert!(out.contains("(no routes in manifest)"));
        assert!(out.contains("OK"));
    }
}
