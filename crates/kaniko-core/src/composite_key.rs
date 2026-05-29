//! Composite cache key computation.
//!
//! Computes a deterministic hash key for caching build layers.
//! Analogous to Go: `pkg/executor/composite_cache.go` — `CompositeCache`.

use crate::command::BuildArgs;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Composite cache key — accumulates state for deterministic cache key computation.
///
/// Each command execution updates the composite key with:
/// 1. The command string
/// 2. Files used from context
/// 3. Build arguments (if required by the command)
/// 4. File content hashes (for COPY/ADD context files)
///
/// Analogous to Go: `pkg/executor/composite_cache.go` — `CompositeCache`.
#[derive(Debug, Clone)]
pub struct CompositeCache {
    /// The accumulated key components.
    keys: Vec<String>,
}

impl CompositeCache {
    /// Create a new composite cache key with one or more initial keys.
    /// Analogous to Go: `NewCompositeCache(initial...)`.
    pub fn new(initial: &str) -> Self {
        Self {
            keys: vec![initial.to_string()],
        }
    }

    /// Add raw key strings to the composite key.
    /// Analogous to Go: `CompositeCache.AddKey(k...)`.
    pub fn add_key(&mut self, keys: &[&str]) {
        for k in keys {
            self.keys.push(k.to_string());
        }
    }

    /// Add a file path's content hash to the composite key.
    ///
    /// For directories, recursively hashes all files.
    /// For regular files, hashes the file content.
    /// Skips files that don't exist or can't be read.
    ///
    /// Analogous to Go: `CompositeCache.AddPath(p, context)`.
    pub fn add_path(&mut self, path: &str) -> std::io::Result<()> {
        let p = PathBuf::from(path);
        if !p.exists() {
            return Ok(());
        }

        if p.is_dir() {
            let (_, hash) = hash_dir(path)?;
            self.keys.push(hash);
        } else {
            let hash = hash_file(path)?;
            self.keys.push(hash);
        }

        Ok(())
    }

    /// Get the human-readable composite key as a string.
    /// Analogous to Go: `CompositeCache.Key()`.
    pub fn key(&self) -> String {
        self.keys.join("-")
    }

    /// Compute the final hash key.
    /// Analogous to Go: `CompositeCache.Hash()`.
    pub fn hash(&self) -> String {
        let key_str = self.key();
        let mut hasher = Sha256::new();
        hasher.update(key_str.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Update the composite key with a command's contribution.
    /// This is a convenience method that combines add_key for the command string,
    /// file paths, and build args.
    pub fn update(
        self,
        command_string: &str,
        files: Vec<PathBuf>,
        args: &BuildArgs,
    ) -> Self {
        let mut new_cache = self;
        new_cache.add_key(&[command_string]);

        // Include file paths
        for file in &files {
            new_cache.add_key(&[&file.to_string_lossy()]);
        }

        // Include build args
        for (key, value) in &args.build_args {
            new_cache.add_key(&[key, value]);
        }

        // Include env vars from args
        for (key, value) in &args.env {
            new_cache.add_key(&[key, value]);
        }

        new_cache
    }

    /// Get the current keys (for debugging).
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    /// Get the current state (for debugging).
    pub fn state(&self) -> String {
        self.key()
    }
}

/// Hash a single file's content using SHA-256.
/// Analogous to Go: `util.CacheHasher()(path)`.
fn hash_file(path: &str) -> std::io::Result<String> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Recursively hash a directory's contents.
/// Analogous to Go: `hashDir(p, context)`.
fn hash_dir(path: &str) -> std::io::Result<(bool, String)> {
    let mut hasher = Sha256::new();
    let mut empty = true;

    visit_dir(path, &mut hasher, &mut empty)?;

    Ok((empty, format!("{:x}", hasher.finalize())))
}

/// Recursively visit a directory and hash each file.
fn visit_dir(
    dir: &str,
    hasher: &mut sha2::Sha256,
    empty: &mut bool,
) -> std::io::Result<()> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            visit_dir(&path.to_string_lossy(), hasher, empty)?;
        } else {
            let data = std::fs::read(&path)?;
            hasher.update(&data);
            *empty = false;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_composite_key_hash() {
        let key1 = CompositeCache::new("sha256:abc123");
        let hash1 = key1.hash();
        assert!(!hash1.is_empty());
        assert_eq!(hash1.len(), 64); // SHA-256 hex output
    }

    #[test]
    fn test_different_keys_produce_different_hashes() {
        let key1 = CompositeCache::new("sha256:abc123");
        let key2 = CompositeCache::new("sha256:def456");
        assert_ne!(key1.hash(), key2.hash());
    }

    #[test]
    fn test_update_changes_hash() {
        let key = CompositeCache::new("sha256:abc123");
        let original_hash = key.hash();

        let updated_key = key.update("RUN echo hello", vec![], &BuildArgs::default());
        let updated_hash = updated_key.hash();
        assert_ne!(original_hash, updated_hash);
    }

    #[test]
    fn test_update_with_build_args() {
        let mut build_args = BuildArgs::default();
        build_args.build_args.insert("VERSION".to_string(), "1.0".to_string());

        let key = CompositeCache::new("sha256:abc123");
        let hash_without_args = key.clone().update("ARG VERSION", vec![], &BuildArgs::default()).hash();
        let hash_with_args = key.update("ARG VERSION", vec![], &build_args).hash();
        assert_ne!(hash_without_args, hash_with_args);
    }

    #[test]
    fn test_deterministic() {
        let key1 = CompositeCache::new("sha256:abc123")
            .update("RUN echo hello", vec![], &BuildArgs::default())
            .update("COPY . .", vec![PathBuf::from("/app")], &BuildArgs::default())
            .hash();

        let key2 = CompositeCache::new("sha256:abc123")
            .update("RUN echo hello", vec![], &BuildArgs::default())
            .update("COPY . .", vec![PathBuf::from("/app")], &BuildArgs::default())
            .hash();

        assert_eq!(key1, key2);
    }

    #[test]
    fn test_add_key() {
        let mut key = CompositeCache::new("base");
        key.add_key(&["cmd1", "cmd2"]);
        assert_eq!(key.keys().len(), 3);
        assert_eq!(key.key(), "base-cmd1-cmd2");
    }

    #[test]
    fn test_key_method() {
        let key = CompositeCache::new("abc");
        assert_eq!(key.key(), "abc");
    }

    #[test]
    fn test_add_path_nonexistent() {
        let mut key = CompositeCache::new("base");
        // Should not error on nonexistent path
        assert!(key.add_path("/nonexistent/path/file.txt").is_ok());
        // No new key should be added for nonexistent path
        assert_eq!(key.keys().len(), 1);
    }

    #[test]
    fn test_hash_file() {
        let dir = std::env::temp_dir().join("kaniko_test_composite");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("test_file.txt");
        std::fs::write(&file_path, "hello world").unwrap();
        let result = hash_file(file_path.to_str().unwrap()).unwrap();
        assert_eq!(result.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_hash_dir() {
        let dir = std::env::temp_dir().join("kaniko_test_dir_hash");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file1.txt"), "content1").unwrap();
        std::fs::write(dir.join("file2.txt"), "content2").unwrap();
        let (empty, hash) = hash_dir(dir.to_str().unwrap()).unwrap();
        assert!(!empty);
        assert_eq!(hash.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_hash_dir_empty() {
        let dir = std::env::temp_dir().join("kaniko_test_empty_dir_hash");
        std::fs::create_dir_all(&dir).unwrap();
        let (empty, hash) = hash_dir(dir.to_str().unwrap()).unwrap();
        assert!(empty);
        assert_eq!(hash.len(), 64);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_composite_cache_deterministic_with_paths() {
        let dir = std::env::temp_dir().join("kaniko_test_deterministic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.txt"), "content").unwrap();

        let mut key1 = CompositeCache::new("base");
        key1.add_path(dir.join("test.txt").to_str().unwrap()).unwrap();

        let mut key2 = CompositeCache::new("base");
        key2.add_path(dir.join("test.txt").to_str().unwrap()).unwrap();

        assert_eq!(key1.hash(), key2.hash());
        std::fs::remove_dir_all(&dir).ok();
    }
}