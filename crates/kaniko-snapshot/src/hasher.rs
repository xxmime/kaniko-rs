//! Snapshot hashing strategies.
//!
//! Analogous to Go: `pkg/util/util.go` — `Hasher()` and `getHasher()`.
//!
//! Supports multiple snapshot modes:
//! - "full": Hash file contents (SHA-256). Detects all changes.
//! - "redo": No hashing — always treat files as changed.
//! - "time": Hash only mtime + mode + uid/gid. Fast but may miss content-only changes.

use sha2::{Digest, Sha256};
use std::io::Read;
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

/// Cache hasher — hash mode + uid + gid + content (no mtime).
///
/// Used for cache key computation where mtime should not affect
/// the hash. Analogous to Go: `util.CacheHasher()`.
pub fn cache_hasher(path: &Path) -> Result<String, HasherError> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut hasher = Sha256::new();

    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        hasher.update(format!("link:{}", target.to_string_lossy()).as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    if metadata.is_dir() {
        hasher.update(format!("dir:{}:{}:{}", metadata.mode(), metadata.uid(), metadata.gid()).as_bytes());
        return Ok(hex::encode(hasher.finalize()));
    }

    // Regular file: mode + uid + gid + content (no mtime)
    hasher.update(format!("{}:{}:{}:", metadata.mode(), metadata.uid(), metadata.gid()).as_bytes());
    let data = std::fs::read(path)?;
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

/// Mtime hasher — hash only mtime.
///
/// Very fast but may miss content changes if mtime is not updated.
/// Analogous to Go: `util.MtimeHasher()`.
pub fn mtime_hasher(path: &Path) -> Result<String, HasherError> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut hasher = Sha256::new();

    let mtime = metadata.modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    hasher.update(format!("{}", mtime).as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// Redo hasher — hash mtime + size + mode + uid + gid.
///
/// More thorough than mtime-only but still avoids reading file content.
/// Analogous to Go: `util.RedoHasher()`.
pub fn redo_hasher(path: &Path) -> Result<String, HasherError> {
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

    tracing::debug!(
        "Hash components for file: {:?}, mode: {}, mtime: {}, size: {}, uid: {}, gid: {}",
        path, metadata.mode(), mtime, metadata.size(), metadata.uid(), metadata.gid()
    );

    hasher.update(format!(
        "{}:{}:{}:{}:{}",
        metadata.mode(), mtime, metadata.size(), metadata.uid(), metadata.gid()
    ).as_bytes());
    Ok(hex::encode(hasher.finalize()))
}

/// Read extended attribute from a file (Linux only).
///
/// Returns None if the attribute doesn't exist or cannot be read.
/// Analogous to Go: `util.Lgetxattr()`.
#[cfg(target_os = "linux")]
pub fn lgetxattr(path: &Path, attr: &str) -> Option<Vec<u8>> {
    use std::ffi::CString;
    let c_path = CString::new(path.to_string_lossy().into_owned()).ok()?;
    let c_attr = CString::new(attr).ok()?;

    // First call with empty buffer to get size
    let size = unsafe { libc::lgetxattr(c_path.as_ptr(), c_attr.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return None;
    }
    if size == 0 {
        return Some(Vec::new());
    }

    // Second call with proper buffer
    let mut buf = vec![0u8; size as usize];
    let result = unsafe { libc::lgetxattr(c_path.as_ptr(), c_attr.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, size as usize) };
    if result < 0 {
        return None;
    }
    buf.truncate(result as usize);
    Some(buf)
}

/// Non-Linux fallback for lgetxattr — always returns None.
#[cfg(not(target_os = "linux"))]
pub fn lgetxattr(_path: &Path, _attr: &str) -> Option<Vec<u8>> {
    None
}

/// Compute SHA-256 of a reader's contents.
/// Analogous to Go: `util.SHA256()`.
pub fn sha256_reader<R: std::io::Read>(mut reader: R) -> Result<String, HasherError> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
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

    #[test]
    fn test_cache_hasher() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let hash1 = cache_hasher(&file_path).unwrap();
        let hash2 = cache_hasher(&file_path).unwrap();
        assert_eq!(hash1, hash2);

        // Modify content → hash changes
        fs::write(&file_path, "hello world!").unwrap();
        let hash3 = cache_hasher(&file_path).unwrap();
        assert_ne!(hash1, hash3);
    }

    #[test]
    fn test_mtime_hasher() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let hash1 = mtime_hasher(&file_path).unwrap();
        assert!(!hash1.is_empty());
    }

    #[test]
    fn test_redo_hasher() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello world").unwrap();

        let hash1 = redo_hasher(&file_path).unwrap();
        let hash2 = redo_hasher(&file_path).unwrap();
        assert_eq!(hash1, hash2);
        assert!(!hash1.is_empty());
    }

    #[test]
    fn test_sha256_reader() {
        use std::io::Cursor;
        let data = b"hello world";
        let hash = sha256_reader(Cursor::new(data)).unwrap();
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // SHA-256 hex is 64 chars
    }

    #[test]
    fn test_lgetxattr_nonexistent() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "hello").unwrap();

        // security.capability shouldn't exist on a test file
        let result = lgetxattr(&file_path, "security.capability");
        assert!(result.is_none());
    }
}