//! Volume path tracking for snapshot operations.
//!
//! Tracks VOLUME paths declared in Dockerfiles. These paths must be included
//! in subsequent snapshots even if no files were changed by the current command,
//! because volume contents persist across commands.
//!
//! Analogous to Go: `pkg/util/fs_util.go` — `var volumes` + `Volumes()` + `AddVolumePathToIgnoreList()`.

use once_cell::sync::Lazy;
use std::path::PathBuf;
use std::sync::Mutex;

/// Global list of volume paths.
/// Analogous to Go: `var volumes = []string{}`.
static VOLUME_PATHS: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Add a volume path to the global tracking list.
///
/// Called when a VOLUME instruction is encountered in a Dockerfile.
/// Volume paths are included in subsequent snapshots to ensure
/// volume contents are properly captured.
///
/// Analogous to Go: `volumes = append(volumes, path)`.
pub fn add_volume(path: &str) {
    let normalized = normalize_path(path);
    let mut volumes = VOLUME_PATHS.lock().unwrap();
    if !volumes.contains(&normalized) {
        tracing::debug!("Adding volume path: {}", normalized);
        volumes.push(normalized);
    }
}

/// Add multiple volume paths at once.
pub fn add_volumes(paths: &[String]) {
    for path in paths {
        add_volume(path);
    }
}

/// Get a copy of all tracked volume paths.
///
/// Volume paths are appended to the files list during snapshotting
/// to ensure volume contents are included in the layer.
///
/// Analogous to Go: `util.Volumes()`.
pub fn volumes() -> Vec<String> {
    VOLUME_PATHS.lock().unwrap().clone()
}

/// Check if a path is a tracked volume.
pub fn is_volume(path: &str) -> bool {
    let normalized = normalize_path(path);
    VOLUME_PATHS.lock().unwrap().contains(&normalized)
}

/// Clear all tracked volumes (used in tests).
pub fn clear_volumes() {
    VOLUME_PATHS.lock().unwrap().clear();
}

/// Add volume paths to the ignore list.
///
/// Volume directories that exist on the host filesystem should be
/// excluded from snapshots, because they typically contain mounted
/// data from the host that shouldn't be baked into the image.
///
/// Analogous to Go: `util.AddVolumePathToIgnoreList(path)`.
pub fn add_volume_to_ignore_list(path: &str) {
    crate::ignore_list::add_to_ignore_list(
        crate::ignore_list::IgnoreListEntry::new(path, false),
    );
}

/// Normalize a path: ensure it's absolute and doesn't have trailing slashes.
fn normalize_path(path: &str) -> String {
    let p = PathBuf::from(path);
    // Ensure absolute path
    let normalized = if p.is_relative() {
        PathBuf::from("/").join(&p)
    } else {
        p
    };
    // Remove trailing slash
    let s = normalized.to_string_lossy().to_string();
    s.trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_volume() {
        clear_volumes();
        add_volume("/var/lib/data");
        let vols = volumes();
        assert!(vols.contains(&"/var/lib/data".to_string()));
        clear_volumes();
    }

    #[test]
    fn test_add_volumes_batch() {
        clear_volumes();
        add_volumes(&[
            "/var/lib/data".to_string(),
            "/app/uploads".to_string(),
        ]);
        let vols = volumes();
        assert_eq!(vols.len(), 2);
        assert!(vols.contains(&"/var/lib/data".to_string()));
        assert!(vols.contains(&"/app/uploads".to_string()));
        clear_volumes();
    }

    #[test]
    fn test_no_duplicate_volumes() {
        clear_volumes();
        add_volume("/data");
        add_volume("/data");
        let vols = volumes();
        assert_eq!(vols.len(), 1);
        clear_volumes();
    }

    #[test]
    fn test_is_volume() {
        clear_volumes();
        add_volume("/var/lib/data");
        assert!(is_volume("/var/lib/data"));
        assert!(!is_volume("/var/lib/other"));
        clear_volumes();
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("/var/lib/data"), "/var/lib/data");
        assert_eq!(normalize_path("/var/lib/data/"), "/var/lib/data");
        assert_eq!(normalize_path("relative"), "/relative");
    }

    #[test]
    fn test_clear_volumes() {
        clear_volumes();
        add_volume("/test");
        assert!(!volumes().is_empty());
        clear_volumes();
        assert!(volumes().is_empty());
    }
}