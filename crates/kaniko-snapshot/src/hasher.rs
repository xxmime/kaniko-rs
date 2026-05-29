//! Snapshot hashing strategies.
//!
//! Analogous to Go: `pkg/util/util.go` — `Hasher()` and `getHasher()`.
//!
//! Supports multiple snapshot modes:
//! - "full": Hash file contents (SHA-256). Detects all changes.
//! - "redo": No hashing — always treat files as changed.
//! - "time": Hash only mtime + mode + uid/gid. Fast but may miss content-only changes.

use sha2::{Digest, Sha256};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use thiserror::Error;

/// Hashing mode for snapshot operations.
/// Analogous to Go: `opts.SnapshotMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotMode {
    /// Hash full file contents (SHA-256). Default mode.
    #[default]
    Full,
    /// No hashing — always return a fixed value, forcing re-snapshot.
    Redo,
    /// Hash only metadata (mtime + mode + uid + gid). Fast but may miss content changes.
    Time,
}

impl SnapshotMode {
    /// Parse from string, matching Go's `getHasher(opts.SnapshotMode)`.
    pub fn from_str(mode: &str) -> Self {
        match mode.to_lowercase().as_str() {
            "redo" => SnapshotMode::Redo,
            "time" => SnapshotMode::Time,
            _ => SnapshotMode::Full,
        }
    }

    /// Get the hasher function for this mode.
    pub fn hasher(&self) -> Box<dyn Fn(&Path) -> Result<String, HasherError> + Send + Sync> {
        match self {
            SnapshotMode::Full => Box::new(hash_full),
            SnapshotMode::Redo => Box::new(hash_redo),
            SnapshotMode::Time => Box::new(hash_time),
        }
    }
}

/// Errors during hash computation.
#[derive(Debug, Error)]
pub enum HasherError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Full content hash — SHA-256 of file contents.
/// Analogous to Go: `util.Hasher()` with highwayhash.
fn hash_full(path: &Path) -> Result<String, HasherError> {
    let metadata = std::fs::symlink_metadata(path)?;

    if metadata.is_dir() {
        // For directories, hash the mode + uid + gid
        let mut hasher = Sha256::new();
        hasher.update(format!("dir:{}:{}:{}", metadata.mode(), metadata.uid(), metadata.gid()).as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        let mut hasher = Sha256::new();
        hasher.update(format!("link:{}", target.to_string_lossy()).as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    // Regular file: hash mode + uid + gid + content
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}:", metadata.mode(), metadata.uid(), metadata.gid()).as_bytes());
    let data = std::fs::read(path)?;
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

/// Redo mode — always returns a constant, forcing re-snapshot.
/// Analogous to Go: snapshotMode == "redo".
fn hash_redo(_path: &Path) -> Result<String, HasherError> {
    Ok("redo".to_string())
}

/// Time mode — hash only mtime + mode + uid + gid (no content hash).
/// Fast but may miss content-only changes if mtime is not updated.
/// Analogous to Go: snapshotMode == "time".
fn hash_time(path: &Path) -> Result<String, HasherError> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut hasher = Sha256::new();

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        hasher.update(format!("link:{}", target.to_string_lossy()).as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    let mtime = metadata.modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    hasher.update(format!("{}:{}:{}:{}", mtime, metadata.mode(), metadata.uid(), metadata.gid()).as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_snapshot_mode_from_str() {
        assert_eq!(SnapshotMode::from_str("full"), SnapshotMode::Full);
        assert_eq!(SnapshotMode::from_str("redo"), SnapshotMode::Redo);
        assert_eq!(SnapshotMode::from_str("time"), SnapshotMode::Time);
        assert_eq!(SnapshotMode::from_str("FULL"), SnapshotMode::Full);
        assert_eq!(SnapshotMode::from_str("unknown"), SnapshotMode::Full);
    }

    #[test]
    fn test_hash_full_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let hash1 = hash_full(&file_path).unwrap();
        let hash2 = hash_full(&file_path).unwrap();
        assert_eq!(hash1, hash2);

        // Modify content → hash changes
        fs::write(&file_path, "hello world!").unwrap();
        let hash3 = hash_full(&file_path).unwrap();
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_hash_time_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let hash1 = hash_time(&file_path).unwrap();
        let hash2 = hash_time(&file_path).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hash_redo() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        let hash = hash_redo(&file_path).unwrap();
        assert_eq!(hash, "redo");
    }

    #[test]
    fn test_hash_full_directory() {
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir).unwrap();

        let hash = hash_full(&subdir).unwrap();
        assert!(!hash.is_empty());
    }

    #[test]
    fn test_hasher_factory() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "test").unwrap();

        let full_hasher = SnapshotMode::Full.hasher();
        let full_result = full_hasher(&file_path).unwrap();
        assert!(!full_result.is_empty());

        let redo_hasher = SnapshotMode::Redo.hasher();
        let redo_result = redo_hasher(&file_path).unwrap();
        assert_eq!(redo_result, "redo");

        let time_hasher = SnapshotMode::Time.hasher();
        let time_result = time_hasher(&file_path).unwrap();
        assert!(!time_result.is_empty());
    }
}