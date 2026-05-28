//! Composite cache key computation.
//!
//! Computes a deterministic hash key for caching build layers.
//! Analogous to Go: `pkg/util/composite_key.CompositeCache`.

use crate::command::BuildArgs;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// Composite cache key — accumulates state for deterministic cache key computation.
///
/// Each command execution updates the composite key with:
/// 1. The command string
/// 2. Files used from context
/// 3. Build arguments (if required by the command)
#[derive(Debug, Clone)]
pub struct CompositeCache {
    /// The current hash state.
    state: String,
}

impl CompositeCache {
    /// Create a new composite cache key with the base image digest.
    pub fn new(base_image_digest: &str) -> Self {
        Self {
            state: base_image_digest.to_string(),
        }
    }

    /// Update the composite key with a command's contribution.
    pub fn update(
        self,
        command_string: &str,
        files: Vec<PathBuf>,
        args: &BuildArgs,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(self.state.as_bytes());
        hasher.update(command_string.as_bytes());

        // Include file paths
        for file in &files {
            hasher.update(file.to_string_lossy().as_bytes());
        }

        // Include build args
        for (key, value) in &args.build_args {
            hasher.update(key.as_bytes());
            hasher.update(value.as_bytes());
        }

        // Include env vars from args
        for (key, value) in &args.env {
            hasher.update(key.as_bytes());
            hasher.update(value.as_bytes());
        }

        let result = hex::encode(hasher.finalize());
        Self { state: result }
    }

    /// Compute the final hash key.
    pub fn hash(&self) -> String {
        // Hash once more to get the final key
        let mut hasher = Sha256::new();
        hasher.update(self.state.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// Get the current state (for debugging).
    pub fn state(&self) -> &str {
        &self.state
    }
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
        assert!(hash1.starts_with("sha256:") || hash1.len() == 64);
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
}