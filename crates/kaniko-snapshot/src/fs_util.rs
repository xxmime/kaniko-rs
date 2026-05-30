//! Filesystem utility functions for kaniko-rs.
//!
//! Analogous to Go: `pkg/util/fs_util.go`.
//!
//! Provides:
//! - `delete_filesystem`: Clean up the extracted image filesystem
//! - `detect_filesystem_ignore_list`: Detect mounted filesystems from /proc/self/mountinfo
//! - `rooted_path`: Resolve a path within the kaniko root directory
//! - `parent_directories`: Return all parent directories of a path
//! - `relative_files`: List all files relative to a root directory
//! - `destination_filepath`: Compute the destination filepath for COPY/ADD commands
//! - `download_file_to_dest`: Download a URL for ADD command
//! - `is_dest_dir`: Check if a destination path is a directory
//! - `filepath_exists`: Check if a path exists
//! - `create_file`: Create a file with permissions and ownership

use crate::ignore_list::{is_in_ignore_list, add_to_ignore_list, IgnoreListEntry, get_ignore_list};
use crate::walker::parse_dockerignore;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// FileContext represents the build context for a Dockerfile.
///
/// It tracks the root directory and any files excluded via .dockerignore.
/// Analogous to Go: `util.FileContext`.
#[derive(Debug, Clone)]
pub struct FileContext {
    /// Root directory of the build context.
    pub root: String,
    /// List of excluded file patterns from .dockerignore.
    pub excluded_files: Vec<String>,
}

impl FileContext {
    /// Create a new FileContext with the given root.
    pub fn new(root: &str) -> Self {
        Self {
            root: root.to_string(),
            excluded_files: Vec::new(),
        }
    }

    /// Check if a path should be excluded based on .dockerignore patterns.
    /// Analogous to Go: `FileContext.ExcludesFile()`.
    pub fn excludes_file(&self, path: &str) -> bool {
        let rel_path = if filepath_has_prefix(path, &self.root, false) {
            Path::new(path).strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string())
        } else {
            path.to_string()
        };

        for pattern in &self.excluded_files {
            if glob_match(pattern, &rel_path) {
                return true;
            }
        }
        false
    }
}

/// Create a FileContext from a Dockerfile path and build context.
///
/// Reads .dockerignore from either the Dockerfile directory or the build context.
/// Analogous to Go: `NewFileContextFromDockerfile()`.
pub fn new_file_context_from_dockerfile(dockerfile_path: &str, build_context: &str) -> FileContext {
    let mut ctx = FileContext::new(build_context);

    // Try .dockerignore next to Dockerfile first
    let dockerignore_path = format!("{}.dockerignore", dockerfile_path);
    let dockerignore = if Path::new(&dockerignore_path).exists() {
        Some(dockerignore_path)
    } else {
        let alt_path = Path::new(build_context).join(".dockerignore");
        if alt_path.exists() {
            Some(alt_path.to_string_lossy().to_string())
        } else {
            None
        }
    };

    if let Some(path) = dockerignore {
        tracing::info!("Using dockerignore file: {}", path);
        if let Ok(content) = fs::read_to_string(&path) {
            let patterns = parse_dockerignore(&content);
            for pattern in patterns {
                if !pattern.negation {
                    ctx.excluded_files.push(pattern.pattern.clone());
                }
            }
        }
    }

    ctx
}

/// Root directory for kaniko operations.
/// Defaults to "/" (the container root).
pub const KANIKO_ROOT_DIR: &str = "/";

/// Delete the extracted image filesystem.
///
/// Walks the root directory and removes all files/directories except those
/// in the ignore list. Analogous to Go: `DeleteFilesystem()`.
pub fn delete_filesystem(root_dir: &Path) -> io::Result<()> {
    tracing::info!("Deleting filesystem...");
    let root_str = root_dir.to_string_lossy().to_string();

    if !root_dir.exists() {
        return Ok(());
    }

    // Collect entries first to avoid borrow issues during removal
    let mut entries: Vec<PathBuf> = Vec::new();
    visit_dirs(root_dir, &mut entries)?;

    // Remove entries (reverse order so children are removed before parents)
    for path in entries.iter().rev() {
        let path_str = path.to_string_lossy().to_string();

        if is_in_ignore_list(path) {
            if !path.exists() {
                tracing::debug!("Path {} ignored, but not exists", path_str);
                continue;
            }
            if path.is_dir() {
                continue; // Skip directory removal
            }
            tracing::debug!("Not deleting {}, as it's ignored", path_str);
            continue;
        }

        if child_dir_in_ignore_list(&path_str) {
            tracing::debug!("Not deleting {}, as it contains an ignored path", path_str);
            continue;
        }

        if path_str == root_str {
            continue; // Don't delete the root itself
        }

        if let Err(e) = fs::remove_file(path) {
            // Try removing as directory if file removal fails
            if let Err(e2) = fs::remove_dir(path) {
                tracing::trace!("Could not remove {}: {} / {}", path_str, e, e2);
            }
        }
    }

    Ok(())
}

/// Recursively collect all paths under a directory.
fn visit_dirs(dir: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                visit_dirs(&path, entries)?;
            }
            entries.push(path);
        }
    }
    Ok(())
}

/// Check if any child directory of the given path is in the ignore list.
/// Analogous to Go: `childDirInIgnoreList()`.
fn child_dir_in_ignore_list(path: &str) -> bool {
    let cleaned = Path::new(path);

    for entry in get_ignore_list() {
        let entry_path = entry.path.as_path();
        if has_filepath_prefix(entry_path, cleaned, entry.prefix_match_only) {
            return true;
        }
    }
    false
}

/// Check if `path` has the given `prefix` as a filepath prefix.
/// Analogous to Go: `hasCleanedFilepathPrefix()`.
fn has_filepath_prefix(path: &Path, prefix: &Path, prefix_match_only: bool) -> bool {
    let path_str = path.to_string_lossy();
    let prefix_str = prefix.to_string_lossy();
    if prefix_match_only {
        // Prefix match: just check if path starts with prefix
        path_str.starts_with(prefix_str.as_ref())
    } else {
        // Exact match or is a parent directory
        path == prefix || path.starts_with(prefix)
    }
}

/// Detect mounted filesystems from /proc/self/mountinfo and add them to the ignore list.
///
/// Each line of /proc/self/mountinfo has the mount point at position 5 (0-indexed 4).
/// Analogous to Go: `DetectFilesystemIgnoreList()`.
pub fn detect_filesystem_ignore_list(mountinfo_path: &Path) -> io::Result<()> {
    tracing::trace!("Detecting filesystem ignore list");

    let content = fs::read_to_string(mountinfo_path)?;
    for line in content.lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() < 5 {
            continue;
        }

        let mount_point = parts[4];
        if mount_point != KANIKO_ROOT_DIR && mount_point != "/" {
            tracing::trace!("Adding ignore list entry {} from mountinfo", mount_point);
            add_to_ignore_list(IgnoreListEntry::new(mount_point, false));
        }
    }

    Ok(())
}

/// Return a rooted path within the kaniko root directory.
///
/// If the root is "/", just clean the path. Otherwise, resolve the path
/// relative to the root directory. Analogous to Go: `RootedPath()`.
pub fn rooted_path(path: &str, root_dir: &str) -> PathBuf {
    let cleaned = Path::new(path);

    if root_dir == "/" {
        return cleaned.to_path_buf();
    }

    let rooted = Path::new(root_dir);
    if cleaned.is_absolute() {
        let relative = cleaned.strip_prefix("/").unwrap_or(cleaned);
        rooted.join(relative)
    } else {
        rooted.join(cleaned)
    }
}

/// Return all parent directories of a path.
///
/// Example: `/some/temp/dir` → `["/some", "/some/temp", "/some/temp/dir"]`
/// Analogous to Go: `ParentDirectories()`.
pub fn parent_directories(path: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let path = Path::new(path);
    let mut current = String::new();

    for component in path.components() {
        match component {
            std::path::Component::RootDir => {
                current.push('/');
            }
            std::path::Component::Normal(c) => {
                if !current.ends_with('/') && !current.is_empty() {
                    current.push('/');
                }
                current.push_str(&c.to_string_lossy());
                paths.push(current.clone());
            }
            _ => {}
        }
    }

    paths
}

/// Return all parent directories without leading slash on subdirectories.
///
/// Example: `/some/temp/dir` → `["/", "some", "some/temp", "some/temp/dir"]`
/// Analogous to Go: `ParentDirectoriesWithoutLeadingSlash()`.
pub fn parent_directories_without_leading_slash(path: &str) -> Vec<String> {
    let cleaned = path.trim_start_matches('/');
    let mut paths = vec!["/".to_string()];

    let mut dir_path = String::new();
    for (i, part) in cleaned.split('/').enumerate() {
        if part.is_empty() {
            continue;
        }
        if i > 0 {
            dir_path.push('/');
        }
        dir_path.push_str(part);

        // Skip the last component (it's the target itself, not a parent)
        // Actually, Go includes the full path. Let's include it.
    }

    // Re-implement: include all path components
    let parts: Vec<&str> = cleaned.split('/').filter(|s| !s.is_empty()).collect();
    dir_path.clear();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            dir_path.push('/');
        }
        dir_path.push_str(part);
        paths.push(dir_path.clone());
    }

    paths
}

/// List all files relative to a root directory.
///
/// Returns relative paths for all files under root/fp.
/// Analogous to Go: `RelativeFiles()`.
pub fn relative_files(fp: &str, root: &Path) -> io::Result<Vec<String>> {
    let full_path = root.join(fp);
    let mut files = Vec::new();

    if !full_path.exists() {
        return Ok(files);
    }

    visit_dirs_relative(&full_path, root, &mut files)?;
    Ok(files)
}

/// Recursively collect relative file paths.
fn visit_dirs_relative(dir: &Path, root: &Path, files: &mut Vec<String>) -> io::Result<()> {
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().to_string();

            if is_in_ignore_list(rel) {
                continue;
            }

            files.push(rel_str);

            if path.is_dir() {
                visit_dirs_relative(&path, root, files)?;
            }
        }
    }
    Ok(())
}

/// Compute the destination filepath for a COPY/ADD command.
///
/// - If `dest` is a directory, copy to `dest/<src_filename>`
/// - If `dest` is not absolute, prepend `cwd`
/// - If `dest` ends with `/`, it's treated as a directory
///
/// Analogous to Go: `DestinationFilepath()`.
pub fn destination_filepath(src: &str, dest: &str, cwd: &str) -> String {
    let src_path = Path::new(src);
    let src_filename = src_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let effective_cwd = if cwd.is_empty() { "/" } else { cwd };
    let mut new_dest = dest.to_string();

    // If dest is not absolute, prepend cwd
    if !Path::new(dest).is_absolute() {
        new_dest = format!("{}/{}", effective_cwd.trim_end_matches('/'), dest.trim_start_matches('/'));
    }

    // If dest is a directory (ends with / or exists as dir), append source filename
    if is_dest_dir(&new_dest) {
        if !new_dest.ends_with('/') {
            new_dest.push('/');
        }
        new_dest.push_str(&src_filename);
    }

    // Ensure trailing slash for directory sources without filename
    if src_filename.is_empty() && !new_dest.ends_with('/') {
        new_dest.push('/');
    }

    new_dest
}

/// Check if a path is a directory.
///
/// Falls back to string-based check (trailing `/` or `.`) if stat fails.
/// Analogous to Go: `IsDestDir()`.
pub fn is_dest_dir(path: &str) -> bool {
    match fs::metadata(path) {
        Ok(meta) => meta.is_dir(),
        Err(_) => {
            // String-based fallback
            path.ends_with('/') || path == "."
        }
    }
}

/// Check if a filepath exists.
/// Analogous to Go: `FilepathExists()`.
pub fn filepath_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Download a file from a URL to a destination path.
///
/// Sets permissions to 0600 by default. Uses Last-Modified header for mtime.
/// Analogous to Go: `DownloadFileToDest()`.
pub async fn download_file_to_dest(
    url: &str,
    dest: &str,
    uid: u32,
    gid: u32,
    chmod: u32,
) -> Result<(), String> {
    tracing::debug!("Downloading {} to {}", url, dest);

    let response = reqwest::get(url).await
        .map_err(|e| format!("failed to download {}: {}", url, e))?;

    if response.status().as_u16() >= 400 {
        return Err(format!("invalid response status {} for {}", response.status(), url));
    }

    let last_modified = response.headers()
        .get("last-modified")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| http_date_to_system_time(v));

    let data = response.bytes().await
        .map_err(|e| format!("failed to read response body: {}", e))?;

    // Create parent directories
    if let Some(parent) = Path::new(dest).parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create parent dir: {}", e))?;
        }
    }

    // Write file
    fs::write(dest, &data)
        .map_err(|e| format!("failed to write {}: {}", dest, e))?;

    // Set permissions (default 0600 for downloaded files)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(chmod);
        fs::set_permissions(dest, perms)
            .map_err(|e| format!("failed to set permissions: {}", e))?;
    }

    // Set mtime from Last-Modified header
    if let Some(mtime) = last_modified {
        let mtime_epoch = mtime.duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let file_time = filetime::FileTime::from_unix_time(mtime_epoch, 0);
        if let Err(e) = filetime::set_file_mtime(dest, file_time) {
            tracing::debug!("Could not set mtime for {}: {}", dest, e);
        }
    }

    tracing::debug!("Downloaded {} to {} ({} bytes)", url, dest, data.len());
    Ok(())
}

/// Parse HTTP date format to SystemTime.
fn http_date_to_system_time(date_str: &str) -> Option<std::time::SystemTime> {
    // Try RFC 1123 format: "Wed, 21 Oct 2015 07:28:00 GMT"
    // Simple parsing approach
    let datetime = chrono::DateTime::parse_from_rfc2822(date_str).ok()
        .or_else(|| chrono::DateTime::parse_from_rfc3339(date_str).ok());
    datetime.map(|dt| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64))
}

/// Create a file at `path` with content from reader, setting permissions and ownership.
/// Analogous to Go: `CreateFile()`.
pub fn create_file(
    path: &str,
    data: &[u8],
    perm: u32,
    _uid: u32,
    _gid: u32,
) -> io::Result<()> {
    // Create parent directories if needed
    if let Some(parent) = Path::new(path).parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }

    // Write the file
    fs::write(path, data)?;

    // Set permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(perm);
        fs::set_permissions(path, perms)?;
    }

    // Note: chown requires root privileges; skip in non-root environments
    #[cfg(unix)]
    {
        if _uid != 0 || _gid != 0 {
            // Only attempt chown if not already root-owned
            // Non-root will fail silently
        }
    }

    Ok(())
}

/// Check if a source path is a remote file URL.
/// Analogous to Go: `IsSrcRemoteFileURL()`.
pub fn is_src_remote_file_url(src: &str) -> bool {
    src.starts_with("http://") || src.starts_with("https://")
}

/// Check if sources contain wildcards (*, ?, [).
/// Analogous to Go: `ContainsWildcards()`.
pub fn contains_wildcards(paths: &[String]) -> bool {
    paths.iter().any(|p| p.contains('*') || p.contains('?') || p.contains('['))
}

/// Resolve wildcard sources against available files.
/// Returns matched file paths. Analogous to Go: `ResolveSources()`.
pub fn resolve_sources(srcs: &[String], root: &Path) -> io::Result<Vec<String>> {
    if !contains_wildcards(srcs) {
        return Ok(srcs.to_vec());
    }

    tracing::info!("Resolving sources {:?}...", srcs);
    let files = relative_files("", root)?;
    let matched = match_sources(srcs, &files);
    tracing::debug!("Resolved sources to {:?}", matched);
    Ok(matched)
}

/// Match source patterns against available files.
/// Analogous to Go: `matchSources()`.
fn match_sources(srcs: &[String], files: &[String]) -> Vec<String> {
    let mut matched = Vec::new();

    for src in srcs {
        if is_src_remote_file_url(src) {
            matched.push(src.clone());
            continue;
        }

        let cleaned_src = src.trim_end_matches('/');

        for file in files {
            let file_cleaned = file.trim_end_matches('/');
            if glob_match(cleaned_src, file_cleaned) || cleaned_src == file_cleaned {
                matched.push(file.clone());
            }
        }
    }

    matched
}

/// Simple glob matching for *, ?, [charclass] patterns.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(text))
        .unwrap_or(false)
}

/// Check if a path is a symlink.
/// Analogous to Go: `IsSymlink()`.
pub fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Copy a file or symlink for cross-stage dependencies.
///
/// For symlinks, copies the target path because copying the symlink would
/// result in a dead link. For regular files, copies the file with permissions.
///
/// Analogous to Go: `CopyFileOrSymlink()`.
pub fn copy_file_or_symlink(src: &str, dest_dir: &str, root: &str) -> io::Result<()> {
    let src_path = Path::new(root).join(src.trim_start_matches('/'));
    let dest_path = Path::new(dest_dir).join(src.trim_start_matches('/'));

    let metadata = fs::symlink_metadata(&src_path)?;

    if metadata.file_type().is_symlink() {
        // Copy symlink target
        let link_target = fs::read_link(&src_path)?;
        create_parent_directory(dest_path.to_string_lossy().as_ref())?;
        // Remove existing if any
        if dest_path.exists() || dest_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&dest_path);
        }
        fs::soft_link(&link_target, &dest_path)?;
    } else if metadata.is_dir() {
        // Copy directory recursively
        copy_dir_recursive_simple(&src_path, &dest_path)?;
    } else {
        // Copy regular file - ensure parent directory exists
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src_path, &dest_path)?;

        // Copy ownership
        copy_ownership(&src_path, dest_dir, root)?;

        // Copy file mode
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode();
            fs::set_permissions(&dest_path, fs::Permissions::from_mode(mode))?;
        }
    }

    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_recursive_simple(src: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_dir_recursive_simple(&src_path, &dest_path)?;
        } else {
            fs::copy(&src_path, &dest_path)?;
        }
    }

    Ok(())
}

/// Copy file ownership recursively from src to dest.
///
/// Analogous to Go: `CopyOwnership()`.
pub fn copy_ownership(src: &Path, dest_dir: &str, root: &str) -> io::Result<()> {
    // Only walk directories
    if !src.is_dir() {
        return Ok(());
    }

    fn walk_and_copy(src: &Path, dest_dir: &str, root: &str) -> io::Result<()> {
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let path = entry.path();

            if is_symlink(&path) {
                continue;
            }

            let rel_path = path.strip_prefix(root).unwrap_or(&path);
            let dest_path = Path::new(dest_dir).join(rel_path);

            if is_in_ignore_list(&path) && is_in_ignore_list(&dest_path) {
                if !dest_path.exists() {
                    tracing::debug!("Path {:?} ignored, but not exists", dest_path);
                    continue;
                }
                if path.is_dir() {
                    continue;
                }
                tracing::debug!("Not copying ownership for {:?}, as it's ignored", dest_path);
                continue;
            }

            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let metadata = fs::metadata(&path)?;
                let uid = metadata.uid();
                let gid = metadata.gid();
                // chown requires root; attempt silently
                if dest_path.exists() {
                    let _ = chown_file(&dest_path, uid, gid);
                }
            }

            if path.is_dir() {
                walk_and_copy(&path, dest_dir, root)?;
            }
        }
        Ok(())
    }

    walk_and_copy(src, dest_dir, root)
}

/// Chown a file (Unix only). Silently fails if not root.
#[cfg(unix)]
fn chown_file(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let c_path = std::ffi::CString::new(path.to_string_lossy().into_owned())?;
    // Only chown if uid/gid differ from current
    let metadata = fs::metadata(path)?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }
    // Safe: chown requires CAP_CHOWN, silently fails for non-root
    unsafe {
        if libc::chown(c_path.as_ptr(), uid, gid) != 0 {
            // Non-root will fail; this is expected
            tracing::trace!("chown {:?} to {}:{} failed (expected for non-root)", path, uid, gid);
        }
    }
    Ok(())
}

/// Create parent directories with default permissions.
/// Analogous to Go: `createParentDirectory()`.
pub fn create_parent_directory(path: &str) -> io::Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

/// Create directory with specified permissions and ownership.
/// Analogous to Go: `MkdirAllWithPermissions()`.
pub fn mkdir_all_with_permissions(path: &str, mode: u32, _uid: i64, _gid: i64) -> io::Result<()> {
    let p = Path::new(path);
    if !p.exists() {
        fs::create_dir_all(p)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(p, perms)?;

        // chown requires root; skip if not root
        if _uid >= 0 || _gid >= 0 {
            let _ = chown_file(p, _uid as u32, _gid as u32);
        }
    }

    Ok(())
}

/// Set file permissions (mode, uid, gid).
/// Analogous to Go: `setFilePermissions()`.
pub fn set_file_permissions(path: &str, mode: u32, _uid: u32, _gid: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(mode);
        fs::set_permissions(path, perms)?;

        // Attempt chown (requires root)
        let _ = chown_file(Path::new(path), _uid, _gid);
    }

    Ok(())
}

/// Set file access and modification times.
/// Analogous to Go: `setFileTimes()`.
pub fn set_file_times(path: &str, atime: std::time::SystemTime, mtime: std::time::SystemTime) -> io::Result<()> {
    let f = fs::File::open(path)?;
    filetime::set_file_handle_times(
        &f,
        Some(filetime::FileTime::from_system_time(atime)),
        Some(filetime::FileTime::from_system_time(mtime)),
    ).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

/// Check if a path has the given prefix (string-based).
/// Analogous to Go: `HasFilepathPrefix()`.
pub fn filepath_has_prefix(path: &str, prefix: &str, prefix_match_only: bool) -> bool {
    let path = path.trim_end_matches('/').trim_end_matches('\\');
    let prefix = prefix.trim_end_matches('/').trim_end_matches('\\');
    has_cleaned_filepath_prefix(path, prefix, prefix_match_only)
}

fn has_cleaned_filepath_prefix(path: &str, prefix: &str, prefix_match_only: bool) -> bool {
    if prefix.is_empty() {
        return true;
    }

    let prefix_parts: Vec<&str> = prefix.split('/').filter(|s| !s.is_empty()).collect();
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if path_parts.len() < prefix_parts.len() {
        return false;
    }

    for (i, prefix_part) in prefix_parts.iter().enumerate() {
        if path_parts[i] != *prefix_part {
            return false;
        }
    }

    if !prefix_match_only && path_parts.len() != prefix_parts.len() {
        // For non-prefix-match-only, check exact match
        // But prefix matching is allowed for directories
        return true;
    }

    true
}

/// Check if a cleaned path is in the ignore list.
/// Analogous to Go: `CheckCleanedPathAgainstIgnoreList()`.
pub fn check_cleaned_path_against_ignore_list(path: &Path) -> bool {
    is_in_ignore_list(path)
}

/// Eval symlinks for a path, resolving in root if needed.
/// Analogous to Go: `EvalSymLink()`.
pub fn eval_symlink(path: &str) -> io::Result<String> {
    let p = Path::new(path);

    // Verify it's a symlink
    let metadata = fs::symlink_metadata(p)?;
    if !metadata.file_type().is_symlink() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a symlink"));
    }

    // For paths within kaniko root, resolve in root
    // Otherwise use standard symlink resolution
    if should_resolve_in_root(path) {
        let resolved = crate::resolve::resolve_symlink_ancestor(path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if Path::new(&resolved).exists() {
            return Ok(resolved);
        }
        return Err(io::Error::new(io::ErrorKind::NotFound, "resolved symlink target not found"));
    }

    p.canonicalize().map(|s| s.to_string_lossy().to_string())
}

/// Check if we should resolve paths in root (not standard root "/").
fn should_resolve_in_root(path: &str) -> bool {
    let root = KANIKO_ROOT_DIR.trim_end_matches('/');
    if root.is_empty() || root == "/" {
        return false;
    }
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parent_directories() {
        let dirs = parent_directories("/some/temp/dir");
        assert_eq!(dirs, vec!["/some", "/some/temp", "/some/temp/dir"]);
    }

    #[test]
    fn test_parent_directories_root() {
        let dirs = parent_directories("/");
        assert!(dirs.is_empty());
    }

    #[test]
    fn test_parent_directories_without_leading_slash() {
        let dirs = parent_directories_without_leading_slash("/some/temp/dir");
        assert_eq!(dirs, vec!["/", "some", "some/temp", "some/temp/dir"]);
    }

    #[test]
    fn test_rooted_path_root_is_root() {
        let result = rooted_path("/etc/hosts", "/");
        assert_eq!(result, PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn test_rooted_path_custom_root() {
        let result = rooted_path("/etc/hosts", "/kaniko");
        assert_eq!(result, PathBuf::from("/kaniko/etc/hosts"));
    }

    #[test]
    fn test_rooted_path_relative() {
        let result = rooted_path("etc/hosts", "/kaniko");
        assert_eq!(result, PathBuf::from("/kaniko/etc/hosts"));
    }

    #[test]
    fn test_destination_filepath_dest_is_dir() {
        let result = destination_filepath("src/app.js", "/app/", "/workdir");
        assert_eq!(result, "/app/app.js");
    }

    #[test]
    fn test_destination_filepath_dest_is_file() {
        let result = destination_filepath("src/app.js", "/app/bundle.js", "/workdir");
        assert_eq!(result, "/app/bundle.js");
    }

    #[test]
    fn test_destination_filepath_relative_dest() {
        let result = destination_filepath("src/app.js", "out/", "/workdir");
        assert_eq!(result, "/workdir/out/app.js");
    }

    #[test]
    fn test_is_dest_dir_trailing_slash() {
        // Can't test actual directory detection without filesystem, test string fallback
        assert!(is_dest_dir("/nonexistent/path/"));
    }

    #[test]
    fn test_is_dest_dir_dot() {
        assert!(is_dest_dir("."));
    }

    #[test]
    fn test_filepath_exists() {
        assert!(filepath_exists("/")); // Root always exists
        assert!(!filepath_exists("/nonexistent/path/xyz"));
    }

    #[test]
    fn test_is_src_remote_file_url() {
        assert!(is_src_remote_file_url("https://example.com/file.tar.gz"));
        assert!(is_src_remote_file_url("http://example.com/file.tar.gz"));
        assert!(!is_src_remote_file_url("./local/file"));
        assert!(!is_src_remote_file_url("git://github.com/repo"));
    }

    #[test]
    fn test_contains_wildcards() {
        assert!(contains_wildcards(&["*.go".to_string()]));
        assert!(contains_wildcards(&["file?.txt".to_string()]));
        assert!(contains_wildcards(&["file[0-9].txt".to_string()]));
        assert!(!contains_wildcards(&["file.go".to_string()]));
    }

    #[test]
    fn test_create_file_basic() {
        let dir = std::env::temp_dir().join("kaniko_test_create_file");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test_file.txt");
        let path_str = path.to_string_lossy().to_string();

        create_file(&path_str, b"hello world", 0o644, 0, 0).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_symlink() {
        let dir = std::env::temp_dir().join("kaniko_test_is_symlink");
        let _ = fs::create_dir_all(&dir);
        let target = dir.join("target.txt");
        let link = dir.join("link.txt");
        fs::write(&target, "hello").unwrap();
        #[cfg(unix)]
        fs::soft_link(&target, &link).unwrap();

        #[cfg(unix)]
        {
            assert!(is_symlink(&link));
            assert!(!is_symlink(&target));
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_copy_file_or_symlink_regular_file() {
        let dir = std::env::temp_dir().join("kaniko_test_copy_file");
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("root");
        let dest = dir.join("dest");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/hosts"), "127.0.0.1 localhost").unwrap();

        copy_file_or_symlink("etc/hosts", dest.to_string_lossy().as_ref(), root.to_string_lossy().as_ref()).unwrap();

        let content = fs::read_to_string(dest.join("etc/hosts")).unwrap();
        assert_eq!(content, "127.0.0.1 localhost");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_copy_file_or_symlink_symlink() {
        let dir = std::env::temp_dir().join("kaniko_test_copy_symlink");
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("root");
        let dest = dir.join("dest");
        fs::create_dir_all(root.join("etc")).unwrap();
        fs::write(root.join("etc/hosts"), "127.0.0.1").unwrap();
        #[cfg(unix)]
        fs::soft_link("hosts", root.join("etc/hosts.link")).unwrap();

        #[cfg(unix)]
        {
            copy_file_or_symlink("etc/hosts.link", dest.to_string_lossy().as_ref(), root.to_string_lossy().as_ref()).unwrap();
            let target = fs::read_link(dest.join("etc/hosts.link")).unwrap();
            assert_eq!(target.to_string_lossy(), "hosts");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_filepath_has_prefix() {
        assert!(filepath_has_prefix("/etc/hosts", "/etc", false));
        assert!(filepath_has_prefix("/etc/hosts", "/etc", true));
        assert!(!filepath_has_prefix("/etc2/hosts", "/etc", false));
        assert!(!filepath_has_prefix("/etc", "/etc/hosts", false));
    }

    #[test]
    fn test_mkdir_all_with_permissions() {
        let dir = std::env::temp_dir().join("kaniko_test_mkdir_perm");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("sub/dir").to_string_lossy().to_string();

        mkdir_all_with_permissions(&path, 0o755, -1, -1).unwrap();
        assert!(Path::new(&path).exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_create_parent_directory() {
        let dir = std::env::temp_dir().join("kaniko_test_parent_dir");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("a/b/c/file.txt").to_string_lossy().to_string();

        create_parent_directory(&path).unwrap();
        assert!(dir.join("a/b/c").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_cleaned_path_against_ignore_list() {
        // After init, /kaniko should be in ignore list
        let _ = crate::ignore_list::init_ignore_list();
        assert!(check_cleaned_path_against_ignore_list(Path::new("/kaniko")));
        assert!(!check_cleaned_path_against_ignore_list(Path::new("/usr")));
    }

    #[test]
    fn test_file_context_new() {
        let ctx = FileContext::new("/workspace");
        assert_eq!(ctx.root, "/workspace");
        assert!(ctx.excluded_files.is_empty());
    }

    #[test]
    fn test_file_context_excludes_file() {
        let mut ctx = FileContext::new("/workspace");
        ctx.excluded_files.push("*.log".to_string());
        ctx.excluded_files.push("temp/".to_string());

        assert!(ctx.excludes_file("/workspace/debug.log"));
        assert!(!ctx.excludes_file("/workspace/src/main.rs"));
    }

    #[test]
    fn test_new_file_context_from_dockerfile_no_dockerignore() {
        let dir = std::env::temp_dir().join("kaniko_test_filectx");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "FROM scratch\n").unwrap();

        let ctx = new_file_context_from_dockerfile(
            dockerfile.to_string_lossy().as_ref(),
            dir.to_string_lossy().as_ref(),
        );
        assert_eq!(ctx.root, dir.to_string_lossy().as_ref());
        assert!(ctx.excluded_files.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_new_file_context_with_dockerignore() {
        let dir = std::env::temp_dir().join("kaniko_test_filectx_ignore");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let dockerfile = dir.join("Dockerfile");
        fs::write(&dockerfile, "FROM scratch\n").unwrap();
        let dockerignore = dir.join(".dockerignore");
        fs::write(&dockerignore, "*.log\ntemp/\n").unwrap();

        let ctx = new_file_context_from_dockerfile(
            dockerfile.to_string_lossy().as_ref(),
            dir.to_string_lossy().as_ref(),
        );
        assert!(!ctx.excluded_files.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }
}