//! COPY command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::collections::HashMap;

/// COPY instruction — copies files from build context to the image.
#[derive(Debug)]
pub struct CopyCommand {
    sources: Vec<String>,
    destination: String,
    from: Option<String>,
    chown: Option<String>,
    chmod: Option<String>,
    link: bool,
    should_cache: bool,
    snapshot_files: Mutex<Vec<PathBuf>>,
    context_dir: PathBuf,
    /// Maps stage names/indices to their built images for --from support
    stages: HashMap<String, oci_image::mutate::MutableImage>,
}

impl CopyCommand {
    pub fn new(
        sources: Vec<String>,
        destination: String,
        from: Option<String>,
        context_dir: PathBuf,
        should_cache: bool,
    ) -> Self {
        Self {
            sources,
            destination,
            from,
            chown: None,
            chmod: None,
            link: false,
            should_cache,
            snapshot_files: Mutex::new(vec![]),
            context_dir,
            stages: HashMap::new(),
        }
    }

    /// Create a CopyCommand with all flags from the parsed instruction.
    pub fn with_flags(
        sources: Vec<String>,
        destination: String,
        from: Option<String>,
        chown: Option<String>,
        chmod: Option<String>,
        link: bool,
        context_dir: PathBuf,
        should_cache: bool,
    ) -> Self {
        Self {
            sources,
            destination,
            from,
            chown,
            chmod,
            link,
            should_cache,
            snapshot_files: Mutex::new(vec![]),
            context_dir,
            stages: HashMap::new(),
        }
    }

    /// Set the stages map for --from support
    pub fn with_stages(mut self, stages: HashMap<String, oci_image::mutate::MutableImage>) -> Self {
        self.stages = stages;
        self
    }

    async fn copy_from_context(&self, dest: &str) -> Result<()> {
        for src in &self.sources {
            let src_path = self.context_dir.join(src);
            if !src_path.exists() {
                return Err(CommandError::Failed(format!(
                    "COPY source not found: {}",
                    src_path.display()
                )));
            }

            // Resolve destination if it's a symlink
            let resolved_dest = resolve_if_symlink(dest)?;

            let metadata = std::fs::symlink_metadata(&src_path)?;
            if metadata.is_dir() {
                if self.link {
                    copy_dir_recursive_link(&src_path, Path::new(&resolved_dest), &self.chown, &self.chmod)?;
                } else {
                    copy_dir_recursive(&src_path, Path::new(&resolved_dest), &self.chown, &self.chmod)?;
                }
            } else if metadata.file_type().is_symlink() {
                // Copy symlink target (--link doesn't apply to symlinks)
                let link_target = std::fs::read_link(&src_path)?;
                let dest_path = if resolved_dest.ends_with('/') || Path::new(&resolved_dest).is_dir() {
                    PathBuf::from(&resolved_dest).join(src_path.file_name().unwrap_or_default())
                } else {
                    PathBuf::from(&resolved_dest)
                };
                if let Some(parent) = dest_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&link_target, &dest_path)?;
                }
                #[cfg(not(unix))]
                {
                    let real_src = if link_target.is_absolute() {
                        link_target
                    } else {
                        src_path.parent().unwrap_or(Path::new(".")).join(&link_target)
                    };
                    if real_src.exists() {
                        copy_file(&real_src, &dest_path, &self.chown, &self.chmod)?;
                    }
                }
                apply_permissions(&dest_path, &self.chown, &self.chmod)?;
            } else {
                let dest_path = if resolved_dest.ends_with('/') || Path::new(&resolved_dest).is_dir() {
                    PathBuf::from(&resolved_dest).join(src_path.file_name().unwrap_or_default())
                } else {
                    PathBuf::from(&resolved_dest)
                };
                if self.link {
                    copy_file_link(&src_path, &dest_path, &self.chown, &self.chmod)?;
                } else {
                    copy_file(&src_path, &dest_path, &self.chown, &self.chmod)?;
                }
            }
        }
        Ok(())
    }

    async fn copy_from_stage(&self, from_stage: &str, dest: &str) -> Result<()> {
        // Find the source stage
        let source_image = self.stages.get(from_stage)
            .ok_or_else(|| CommandError::Failed(format!("Stage '{}' not found for COPY --from", from_stage)))?;

        // Extract files from the source stage's layers
        for src in &self.sources {
            self.copy_file_from_image(source_image, src, dest)?;
        }
        Ok(())
    }

    fn copy_file_from_image(&self, image: &oci_image::mutate::MutableImage, src: &str, dest: &str) -> Result<()> {
        tracing::info!("Copying '{}' from stage to '{}'", src, dest);

        // Build a merged filesystem view from all layers in order.
        // Each layer may add, modify, or delete files (via whiteout entries).
        // We walk through all layers and collect the final state of files
        // matching the source pattern.
        let mut matched_files: Vec<(PathBuf, Vec<u8>)> = Vec::new();
        let src_path = PathBuf::from(src);

        for layer in &image.layers {
            let tar_data = layer.uncompressed_data()
                .map_err(|e| CommandError::Failed(format!("decompress layer: {}", e)))?;

            let mut archive = tar::Archive::new(tar_data.as_slice());
            let entries = archive.entries()
                .map_err(|e| CommandError::Failed(format!("read tar entries: {}", e)))?;

            for entry in entries {
                let mut entry = entry.map_err(|e| CommandError::Failed(format!("read tar entry: {}", e)))?;
                let path = entry.path().map_err(|e| CommandError::Failed(format!("get entry path: {}", e)))?;
                let path_str = path.to_string_lossy().to_string();
                let path_buf = path.to_path_buf();

                // Handle whiteout entries (file deletions)
                if let Some(whiteout_name) = path_str.strip_prefix(".wh.") {
                    // Remove the corresponding file from matched_files
                    let deleted_path = PathBuf::from(whiteout_name);
                    matched_files.retain(|(p, _)| p != &deleted_path);
                    continue;
                }
                // Handle opaque whiteout directories (.wh..wh..opq)
                if path_str.contains(".wh..wh..opq") {
                    let parent = path_buf.parent().unwrap_or(Path::new(""));
                    matched_files.retain(|(p, _)| !p.starts_with(parent));
                    continue;
                }

                // Check if this path matches our source pattern
                // Support exact match and glob-like prefix matching
                let matches = if src_path.is_absolute() {
                    path_buf.as_os_str() == src_path.as_os_str()
                        || path_buf.starts_with(&src_path)
                } else {
                    // Relative source: match against the end of the path
                    let file_name = path_buf.file_name().map(|f| f.to_string_lossy().to_string());
                    file_name.as_deref() == Some(src)
                        || path_str.ends_with(&format!("/{}", src))
                };

                if matches {
                    let mut data = Vec::new();
                    use std::io::Read;
                    entry.read_to_end(&mut data)
                        .map_err(|e| CommandError::Failed(format!("read entry data: {}", e)))?;
                    matched_files.push((path_buf, data));
                }
            }
        }

        if matched_files.is_empty() {
            return Err(CommandError::Failed(format!(
                "COPY --from: source '{}' not found in stage layers", src
            )));
        }

        // Write matched files to destination
        let dest_path = PathBuf::from(dest);
        for (file_path, data) in &matched_files {
            let target = if dest.ends_with('/') || dest_path.is_dir() {
                dest_path.join(file_path.file_name().unwrap_or_default())
            } else if matched_files.len() == 1 {
                dest_path.clone()
            } else {
                dest_path.join(file_path.file_name().unwrap_or_default())
            };

            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&target, data)?;
            apply_permissions(&target, &self.chown, &self.chmod)?;

            let mut files = self.snapshot_files.lock().unwrap();
            files.push(target);
        }

        Ok(())
    }
}

#[async_trait]
impl BaseCommand for CopyCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        let dest = resolve_destination(&self.destination, config);
        tracing::info!(
            "COPY {:?} {} (chown={:?}, chmod={:?}, link={})",
            self.sources, dest, self.chown, self.chmod, self.link
        );

        if let Some(ref from_stage) = self.from {
            // COPY --from=stage support
            self.copy_from_stage(from_stage, &dest).await?;
        } else {
            // Regular COPY from build context
            self.copy_from_context(&dest).await?;
        }

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref f) = self.from {
            parts.push(format!("--from={}", f));
        }
        if let Some(ref c) = self.chown {
            parts.push(format!("--chown={}", c));
        }
        if let Some(ref c) = self.chmod {
            parts.push(format!("--chmod={}", c));
        }
        if self.link {
            parts.push("--link".to_string());
        }
        parts.extend(self.sources.iter().cloned());
        parts.push(self.destination.clone());
        format!("COPY {}", parts.join(" "))
    }

    fn metadata_only_impl(&self) -> bool {
        false
    }

    fn requires_unpacked_fs_impl(&self) -> bool {
        false
    }

    fn should_cache_output_impl(&self) -> bool {
        self.should_cache
    }

    fn provides_files_to_snapshot_impl(&self) -> bool {
        true
    }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.snapshot_files.lock().unwrap();
        if files.is_empty() { None } else { Some(files.clone()) }
    }
}

fn resolve_destination(dest: &str, config: &ContainerConfig) -> String {
    if dest.starts_with('/') {
        dest.to_string()
    } else {
        let cwd = config.working_dir.as_deref().unwrap_or("/");
        format!("{}/{}", cwd.trim_end_matches('/'), dest)
    }
}

/// Resolve destination path if it (or its parent dirs) contains symlinks.
/// Analogous to Go: `commands.resolveIfSymlink`.
fn resolve_if_symlink(dest: &str) -> Result<String> {
    if !dest.starts_with('/') {
        return Ok(dest.to_string());
    }

    let mut current = PathBuf::from("/");
    let parts: Vec<&str> = dest.trim_start_matches('/').split('/').collect();

    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        current = current.join(part);

        if current.is_symlink() {
            let link_target = std::fs::read_link(&current)?;
            if link_target.is_absolute() {
                // Replace current with the resolved target + remaining parts
                let remaining = parts[i + 1..].join("/");
                current = link_target;
                if !remaining.is_empty() {
                    current = current.join(remaining);
                }
                // Recurse to handle chained symlinks
                let resolved = resolve_if_symlink(&current.to_string_lossy())?;
                return Ok(resolved);
            } else {
                // Relative symlink: resolve relative to parent
                let parent = current.parent().unwrap_or(Path::new("/"));
                current = parent.join(&link_target);
            }
        }
    }

    Ok(current.to_string_lossy().to_string())
}

/// Apply --chown and --chmod permissions to a file or directory.
/// Apply --chown and --chmod permissions to a file or directory. (Public for reuse by AddCommand)
pub fn apply_permissions_pub(path: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    apply_permissions(path, chown, chmod)
}

fn apply_permissions(path: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    if let Some(chmod_val) = chmod {
        let mode = u32::from_str_radix(chmod_val, 8).map_err(|e| {
            CommandError::Failed(format!("Invalid chmod value '{}': {}", chmod_val, e))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            tracing::warn!("--chmod is not supported on non-Unix platforms");
        }
    }

    if let Some(chown_val) = chown {
        // Parse user:group format
        let parts: Vec<&str> = chown_val.split(':').collect();
        let uid: u32 = parts[0].parse().unwrap_or(0);
        let gid: u32 = if parts.len() > 1 {
            parts[1].parse().unwrap_or(0)
        } else {
            uid
        };

        #[cfg(unix)]
        {
            // Only change ownership if we're running as root
            // In container builds, we typically run as root
            if unsafe { libc::geteuid() } == 0 {
                let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
                let ret = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
                if ret != 0 {
                    tracing::warn!(
                        "Failed to chown {} to {}:{}: {}",
                        path.display(), uid, gid,
                        std::io::Error::last_os_error()
                    );
                    // Non-fatal: continue build even if chown fails
                }
            } else {
                tracing::debug!(
                    "Skipping chown for {} (not running as root)",
                    path.display()
                );
            }
        }
        #[cfg(not(unix))]
        {
            tracing::warn!("--chown is not supported on non-Unix platforms (uid={}, gid={})", uid, gid);
            let _ = (uid, gid); // suppress unused warnings
        }
    }

    Ok(())
}

fn copy_file(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dest)?;
    apply_permissions(dest, chown, chmod)?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    let dest = dest.join(src.file_name().unwrap_or_default());
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            apply_permissions(&target, chown, chmod)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            apply_permissions(&target, chown, chmod)?;
        }
    }
    Ok(())
}

/// Copy a file using a hard link (--link mode).
/// Falls back to regular copy if hard link fails (e.g., cross-device).
fn copy_file_link(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Try hard link first
    #[cfg(unix)]
    {
        match std::fs::hard_link(src, dest) {
            Ok(()) => {
                tracing::debug!("Hard linked {} -> {}", src.display(), dest.display());
                // Note: --chown may not work on hard links (same inode)
                // but --chmod will affect the link independently on some systems
                apply_permissions(dest, chown, chmod)?;
                return Ok(());
            }
            Err(e) => {
                tracing::debug!("Hard link failed ({}), falling back to copy: {} -> {}", e, src.display(), dest.display());
            }
        }
    }
    // Fallback to copy
    std::fs::copy(src, dest)?;
    apply_permissions(dest, chown, chmod)?;
    Ok(())
}

/// Copy a directory recursively using hard links (--link mode).
fn copy_dir_recursive_link(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    let dest = dest.join(src.file_name().unwrap_or_default());
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            apply_permissions(&target, chown, chmod)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Try hard link, fallback to copy
            #[cfg(unix)]
            {
                match std::fs::hard_link(entry.path(), &target) {
                    Ok(()) => {
                        apply_permissions(&target, chown, chmod)?;
                        continue;
                    }
                    Err(_) => {
                        tracing::debug!("Hard link failed for {}, falling back to copy", target.display());
                    }
                }
            }
            std::fs::copy(entry.path(), &target)?;
            apply_permissions(&target, chown, chmod)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_image::config::ContainerConfig;
    use std::fs;

    #[tokio::test]
    async fn test_copy_command_string() {
        let context_dir = PathBuf::from("/tmp");
        let command = CopyCommand::new(
            vec!["src".to_string()],
            "/dest".to_string(),
            None,
            context_dir,
            true,
        );
        
        assert_eq!(command.command_string_impl(), "COPY src /dest");
    }

    #[tokio::test]
    async fn test_copy_command_with_from() {
        let context_dir = PathBuf::from("/tmp");
        let command = CopyCommand::new(
            vec!["app".to_string()],
            "/app".to_string(),
            Some("builder".to_string()),
            context_dir,
            true,
        );
        
        assert_eq!(command.command_string_impl(), "COPY --from=builder app /app");
    }

    #[tokio::test]
    async fn test_copy_command_with_all_flags() {
        let context_dir = PathBuf::from("/tmp");
        let command = CopyCommand::with_flags(
            vec!["app".to_string()],
            "/app".to_string(),
            Some("builder".to_string()),
            Some("1000:1000".to_string()),
            Some("755".to_string()),
            true,
            context_dir,
            true,
        );
        
        assert_eq!(
            command.command_string_impl(),
            "COPY --from=builder --chown=1000:1000 --chmod=755 --link app /app"
        );
    }

    #[tokio::test]
    async fn test_copy_command_multiple_sources() {
        let context_dir = PathBuf::from("/tmp");
        let command = CopyCommand::new(
            vec!["file1".to_string(), "file2".to_string()],
            "/dest/".to_string(),
            None,
            context_dir,
            true,
        );
        
        assert_eq!(command.command_string_impl(), "COPY file1 file2 /dest/");
    }

    #[tokio::test]
    async fn test_resolve_destination_absolute() {
        let config = ContainerConfig::default();
        let dest = resolve_destination("/absolute/path", &config);
        assert_eq!(dest, "/absolute/path");
    }

    #[tokio::test]
    async fn test_resolve_destination_relative() {
        let mut config = ContainerConfig::default();
        config.working_dir = Some("/app".to_string());
        let dest = resolve_destination("relative/path", &config);
        assert_eq!(dest, "/app/relative/path");
    }

    #[tokio::test]
    async fn test_resolve_destination_default_workdir() {
        let config = ContainerConfig::default();
        let dest = resolve_destination("file.txt", &config);
        assert_eq!(dest, "/file.txt");
    }

    #[tokio::test]
    async fn test_copy_command_properties() {
        let context_dir = PathBuf::from("/tmp");
        let command = CopyCommand::new(
            vec!["src".to_string()],
            "/dest".to_string(),
            None,
            context_dir,
            true,
        );
        
        assert!(!command.metadata_only_impl());
        assert!(!command.requires_unpacked_fs_impl());
        assert!(command.should_cache_output_impl());
        assert!(command.provides_files_to_snapshot_impl());
        assert!(command.files_to_snapshot_impl().is_none()); // Empty initially
    }

    #[test]
    fn test_apply_permissions_chmod() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("test.txt");
        fs::write(&file_path, "test").unwrap();

        // Test chmod
        let result = apply_permissions(&file_path, &None, &Some("755".to_string()));
        assert!(result.is_ok());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = fs::metadata(&file_path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
    }

    #[test]
    fn test_apply_permissions_invalid_chmod() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("test.txt");
        fs::write(&file_path, "test").unwrap();

        let result = apply_permissions(&file_path, &None, &Some("999".to_string()));
        // 999 is not valid octal (9 is not an octal digit)
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_permissions_chmod_644() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let file_path = tmp_dir.path().join("test.txt");
        fs::write(&file_path, "test").unwrap();

        let result = apply_permissions(&file_path, &None, &Some("644".to_string()));
        assert!(result.is_ok());

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let mode = fs::metadata(&file_path).unwrap().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }
    }
}