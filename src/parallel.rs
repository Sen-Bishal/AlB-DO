use crate::graph::ComponentGraph;
use crate::types::*;
use dashmap::DashMap;
use std::collections::HashMap;
use std::f64::consts::PI;

const PARALLEL_THRESHOLD: usize = 50;

pub struct ParallelAnalyzer<'a> {
    graph: &'a ComponentGraph,
}

impl<'a> ParallelAnalyzer<'a> {
    pub fn new(graph: &'a ComponentGraph) -> Self {
        Self { graph }
    }

    pub fn analyze(&self) -> Result<HashMap<ComponentId, ComponentAnalysis>> {
        self.graph.validate()?;

        let total_weight = self.graph.total_weight();
        if total_weight == 0.0 {
            return Err(CompilerError::AnalysisFailed(
                "Total weight is zero".to_string(),
            ));
        }

        let components = self.graph.components();
        let mut analyses = if components.len() > PARALLEL_THRESHOLD {
            self.parallel_analyze(&components, total_weight)
        } else {
            self.serial_analyze(&components, total_weight)
        };

        let phases = self.calculate_phases(total_weight, &components);
        for (id, phase) in phases {
            if let Some(a) = analyses.get_mut(&id) {
                a.phase = phase;
            }
        }

        Ok(analyses)
    }

    fn parallel_analyze(
        &self,
        components: &[Component],
        total_weight: f64,
    ) -> HashMap<ComponentId, ComponentAnalysis> {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let n_threads = num_cpus::get().min(components.len()).max(1);
        let chunk_size = components.len().div_ceil(n_threads);
        let results: DashMap<ComponentId, ComponentAnalysis> = DashMap::new();

        crossbeam::scope(|s| {
            for (i, chunk) in components.chunks(chunk_size).enumerate() {
                let results = &results;
                let core_id = core_ids.get(i % core_ids.len().max(1)).copied();
                s.spawn(move |_| {
                    if let Some(id) = core_id {
                        core_affinity::set_for_current(id);
                    }
                    for comp in chunk {
                        results.insert(comp.id, self.analyze_component(comp, total_weight));
                    }
                });
            }
        })
        .unwrap();

        results.into_iter().collect()
    }

    fn serial_analyze(
        &self,
        components: &[Component],
        total_weight: f64,
    ) -> HashMap<ComponentId, ComponentAnalysis> {
        components
            .iter()
            .map(|comp| (comp.id, self.analyze_component(comp, total_weight)))
            .collect()
    }

    fn analyze_component(&self, component: &Component, _total_weight: f64) -> ComponentAnalysis {
        let adjusted_bitrate = component.calculate_adjusted_bitrate();
        let priority = if component.weight > 0.0 {
            adjusted_bitrate / component.weight
        } else {
            adjusted_bitrate
        };
        let estimated_time_ms = if adjusted_bitrate > 0.0 {
            (component.weight / adjusted_bitrate) * 1000.0
        } else {
            component.weight
        };

        ComponentAnalysis {
            id: component.id,
            priority,
            estimated_time_ms,
            phase: 0.0,
            topological_level: 0,
        }
    }

    fn calculate_phases(
        &self,
        total_weight: f64,
        components: &[Component],
    ) -> HashMap<ComponentId, f64> {
        if components.len() > PARALLEL_THRESHOLD {
            let core_ids = core_affinity::get_core_ids().unwrap_or_default();
            let n_threads = num_cpus::get().min(components.len()).max(1);
            let chunk_size = components.len().div_ceil(n_threads);
            let results: DashMap<ComponentId, f64> = DashMap::new();

            crossbeam::scope(|s| {
                for (i, chunk) in components.chunks(chunk_size).enumerate() {
                    let results = &results;
                    let core_id = core_ids.get(i % core_ids.len().max(1)).copied();
                    s.spawn(move |_| {
                        if let Some(id) = core_id {
                            core_affinity::set_for_current(id);
                        }
                        for comp in chunk {
                            results.insert(comp.id, self.component_phase(comp, total_weight));
                        }
                    });
                }
            })
            .unwrap();

            results.into_iter().collect()
        } else {
            components
                .iter()
                .map(|comp| (comp.id, self.component_phase(comp, total_weight)))
                .collect()
        }
    }

    fn component_phase(&self, component: &Component, total_weight: f64) -> f64 {
        self.graph
            .get_dependencies(&component.id)
            .iter()
            .filter_map(|dep_id| self.graph.get(dep_id))
            .map(|dep| 2.0 * PI * (dep.weight / total_weight))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_graph() -> ComponentGraph {
        let graph = ComponentGraph::new();

        let mut comp_a = Component::new(ComponentId::new(0), "A".to_string());
        comp_a.weight = 100.0;
        comp_a.bitrate = 500.0;
        comp_a.is_above_fold = true;

        let mut comp_b = Component::new(ComponentId::new(0), "B".to_string());
        comp_b.weight = 200.0;
        comp_b.bitrate = 300.0;

        let mut comp_c = Component::new(ComponentId::new(0), "C".to_string());
        comp_c.weight = 150.0;
        comp_c.bitrate = 200.0;

        let id_a = graph.add_component(comp_a);
        let id_b = graph.add_component(comp_b);
        let id_c = graph.add_component(comp_c);

        graph.add_dependency(id_a, id_b).unwrap();
        graph.add_dependency(id_b, id_c).unwrap();

        graph
    }

    #[test]
    fn test_parallel_analysis() {
        let graph = create_test_graph();
        let analyzer = ParallelAnalyzer::new(&graph);
        let analyses = analyzer.analyze().unwrap();
        assert_eq!(analyses.len(), 3);
    }

    #[test]
    fn test_priority_positive() {
        let graph = create_test_graph();
        let analyzer = ParallelAnalyzer::new(&graph);
        let analyses = analyzer.analyze().unwrap();
        for analysis in analyses.values() {
            assert!(analysis.priority > 0.0);
        }
    }
}
