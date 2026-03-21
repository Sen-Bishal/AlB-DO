use crate::graph::ComponentGraph;
use crate::types::*;
use std::collections::{HashMap, HashSet};
pub struct TopologicalSorter<'a> {
    graph: &'a ComponentGraph,
}

impl<'a> TopologicalSorter<'a> {
    pub fn new(graph: &'a ComponentGraph) -> Self {
        Self { graph }
    }
    pub fn sort(&self) -> Result<Vec<Vec<ComponentId>>> {
        self.graph.validate()?;
        let mut in_degree = self.graph.calculate_in_degrees();
        let mut processed = HashSet::new();
        let mut levels = Vec::new();
        let mut current_level: Vec<ComponentId> = in_degree
            .iter()
            .filter(|(_, &degree)| degree == 0)
            .map(|(id, _)| *id)
            .collect();

        if current_level.is_empty() && !self.graph.is_empty() {
            return Err(CompilerError::InvalidGraph(
                "No components with zero dependencies found".to_string(),
            ));
        }
        while !current_level.is_empty() {
            levels.push(current_level.clone());
            for &node in &current_level {
                processed.insert(node);
            }
            let mut next_level = Vec::new();

            for &node in &current_level {
                let dependents = self.graph.get_dependents(&node);

                for dependent in dependents {
                    if processed.contains(&dependent) {
                        continue;
                    }

                    if let Some(degree) = in_degree.get_mut(&dependent) {
                        if *degree > 0 {
                            *degree -= 1;
                        }

                        if *degree == 0 && !next_level.contains(&dependent) {
                            next_level.push(dependent);
                        }
                    }
                }
            }

            current_level = next_level;
        }
        if processed.len() != self.graph.len() {
            return Err(CompilerError::InvalidGraph(format!(
                "Only processed {} of {} components - possible cycle",
                processed.len(),
                self.graph.len()
            )));
        }

        Ok(levels)
    }

    pub fn sort_with_priority(
        &self,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Result<Vec<Vec<ComponentId>>> {
        let mut levels = self.sort()?;

        // Sort each level by priority (highest first)
        for level in &mut levels {
            level.sort_by(|a, b| {
                let priority_a = analyses.get(a).map(|a| a.priority).unwrap_or(0.0);
                let priority_b = analyses.get(b).map(|a| a.priority).unwrap_or(0.0);
                priority_b.partial_cmp(&priority_a).unwrap()
            });
        }

        Ok(levels)
    }
    pub fn create_batches(
        &self,
        levels: Vec<Vec<ComponentId>>,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Vec<RenderBatch> {
        levels
            .into_iter()
            .enumerate()
            .map(|(idx, components)| {
                let estimated_time_ms = components
                    .iter()
                    .filter_map(|id| analyses.get(id))
                    .map(|a| a.estimated_time_ms)
                    .fold(0.0, f64::max);

                let can_defer = idx > 0;

                RenderBatch {
                    level: idx,
                    components,
                    estimated_time_ms,
                    can_defer,
                }
            })
            .collect()
    }
}
pub fn find_critical_path(
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
) -> Vec<ComponentId> {
    let mut max_path = Vec::new();
    let mut max_time = 0.0;
    let in_degrees = graph.calculate_in_degrees();
    let roots: Vec<ComponentId> = in_degrees
        .iter()
        .filter(|(_, &degree)| degree == 0)
        .map(|(id, _)| *id)
        .collect();

    for root in roots {
        let (path, time) = find_longest_path_from(root, graph, analyses, &mut HashSet::new());
        if time > max_time {
            max_time = time;
            max_path = path;
        }
    }

    max_path
}
fn find_longest_path_from(
    node: ComponentId,
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
    visited: &mut HashSet<ComponentId>,
) -> (Vec<ComponentId>, f64) {
    if visited.contains(&node) {
        return (vec![node], 0.0);
    }

    visited.insert(node);

    let node_time = analyses
        .get(&node)
        .map(|a| a.estimated_time_ms)
        .unwrap_or(0.0);

    let dependents = graph.get_dependents(&node);

    if dependents.is_empty() {
        visited.remove(&node);
        return (vec![node], node_time);
    }
    let mut longest_path = Vec::new();
    let mut longest_time = 0.0;

    for dependent in dependents {
        let (path, time) = find_longest_path_from(dependent, graph, analyses, visited);
        if time > longest_time {
            longest_time = time;
            longest_path = path;
        }
    }
    longest_path.insert(0, node);
    visited.remove(&node);

    (longest_path, node_time + longest_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_graph() -> ComponentGraph {
        let graph = ComponentGraph::new();
        let comp_a = Component::new(ComponentId::new(0), "A".to_string());
        let comp_b = Component::new(ComponentId::new(0), "B".to_string());
        let comp_c = Component::new(ComponentId::new(0), "C".to_string());
        let comp_d = Component::new(ComponentId::new(0), "D".to_string());

        let id_a = graph.add_component(comp_a);
        let id_b = graph.add_component(comp_b);
        let id_c = graph.add_component(comp_c);
        let id_d = graph.add_component(comp_d);

        graph.add_dependency(id_a, id_b).unwrap();
        graph.add_dependency(id_a, id_c).unwrap();
        graph.add_dependency(id_b, id_d).unwrap();
        graph.add_dependency(id_c, id_d).unwrap();

        graph
    }

    #[test]
    fn test_topological_sort() {
        let graph = create_test_graph();
        let sorter = TopologicalSorter::new(&graph);

        let levels = sorter.sort().unwrap();
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0].len(), 1);
        assert_eq!(levels[1].len(), 2);
        assert_eq!(levels[2].len(), 1);
    }

    #[test]
    fn test_empty_graph() {
        let graph = ComponentGraph::new();
        let sorter = TopologicalSorter::new(&graph);

        let levels = sorter.sort().unwrap();
        assert_eq!(levels.len(), 0);
    }

    #[test]
    fn test_critical_path() {
        let graph = create_test_graph();
        let mut analyses = HashMap::new();
        for comp in graph.components() {
            let mut analysis = ComponentAnalysis::new(comp.id);
            analysis.estimated_time_ms = 100.0;
            analyses.insert(comp.id, analysis);
        }

        let path = find_critical_path(&graph, &analyses);
        assert_eq!(path.len(), 3);
    }
}
