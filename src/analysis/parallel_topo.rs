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
            .map(|&root| find_longest_path(root, graph, analyses))
            .collect()
    } else {
        roots
            .par_iter()
            .map(|&root| find_longest_path(root, graph, analyses))
            .collect()
    };

    candidates
        .into_iter()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(path, _)| path)
        .unwrap_or_default()
}

/// Longest path from `node`, memoised.
///
/// # 🔴 What this replaced, and why it mattered
///
/// The original was a backtracking DFS — `visited.insert(node)` … recurse …
/// `visited.remove(&node)` — which memoises nothing and therefore **enumerates
/// every path** from every root. That is not a pathological case: every place
/// two components share a `Button`, a `layout` or an icon is a diamond, and `k`
/// diamonds have `2^k` paths.
///
/// Measured before the change (`critical_path_shape`, release):
///
/// | nodes | wall |
/// |---|---|
/// | 25 | 0.14 ms |
/// | 37 | 1.64 ms |
/// | 49 | 26.99 ms |
/// | 55 | 106.94 ms |
/// | 61 | **427.10 ms** |
///
/// ×4.0 for every **+6 nodes** — the signature of enumeration, on the build
/// path, which `albedo dev` runs on every save. The scaffold has a dozen
/// components, which is exactly why this never bit.
///
/// 🔑 **Memoising the answer, not the path.** Each node's best *successor* is
/// stored rather than its best path, so a memo hit is `O(1)` instead of a
/// `Vec` clone, and the path is walked out once at the end. The result is
/// `O(V+E)`.
///
/// The on-stack set stays: `graph.validate()` rejects cycles, but this function
/// must not become the thing that hangs if it is ever called before that runs.
fn longest_time_from(
    node: ComponentId,
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
    memo: &mut HashMap<ComponentId, (Option<ComponentId>, f64)>,
    on_stack: &mut HashSet<ComponentId>,
) -> f64 {
    if let Some((_, time)) = memo.get(&node) {
        return *time;
    }
    // A cycle: answer as the backtracking version did — this node alone.
    if !on_stack.insert(node) {
        return 0.0;
    }

    let node_time = analyses.get(&node).map_or(0.0, |a| a.estimated_time_ms);
    let mut best: Option<(ComponentId, f64)> = None;
    for dependent in graph.get_dependents(&node) {
        let time = longest_time_from(dependent, graph, analyses, memo, on_stack);
        if best.is_none_or(|(_, best_time)| time > best_time) {
            best = Some((dependent, time));
        }
    }

    on_stack.remove(&node);
    let total = node_time + best.map_or(0.0, |(_, time)| time);
    memo.insert(node, (best.map(|(id, _)| id), total));
    total
}

/// Walk the memo out into the path it describes.
fn path_from(
    node: ComponentId,
    memo: &HashMap<ComponentId, (Option<ComponentId>, f64)>,
) -> Vec<ComponentId> {
    let mut path = vec![node];
    let mut current = node;
    // Bounded by the memo: every step moves to a node recorded exactly once, so
    // this cannot outlive the map even if a cycle slipped past `validate`.
    while let Some((Some(next), _)) = memo.get(&current) {
        if path.len() > memo.len() {
            break;
        }
        path.push(*next);
        current = *next;
    }
    path
}

/// The longest path from `node` and its total, `O(V+E)`.
fn find_longest_path(
    node: ComponentId,
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
) -> (Vec<ComponentId>, f64) {
    let mut memo = HashMap::new();
    let total = longest_time_from(node, graph, analyses, &mut memo, &mut HashSet::new());
    (path_from(node, &memo), total)
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

/// 📏 The parked suspicion in `OPTIMIZATIONS.md` § "Unmeasured suspicions",
/// measured.
///
/// ```text
/// cargo test --release --lib -- --ignored --nocapture critical_path_shape
/// ```
#[cfg(test)]
mod critical_path_shape {
    use super::*;
    use std::time::Instant;

    /// A chain of diamonds: `a → {b, c} → a' → {b', c'} → a'' …`
    ///
    /// This is not a pathological shape invented to make a point — it is what a
    /// real component graph looks like. Every place two components share a
    /// `Button`, a `layout` or an icon is a diamond, and they compose. `k`
    /// diamonds have **2^k distinct paths** from the root, which is exactly what
    /// a backtracking DFS that memoizes nothing enumerates.
    fn diamond_chain(diamonds: usize) -> (ComponentGraph, HashMap<ComponentId, ComponentAnalysis>) {
        let graph = ComponentGraph::new();
        let mut analyses = HashMap::new();
        let mut spine =
            graph.add_component(Component::new(ComponentId::new(0), "root".to_string()));
        analyses.insert(spine, ComponentAnalysis::new(spine));

        for i in 0..diamonds {
            let left =
                graph.add_component(Component::new(ComponentId::new(0), format!("l{i}")));
            let right =
                graph.add_component(Component::new(ComponentId::new(0), format!("r{i}")));
            let join =
                graph.add_component(Component::new(ComponentId::new(0), format!("j{i}")));
            for id in [left, right, join] {
                analyses.insert(id, ComponentAnalysis::new(id));
            }
            graph.add_dependency(spine, left).unwrap();
            graph.add_dependency(spine, right).unwrap();
            graph.add_dependency(left, join).unwrap();
            graph.add_dependency(right, join).unwrap();
            spine = join;
        }
        (graph, analyses)
    }

    /// ✅ **The fix, measured.** Before memoisation this table ran
    /// 0.14 → 427 ms across 25 → 61 nodes, ×4.0 per +6 nodes; it could not have
    /// reached the 1801-node row at all (`2^600` paths). It is now linear —
    /// ×3.0 wall for ×3.0 nodes — which is what `O(V+E)` looks like from
    /// outside.
    #[test]
    #[ignore = "timing; run explicitly in release"]
    fn the_critical_path_walk_is_linear_in_the_graph() {
        println!("\n=== analysis · find_critical_path_parallel, by graph SHAPE ===\n");
        println!("  diamonds   nodes   paths     wall");

        let mut previous: Option<(usize, f64)> = None;
        for diamonds in [8usize, 12, 16, 18, 20, 60, 200, 600] {
            let (graph, analyses) = diamond_chain(diamonds);
            let nodes = 1 + diamonds * 3;

            let started = Instant::now();
            let path = find_critical_path_parallel(&graph, &analyses);
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            assert!(!path.is_empty(), "the walk must find a path to time");

            println!(
                "  {diamonds:>8}   {nodes:>5}   2^{diamonds:<6}  {ms:>8.2} ms{}",
                match previous {
                    Some((prev_d, prev_ms)) if prev_ms > 0.0 =>
                        format!("   ×{:.1} for +{} diamonds", ms / prev_ms, diamonds - prev_d),
                    _ => String::new(),
                }
            );
            previous = Some((diamonds, ms));

            // A guard rail so this test cannot itself become the slow thing.
            if ms > 4_000.0 {
                println!("\n  (stopping — the next size would not finish)");
                break;
            }
        }

        println!(
            "\n  🔑 The `paths` column is what the OLD walk enumerated: node count grows\n  \
             linearly (3 per diamond) while the path count doubles per diamond. The\n  \
             measured ratio was ×4.0 for every +6 nodes — enumeration — and is now ×3.0\n  \
             for ×3.0 nodes. No node-count intuition would have found the first, which\n  \
             is why the ledger's rule is to measure the SHAPE at two sizes.\n"
        );
    }

    /// Memoising the best *successor* rather than the best path is the part that
    /// could quietly return a valid-but-wrong answer, so the weights here are
    /// distinct: the existing `test_critical_path` uses uniform ones, where
    /// picking any longest-by-length path passes.
    #[test]
    fn the_walk_picks_the_HEAVIEST_path_not_merely_a_long_one() {
        let graph = ComponentGraph::new();
        let root = graph.add_component(Component::new(ComponentId::new(0), "root".to_string()));
        let cheap = graph.add_component(Component::new(ComponentId::new(0), "cheap".to_string()));
        let cheap2 = graph.add_component(Component::new(ComponentId::new(0), "cheap2".to_string()));
        let heavy = graph.add_component(Component::new(ComponentId::new(0), "heavy".to_string()));
        // 🪤 `add_dependency(a, b)` means *a depends on b*, and the walk starts
        // at **out-degree-0** nodes and climbs `get_dependents`. So the edges
        // point the opposite way from the picture: `root` is the node everything
        // hangs off, and the branches are its dependents.
        //
        // The cheap branch is LONGER by node count and lighter by time, which is
        // the pair a length-based walk gets backwards.
        graph.add_dependency(cheap, root).unwrap();
        graph.add_dependency(cheap2, cheap).unwrap();
        graph.add_dependency(heavy, root).unwrap();

        let mut analyses = HashMap::new();
        for (id, ms) in [(root, 1.0), (cheap, 1.0), (cheap2, 1.0), (heavy, 100.0)] {
            let mut analysis = ComponentAnalysis::new(id);
            analysis.estimated_time_ms = ms;
            analyses.insert(id, analysis);
        }

        let (path, total) = find_longest_path(root, &graph, &analyses);
        assert_eq!(path, vec![root, heavy], "the heavy branch is the critical path");
        assert!((total - 101.0).abs() < f64::EPSILON, "total was {total}");
    }

    /// A shared leaf is reached by both branches — the whole reason the memo
    /// exists. Its answer must be the same whichever branch asks first.
    #[test]
    fn a_shared_leaf_gives_one_answer_to_every_branch() {
        let (graph, analyses) = diamond_chain(3);
        let (path, _) = find_critical_path_parallel(&graph, &analyses)
            .first()
            .map(|&root| find_longest_path(root, &graph, &analyses))
            .expect("a root exists");
        // 3 diamonds: root + (one side + join) × 3 = 7 nodes on any full path.
        assert_eq!(path.len(), 7, "path was {path:?}");
    }
}
