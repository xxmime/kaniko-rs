//! Cached command implementations.
//!
//! When a cache hit occurs, instead of re-executing the command,
//! we extract the cached layer's filesystem changes directly.
//! Analogous to Go: `commands.CachingCopyCommand` and `commands.CachingRunCommand`.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use oci_image::layer::Layer;
use oci_image::mutate::MutableImage;
use std::path::PathBuf;
use std::sync::Mutex;

/// A cached COPY command that extracts files from a cached layer.
///
/// When the cache hits for a COPY instruction, instead of re-copying files,
/// we extract the layer's tar data directly to the filesystem.
/// Analogous to Go: `commands.CachingCopyCommand`.
#[derive(Debug)]
pub struct CachingCopyCommand {
    /// The cached image containing the layer to extract.
    cached_image: Option<MutableImage>,
    /// Command string for identification.
    command_str: String,
    /// Files extracted from the cached layer.
    extracted_files: Mutex<Vec<PathBuf>>,
    /// The cached layer.
    layer: Mutex<Option<Layer>>,
}

impl CachingCopyCommand {
    pub fn new(cached_image: MutableImage, command_str: String) -> Self {
        Self {
            cached_image: Some(cached_image),
            command_str,
            extracted_files: Mutex::new(vec![]),
            layer: Mutex::new(None),
        }
    }

    /// Get the cached layer (if extracted).
    pub fn layer(&self) -> Option<Layer> {
        self.layer.lock().unwrap().clone()
    }
}

#[async_trait]
impl BaseCommand for CachingCopyCommand {
    async fn execute_impl(&self, _config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        tracing::info!("Found cached COPY layer, extracting to filesystem");

        let cached_image = self.cached_image.as_ref()
            .ok_or_else(|| CommandError::Failed("cached command image is nil".to_string()))?;

        if cached_image.layers.is_empty() {
            return Err(CommandError::Failed("expected 1 layer in cached image but got 0".to_string()));
        }

        // Take the last layer (the one added by the cached COPY)
        let layer = cached_image.layers.last().unwrap().clone();
        
        tracing::debug!("Extracting cached layer: digest={}", layer.digest());

        // Extract the layer to the filesystem
        let extracted = extract_layer_to_fs(&layer)?;

        *self.layer.lock().unwrap() = Some(layer);
        *self.extracted_files.lock().unwrap() = extracted;

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("{} (cached)", self.command_str)
    }

    fn metadata_only_impl(&self) -> bool { false }
    fn requires_unpacked_fs_impl(&self) -> bool { true }
    fn should_cache_output_impl(&self) -> bool { false }
    fn should_detect_deleted_files_impl(&self) -> bool { false }
    fn provides_files_to_snapshot_impl(&self) -> bool { true }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.extracted_files.lock().unwrap();
        if files.is_empty() { None } else { Some(files.clone()) }
    }
}

/// A cached RUN command that extracts files from a cached layer.
///
/// When the cache hits for a RUN instruction, instead of re-running the command,
/// we extract the layer's filesystem changes directly.
/// Analogous to Go: `commands.CachingRunCommand`.
#[derive(Debug)]
pub struct CachingRunCommand {
    /// The cached image containing the layer to extract.
    cached_image: Option<MutableImage>,
    /// Command string for identification.
    command_str: String,
    /// Files extracted from the cached layer.
    extracted_files: Mutex<Vec<PathBuf>>,
    /// The cached layer.
    layer: Mutex<Option<Layer>>,
}

impl CachingRunCommand {
    pub fn new(cached_image: MutableImage, command_str: String) -> Self {
        Self {
            cached_image: Some(cached_image),
            command_str,
            extracted_files: Mutex::new(vec![]),
            layer: Mutex::new(None),
        }
    }

    /// Get the cached layer (if extracted).
    pub fn layer(&self) -> Option<Layer> {
        self.layer.lock().unwrap().clone()
    }
}

#[async_trait]
impl BaseCommand for CachingRunCommand {
    async fn execute_impl(&self, _config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        tracing::info!("Found cached RUN layer, extracting to filesystem");

        let cached_image = self.cached_image.as_ref()
            .ok_or_else(|| CommandError::Failed("cached command image is nil".to_string()))?;

        if cached_image.layers.is_empty() {
            return Err(CommandError::Failed("expected 1 layer in cached image but got 0".to_string()));
        }

        let layer = cached_image.layers.last().unwrap().clone();
        tracing::debug!("Extracting cached layer: digest={}", layer.digest());

        let extracted = extract_layer_to_fs(&layer)?;

        *self.layer.lock().unwrap() = Some(layer);
        *self.extracted_files.lock().unwrap() = extracted;

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("{} (cached)", self.command_str)
    }

    fn metadata_only_impl(&self) -> bool { false }
    fn requires_unpacked_fs_impl(&self) -> bool { true }
    fn should_cache_output_impl(&self) -> bool { false }
    fn should_detect_deleted_files_impl(&self) -> bool { true }
    fn is_args_envs_required_in_cache_impl(&self) -> bool { true }
    fn provides_files_to_snapshot_impl(&self) -> bool { true }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.extracted_files.lock().unwrap();
        if files.is_empty() { None } else { Some(files.clone()) }
    }
}

/// Extract a layer's tar data to the filesystem.
/// Returns the list of files that were extracted.
fn extract_layer_to_fs(layer: &Layer) -> Result<Vec<PathBuf>> {
    let mut extracted_files = Vec::new();

    // Get uncompressed layer data (handles gzip/zstd decompression)
    let tar_data = layer.uncompressed_data()
        .map_err(|e| CommandError::Failed(format!("decompress layer: {}", e)))?;

    let mut archive = tar::Archive::new(tar_data.as_slice());
    let entries = archive.entries()
        .map_err(|e| CommandError::Failed(format!("read tar entries: {}", e)))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| CommandError::Failed(format!("read tar entry: {}", e)))?;
        let path = entry.path().map_err(|e| CommandError::Failed(format!("get entry path: {}", e)))?;
        let path_str = path.to_string_lossy();

        // Skip whiteout files — they mark deletions
        if path_str.contains(".wh.") {
            continue;
        }

        let dest_path = PathBuf::from("/").join(path);
        entry.unpack(&dest_path)
            .map_err(|e| CommandError::Failed(format!("unpack {}: {}", dest_path.display(), e)))?;
        extracted_files.push(dest_path);
    }

    tracing::debug!("Extracted {} files from cached layer", extracted_files.len());
    Ok(extracted_files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_caching_copy_command_creation() {
        let image = MutableImage::empty();
        let cmd = CachingCopyCommand::new(image, "COPY . /app".to_string());
        assert_eq!(cmd.command_string_impl(), "COPY . /app (cached)");
        assert!(cmd.layer().is_none());
    }

    #[test]
    fn test_caching_run_command_creation() {
        let image = MutableImage::empty();
        let cmd = CachingRunCommand::new(image, "RUN make build".to_string());
        assert_eq!(cmd.command_string_impl(), "RUN make build (cached)");
        assert!(cmd.layer().is_none());
    }

    #[test]
    fn test_caching_copy_command_properties() {
        let image = MutableImage::empty();
        let cmd = CachingCopyCommand::new(image, "COPY . /app".to_string());
        assert!(!cmd.metadata_only_impl());
        assert!(cmd.requires_unpacked_fs_impl());
        assert!(!cmd.should_cache_output_impl());
        assert!(!cmd.should_detect_deleted_files_impl());
        assert!(cmd.provides_files_to_snapshot_impl());
    }

    #[test]
    fn test_caching_run_command_properties() {
        let image = MutableImage::empty();
        let cmd = CachingRunCommand::new(image, "RUN make build".to_string());
        assert!(!cmd.metadata_only_impl());
        assert!(cmd.requires_unpacked_fs_impl());
        assert!(cmd.should_detect_deleted_files_impl());
        assert!(cmd.is_args_envs_required_in_cache_impl());
        assert!(cmd.provides_files_to_snapshot_impl());
    }
}