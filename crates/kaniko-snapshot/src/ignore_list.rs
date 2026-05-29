//! Default ignore list for snapshot operations.
//!
//! Analogous to Go: `pkg/util/fs_util.go` — `defaultIgnoreList` + `InitIgnoreList()`.
//!
//! Maintains a list of paths that should be excluded from filesystem snapshots,
//! such as the kaniko working directory, mounted paths, and temporary files.

use std::path::PathBuf;
use std::sync::Mutex;
use once_cell::sync::Lazy;

/// Default kaniko working directory.
pub const KANIKO_DIR: &str = "/kaniko";

/// An entry in the ignore list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoreListEntry {
    /// The path to ignore.
    pub path: PathBuf,
    /// If true, only match path prefix (not exact match).
    pub prefix_match_only: bool,
}

impl IgnoreListEntry {
    /// Create a new ignore list entry.
    pub fn new(path: &str, prefix_match_only: bool) -> Self {
        Self {
            path: PathBuf::from(path),
            prefix_match_only,
        }
    }

    /// Check if a given path matches this ignore entry.
    /// For prefix_match_only entries, uses string-based prefix matching
    /// (e.g. "/tmp/apt-key-gpghome" matches "/tmp/apt-key-gpghome.RANDOM").
    /// For exact entries, matches the exact path or any subdirectory under it.
    pub fn matches(&self, candidate: &std::path::Path) -> bool {
        if self.prefix_match_only {
            // String prefix match — handles cases like "/tmp/apt-key-gpghome" matching
            // "/tmp/apt-key-gpghome.RANDOM" where the suffix is not a path separator.
            candidate.as_os_str().to_string_lossy().starts_with(self.path.as_os_str().to_string_lossy().as_ref())
        } else {
            // Exact path match or subdirectory match (path-component aware).
            candidate == self.path || candidate.starts_with(&self.path)
        }
    }
}

/// Global default ignore list.
/// Analogous to Go: `var defaultIgnoreList = []IgnoreListEntry{...}`.
static DEFAULT_IGNORE_LIST: Lazy<Mutex<Vec<IgnoreListEntry>>> = Lazy::new(|| {
    let entries = vec![
        // Kaniko working directory
        IgnoreListEntry::new(KANIKO_DIR, false),
        // /etc/mtab is typically a symlink to /proc/mounts; impossible to know
        // if it came from the base image or was mounted at runtime.
        IgnoreListEntry::new("/etc/mtab", false),
        // /tmp/apt-key-gpghome contains temporary apt keys
        IgnoreListEntry::new("/tmp/apt-key-gpghome", true),
        // /var/run is commonly a mount point for docker.sock etc.
        // Added when --ignore-var-run is set (default true).
    ];
    Mutex::new(entries)
});

/// Global runtime ignore list (default + user additions).
/// Analogous to Go: `var ignorelist = append([]IgnoreListEntry{}, defaultIgnoreList...)`.
static IGNORE_LIST: Lazy<Mutex<Vec<IgnoreListEntry>>> = Lazy::new(|| {
    let default = default_ignore_list();
    Mutex::new(default)
});

/// Get a copy of the default ignore list.
fn default_ignore_list() -> Vec<IgnoreListEntry> {
    DEFAULT_IGNORE_LIST.lock().unwrap().clone()
}

/// Initialize the ignore list.
/// This resets the runtime ignore list to the default + any user additions.
/// Analogous to Go: `util.InitIgnoreList()`.
pub fn init_ignore_list() {
    let default = default_ignore_list();
    let mut list = IGNORE_LIST.lock().unwrap();
    *list = default;
}

/// Add an entry to the runtime ignore list.
/// Analogous to Go: `util.AddToIgnoreList(entry)`.
pub fn add_to_ignore_list(entry: IgnoreListEntry) {
    let mut list = IGNORE_LIST.lock().unwrap();
    list.push(normalize_entry(entry));
}

/// Add an entry to the default ignore list.
/// Analogous to Go: `util.AddToDefaultIgnoreList(entry)`.
pub fn add_to_default_ignore_list(entry: IgnoreListEntry) {
    let mut default = DEFAULT_IGNORE_LIST.lock().unwrap();
    default.push(IgnoreListEntry {
        path: PathBuf::from(entry.path.to_string_lossy().to_string()),
        prefix_match_only: entry.prefix_match_only,
    });
    // Also add to runtime list
    add_to_ignore_list(entry);
}

/// Add /var/run to the default ignore list.
/// Called when --ignore-var-run is set (default: true).
pub fn add_var_run_to_ignore_list() {
    add_to_default_ignore_list(IgnoreListEntry::new("/var/run", false));
}

/// Add user-specified ignore paths from --ignore-path flags.
pub fn add_ignore_paths(paths: &[String]) {
    for path in paths {
        add_to_ignore_list(IgnoreListEntry::new(path, false));
    }
}

/// Get a snapshot of the current ignore list.
pub fn get_ignore_list() -> Vec<IgnoreListEntry> {
    IGNORE_LIST.lock().unwrap().clone()
}

/// Check if a path should be ignored based on the current ignore list.
pub fn is_in_ignore_list(path: &std::path::Path) -> bool {
    let list = IGNORE_LIST.lock().unwrap();
    list.iter().any(|entry| entry.matches(path))
}

/// Normalize an ignore list entry path.
fn normalize_entry(entry: IgnoreListEntry) -> IgnoreListEntry {
    let path = entry.path;
    let cleaned = if path.is_absolute() {
        // Keep as-is for absolute paths
        path
    } else {
        // Make relative paths relative to root
        PathBuf::from("/").join(&path)
    };
    IgnoreListEntry {
        path: cleaned,
        prefix_match_only: entry.prefix_match_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_ignore_list() {
        init_ignore_list();
        let list = get_ignore_list();
        // Should contain /kaniko, /etc/mtab, /tmp/apt-key-gpghome
        assert!(list.iter().any(|e| e.path == PathBuf::from(KANIKO_DIR)));
        assert!(list.iter().any(|e| e.path == PathBuf::from("/etc/mtab")));
        assert!(list.iter().any(|e| e.path == PathBuf::from("/tmp/apt-key-gpghome")));
    }

    #[test]
    fn test_add_to_ignore_list() {
        init_ignore_list();
        let initial_len = get_ignore_list().len();
        add_to_ignore_list(IgnoreListEntry::new("/tmp/test", false));
        let list = get_ignore_list();
        assert_eq!(list.len(), initial_len + 1);
        assert!(list.iter().any(|e| e.path == PathBuf::from("/tmp/test")));
    }

    #[test]
    fn test_is_in_ignore_list() {
        init_ignore_list();
        assert!(is_in_ignore_list(PathBuf::from(KANIKO_DIR).as_path()));
        assert!(is_in_ignore_list(PathBuf::from("/etc/mtab").as_path()));
        assert!(!is_in_ignore_list(PathBuf::from("/usr/bin/ls").as_path()));
    }

    #[test]
    fn test_ignore_entry_prefix_match() {
        let entry = IgnoreListEntry::new("/tmp/apt-key-gpghome", true);
        assert!(entry.matches(PathBuf::from("/tmp/apt-key-gpghome.RANDOM").as_path()));
        assert!(!entry.matches(PathBuf::from("/tmp/other").as_path()));
    }

    #[test]
    fn test_add_var_run() {
        init_ignore_list();
        add_var_run_to_ignore_list();
        assert!(is_in_ignore_list(PathBuf::from("/var/run").as_path()));
    }

    #[test]
    fn test_add_ignore_paths() {
        init_ignore_list();
        add_ignore_paths(&["/custom/path".to_string(), "/another/path".to_string()]);
        assert!(is_in_ignore_list(PathBuf::from("/custom/path").as_path()));
        assert!(is_in_ignore_list(PathBuf::from("/another/path").as_path()));
    }
}