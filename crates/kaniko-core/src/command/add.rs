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
        tracing::info!("ADD {:?} {}", self.sources, dest);

        for src in &self.sources {
            if src.starts_with("http://") || src.starts_with("https://") {
                // Download from URL
                let downloaded_path = download_url(src, &dest).await?;
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
                } else if src_path.is_dir() {
                    copy_dir_recursive(&src_path, Path::new(&dest))?;
                } else {
                    let dest_path = if dest.ends_with('/') || Path::new(&dest).is_dir() {
                        PathBuf::from(&dest).join(src_path.file_name().unwrap_or_default())
                    } else {
                        PathBuf::from(&dest)
                    };
                    copy_file(&src_path, &dest_path)?;
                }
            }
        }

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("ADD {} {}", self.sources.join(" "), self.destination)
    }

    fn metadata_only_impl(&self) -> bool { false }
    fn requires_unpacked_fs_impl(&self) -> bool { false }
    fn should_cache_output_impl(&self) -> bool { self.should_cache }
    fn provides_files_to_snapshot_impl(&self) -> bool { true }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.snapshot_files.lock().unwrap();
        if files.is_empty() { None } else { Some(files.clone()) }
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

fn copy_file(src: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() { std::fs::create_dir_all(parent)?; }
    std::fs::copy(src, dest)?;
    Ok(())
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    let dest = dest.join(src.file_name().unwrap_or_default());
    for entry in walkdir::WalkDir::new(src) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() { std::fs::create_dir_all(&target)?; }
        else {
            if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}