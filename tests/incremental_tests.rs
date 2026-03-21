use dom_render_compiler::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_incremental_cache_basic() {
    let temp_dir = TempDir::new().unwrap();
    let mut compiler = RenderCompiler::with_cache(temp_dir.path().to_path_buf());
    let app_file = temp_dir.path().join("app.tsx");
    let header_file = temp_dir.path().join("header.tsx");
    fs::write(&app_file, "export default function App() { return null; }").unwrap();
    fs::write(
        &header_file,
        "export default function Header() { return null; }",
    )
    .unwrap();

    // Add components
    let mut app = types::Component::new(types::ComponentId::new(1), "App".to_string());
    app.weight = 100.0;
    app.file_path = app_file.to_string_lossy().to_string();

    let mut header = types::Component::new(types::ComponentId::new(2), "Header".to_string());
    header.weight = 50.0;
    header.file_path = header_file.to_string_lossy().to_string();

    let app_id = compiler.add_component(app);
    let header_id = compiler.add_component(header);
    compiler.add_dependency(app_id, header_id).unwrap();

    // First optimization
    let files = vec![app_file.clone(), header_file.clone()];
    let result1 = compiler.optimize_incremental(&files).unwrap();

    assert_eq!(result1.metrics.total_components, 2);

    // Save cache
    compiler.save_cache().unwrap();

    // Create new compiler with same cache
    let mut compiler2 = RenderCompiler::with_cache(temp_dir.path().to_path_buf());

    // Rebuild graph
    let mut app = types::Component::new(types::ComponentId::new(1), "App".to_string());
    app.weight = 100.0;
    app.file_path = app_file.to_string_lossy().to_string();

    let mut header = types::Component::new(types::ComponentId::new(2), "Header".to_string());
    header.weight = 50.0;
    header.file_path = header_file.to_string_lossy().to_string();

    let app_id = compiler2.add_component(app);
    let header_id = compiler2.add_component(header);
    compiler2.add_dependency(app_id, header_id).unwrap();

    // Second optimization should be cached
    let result2 = compiler2.optimize_incremental(&files).unwrap();

    assert_eq!(result2.metrics.total_components, 2);

    // Check cache was used
    if let Some(stats) = compiler2.cache_stats() {
        assert!(stats.cache_hit_rate > 0.0);
    }
}

#[test]
fn test_incremental_invalidation() {
    let temp_dir = TempDir::new().unwrap();

    // Setup cache with dependency chain: A -> B -> C
    let cache = incremental::IncrementalCache::new(temp_dir.path());

    let id_a = types::ComponentId::new(1);
    let id_b = types::ComponentId::new(2);
    let id_c = types::ComponentId::new(3);

    cache
        .dependency_graph
        .insert(id_b, vec![id_a].into_iter().collect());
    cache
        .dependency_graph
        .insert(id_c, vec![id_b].into_iter().collect());

    // Invalidate A
    cache.invalidate_component(id_a, incremental::InvalidationReason::FileChanged);

    // B and C should also be invalidated (cascade)
    assert!(cache.is_invalidated(id_a));
    assert!(cache.is_invalidated(id_b));
    assert!(cache.is_invalidated(id_c));
}

#[test]
fn test_cache_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let cache_path = temp_dir.path().join(".dom-compiler-cache.bin");

    // Create and populate cache
    {
        let cache = incremental::IncrementalCache::new(temp_dir.path());
        let file_path = temp_dir.path().join("test.tsx");
        fs::write(
            &file_path,
            "export default function Test() { return null; }",
        )
        .unwrap();

        let comp = types::Component::new(types::ComponentId::new(1), "Test".to_string());
        let analysis = types::ComponentAnalysis::new(types::ComponentId::new(1));

        cache.cache_analysis(comp, analysis, file_path).unwrap();
        cache.save().unwrap();
    }

    // Verify cache file exists
    assert!(cache_path.exists());

    // Load cache in new instance
    {
        let mut cache = incremental::IncrementalCache::new(temp_dir.path());
        cache.load().unwrap();

        assert!(cache.has_cached_component(types::ComponentId::new(1)));
    }
}

#[test]
fn test_change_detection() {
    use std::fs;

    let temp_dir = TempDir::new().unwrap();
    let cache = incremental::IncrementalCache::new(temp_dir.path());

    // Create test files
    let file1 = temp_dir.path().join("file1.tsx");
    let file2 = temp_dir.path().join("file2.tsx");

    fs::write(&file1, b"content1").unwrap();
    fs::write(&file2, b"content2").unwrap();

    // First scan - all new
    let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
    assert_eq!(changes.new_files.len(), 2);

    // Cache hashes
    cache.prime_file_hash(&file1).unwrap();
    cache.prime_file_hash(&file2).unwrap();

    // No changes
    let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
    assert!(changes.is_empty());

    // Modify file1
    fs::write(&file1, b"modified").unwrap();
    let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
    assert_eq!(changes.changed_files.len(), 1);

    // Delete file2
    let changes = cache.detect_changes(std::slice::from_ref(&file1));
    assert_eq!(changes.deleted_files.len(), 1);
}

#[test]
fn test_cache_stats() {
    let temp_dir = TempDir::new().unwrap();
    let cache = incremental::IncrementalCache::new(temp_dir.path());

    // Add some cached components
    for i in 0..10 {
        let file_path = temp_dir.path().join(format!("comp{}.tsx", i));
        fs::write(
            &file_path,
            format!(
                "export default function Component{}() {{ return null; }}",
                i
            ),
        )
        .unwrap();

        let comp = types::Component::new(types::ComponentId::new(i), format!("Component{}", i));
        let analysis = types::ComponentAnalysis::new(types::ComponentId::new(i));

        cache.cache_analysis(comp, analysis, file_path).unwrap();
    }

    // Invalidate some
    cache.invalidate_component(
        types::ComponentId::new(0),
        incremental::InvalidationReason::FileChanged,
    );
    cache.invalidate_component(
        types::ComponentId::new(1),
        incremental::InvalidationReason::FileChanged,
    );

    let stats = cache.get_stats();
    assert_eq!(stats.total_cached, 8);
    assert_eq!(stats.invalidated, 2);
    assert_eq!(stats.files_tracked, 10);
    assert!(stats.cache_hit_rate > 0.0 && stats.cache_hit_rate < 1.0);
}
