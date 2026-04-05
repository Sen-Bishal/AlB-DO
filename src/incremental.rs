use crate::types::*;
use dashmap::DashMap;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

type FileHash = u64;

#[allow(dead_code)]
struct LazyCacheData {
    file_hashes: HashMap<PathBuf, FileHash>,
    component_cache: HashMap<ComponentId, CachedAnalysis>,
    file_to_components: HashMap<PathBuf, HashSet<ComponentId>>,
    component_to_file: HashMap<ComponentId, PathBuf>,
    dependency_graph: HashMap<ComponentId, HashSet<ComponentId>>,
}

pub struct IncrementalCache {
    file_hashes: DashMap<PathBuf, FileHash>,
    component_cache: DashMap<ComponentId, CachedAnalysis>,
    file_to_components: DashMap<PathBuf, HashSet<ComponentId>>,
    component_to_file: DashMap<ComponentId, PathBuf>,
    pub dependency_graph: DashMap<ComponentId, HashSet<ComponentId>>,
    cache_file_path: PathBuf,
    invalidated: DashMap<ComponentId, InvalidationReason>,
    lazy_data: Lazy<RwLock<Option<LazyCacheData>>>,
    loaded: RwLock<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InvalidationReason {
    FileChanged,
    DependencyChanged,
    NewComponent,
    Deleted,
}
// This is another test for my lua and nvim setup
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedAnalysis {
    pub component: Component,
    pub analysis: ComponentAnalysis,
    pub cached_at: SystemTime,
    pub file_hash: FileHash,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistentCache {
    version: String,
    file_hashes: HashMap<PathBuf, FileHash>,
    component_cache: HashMap<ComponentId, CachedAnalysis>,
    file_to_components: HashMap<PathBuf, HashSet<ComponentId>>,
    component_to_file: HashMap<ComponentId, PathBuf>,
    dependency_graph: HashMap<ComponentId, HashSet<ComponentId>>,
}

impl IncrementalCache {
    pub fn new(cache_dir: impl AsRef<Path>) -> Self {
        let cache_file_path = cache_dir.as_ref().join(".dom-compiler-cache.bin");

        Self {
            file_hashes: DashMap::new(),
            component_cache: DashMap::new(),
            file_to_components: DashMap::new(),
            component_to_file: DashMap::new(),
            dependency_graph: DashMap::new(),
            cache_file_path,
            invalidated: DashMap::new(),
            lazy_data: Lazy::new(|| RwLock::new(None)),
            loaded: RwLock::new(false),
        }
    }

    pub fn load(&self) -> io::Result<()> {
        if !self.cache_file_path.exists() {
            return Ok(());
        }

        let mut file = fs::File::open(&self.cache_file_path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        match bincode::deserialize::<PersistentCache>(&buffer) {
            Ok(cache) => {
                let lazy_data = LazyCacheData {
                    file_hashes: cache.file_hashes,
                    component_cache: cache.component_cache,
                    file_to_components: cache.file_to_components,
                    component_to_file: cache.component_to_file,
                    dependency_graph: cache.dependency_graph,
                };
                *self.lazy_data.write().unwrap() = Some(lazy_data);
                *self.loaded.write().unwrap() = true;
                Ok(())
            }
            Err(e) => {
                eprintln!(
                    "Warning: Failed to deserialize cache, starting fresh: {}",
                    e
                );
                *self.loaded.write().unwrap() = true;
                Ok(())
            }
        }
    }

    fn ensure_loaded(&self) {
        if !*self.loaded.read().unwrap() {
            let _ = self.load();
        }
    }

    fn get_or_load_component(&self, id: ComponentId) -> Option<CachedAnalysis> {
        self.ensure_loaded();

        if let Some(ref data) = *self.lazy_data.read().unwrap() {
            if let Some(cached) = data.component_cache.get(&id) {
                return Some(cached.clone());
            }
        }

        if let Some(entry) = self.component_cache.get(&id) {
            return Some(entry.clone());
        }

        None
    }

    fn get_or_load_file_hash(&self, path: &PathBuf) -> Option<FileHash> {
        self.ensure_loaded();

        if let Some(ref data) = *self.lazy_data.read().unwrap() {
            if let Some(&hash) = data.file_hashes.get(path) {
                return Some(hash);
            }
        }

        if let Some(entry) = self.file_hashes.get(path) {
            return Some(*entry.value());
        }

        None
    }

    fn get_or_load_file_components(&self, path: &PathBuf) -> Option<HashSet<ComponentId>> {
        self.ensure_loaded();

        if let Some(ref data) = *self.lazy_data.read().unwrap() {
            if let Some(ids) = data.file_to_components.get(path) {
                return Some(ids.clone());
            }
        }

        if let Some(entry) = self.file_to_components.get(path) {
            return Some(entry.clone());
        }

        None
    }

    pub fn save(&self) -> io::Result<()> {
        let persistent = PersistentCache {
            version: "1.0".to_string(),
            file_hashes: self
                .file_hashes
                .iter()
                .map(|entry| (entry.key().clone(), *entry.value()))
                .collect(),
            component_cache: self
                .component_cache
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
                .collect(),
            file_to_components: self
                .file_to_components
                .iter()
                .map(|entry| (entry.key().clone(), entry.value().clone()))
                .collect(),
            component_to_file: self
                .component_to_file
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
                .collect(),
            dependency_graph: self
                .dependency_graph
                .iter()
                .map(|entry| (*entry.key(), entry.value().clone()))
                .collect(),
        };

        let encoded = bincode::serialize(&persistent).map_err(io::Error::other)?;
        let temp_path = self.cache_file_path.with_extension("tmp");
        let mut file = fs::File::create(&temp_path)?;
        file.write_all(&encoded)?;
        file.sync_all()?;

        fs::rename(temp_path, &self.cache_file_path)?;

        Ok(())
    }
    fn hash_file(path: impl AsRef<Path>) -> io::Result<FileHash> {
        let content = fs::read(path)?;
        Ok(fnv1a_hash_bytes(&content))
    }

    pub fn compute_file_hash(path: impl AsRef<Path>) -> io::Result<u64> {
        Self::hash_file(path)
    }

    pub fn prime_file_hash(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref().to_path_buf();
        let hash = Self::hash_file(&path)?;
        self.file_hashes.insert(path, hash);
        Ok(())
    }

    pub fn detect_changes(&self, current_files: &[PathBuf]) -> ChangeSet {
        self.ensure_loaded();

        let mut changed_files = Vec::new();
        let mut new_files = Vec::new();
        let mut deleted_files = Vec::new();
        for path in current_files {
            match Self::hash_file(path) {
                Ok(current_hash) => {
                    if let Some(cached_hash) = self.get_or_load_file_hash(path) {
                        if cached_hash != current_hash {
                            changed_files.push(path.clone());
                        }
                    } else {
                        new_files.push(path.clone());
                    }
                }
                Err(e) => {
                    eprintln!("Warning: Failed to hash file {:?}: {}", path, e);
                }
            }
        }
        let current_set: HashSet<_> = current_files.iter().collect();

        let tracked_files: Vec<PathBuf> = {
            let mut files = Vec::new();
            for entry in self.file_hashes.iter() {
                files.push(entry.key().clone());
            }
            if let Some(ref data) = *self.lazy_data.read().unwrap() {
                for path in data.file_hashes.keys() {
                    if !self.file_hashes.contains_key(path) {
                        files.push(path.clone());
                    }
                }
            }
            files
        };

        for path in tracked_files {
            if !current_set.contains(&path) {
                deleted_files.push(path);
            }
        }

        ChangeSet {
            changed_files,
            new_files,
            deleted_files,
        }
    }
    pub fn invalidate_component(&self, id: ComponentId, reason: InvalidationReason) {
        self.invalidated.insert(id, reason.clone());
        self.component_cache.remove(&id);
        if let Some(dependents) = self.get_dependents(id) {
            for dependent_id in dependents.iter() {
                if !self.invalidated.contains_key(dependent_id) {
                    self.invalidate_component(*dependent_id, InvalidationReason::DependencyChanged);
                }
            }
        }
    }
    fn get_dependents(&self, id: ComponentId) -> Option<HashSet<ComponentId>> {
        let mut dependents = HashSet::new();

        for entry in self.dependency_graph.iter() {
            if entry.value().contains(&id) {
                dependents.insert(*entry.key());
            }
        }

        if dependents.is_empty() {
            None
        } else {
            Some(dependents)
        }
    }

    pub fn invalidate_changed_files(&self, changes: &ChangeSet) {
        self.ensure_loaded();

        for path in &changes.changed_files {
            if let Some(component_ids) = self.get_or_load_file_components(path) {
                for id in component_ids.iter() {
                    self.invalidate_component(*id, InvalidationReason::FileChanged);
                }
            }
        }

        for path in &changes.deleted_files {
            if let Some(component_ids) = self.get_or_load_file_components(path) {
                for id in component_ids.iter() {
                    self.invalidate_component(*id, InvalidationReason::Deleted);
                }
            }
            self.file_hashes.remove(path);
            self.file_to_components.remove(path);
        }
    }

    pub fn is_cached(&self, id: ComponentId) -> bool {
        !self.invalidated.contains_key(&id) && self.get_or_load_component(id).is_some()
    }

    pub fn get_cached_analysis(&self, id: ComponentId) -> Option<CachedAnalysis> {
        if self.invalidated.contains_key(&id) {
            return None;
        }
        self.get_or_load_component(id)
    }

    pub fn cache_analysis(
        &self,
        component: Component,
        analysis: ComponentAnalysis,
        file_path: PathBuf,
    ) -> io::Result<()> {
        let file_hash = Self::hash_file(&file_path)?;

        let cached = CachedAnalysis {
            component: component.clone(),
            analysis,
            cached_at: SystemTime::now(),
            file_hash,
        };

        self.component_cache.insert(component.id, cached);
        self.component_to_file
            .insert(component.id, file_path.clone());
        self.file_to_components
            .entry(file_path.clone())
            .or_default()
            .insert(component.id);
        self.file_hashes.insert(file_path, file_hash);

        Ok(())
    }

    pub fn update_dependencies(&self, id: ComponentId, dependencies: HashSet<ComponentId>) {
        self.dependency_graph.insert(id, dependencies);
    }
    pub fn get_stats(&self) -> CacheStats {
        CacheStats {
            total_cached: self.component_cache.len(),
            invalidated: self.invalidated.len(),
            files_tracked: self.file_hashes.len(),
            cache_hit_rate: self.calculate_hit_rate(),
        }
    }

    fn calculate_hit_rate(&self) -> f64 {
        let total = self.component_cache.len() + self.invalidated.len();
        if total == 0 {
            return 0.0;
        }

        self.component_cache.len() as f64 / total as f64
    }

    pub fn clear_invalidated(&self) {
        self.invalidated.clear();
    }

    pub fn get_invalidated_components(&self) -> Vec<ComponentId> {
        self.invalidated.iter().map(|entry| *entry.key()).collect()
    }

    pub fn is_invalidated(&self, id: ComponentId) -> bool {
        self.invalidated.contains_key(&id)
    }

    pub fn has_cached_component(&self, id: ComponentId) -> bool {
        self.component_cache.contains_key(&id)
    }
}

#[derive(Debug, Default)]
pub struct ChangeSet {
    pub changed_files: Vec<PathBuf>,
    pub new_files: Vec<PathBuf>,
    pub deleted_files: Vec<PathBuf>,
}

impl ChangeSet {
    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty() && self.new_files.is_empty() && self.deleted_files.is_empty()
    }

    pub fn total_changes(&self) -> usize {
        self.changed_files.len() + self.new_files.len() + self.deleted_files.len()
    }
}

#[derive(Debug)]
pub struct CacheStats {
    pub total_cached: usize,
    pub invalidated: usize,
    pub files_tracked: usize,
    pub cache_hit_rate: f64,
}

/// FNV-1a 64-bit hash — stable across Rust versions, process runs, and platforms.
/// Unlike `DefaultHasher`, this is deterministic: same bytes always → same hash.
fn fnv1a_hash_bytes(data: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_hash_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        fs::write(&file_path, b"hello world").unwrap();
        let hash1 = IncrementalCache::hash_file(&file_path).unwrap();

        fs::write(&file_path, b"hello world").unwrap();
        let hash2 = IncrementalCache::hash_file(&file_path).unwrap();

        assert_eq!(hash1, hash2);

        fs::write(&file_path, b"hello rust").unwrap();
        let hash3 = IncrementalCache::hash_file(&file_path).unwrap();

        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_detect_changes() {
        let temp_dir = TempDir::new().unwrap();
        let cache = IncrementalCache::new(temp_dir.path());

        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");

        fs::write(&file1, b"content1").unwrap();
        fs::write(&file2, b"content2").unwrap();
        let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
        assert_eq!(changes.new_files.len(), 2);
        cache
            .file_hashes
            .insert(file1.clone(), IncrementalCache::hash_file(&file1).unwrap());
        cache
            .file_hashes
            .insert(file2.clone(), IncrementalCache::hash_file(&file2).unwrap());
        let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
        assert!(changes.is_empty());
        fs::write(&file1, b"modified content").unwrap();
        let changes = cache.detect_changes(&[file1.clone(), file2.clone()]);
        assert_eq!(changes.changed_files.len(), 1);
        assert_eq!(changes.changed_files[0], file1);
    }

    #[test]
    fn test_invalidation_cascade() {
        let temp_dir = TempDir::new().unwrap();
        let cache = IncrementalCache::new(temp_dir.path());
        let id_a = ComponentId::new(1);
        let id_b = ComponentId::new(2);
        let id_c = ComponentId::new(3);

        cache
            .dependency_graph
            .insert(id_b, vec![id_a].into_iter().collect());
        cache
            .dependency_graph
            .insert(id_c, vec![id_b].into_iter().collect());
        cache.invalidate_component(id_a, InvalidationReason::FileChanged);
        assert!(cache.invalidated.contains_key(&id_a));
        assert!(cache.invalidated.contains_key(&id_b));
        assert!(cache.invalidated.contains_key(&id_c));
    }
}
