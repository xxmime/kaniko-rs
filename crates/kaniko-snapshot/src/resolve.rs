//! Filesystem path resolution for symlinks and parent directories.
//!
//! Analogous to Go: `pkg/filesystem/resolve.go`.
//!
//! Resolves symlink ancestors, evaluates symlinks in root,
//! and adds parent directories for correct permission handling.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ignore_list::IgnoreListEntry;

/// Resolve paths according to a set of rules:
/// - If path is in ignorelist, skip it.
/// - If path is a symlink, resolve its ancestor link and add it.
/// - If path is a symlink, resolve its target. If not ignored, add it.
/// - Add all ancestors of each path.
///
/// Analogous to Go: `ResolvePaths()`.
pub fn resolve_paths(
    paths: &[String],
    ignore_list: &[IgnoreListEntry],
) -> Vec<String> {
    tracing::trace!("Resolving paths {:?}", paths);

    let mut paths_to_add = Vec::new();
    let mut file_set = HashSet::new();

    for f in paths {
        // If the given path is part of the ignorelist, ignore it
        if is_in_provided_ignore_list(f, ignore_list) {
            tracing::debug!("Path {} is in list to ignore, ignoring it", f);
            continue;
        }

        // Resolve symlink ancestor
        let link = match resolve_symlink_ancestor(f) {
            Ok(l) => l,
            Err(_) => continue,
        };

        if f != &link {
            tracing::trace!("Updated link {} to {}", f, link);
        }

        if !file_set.contains(&link) {
            paths_to_add.push(link.clone());
        }
        file_set.insert(link);

        // If the path is a symlink, also consider the target
        match eval_symlinks_in_root(f) {
            Ok(evaled) => {
                if f != &evaled {
                    tracing::trace!("Resolved symlink {} to {}", f, evaled);
                }

                // If target is in ignorelist, skip it
                if check_cleaned_path_against_ignore_list(&evaled, ignore_list) {
                    tracing::debug!("Path {} is ignored, ignoring it", evaled);
                    continue;
                }

                if !file_set.contains(&evaled) {
                    paths_to_add.push(evaled.clone());
                }
                file_set.insert(evaled);
            }
            Err(_) => {
                tracing::trace!("Symlink path {}, target does not exist", f);
                continue;
            }
        }
    }

    // Add parent directories for correct permissions
    files_with_parent_dirs(&paths_to_add)
}

/// Add parent directories for each file path.
///
/// E.g. /foo/bar/baz/boom.txt => [/, /foo, /foo/bar, /foo/bar/baz, /foo/bar/baz/boom.txt]
///
/// Analogous to Go: `filesWithParentDirs()`.
pub fn files_with_parent_dirs(files: &[String]) -> Vec<String> {
    let mut file_set = HashSet::new();

    for file in files {
        let cleaned = Path::new(file);
        file_set.insert(cleaned.to_string_lossy().to_string());

        // Add all parent directories
        for dir in crate::fs_util::parent_directories(file) {
            file_set.insert(dir.clone());
        }
    }

    file_set.into_iter().collect()
}

/// Resolve the ancestor link of a symlink path.
///
/// Returns the path itself if it is not a link.
///
/// Analogous to Go: `resolveSymlinkAncestor()`.
pub fn resolve_symlink_ancestor(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("dest path must be abs".to_string());
    }

    let root = Path::new(crate::fs_util::KANIKO_ROOT_DIR);
    let mut new_path = p.to_path_buf();
    let mut last = String::new();

    while new_path != root {
        let metadata = match fs::symlink_metadata(&new_path) {
            Ok(m) => m,
            Err(e) => return Err(format!("resolvePaths: failed to lstat: {}", e)),
        };

        if metadata.file_type().is_symlink() {
            last = new_path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            new_path = new_path.parent().unwrap_or(root).to_path_buf();
        } else {
            // Check if any ancestor is a symlink
            let target = match fs::canonicalize(&new_path) {
                Ok(t) => t,
                Err(_) => break,
            };

            if target != new_path {
                last = new_path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                new_path = new_path.parent().unwrap_or(root).to_path_buf();
            } else {
                break;
            }
        }
    }

    if !last.is_empty() {
        new_path = new_path.join(&last);
    }

    Ok(new_path.to_string_lossy().to_string())
}

/// Evaluate symlinks in the root directory.
///
/// Analogous to Go: `evalSymlinksInRoot()`.
fn eval_symlinks_in_root(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    match fs::canonicalize(p) {
        Ok(resolved) => Ok(resolved.to_string_lossy().to_string()),
        Err(e) => Err(format!("failed to eval symlinks: {}", e)),
    }
}

/// Check if a path is in the provided ignore list.
fn is_in_provided_ignore_list(path: &str, ignore_list: &[IgnoreListEntry]) -> bool {
    for entry in ignore_list {
        let entry_path = entry.path.to_string_lossy();
        if entry.prefix_match_only {
            if path.starts_with(entry_path.as_ref()) {
                return true;
            }
        } else if path == entry_path.as_ref() {
            return true;
        }
    }
    false
}

/// Check a cleaned path against the ignore list.
fn check_cleaned_path_against_ignore_list(path: &str, ignore_list: &[IgnoreListEntry]) -> bool {
    is_in_provided_ignore_list(path, ignore_list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_symlink_ancestor_regular() {
        // Create a real regular file for testing
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("regular.txt");
        fs::write(&file_path, b"test").unwrap();
        let abs_path = file_path.to_string_lossy().to_string();
        let result = resolve_symlink_ancestor(&abs_path);
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_symlink_ancestor_relative_error() {
        let result = resolve_symlink_ancestor("relative/path");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be abs"));
    }

    #[test]
    fn test_files_with_parent_dirs() {
        let files = vec!["/foo/bar/baz.txt".to_string()];
        let result = files_with_parent_dirs(&files);
        assert!(result.contains(&"/foo".to_string()));
        assert!(result.contains(&"/foo/bar".to_string()));
        assert!(result.contains(&"/foo/bar/baz.txt".to_string()));
        // Should contain at least the file and its parents
        assert!(result.len() >= 3);
    }

    #[test]
    fn test_is_in_provided_ignore_list() {
        let ignore_list = vec![
            IgnoreListEntry::new("/tmp", true),  // prefix match
            IgnoreListEntry::new("/kaniko", false), // exact match
        ];
        assert!(is_in_provided_ignore_list("/tmp/apt-key-gpghome.RANDOM", &ignore_list));
        assert!(is_in_provided_ignore_list("/kaniko", &ignore_list));
        assert!(!is_in_provided_ignore_list("/usr/bin", &ignore_list));
    }

    #[test]
    fn test_resolve_paths_empty() {
        let result = resolve_paths(&[], &[]);
        assert!(result.is_empty());
    }
}