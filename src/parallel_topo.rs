use crate::graph::ComponentGraph;
use crate::types::*;
use crossbeam::channel;
use dashmap::DashMap;
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

        let mut in_degree = self.graph.calculate_in_degrees();
        let mut processed = HashSet::new();
        let mut levels = Vec::new();

        let mut current_level: Vec<ComponentId> = in_degree
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
                self.parallel_next_level(&current_level, &mut in_degree, &processed)
            } else {
                self.serial_next_level(&current_level, &mut in_degree, &processed)
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
        in_degree: &mut HashMap<ComponentId, usize>,
        processed: &HashSet<ComponentId>,
    ) -> Vec<ComponentId> {
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let n_threads = num_cpus::get().min(current.len()).max(1);
        let chunk_size = current.len().div_ceil(n_threads);
        let decrement_counts: DashMap<ComponentId, usize> = DashMap::new();

        crossbeam::scope(|s| {
            for (i, chunk) in current.chunks(chunk_size).enumerate() {
                let decrement_counts = &decrement_counts;
                let core_id = core_ids.get(i % core_ids.len().max(1)).copied();
                s.spawn(move |_| {
                    if let Some(id) = core_id {
                        core_affinity::set_for_current(id);
                    }
                    for &node in chunk {
                        for dep in self.graph.get_dependents(&node) {
                            if !processed.contains(&dep) {
                                *decrement_counts.entry(dep).or_insert(0) += 1;
                            }
                        }
                    }
                });
            }
        })
        .unwrap();

        decrement_counts
            .into_iter()
            .filter_map(|(id, count)| {
                in_degree.get_mut(&id).and_then(|deg| {
                    *deg = deg.saturating_sub(count);
                    (*deg == 0).then_some(id)
                })
            })
            .collect()
    }

    fn serial_next_level(
        &self,
        current: &[ComponentId],
        in_degree: &mut HashMap<ComponentId, usize>,
        processed: &HashSet<ComponentId>,
    ) -> Vec<ComponentId> {
        let mut next = Vec::new();
        for &node in current {
            for dep in self.graph.get_dependents(&node) {
                if processed.contains(&dep) {
                    continue;
                }
                if let Some(deg) = in_degree.get_mut(&dep) {
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

        let n_threads = num_cpus::get().min(levels.len()).max(1);
        let chunk_size = levels.len().div_ceil(n_threads);
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();

        crossbeam::scope(|s| {
            for (i, chunk) in levels.chunks_mut(chunk_size).enumerate() {
                let core_id = core_ids.get(i % core_ids.len().max(1)).copied();
                s.spawn(move |_| {
                    if let Some(id) = core_id {
                        core_affinity::set_for_current(id);
                    }
                    for level in chunk.iter_mut() {
                        level.sort_unstable_by(|a, b| {
                            let pa = analyses.get(a).map_or(0.0, |x| x.priority);
                            let pb = analyses.get(b).map_or(0.0, |x| x.priority);
                            pb.partial_cmp(&pa).unwrap()
                        });
                    }
                });
            }
        })
        .unwrap();

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

        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let n_threads = num_cpus::get().min(levels.len()).max(1);
        let chunk_size = levels.len().div_ceil(n_threads);
        let (tx, rx) = channel::unbounded::<Vec<RenderBatch>>();

        crossbeam::scope(|s| {
            for (thread_idx, chunk) in levels.chunks(chunk_size).enumerate() {
                let tx = tx.clone();
                let core_id = core_ids.get(thread_idx % core_ids.len().max(1)).copied();
                let base_idx = thread_idx * chunk_size;
                s.spawn(move |_| {
                    if let Some(id) = core_id {
                        core_affinity::set_for_current(id);
                    }
                    let batches = chunk
                        .iter()
                        .enumerate()
                        .map(|(offset, components)| {
                            self.make_batch(base_idx + offset, components, analyses)
                        })
                        .collect();
                    tx.send(batches).unwrap();
                });
            }
            drop(tx);
        })
        .unwrap();

        let mut out: Vec<RenderBatch> = rx.iter().flatten().collect();
        out.sort_unstable_by_key(|b| b.level);
        out
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
    let in_degrees = graph.calculate_in_degrees();
    let roots: Vec<ComponentId> = in_degrees
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(id, _)| *id)
        .collect();

    if roots.is_empty() {
        return Vec::new();
    }

    if roots.len() <= 4 {
        return roots
            .iter()
            .map(|&root| find_longest_path(root, graph, analyses, &mut HashSet::new()))
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .map(|(path, _)| path)
            .unwrap_or_default();
    }

    let core_ids = core_affinity::get_core_ids().unwrap_or_default();
    let n_threads = num_cpus::get().min(roots.len()).max(1);
    let chunk_size = roots.len().div_ceil(n_threads);
    let (tx, rx) = channel::unbounded::<(Vec<ComponentId>, f64)>();

    crossbeam::scope(|s| {
        for (i, chunk) in roots.chunks(chunk_size).enumerate() {
            let tx = tx.clone();
            let core_id = core_ids.get(i % core_ids.len().max(1)).copied();
            s.spawn(move |_| {
                if let Some(id) = core_id {
                    core_affinity::set_for_current(id);
                }
                if let Some(best) = chunk
                    .iter()
                    .map(|&root| find_longest_path(root, graph, analyses, &mut HashSet::new()))
                    .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
                {
                    tx.send(best).unwrap();
                }
            });
        }
        drop(tx);
    })
    .unwrap();

    rx.iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
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
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
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
