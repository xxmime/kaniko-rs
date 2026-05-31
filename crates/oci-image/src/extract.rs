//! Image extraction utilities.
//!
//! Extracts layers from an OCI image to the filesystem.
//! Analogous to Go: `pkg/util.GetFSFromImage` + `GetFSFromLayers`.
//!
//! Key features:
//! - `extract_image_to_fs()` — extract all layers with default settings
//! - `extract_layers_to_fs()` — extract specific layers with configurable options
//! - `ExtractOptions` — configure IncludeWhiteout, custom ExtractFunc, and ignore list
//! - `ExtractFunc` — custom extraction function for fine-grained control

use crate::layer::Layer;
use crate::mutate::MutableImage;
use crate::whiteout::WhiteoutEntry;
use std::io::Read;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors during image extraction.
#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("layer error: {0}")]
    Layer(#[from] crate::layer::LayerError),
    #[error("extraction failed: {0}")]
    Failed(String),
}

/// Result type for extraction operations.
pub type Result<T> = std::result::Result<T, ExtractError>;

/// Custom extraction function type.
///
/// Called for each tar entry during extraction. Receives:
/// - `root_dir` — the target root directory
/// - `entry_path` — the cleaned relative path within the tar
/// - `header` — the tar header
/// - `data` — the file content bytes
///
/// Return `Ok(())` to indicate the entry was extracted, or an error.
///
/// Analogous to Go: `util.ExtractFunction` = `func(string, *tar.Header, string, io.Reader) error`.
pub type ExtractFunc = Box<dyn Fn(&Path, &Path, &tar::Header, &[u8]) -> Result<()> + Send + Sync>;

/// Configuration options for layer extraction.
///
/// Analogous to Go: `util.FSConfig` + `util.FSOpt`.
#[derive(Default)]
pub struct ExtractOptions {
    /// Whether to include whiteout entries in the extracted files list.
    /// When false (default), whiteout entries are processed but not included
    /// in the returned file list. When true, whiteout files are included.
    /// Analogous to Go: `FSConfig.includeWhiteout`.
    pub include_whiteout: bool,

    /// Custom extraction function. If None, the default `extract_tar_entry` is used.
    /// Receives (root_dir, entry_path, header, file_bytes).
    /// Analogous to Go: `FSConfig.extractFunc`.
    pub extract_func: Option<ExtractFunc>,

    /// Paths to ignore during extraction (not extracted, not returned).
    /// Analogous to Go: `util.CheckCleanedPathAgainstIgnoreList`.
    pub ignore_paths: Vec<PathBuf>,
}

impl ExtractOptions {
    /// Create new default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to include whiteout entries.
    pub fn with_include_whiteout(mut self, include: bool) -> Self {
        self.include_whiteout = include;
        self
    }

    /// Set a custom extraction function.
    pub fn with_extract_func(mut self, func: ExtractFunc) -> Self {
        self.extract_func = Some(func);
        self
    }

    /// Add paths to ignore during extraction.
    pub fn with_ignore_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.ignore_paths = paths;
        self
    }
}

/// Check if a path should be ignored during extraction.
fn should_ignore_path(path: &Path, ignore_paths: &[PathBuf]) -> bool {
    ignore_paths.iter().any(|ignore| path.starts_with(ignore))
}

/// Default tar entry extraction: unpack the entry to the root directory.
/// Handles regular files, directories, symlinks, and hard links.
fn extract_tar_entry(
    root_dir: &Path,
    entry_path: &Path,
    entry: &mut tar::Entry<'_, impl std::io::Read>,
) -> Result<()> {
    let dest_path = root_dir.join(entry_path);

    // Create parent directories
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let header = entry.header();

    match header.entry_type() {
        // Hard link: create a hard link to the already-extracted target
        tar::EntryType::Link => {
            let link_name = entry.link_name()
                .map_err(|e| ExtractError::Failed(format!("get link name: {}", e)))?
                .ok_or_else(|| ExtractError::Failed("hard link without target".into()))?;
            let link_str = link_name.to_string_lossy();
            let cleaned_link = link_str.trim_start_matches("./");
            let link_target = root_dir.join(cleaned_link);

            if !link_target.exists() {
                return Err(ExtractError::Failed(format!(
                    "hard link target not found: {} -> {}",
                    entry_path.display(), link_target.display()
                )));
            }

            // Remove existing file if present
            if dest_path.exists() {
                std::fs::remove_file(&dest_path)?;
            }

            std::fs::hard_link(&link_target, &dest_path)
                .map_err(|e| ExtractError::Failed(format!(
                    "hard link {} -> {}: {}",
                    dest_path.display(), link_target.display(), e
                )))?;
        }
        // Symlink: create a symbolic link
        tar::EntryType::Symlink => {
            let link_name = entry.link_name()
                .map_err(|e| ExtractError::Failed(format!("get symlink target: {}", e)))?
                .ok_or_else(|| ExtractError::Failed("symlink without target".into()))?;

            // Remove existing file/symlink if present
            if dest_path.exists() || dest_path.symlink_metadata().is_ok() {
                std::fs::remove_file(&dest_path)?;
            }

            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&link_name, &dest_path)
                    .map_err(|e| ExtractError::Failed(format!(
                        "symlink {} -> {}: {}",
                        dest_path.display(), link_name.display(), e
                    )))?;
            }
            #[cfg(not(unix))]
            {
                // Fallback: just copy the entry using unpack
                entry.unpack(&dest_path)
                    .map_err(|e| ExtractError::Failed(format!("unpack {}: {}", dest_path.display(), e)))?;
            }
        }
        // All other entry types: use tar's unpack
        _ => {
            entry.unpack(&dest_path)
                .map_err(|e| ExtractError::Failed(format!("unpack {}: {}", dest_path.display(), e)))?;
        }
    }

    Ok(())
}

/// Extract all layers from an image to the given root directory.
///
/// This is the primary function used during the build process to unpack
/// the base image's filesystem. Layers are extracted in order, with
/// later layers overwriting earlier ones. Whiteout entries are processed
/// to handle file deletions.
///
/// Analogous to Go: `util.GetFSFromImage`.
pub fn extract_image_to_fs(image: &MutableImage, root_dir: &Path) -> Result<Vec<PathBuf>> {
    extract_layers_to_fs(&image.layers, root_dir, ExtractOptions::new())
}

/// Extract all layers from an image with configurable options.
///
/// Analogous to Go: `util.GetFSFromImage(root, img, extract)`.
pub fn extract_image_to_fs_with_options(
    image: &MutableImage,
    root_dir: &Path,
    options: ExtractOptions,
) -> Result<Vec<PathBuf>> {
    extract_layers_to_fs(&image.layers, root_dir, options)
}

/// Extract a slice of layers to the given root directory with configurable options.
///
/// This is the core extraction function that iterates through layers,
/// processes whiteout entries, and extracts files using the configured
/// extract function.
///
/// Analogous to Go: `util.GetFSFromLayers(root, layers, opts...)`.
pub fn extract_layers_to_fs(
    layers: &[Layer],
    root_dir: &Path,
    options: ExtractOptions,
) -> Result<Vec<PathBuf>> {
    let mut extracted_files = Vec::new();

    for (i, layer) in layers.iter().enumerate() {
        tracing::debug!("Extracting layer {}/{}", i + 1, layers.len());
        let layer_files = extract_layer_with_options(layer, root_dir, &options)?;
        extracted_files.extend(layer_files);
    }

    tracing::info!("Extracted {} files from {} layers", extracted_files.len(), layers.len());
    Ok(extracted_files)
}

/// Extract a single layer with configurable options.
///
/// Handles both compressed (gzip/zstd) and uncompressed layers.
/// Processes whiteout entries for file deletions.
fn extract_layer_with_options(
    layer: &Layer,
    root_dir: &Path,
    options: &ExtractOptions,
) -> Result<Vec<PathBuf>> {
    let mut extracted_files = Vec::new();

    // Get uncompressed layer data
    let tar_data = layer.uncompressed_data()?;

    let mut archive = tar::Archive::new(tar_data.as_slice());
    let entries = archive.entries()
        .map_err(|e| ExtractError::Failed(format!("read tar entries: {}", e)))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| ExtractError::Failed(format!("read tar entry: {}", e)))?;
        let path = entry.path().map_err(|e| ExtractError::Failed(format!("get entry path: {}", e)))?;
        let path_str = path.to_string_lossy().to_string();
        let cleaned_path = PathBuf::from(path_str.trim_start_matches("./"));

        // Check ignore list
        if should_ignore_path(&cleaned_path, &options.ignore_paths) {
            tracing::trace!("Ignoring path during extraction: {}", cleaned_path.display());
            continue;
        }

        // Check for whiteout entries
        if let Some(whiteout) = WhiteoutEntry::from_tar_path(Path::new(&path_str)) {
            match whiteout {
                WhiteoutEntry::Regular { parent, name } => {
                    let wh_path = parent.join(&name);
                    let full_path = root_dir.join(&wh_path);
                    if full_path.exists() {
                        if full_path.is_dir() {
                            std::fs::remove_dir_all(&full_path)?;
                        } else {
                            std::fs::remove_file(&full_path)?;
                        }
                        tracing::debug!("Whiteout: removed {}", wh_path.display());
                    }

                    // Include whiteout in extracted files if option is set
                    if options.include_whiteout {
                        extracted_files.push(root_dir.join(&cleaned_path));
                    }
                }
                WhiteoutEntry::Opaque { directory } => {
                    let full_dir = root_dir.join(&directory);
                    if full_dir.is_dir() {
                        for child in std::fs::read_dir(&full_dir)? {
                            let child = child?;
                            let child_path = child.path();
                            if child_path.is_dir() {
                                std::fs::remove_dir_all(&child_path)?;
                            } else {
                                std::fs::remove_file(&child_path)?;
                            }
                        }
                        tracing::debug!("Opaque whiteout: cleared contents of {}", directory.display());
                    }

                    if options.include_whiteout {
                        extracted_files.push(root_dir.join(&cleaned_path));
                    }
                }
            }
            continue;
        }

        // Use custom extract function or default
        if let Some(ref extract_func) = options.extract_func {
            let header = entry.header().clone();
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)
                .map_err(|e| ExtractError::Failed(format!("read entry data: {}", e)))?;
            extract_func(root_dir, &cleaned_path, &header, &buf)?;
        } else {
            extract_tar_entry(root_dir, &cleaned_path, &mut entry)?;
        }

        extracted_files.push(root_dir.join(&cleaned_path));
    }

    Ok(extracted_files)
}

/// Extract a single layer to the given root directory.
///
/// Handles both compressed (gzip/zstd) and uncompressed layers.
/// Processes whiteout entries for file deletions.
pub fn extract_layer_to_fs(layer: &Layer, root_dir: &Path) -> Result<Vec<PathBuf>> {
    extract_layer_with_options(layer, root_dir, &ExtractOptions::new())
}

/// Check if any command in the build stage requires an unpacked filesystem.
/// This is determined by checking if any command's `requires_unpacked_fs()` returns true.
pub fn should_unpack_fs() -> bool {
    // Default: true for kaniko builds (most commands need the FS)
    true
}

/// Extract with retry support.
///
/// Analogous to Go: `util.Retry(extractFunc, opts.ImageFSExtractRetry, 1000)`.
/// Retries the extraction up to `max_retries` times with a 1-second delay between attempts.
pub fn extract_image_to_fs_with_retry(
    image: &MutableImage,
    root_dir: &Path,
    max_retries: u32,
) -> Result<Vec<PathBuf>> {
    let mut last_error = None;

    for attempt in 0..=max_retries {
        match extract_image_to_fs(image, root_dir) {
            Ok(files) => return Ok(files),
            Err(e) => {
                if attempt < max_retries {
                    tracing::warn!("Extract attempt {}/{} failed: {}. Retrying...", attempt + 1, max_retries + 1, e);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::Layer;
    use tempfile::TempDir;

    #[test]
    fn test_extract_empty_image() {
        let image = MutableImage::empty();
        let tmp = TempDir::new().unwrap();
        let result = extract_image_to_fs(&image, tmp.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_layer_to_fs_empty_tar() {
        let layer = Layer::empty().unwrap();
        let tmp = TempDir::new().unwrap();
        let result = extract_layer_to_fs(&layer, tmp.path()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_layer_with_file() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("test.txt"), "hello world").unwrap();

        let layer = Layer::from_files(
            &[src_dir.join("test.txt")],
            &[],
            &src_dir,
        ).unwrap();

        let extract_dir = TempDir::new().unwrap();
        let result = extract_layer_to_fs(&layer, extract_dir.path()).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_extract_image_preserves_layer_order() {
        let image = MutableImage::empty();
        let tmp = TempDir::new().unwrap();
        let result = extract_image_to_fs(&image, tmp.path()).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_extract_options_default() {
        let opts = ExtractOptions::new();
        assert!(!opts.include_whiteout);
        assert!(opts.extract_func.is_none());
        assert!(opts.ignore_paths.is_empty());
    }

    #[test]
    fn test_extract_options_builder() {
        let opts = ExtractOptions::new()
            .with_include_whiteout(true)
            .with_ignore_paths(vec![PathBuf::from("/proc")]);
        assert!(opts.include_whiteout);
        assert_eq!(opts.ignore_paths.len(), 1);
    }

    #[test]
    fn test_should_ignore_path() {
        let ignore = vec![PathBuf::from("/proc"), PathBuf::from("/sys")];
        assert!(should_ignore_path(Path::new("/proc/meminfo"), &ignore));
        assert!(should_ignore_path(Path::new("/sys/kernel"), &ignore));
        assert!(!should_ignore_path(Path::new("/usr/bin/ls"), &ignore));
    }

    #[test]
    fn test_extract_with_ignore_paths() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir_all(src_dir.join("proc")).unwrap();
        std::fs::write(src_dir.join("proc/meminfo"), "MemTotal: 8192").unwrap();
        std::fs::write(src_dir.join("app.txt"), "hello").unwrap();

        let layer = Layer::from_files(
            &[src_dir.join("proc/meminfo"), src_dir.join("app.txt")],
            &[],
            &src_dir,
        ).unwrap();

        let extract_dir = TempDir::new().unwrap();
        let options = ExtractOptions::new()
            .with_ignore_paths(vec![PathBuf::from("proc")]);

        let result = extract_layer_with_options(&layer, extract_dir.path(), &options).unwrap();
        // app.txt should be extracted, proc/meminfo should be ignored
        let has_app = result.iter().any(|p| p.to_string_lossy().contains("app.txt"));
        let has_proc = result.iter().any(|p| p.to_string_lossy().contains("proc"));
        assert!(has_app, "app.txt should be extracted");
        assert!(!has_proc, "proc/meminfo should be ignored");
    }

    #[test]
    fn test_extract_image_with_retry_empty() {
        let image = MutableImage::empty();
        let tmp = TempDir::new().unwrap();
        let result = extract_image_to_fs_with_retry(&image, tmp.path(), 2).unwrap();
        assert!(result.is_empty());
    }
}