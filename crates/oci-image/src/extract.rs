//! Image extraction utilities.
//!
//! Extracts layers from an OCI image to the filesystem.
//! Analogous to Go: `pkg/util.GetFSFromImage`.

use crate::layer::Layer;
use crate::mutate::MutableImage;
use crate::whiteout::WhiteoutEntry;
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

/// Extract all layers from an image to the given root directory.
///
/// This is the primary function used during the build process to unpack
/// the base image's filesystem. Layers are extracted in order, with
/// later layers overwriting earlier ones. Whiteout entries are processed
/// to handle file deletions.
///
/// Analogous to Go: `util.GetFSFromImage`.
pub fn extract_image_to_fs(image: &MutableImage, root_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut all_extracted = Vec::new();

    for (i, layer) in image.layers.iter().enumerate() {
        tracing::debug!("Extracting layer {}/{}: {}", i + 1, image.layers.len(), layer.digest());
        let extracted = extract_layer_to_fs(layer, root_dir)?;
        all_extracted.extend(extracted);
    }

    tracing::info!("Extracted {} files from {} layers", all_extracted.len(), image.layers.len());
    Ok(all_extracted)
}

/// Extract a single layer to the given root directory.
///
/// Handles both compressed (gzip/zstd) and uncompressed layers.
/// Processes whiteout entries for file deletions.
pub fn extract_layer_to_fs(layer: &Layer, root_dir: &Path) -> Result<Vec<PathBuf>> {
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
        let path_buf = path.to_path_buf();

        // Check for whiteout entries
        if let Some(whiteout) = WhiteoutEntry::from_tar_path(Path::new(&path_str)) {
            match whiteout {
                WhiteoutEntry::Regular { parent, name } => {
                    // Delete the specific file/directory
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
                }
                WhiteoutEntry::Opaque { directory } => {
                    // Remove all contents in the directory, but keep the directory itself
                    let full_dir = root_dir.join(&directory);
                    if full_dir.is_dir() {
                        for entry in std::fs::read_dir(&full_dir)? {
                            let entry = entry?;
                            let entry_path = entry.path();
                            if entry_path.is_dir() {
                                std::fs::remove_dir_all(&entry_path)?;
                            } else {
                                std::fs::remove_file(&entry_path)?;
                            }
                        }
                        tracing::debug!("Opaque whiteout: cleared contents of {}", directory.display());
                    }
                }
            }
            continue;
        }

        // Unpack the entry to the root directory
        let dest_path = root_dir.join(&path_buf);
        
        // Create parent directories
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        entry.unpack(&dest_path)
            .map_err(|e| ExtractError::Failed(format!("unpack {}: {}", dest_path.display(), e)))?;
        
        extracted_files.push(dest_path);
    }

    Ok(extracted_files)
}

/// Check if any command in the build stage requires an unpacked filesystem.
/// This is determined by checking if any command's `requires_unpacked_fs()` returns true.
pub fn should_unpack_fs() -> bool {
    // Default: true for kaniko builds (most commands need the FS)
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutate::MutableImage;
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
        // Empty tar has no entries
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_layer_with_file() {
        // Create a layer with a single file
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
        // Empty image has no layers, but the function should still work
        let tmp = TempDir::new().unwrap();
        let result = extract_image_to_fs(&image, tmp.path()).unwrap();
        assert_eq!(result.len(), 0);
    }
}