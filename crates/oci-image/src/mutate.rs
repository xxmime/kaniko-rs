//! OCI Image mutation operations.
//!
//! Provides functions to modify OCI images by appending layers or
//! updating configuration. Analogous to `go-containerregistry/pkg/v1/mutate`.

use crate::config::{ContainerConfig, HistoryEntry, ImageConfig};
use crate::digest::Sha256Digest;
use crate::layer::Layer;
use crate::manifest::{Descriptor, Manifest, MediaType};

/// Errors that can occur during image mutation.
#[derive(Debug, thiserror::Error)]
pub enum MutateError {
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("digest error: {0}")]
    Digest(#[from] crate::digest::DigestError),
    #[error("layer error: {0}")]
    Layer(#[from] crate::layer::LayerError),
}

/// Result type for mutation operations.
pub type Result<T> = std::result::Result<T, MutateError>;

/// A mutable OCI image with in-memory data.
///
/// This is the primary image type used during the build process.
/// It holds the manifest, config, and layer data in memory.
#[derive(Debug, Clone)]
pub struct MutableImage {
    /// The image manifest.
    pub manifest: Manifest,
    /// The image configuration.
    pub config: ImageConfig,
    /// Serialized config bytes.
    pub config_bytes: Vec<u8>,
    /// The image layers (raw data).
    pub layers: Vec<Layer>,
}

impl MutableImage {
    /// Create a new empty image (scratch).
    ///
    /// Analogous to `go-containerregistry/pkg/v1/empty.Image`.
    pub fn empty() -> Self {
        let config = ImageConfig::scratch();
        Self::from_config(config)
    }

    /// Create an image from an existing config.
    pub fn from_config(config: ImageConfig) -> Self {
        let config_bytes = serde_json::to_vec(&config).unwrap_or_default();
        let config_digest = Sha256Digest::from_bytes(&config_bytes);
        let config_size = config_bytes.len() as u64;

        let manifest = Manifest {
            schema_version: 2,
            media_type: Some(MediaType::OCI_IMAGE_MANIFEST_V1.to_string()),
            config: Descriptor {
                media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
                digest: config_digest,
                size: config_size,
                annotations: Default::default(),
                platform: None,
            },
            layers: vec![],
            annotations: Default::default(),
        };

        Self {
            manifest,
            config,
            config_bytes,
            layers: vec![],
        }
    }

    /// Get the digest of the image manifest.
    pub fn digest(&self) -> Sha256Digest {
        let manifest_bytes = serde_json::to_vec(&self.manifest).unwrap_or_default();
        Sha256Digest::from_bytes(&manifest_bytes)
    }

    /// Get the digest of the config.
    pub fn config_digest(&self) -> Sha256Digest {
        Sha256Digest::from_bytes(&self.config_bytes)
    }

    /// Get the number of layers.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Recompute config_bytes and update the manifest.config descriptor.
    ///
    /// **Must** be called after any direct mutation of `self.config` that
    /// doesn't go through the `mutate::*` helpers (e.g. setting
    /// `config.os`, `config.architecture`, clearing timestamps for
    /// reproducibility, etc.). If this is not called, the manifest's
    /// config descriptor will reference a stale digest, causing
    /// `MANIFEST_BLOB_UNKNOWN` errors on push.
    pub fn recalculate_config_descriptor(&mut self) {
        self.config_bytes = serde_json::to_vec(&self.config).unwrap_or_default();
        let config_digest = Sha256Digest::from_bytes(&self.config_bytes);
        let config_size = self.config_bytes.len() as u64;
        self.manifest.config = Descriptor {
            media_type: self.manifest.config.media_type.clone(),
            digest: config_digest,
            size: config_size,
            annotations: Default::default(),
            platform: None,
        };
    }
}

/// Append layers to an image.
///
/// This is the core mutation operation used during builds.
/// Each layer addition updates the manifest, config (rootfs.diff_ids + history),
/// and recalculates the config descriptor.
pub fn append_layers(image: MutableImage, new_layers: Vec<Layer>) -> Result<MutableImage> {
    let mut manifest = image.manifest;
    let mut config = image.config;
    let mut existing_layers = image.layers;

    for layer in new_layers {
        // Update manifest.layers
        manifest.layers.push(layer.to_descriptor());

        // Update config.rootfs.diff_ids
        config.rootfs.diff_ids.push(layer.diff_id().to_string());

        // Update config.history
        config.history.push(HistoryEntry {
            created: Some(chrono::Utc::now().to_rfc3339()),
            created_by: Some("kaniko".to_string()),
            comment: None,
            empty_layer: Some(false),
            author: None,
        });

        existing_layers.push(layer);
    }

    // Recalculate config descriptor
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = Sha256Digest::from_bytes(&config_bytes);
    let config_size = config_bytes.len() as u64;

    manifest.config = Descriptor {
        media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
        digest: config_digest,
        size: config_size,
        annotations: Default::default(),
        platform: None,
    };

    Ok(MutableImage {
        manifest,
        config,
        config_bytes,
        layers: existing_layers,
    })
}

/// Append a single layer to an image.
pub fn append_layer(image: MutableImage, layer: Layer) -> Result<MutableImage> {
    append_layers(image, vec![layer])
}

/// Append a single layer with custom history entry.
///
/// Analogous to Go: `mutate.Append()` with `mutate.Addendum{History: ...}`.
pub fn append_layer_with_history(
    image: MutableImage,
    layer: Layer,
    history: HistoryEntry,
) -> Result<MutableImage> {
    let mut manifest = image.manifest;
    let mut config = image.config;
    let mut existing_layers = image.layers;

    // Update manifest.layers
    manifest.layers.push(layer.to_descriptor());

    // Update config.rootfs.diff_ids
    config.rootfs.diff_ids.push(layer.diff_id().to_string());

    // Update config.history with the provided entry
    config.history.push(history);

    existing_layers.push(layer);

    // Recalculate config descriptor
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = Sha256Digest::from_bytes(&config_bytes);
    let config_size = config_bytes.len() as u64;

    manifest.config = Descriptor {
        media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
        digest: config_digest,
        size: config_size,
        annotations: Default::default(),
        platform: None,
    };

    Ok(MutableImage {
        manifest,
        config,
        config_bytes,
        layers: existing_layers,
    })
}

/// Set the created timestamp on the image.
///
/// Analogous to Go: `mutate.CreatedAt()`.
pub fn set_created_at(image: MutableImage, timestamp: String) -> Result<MutableImage> {
    let mut config = image.config;
    config.created = Some(timestamp);

    // Recalculate config descriptor
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = Sha256Digest::from_bytes(&config_bytes);
    let config_size = config_bytes.len() as u64;

    let mut manifest = image.manifest;
    manifest.config = Descriptor {
        media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
        digest: config_digest,
        size: config_size,
        annotations: Default::default(),
        platform: None,
    };

    Ok(MutableImage {
        manifest,
        config,
        config_bytes,
        layers: image.layers,
    })
}

/// Update the image configuration.
///
/// The closure receives a mutable reference to the ContainerConfig
/// and can modify it in place. After modification, the manifest
/// config descriptor is recalculated.
pub fn update_config(image: MutableImage, f: impl FnOnce(&mut ContainerConfig)) -> Result<MutableImage> {
    let mut config = image.config;
    f(&mut config.config);

    // Recalculate config descriptor
    let config_bytes = serde_json::to_vec(&config)?;
    let config_digest = Sha256Digest::from_bytes(&config_bytes);
    let config_size = config_bytes.len() as u64;

    let mut manifest = image.manifest;
    manifest.config = Descriptor {
        media_type: MediaType::OCI_IMAGE_CONFIG_V1.to_string(),
        digest: config_digest,
        size: config_size,
        annotations: Default::default(),
        platform: None,
    };

    Ok(MutableImage {
        manifest,
        config,
        config_bytes,
        layers: image.layers,
    })
}

/// Set the entrypoint on the image.
pub fn set_entrypoint(image: MutableImage, entrypoint: Vec<String>) -> Result<MutableImage> {
    update_config(image, |cfg| {
        cfg.entrypoint = Some(entrypoint);
    })
}

/// Set the cmd on the image.
pub fn set_cmd(image: MutableImage, cmd: Vec<String>) -> Result<MutableImage> {
    update_config(image, |cfg| {
        cfg.cmd = Some(cmd);
    })
}

/// Set the working directory on the image.
pub fn set_working_dir(image: MutableImage, working_dir: String) -> Result<MutableImage> {
    update_config(image, |cfg| {
        cfg.working_dir = Some(working_dir);
    })
}

/// Set an environment variable on the image.
pub fn set_env(image: MutableImage, key: &str, value: &str) -> Result<MutableImage> {
    update_config(image, |cfg| {
        cfg.set_env(key, value);
    })
}

/// Set a label on the image.
pub fn set_label(image: MutableImage, key: &str, value: &str) -> Result<MutableImage> {
    update_config(image, |cfg| {
        let labels = cfg.labels.get_or_insert_with(Default::default);
        labels.insert(key.to_string(), value.to_string());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_image() {
        let image = MutableImage::empty();
        assert_eq!(image.layer_count(), 0);
        assert_eq!(image.manifest.schema_version, 2);
    }

    #[test]
    fn test_append_empty_layer() {
        let image = MutableImage::empty();
        let layer = Layer::empty().unwrap();
        let image = append_layer(image, layer).unwrap();
        assert_eq!(image.layer_count(), 1);
        assert_eq!(image.config.rootfs.diff_ids.len(), 1);
        assert_eq!(image.config.history.len(), 1);
    }

    #[test]
    fn test_update_config() {
        let image = MutableImage::empty();
        let image = set_entrypoint(image, vec!["/bin/sh".to_string()]).unwrap();
        assert_eq!(image.config.config.entrypoint, Some(vec!["/bin/sh".to_string()]));
    }

    #[test]
    fn test_set_env() {
        let image = MutableImage::empty();
        let image = set_env(image, "FOO", "bar").unwrap();
        assert_eq!(image.config.config.get_env("FOO"), Some("bar".to_string()));
    }

    #[test]
    fn test_set_label() {
        let image = MutableImage::empty();
        let image = set_label(image, "version", "1.0").unwrap();
        assert_eq!(
            image.config.config.labels.as_ref().unwrap().get("version"),
            Some(&"1.0".to_string())
        );
    }
}