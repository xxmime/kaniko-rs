//! OCI Layout-based local layer cache.
//!
//! Stores cached layers on the local filesystem using the OCI Layout format.
//! Analogous to Go: `pkg/cache.LayoutCache`.

use oci_image::config::ImageConfig;
use oci_image::layer::Layer;
use oci_image::manifest::Manifest;
use oci_image::mutate::MutableImage;
use std::path::PathBuf;
use thiserror::Error;

/// Errors for layout cache operations.
#[derive(Debug, Error)]
pub enum LayoutCacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
    #[error("mutation error: {0}")]
    Mutate(#[from] oci_image::mutate::MutateError),
    #[error("cache miss for key: {0}")]
    Miss(String),
}

/// Result type for layout cache operations.
pub type Result<T> = std::result::Result<T, LayoutCacheError>;

/// Local OCI Layout cache.
///
/// Caches intermediate build layers on the local filesystem.
pub struct LayoutCache {
    /// Root directory for the cache.
    path: PathBuf,
}

impl LayoutCache {
    /// Create a new layout cache at the given path.
    pub fn new(path: &str) -> Self {
        Self {
            path: PathBuf::from(path),
        }
    }

    /// Initialize the cache directory structure.
    pub fn init(&self) -> Result<()> {
        let blob_dir = self.path.join("blobs/sha256");
        std::fs::create_dir_all(&blob_dir)?;

        // Write oci-layout marker file
        std::fs::write(
            self.path.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )?;

        // Write empty index.json
        let index = serde_json::json!({
            "schemaVersion": 2,
            "manifests": []
        });
        std::fs::write(self.path.join("index.json"), serde_json::to_string_pretty(&index)?)?;

        Ok(())
    }

    /// Retrieve a cached image by key.
    pub fn retrieve_layer(&self, key: &str) -> Result<MutableImage> {
        let key_dir = self.path.join("cache").join(key);
        if !key_dir.exists() {
            return Err(LayoutCacheError::Miss(key.to_string()));
        }

        // Read manifest
        let manifest_path = key_dir.join("manifest.json");
        if !manifest_path.exists() {
            return Err(LayoutCacheError::Miss(key.to_string()));
        }
        let manifest_bytes = std::fs::read(&manifest_path)?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

        // Read config
        let config_path = key_dir.join("config.json");
        if !config_path.exists() {
            return Err(LayoutCacheError::Miss(key.to_string()));
        }
        let config_bytes = std::fs::read(&config_path)?;
        let config: ImageConfig = serde_json::from_slice(&config_bytes)?;

        // Read layers
        let mut layers = Vec::new();
        for layer_desc in &manifest.layers {
            let layer_path = self.path.join("blobs/sha256").join(layer_desc.digest.to_string());
            if !layer_path.exists() {
                return Err(LayoutCacheError::Miss(format!("layer {}", layer_desc.digest)));
            }
            let data = std::fs::read(&layer_path)?;
            let layer = Layer::from_bytes(data, &layer_desc.media_type)?;
            layers.push(layer);
        }

        Ok(MutableImage {
            manifest,
            config,
            config_bytes,
            layers,
        })
    }

    /// Check if a cached layer exists for the given key.
    pub fn exists(&self, key: &str) -> bool {
        let key_dir = self.path.join("cache").join(key);
        key_dir.join("manifest.json").exists()
    }

    /// Push a layer to the cache.
    pub fn push_layer(&self, key: &str, image: &MutableImage) -> Result<()> {
        // Ensure cache directory exists
        let key_dir = self.path.join("cache").join(key);
        std::fs::create_dir_all(&key_dir)?;

        // Write blobs
        let blob_dir = self.path.join("blobs/sha256");
        std::fs::create_dir_all(&blob_dir)?;

        // Write config blob
        let config_digest = image.config_digest().to_string();
        std::fs::write(blob_dir.join(&config_digest), &image.config_bytes)?;

        // Write layer blobs
        for layer in &image.layers {
            let digest = layer.digest().to_string();
            std::fs::write(blob_dir.join(&digest), layer.data())?;
        }

        // Write manifest
        let manifest_json = serde_json::to_string_pretty(&image.manifest)?;
        std::fs::write(key_dir.join("manifest.json"), &manifest_json)?;

        // Write config
        let config_json = serde_json::to_string_pretty(&image.config)?;
        std::fs::write(key_dir.join("config.json"), &config_json)?;

        tracing::info!("Cached layer with key {} to {}", key, self.path.display());
        Ok(())
    }
}