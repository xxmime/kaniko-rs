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

/// Locate and load an image from a local OCI Layout path.
///
/// Reads the index.json, finds the first manifest, and loads the image
/// from the layout's blobs directory.
///
/// Analogous to Go: `locateImage()`.
pub fn locate_image(path: &std::path::Path) -> Result<MutableImage> {
    let layout_path = path;

    // Read index.json to find manifests
    let index_path = layout_path.join("index.json");
    if !index_path.exists() {
        return Err(LayoutCacheError::Miss(format!(
            "no index.json in {}",
            path.display()
        )));
    }

    let index_bytes = std::fs::read(&index_path)?;
    let index: serde_json::Value = serde_json::from_slice(&index_bytes)?;

    let manifests = index
        .get("manifests")
        .and_then(|m| m.as_array())
        .ok_or_else(|| LayoutCacheError::Miss(format!("no manifests in index.json at {}", path.display())))?;

    if manifests.is_empty() {
        return Err(LayoutCacheError::Miss(format!(
            "path contains no images: {}",
            path.display()
        )));
    }

    // Use the first manifest
    let first_manifest = &manifests[0];
    let digest = first_manifest
        .get("digest")
        .and_then(|d| d.as_str())
        .ok_or_else(|| LayoutCacheError::Miss("manifest missing digest".to_string()))?;

    // Load manifest blob
    let blob_path = layout_path
        .join("blobs")
        .join("sha256")
        .join(digest.strip_prefix("sha256:").unwrap_or(digest));
    if !blob_path.exists() {
        return Err(LayoutCacheError::Miss(format!(
            "manifest blob not found: {}",
            blob_path.display()
        )));
    }

    let manifest_bytes = std::fs::read(&blob_path)?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

    // Load config blob
    let config_digest = manifest.config.digest.to_string();
    let config_blob_path = layout_path
        .join("blobs")
        .join("sha256")
        .join(config_digest.strip_prefix("sha256:").unwrap_or(&config_digest));
    let config_bytes = std::fs::read(&config_blob_path)?;
    let config: ImageConfig = serde_json::from_slice(&config_bytes)?;

    // Load layer blobs
    let mut layers = Vec::new();
    for layer_desc in &manifest.layers {
        let layer_digest = layer_desc.digest.to_string();
        let layer_blob_path = layout_path
            .join("blobs")
            .join("sha256")
            .join(layer_digest.strip_prefix("sha256:").unwrap_or(&layer_digest));
        let data = std::fs::read(&layer_blob_path)?;
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

/// Load a cached image from a tar file path.
///
/// This is used when the cache is stored as a Docker tar archive.
/// Also checks for an adjacent manifest file (path.json).
///
/// Analogous to Go: `cachedImageFromPath()`.
pub fn cached_image_from_path(path: &std::path::Path) -> Result<MutableImage> {
    if !path.exists() {
        return Err(LayoutCacheError::Miss(format!(
            "cache tar file not found: {}",
            path.display()
        )));
    }

    // Try to read the tar as an OCI image
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);

    let manifest: Option<Manifest> = None;
    let mut config: Option<ImageConfig> = None;
    let mut config_bytes: Option<Vec<u8>> = None;
    let mut layers: Vec<Layer> = Vec::new();

    // Iterate tar entries to find manifest.json, config, and layer files
    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.to_path_buf();
        let entry_str = entry_path.to_string_lossy().to_string();

        if entry_str == "manifest.json" {
            let mut bytes = Vec::new();
            use std::io::Read;
            entry.read_to_end(&mut bytes)?;

            // Docker tar manifest is an array
            let manifests: Vec<serde_json::Value> = serde_json::from_slice(&bytes)?;
            if let Some(first) = manifests.first() {
                if let Some(_config_file) = first.get("Config").and_then(|c| c.as_str()) {
                    // Will be processed in a subsequent entry
                }
            }
        } else if entry_str.ends_with(".json") && !entry_str.contains('/') {
            // Could be a config JSON file
            let mut bytes = Vec::new();
            use std::io::Read;
            entry.read_to_end(&mut bytes)?;

            if let Ok(cfg) = serde_json::from_slice::<ImageConfig>(&bytes) {
                config = Some(cfg);
                config_bytes = Some(bytes);
            }
        }
    }

    // Try to find layers — re-open the archive for layer data
    let file = std::fs::File::open(path)?;
    let mut archive = tar::Archive::new(file);

    for entry_result in archive.entries()? {
        let mut entry = entry_result?;
        let entry_path = entry.path()?.to_path_buf();
        let entry_str = entry_path.to_string_lossy().to_string();

        // Layer files are typically in the root of the tar, named by their digest
        // with .tar.gz or no extension
        if entry_str.ends_with(".tar.gz")
            || entry_str.ends_with(".tgz")
            || (entry_str.ends_with(".tar") && !entry_str.contains('/'))
        {
            let mut bytes = Vec::new();
            use std::io::Read;
            entry.read_to_end(&mut bytes)?;

            let media_type = if entry_str.ends_with(".tar.gz") || entry_str.ends_with(".tgz") {
                oci_image::manifest::MediaType::LAYER_OCI_V1_TAR_GZIP
            } else {
                oci_image::manifest::MediaType::LAYER_OCI_V1_TAR
            };

            if let Ok(layer) = Layer::from_bytes(bytes, &media_type) {
                layers.push(layer);
            }
        }
    }

    // Construct manifest if not found
    let manifest = manifest.unwrap_or_else(|| {
        let mut m = Manifest::new();
        m.layers = layers.iter().map(|l| l.to_descriptor()).collect();
        m
    });

    // If we couldn't load config, create a minimal one
    let (config, config_bytes) = match (config, config_bytes) {
        (Some(c), Some(b)) => (c, b),
        _ => {
            let cfg = ImageConfig::default();
            let bytes = serde_json::to_vec(&cfg).unwrap_or_default();
            (cfg, bytes)
        }
    };

    // Check for adjacent manifest file (path.json)
    let mfst_path = format!("{}.json", path.display());
    let mfst_path = std::path::Path::new(&mfst_path);
    let final_manifest = if mfst_path.exists() {
        if let Ok(mfst_bytes) = std::fs::read(mfst_path) {
            if let Ok(m) = serde_json::from_slice::<Manifest>(&mfst_bytes) {
                tracing::info!("Found manifest at {}", mfst_path.display());
                m
            } else {
                manifest
            }
        } else {
            manifest
        }
    } else {
        manifest
    };

    Ok(MutableImage {
        manifest: final_manifest,
        config,
        config_bytes,
        layers,
    })
}