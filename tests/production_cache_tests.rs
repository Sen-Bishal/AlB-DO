use dom_render_compiler::*;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::TempDir;

const PRODUCTION_COMPONENTS_DIR: &str = "tests/fixtures/components";

fn verify_production_components_exist() {
    let path = Path::new(PRODUCTION_COMPONENTS_DIR);
    assert!(
        path.exists(),
        "Production components directory not found: {}",
        PRODUCTION_COMPONENTS_DIR
    );

    let required_files = [
        "App.jsx",
        "Header.jsx",
        "Navigation.jsx",
        "Button.jsx",
        "HeroImage.jsx",
        "Features.jsx",
        "FeatureCard.jsx",
        "Footer.jsx",
    ];

    for file in &required_files {
        let file_path = path.join(file);
        assert!(
            file_path.exists(),
            "Required production file missing: {}",
            file
        );
    }
}

fn scan_and_build_compiler(
    cache_dir: Option<PathBuf>,
) -> (RenderCompiler, Vec<parser::ParsedComponent>) {
    verify_production_components_exist();

    let scanner = scanner::ProjectScanner::new();
    let components = scanner
        .scan_directory(Path::new(PRODUCTION_COMPONENTS_DIR))
        .expect("Failed to scan production components");

    let mut compiler = if let Some(dir) = cache_dir {
        RenderCompiler::with_cache(dir)
    } else {
        RenderCompiler::new()
    };

    let mut component_map = std::collections::HashMap::new();

    for parsed in &components {
        let mut component = types::Component::new(types::ComponentId::new(0), parsed.name.clone());
        component.file_path = parsed.file_path.clone();
        component.weight = (parsed.name.len() * 10) as f64;
        component.bitrate = 500.0;

        let id = compiler.add_component(component);
        component_map.insert(parsed.name.clone(), id);
    }

    for parsed in &components {
        if let Some(&from_id) = component_map.get(&parsed.name) {
            for import in &parsed.imports {
                if let Some(&to_id) = component_map.get(import) {
                    let _ = compiler.add_dependency(from_id, to_id);
                }
            }
        }
    }

    (compiler, components)
}

fn get_production_file_paths() -> Vec<PathBuf> {
    let base = PathBuf::from(PRODUCTION_COMPONENTS_DIR);
    vec![
        base.join("App.jsx"),
        base.join("Header.jsx"),
        base.join("Navigation.jsx"),
        base.join("Button.jsx"),
        base.join("HeroImage.jsx"),
        base.join("Features.jsx"),
        base.join("FeatureCard.jsx"),
        base.join("Footer.jsx"),
    ]
}

#[test]
fn test_production_baseline_compilation() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Baseline Compilation");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let temp_dir = TempDir::new().unwrap();
    let (mut compiler, components) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));

    println!("  COMPONENTS DISCOVERED: {}", components.len());
    for comp in &components {
        println!("    {} → imports: {:?}", comp.name, comp.imports);
    }

    let files = get_production_file_paths();

    println!("\n  COMPILING");
    let start = Instant::now();
    let result = compiler.optimize_incremental(&files).unwrap();
    let duration = start.elapsed();

    compiler.save_cache().unwrap();

    println!("\n  RESULTS");
    println!("    Components:       {}", result.metrics.total_components);
    println!(
        "    Total Weight:     {:.2} KB",
        result.metrics.total_weight_kb
    );
    println!("    Compile Time:     {:?}", duration);
    println!("    Parallel Batches: {}", result.parallel_batches.len());
    println!(
        "    Critical Path:    {} components",
        result.critical_path.len()
    );
    println!(
        "    Improvement:      {:.0}ms saved",
        result.metrics.estimated_improvement_ms
    );

    assert_eq!(
        result.metrics.total_components, 8,
        "Should have 8 production components"
    );
    assert!(
        !result.parallel_batches.is_empty(),
        "Should create parallel batches"
    );

    println!("\n  [OK] PASSED");
}

#[test]
fn test_production_cache_effectiveness() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Cache Performance");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let temp_dir = TempDir::new().unwrap();
    let files = get_production_file_paths();

    println!("  COLD START (no cache)");
    let (mut compiler1, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));

    let start = Instant::now();
    compiler1.optimize_incremental(&files).unwrap();
    let cold_time = start.elapsed();

    compiler1.save_cache().unwrap();
    println!("    Time: {:?}", cold_time);

    drop(compiler1);

    println!("\n  WARM START (with cache)");
    let (mut compiler2, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));

    let start = Instant::now();
    compiler2.optimize_incremental(&files).unwrap();
    let warm_time = start.elapsed();

    println!("    Time: {:?}", warm_time);

    let speedup = cold_time.as_secs_f64() / warm_time.as_secs_f64();

    if let Some(stats) = compiler2.cache_stats() {
        println!("\n  PERFORMANCE METRICS");
        println!("    Cold Start:       {:?}", cold_time);
        println!("    Warm Start:       {:?}", warm_time);
        println!("    Speedup:          {:.2}x", speedup);
        println!("    Cache Hit Rate:   {:.1}%", stats.cache_hit_rate * 100.0);
        println!("    Cached Components: {}/{}", stats.total_cached, 8);

        assert!(
            warm_time < cold_time,
            "Cache should make compilation faster"
        );
        assert!(speedup > 1.5, "Should achieve at least 1.5x speedup");
        assert!(
            stats.cache_hit_rate > 0.7,
            "Should have >70% cache hit rate"
        );

        println!("\n  [OK] PASSED - Cache providing {:.2}x speedup", speedup);
    }
}

#[test]
fn test_production_cache_persistence() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Cache Persistence Across Restarts");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let temp_dir = TempDir::new().unwrap();
    let cache_file = temp_dir.path().join(".dom-compiler-cache.bin");
    let files = get_production_file_paths();

    println!("  SESSION 1: Create and save cache");
    {
        let (mut compiler, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));
        compiler.optimize_incremental(&files).unwrap();
        compiler.save_cache().unwrap();

        assert!(cache_file.exists(), "Cache file should exist");
        let size = std::fs::metadata(&cache_file).unwrap().len();
        println!("    [OK] Cache saved: {} bytes", size);
    }

    println!("\n  SESSION 2: Load from disk");
    {
        assert!(cache_file.exists(), "Cache file should still exist");

        let (mut compiler, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));

        let start = Instant::now();
        compiler.optimize_incremental(&files).unwrap();
        let load_time = start.elapsed();

        if let Some(stats) = compiler.cache_stats() {
            println!("    [OK] Cache loaded successfully");
            println!("    Compile time: {:?}", load_time);
            println!("    Hit rate: {:.1}%", stats.cache_hit_rate * 100.0);
            println!("    Components from cache: {}", stats.total_cached);

            assert!(
                stats.cache_hit_rate > 0.7,
                "Should load majority from cache"
            );
            assert_eq!(stats.files_tracked, 8, "Should track all 8 files");

            println!("\n  [OK] PASSED - Cache persisted successfully");
        }
    }
}

#[test]
fn test_production_dependency_graph() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Dependency Graph Analysis");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let (compiler, components) = scan_and_build_compiler(None);

    println!("  ANALYZING DEPENDENCIES\n");

    let mut has_dependencies = false;
    for comp in &components {
        if !comp.imports.is_empty() {
            println!("    {} →", comp.name);
            for import in &comp.imports {
                println!("      ↳ {}", import);
                has_dependencies = true;
            }
        }
    }

    if !has_dependencies {
        println!("    No dependencies detected (isolated components)");
    }

    let result = compiler.optimize().unwrap();

    println!("\n  OPTIMIZATION RESULTS");
    println!("    Total Components:  {}", result.metrics.total_components);
    println!(
        "    Critical Path:     {} components",
        result.critical_path.len()
    );
    println!("    Parallel Batches:  {}", result.parallel_batches.len());

    for (i, batch) in result.parallel_batches.iter().enumerate() {
        println!(
            "    Batch {}: {} components, {}ms, deferrable: {}",
            i,
            batch.components.len(),
            batch.estimated_time_ms as u64,
            batch.can_defer
        );
    }

    assert_eq!(result.metrics.total_components, 8);

    println!("\n  [OK] PASSED - Dependency graph analyzed");
}

#[test]
fn test_production_realistic_workflow() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Real Development Workflow");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let temp_dir = TempDir::new().unwrap();
    let files = get_production_file_paths();

    println!("  SIMULATING DEVELOPER WORKFLOW\n");

    println!("  Step 1: Fresh project clone (cold start)");
    let (mut compiler1, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));
    let start = Instant::now();
    compiler1.optimize_incremental(&files).unwrap();
    let step1_time = start.elapsed();
    compiler1.save_cache().unwrap();
    println!("    Time: {:?}\n", step1_time);

    drop(compiler1);

    println!("  Step 2: Open project next day (warm cache)");
    let (mut compiler2, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));
    let start = Instant::now();
    compiler2.optimize_incremental(&files).unwrap();
    let step2_time = start.elapsed();
    println!(
        "    Time: {:?} ({:.1}x faster)\n",
        step2_time,
        step1_time.as_secs_f64() / step2_time.as_secs_f64()
    );

    drop(compiler2);

    println!("  Step 3: Recompile after code review");
    let (mut compiler3, _) = scan_and_build_compiler(Some(temp_dir.path().to_path_buf()));
    let start = Instant::now();
    compiler3.optimize_incremental(&files).unwrap();
    let step3_time = start.elapsed();
    println!(
        "    Time: {:?} ({:.1}x faster)\n",
        step3_time,
        step1_time.as_secs_f64() / step3_time.as_secs_f64()
    );

    if let Some(stats) = compiler3.cache_stats() {
        let avg_warm_time = (step2_time + step3_time).as_secs_f64() / 2.0;
        let avg_speedup = step1_time.as_secs_f64() / avg_warm_time;

        println!("  WORKFLOW SUMMARY");
        println!("    Cold start:        {:?}", step1_time);
        println!(
            "    Avg warm start:    {:.2?}",
            std::time::Duration::from_secs_f64(avg_warm_time)
        );
        println!("    Average speedup:   {:.2}x", avg_speedup);
        println!(
            "    Cache efficiency:  {:.1}%",
            stats.cache_hit_rate * 100.0
        );

        assert!(step2_time < step1_time, "Warm starts should be faster");
        assert!(step3_time < step1_time, "Subsequent runs should be faster");

        println!("\n  [OK] PASSED - Realistic workflow optimized");
    }
}

#[test]
fn test_production_component_structure() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Component Structure Validation");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let (_, components) = scan_and_build_compiler(None);

    println!("  VALIDATING STRUCTURE\n");

    let expected_components = vec![
        "App",
        "Header",
        "Navigation",
        "Button",
        "HeroImage",
        "Features",
        "FeatureCard",
        "Footer",
    ];

    for expected in &expected_components {
        let found = components.iter().any(|c| c.name == *expected);
        println!("    {} {}", if found { "[OK]" } else { "[!!]" }, expected);
        assert!(found, "Production component '{}' should be found", expected);
    }

    println!("\n  COMPONENT DETAILS");
    for comp in &components {
        let file_name = std::path::Path::new(&comp.file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");
        println!("    {} ({})", comp.name, file_name);
        println!("      Imports: {}", comp.imports.len());
        println!("      Path: {}", comp.file_path);
    }

    assert_eq!(
        components.len(),
        8,
        "Should have exactly 8 production components"
    );

    println!("\n  [OK] PASSED - All production components validated");
}

#[test]
fn test_production_optimization_metrics() {
    println!("\n  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  PRODUCTION TEST: Optimization Metrics");
    println!("  ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n");

    let (compiler, _) = scan_and_build_compiler(None);

    let start = Instant::now();
    let result = compiler.optimize().unwrap();
    let compile_time = start.elapsed();

    println!("  COMPILATION METRICS");
    println!("    Time:              {:?}", compile_time);
    println!("    Components:        {}", result.metrics.total_components);
    println!(
        "    Total Weight:      {:.2} KB",
        result.metrics.total_weight_kb
    );
    println!(
        "    Optimization Time: {}ms",
        result.metrics.optimization_time_ms
    );
    println!(
        "    Time Saved:        {:.0}ms",
        result.metrics.estimated_improvement_ms
    );

    println!("\n  BATCHING STRATEGY");
    println!("    Batches Created:   {}", result.parallel_batches.len());

    let mut total_batch_time = 0.0;
    for batch in &result.parallel_batches {
        total_batch_time += batch.estimated_time_ms;
        println!(
            "    Level {}: {} components @ {:.0}ms (defer: {})",
            batch.level,
            batch.components.len(),
            batch.estimated_time_ms,
            batch.can_defer
        );
    }

    println!("\n  PERFORMANCE ANALYSIS");
    let improvement_pct = if total_batch_time > 0.0 {
        (result.metrics.estimated_improvement_ms / total_batch_time) * 100.0
    } else {
        0.0
    };
    println!("    Improvement:       {:.1}%", improvement_pct);
    println!(
        "    Critical Path:     {}/{} components",
        result.critical_path.len(),
        result.metrics.total_components
    );

    assert!(
        result.metrics.total_weight_kb > 0.0,
        "Should have measurable weight"
    );
    assert!(
        result.metrics.estimated_improvement_ms > 0.0,
        "Should show time savings"
    );

    println!("\n  [OK] PASSED - Optimization metrics verified");
}
