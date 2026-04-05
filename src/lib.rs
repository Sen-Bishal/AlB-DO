//! # dom-render-compiler
//!
//! The core compiler crate for [AlBDO](https://albedo.dev) — a Rust-native DOM render compiler
//! and HTTP runtime for JSX/TSX applications.
//!
//! This crate is responsible for the full compilation pipeline:
//!
//! 1. **Parsing** — SWC-powered JSX/TSX ingestion with effect inference
//! 2. **Graph construction** — [`ComponentGraph`] built from scanned source files
//! 3. **Analysis** — [`ParallelAnalyzer`] assigns effect profiles and weight estimates
//! 4. **Scheduling** — topological sort + critical-path scoring for parallel render batching
//! 5. **Manifest emission** — [`RenderManifestV2`] consumed by `albedo-server`
//! 6. **Bundling** — classify → plan → rewrite → emit pipeline in [`bundler`]
//!
//! ## Effect Lattice — Hydration Tiers
//!
//! Every component is classified into one of three tiers at compile time:
//!
//! | Tier | Profile | Client JS |
//! |------|---------|----------|
//! | A | No hooks, no async, no side effects | Zero bytes |
//! | B | Light interactivity / event handlers | Island only |
//! | C | Full hook surface, async I/O | Full hydration |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use dom_render_compiler::RenderCompiler;
//!
//! let compiler = RenderCompiler::new();
//! let manifest = compiler.optimize_manifest_v2().unwrap();
//! println!("{} components compiled", manifest.components.len());
//! ```

#![deny(clippy::all)]
#![warn(clippy::pedantic)]
#![warn(clippy::nursery)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![deny(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::as_conversions,
    clippy::shadow_unrelated,
    clippy::todo
)]
#![warn(clippy::expect_used)]
#![warn(clippy::integer_arithmetic)]
#![warn(rustdoc::broken_intra_doc_links)]
#![warn(rustdoc::missing_crate_level_docs)]
#![warn(rustdoc::invalid_codeblock_attributes)]

pub mod analysis;
pub mod bundler;
pub mod dev;
pub mod effects;
pub mod estimator;
pub mod graph;
pub mod hydration;
pub mod incremental;
pub mod ir;
pub mod manifest;
pub mod parser;
pub mod runtime;
pub mod scanner;
pub mod types;

pub use analysis::adaptive;
pub use analysis::analyzer;
pub use analysis::parallel;
pub use analysis::parallel_topo;
pub use analysis::topological;
pub use dev::benchmark;
pub use dev::contract as dev_contract;
pub use dev::showcase;

use crate::graph::ComponentGraph;
use crate::incremental::IncrementalCache;
use crate::parallel::ParallelAnalyzer;
use crate::parallel_topo::{find_critical_path_parallel, ParallelTopologicalSorter};
use crate::types::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// The primary facade for the AlBDO compilation pipeline.
///
/// Owns a [`ComponentGraph`] and an optional [`IncrementalCache`]. Drives analysis,
/// scheduling, manifest generation, and bundle emission in a single API surface.
///
/// # Examples
///
/// ```rust,no_run
/// use dom_render_compiler::RenderCompiler;
///
/// let compiler = RenderCompiler::new();
/// let json = compiler.export_manifest_v2_json().unwrap();
/// ```
pub struct RenderCompiler {
    graph: ComponentGraph,
    cache: Option<IncrementalCache>,
}

impl RenderCompiler {
    /// Creates a new [`RenderCompiler`] with an empty graph and no cache.
    pub fn new() -> Self {
        Self {
            graph: ComponentGraph::new(),
            cache: None,
        }
    }

    /// Creates a [`RenderCompiler`] backed by a persistent [`IncrementalCache`] at `cache_dir`.
    ///
    /// The cache is loaded eagerly. A load failure is logged as a warning and the compiler
    /// continues without cached state rather than returning an error.
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

    /// Inserts a [`Component`] into the graph and returns its assigned [`ComponentId`].
    pub fn add_component(&mut self, component: Component) -> ComponentId {
        self.graph.add_component(component)
    }

    /// Records a render dependency edge `from` → `to` in the graph.
    ///
    /// Returns an error if adding the edge would introduce a cycle.
    pub fn add_dependency(&mut self, from: ComponentId, to: ComponentId) -> Result<()> {
        self.graph.add_dependency(from, to)
    }

    /// Returns a shared reference to the underlying [`ComponentGraph`].
    pub fn graph(&self) -> &ComponentGraph {
        &self.graph
    }

    /// Runs the full analysis and scheduling pipeline.
    ///
    /// Validates the graph for cycles, runs [`ParallelAnalyzer`], computes a topological
    /// sort with priority scoring, and returns an [`OptimizationResult`] containing the
    /// critical path and parallel render batches.
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

    /// Runs [`Self::optimize`] and serializes the result to a pretty-printed JSON string.
    pub fn export_json(&self) -> Result<String> {
        let result = self.optimize()?;
        serde_json::to_string_pretty(&result)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    /// Produces a [`ir::CanonicalIrDocument`] from the current graph state.
    ///
    /// The canonical IR is a stable, version-tagged representation of the component graph
    /// and effect analyses, suitable for offline inspection and tooling interop.
    pub fn optimize_canonical_ir(&self) -> Result<ir::CanonicalIrDocument> {
        self.graph.validate()?;
        let analyzer = ParallelAnalyzer::new(&self.graph);
        let analyses = analyzer.analyze()?;
        Ok(ir::build_canonical_ir_from_graph(&self.graph, &analyses))
    }

    /// Runs [`Self::optimize_canonical_ir`] and serializes to a pretty-printed JSON string.
    pub fn export_canonical_ir_json(&self) -> Result<String> {
        let ir = self.optimize_canonical_ir()?;
        serde_json::to_string_pretty(&ir).map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    /// Builds a [`manifest::schema::RenderManifestV2`] from an existing [`OptimizationResult`].
    ///
    /// Use this when you already hold an `OptimizationResult` and want to avoid re-running
    /// analysis. For the common one-shot case, prefer [`Self::optimize_manifest_v2`].
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

    /// Runs the full pipeline and returns a [`manifest::schema::RenderManifestV2`].
    ///
    /// This is the primary output consumed by `albedo-server` to configure the HTTP runtime.
    pub fn optimize_manifest_v2(&self) -> Result<manifest::schema::RenderManifestV2> {
        let result = self.optimize()?;
        Ok(self.manifest_v2_from_result(&result))
    }

    /// Runs [`Self::optimize_manifest_v2`] and serializes to a pretty-printed JSON string.
    pub fn export_manifest_v2_json(&self) -> Result<String> {
        let manifest = self.optimize_manifest_v2()?;
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    /// Derives a [`bundler::BundlePlan`] from an existing manifest and bundle options.
    ///
    /// Prefer [`Self::optimize_bundle_plan`] for the common one-shot case.
    pub fn bundle_plan_from_manifest_v2(
        &self,
        manifest: &manifest::schema::RenderManifestV2,
        options: &bundler::BundlePlanOptions,
    ) -> bundler::BundlePlan {
        bundler::build_bundle_plan(manifest, options)
    }

    /// Runs the full pipeline and returns a [`bundler::BundlePlan`].
    pub fn optimize_bundle_plan(&self) -> Result<bundler::BundlePlan> {
        let manifest = self.optimize_manifest_v2()?;
        Ok(self.bundle_plan_from_manifest_v2(&manifest, &bundler::BundlePlanOptions::default()))
    }

    /// Runs [`Self::optimize_bundle_plan`] and serializes to a pretty-printed JSON string.
    pub fn export_bundle_plan_json(&self) -> Result<String> {
        let plan = self.optimize_bundle_plan()?;
        bundler::emit::emit_bundle_plan_json(&plan)
            .map_err(|e| CompilerError::AnalysisFailed(e.to_string()))
    }

    /// Emits bundle artifacts to `output_dir` from an existing manifest and options.
    ///
    /// Returns a [`bundler::emit::BundleEmitReport`] describing every file written.
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

    /// Emits bundle artifacts with pre-resolved module sources to `output_dir`.
    ///
    /// `module_sources` maps module paths to their source strings, allowing the bundler
    /// to inline real source content rather than emitting placeholder stubs.
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

    /// Runs the full pipeline and writes all bundle artifacts to `output_dir`.
    ///
    /// This is the one-shot emit path used by the `albedo build` CLI command.
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

    /// Runs an incremental compilation pass, reusing cached analyses for unchanged files.
    ///
    /// `file_paths` is the full list of source files in the project. The compiler diffs
    /// this list against the cache to determine which components need re-analysis.
    ///
    /// Requires the compiler to have been constructed via [`Self::with_cache`]; if no cache
    /// is present, falls back to a full non-incremental [`Self::optimize`] pass.
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

    /// Flushes the incremental cache to disk.
    ///
    /// No-op if the compiler was not constructed with [`Self::with_cache`].
    pub fn save_cache(&self) -> std::io::Result<()> {
        if let Some(cache) = &self.cache {
            cache.save()?;
            println!(" Cache saved successfully");
        }
        Ok(())
    }

    /// Returns a snapshot of cache performance metrics, or `None` if caching is disabled.
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
