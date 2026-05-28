//! File system walker with .dockerignore support.
//!
//! Analogous to Go: `pkg/util/fs.FsWalker` + `.dockerignore` parsing.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors for walker operations.
#[derive(Debug, Error)]
pub enum WalkerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("walk error: {0}")]
    Walk(#[from] walkdir::Error),
}

/// Result type for walker operations.
pub type Result<T> = std::result::Result<T, WalkerError>;

/// A parsed .dockerignore pattern.
#[derive(Debug, Clone)]
pub struct IgnorePattern {
    /// The raw pattern string.
    pub pattern: String,
    /// Whether this is a negation pattern (prefixed with `!`).
    pub negation: bool,
    /// Whether this pattern matches directories only.
    pub dir_only: bool,
}

impl IgnorePattern {
    /// Parse a .dockerignore line into a pattern.
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim();
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        let negation = line.starts_with('!');
        let pattern = if negation { &line[1..] } else { line };
        let dir_only = pattern.ends_with('/');

        Some(Self {
            pattern: pattern.trim_end_matches('/').to_string(),
            negation,
            dir_only,
        })
    }

    /// Check if a path matches this pattern.
    ///
    /// Supports the following .dockerignore syntax:
    /// - `*` matches any sequence of non-separator characters
    /// - `**` matches any sequence of characters (including separators)
    /// - `?` matches any single non-separator character
    /// - `foo/` matches only directories named foo
    /// - `!foo` negates the pattern (un-ignore)
    /// - Lines starting with `#` are comments
    pub fn matches(&self, path: &Path, is_dir: bool) -> bool {
        // Dir-only patterns only match directories
        if self.dir_only && !is_dir {
            return false;
        }

        let path_str = path.to_string_lossy();
        let pattern = &self.pattern;

        // Exact match
        if path_str == *pattern {
            return true;
        }

        // ** pattern — match any path prefix/suffix
        if pattern.contains("**") {
            if glob_match_doublestar(pattern, &path_str) {
                return true;
            }
        }

        // Glob-style matching (single *)
        if pattern.contains('*') || pattern.contains('?') {
            if glob_match(pattern, &path_str) {
                return true;
            }
        }

        // Prefix match (e.g., "foo" matches "foo/bar")
        if path_str.starts_with(&format!("{}/", pattern)) || 
            format!("{}/", pattern).starts_with(&*path_str) {
            return true;
        }

        // Pattern matches any component of the path
        for component in path.components() {
            if let std::path::Component::Normal(os_str) = component {
                if os_str.to_string_lossy() == *pattern {
                    return true;
                }
                // Also try glob matching on individual components
                if pattern.contains('*') || pattern.contains('?') {
                    if glob_match(pattern, &os_str.to_string_lossy()) {
                        return true;
                    }
                }
            }
        }

        false
    }
}

/// Parse a .dockerignore file into a list of patterns.
pub fn parse_dockerignore(content: &str) -> Vec<IgnorePattern> {
    content
        .lines()
        .filter_map(|line| IgnorePattern::parse(line))
        .collect()
}

/// Read and parse a .dockerignore file from the given directory.
pub fn read_dockerignore(context_dir: &Path) -> Vec<IgnorePattern> {
    let ignore_path = context_dir.join(".dockerignore");
    if let Ok(content) = std::fs::read_to_string(&ignore_path) {
        parse_dockerignore(&content)
    } else {
        vec![]
    }
}

/// Check if a path should be ignored based on .dockerignore patterns.
pub fn is_ignored(path: &Path, patterns: &[IgnorePattern], is_dir: bool) -> bool {
    let mut ignored = false;

    for pattern in patterns {
        if pattern.matches(path, is_dir) {
            if pattern.negation {
                ignored = false;
            } else {
                ignored = true;
            }
        }
    }

    ignored
}

/// Walk a directory tree, respecting .dockerignore patterns.
///
/// Returns all non-ignored file paths relative to the root.
/// Directories that match ignore patterns are skipped entirely for efficiency.
pub fn walk_with_ignore(root: &Path, patterns: &[IgnorePattern]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !root.exists() {
        return Ok(files);
    }

    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            // Skip ignored directories entirely (don't descend into them)
            if e.file_type().is_dir() {
                !is_ignored(e.path(), patterns, true)
            } else {
                !is_ignored(e.path(), patterns, false)
            }
        })
    {
        let entry = entry?;
        let path = entry.path().to_path_buf();
        let is_dir = entry.file_type().is_dir();

        // Skip ignored paths (redundant check for safety)
        if is_ignored(path.as_path(), patterns, is_dir) {
            continue;
        }

        files.push(path);
    }

    Ok(files)
}

/// Walk a directory tree with default kaniko ignore paths.
///
/// Returns all file paths, excluding /proc, /sys, /dev, etc.
pub fn walk_for_snapshot(root: &Path, extra_patterns: &[IgnorePattern]) -> Result<Vec<PathBuf>> {
    let mut all_patterns = get_default_snapshot_ignores();
    all_patterns.extend_from_slice(extra_patterns);
    walk_with_ignore(root, &all_patterns)
}

/// Get the default ignore patterns for kaniko snapshots.
fn get_default_snapshot_ignores() -> Vec<IgnorePattern> {
    vec![
        IgnorePattern { pattern: "proc".to_string(), negation: false, dir_only: true },
        IgnorePattern { pattern: "sys".to_string(), negation: false, dir_only: true },
        IgnorePattern { pattern: "dev".to_string(), negation: false, dir_only: true },
        IgnorePattern { pattern: "kaniko".to_string(), negation: false, dir_only: true },
        IgnorePattern { pattern: "var/run".to_string(), negation: false, dir_only: true },
        IgnorePattern { pattern: "etc/mtab".to_string(), negation: false, dir_only: false },
    ]
}

/// Simple glob matching (supports * and ? wildcards).
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_impl(&pattern, &text, 0, 0)
}

fn glob_match_impl(pattern: &[char], text: &[char], pi: usize, ti: usize) -> bool {
    if pi == pattern.len() && ti == text.len() {
        return true;
    }
    if pi == pattern.len() {
        return false;
    }

    match pattern[pi] {
        '*' => {
            // Match zero or more characters
            for i in ti..=text.len() {
                if glob_match_impl(pattern, text, pi + 1, i) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if ti < text.len() {
                glob_match_impl(pattern, text, pi + 1, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < text.len() && text[ti] == c {
                glob_match_impl(pattern, text, pi + 1, ti + 1)
            } else {
                false
            }
        }
    }
}

/// Glob matching with ** (doublestar) support.
///
/// `**` matches any sequence of characters including path separators.
/// This is the standard .dockerignore behavior for recursive matching.
fn glob_match_doublestar(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split("**").collect();
    if parts.len() == 1 {
        // No ** in pattern, use regular glob
        return glob_match(pattern, text);
    }

    // Match each part against the text, allowing any separator content between them
    let mut text_remaining = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        if i == 0 {
            // First part must match the beginning
            if !text_remaining.starts_with(part) {
                return false;
            }
            text_remaining = &text_remaining[part.len()..];
        } else if i == parts.len() - 1 {
            // Last part must match the end
            if !text_remaining.ends_with(part) {
                // Try matching part as a glob against the remaining text
                if part.contains('*') || part.contains('?') {
                    return glob_match(part, text_remaining);
                }
                return false;
            }
            text_remaining = &text_remaining[..text_remaining.len() - part.len()];
        } else {
            // Middle part must appear somewhere in the remaining text
            if let Some(pos) = text_remaining.find(part) {
                text_remaining = &text_remaining[pos + part.len()..];
            } else {
                return false;
            }
        }
    }

    // If we consumed all pattern parts, the match succeeds
    // (text_remaining can be anything between the ** gaps)
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dockerignore() {
        let content = "# comment\n*.log\n!important.log\ntmp/\n";
        let patterns = parse_dockerignore(content);
        assert_eq!(patterns.len(), 3);
        assert_eq!(patterns[0].pattern, "*.log");
        assert!(!patterns[0].negation);
        assert!(!patterns[0].dir_only);

        assert_eq!(patterns[1].pattern, "important.log");
        assert!(patterns[1].negation);

        assert_eq!(patterns[2].pattern, "tmp");
        assert!(patterns[2].dir_only);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.log", "app.log"));
        assert!(!glob_match("*.log", "app.txt"));
        assert!(glob_match("file?.txt", "file1.txt"));
        assert!(glob_match("*", "anything"));
    }

    #[test]
    fn test_ignore_pattern_matching() {
        let pattern = IgnorePattern::parse("*.log").unwrap();
        assert!(pattern.matches(Path::new("app.log"), false));
        assert!(!pattern.matches(Path::new("app.txt"), false));
    }

    #[test]
    fn test_negation_pattern() {
        let patterns = vec![
            IgnorePattern { pattern: "*.log".to_string(), negation: false, dir_only: false },
            IgnorePattern { pattern: "important.log".to_string(), negation: true, dir_only: false },
        ];
        assert!(is_ignored(Path::new("debug.log"), &patterns, false));
        assert!(!is_ignored(Path::new("important.log"), &patterns, false));
        assert!(!is_ignored(Path::new("app.txt"), &patterns, false));
    }

    #[test]
    fn test_doublestar_glob() {
        // ** matches across directories
        assert!(glob_match_doublestar("**/test", "src/pkg/test"));
        assert!(glob_match_doublestar("src/**", "src/pkg/test/file.rs"));
        assert!(glob_match_doublestar("src/**/test", "src/pkg/mod/test"));
        assert!(glob_match_doublestar("**", "anything/at/all"));
    }

    #[test]
    fn test_dockerignore_complex_patterns() {
        let content = r#"
# Ignore all log files
*.log
# But keep important logs
!important.log
# Ignore all files in any build directory
**/build/
# Ignore temp directories at any level
**/tmp/
# Keep specific config
!config/important.conf
"#;
        let patterns = parse_dockerignore(content);
        assert_eq!(patterns.len(), 5);
        
        // *.log should match regular log files
        assert!(patterns[0].matches(Path::new("app.log"), false));
        assert!(!patterns[0].matches(Path::new("app.txt"), false));
        
        // !important.log should be a negation
        assert!(patterns[1].negation);
        
        // **/build should match build at any depth
        assert!(patterns[2].matches(Path::new("src/build"), true));
        assert!(patterns[2].dir_only);
    }

    #[test]
    fn test_dir_only_pattern() {
        let pattern = IgnorePattern::parse("node_modules/").unwrap();
        assert!(pattern.dir_only);
        assert!(pattern.matches(Path::new("node_modules"), true));
        assert!(!pattern.matches(Path::new("node_modules"), false)); // Not a dir
    }

    #[test]
    fn test_default_snapshot_ignores() {
        let patterns = get_default_snapshot_ignores();
        assert!(!patterns.is_empty());
        
        // /proc should be ignored
        assert!(is_ignored(Path::new("proc"), &patterns, true));
        // /sys should be ignored
        assert!(is_ignored(Path::new("sys"), &patterns, true));
        // /dev should be ignored
        assert!(is_ignored(Path::new("dev"), &patterns, true));
        // Regular files should not be ignored
        assert!(!is_ignored(Path::new("usr/bin/app"), &patterns, false));
    }
}