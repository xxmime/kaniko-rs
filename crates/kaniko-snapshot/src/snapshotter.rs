//! File system snapshotter.
//!
//! Takes incremental snapshots of the file system and generates OCI layers.
//! Supports a configurable snapshot timeout via the `SNAPSHOT_TIMEOUT_DURATION`
//! environment variable (default: 90 minutes).
//! Analogous to Go: `pkg/snapshot.Snapshotter`.

use crate::ignore_list::is_in_ignore_list;
use crate::layered_map::LayeredMap;
use crate::volumes;
use crate::walker::{IgnorePattern, walk_for_snapshot, walk_fs, read_dockerignore, is_ignored};
use oci_image::layer::Layer;
use oci_image::whiteout::WhiteoutEntry;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Default snapshot timeout: 90 minutes.
/// Analogous to Go: `fs_util.SNAPSHOT_TIMEOUT_DURATION = "90m"`.
pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(90 * 60);

/// Global snapshot timeout, read from the `SNAPSHOT_TIMEOUT_DURATION` env var.
/// Supported formats: "90m", "1h30m", "5400s", or a plain number of seconds.
static SNAPSHOT_TIMEOUT: LazyLock<Duration> = LazyLock::new(|| {
    match std::env::var("SNAPSHOT_TIMEOUT_DURATION") {
        Ok(val) => parse_snapshot_timeout(&val),
        Err(_) => DEFAULT_SNAPSHOT_TIMEOUT,
    }
});

/// Parse the snapshot timeout string.
/// Supports: "90m", "1h", "5400s", "5400" (bare seconds).
pub fn parse_snapshot_timeout(s: &str) -> Duration {
    let s = s.trim();
    if s.is_empty() {
        return DEFAULT_SNAPSHOT_TIMEOUT;
    }

    // Try to parse as a simple number of seconds
    if let Ok(secs) = s.parse::<u64>() {
        return Duration::from_secs(secs);
    }

    // Parse h/m/s suffixes
    let mut total_secs: u64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        match ch {
            'h' => {
                if let Ok(n) = num_buf.parse::<u64>() {
                    total_secs += n * 3600;
                }
                num_buf.clear();
            }
            'm' => {
                if let Ok(n) = num_buf.parse::<u64>() {
                    total_secs += n * 60;
                }
                num_buf.clear();
            }
            's' => {
                if let Ok(n) = num_buf.parse::<u64>() {
                    total_secs += n;
                }
                num_buf.clear();
            }
            '0'..='9' => {
                num_buf.push(ch);
            }
            _ => {
                // Skip unknown characters
            }
        }
    }
    // Handle trailing number without suffix as seconds
    if let Ok(n) = num_buf.parse::<u64>() {
        total_secs += n;
    }

    if total_secs > 0 {
        Duration::from_secs(total_secs)
    } else {
        DEFAULT_SNAPSHOT_TIMEOUT
    }
}

/// Check if a snapshot operation has exceeded the timeout.
///
/// Returns `Ok(())` if within the timeout, or an error if exceeded.
/// Analogous to Go: `fs_util.CheckSnapshotTimeout()`.
pub fn check_snapshot_timeout(start: Instant) -> std::result::Result<(), SnapshotError> {
    let elapsed = start.elapsed();
    if elapsed > *SNAPSHOT_TIMEOUT {
        Err(SnapshotError::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "Snapshot operation exceeded timeout of {:.0}s (elapsed: {:.0}s)",
                SNAPSHOT_TIMEOUT.as_secs_f64(),
                elapsed.as_secs_f64()
            ),
        )))
    } else {
        Ok(())
    }
}

/// Get the configured snapshot timeout duration.
pub fn snapshot_timeout() -> Duration {
    *SNAPSHOT_TIMEOUT
}

/// Errors for snapshotter operations.
#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("walk error: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("walker error: {0}")]
    Walker(#[from] crate::walker::WalkerError),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
    #[error("layered map error: {0}")]
    LayeredMap(#[from] crate::layered_map::LayeredMapError),
}

/// Result type for snapshotter operations.
pub type Result<T> = std::result::Result<T, SnapshotError>;

/// File system snapshotter.
///
/// Tracks file system changes and generates OCI layers from diffs.
/// Supports .dockerignore patterns for excluding files from snapshots.
/// Analogous to Go: `snapshot.Snapshotter`.
pub struct Snapshotter {
    /// The layered map tracking file states.
    layered_map: LayeredMap,
    /// Root directory to snapshot.
    directory: PathBuf,
    /// .dockerignore patterns loaded from the build context.
    dockerignore_patterns: Vec<IgnorePattern>,
}

impl Snapshotter {
    /// Create a new snapshotter.
    pub fn new(layered_map: LayeredMap, directory: PathBuf) -> Self {
        let dockerignore_patterns = read_dockerignore(&directory);
        Self {
            layered_map,
            directory,
            dockerignore_patterns,
        }
    }

    /// Create a snapshotter with explicit .dockerignore patterns.
    pub fn with_dockerignore(layered_map: LayeredMap, directory: PathBuf, patterns: Vec<IgnorePattern>) -> Self {
        Self {
            layered_map,
            directory,
            dockerignore_patterns: patterns,
        }
    }

    /// Initialize the snapshotter by scanning the full file system.
    pub fn init(&mut self) -> Result<()> {
        let files = self.scan_full_filesystem()?;
        self.layered_map.set_current_paths(files);
        Ok(())
    }

    /// Get a hash key representing the current file system state.
    pub fn key(&self) -> String {
        self.layered_map.key()
    }

    /// Take a snapshot of specific files.
    ///
    /// Returns a Layer containing the changed files and any whiteout entries.
    pub fn take_snapshot(
        &mut self,
        files: &[PathBuf],
        should_check_delete: bool,
        force_build_metadata: bool,
    ) -> Result<Option<Layer>> {
        self.layered_map.snapshot();

        // Append volume paths to the files list.
        // Analogous to Go: `files = append(files, util.Volumes()...)`
        // Volume contents must be included in snapshots even if no changes were detected,
        // because Docker volumes persist across commands.
        let mut all_files: Vec<PathBuf> = files.to_vec();
        for vol in volumes::volumes() {
            let vol_path = PathBuf::from(&vol);
            if vol_path.exists() && !all_files.contains(&vol_path) {
                all_files.push(vol_path);
            }
        }

        if all_files.is_empty() && !force_build_metadata {
            tracing::info!("No files changed in this command, skipping snapshotting.");
            return Ok(None);
        }

        // Resolve paths against .dockerignore patterns and ignore list
        let resolved = self.resolve_paths(&all_files);
        self.layered_map.add_files(&resolved)?;

        // Detect deleted files for whiteout entries
        let whiteouts = if should_check_delete {
            self.detect_deleted_files(&resolved)?
        } else {
            vec![]
        };

        tracing::info!("Taking snapshot of {} files...", resolved.len());

        let layer = Layer::from_files(&resolved, &whiteouts, &self.directory)?;
        Ok(Some(layer))
    }

    /// Take a snapshot of the entire file system.
    ///
    /// Used when we can't determine which files changed (e.g., after RUN).
    /// Includes timeout checking to prevent indefinitely long snapshots.
    pub fn take_snapshot_fs(&mut self) -> Result<Layer> {
        let start = Instant::now();

        let (added, deleted) = self.scan_full_filesystem_with_diff()?;

        // Check timeout after the potentially long filesystem scan
        check_snapshot_timeout(start)?;

        let whiteouts: Vec<WhiteoutEntry> = deleted
            .iter()
            .map(|p| WhiteoutEntry::regular(p))
            .collect();

        let layer = Layer::from_files(&added, &whiteouts, &self.directory)?;
        Ok(layer)
    }

    /// Force sync file system buffers to disk using syncfs syscall.
    ///
    /// This ensures all pending writes are flushed to disk before taking snapshots,
    /// which is critical for consistent file system snapshots.
    pub fn sync_filesystem(&self) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            use std::fs::File;
            use std::os::unix::io::AsRawFd;
            use nix::unistd::syncfs;
            
            // Open the root directory of the file system we want to sync
            let root_file = File::open(&self.directory)?;
            
            // Call syncfs syscall to sync the entire file system
            syncfs(root_file.as_raw_fd())?;
            
            tracing::debug!("Filesystem synced successfully");
        }
        
        #[cfg(not(target_os = "linux"))]
        {
            // On non-Linux systems, use a fallback approach
            tracing::warn!("syncfs not available on this platform, using sync_all as fallback");
            std::process::Command::new("sync")
                .status()
                .map_err(|e| SnapshotError::Io(e))?;
        }
        
        Ok(())
    }

    /// Initialize the snapshotter with forced filesystem sync.
    ///
    /// This version ensures all pending writes are flushed before scanning.
    pub fn init_with_sync(&mut self) -> Result<()> {
        // Sync the filesystem before scanning
        self.sync_filesystem()?;
        
        // Then perform the normal initialization
        let files = self.scan_full_filesystem()?;
        self.layered_map.set_current_paths(files);
        Ok(())
    }

    /// Take a snapshot with forced filesystem sync for consistency.
    ///
    /// This ensures all pending writes are flushed before taking the snapshot.
    pub fn take_snapshot_with_sync(
        &mut self,
        files: &[PathBuf],
        should_check_delete: bool,
        force_build_metadata: bool,
    ) -> Result<Option<Layer>> {
        // Sync the filesystem before taking snapshot
        self.sync_filesystem()?;
        
        // Then perform the normal snapshot operation
        self.take_snapshot(files, should_check_delete, force_build_metadata)
    }

    /// Scan the full file system using the walker with .dockerignore support.
    fn scan_full_filesystem(&self) -> Result<Vec<PathBuf>> {
        if !self.directory.exists() {
            return Ok(vec![]);
        }

        let files = walk_for_snapshot(&self.directory, &self.dockerignore_patterns)?;
        Ok(files)
    }

    /// Scan with diff detection using WalkFS + CheckFileChange.
    ///
    /// This is the Go-compatible scan that:
    /// 1. Walks the directory with WalkFS
    /// 2. Uses `check_file_change` to detect which files actually changed (hash comparison)
    /// 3. Tracks which files were deleted (present in previous but not on disk)
    ///
    /// Analogous to Go: `Snapshotter.scanFullFilesystem()`.
    fn scan_full_filesystem_with_diff(&self) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        if !self.directory.exists() {
            return Ok((vec![], vec![]));
        }

        // Get the existing paths from the layered map for deletion detection
        let existing_paths = self.layered_map.get_current_paths_set();

        // Create a clone of the layered map for the change function
        // Note: We use interior mutability pattern via RefCell in the snapshotter
        // For WalkFS, we need to pass a change_func. Since walk_fs requires a
        // Fn(&Path) -> Result<bool, String>, we compute hashes inline.
        let previous_map = self.layered_map.previous_hashes().clone();

        let result = walk_fs(&self.directory, existing_paths, |path| {
            // Analogous to Go: `s.l.CheckFileChange(path)`
            // Compute hash and compare with previous
            match compute_path_hash(path) {
                Ok(new_hash) => {
                    let key = path.to_string_lossy().to_string();
                    let is_changed = match previous_map.get(&key) {
                        Some(prev_hash) => new_hash != *prev_hash,
                        None => true, // New file → changed
                    };
                    Ok(is_changed)
                }
                Err(e) => Err(e.to_string()),
            }
        })?;

        let added: Vec<PathBuf> = result.files_added;
        let deleted: Vec<PathBuf> = result
            .deleted_paths
            .into_iter()
            .map(PathBuf::from)
            .collect();

        Ok((added, deleted))
    }

    /// Check if a path should be ignored based on .dockerignore patterns and ignore list.
    fn should_ignore(&self, path: &Path) -> bool {
        // Check .dockerignore patterns
        if !self.dockerignore_patterns.is_empty() {
            let is_dir = path.is_dir();
            if is_ignored(path, &self.dockerignore_patterns, is_dir) {
                return true;
            }
        }
        // Check the global ignore list (kaniko internal paths)
        if is_in_ignore_list(path) {
            return true;
        }
        false
    }

    /// Resolve paths against the ignore patterns.
    fn resolve_paths(&self, files: &[PathBuf]) -> Vec<PathBuf> {
        files
            .iter()
            .filter(|p| !self.should_ignore(p))
            .cloned()
            .collect()
    }

    /// Detect deleted files since last snapshot.
    /// Uses remove_obsolete_whiteouts to filter out whiteouts whose parent dir
    /// was also deleted (they would be redundant).
    fn detect_deleted_files(&self, _current: &[PathBuf]) -> Result<Vec<WhiteoutEntry>> {
        let deleted = self.layered_map.get_deleted_files();
        let deleted_set: std::collections::HashSet<PathBuf> = deleted.iter().cloned().collect();
        let filtered = remove_obsolete_whiteouts(&deleted_set);
        Ok(filtered.iter().map(|p| WhiteoutEntry::regular(p)).collect())
    }
}

/// Compute a hash for a file path (used in WalkFS change detection).
/// This is a standalone version of `compute_file_hash` from LayeredMap,
/// needed because WalkFS's change_func closure cannot hold mutable refs.
/// Analogous to Go: `LayeredMap.hasher(s string)`.
fn compute_path_hash(path: &Path) -> std::io::Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() {
        return Ok("dir".to_string());
    }
    if metadata.is_symlink() {
        let target = std::fs::read_link(path)?;
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(target.to_string_lossy().as_bytes());
        return Ok(format!("link:{}", hex::encode(hasher.finalize())));
    }
    let data = std::fs::read(path)?;
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

/// Filter deleted files: only include a whiteout if its parent directory was NOT also deleted.
/// If a parent directory is deleted, the child whiteout is redundant because the parent
/// whiteout already covers it.
/// Analogous to Go: `snapshot.removeObsoleteWhiteouts`.
pub fn remove_obsolete_whiteouts(deleted_files: &std::collections::HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut result = Vec::new();
    for path in deleted_files {
        let parent = path.parent();
        let parent_deleted = parent.map_or(false, |p| deleted_files.contains(p));
        if !parent_deleted {
            tracing::trace!("Adding whiteout for {}", path.display());
            result.push(path.clone());
        }
    }
    result
}

/// Returns true if a parent of the given path has been replaced with anything other than a directory.
/// This is used to skip whiteout entries when a parent directory was replaced by a file or symlink,
/// because the whiteout would be invalid in that case.
/// Analogous to Go: `snapshot.parentPathIncludesNonDirectory`.
pub fn parent_path_includes_non_directory(path: &Path) -> std::io::Result<bool> {
    for parent in parent_directories(path) {
        let metadata = std::fs::symlink_metadata(&parent)?;
        if !metadata.is_dir() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Return all parent directories of a path, from root to the immediate parent.
/// E.g., /some/temp/dir -> [/, /some, /some/temp]
/// Analogous to Go: `util.ParentDirectories`.
pub fn parent_directories(path: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut current = path;

    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() || parent == Path::new("/") {
            break;
        }
        if !paths.contains(&parent.to_path_buf()) {
            paths.insert(0, parent.to_path_buf());
        }
        current = parent;
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layered_map::LayeredMap;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_snapshotter_creation() {
        let layered_map = LayeredMap::new();
        let temp_dir = TempDir::new().unwrap();
        let snapshotter = Snapshotter::new(layered_map, temp_dir.path().to_path_buf());
        
        assert_eq!(snapshotter.directory, temp_dir.path());
    }

    #[test]
    fn test_sync_filesystem() {
        let layered_map = LayeredMap::new();
        let temp_dir = TempDir::new().unwrap();
        let snapshotter = Snapshotter::new(layered_map, temp_dir.path().to_path_buf());
        
        // This should not panic
        let result = snapshotter.sync_filesystem();
        assert!(result.is_ok());
    }

    #[test]
    fn test_init_with_sync() {
        let layered_map = LayeredMap::new();
        let temp_dir = TempDir::new().unwrap();
        let mut snapshotter = Snapshotter::new(layered_map, temp_dir.path().to_path_buf());
        
        // Create some test files
        fs::write(temp_dir.path().join("test.txt"), "test content").unwrap();
        
        let result = snapshotter.init_with_sync();
        assert!(result.is_ok());
    }

    #[test]
    fn test_take_snapshot_with_sync() {
        let layered_map = LayeredMap::new();
        let temp_dir = TempDir::new().unwrap();
        let mut snapshotter = Snapshotter::new(layered_map, temp_dir.path().to_path_buf());
        
        // Create some test files
        fs::write(temp_dir.path().join("test.txt"), "test content").unwrap();
        
        let files = vec![temp_dir.path().join("test.txt")];
        let result = snapshotter.take_snapshot_with_sync(&files, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_remove_obsolete_whiteouts() {
        // If both parent and child are deleted, only parent whiteout is needed
        let mut deleted: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        deleted.insert(PathBuf::from("/usr/local/bin/app"));
        deleted.insert(PathBuf::from("/usr/local/bin"));
        deleted.insert(PathBuf::from("/etc/config"));

        let result = remove_obsolete_whiteouts(&deleted);
        // /usr/local/bin should be included (parent /usr/local not deleted)
        // /usr/local/bin/app should NOT be included (parent /usr/local/bin is deleted)
        // /etc/config should be included (parent /etc not deleted)
        assert!(result.contains(&PathBuf::from("/usr/local/bin")));
        assert!(!result.contains(&PathBuf::from("/usr/local/bin/app")));
        assert!(result.contains(&PathBuf::from("/etc/config")));
    }

    #[test]
    fn test_parent_directories() {
        let path = PathBuf::from("/some/temp/dir");
        let parents = parent_directories(&path);
        assert_eq!(parents, vec![
            PathBuf::from("/some"),
            PathBuf::from("/some/temp"),
        ]);
    }

    #[test]
    fn test_parent_path_includes_non_directory() {
        // Test with a simple path - parent directories are all directories
        // Use a path that doesn't include macOS /tmp symlink issues
        let result = parent_path_includes_non_directory(Path::new("/usr/bin/ls"));
        assert!(result.is_ok());
        // /usr and /usr/bin are directories, so this should be false
        assert!(!result.unwrap());
    }
}