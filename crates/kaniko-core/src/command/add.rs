//! ADD command implementation.
//!
//! ADD is like COPY but also supports URL downloads and tar auto-extraction.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// ADD instruction — adds files from context or URLs.
#[derive(Debug)]
pub struct AddCommand {
    sources: Vec<String>,
    destination: String,
    chown: Option<String>,
    chmod: Option<String>,
    link: bool,
    should_cache: bool,
    snapshot_files: Mutex<Vec<PathBuf>>,
    context_dir: PathBuf,
}

impl AddCommand {
    pub fn new(
        sources: Vec<String>,
        destination: String,
        context_dir: PathBuf,
        should_cache: bool,
    ) -> Self {
        Self {
            sources,
            destination,
            chown: None,
            chmod: None,
            link: false,
            should_cache,
            snapshot_files: Mutex::new(vec![]),
            context_dir,
        }
    }

    /// Create an AddCommand with all flags from the parsed instruction.
    pub fn with_flags(
        sources: Vec<String>,
        destination: String,
        chown: Option<String>,
        chmod: Option<String>,
        link: bool,
        context_dir: PathBuf,
        should_cache: bool,
    ) -> Self {
        Self {
            sources,
            destination,
            chown,
            chmod,
            link,
            should_cache,
            snapshot_files: Mutex::new(vec![]),
            context_dir,
        }
    }
}

#[async_trait]
impl BaseCommand for AddCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        let dest = resolve_add_destination(&self.destination, config);
        tracing::info!(
            "ADD {:?} {} (chown={:?}, chmod={:?}, link={})",
            self.sources, dest, self.chown, self.chmod, self.link
        );

        for src in &self.sources {
            if src.starts_with("http://") || src.starts_with("https://") {
                // Download from URL — permissions default to 0600 per Docker spec
                let downloaded_path = download_url(src, &dest).await?;
                // Apply chmod: URL downloads default to 0600 unless --chmod is specified
                let effective_chmod = self.chmod.as_deref().unwrap_or("600");
                crate::command::copy::apply_permissions_pub(&downloaded_path, &self.chown, &Some(effective_chmod.to_string()))?;
                self.snapshot_files.lock().unwrap().push(downloaded_path);
            } else {
                let src_path = self.context_dir.join(src);
                if !src_path.exists() {
                    return Err(CommandError::Failed(format!(
                        "ADD source not found: {}",
                        src_path.display()
                    )));
                }

                // Check if source is a tar archive (auto-extract)
                if src.ends_with(".tar") || src.ends_with(".tar.gz") || src.ends_with(".tgz") {
                    extract_tar(&src_path, &dest)?;
                    // Apply permissions to extracted files
                    if self.chown.is_some() || self.chmod.is_some() {
                        apply_permissions_recursive(&dest, &self.chown, &self.chmod)?;
                    }
                } else if src_path.is_dir() {
                    copy_dir_recursive_with_perms(&src_path, Path::new(&dest), &self.chown, &self.chmod)?;
                } else {
                    let dest_path = if dest.ends_with('/') || Path::new(&dest).is_dir() {
                        PathBuf::from(&dest).join(src_path.file_name().unwrap_or_default())
                    } else {
                        PathBuf::from(&dest)
                    };
                    copy_file_with_perms(&src_path, &dest_path, &self.chown, &self.chmod)?;
                }
                self.snapshot_files.lock().unwrap().push(PathBuf::from(&dest));
            }
        }

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        let mut parts = Vec::new();
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
        format!("ADD {}", parts.join(" "))
    }

    fn metadata_only_impl(&self) -> bool { false }
    fn requires_unpacked_fs_impl(&self) -> bool { true }
    fn should_cache_output_impl(&self) -> bool { self.should_cache }
    fn provides_files_to_snapshot_impl(&self) -> bool { true }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.snapshot_files.lock().unwrap();
        if files.is_empty() { None } else { Some(files.clone()) }
    }

    /// Files used from the build context for cache key computation.
    /// Analogous to Go: `AddCommand.FilesUsedFromContext`.
    /// Skips remote URLs and tar archives (they don't come from build context).
    fn files_used_from_context_impl(
        &self,
        _config: &ContainerConfig,
        _args: &BuildArgs,
    ) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for src in &self.sources {
            // Skip remote URLs
            if src.starts_with("http://") || src.starts_with("https://") {
                continue;
            }
            // Skip tar archives (they get auto-extracted, not copied as context files)
            if src.ends_with(".tar") || src.ends_with(".tar.gz") || src.ends_with(".tgz") {
                continue;
            }
            let full_path = self.context_dir.join(src);
            files.push(full_path);
        }
        Ok(files)
    }
}

async fn download_url(url: &str, dest: &str) -> Result<PathBuf> {
    tracing::info!("Downloading {}", url);
    let response = reqwest::get(url).await
        .map_err(|e| CommandError::Failed(format!("download failed: {}", e)))?;

    if !response.status().is_success() {
        return Err(CommandError::Failed(format!("download failed: HTTP {}", response.status())));
    }

    let bytes = response.bytes().await
        .map_err(|e| CommandError::Failed(format!("download read failed: {}", e)))?;

    let filename = url.split('/').last().unwrap_or("downloaded");
    let dest_path = PathBuf::from(dest).join(filename);
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest_path, &bytes)?;
    Ok(dest_path)
}

fn extract_tar(tar_path: &Path, dest: &str) -> Result<()> {
    tracing::info!("Extracting tar {} to {}", tar_path.display(), dest);
    let file = std::fs::File::open(tar_path)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    std::fs::create_dir_all(dest)?;
    archive.unpack(dest)?;
    Ok(())
}

fn resolve_add_destination(dest: &str, config: &ContainerConfig) -> String {
    if dest.starts_with('/') { dest.to_string() }
    else {
        let cwd = config.working_dir.as_deref().unwrap_or("/");
        format!("{}/{}", cwd.trim_end_matches('/'), dest)
    }
}

fn copy_file_with_perms(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::copy(src, dest)?;
    crate::command::copy::apply_permissions_pub(dest, chown, chmod)?;
    Ok(())
}

fn copy_dir_recursive_with_perms(src: &Path, dest: &Path, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    let dest = dest.join(src.file_name().unwrap_or_default());
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&target)?;
            crate::command::copy::apply_permissions_pub(&target, chown, chmod)?;
        } else {
            if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
            std::fs::copy(entry.path(), &target)?;
            crate::command::copy::apply_permissions_pub(&target, chown, chmod)?;
        }
    }
    Ok(())
}

fn apply_permissions_recursive(dest: &str, chown: &Option<String>, chmod: &Option<String>) -> Result<()> {
    let dest_path = Path::new(dest);
    if !dest_path.exists() { return Ok(()); }
    for entry in walkdir::WalkDir::new(dest_path) {
        let entry = entry?;
        crate::command::copy::apply_permissions_pub(entry.path(), chown, chmod)?;
    }
    Ok(())
}