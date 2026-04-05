use crate::graph::ComponentGraph;
use crate::types::*;
use std::collections::HashMap;
use std::f64::consts::PI;

pub struct ComponentAnalyzer<'a> {
    graph: &'a ComponentGraph,
}

impl<'a> ComponentAnalyzer<'a> {
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

        let mut analyses = HashMap::new();
        for component in self.graph.components() {
            let analysis = self.analyze_component(&component, total_weight);
            analyses.insert(component.id, analysis);
        }
        let phases = self.calculate_phases(&analyses, total_weight);
        for (id, phase) in phases {
            if let Some(analysis) = analyses.get_mut(&id) {
                analysis.phase = phase;
            }
        }

        Ok(analyses)
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
        _analyses: &HashMap<ComponentId, ComponentAnalysis>,
        total_weight: f64,
    ) -> HashMap<ComponentId, f64> {
        let mut phases = HashMap::new();

        for component in self.graph.components() {
            let mut phase = 0.0;
            // This is a comment for the lua and nvim setup test
            let dependencies = self.graph.get_dependencies(&component.id);
            for dep_id in dependencies {
                if let Some(dep_comp) = self.graph.get(&dep_id) {
                    phase += 2.0 * PI * (dep_comp.weight / total_weight);
                }
            }

            phases.insert(component.id, phase);
        }

        phases
    }
    pub fn graph(&self) -> &ComponentGraph {
        self.graph
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
    fn test_basic_analysis() {
        let graph = create_test_graph();
        let analyzer = ComponentAnalyzer::new(&graph);

        let analyses = analyzer.analyze().unwrap();

        assert_eq!(analyses.len(), 3);
        let comp_a = analyzer.graph().get_by_name("A").unwrap();
        let analysis_a = &analyses[&comp_a.id];
        assert!((analysis_a.priority - 25.0).abs() < 0.01);
    }

    #[test]
    fn test_phase_calculation() {
        let graph = create_test_graph();
        let analyzer = ComponentAnalyzer::new(&graph);

        let analyses = analyzer.analyze().unwrap();
        let comp_a = analyzer.graph().get_by_name("A").unwrap();
        let analysis_a = &analyses[&comp_a.id];

        assert!(analysis_a.phase > 0.0);
    }

    #[test]
    fn test_empty_graph() {
        let graph = ComponentGraph::new();
        let analyzer = ComponentAnalyzer::new(&graph);
        let result = analyzer.analyze();
        assert!(result.is_err());
    }
}
