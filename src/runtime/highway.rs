use crate::graph::ComponentGraph;
use crate::ir::columns::LANE_COUNT as IR_LANE_COUNT;
use crate::parallel_topo::ParallelTopologicalSorter;
use crate::types::{CompilerError, ComponentAnalysis, ComponentId, Result};
use std::collections::HashMap;
use std::f64::consts::PI;

pub const LANE_COUNT: usize = 4;
const TAU: f64 = 2.0 * PI;

// Cycle 4 aligns `IrColumns` physically to the 4-lane highway. A mismatch
// between the IR column partition count and the runtime lane count would
// silently corrupt `lane_offsets`; assert at compile time that the two
// constants cannot drift.
const _: () = assert!(
    LANE_COUNT == IR_LANE_COUNT,
    "runtime LANE_COUNT must equal IR LANE_COUNT"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrossLaneDependency {
    pub dependency: ComponentId,
    pub dependent: ComponentId,
    pub from_lane: usize,
    pub to_lane: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighwayLanePlan {
    pub lane_id: usize,
    pub levels: Vec<Vec<ComponentId>>,
}

impl HighwayLanePlan {
    pub fn flattened_components(&self) -> Vec<ComponentId> {
        let mut flattened = Vec::new();
        for level in &self.levels {
            flattened.extend(level.iter().copied());
        }
        flattened
    }
}

#[derive(Debug, Clone)]
pub struct HighwayPlan {
    pub lanes: Vec<HighwayLanePlan>,
    /// Component-to-lane lookup table sorted by `ComponentId` for
    /// binary-search access. Replaces the pre-cycle-4 `HashMap` so the hot
    /// path does a cache-friendly linear probe instead of a hash load.
    pub component_lane: Vec<(ComponentId, u8)>,
    /// Prefix-sum of lane sizes across the cycle-4 lane-sorted column
    /// store. `lane_offsets[i]..lane_offsets[i + 1]` is the half-open
    /// column range owned by lane `i`.
    pub lane_offsets: [u32; LANE_COUNT + 1],
    pub cross_lane_dependencies: Vec<CrossLaneDependency>,
}

impl HighwayPlan {
    pub fn build(
        graph: &ComponentGraph,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Result<Self> {
        let sorter = ParallelTopologicalSorter::new(graph);
        let levels = sorter.sort_with_priority(analyses)?;
        Self::from_levels(graph, analyses, &levels)
    }

    pub fn from_levels(
        graph: &ComponentGraph,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
        levels: &[Vec<ComponentId>],
    ) -> Result<Self> {
        let mut lanes = (0..LANE_COUNT)
            .map(|lane_id| HighwayLanePlan {
                lane_id,
                levels: Vec::new(),
            })
            .collect::<Vec<_>>();

        let mut pairs: Vec<(ComponentId, u8)> = Vec::new();

        for level in levels {
            let mut buckets = vec![Vec::new(); LANE_COUNT];
            for component_id in level {
                let Some(analysis) = analyses.get(component_id) else {
                    return Err(CompilerError::AnalysisFailed(format!(
                        "missing analysis for component {:?} while building 4-lane topology",
                        component_id
                    )));
                };

                let lane = phase_to_lane(analysis.phase);
                if let Some(bucket) = buckets.get_mut(lane) {
                    bucket.push(*component_id);
                }
                pairs.push((*component_id, u8::try_from(lane).unwrap_or(0)));
            }

            for lane_id in 0..LANE_COUNT {
                if let Some(bucket) = buckets.get_mut(lane_id) {
                    bucket.sort_unstable_by(|left, right| {
                        let left_priority =
                            analyses.get(left).map_or(0.0, |analysis| analysis.priority);
                        let right_priority = analyses
                            .get(right)
                            .map_or(0.0, |analysis| analysis.priority);
                        right_priority
                            .partial_cmp(&left_priority)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then_with(|| left.as_u64().cmp(&right.as_u64()))
                    });
                }
                if let (Some(lane_plan), Some(bucket)) =
                    (lanes.get_mut(lane_id), buckets.get(lane_id))
                {
                    lane_plan.levels.push(bucket.clone());
                }
            }
        }

        pairs.sort_unstable_by_key(|(id, _)| id.as_u64());
        pairs.dedup_by_key(|(id, _)| *id);

        let mut lane_counts = [0_u32; LANE_COUNT];
        for (_, lane) in &pairs {
            if let Some(count) = lane_counts.get_mut(usize::from(*lane)) {
                *count = count.saturating_add(1);
            }
        }

        let mut lane_offsets = [0_u32; LANE_COUNT + 1];
        let mut running: u32 = 0;
        for lane in 0..LANE_COUNT {
            if let Some(slot) = lane_offsets.get_mut(lane) {
                *slot = running;
            }
            running = running.saturating_add(lane_counts.get(lane).copied().unwrap_or(0));
        }
        if let Some(last) = lane_offsets.get_mut(LANE_COUNT) {
            *last = running;
        }

        let mut cross_lane_dependencies = Vec::new();
        for (dependent, to_lane) in &pairs {
            for dependency in graph.get_dependencies(dependent) {
                let Some(from_lane) = lookup_lane(&pairs, dependency) else {
                    continue;
                };
                if from_lane != *to_lane {
                    cross_lane_dependencies.push(CrossLaneDependency {
                        dependency,
                        dependent: *dependent,
                        from_lane: usize::from(from_lane),
                        to_lane: usize::from(*to_lane),
                    });
                }
            }
        }

        cross_lane_dependencies.sort_unstable_by(|left, right| {
            left.dependent
                .as_u64()
                .cmp(&right.dependent.as_u64())
                .then_with(|| left.dependency.as_u64().cmp(&right.dependency.as_u64()))
                .then_with(|| left.from_lane.cmp(&right.from_lane))
                .then_with(|| left.to_lane.cmp(&right.to_lane))
        });

        Ok(Self {
            lanes,
            component_lane: pairs,
            lane_offsets,
            cross_lane_dependencies,
        })
    }

    /// Binary-searches the component-to-lane table for `component_id`.
    pub fn lane_of(&self, component_id: ComponentId) -> Option<usize> {
        lookup_lane(&self.component_lane, component_id).map(usize::from)
    }

    /// Returns the contiguous cross-lane dependency slice whose `dependent`
    /// matches `component_id`.
    ///
    /// `cross_lane_dependencies` is sorted by `dependent`, so this runs in
    /// `O(log N)` via a single binary-search probe plus a linear expansion
    /// across the equal-key run.
    pub fn cross_lane_deps_for_dependent(
        &self,
        component_id: ComponentId,
    ) -> &[CrossLaneDependency] {
        let key = component_id.as_u64();
        let deps = self.cross_lane_dependencies.as_slice();
        let Ok(anchor) = deps.binary_search_by_key(&key, |entry| entry.dependent.as_u64()) else {
            return &[];
        };

        let mut start = anchor;
        while start > 0
            && deps
                .get(start.saturating_sub(1))
                .map_or(false, |entry| entry.dependent.as_u64() == key)
        {
            start = start.saturating_sub(1);
        }

        let mut end = anchor.saturating_add(1);
        while deps.get(end).map_or(false, |entry| entry.dependent.as_u64() == key) {
            end = end.saturating_add(1);
        }

        deps.get(start..end).unwrap_or(&[])
    }
}

fn lookup_lane(pairs: &[(ComponentId, u8)], component_id: ComponentId) -> Option<u8> {
    pairs
        .binary_search_by_key(&component_id.as_u64(), |(id, _)| id.as_u64())
        .ok()
        .and_then(|idx| pairs.get(idx).map(|(_, lane)| *lane))
}

pub fn phase_to_lane(phase: f64) -> usize {
    let normalized = phase.rem_euclid(TAU);
    let lane_width = TAU / LANE_COUNT as f64;
    let lane = (normalized / lane_width).floor() as usize;
    lane.min(LANE_COUNT - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Component;
    use std::f64::consts::PI;

    fn component(id: u64, name: &str) -> Component {
        Component::new(ComponentId::new(id), name.to_string())
    }

    fn analysis(id: ComponentId, phase: f64, priority: f64) -> ComponentAnalysis {
        ComponentAnalysis {
            id,
            priority,
            estimated_time_ms: 1.0,
            phase,
            topological_level: 0,
        }
    }

    #[test]
    fn test_phase_to_lane_boundaries() {
        let lane_width = TAU / LANE_COUNT as f64;
        assert_eq!(phase_to_lane(0.0), 0);
        assert_eq!(phase_to_lane(lane_width - 0.0001), 0);
        assert_eq!(phase_to_lane(lane_width), 1);
        assert_eq!(phase_to_lane(2.0 * lane_width), 2);
        assert_eq!(phase_to_lane(3.0 * lane_width), 3);
        assert_eq!(phase_to_lane(TAU + 0.01), 0);
    }

    #[test]
    fn test_highway_plan_assigns_lanes_and_tracks_cross_lane_dependencies() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(component(0, "A"));
        let id_b = graph.add_component(component(0, "B"));
        let id_c = graph.add_component(component(0, "C"));

        graph.add_dependency(id_a, id_b).unwrap();
        graph.add_dependency(id_b, id_c).unwrap();

        let mut analyses = HashMap::new();
        analyses.insert(id_c, analysis(id_c, 0.1, 3.0)); // lane 0
        analyses.insert(id_b, analysis(id_b, PI + 0.1, 2.0)); // lane 2
        analyses.insert(id_a, analysis(id_a, (TAU * 0.9) + 0.01, 1.0)); // lane 3

        let levels = vec![vec![id_c], vec![id_b], vec![id_a]];
        let plan = HighwayPlan::from_levels(&graph, &analyses, &levels).unwrap();

        assert_eq!(plan.lane_of(id_c), Some(0));
        assert_eq!(plan.lane_of(id_b), Some(2));
        assert_eq!(plan.lane_of(id_a), Some(3));
        assert_eq!(plan.cross_lane_dependencies.len(), 2);
    }

    #[test]
    fn test_highway_plan_orders_by_priority_within_lane_level() {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(component(0, "A"));
        let id_b = graph.add_component(component(0, "B"));

        let mut analyses = HashMap::new();
        analyses.insert(id_a, analysis(id_a, 0.2, 1.0));
        analyses.insert(id_b, analysis(id_b, 0.1, 5.0));

        let levels = vec![vec![id_a, id_b]];
        let plan = HighwayPlan::from_levels(&graph, &analyses, &levels).unwrap();
        assert_eq!(plan.lanes[0].levels[0], vec![id_b, id_a]);
    }
}
