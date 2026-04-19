use crate::graph::ComponentGraph;
use crate::types::*;
use dashmap::DashMap;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

const PARALLEL_THRESHOLD: usize = 20;

pub struct ParallelTopologicalSorter<'a> {
    graph: &'a ComponentGraph,
}

impl<'a> ParallelTopologicalSorter<'a> {
    pub fn new(graph: &'a ComponentGraph) -> Self {
        Self { graph }
    }

    pub fn sort(&self) -> Result<Vec<Vec<ComponentId>>> {
        self.graph.validate()?;

        let mut out_degree = self.graph.calculate_out_degrees();
        let mut processed = HashSet::new();
        let mut levels = Vec::new();

        let mut current_level: Vec<ComponentId> = out_degree
            .iter()
            .filter(|(_, &d)| d == 0)
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
            current_level = if current_level.len() > PARALLEL_THRESHOLD {
                self.parallel_next_level(&current_level, &mut out_degree, &processed)
            } else {
                self.serial_next_level(&current_level, &mut out_degree, &processed)
            };
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

    fn parallel_next_level(
        &self,
        current: &[ComponentId],
        out_degree: &mut HashMap<ComponentId, usize>,
        processed: &HashSet<ComponentId>,
    ) -> Vec<ComponentId> {
        let decrement_counts: DashMap<ComponentId, usize> = DashMap::new();

        current.par_iter().for_each(|&node| {
            for dep in self.graph.get_dependents(&node) {
                if !processed.contains(&dep) {
                    *decrement_counts.entry(dep).or_insert(0) += 1;
                }
            }
        });

        decrement_counts
            .into_iter()
            .filter_map(|(id, count)| {
                out_degree.get_mut(&id).and_then(|deg| {
                    *deg = deg.saturating_sub(count);
                    (*deg == 0).then_some(id)
                })
            })
            .collect()
    }

    fn serial_next_level(
        &self,
        current: &[ComponentId],
        out_degree: &mut HashMap<ComponentId, usize>,
        processed: &HashSet<ComponentId>,
    ) -> Vec<ComponentId> {
        let mut next = Vec::new();
        for &node in current {
            for dep in self.graph.get_dependents(&node) {
                if processed.contains(&dep) {
                    continue;
                }
                if let Some(deg) = out_degree.get_mut(&dep) {
                    if *deg > 0 {
                        *deg -= 1;
                    }
                    if *deg == 0 && !next.contains(&dep) {
                        next.push(dep);
                    }
                }
            }
        }
        next
    }

    pub fn sort_with_priority(
        &self,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Result<Vec<Vec<ComponentId>>> {
        let mut levels = self.sort()?;

        levels.par_iter_mut().for_each(|level| {
            level.sort_unstable_by(|a, b| {
                let pa = analyses.get(a).map_or(0.0, |x| x.priority);
                let pb = analyses.get(b).map_or(0.0, |x| x.priority);
                pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
            });
        });

        Ok(levels)
    }

    pub fn create_batches(
        &self,
        levels: Vec<Vec<ComponentId>>,
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> Vec<RenderBatch> {
        if levels.len() <= PARALLEL_THRESHOLD {
            return levels
                .iter()
                .enumerate()
                .map(|(idx, components)| self.make_batch(idx, components, analyses))
                .collect();
        }

        levels
            .into_par_iter()
            .enumerate()
            .map(|(idx, components)| self.make_batch(idx, &components, analyses))
            .collect()
    }

    fn make_batch(
        &self,
        idx: usize,
        components: &[ComponentId],
        analyses: &HashMap<ComponentId, ComponentAnalysis>,
    ) -> RenderBatch {
        let estimated_time_ms = components
            .iter()
            .filter_map(|id| analyses.get(id))
            .map(|a| a.estimated_time_ms)
            .fold(0.0_f64, f64::max);

        RenderBatch {
            level: idx,
            components: components.to_vec(),
            estimated_time_ms,
            can_defer: idx > 0,
        }
    }
}

pub fn find_critical_path_parallel(
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
) -> Vec<ComponentId> {
    let out_degrees = graph.calculate_out_degrees();
    let roots: Vec<ComponentId> = out_degrees
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(id, _)| *id)
        .collect();

    if roots.is_empty() {
        return Vec::new();
    }

    let candidates: Vec<(Vec<ComponentId>, f64)> = if roots.len() <= 4 {
        roots
            .iter()
            .map(|&root| find_longest_path(root, graph, analyses, &mut HashSet::new()))
            .collect()
    } else {
        roots
            .par_iter()
            .map(|&root| find_longest_path(root, graph, analyses, &mut HashSet::new()))
            .collect()
    };

    candidates
        .into_iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(path, _)| path)
        .unwrap_or_default()
}

fn find_longest_path(
    node: ComponentId,
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
    visited: &mut HashSet<ComponentId>,
) -> (Vec<ComponentId>, f64) {
    if visited.contains(&node) {
        return (vec![node], 0.0);
    }

    visited.insert(node);
    let node_time = analyses.get(&node).map_or(0.0, |a| a.estimated_time_ms);
    let dependents = graph.get_dependents(&node);

    if dependents.is_empty() {
        visited.remove(&node);
        return (vec![node], node_time);
    }

    let (mut longest_path, longest_time) = dependents
        .iter()
        .map(|&dep| find_longest_path(dep, graph, analyses, visited))
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or_default();

    longest_path.insert(0, node);
    visited.remove(&node);

    (longest_path, node_time + longest_time)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_graph() -> ComponentGraph {
        let graph = ComponentGraph::new();
        let id_a = graph.add_component(Component::new(ComponentId::new(0), "A".to_string()));
        let id_b = graph.add_component(Component::new(ComponentId::new(0), "B".to_string()));
        let id_c = graph.add_component(Component::new(ComponentId::new(0), "C".to_string()));
        let id_d = graph.add_component(Component::new(ComponentId::new(0), "D".to_string()));
        graph.add_dependency(id_a, id_b).unwrap();
        graph.add_dependency(id_a, id_c).unwrap();
        graph.add_dependency(id_b, id_d).unwrap();
        graph.add_dependency(id_c, id_d).unwrap();
        graph
    }

    #[test]
    fn test_parallel_topological_sort() {
        let graph = create_test_graph();
        let sorter = ParallelTopologicalSorter::new(&graph);
        let levels = sorter.sort().unwrap();
        assert_eq!(levels.len(), 3);
    }

    #[test]
    fn test_empty_graph() {
        let graph = ComponentGraph::new();
        let sorter = ParallelTopologicalSorter::new(&graph);
        let levels = sorter.sort().unwrap();
        assert_eq!(levels.len(), 0);
    }

    #[test]
    fn test_parallel_path_thread_pinning() {
        let graph = ComponentGraph::new();
        let root = graph.add_component(Component::new(ComponentId::new(0), "Root".to_string()));
        for i in 1..=25 {
            let id = graph.add_component(Component::new(ComponentId::new(0), format!("C{i}")));
            graph.add_dependency(root, id).unwrap();
        }
        let sorter = ParallelTopologicalSorter::new(&graph);
        let levels = sorter.sort().unwrap();
        assert_eq!(levels.len(), 2);
        assert_eq!(levels[0].len(), 25);
        assert_eq!(levels[1].len(), 1);
    }
}
