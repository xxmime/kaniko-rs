//! Layered map for tracking file system state across snapshots.
//!
//! Analogous to Go: `pkg/snapshot.LayeredMap`.
//!
//! Key features:
//! - Tracks file paths and content hashes across layers for incremental snapshots
//! - `check_file_change()` uses hash comparison to detect actual content changes
//! - `add_with_hash()` stores pre-computed hashes to avoid recomputation
//! - `get_current_paths_set()` returns paths as a HashSet for WalkFS deletion detection

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors for layered map operations.
#[derive(Debug, Error)]
pub enum LayeredMapError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A map tracking file paths and their content hashes across layers.
///
/// This enables incremental snapshot detection by tracking which files
/// have changed since the last snapshot.
///
/// Analogous to Go: `pkg/snapshot.LayeredMap` which tracks:
/// - `adds[]` map of path→hash for current additions
/// - `layerHashCache` for caching computed hashes
/// - `currentImage` map for the current image state
#[derive(Debug, Clone)]
pub struct LayeredMap {
    /// Files in the current layer, mapping path -> content hash.
    current: BTreeMap<String, String>,
    /// Files from the previous layer (image state before this command).
    previous: BTreeMap<String, String>,
    /// Whether a snapshot has been taken.
    snapshot_taken: bool,
    /// Cache of computed hashes to avoid recomputation during CheckFileChange.
    /// Analogous to Go: `LayeredMap.layerHashCache`.
    hash_cache: BTreeMap<String, String>,
}

impl LayeredMap {
    /// Create a new empty layered map.
    pub fn new() -> Self {
        Self {
            current: BTreeMap::new(),
            previous: BTreeMap::new(),
            snapshot_taken: false,
            hash_cache: BTreeMap::new(),
        }
    }

    /// Take a snapshot: copy current → previous, clear current.
    /// Analogous to Go: `LayeredMap.Snapshot()`.
    pub fn snapshot(&mut self) {
        self.previous = self.current.clone();
        self.current.clear();
        self.snapshot_taken = true;
    }

    /// Add a file path to the current layer with computed hash.
    /// Analogous to Go: `LayeredMap.Add(s string)`.
    pub fn add(&mut self, path: &Path) -> Result<(), LayeredMapError> {
        let key = path.to_string_lossy().to_string();
        let hash = compute_file_hash(path)?;
        self.current.insert(key, hash);
        Ok(())
    }

    /// Add a file path with a pre-computed hash (avoids recomputation).
    /// Analogous to Go: `LayeredMap.Add(s, newV)` where hash was pre-computed
    /// by `CheckFileChange`.
    pub fn add_with_hash(&mut self, path: &str, hash: &str) {
        self.current.insert(path.to_string(), hash.to_string());
    }

    /// Add multiple file paths.
    pub fn add_files(&mut self, paths: &[PathBuf]) -> Result<(), LayeredMapError> {
        for path in paths {
            self.add(path)?;
        }
        Ok(())
    }

    /// Record a file deletion (whiteout entry).
    /// Analogous to Go: `LayeredMap.AddDelete(s string)`.
    pub fn add_delete(&mut self, path: &str) {
        self.current.remove(path);
    }

    /// Get the current file paths.
    pub fn get_current_paths(&self) -> Vec<PathBuf> {
        self.current.keys().map(PathBuf::from).collect()
    }

    /// Get the current file paths as a HashSet for WalkFS deletion detection.
    /// Analogous to Go: `LayeredMap.GetCurrentPaths()`.
    pub fn get_current_paths_set(&self) -> HashSet<String> {
        self.current.keys().cloned().collect()
    }

    /// Set the current paths (used during initialization).
    pub fn set_current_paths(&mut self, paths: Vec<PathBuf>) {
        self.current.clear();
        for path in paths {
            let key = path.to_string_lossy().to_string();
            self.current.insert(key, String::new());
        }
    }

    /// Compute a hash key representing the current file system state.
    pub fn key(&self) -> String {
        let mut hasher = Sha256::new();
        for (path, hash) in &self.current {
            hasher.update(path.as_bytes());
            hasher.update(hash.as_bytes());
        }
        let result = hasher.finalize();
        hex::encode(result)
    }

    /// Get files that are new or modified compared to the previous snapshot.
    pub fn get_added_files(&self) -> Vec<PathBuf> {
        self.current
            .iter()
            .filter(|(path, hash)| {
                self.previous.get(*path).map_or(true, |prev_hash| prev_hash != *hash)
            })
            .map(|(path, _)| PathBuf::from(path))
            .collect()
    }

    /// Get files that were deleted compared to the previous snapshot.
    pub fn get_deleted_files(&self) -> Vec<PathBuf> {
        self.previous
            .keys()
            .filter(|path| !self.current.contains_key(*path))
            .map(|path| PathBuf::from(path))
            .collect()
    }

    /// Check whether a given file (must exist) changed from the current layered map.
    ///
    /// Computes a hash of the file, caches it, and compares with the previous
    /// layer's hash. Returns `true` if the file is new or its content changed.
    ///
    /// Analogous to Go: `LayeredMap.CheckFileChange(s string) (bool, error)`.
    /// Returns `(is_changed, hash)` where `hash` is the cached value.
    pub fn check_file_change(&mut self, path: &Path) -> Result<(bool, String), LayeredMapError> {
        let key = path.to_string_lossy().to_string();
        let new_hash = compute_file_hash(path)?;

        // Cache the hash to avoid recomputation when adding the file later.
        self.hash_cache.insert(key.clone(), new_hash.clone());

        // Compare with previous layer's hash.
        let is_changed = match self.previous.get(&key) {
            Some(prev_hash) => new_hash != *prev_hash,
            None => true, // File does not exist in previous → changed (new file)
        };

        Ok((is_changed, new_hash))
    }

    /// Get a cached hash for a path (set by `check_file_change`).
    /// Used to avoid recomputing hashes when `add()` is called later.
    pub fn get_cached_hash(&self, path: &str) -> Option<String> {
        self.hash_cache.get(path).cloned()
    }

    /// Clear the hash cache (typically after all files have been added).
    pub fn clear_hash_cache(&mut self) {
        self.hash_cache.clear();
    }

    /// Get the previous layer's path→hash map for WalkFS change detection.
    /// This is used by `walk_fs` to compare current file hashes against
    /// the previous snapshot without requiring mutable access to the LayeredMap.
    /// Analogous to Go: `LayeredMap.currentImage` (read-only access).
    pub fn previous_hashes(&self) -> &BTreeMap<String, String> {
        &self.previous
    }
}

impl Default for LayeredMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute a SHA-256 hash of a file's contents.
/// Analogous to Go: `util.Hasher()` → SHA256 hashing.
fn compute_file_hash(path: &Path) -> Result<String, LayeredMapError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        return Ok("dir".to_string());
    }
    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let mut hasher = Sha256::new();
        hasher.update(target.to_string_lossy().as_bytes());
        return Ok(format!("link:{}", hex::encode(hasher.finalize())));
    }
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_layered_map_new() {
        let map = LayeredMap::new();
        assert!(map.get_current_paths().is_empty());
        assert!(map.get_deleted_files().is_empty());
    }

    #[test]
    fn test_layered_map_add_and_snapshot() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let mut map = LayeredMap::new();
        map.add(&file_path).unwrap();
        assert_eq!(map.get_current_paths().len(), 1);

        map.snapshot();
        assert!(map.get_current_paths().is_empty());
        // After snapshot, previous has the file but current is empty,
        // so the file shows as "deleted" relative to current.
        // This is correct Go behavior: deleted = previous - current.
        assert_eq!(map.get_deleted_files().len(), 1);
    }

    #[test]
    fn test_check_file_change_new_file() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("new.txt");
        std::fs::write(&file_path, "content").unwrap();

        let mut map = LayeredMap::new();
        let (changed, hash) = map.check_file_change(&file_path).unwrap();
        assert!(changed);
        assert!(!hash.is_empty());
    }

    #[test]
    fn test_check_file_change_unchanged() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("same.txt");
        std::fs::write(&file_path, "unchanged").unwrap();

        let mut map = LayeredMap::new();
        map.add(&file_path).unwrap();
        map.snapshot();

        let (changed, _) = map.check_file_change(&file_path).unwrap();
        assert!(!changed);
    }

    #[test]
    fn test_check_file_change_modified() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("modify.txt");
        std::fs::write(&file_path, "original").unwrap();

        let mut map = LayeredMap::new();
        map.add(&file_path).unwrap();
        map.snapshot();

        std::fs::write(&file_path, "modified").unwrap();
        let (changed, _) = map.check_file_change(&file_path).unwrap();
        assert!(changed);
    }

    #[test]
    fn test_add_with_hash_and_delete() {
        let mut map = LayeredMap::new();
        map.add_with_hash("/some/path", "abc123");
        assert_eq!(map.get_current_paths().len(), 1);

        map.snapshot();
        map.add_delete("/some/path");
        assert!(map.get_current_paths().is_empty());
    }

    #[test]
    fn test_get_current_paths_set() {
        let mut map = LayeredMap::new();
        map.add_with_hash("/a", "h1");
        map.add_with_hash("/b", "h2");
        let set = map.get_current_paths_set();
        assert_eq!(set.len(), 2);
        assert!(set.contains("/a"));
        assert!(set.contains("/b"));
    }

    #[test]
    fn test_hash_cache() {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("cached.txt");
        std::fs::write(&file_path, "cache me").unwrap();

        let mut map = LayeredMap::new();
        let (_, hash) = map.check_file_change(&file_path).unwrap();
        let key = file_path.to_string_lossy().to_string();
        assert_eq!(map.get_cached_hash(&key), Some(hash));

        map.clear_hash_cache();
        assert!(map.get_cached_hash(&key).is_none());
    }
}