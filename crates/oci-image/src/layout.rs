//! OCI Image Layout implementation.
//!
//! Writes an OCI image to a directory following the OCI Image Layout Specification.
//! Analogous to Go: `go-containerregistry/pkg/v1/layout`.

use crate::config::ImageConfig;
use crate::digest::Sha256Digest;
use crate::layer::Layer;
use crate::manifest::{Descriptor, Manifest, MediaType};
use crate::mutate::MutableImage;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

/// Errors for layout operations.
#[derive(Debug, Error)]
pub enum LayoutError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("digest error: {0}")]
    Digest(#[from] crate::digest::DigestError),
    #[error("layer error: {0}")]
    Layer(#[from] crate::layer::LayerError),
    #[error("layout error: {0}")]
    Layout(String),
}

/// Result type for layout operations.
pub type Result<T> = std::result::Result<T, LayoutError>;

/// Write an OCI image to a directory layout.
///
/// Creates the standard OCI layout structure:
/// ```text
/// <dir>/
/// ├── oci-layout
/// ├── index.json
/// ├── blobs/
/// │   └── sha256/
/// │       ├── <config-digest>
/// │       ├── <manifest-digest>
/// │       └── <layer-digest>...
/// ```
pub fn write_layout(image: &MutableImage, dir: &Path) -> Result<()> {
    // Create directory structure
    fs::create_dir_all(dir.join("blobs").join("sha256"))?;

    // Write oci-layout file
    let oci_layout = serde_json::json!({
        "imageLayoutVersion": "1.0.0"
    });
    fs::write(dir.join("oci-layout"), serde_json::to_string_pretty(&oci_layout)?)?;

    // Write config blob
    let config_bytes = serde_json::to_vec(&image.config)?;
    let config_digest = Sha256Digest::from_bytes(&config_bytes);
    let config_blob_path = dir.join("blobs").join("sha256").join(config_digest.hex_only());
    fs::write(&config_blob_path, &config_bytes)?;

    // Write layer blobs
    let mut layer_descriptors = Vec::new();
    for layer in &image.layers {
        let layer_digest = layer.digest();
        let layer_blob_path = dir.join("blobs").join("sha256").join(layer_digest.hex_only());
        fs::write(&layer_blob_path, layer.data())?;

        layer_descriptors.push(layer.to_descriptor());
    }

    // Build manifest
    let manifest = Manifest {
        schema_version: 2,
        media_type: Some(MediaType::OCI_IMAGE_MANIFEST_V1.to_string()),
        config: Descriptor {
            media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
            digest: config_digest,
            size: config_bytes.len() as u64,
            annotations: BTreeMap::new(),
            platform: None,
        },
        layers: layer_descriptors,
        annotations: BTreeMap::new(),
    };

    // Write manifest blob
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let manifest_digest = Sha256Digest::from_bytes(&manifest_bytes);
    let manifest_blob_path = dir.join("blobs").join("sha256").join(manifest_digest.hex_only());
    fs::write(&manifest_blob_path, &manifest_bytes)?;

    // Write index.json
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": MediaType::OCI_IMAGE_MANIFEST_V1,
            "digest": manifest_digest.to_string(),
            "size": manifest_bytes.len(),
        }]
    });
    fs::write(dir.join("index.json"), serde_json::to_string_pretty(&index)?)?;

    tracing::info!("Wrote OCI layout to {}", dir.display());
    Ok(())
}

/// Read an OCI image layout from a directory.
pub fn read_layout(dir: &Path) -> Result<MutableImage> {
    // Read oci-layout to verify format
    let oci_layout_str = fs::read_to_string(dir.join("oci-layout"))
        .map_err(|e| LayoutError::Layout(format!("Failed to read oci-layout: {}", e)))?;
    let oci_layout: serde_json::Value = serde_json::from_str(&oci_layout_str)?;
    if oci_layout.get("imageLayoutVersion").and_then(|v| v.as_str()) != Some("1.0.0") {
        return Err(LayoutError::Layout("Unsupported OCI layout version".to_string()));
    }

    // Read index.json
    let index_str = fs::read_to_string(dir.join("index.json"))?;
    let index: serde_json::Value = serde_json::from_str(&index_str)?;
    let manifests = index.get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| LayoutError::Layout("Invalid index.json: missing manifests".to_string()))?;

    if manifests.is_empty() {
        return Err(LayoutError::Layout("No manifests in index.json".to_string()));
    }

    // Read the first manifest
    let manifest_desc = &manifests[0];
    let manifest_digest = manifest_desc.get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| LayoutError::Layout("Missing digest in manifest descriptor".to_string()))?;
    let digest_hex = manifest_digest.strip_prefix("sha256:").unwrap_or(manifest_digest);

    let manifest_bytes = fs::read(dir.join("blobs").join("sha256").join(digest_hex))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

    // Read config
    let config_digest_hex = manifest.config.digest.hex_only();
    let config_bytes = fs::read(dir.join("blobs").join("sha256").join(config_digest_hex))?;
    let config: ImageConfig = serde_json::from_slice(&config_bytes)?;

    // Read layers
    let mut layers = Vec::new();
    for layer_desc in &manifest.layers {
        let layer_digest_hex = layer_desc.digest.hex_only();
        let layer_data = fs::read(dir.join("blobs").join("sha256").join(layer_digest_hex))?;
        let layer = Layer::from_bytes(layer_data, &layer_desc.media_type)?;
        layers.push(layer);
    }

    Ok(MutableImage {
        manifest,
        config,
        config_bytes,
        layers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ContainerConfig, RootFs};
    use crate::layer::Layer;
    use tempfile::TempDir;

    #[test]
    fn test_write_and_read_layout() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("image");

        // Create a simple image
        let tar_data = vec![];
        let layer = Layer::from_tar_uncompressed(tar_data).unwrap();
        let image = MutableImage::empty();
        let image = crate::mutate::append_layer(image, layer).unwrap();

        // Write layout
        write_layout(&image, &dir).unwrap();

        // Verify files exist
        assert!(dir.join("oci-layout").exists());
        assert!(dir.join("index.json").exists());
        assert!(dir.join("blobs/sha256").exists());

        // Read it back
        let read_image = read_layout(&dir).unwrap();
        assert_eq!(read_image.layers.len(), 1);
        assert_eq!(read_image.config.architecture, "amd64");
    }

    #[test]
    fn test_write_layout_creates_directories() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("nested").join("image");

        let image = MutableImage::empty();
        write_layout(&image, &dir).unwrap();

        assert!(dir.join("oci-layout").exists());
        assert!(dir.join("blobs/sha256").exists());
    }
}