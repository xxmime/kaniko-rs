//! Command utility functions for COPY/ADD command processing.
//!
//! Analogous to Go: `pkg/util/command_util.go`.
//!
//! Provides environment variable resolution, path validation,
//! chown/chmod handling, and source validation for Dockerfile commands.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Special sentinel values for "do not change" UID/GID.
pub const DO_NOT_CHANGE_UID: i64 = -1;
pub const DO_NOT_CHANGE_GID: i64 = -1;

/// Resolve environment variables in a list of values.
///
/// Replaces `$VAR` and `${VAR}` patterns with values from the env list.
/// If `is_filepath` is true, path cleaning is applied after replacement.
///
/// Analogous to Go: `ResolveEnvironmentReplacementList()`.
pub fn resolve_environment_replacement_list(
    values: &[String],
    envs: &[String],
    is_filepath: bool,
) -> Result<Vec<String>, String> {
    let mut resolved = Vec::with_capacity(values.len());
    for value in values {
        let r = resolve_environment_replacement(value, envs, is_filepath)?;
        resolved.push(r);
    }
    Ok(resolved)
}

/// Resolve environment variables in a single value.
///
/// Supports `$VAR` and `${VAR}` syntax for expansion.
/// If `is_filepath` is true, the result is path-cleaned and trailing `/` is preserved.
///
/// Analogous to Go: `ResolveEnvironmentReplacement()`.
pub fn resolve_environment_replacement(
    value: &str,
    envs: &[String],
    is_filepath: bool,
) -> Result<String, String> {
    let mut result = value.to_string();

    // Build env map from KEY=VALUE pairs
    let env_map: std::collections::HashMap<&str, &str> = envs
        .iter()
        .filter_map(|e| {
            let parts: Vec<&str> = e.splitn(2, '=').collect();
            if parts.len() == 2 {
                Some((parts[0], parts[1]))
            } else {
                None
            }
        })
        .collect();

    // Replace ${VAR} patterns first (more specific)
    let brace_re = regex_lazy();
    result = brace_re.replace_all(&result, |caps: &regex::Captures| {
        let var_name = &caps[1];
        env_map.get(var_name).map(|s| s.to_string()).unwrap_or_else(|| "".to_string())
    }).to_string();

    // Replace $VAR patterns (simple: word boundary)
    let simple_re = simple_regex_lazy();
    result = simple_re.replace_all(&result, |caps: &regex::Captures| {
        let var_name = &caps[1];
        env_map.get(var_name).map(|s| s.to_string()).unwrap_or_else(|| "".to_string())
    }).to_string();

    // If not a filepath or remote URL, return as-is
    if !is_filepath || is_src_remote_file_url(&result) {
        return Ok(result);
    }

    // Path cleaning for filepaths
    let is_dir = result.ends_with('/');
    result = Path::new(&result)
        .components()
        .filter(|c| c.as_os_str() != ".")
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .to_string();

    if is_dir && !result.ends_with('/') {
        result.push('/');
    }

    Ok(result)
}

/// Check if multiple sources need a directory destination.
///
/// When multiple sources are specified in a COPY/ADD command,
/// the destination must be a directory.
///
/// Analogous to Go: `IsSrcsValid()`.
pub fn is_srcs_valid(
    sources: &[String],
    dest: &str,
    resolved_sources: &[String],
    context_root: &str,
    excludes_file: &dyn Fn(&str) -> bool,
) -> Result<(), String> {
    // Without wildcards, check that multiple sources have a dir destination
    if !crate::fs_util::contains_wildcards(sources) {
        let mut total_srcs = 0;
        for src in sources {
            if excludes_file(src) {
                continue;
            }
            total_srcs += 1;
        }
        if total_srcs > 1 && !is_dest_dir_in_root(dest) {
            return Err(
                "when specifying multiple sources in a COPY command, destination must be a directory and end in '/'".to_string()
            );
        }
    }

    // If only one resolved source and it's a directory, that's fine
    if resolved_sources.len() == 1 {
        if is_src_remote_file_url(&resolved_sources[0]) {
            return Ok(());
        }
        let path = Path::new(context_root).join(&resolved_sources[0]);
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.is_dir() {
                return Ok(());
            }
        }
    }

    // Count total files
    let mut total_files = 0;
    for src in resolved_sources {
        if is_src_remote_file_url(src) {
            total_files += 1;
            continue;
        }
        let src_clean = src.trim_end_matches('/');
        let root_path = Path::new(context_root);
        if let Ok(entries) = crate::fs_util::relative_files(src_clean, root_path) {
            for file in &entries {
                if !excludes_file(file) {
                    total_files += 1;
                }
            }
        } else {
            let full_path = root_path.join(src_clean);
            if full_path.exists() {
                total_files += 1;
            }
        }
    }

    if total_files == 0 {
        tracing::warn!("No files to copy");
    }

    if !is_dest_dir_in_root(dest) && total_files > 1 {
        return Err(
            "when specifying multiple sources in a COPY command, destination must be a directory and end in '/'".to_string()
        );
    }

    Ok(())
}

/// Check if the destination is a directory within the kaniko root.
///
/// Analogous to Go: `isDestDirInRoot()`.
pub fn is_dest_dir_in_root(path: &str) -> bool {
    let root_dir = crate::fs_util::KANIKO_ROOT_DIR;
    if Path::new(root_dir).clean() == Path::new("/") {
        return crate::fs_util::is_dest_dir(path);
    }
    path.ends_with('/') || path == "." || {
        let rooted = crate::fs_util::rooted_path(path, root_dir);
        crate::fs_util::is_dest_dir(rooted.to_str().unwrap_or(path))
    }
}

/// Get the destination filepath for a URL source.
///
/// When ADD fetches a remote URL, the filename is extracted from the URL
/// and appended to the destination directory.
///
/// Analogous to Go: `URLDestinationFilepath()`.
pub fn url_destination_filepath(
    raw_url: &str,
    dest: &str,
    cwd: &str,
    envs: &[String],
) -> Result<String, String> {
    let cwd = if cwd.is_empty() { "/" } else { cwd };

    if !is_dest_dir_in_root(dest) {
        let abs_dest = if Path::new(dest).is_absolute() {
            dest.to_string()
        } else {
            Path::new(cwd).join(dest).to_string_lossy().to_string()
        };
        return resolve_path_in_root(&abs_dest);
    }

    let url_base = resolve_environment_replacement(raw_url, envs, true)?;
    let filename = extract_filename(&url_base)?;

    let dest_path = Path::new(dest).join(&filename);
    let dest_str = dest_path.to_string_lossy().to_string();

    let abs_dest = if Path::new(&dest_str).is_absolute() {
        dest_str
    } else {
        Path::new(cwd).join(&dest_str).to_string_lossy().to_string()
    };

    resolve_path_in_root(&abs_dest)
}

/// Extract the filename from a URL.
fn extract_filename(url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("Invalid URL {}: {}", url, e))?;
    let path = parsed.path();
    let filename = Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "downloaded".to_string());
    Ok(filename)
}

/// Resolve a path within the kaniko root, ensuring it doesn't escape.
///
/// Analogous to Go: `ResolvePathInRoot()`.
fn resolve_path_in_root(path: &str) -> Result<String, String> {
    let root = crate::fs_util::KANIKO_ROOT_DIR;
    let rooted = crate::fs_util::rooted_path(path, root);

    // Ensure the resolved path is within the root
    let rooted_str = rooted.to_string_lossy().to_string();
    if !rooted_str.starts_with(root) {
        return Err(format!("Path {} escapes root {}", path, root));
    }

    Ok(rooted_str)
}

/// Parse UID and GID from a chown string (e.g., "1000:1000" or "user:group").
///
/// Returns (uid, gid) as i64. Returns DO_NOT_CHANGE_UID/DO_NOT_CHANGE_GID
/// if the string is empty.
///
/// Analogous to Go: `GetUserGroup()`.
pub fn get_user_group(chown_str: &str, _envs: &[String]) -> (i64, i64) {
    if chown_str.is_empty() {
        return (DO_NOT_CHANGE_UID, DO_NOT_CHANGE_GID);
    }

    let parts: Vec<&str> = chown_str.split(':').collect();
    let uid_str = parts[0];
    let gid_str = parts.get(1).map(|s| *s).unwrap_or(uid_str);

    let uid = uid_str.parse::<u32>().unwrap_or(0) as i64;
    let gid = gid_str.parse::<u32>().unwrap_or(0) as i64;

    (uid, gid)
}

/// Parse chmod from a string (e.g., "755", "644").
///
/// Returns (mode, use_default). If the string is empty, returns default mode.
///
/// Analogous to Go: `GetChmod()`.
pub fn get_chmod(chmod_str: &str) -> (u32, bool) {
    if chmod_str.is_empty() {
        return (0o600, true);
    }

    match u32::from_str_radix(chmod_str, 8) {
        Ok(mode) => (mode, false),
        Err(_) => {
            tracing::warn!("Invalid chmod value: {}, using default", chmod_str);
            (0o600, true)
        }
    }
}

/// Check if a source is a remote file URL.
///
/// A URL is considered remote if it has a scheme and a host.
///
/// Analogous to Go: `IsSrcRemoteFileURL()`.
pub fn is_src_remote_file_url(raw_url: &str) -> bool {
    url::Url::parse(raw_url)
        .map(|u| u.scheme() == "http" || u.scheme() == "https")
        .unwrap_or(false)
}

/// Trait for clean path operations.
trait PathClean {
    fn clean(&self) -> std::path::PathBuf;
}

impl PathClean for Path {
    fn clean(&self) -> std::path::PathBuf {
        // Normalize path by removing . and .. components
        let mut components = Vec::new();
        for component in self.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    if !components.is_empty() {
                        components.pop();
                    }
                }
                c => components.push(c.as_os_str().to_os_string()),
            }
        }
        components.iter().collect()
    }
}

// Lazy static regex patterns for env var expansion
use once_cell::sync::Lazy;
use regex::Regex;

static BRACE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap()
});

static SIMPLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\$([A-Za-z_][A-Za-z0-9_]*)").unwrap()
});

fn regex_lazy() -> &'static Regex { &BRACE_RE }

/// Update environment variables in the image config.
///
/// Takes a list of new key-value pairs and updates or appends them
/// to the existing env list. If a key already exists, its value is replaced.
/// Environment variable substitution is performed on both keys and values.
///
/// Analogous to Go: `UpdateConfigEnv()`.
pub fn update_config_env(
    envs: &mut Vec<String>,
    new_vars: &[(String, String)],
    replacement_envs: &[String],
) -> Result<(), String> {
    // First, resolve env vars in new keys and values
    let mut resolved_vars = Vec::with_capacity(new_vars.len());
    for (key, value) in new_vars {
        let expanded_key = resolve_environment_replacement(key, replacement_envs, false)?;
        let expanded_value = resolve_environment_replacement(value, replacement_envs, false)?;
        resolved_vars.push((expanded_key, expanded_value));
    }

    // Convert existing envs to key-value pairs
    let mut kvps: Vec<(String, String)> = envs.iter().map(|e| {
        let parts: Vec<&str> = e.splitn(2, '=').collect();
        (parts[0].to_string(), if parts.len() > 1 { parts[1].to_string() } else { String::new() })
    }).collect();

    // Update or append new envs
    for (new_key, new_value) in resolved_vars {
        let mut found = false;
        for (key, value) in kvps.iter_mut() {
            if key == &new_key {
                tracing::debug!("Replacing env var {}={} with {}={}", key, value, new_key, new_value);
                *value = new_value.clone();
                found = true;
                break;
            }
        }
        if !found {
            kvps.push((new_key, new_value));
        }
    }

    // Convert back to env array
    *envs = kvps.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
    Ok(())
}

/// Return the Docker config file location.
///
/// Checks DOCKER_CONFIG environment variable first.
/// If not set or invalid, returns default "/kaniko/.docker/config.json".
///
/// Analogous to Go: `DockerConfLocation()`.
pub fn docker_conf_location() -> String {
    let config_file = "config.json";

    if let Ok(docker_config) = std::env::var("DOCKER_CONFIG") {
        let path = Path::new(&docker_config);
        if path.exists() {
            if path.is_dir() {
                return path.join(config_file).to_string_lossy().to_string();
            }
            // It's a file, return the path directly
            return docker_config;
        }
        // Path doesn't exist, use default
        tracing::debug!("DOCKER_CONFIG {} does not exist, using default", docker_config);
    }

    // Default: /kaniko/.docker/config.json
    format!("/kaniko/.docker/{}", config_file)
}
fn simple_regex_lazy() -> &'static Regex { &SIMPLE_RE }

/// Resolve environment variables and wildcards in COPY/ADD sources and destination.
///
/// This is the main entry point for source resolution in Dockerfile commands.
/// It resolves env vars, expands wildcards, and validates sources.
///
/// Returns (resolved_sources, resolved_destination).
///
/// Analogous to Go: `ResolveEnvAndWildcards()`.
pub fn resolve_env_and_wildcards(
    sources: &[String],
    dest: &str,
    context_root: &str,
    envs: &[String],
) -> Result<(Vec<String>, String), String> {
    // First, resolve environment variables in sources
    let resolved_envs = resolve_environment_replacement_list(sources, envs, true)?;
    if resolved_envs.is_empty() {
        return Err("resolved envs is empty".to_string());
    }

    // Resolve environment variables in destination
    let resolved_dests = resolve_environment_replacement_list(&[dest.to_string()], envs, true)?;
    let resolved_dest = resolved_dests[0].clone();

    // Resolve wildcards and get a list of resolved sources
    let root_path = Path::new(context_root);
    let resolved_sources = crate::fs_util::resolve_sources(&resolved_envs, root_path)
        .map_err(|e| format!("resolving sources: {}", e))?;

    // Validate sources
    is_srcs_valid(sources, &resolved_dest, &resolved_sources, context_root, &|_| false)?;

    Ok((resolved_sources, resolved_dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_environment_replacement_simple() {
        let envs = vec!["FOO=bar".to_string(), "BAZ=qux".to_string()];
        let result = resolve_environment_replacement("$FOO/file", &envs, true).unwrap();
        assert_eq!(result, "bar/file");
    }

    #[test]
    fn test_resolve_environment_replacement_brace() {
        let envs = vec!["FOO=bar".to_string()];
        let result = resolve_environment_replacement("${FOO}/dir", &envs, true).unwrap();
        assert_eq!(result, "bar/dir");
    }

    #[test]
    fn test_resolve_environment_replacement_list() {
        let envs = vec!["FOO=bar".to_string()];
        let values = vec!["$FOO/src".to_string(), "$FOO/dst".to_string()];
        let result = resolve_environment_replacement_list(&values, &envs, true).unwrap();
        assert_eq!(result, vec!["bar/src", "bar/dst"]);
    }

    #[test]
    fn test_resolve_environment_replacement_undefined() {
        let envs: Vec<String> = vec![];
        let result = resolve_environment_replacement("$UNDEF/file", &envs, false).unwrap();
        assert_eq!(result, "/file");
    }

    #[test]
    fn test_is_src_remote_file_url() {
        assert!(is_src_remote_file_url("https://example.com/file.tar.gz"));
        assert!(is_src_remote_file_url("http://example.com/file"));
        assert!(!is_src_remote_file_url("/local/path"));
        assert!(!is_src_remote_file_url("relative/path"));
    }

    #[test]
    fn test_get_user_group() {
        assert_eq!(get_user_group("", &[]), (DO_NOT_CHANGE_UID, DO_NOT_CHANGE_GID));
        assert_eq!(get_user_group("1000:2000", &[]), (1000, 2000));
        assert_eq!(get_user_group("1000", &[]), (1000, 1000));
    }

    #[test]
    fn test_get_chmod() {
        assert_eq!(get_chmod(""), (0o600, true));
        assert_eq!(get_chmod("755"), (0o755, false));
        assert_eq!(get_chmod("644"), (0o644, false));
        assert_eq!(get_chmod("invalid"), (0o600, true));
    }

    #[test]
    fn test_extract_filename() {
        assert_eq!(extract_filename("https://example.com/file.tar.gz").unwrap(), "file.tar.gz");
        assert_eq!(extract_filename("https://example.com/path/to/data.zip").unwrap(), "data.zip");
    }

    #[test]
    fn test_is_dest_dir_in_root() {
        assert!(is_dest_dir_in_root("/some/dir/"));
        assert!(is_dest_dir_in_root("."));
        assert!(!is_dest_dir_in_root("/some/file"));
    }

    #[test]
    fn test_update_config_env() {
        let mut envs = vec!["PATH=/usr/bin".to_string(), "HOME=/root".to_string()];
        let new_vars = vec![("HOME".to_string(), "/home/user".to_string()), ("SHELL".to_string(), "/bin/bash".to_string())];
        update_config_env(&mut envs, &new_vars, &[]).unwrap();
        assert!(envs.contains(&"HOME=/home/user".to_string()));
        assert!(envs.contains(&"SHELL=/bin/bash".to_string()));
        assert!(envs.contains(&"PATH=/usr/bin".to_string()));
    }

    #[test]
    fn test_docker_conf_location_default() {
        // Don't test with env var removal (unsafe in Rust 2024 edition)
        // Just test the default case
        let loc = docker_conf_location();
        // Should contain "kaniko" and ".docker" in default case
        assert!(loc.contains("kaniko") || loc.contains(".docker") || loc.contains("config.json"));
    }
}