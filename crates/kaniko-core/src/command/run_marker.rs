//! RunMarkerCommand — RUN command with filesystem change detection.
//!
//! Unlike the regular RunCommand, RunMarkerCommand scans the filesystem
//! before and after execution to identify exactly which files changed.
//! This enables precise snapshotting instead of full-filesystem scans.
//!
//! Analogous to Go: `pkg/commands/run_marker.go` — `RunMarkerCommand`.

use crate::command::base::BaseCommand;
use crate::command::mount::{apply_mount, parse_mount, parse_network};
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Filesystem entry metadata for change detection.
#[derive(Debug, Clone)]
struct FileMeta {
    /// File size in bytes.
    size: u64,
    /// Modification time (seconds since epoch).
    mtime_secs: u64,
    /// Whether this is a directory.
    is_dir: bool,
}

/// Scan the filesystem at `root` and collect metadata for all entries.
///
/// Returns a map of relative paths to their metadata.
/// Analogous to Go: `util.GetFSInfoMap()`.
fn scan_fs(root: &Path) -> HashMap<PathBuf, FileMeta> {
    let mut map = HashMap::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path == root {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(path);
        if relative.as_os_str().is_empty() {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let file_meta = FileMeta {
                size: meta.len(),
                mtime_secs: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                is_dir: meta.is_dir(),
            };
            map.insert(relative.to_path_buf(), file_meta);
        }
    }
    map
}

/// Compare two filesystem scans and return the list of changed (added/modified) paths.
fn diff_fs(before: &HashMap<PathBuf, FileMeta>, after: &HashMap<PathBuf, FileMeta>) -> Vec<PathBuf> {
    let mut changed = Vec::new();
    for (path, after_meta) in after {
        match before.get(path) {
            Some(before_meta) => {
                // File existed before — check if modified
                if before_meta.size != after_meta.size
                    || before_meta.mtime_secs != after_meta.mtime_secs
                    || before_meta.is_dir != after_meta.is_dir
                {
                    changed.push(path.clone());
                }
            }
            None => {
                // New file
                changed.push(path.clone());
            }
        }
    }
    changed.sort();
    changed
}

/// RUN command with filesystem change detection.
///
/// Instead of requiring a full filesystem snapshot, this command scans
/// the filesystem before and after execution to identify precisely
/// which files were changed. This is more efficient for large filesystems.
///
/// When `--run-v2` mode is enabled (RunV2 in Go), RUN commands use
/// this marker-based approach instead of the default approach.
#[derive(Debug)]
pub struct RunMarkerCommand {
    /// The command to run.
    command: Vec<String>,
    /// Whether this is in exec form.
    is_exec_form: bool,
    /// Shell to use when not in exec form.
    shell: Option<Vec<String>>,
    /// Mount specifications.
    mounts: Vec<String>,
    /// Whether to cache this layer.
    should_cache: bool,
    /// Network mode.
    network: Option<String>,
    /// Root directory for scanning.
    root_dir: PathBuf,
    /// Files changed by this command (populated after execution).
    changed_files: std::sync::Mutex<Vec<PathBuf>>,
}

impl RunMarkerCommand {
    pub fn new_shell(command: String, should_cache: bool) -> Self {
        Self {
            command: vec![command],
            is_exec_form: false,
            shell: None,
            mounts: vec![],
            should_cache,
            network: None,
            root_dir: PathBuf::from("/"),
            changed_files: std::sync::Mutex::new(vec![]),
        }
    }

    pub fn new_exec(args: Vec<String>, should_cache: bool) -> Self {
        Self {
            command: args,
            is_exec_form: true,
            shell: None,
            mounts: vec![],
            should_cache,
            network: None,
            root_dir: PathBuf::from("/"),
            changed_files: std::sync::Mutex::new(vec![]),
        }
    }

    pub fn with_shell(mut self, shell: Vec<String>) -> Self {
        self.shell = Some(shell);
        self
    }

    pub fn with_mount(mut self, mount: String) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn with_network(mut self, network: String) -> Self {
        self.network = Some(network);
        self
    }

    pub fn with_root_dir(mut self, root_dir: PathBuf) -> Self {
        self.root_dir = root_dir;
        self
    }
}

#[async_trait]
impl BaseCommand for RunMarkerCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        // Apply mount specifications
        for mount_spec in &self.mounts {
            let mount = parse_mount(mount_spec)?;
            apply_mount(&mount)?;
        }

        // Parse network mode
        if let Some(ref network) = self.network {
            let _net_mode = parse_network(network)?;
        }

        let cmd_str = if self.is_exec_form {
            format!("{:?}", self.command)
        } else {
            self.command.first().cloned().unwrap_or_default()
        };
        tracing::info!("RUN (marker) {}", cmd_str);

        // Scan filesystem BEFORE execution
        let before = scan_fs(&self.root_dir);

        let (program, cmd_args) = if self.is_exec_form {
            if self.command.is_empty() {
                return Err(CommandError::Failed(
                    "RUN exec form requires at least one argument".into(),
                ));
            }
            (self.command[0].clone(), self.command[1..].to_vec())
        } else {
            let default_shell = vec!["/bin/sh".to_string(), "-c".to_string()];
            let shell = self.shell.as_deref().unwrap_or(&default_shell);
            let mut full_args = shell[1..].to_vec();
            full_args.push(cmd_str.clone());
            (shell[0].clone(), full_args)
        };

        let workdir = config.working_dir.as_deref().unwrap_or("/");
        let result = tokio::process::Command::new(&program)
            .args(&cmd_args)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .current_dir(workdir)
            .output()
            .await
            .map_err(|e| CommandError::Failed(format!("RUN command failed: {}", e)))?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(CommandError::Failed(format!(
                "RUN command exited with {}: {}",
                result.status,
                stderr.trim()
            )));
        }

        // Scan filesystem AFTER execution
        let after = scan_fs(&self.root_dir);
        *self.changed_files.lock().unwrap() = diff_fs(&before, &after);

        tracing::debug!(
            "RunMarker: {} files changed",
            self.changed_files.lock().unwrap().len()
        );

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        let mut parts = Vec::new();
        for mount in &self.mounts {
            parts.push(format!("--mount={}", mount));
        }
        if let Some(ref network) = self.network {
            parts.push(format!("--network={}", network));
        }
        if self.is_exec_form {
            parts.push(format!("{:?}", self.command));
        } else {
            parts.push(self.command.first().cloned().unwrap_or_default());
        }
        format!("RUN {}", parts.join(" "))
    }

    fn metadata_only_impl(&self) -> bool {
        false
    }

    fn requires_unpacked_fs_impl(&self) -> bool {
        true
    }

    fn should_cache_output_impl(&self) -> bool {
        self.should_cache
    }

    fn should_detect_deleted_files_impl(&self) -> bool {
        true
    }

    fn is_args_envs_required_in_cache_impl(&self) -> bool {
        true
    }

    fn provides_files_to_snapshot_impl(&self) -> bool {
        true
    }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.changed_files.lock().unwrap();
        if files.is_empty() {
            None
        } else {
            Some(files.clone())
        }
    }

    /// Return a cache-aware RUN marker command implementation.
    /// Analogous to Go: `RunMarkerCommand.CacheCommand(img) -> CachingRunCommand`.
    fn cache_command_impl(&self, cached_image: &oci_image::mutate::MutableImage) -> Option<Box<dyn crate::command::DockerCommand>> {
        let command_str = self.command_string_impl();
        Some(Box::new(crate::command::CachingRunCommand::new(
            cached_image.clone(),
            command_str,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_run_marker_command_creation() {
        let cmd = RunMarkerCommand::new_shell("echo hello".to_string(), true);
        assert_eq!(cmd.command_string_impl(), "RUN echo hello");
        assert!(cmd.should_cache_output_impl());
        assert!(cmd.requires_unpacked_fs_impl());
        assert!(cmd.should_detect_deleted_files_impl());
        assert!(cmd.provides_files_to_snapshot_impl());
    }

    #[test]
    fn test_run_marker_exec_form() {
        let cmd = RunMarkerCommand::new_exec(
            vec!["echo".to_string(), "hello".to_string()],
            false,
        );
        assert!(cmd.is_exec_form);
        assert!(!cmd.should_cache_output_impl());
    }

    #[test]
    fn test_scan_fs_and_diff() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create some files
        fs::write(root.join("file1.txt"), "hello").unwrap();
        fs::create_dir(root.join("subdir")).unwrap();
        fs::write(root.join("subdir/file2.txt"), "world").unwrap();

        // Scan before
        let before = scan_fs(root);
        assert!(before.contains_key(PathBuf::from("file1.txt").as_path()));
        assert!(before.contains_key(PathBuf::from("subdir/file2.txt").as_path()));

        // Modify a file
        fs::write(root.join("file1.txt"), "modified").unwrap();
        // Add a new file
        fs::write(root.join("file3.txt"), "new").unwrap();

        // Scan after
        let after = scan_fs(root);
        let changed = diff_fs(&before, &after);

        // file1.txt should be detected as changed
        assert!(changed.contains(&PathBuf::from("file1.txt")));
        // file3.txt should be detected as new
        assert!(changed.contains(&PathBuf::from("file3.txt")));
        // file2.txt should NOT be in the changed list
        assert!(!changed.contains(&PathBuf::from("subdir/file2.txt")));
    }

    #[test]
    fn test_diff_fs_empty() {
        let before = HashMap::new();
        let after = HashMap::new();
        let changed = diff_fs(&before, &after);
        assert!(changed.is_empty());
    }

    #[test]
    fn test_run_marker_with_mount_and_network() {
        let cmd = RunMarkerCommand::new_shell("make build".to_string(), true)
            .with_mount("type=cache,target=/cache".to_string())
            .with_network("none".to_string());
        assert_eq!(
            cmd.command_string_impl(),
            "RUN --mount=type=cache,target=/cache --network=none make build"
        );
    }
}