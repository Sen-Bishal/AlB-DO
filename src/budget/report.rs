//! Budget evaluation — walks a [`RenderManifestV2`] and produces a
//! [`BudgetReport`] of every ceiling violation, with the top
//! contributing components attached so the failure message stays
//! actionable in CI output.

use crate::budget::config::TierBudget;
use crate::manifest::schema::{RenderManifestV2, RouteManifest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// One ceiling that was exceeded by the current manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetViolation {
    /// Route the violation applies to. For per-component violations
    /// this is the route the component renders under; for budget
    /// kinds that are not route-scoped this is the empty string.
    pub route: String,
    pub kind: ViolationKind,
    pub limit: u64,
    pub actual: u64,
    /// Components contributing the most to the breach, descending by
    /// individual weight. Up to 3 entries; fewer when the route has
    /// fewer components.
    pub top_contributors: Vec<ComponentContribution>,
}

/// Kinds of ceiling the evaluator recognises. Adding a kind is a
/// (config + report) two-touch change so the failure modes stay
/// enumerable from a single file.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViolationKind {
    TierAMaxComponentsPerRoute,
    TierBMaxKbPerRoute,
    TierBMaxKbPerComponent,
    TierCMaxConcurrentFetchesPerRoute,
    /// Phase O.3 · per-component Tier-B emitted bundle weight
    /// exceeded. Distinct from `TierBMaxKbPerComponent` (source
    /// weight) so the diagnostic message can point at the bundle
    /// path instead of the source file.
    TierBBundleKbPerComponent,
}

impl ViolationKind {
    /// Short tag rendered in the pretty formatter and as the JSON
    /// label. Stable so tooling can grep for them.
    pub fn label(self) -> &'static str {
        match self {
            Self::TierAMaxComponentsPerRoute => "tier-a count",
            Self::TierBMaxKbPerRoute => "tier-b route weight",
            Self::TierBMaxKbPerComponent => "tier-b component weight",
            Self::TierCMaxConcurrentFetchesPerRoute => "tier-c concurrent fetches",
            Self::TierBBundleKbPerComponent => "tier-b component bundle",
        }
    }

    /// Unit string for the ceiling values — `"count"` or `"KB"`.
    pub fn unit(self) -> &'static str {
        match self {
            Self::TierAMaxComponentsPerRoute | Self::TierCMaxConcurrentFetchesPerRoute => "count",
            Self::TierBMaxKbPerRoute
            | Self::TierBMaxKbPerComponent
            | Self::TierBBundleKbPerComponent => "KB",
        }
    }
}

/// One component's contribution to a violation. Weight is in bytes
/// so callers control rounding; the pretty formatter renders KB with
/// one decimal place.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComponentContribution {
    pub name: String,
    pub weight_bytes: u64,
}

/// Top-level evaluation output. `violations.is_empty()` is the
/// canonical "did we pass" signal; the report is also returned on
/// success so the pretty formatter can render a "you're at X/Y"
/// status line.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BudgetReport {
    pub violations: Vec<BudgetViolation>,
    pub route_summaries: Vec<RouteSummary>,
}

impl BudgetReport {
    pub fn is_ok(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Per-route metric snapshot. Surfaced in the report regardless of
/// whether the route is over budget so the formatter can render a
/// "current usage" table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteSummary {
    pub route: String,
    pub tier_a_component_count: u32,
    pub tier_b_total_bytes: u64,
    pub tier_c_concurrent_fetches: u32,
}

/// Evaluate `manifest` against `budget`. Pure function — no IO, no
/// allocation surprises beyond the obvious. Routes are walked in the
/// order [`RenderManifestV2.routes`] iterates (`HashMap` order is
/// not stable, so the formatter sorts route_summaries before
/// printing).
pub fn evaluate_budget(manifest: &RenderManifestV2, budget: &TierBudget) -> BudgetReport {
    let weights = component_weight_index(manifest);
    let mut violations: Vec<BudgetViolation> = Vec::new();
    let mut summaries: Vec<RouteSummary> = Vec::new();

    for (route_path, route) in &manifest.routes {
        let resolved = budget.for_route(route_path);

        let tier_a_count = count_tier_a_components(route);
        let (tier_b_total_bytes, tier_b_contribs) = tier_b_weight_breakdown(route, &weights);
        let tier_c_count: u32 = u32::try_from(route.tier_c.len()).unwrap_or(u32::MAX);

        summaries.push(RouteSummary {
            route: route_path.clone(),
            tier_a_component_count: tier_a_count,
            tier_b_total_bytes,
            tier_c_concurrent_fetches: tier_c_count,
        });

        if tier_a_count > resolved.tier_a_max_components_per_route {
            violations.push(BudgetViolation {
                route: route_path.clone(),
                kind: ViolationKind::TierAMaxComponentsPerRoute,
                limit: u64::from(resolved.tier_a_max_components_per_route),
                actual: u64::from(tier_a_count),
                top_contributors: Vec::new(),
            });
        }

        let tier_b_route_limit_bytes =
            u64::from(resolved.tier_b_max_kb_per_route).saturating_mul(1024);
        if tier_b_total_bytes > tier_b_route_limit_bytes {
            violations.push(BudgetViolation {
                route: route_path.clone(),
                kind: ViolationKind::TierBMaxKbPerRoute,
                limit: tier_b_route_limit_bytes,
                actual: tier_b_total_bytes,
                top_contributors: top_n_contributors(&tier_b_contribs, 3),
            });
        }

        let tier_b_component_limit_bytes =
            u64::from(resolved.tier_b_max_kb_per_component).saturating_mul(1024);
        for contrib in &tier_b_contribs {
            if contrib.weight_bytes > tier_b_component_limit_bytes {
                violations.push(BudgetViolation {
                    route: route_path.clone(),
                    kind: ViolationKind::TierBMaxKbPerComponent,
                    limit: tier_b_component_limit_bytes,
                    actual: contrib.weight_bytes,
                    top_contributors: vec![contrib.clone()],
                });
            }
        }

        if tier_c_count > resolved.tier_c_max_concurrent_fetches_per_route {
            violations.push(BudgetViolation {
                route: route_path.clone(),
                kind: ViolationKind::TierCMaxConcurrentFetchesPerRoute,
                limit: u64::from(resolved.tier_c_max_concurrent_fetches_per_route),
                actual: u64::from(tier_c_count),
                top_contributors: Vec::new(),
            });
        }
    }

    // Deterministic output regardless of manifest HashMap iteration.
    summaries.sort_by(|a, b| a.route.cmp(&b.route));
    violations.sort_by(|a, b| {
        a.route
            .cmp(&b.route)
            .then_with(|| format!("{:?}", a.kind).cmp(&format!("{:?}", b.kind)))
    });

    BudgetReport {
        violations,
        route_summaries: summaries,
    }
}

/// Name → weight_bytes lookup. Built once per evaluation rather
/// than per-route so the per-component cap check is O(1) per node.
fn component_weight_index(manifest: &RenderManifestV2) -> HashMap<String, u64> {
    let mut map = HashMap::with_capacity(manifest.components.len());
    for entry in &manifest.components {
        map.insert(entry.name.clone(), entry.weight_bytes);
    }
    map
}

/// Tier-A surface a route paints = top-level Tier-A nodes plus the
/// nested-A children of every Tier-B node. Tier-C children are
/// rendered server-side as Tier-A HTML too but the manifest does not
/// inline them, so they aren't counted here.
fn count_tier_a_components(route: &RouteManifest) -> u32 {
    let top_level = route.tier_a_root.len();
    let nested = route
        .tier_b
        .iter()
        .map(|node| node.tier_a_children.len())
        .sum::<usize>();
    u32::try_from(top_level.saturating_add(nested)).unwrap_or(u32::MAX)
}

fn tier_b_weight_breakdown(
    route: &RouteManifest,
    weights: &HashMap<String, u64>,
) -> (u64, Vec<ComponentContribution>) {
    let mut contribs = Vec::with_capacity(route.tier_b.len());
    let mut total = 0u64;
    for node in &route.tier_b {
        let weight = weights.get(&node.component_id).copied().unwrap_or(0);
        total = total.saturating_add(weight);
        contribs.push(ComponentContribution {
            name: node.component_id.clone(),
            weight_bytes: weight,
        });
    }
    (total, contribs)
}

fn top_n_contributors(contribs: &[ComponentContribution], n: usize) -> Vec<ComponentContribution> {
    let mut sorted: Vec<ComponentContribution> = contribs.to_vec();
    sorted.sort_by(|a, b| b.weight_bytes.cmp(&a.weight_bytes).then_with(|| a.name.cmp(&b.name)));
    sorted.truncate(n);
    sorted
}

/// Phase O.3 · evaluate the measured-bundle-byte report against the
/// budget. Independent of [`evaluate_budget`] so a caller can run
/// either (or both) gates — the source-weight pass uses the manifest
/// only and is cheap, the bundle pass requires a full emit and is
/// the production-truthful metric.
///
/// Violations carry the offending component's `wrapper_bytes` as the
/// `actual` value; the source-map weight is intentionally excluded
/// (maps don't ship to production browsers as part of the
/// hot-path payload).
#[must_use]
pub fn evaluate_bundle_budget(
    bundle_report: &crate::budget::bundle::BundleByteReport,
    budget: &crate::budget::config::TierBudget,
) -> BudgetReport {
    let mut violations: Vec<BudgetViolation> = Vec::new();
    let mut route_summaries: Vec<RouteSummary> = Vec::new();

    // The bundle pass doesn't have per-route attribution today —
    // wrappers are component-keyed, not route-keyed — so route
    // summaries are empty here. The CLI / formatter still surfaces
    // the per-component figures through the violation breakdown.
    let _ = &mut route_summaries;

    // Per-component sweep. Bundle ceilings apply per-component only;
    // route-aggregate bundle ceilings are a follow-up once route-level
    // wrapper grouping is implemented in the emit step.
    let mut components: Vec<&crate::budget::bundle::ComponentBundleSummary> =
        bundle_report.per_component.values().collect();
    components.sort_by(|left, right| {
        left.component_id
            .cmp(&right.component_id)
            .then_with(|| left.component_name.cmp(&right.component_name))
    });

    for summary in components {
        // Only Tier-B components are subject to the bundle ceiling.
        // Tier-A ships zero JS (no wrapper bytes); Tier-C streams
        // server-side and doesn't ride the hydration JS path.
        if !matches!(summary.tier, crate::manifest::schema::Tier::B) {
            continue;
        }
        let defaults = &budget.defaults;
        let limit_bytes =
            u64::from(defaults.tier_b_bundle_max_kb_per_component).saturating_mul(1024);
        let actual = summary.budget_bytes();
        if actual > limit_bytes {
            violations.push(BudgetViolation {
                route: String::new(),
                kind: ViolationKind::TierBBundleKbPerComponent,
                limit: limit_bytes,
                actual,
                top_contributors: vec![ComponentContribution {
                    name: summary.component_name.clone(),
                    weight_bytes: actual,
                }],
            });
        }
    }

    violations.sort_by(|a, b| {
        a.route
            .cmp(&b.route)
            .then_with(|| format!("{:?}", a.kind).cmp(&format!("{:?}", b.kind)))
            .then_with(|| {
                a.top_contributors
                    .first()
                    .map(|c| c.name.as_str())
                    .cmp(&b.top_contributors.first().map(|c| c.name.as_str()))
            })
    });

    BudgetReport {
        violations,
        route_summaries,
    }
}

// Surface tier weights to userland JSON consumers; kept private to
// the report module — the manifest already exposes the underlying
// components for anyone who needs more detail.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        AssetManifest, ComponentManifestEntry, DomPosition, HtmlShell, HydrationMode, RenderedNode,
        RouteManifest, Tier, TierBNode, TierCNode,
    };

    fn position(slot: &str, order: u32) -> DomPosition {
        DomPosition {
            parent_placeholder: None,
            slot: slot.to_string(),
            order,
        }
    }

    fn entry(name: &str, tier: Tier, weight_bytes: u64) -> ComponentManifestEntry {
        ComponentManifestEntry {
            id: 0,
            name: name.to_string(),
            module_path: format!("src/{name}.tsx"),
            tier,
            weight_bytes,
            priority: 1.0,
            dependencies: Vec::new(),
            can_defer: false,
            hydration_mode: HydrationMode::None,
        }
    }

    fn tier_a_node(name: &str) -> RenderedNode {
        RenderedNode {
            component_id: name.to_string(),
            placeholder_id: format!("ph-{name}"),
            html: format!("<div data-x=\"{name}\"></div>"),
            position: position("root", 0),
        }
    }

    fn tier_b_node(name: &str) -> TierBNode {
        TierBNode {
            component_id: name.to_string(),
            placeholder_id: format!("ph-{name}"),
            render_fn: format!("{name}.render"),
            static_props: serde_json::Value::Null,
            dynamic_prop_keys: Vec::new(),
            data_deps: Vec::new(),
            tier_a_children: Vec::new(),
            position: position("root", 0),
            timeout_ms: 250,
            fallback_html: None,
            initial_html: None,
            initial_opcode_frame: Vec::new(),
        }
    }

    fn tier_c_node(name: &str) -> TierCNode {
        TierCNode {
            component_id: name.to_string(),
            placeholder_id: format!("ph-{name}"),
            bundle_path: format!("_albedo/wrappers/{name}.mjs"),
            initial_props: serde_json::Value::Null,
            hydration_mode: HydrationMode::Immediate,
            position: position("root", 0),
        }
    }

    fn route(
        path: &str,
        tier_a: Vec<RenderedNode>,
        tier_b: Vec<TierBNode>,
        tier_c: Vec<TierCNode>,
    ) -> RouteManifest {
        RouteManifest {
            route: path.to_string(),
            shell: HtmlShell {
                doctype_and_head: String::new(),
                body_open: String::new(),
                body_close: String::new(),
                shim_script: String::new(),
            },
            tier_a_root: tier_a,
            tier_b,
            tier_c,
            shared_slot_topics: Vec::new(),
            action_ids: Vec::new(),
            layout_chain: Vec::new(),
            error_component: None,
            loading_component: None,
            metadata: Default::default(),
        }
    }

    fn manifest(routes: Vec<RouteManifest>, components: Vec<ComponentManifestEntry>) -> RenderManifestV2 {
        let mut m = RenderManifestV2::legacy_defaults();
        for r in routes {
            m.routes.insert(r.route.clone(), r);
        }
        m.components = components;
        m.assets = AssetManifest::default();
        m
    }

    #[test]
    fn under_ceiling_yields_no_violations() {
        let m = manifest(
            vec![route("/", vec![tier_a_node("Hero")], vec![tier_b_node("Counter")], vec![])],
            vec![entry("Hero", Tier::A, 0), entry("Counter", Tier::B, 2 * 1024)],
        );
        let report = evaluate_budget(&m, &TierBudget::default());
        assert!(report.is_ok());
        assert_eq!(report.route_summaries.len(), 1);
        let summary = &report.route_summaries[0];
        assert_eq!(summary.tier_a_component_count, 1);
        assert_eq!(summary.tier_b_total_bytes, 2 * 1024);
        assert_eq!(summary.tier_c_concurrent_fetches, 0);
    }

    #[test]
    fn tier_b_route_total_over_ceiling_violates() {
        let m = manifest(
            vec![route(
                "/dashboard",
                Vec::new(),
                vec![tier_b_node("Chart"), tier_b_node("Table")],
                Vec::new(),
            )],
            vec![
                entry("Chart", Tier::B, 5 * 1024),
                entry("Table", Tier::B, 5 * 1024),
            ],
        );
        let report = evaluate_budget(&m, &TierBudget::default());
        assert!(!report.is_ok());
        let v = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::TierBMaxKbPerRoute)
            .expect("expected route-wide tier-b violation");
        assert_eq!(v.route, "/dashboard");
        assert_eq!(v.limit, 8 * 1024);
        assert_eq!(v.actual, 10 * 1024);
        assert_eq!(v.top_contributors.len(), 2);
        // Top contributor is whichever weighs more (tie-break by name).
        assert_eq!(v.top_contributors[0].name, "Chart");
    }

    #[test]
    fn tier_b_per_component_over_ceiling_violates() {
        let m = manifest(
            vec![route("/", Vec::new(), vec![tier_b_node("Bloated")], Vec::new())],
            vec![entry("Bloated", Tier::B, 6 * 1024)],
        );
        let report = evaluate_budget(&m, &TierBudget::default());
        assert!(!report.is_ok());
        let v = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::TierBMaxKbPerComponent)
            .expect("expected per-component violation");
        assert_eq!(v.limit, 4 * 1024);
        assert_eq!(v.actual, 6 * 1024);
        assert_eq!(v.top_contributors[0].name, "Bloated");
    }

    #[test]
    fn tier_a_component_count_over_ceiling_violates() {
        let many: Vec<RenderedNode> = (0..52)
            .map(|i| tier_a_node(&format!("Card{i}")))
            .collect();
        let m = manifest(vec![route("/", many, Vec::new(), Vec::new())], Vec::new());
        let report = evaluate_budget(&m, &TierBudget::default());
        let v = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::TierAMaxComponentsPerRoute)
            .expect("expected tier-a count violation");
        assert_eq!(v.limit, 50);
        assert_eq!(v.actual, 52);
    }

    #[test]
    fn tier_a_count_includes_children_under_tier_b_nodes() {
        let mut node = tier_b_node("Card");
        node.tier_a_children = vec![tier_a_node("Inner1"), tier_a_node("Inner2")];
        let m = manifest(
            vec![route("/", vec![tier_a_node("Header")], vec![node], Vec::new())],
            vec![entry("Card", Tier::B, 0)],
        );
        let report = evaluate_budget(&m, &TierBudget::default());
        assert_eq!(report.route_summaries[0].tier_a_component_count, 3);
    }

    #[test]
    fn tier_c_concurrent_fetches_over_ceiling_violates() {
        let cs: Vec<TierCNode> = (0..11).map(|i| tier_c_node(&format!("F{i}"))).collect();
        let m = manifest(vec![route("/", Vec::new(), Vec::new(), cs)], Vec::new());
        let report = evaluate_budget(&m, &TierBudget::default());
        let v = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::TierCMaxConcurrentFetchesPerRoute)
            .expect("expected tier-c violation");
        assert_eq!(v.limit, 10);
        assert_eq!(v.actual, 11);
    }

    #[test]
    fn per_route_override_relaxes_the_ceiling() {
        let m = manifest(
            vec![route(
                "/dashboard",
                Vec::new(),
                vec![tier_b_node("Big1"), tier_b_node("Big2")],
                Vec::new(),
            )],
            vec![
                entry("Big1", Tier::B, 6 * 1024),
                entry("Big2", Tier::B, 6 * 1024),
            ],
        );
        let mut budget = TierBudget::default();
        budget.routes.insert(
            "/dashboard".to_string(),
            crate::budget::RouteBudget {
                tier_b_max_kb_per_route: Some(16),
                tier_b_max_kb_per_component: Some(8),
                ..Default::default()
            },
        );
        let report = evaluate_budget(&m, &budget);
        assert!(report.is_ok(), "expected pass under overridden ceiling, got {:?}", report.violations);
    }

    #[test]
    fn bundle_eval_passes_when_every_tier_b_component_is_under_ceiling() {
        use crate::budget::bundle::{BundleByteReport, ComponentBundleSummary};
        use std::collections::HashMap;

        let mut per_component = HashMap::new();
        per_component.insert(
            1u64,
            ComponentBundleSummary {
                component_id: 1,
                component_name: "Counter".to_string(),
                tier: Tier::B,
                wrapper_bytes: 800,
                source_map_bytes: 4000,
            },
        );
        let bundle = BundleByteReport {
            attributions: Vec::new(),
            per_component,
            vendor_total_bytes: 0,
            infrastructure_total_bytes: 0,
        };
        let report = crate::budget::evaluate_bundle_budget(&bundle, &TierBudget::default());
        assert!(report.is_ok());
    }

    #[test]
    fn bundle_eval_flags_oversized_tier_b_component_with_actionable_violation() {
        use crate::budget::bundle::{BundleByteReport, ComponentBundleSummary};
        use std::collections::HashMap;

        let mut per_component = HashMap::new();
        per_component.insert(
            7u64,
            ComponentBundleSummary {
                component_id: 7,
                component_name: "BloatedIsland".to_string(),
                tier: Tier::B,
                wrapper_bytes: 142 * 1024,
                source_map_bytes: 0,
            },
        );
        let bundle = BundleByteReport {
            attributions: Vec::new(),
            per_component,
            vendor_total_bytes: 0,
            infrastructure_total_bytes: 0,
        };
        let report = crate::budget::evaluate_bundle_budget(&bundle, &TierBudget::default());
        assert!(!report.is_ok());
        let v = report
            .violations
            .iter()
            .find(|v| v.kind == ViolationKind::TierBBundleKbPerComponent)
            .expect("expected bundle violation");
        assert_eq!(v.limit, 1024);
        assert_eq!(v.actual, 142 * 1024);
        assert_eq!(
            v.top_contributors.first().map(|c| c.name.as_str()),
            Some("BloatedIsland"),
        );
    }

    #[test]
    fn bundle_eval_skips_tier_a_and_tier_c_components() {
        use crate::budget::bundle::{BundleByteReport, ComponentBundleSummary};
        use std::collections::HashMap;

        // Both are over the 1 KB Tier-B ceiling, but neither is a
        // Tier-B component so the bundle gate must not fire. Tier-A
        // ships zero JS in practice; Tier-C streams server-side.
        let mut per_component = HashMap::new();
        per_component.insert(
            1u64,
            ComponentBundleSummary {
                component_id: 1,
                component_name: "BigStatic".to_string(),
                tier: Tier::A,
                wrapper_bytes: 50 * 1024,
                source_map_bytes: 0,
            },
        );
        per_component.insert(
            2u64,
            ComponentBundleSummary {
                component_id: 2,
                component_name: "BigStreamed".to_string(),
                tier: Tier::C,
                wrapper_bytes: 80 * 1024,
                source_map_bytes: 0,
            },
        );
        let bundle = BundleByteReport {
            attributions: Vec::new(),
            per_component,
            vendor_total_bytes: 0,
            infrastructure_total_bytes: 0,
        };
        let report = crate::budget::evaluate_bundle_budget(&bundle, &TierBudget::default());
        assert!(report.is_ok());
    }

    #[test]
    fn evaluation_output_is_deterministic_across_runs() {
        let m = manifest(
            vec![
                route("/b", Vec::new(), vec![tier_b_node("Big")], Vec::new()),
                route("/a", Vec::new(), vec![tier_b_node("Bigger")], Vec::new()),
            ],
            vec![
                entry("Big", Tier::B, 10 * 1024),
                entry("Bigger", Tier::B, 20 * 1024),
            ],
        );
        let r1 = evaluate_budget(&m, &TierBudget::default());
        let r2 = evaluate_budget(&m, &TierBudget::default());
        assert_eq!(r1, r2);
        // Sorted by route ascending — `/a` precedes `/b`.
        assert_eq!(r1.route_summaries[0].route, "/a");
    }
}
