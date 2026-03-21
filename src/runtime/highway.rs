use crate::graph::ComponentGraph;
use crate::parallel_topo::ParallelTopologicalSorter;
use crate::types::{CompilerError, ComponentAnalysis, ComponentId, Result};
use std::collections::HashMap;
use std::f64::consts::PI;

pub const LANE_COUNT: usize = 4;
const TAU: f64 = 2.0 * PI;

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
    pub component_lane: HashMap<ComponentId, usize>,
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

        let mut component_lane = HashMap::new();

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
                buckets[lane].push(*component_id);
                component_lane.insert(*component_id, lane);
            }

            for lane_id in 0..LANE_COUNT {
                buckets[lane_id].sort_unstable_by(|left, right| {
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
                lanes[lane_id].levels.push(buckets[lane_id].clone());
            }
        }

        let mut cross_lane_dependencies = Vec::new();
        for (dependent, to_lane) in &component_lane {
            for dependency in graph.get_dependencies(dependent) {
                let Some(from_lane) = component_lane.get(&dependency).copied() else {
                    continue;
                };
                if from_lane != *to_lane {
                    cross_lane_dependencies.push(CrossLaneDependency {
                        dependency,
                        dependent: *dependent,
                        from_lane,
                        to_lane: *to_lane,
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
            component_lane,
            cross_lane_dependencies,
        })
    }

    pub fn lane_of(&self, component_id: ComponentId) -> Option<usize> {
        self.component_lane.get(&component_id).copied()
    }
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
