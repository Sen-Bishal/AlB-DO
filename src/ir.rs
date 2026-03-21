use crate::effects::EffectProfile;
use crate::graph::ComponentGraph;
use crate::parser::ParsedComponent;
use crate::types::{ComponentAnalysis, ComponentId};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub const CANONICAL_IR_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum IrExportKind {
    Named,
    Default,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanonicalIrModule {
    pub module_path: String,
    pub source_hash: u64,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalIrComponent {
    pub id: u64,
    pub symbol: String,
    pub module_path: String,
    pub export_kind: IrExportKind,
    pub line_number: usize,
    pub estimated_size_bytes: u64,
    pub effects: EffectProfile,
    pub source_hash: u64,
    pub legacy_priority: Option<f64>,
    pub legacy_phase: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CanonicalIrEdge {
    pub from: u64,
    pub to: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanonicalIrDocument {
    pub schema_version: String,
    pub generated_at: String,
    pub modules: Vec<CanonicalIrModule>,
    pub components: Vec<CanonicalIrComponent>,
    pub edges: Vec<CanonicalIrEdge>,
}

impl CanonicalIrDocument {
    pub fn canonical_hash(&self) -> u64 {
        let mut normalized = self.clone();
        normalized.normalize();
        normalized.generated_at = "<normalized>".to_string();
        let encoded = serde_json::to_vec(&normalized).unwrap_or_default();
        let mut hasher = DefaultHasher::new();
        encoded.hash(&mut hasher);
        hasher.finish()
    }

    pub fn normalize(&mut self) {
        self.modules
            .sort_by(|left, right| left.module_path.cmp(&right.module_path));
        for module in &mut self.modules {
            module.imports.sort();
            module.imports.dedup();
        }

        self.components.sort_by(|left, right| {
            left.id
                .cmp(&right.id)
                .then_with(|| left.symbol.cmp(&right.symbol))
                .then_with(|| left.module_path.cmp(&right.module_path))
        });

        self.edges
            .sort_by(|left, right| left.from.cmp(&right.from).then(left.to.cmp(&right.to)));
    }
}

pub fn build_canonical_ir_from_parsed(components: &[ParsedComponent]) -> CanonicalIrDocument {
    let mut parsed_components = components.to_vec();
    parsed_components.sort_by(|left, right| {
        left.file_path
            .cmp(&right.file_path)
            .then(left.name.cmp(&right.name))
    });

    let mut id_map = BTreeMap::new();
    for (idx, component) in parsed_components.iter().enumerate() {
        id_map.insert(component.name.clone(), (idx + 1) as u64);
    }

    let modules = build_modules(&parsed_components);
    let mut ir_components = Vec::with_capacity(parsed_components.len());
    let mut edges = Vec::new();

    for component in &parsed_components {
        let id = *id_map
            .get(&component.name)
            .expect("component id should be assigned");
        ir_components.push(CanonicalIrComponent {
            id,
            symbol: component.name.clone(),
            module_path: component.file_path.clone(),
            export_kind: if component.is_default_export {
                IrExportKind::Default
            } else {
                IrExportKind::Named
            },
            line_number: component.line_number,
            estimated_size_bytes: component.estimated_size as u64,
            effects: component.effect_profile,
            source_hash: component.source_hash,
            legacy_priority: None,
            legacy_phase: None,
        });

        let mut unique_imports = HashSet::new();
        for import in &component.imports {
            if unique_imports.insert(import) {
                if let Some(target) = id_map.get(import) {
                    edges.push(CanonicalIrEdge {
                        from: id,
                        to: *target,
                    });
                }
            }
        }
    }

    let mut document = CanonicalIrDocument {
        schema_version: CANONICAL_IR_SCHEMA_VERSION.to_string(),
        generated_at: now_rfc3339(),
        modules,
        components: ir_components,
        edges,
    };
    document.normalize();
    document
}

pub fn build_canonical_ir_from_graph(
    graph: &ComponentGraph,
    analyses: &HashMap<ComponentId, ComponentAnalysis>,
) -> CanonicalIrDocument {
    let mut modules_by_path: BTreeMap<String, CanonicalIrModule> = BTreeMap::new();
    let mut components = Vec::new();
    let mut edges = Vec::new();

    let mut graph_components = graph.components();
    graph_components.sort_by(|left, right| left.id.as_u64().cmp(&right.id.as_u64()));

    for component in graph_components {
        modules_by_path
            .entry(component.file_path.clone())
            .or_insert_with(|| CanonicalIrModule {
                module_path: component.file_path.clone(),
                source_hash: component.source_hash,
                imports: Vec::new(),
            });

        if let Some(module) = modules_by_path.get_mut(&component.file_path) {
            let mut dependency_names = graph
                .get_dependencies(&component.id)
                .into_iter()
                .filter_map(|dep| graph.get(&dep).map(|resolved| resolved.name))
                .collect::<Vec<_>>();
            dependency_names.sort();
            dependency_names.dedup();
            module.imports.extend(dependency_names);
            module.imports.sort();
            module.imports.dedup();
        }

        let analysis = analyses.get(&component.id);
        components.push(CanonicalIrComponent {
            id: component.id.as_u64(),
            symbol: component.name.clone(),
            module_path: component.file_path.clone(),
            export_kind: IrExportKind::Named,
            line_number: component.line_number,
            estimated_size_bytes: component.weight.max(0.0).round() as u64,
            effects: component.effect_profile,
            source_hash: component.source_hash,
            legacy_priority: analysis.map(|entry| entry.priority),
            legacy_phase: analysis.map(|entry| entry.phase),
        });

        let mut dependency_ids = graph
            .get_dependencies(&component.id)
            .into_iter()
            .map(|id| id.as_u64())
            .collect::<Vec<_>>();
        dependency_ids.sort_unstable();
        dependency_ids.dedup();
        for target in dependency_ids {
            edges.push(CanonicalIrEdge {
                from: component.id.as_u64(),
                to: target,
            });
        }
    }

    let mut document = CanonicalIrDocument {
        schema_version: CANONICAL_IR_SCHEMA_VERSION.to_string(),
        generated_at: now_rfc3339(),
        modules: modules_by_path.into_values().collect(),
        components,
        edges,
    };
    document.normalize();
    document
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationField {
    pub legacy_source: String,
    pub canonical_target: String,
    pub transform: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigrationMap {
    pub version: String,
    pub parser_to_ir: Vec<MigrationField>,
    pub analyzer_to_ir: Vec<MigrationField>,
}

pub fn default_migration_map() -> MigrationMap {
    let parser_to_ir = vec![
        MigrationField {
            legacy_source: "ParsedComponent.name".to_string(),
            canonical_target: "CanonicalIrComponent.symbol".to_string(),
            transform: "identity".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.file_path".to_string(),
            canonical_target: "CanonicalIrComponent.module_path".to_string(),
            transform: "identity".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.is_default_export".to_string(),
            canonical_target: "CanonicalIrComponent.export_kind".to_string(),
            transform: "bool_to_enum(default|named)".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.estimated_size".to_string(),
            canonical_target: "CanonicalIrComponent.estimated_size_bytes".to_string(),
            transform: "usize_to_u64".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.imports".to_string(),
            canonical_target: "CanonicalIrModule.imports + CanonicalIrEdge".to_string(),
            transform: "symbol_resolution + dedup + deterministic_sort".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.effect_profile".to_string(),
            canonical_target: "CanonicalIrComponent.effects".to_string(),
            transform: "identity".to_string(),
        },
        MigrationField {
            legacy_source: "ParsedComponent.source_hash".to_string(),
            canonical_target: "CanonicalIrModule.source_hash + CanonicalIrComponent.source_hash"
                .to_string(),
            transform: "identity".to_string(),
        },
    ];

    let analyzer_to_ir = vec![
        MigrationField {
            legacy_source: "ComponentAnalysis.priority".to_string(),
            canonical_target: "CanonicalIrComponent.legacy_priority".to_string(),
            transform: "copy_optional".to_string(),
        },
        MigrationField {
            legacy_source: "ComponentAnalysis.phase".to_string(),
            canonical_target: "CanonicalIrComponent.legacy_phase".to_string(),
            transform: "copy_optional".to_string(),
        },
        MigrationField {
            legacy_source: "ComponentAnalysis.topological_level".to_string(),
            canonical_target: "CanonicalIrEdge ordering".to_string(),
            transform: "runtime_order_projection".to_string(),
        },
    ];

    MigrationMap {
        version: "1.0".to_string(),
        parser_to_ir,
        analyzer_to_ir,
    }
}

fn build_modules(parsed_components: &[ParsedComponent]) -> Vec<CanonicalIrModule> {
    let mut modules = BTreeMap::<String, CanonicalIrModule>::new();
    for component in parsed_components {
        let entry = modules
            .entry(component.file_path.clone())
            .or_insert_with(|| CanonicalIrModule {
                module_path: component.file_path.clone(),
                source_hash: component.source_hash,
                imports: Vec::new(),
            });
        entry.imports.extend(component.imports.iter().cloned());
    }

    let mut output = modules.into_values().collect::<Vec<_>>();
    for module in &mut output {
        module.imports.sort_by(|left, right| {
            if left == right {
                Ordering::Equal
            } else {
                left.cmp(right)
            }
        });
        module.imports.dedup();
    }
    output
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_component(name: &str, file: &str, imports: Vec<&str>) -> ParsedComponent {
        ParsedComponent {
            name: name.to_string(),
            file_path: file.to_string(),
            line_number: 1,
            imports: imports.into_iter().map(|value| value.to_string()).collect(),
            estimated_size: 100,
            is_default_export: false,
            props: Vec::new(),
            effect_profile: EffectProfile::default(),
            source_hash: 42,
        }
    }

    #[test]
    fn test_canonical_ir_from_parsed_is_deterministic() {
        let input = vec![
            fixture_component("Button", "src/Button.tsx", vec![]),
            fixture_component("App", "src/App.tsx", vec!["Button"]),
        ];

        let first = build_canonical_ir_from_parsed(&input);
        let second = build_canonical_ir_from_parsed(&input);
        assert_eq!(first.canonical_hash(), second.canonical_hash());
    }

    #[test]
    fn test_default_migration_map_contains_core_fields() {
        let map = default_migration_map();
        assert!(map
            .parser_to_ir
            .iter()
            .any(|entry| entry.legacy_source == "ParsedComponent.name"));
        assert!(map
            .analyzer_to_ir
            .iter()
            .any(|entry| entry.legacy_source == "ComponentAnalysis.priority"));
    }
}
