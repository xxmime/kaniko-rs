//! Layered map for tracking file system state across snapshots.
//!
//! Analogous to Go: `pkg/snapshot.LayeredMap`.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
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
#[derive(Debug, Clone)]
pub struct LayeredMap {
    /// Files in the current layer, mapping path -> content hash.
    current: BTreeMap<String, String>,
    /// Files from the previous layer.
    previous: BTreeMap<String, String>,
    /// Whether a snapshot has been taken.
    snapshot_taken: bool,
}

impl LayeredMap {
    /// Create a new empty layered map.
    pub fn new() -> Self {
        Self {
            current: BTreeMap::new(),
            previous: BTreeMap::new(),
            snapshot_taken: false,
        }
    }

    /// Take a snapshot: copy current → previous, clear current.
    pub fn snapshot(&mut self) {
        self.previous = self.current.clone();
        self.current.clear();
        self.snapshot_taken = true;
    }

    /// Add a file path to the current layer.
    pub fn add(&mut self, path: &Path) -> Result<(), LayeredMapError> {
        let key = path.to_string_lossy().to_string();
        let hash = compute_file_hash(path)?;
        self.current.insert(key, hash);
        Ok(())
    }

    /// Add multiple file paths.
    pub fn add_files(&mut self, paths: &[PathBuf]) -> Result<(), LayeredMapError> {
        for path in paths {
            self.add(path)?;
        }
        Ok(())
    }

    /// Get the current file paths.
    pub fn get_current_paths(&self) -> Vec<PathBuf> {
        self.current.keys().map(PathBuf::from).collect()
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
}

impl Default for LayeredMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute a SHA-256 hash of a file's contents.
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