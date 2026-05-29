//! File system snapshotter.
//!
//! Takes incremental snapshots of the file system and generates OCI layers.
//! Supports a configurable snapshot timeout via the `SNAPSHOT_TIMEOUT_DURATION`
//! environment variable (default: 90 minutes).
//! Analogous to Go: `pkg/snapshot.Snapshotter`.

use crate::ignore_list::is_in_ignore_list;
use crate::layered_map::LayeredMap;
use crate::volumes;
use crate::walker::{IgnorePattern, walk_for_snapshot, read_dockerignore, is_ignored};
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

    /// Scan with diff detection (added + deleted files).
    fn scan_full_filesystem_with_diff(&self) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let current_files = self.scan_full_filesystem()?;
        let current_set: std::collections::HashSet<_> = current_files.iter().cloned().collect();
        let previous_set: std::collections::HashSet<_> = self
            .layered_map
            .get_current_paths()
            .into_iter()
            .collect();

        let added: Vec<PathBuf> = current_set.difference(&previous_set).cloned().collect();
        let deleted: Vec<PathBuf> = previous_set.difference(&current_set).cloned().collect();

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
    fn detect_deleted_files(&self, _current: &[PathBuf]) -> Result<Vec<WhiteoutEntry>> {
        let deleted = self.layered_map.get_deleted_files();
        Ok(deleted.iter().map(|p| WhiteoutEntry::regular(p)).collect())
    }
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
}