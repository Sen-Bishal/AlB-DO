pub mod adaptive;
pub mod analyzer;
pub mod benchmark;
pub mod bundler;
pub mod dev_contract;
pub mod effects;
pub mod estimator;
pub mod graph;
pub mod hydration;
pub mod incremental;
pub mod ir;
pub mod manifest;
pub mod parallel;
pub mod parallel_topo;
pub mod parser;
pub mod runtime;
pub mod scanner;
pub mod showcase;
pub mod topological;
pub mod types;

use crate::graph::ComponentGraph;
use crate::incremental::IncrementalCache;
use crate::parallel::ParallelAnalyzer;
use crate::parallel_topo::{find_critical_path_parallel, ParallelTopologicalSorter};
use crate::types::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub struct RenderCompiler {
    graph: ComponentGraph,
    cache: Option<IncrementalCache>,
}

impl RenderCompiler {
    pub fn new() -> Self {
        Self {
            graph: ComponentGraph::new(),
            cache: None,
        }
    }

    pub fn with_cache(cache_dir: PathBuf) -> Self {
        let cache = IncrementalCache::new(cache_dir);

        if let Err(e) = cache.load() {
            eprintln!("Warning: Failed to load cache: {}", e);
        }

        Self {
            graph: ComponentGraph::new(),
            cache: Some(cache),
        }
    }

    pub fn add_component(&mut self, component: Component) -> ComponentId {
        self.graph.add_component(component)
    }

    pub fn add_dependency(&mut self, from: ComponentId, to: ComponentId) -> Result<()> {
        self.graph.add_dependency(from, to)
    }

    pub fn graph(&self) -> &ComponentGraph {
        &self.graph
    }

    pub fn optimize(&self) -> Result<OptimizationResult> {
        let start = Instant::now();

        self.graph.validate()?;

        let analyzer = ParallelAnalyzer::new(&self.graph);
        let analyses = analyzer.analyze()?;

        let sorter = ParallelTopologicalSorter::new(&self.graph);
        let levels = sorter.sort_with_priority(&analyses)?;

        let batches = sorter.create_batches(levels, &analyses);

        let critical_path = find_critical_path_parallel(&self.graph, &analyses);

        let total_weight_kb = self.graph.total_weight() / 1024.0;
        let optimization_time_ms = start.elapsed().as_millis();

        let sequential_time: f64 = analyses.values().map(|a| a.estimated_time_ms).sum();

        let parallel_time: f64 = batches.iter().map(|b| b.estimated_time_ms).sum();

        let estimated_improvement_ms = sequential_time - parallel_time;

        Ok(OptimizationResult {
            version: "1.0".to_string(),
            generated_at: current_timestamp(),
            critical_path,
            parallel_batches: batches,
            metrics: OptimizationMetrics {
                total_components: self.graph.len(),
                total_weight_kb,
                optimization_time_ms,
                estimated_improvement_ms,
            },
        })
    }

    pub fn export_json(&self) -> Result<String> {
        let result = self.optimize()?;
        serde_json::to_string_pretty(&result)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    pub fn optimize_canonical_ir(&self) -> Result<ir::CanonicalIrDocument> {
        self.graph.validate()?;
        let analyzer = ParallelAnalyzer::new(&self.graph);
        let analyses = analyzer.analyze()?;
        Ok(ir::build_canonical_ir_from_graph(&self.graph, &analyses))
    }

    pub fn export_canonical_ir_json(&self) -> Result<String> {
        let ir = self.optimize_canonical_ir()?;
        serde_json::to_string_pretty(&ir).map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    pub fn manifest_v2_from_result(
        &self,
        result: &OptimizationResult,
    ) -> manifest::schema::RenderManifestV2 {
        manifest::build_render_manifest_v2(
            &self.graph,
            result,
            &manifest::ManifestOptions::default(),
        )
    }

    pub fn optimize_manifest_v2(&self) -> Result<manifest::schema::RenderManifestV2> {
        let result = self.optimize()?;
        Ok(self.manifest_v2_from_result(&result))
    }

    pub fn export_manifest_v2_json(&self) -> Result<String> {
        let manifest = self.optimize_manifest_v2()?;
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    pub fn bundle_plan_from_manifest_v2(
        &self,
        manifest: &manifest::schema::RenderManifestV2,
        options: &bundler::BundlePlanOptions,
    ) -> bundler::BundlePlan {
        bundler::build_bundle_plan(manifest, options)
    }

    pub fn optimize_bundle_plan(&self) -> Result<bundler::BundlePlan> {
        let manifest = self.optimize_manifest_v2()?;
        Ok(self.bundle_plan_from_manifest_v2(&manifest, &bundler::BundlePlanOptions::default()))
    }

    pub fn export_bundle_plan_json(&self) -> Result<String> {
        let plan = self.optimize_bundle_plan()?;
        bundler::emit::emit_bundle_plan_json(&plan)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    pub fn emit_bundle_artifacts_from_manifest_v2(
        &self,
        manifest: &manifest::schema::RenderManifestV2,
        options: &bundler::BundlePlanOptions,
        output_dir: impl AsRef<Path>,
    ) -> Result<bundler::emit::BundleEmitReport> {
        let output_dir = output_dir.as_ref();
        let plan = self.bundle_plan_from_manifest_v2(manifest, options);
        bundler::emit::emit_bundle_artifacts_to_dir(&plan, output_dir).map_err(|e| {
            CompilerError::AnalysisFailed(format!(
                "failed to emit bundle artifacts to '{}': {e}",
                output_dir.display()
            ))
        })
    }

    pub fn emit_bundle_artifacts_from_manifest_v2_with_sources(
        &self,
        manifest: &manifest::schema::RenderManifestV2,
        module_sources: &HashMap<String, String>,
        options: &bundler::BundlePlanOptions,
        output_dir: impl AsRef<Path>,
    ) -> Result<bundler::emit::BundleEmitReport> {
        let output_dir = output_dir.as_ref();
        let plan = self.bundle_plan_from_manifest_v2(manifest, options);
        bundler::emit::emit_bundle_artifacts_to_dir_with_sources(
            &plan,
            manifest,
            module_sources,
            output_dir,
        )
        .map_err(|e| {
            CompilerError::AnalysisFailed(format!(
                "failed to emit bundle artifacts with sources to '{}': {e}",
                output_dir.display()
            ))
        })
    }

    pub fn emit_bundle_artifacts_to_dir(
        &self,
        output_dir: impl AsRef<Path>,
    ) -> Result<bundler::emit::BundleEmitReport> {
        let output_dir = output_dir.as_ref();
        let plan = self.optimize_bundle_plan()?;
        bundler::emit::emit_bundle_artifacts_to_dir(&plan, output_dir).map_err(|e| {
            CompilerError::AnalysisFailed(format!(
                "failed to emit bundle artifacts to '{}': {e}",
                output_dir.display()
            ))
        })
    }

    pub fn optimize_incremental(&mut self, file_paths: &[PathBuf]) -> Result<OptimizationResult> {
        let start = Instant::now();

        self.graph.validate()?;

        let (analyses, cache_stats) = if let Some(cache) = &self.cache {
            let changes = cache.detect_changes(file_paths);

            if changes.is_empty() {
                println!(" No changes detected - using full cache");
            } else {
                println!(" Detected changes:");
                println!("   - Changed: {} files", changes.changed_files.len());
                println!("   - New: {} files", changes.new_files.len());
                println!("   - Deleted: {} files", changes.deleted_files.len());
            }

            cache.invalidate_changed_files(&changes);

            for new_file in &changes.new_files {
                for comp_id in self.graph.component_ids() {
                    if let Some(component) = self.graph.get_component(comp_id) {
                        if Path::new(&component.file_path) == new_file.as_path() {
                            cache.invalidate_component(
                                comp_id,
                                incremental::InvalidationReason::NewComponent,
                            );
                        }
                    }
                }
            }

            let invalidated = cache.get_invalidated_components();
            let total_components = self.graph.len();
            let cached_count = total_components.saturating_sub(invalidated.len());

            println!(
                "Cache status: {}/{} components cached ({:.1}%)",
                cached_count,
                total_components,
                if total_components > 0 {
                    (cached_count as f64 / total_components as f64) * 100.0
                } else {
                    0.0
                }
            );

            let mut analyses = std::collections::HashMap::new();

            for comp_id in self.graph.component_ids() {
                if let Some(cached) = cache.get_cached_analysis(comp_id) {
                    analyses.insert(comp_id, cached.analysis);
                }
            }

            if !invalidated.is_empty() || !changes.new_files.is_empty() {
                let reanalyze_count = invalidated.len() + changes.new_files.len();
                println!(" Reanalyzing {} components...", reanalyze_count);

                let analyzer = ParallelAnalyzer::new(&self.graph);
                let new_analyses = analyzer.analyze()?;

                for id in invalidated.iter() {
                    if let Some(analysis) = new_analyses.get(id) {
                        analyses.insert(*id, analysis.clone());

                        if let Some(component) = self.graph.get_component(*id) {
                            let file_path = PathBuf::from(&component.file_path);
                            if let Err(e) =
                                cache.cache_analysis(component.clone(), analysis.clone(), file_path)
                            {
                                eprintln!("Warning: Failed to cache component {:?}: {}", id, e);
                            }
                        }
                    }
                }

                for comp_id in self.graph.component_ids() {
                    if let Some(component) = self.graph.get_component(comp_id) {
                        let file_path = PathBuf::from(&component.file_path);
                        if changes.new_files.contains(&file_path) {
                            if let Some(analysis) = new_analyses.get(&comp_id) {
                                analyses.insert(comp_id, analysis.clone());
                                if let Err(e) = cache.cache_analysis(
                                    component.clone(),
                                    analysis.clone(),
                                    file_path,
                                ) {
                                    eprintln!(
                                        "Warning: Failed to cache new component {:?}: {}",
                                        comp_id, e
                                    );
                                }
                            }
                        }
                    }
                }
            }

            for comp_id in self.graph.component_ids() {
                if let Some(component) = self.graph.get_component(comp_id) {
                    cache.update_dependencies(comp_id, component.dependencies.clone());
                }
            }

            let invalidated_count = cache.get_invalidated_components().len();

            cache.clear_invalidated();

            let mut stats = cache.get_stats();
            stats.invalidated = invalidated_count;
            (analyses, Some(stats))
        } else {
            let analyzer = ParallelAnalyzer::new(&self.graph);
            (analyzer.analyze()?, None)
        };

        let sorter = ParallelTopologicalSorter::new(&self.graph);
        let levels = sorter.sort_with_priority(&analyses)?;

        let batches = sorter.create_batches(levels, &analyses);

        let critical_path = find_critical_path_parallel(&self.graph, &analyses);

        let total_weight_kb = self.graph.total_weight() / 1024.0;
        let optimization_time_ms = start.elapsed().as_millis();

        let sequential_time: f64 = analyses.values().map(|a| a.estimated_time_ms).sum();

        let parallel_time: f64 = batches.iter().map(|b| b.estimated_time_ms).sum();

        let estimated_improvement_ms = sequential_time - parallel_time;

        if let Some(stats) = cache_stats {
            println!(" Cache performance:");
            println!("   - Hit rate: {:.1}%", stats.cache_hit_rate * 100.0);
            println!("   - Total cached: {}", stats.total_cached);
            println!("   - Files tracked: {}", stats.files_tracked);
        }

        Ok(OptimizationResult {
            version: "1.0".to_string(),
            generated_at: current_timestamp(),
            critical_path,
            parallel_batches: batches,
            metrics: OptimizationMetrics {
                total_components: self.graph.len(),
                total_weight_kb,
                optimization_time_ms,
                estimated_improvement_ms,
            },
        })
    }

    pub fn save_cache(&self) -> std::io::Result<()> {
        if let Some(cache) = &self.cache {
            cache.save()?;
            println!(" Cache saved successfully");
        }
        Ok(())
    }

    pub fn cache_stats(&self) -> Option<incremental::CacheStats> {
        self.cache.as_ref().map(|c| c.get_stats())
    }
}

impl Default for RenderCompiler {
    fn default() -> Self {
        Self::new()
    }
}

fn current_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_example_app() -> RenderCompiler {
        let mut compiler = RenderCompiler::new();

        let mut app = Component::new(ComponentId::new(0), "App".to_string());
        app.weight = 50.0;
        app.bitrate = 1000.0;

        let mut header = Component::new(ComponentId::new(0), "Header".to_string());
        header.weight = 100.0;
        header.bitrate = 500.0;
        header.is_above_fold = true;

        let mut nav = Component::new(ComponentId::new(0), "Navigation".to_string());
        nav.weight = 200.0;
        nav.bitrate = 300.0;
        nav.is_interactive = true;

        let mut hero = Component::new(ComponentId::new(0), "HeroImage".to_string());
        hero.weight = 500.0;
        hero.bitrate = 100.0;
        hero.is_lcp_candidate = true;
        hero.is_above_fold = true;

        let mut footer = Component::new(ComponentId::new(0), "Footer".to_string());
        footer.weight = 150.0;
        footer.bitrate = 200.0;

        let app_id = compiler.add_component(app);
        let header_id = compiler.add_component(header);
        let nav_id = compiler.add_component(nav);
        let hero_id = compiler.add_component(hero);
        let footer_id = compiler.add_component(footer);

        compiler.add_dependency(header_id, nav_id).unwrap();
        compiler.add_dependency(app_id, header_id).unwrap();
        compiler.add_dependency(app_id, hero_id).unwrap();
        compiler.add_dependency(app_id, footer_id).unwrap();

        compiler
    }

    #[test]
    fn test_full_optimization() {
        let compiler = create_example_app();
        let result = compiler.optimize().unwrap();

        assert_eq!(result.metrics.total_components, 5);
        assert!(result.metrics.total_weight_kb > 0.0);
        assert!(!result.parallel_batches.is_empty());
    }

    #[test]
    fn test_json_export() {
        let compiler = create_example_app();
        let json = compiler.export_json().unwrap();

        assert!(json.contains("version"));
        assert!(json.contains("parallel_batches"));
        assert!(json.contains("critical_path"));
    }

    #[test]
    fn test_canonical_ir_export() {
        let compiler = create_example_app();
        let json = compiler.export_canonical_ir_json().unwrap();
        assert!(json.contains("schema_version"));
        assert!(json.contains("\"1.0\""));
        assert!(json.contains("components"));
        assert!(json.contains("edges"));
    }

    #[test]
    fn test_manifest_v2_export() {
        let compiler = create_example_app();
        let json = compiler.export_manifest_v2_json().unwrap();

        assert!(json.contains("schema_version"));
        assert!(json.contains("\"2.0\""));
        assert!(json.contains("parallel_batches"));
        assert!(json.contains("critical_path"));
    }

    #[test]
    fn test_bundle_plan_export() {
        let compiler = create_example_app();
        let json = compiler.export_bundle_plan_json().unwrap();

        assert!(json.contains("rewrite_actions"));
        assert!(json.contains("\"version\": \"1.0\""));
    }

    #[test]
    fn test_emit_bundle_artifacts_to_dir() {
        let compiler = create_example_app();
        let temp_dir = tempfile::tempdir().unwrap();

        let report = compiler
            .emit_bundle_artifacts_to_dir(temp_dir.path())
            .unwrap();
        assert!(!report.artifacts.is_empty());
        assert!(temp_dir.path().join("bundle-plan.json").is_file());
    }

    #[test]
    fn test_priority_ordering() {
        let compiler = create_example_app();
        let result = compiler.optimize().unwrap();

        let hero_id = compiler.graph().get_by_name("HeroImage").unwrap().id;

        let in_critical = result.critical_path.contains(&hero_id);
        let in_early_batch = result
            .parallel_batches
            .iter()
            .take(2)
            .any(|b| b.components.contains(&hero_id));

        assert!(in_critical || in_early_batch);
    }
}
