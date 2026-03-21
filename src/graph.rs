use crate::types::*;
use dashmap::DashMap;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone)]
pub struct ComponentGraph {
    components: DashMap<ComponentId, Component>,
    name_index: DashMap<String, ComponentId>,
    dependencies: DashMap<ComponentId, HashSet<ComponentId>>,
    dependents: DashMap<ComponentId, HashSet<ComponentId>>,
    total_weight: Arc<AtomicUsize>,
    component_count: Arc<AtomicUsize>,
    id_gen: Arc<IdGenerator>,
}

impl ComponentGraph {
    pub fn new() -> Self {
        Self {
            components: DashMap::new(),
            name_index: DashMap::new(),
            dependencies: DashMap::new(),
            dependents: DashMap::new(),
            total_weight: Arc::new(AtomicUsize::new(0)),
            component_count: Arc::new(AtomicUsize::new(0)),
            id_gen: Arc::new(IdGenerator::new()),
        }
    }
    pub fn add_component(&self, mut component: Component) -> ComponentId {
        if component.id.as_u64() == 0 {
            component.id = self.id_gen.next();
        }

        let id = component.id;
        let name = component.name.clone();
        let weight = component.weight;
        self.name_index.insert(name, id);
        self.dependencies.insert(id, HashSet::new());
        self.dependents.insert(id, HashSet::new());
        self.components.insert(id, component);
        self.component_count.fetch_add(1, Ordering::SeqCst);
        self.total_weight.fetch_add(
            (weight * 100.0).clamp(0.0, usize::MAX as f64) as usize,
            Ordering::SeqCst,
        );

        id
    }
    pub fn add_dependency(&self, from_id: ComponentId, to_id: ComponentId) -> Result<()> {
        if !self.components.contains_key(&from_id) {
            return Err(CompilerError::ComponentNotFound(from_id));
        }
        if !self.components.contains_key(&to_id) {
            return Err(CompilerError::ComponentNotFound(to_id));
        }
        self.dependencies.entry(from_id).or_default().insert(to_id);

        self.dependents.entry(to_id).or_default().insert(from_id);

        Ok(())
    }

    pub fn get(&self, id: &ComponentId) -> Option<Component> {
        self.components.get(id).map(|r| r.clone())
    }
    pub fn get_by_name(&self, name: &str) -> Option<Component> {
        self.name_index
            .get(name)
            .and_then(|id| self.components.get(&*id))
            .map(|r| r.clone())
    }
    pub fn get_dependencies(&self, id: &ComponentId) -> HashSet<ComponentId> {
        self.dependencies
            .get(id)
            .map(|deps| deps.clone())
            .unwrap_or_default()
    }
    pub fn get_dependents(&self, id: &ComponentId) -> HashSet<ComponentId> {
        self.dependents
            .get(id)
            .map(|deps| deps.clone())
            .unwrap_or_default()
    }
    pub fn component_ids(&self) -> Vec<ComponentId> {
        self.components.iter().map(|r| *r.key()).collect()
    }
    pub fn components(&self) -> Vec<Component> {
        self.components.iter().map(|r| r.value().clone()).collect()
    }
    pub fn total_weight(&self) -> f64 {
        self.total_weight.load(Ordering::Relaxed) as f64 / 100.0
    }
    pub fn len(&self) -> usize {
        self.component_count.load(Ordering::Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn validate(&self) -> Result<()> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();

        for id in self.component_ids() {
            if !visited.contains(&id) {
                if let Some(cycle) = self.detect_cycle_dfs(id, &mut visited, &mut rec_stack) {
                    return Err(CompilerError::CircularDependency(cycle));
                }
            }
        }

        Ok(())
    }

    fn detect_cycle_dfs(
        &self,
        node: ComponentId,
        visited: &mut HashSet<ComponentId>,
        rec_stack: &mut HashSet<ComponentId>,
    ) -> Option<Vec<ComponentId>> {
        visited.insert(node);
        rec_stack.insert(node);

        if let Some(deps) = self.dependencies.get(&node) {
            for &neighbor in deps.iter() {
                if !visited.contains(&neighbor) {
                    if let Some(cycle) = self.detect_cycle_dfs(neighbor, visited, rec_stack) {
                        return Some(cycle);
                    }
                } else if rec_stack.contains(&neighbor) {
                    return Some(vec![node, neighbor]);
                }
            }
        }

        rec_stack.remove(&node);
        None
    }
    /// Returns the number of outgoing dependency edges for each component
    /// (i.e. how many other components each node directly depends on).
    ///
    /// Despite the historical name "in_degrees", this counts out-edges, not in-edges.
    /// The topological sorters use this to find leaf nodes (count == 0) and process
    /// the graph bottom-up: leaves first, root last — the correct SSR render order.
    pub fn calculate_out_degrees(&self) -> HashMap<ComponentId, usize> {
        let mut out_degrees = HashMap::new();
        for id in self.component_ids() {
            let dep_count = self
                .dependencies
                .get(&id)
                .map(|deps| deps.len())
                .unwrap_or(0);
            out_degrees.insert(id, dep_count);
        }

        out_degrees
    }

    /// Get a component by ID (needed for caching)
    pub fn get_component(&self, id: ComponentId) -> Option<Component> {
        self.components.get(&id).map(|r| r.clone())
    }

    /// Get all components as an iterator
    pub fn iter_components(&self) -> impl Iterator<Item = Component> + '_ {
        self.components.iter().map(|r| r.value().clone())
    }
}

impl Default for ComponentGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_component() {
        let graph = ComponentGraph::new();

        let comp1 = Component::new(ComponentId::new(0), "Button".to_string());
        let id1 = graph.add_component(comp1);

        assert_eq!(graph.len(), 1);
        assert!(graph.get(&id1).is_some());
        assert!(graph.get_by_name("Button").is_some());
    }

    #[test]
    fn test_add_dependency() {
        let graph = ComponentGraph::new();

        let comp1 = Component::new(ComponentId::new(0), "App".to_string());
        let comp2 = Component::new(ComponentId::new(0), "Button".to_string());

        let id1 = graph.add_component(comp1);
        let id2 = graph.add_component(comp2);

        graph.add_dependency(id1, id2).unwrap();

        let deps = graph.get_dependencies(&id1);
        assert!(deps.contains(&id2));

        let dependents = graph.get_dependents(&id2);
        assert!(dependents.contains(&id1));
    }

    #[test]
    fn test_cycle_detection() {
        let graph = ComponentGraph::new();

        let comp1 = Component::new(ComponentId::new(0), "A".to_string());
        let comp2 = Component::new(ComponentId::new(0), "B".to_string());
        let comp3 = Component::new(ComponentId::new(0), "C".to_string());

        let id1 = graph.add_component(comp1);
        let id2 = graph.add_component(comp2);
        let id3 = graph.add_component(comp3);

        graph.add_dependency(id1, id2).unwrap();
        graph.add_dependency(id2, id3).unwrap();
        graph.add_dependency(id3, id1).unwrap();
        assert!(graph.validate().is_err());
    }

    #[test]
    fn test_no_cycle() {
        let graph = ComponentGraph::new();

        let comp1 = Component::new(ComponentId::new(0), "A".to_string());
        let comp2 = Component::new(ComponentId::new(0), "B".to_string());
        let comp3 = Component::new(ComponentId::new(0), "C".to_string());

        let id1 = graph.add_component(comp1);
        let id2 = graph.add_component(comp2);
        let id3 = graph.add_component(comp3);

        graph.add_dependency(id1, id2).unwrap();
        graph.add_dependency(id2, id3).unwrap();

        assert!(graph.validate().is_ok());
    }
}
