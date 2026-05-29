//! OCI Image Manifest.
//!
//! Implements the OCI Image Manifest specification:
//! <https://github.com/opencontainers/image-spec/blob/main/manifest.md>
//!
//! Analogous to `go-containerregistry/pkg/v1.Manifest`.

use crate::digest::Sha256Digest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// OCI Image Manifest.
///
/// The manifest provides configuration and layer information for a container image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Schema version (must be 2 for OCI).
    pub schema_version: u32,

    /// The media type of this manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    /// The descriptor for the image configuration.
    pub config: Descriptor,

    /// An array of layer descriptors.
    #[serde(default)]
    pub layers: Vec<Descriptor>,

    /// Arbitrary metadata attached to the manifest.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Manifest {
    /// Create a new empty manifest with schema version 2.
    pub fn new() -> Self {
        Self {
            schema_version: 2,
            media_type: Some(MediaType::IMAGE_MANIFEST_V1S2.to_string()),
            config: Descriptor::empty_config(),
            layers: vec![],
            annotations: BTreeMap::new(),
        }
    }

    /// Get the number of layers in this manifest.
    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}

/// A content descriptor (digest + size + media type).
///
/// Descriptors describe the content addressed by a digest and size.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    /// The media type of the referenced content.
    pub media_type: String,

    /// The digest of the referenced content.
    pub digest: Sha256Digest,

    /// The size in bytes of the referenced content.
    pub size: u64,

    /// Arbitrary metadata attached to the descriptor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,

    /// A list of platform descriptors for manifest lists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

impl Descriptor {
    /// Create a descriptor for an empty image config.
    pub fn empty_config() -> Self {
        Self {
            media_type: MediaType::IMAGE_CONFIG.to_string(),
            digest: Sha256Digest::from_bytes(b"{}"),
            size: 2,
            annotations: BTreeMap::new(),
            platform: None,
        }
    }
}

/// Platform specification for multi-arch images.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct Platform {
    /// The CPU architecture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,

    /// The operating system.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,

    /// The CPU variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,

    /// The OS version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_version: Option<String>,

    /// OS features.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os_features: Vec<String>,
}

/// OCI Media Types.
///
/// Standard media types used in the OCI Image and Distribution specs.
pub struct MediaType;

impl MediaType {
    // Image Manifest
    pub const IMAGE_MANIFEST_V1S2: &'static str =
        "application/vnd.docker.distribution.manifest.v2+json";
    pub const IMAGE_MANIFEST_LIST_V2S2: &'static str =
        "application/vnd.docker.distribution.manifest.list.v2+json";
    pub const OCI_IMAGE_MANIFEST_V1: &'static str =
        "application/vnd.oci.image.manifest.v1+json";
    pub const OCI_IMAGE_INDEX_V1: &'static str =
        "application/vnd.oci.image.index.v1+json";

    // Image Config
    pub const IMAGE_CONFIG: &'static str =
        "application/vnd.docker.container.image.v1+json";
    pub const OCI_IMAGE_CONFIG_V1: &'static str =
        "application/vnd.oci.image.config.v1+json";

    // Layer media types
    pub const LAYER_DOCKER_V2_TAR: &'static str =
        "application/vnd.docker.image.rootfs.diff.tar";
    pub const LAYER_DOCKER_V2_TAR_GZIP: &'static str =
        "application/vnd.docker.image.rootfs.diff.tar.gzip";
    pub const LAYER_DOCKER_V2_TAR_ZSTD: &'static str =
        "application/vnd.docker.image.rootfs.diff.tar.zstd";
    pub const LAYER_OCI_V1_TAR: &'static str =
        "application/vnd.oci.image.layer.v1.tar";
    pub const LAYER_OCI_V1_TAR_GZIP: &'static str =
        "application/vnd.oci.image.layer.v1.tar+gzip";
    pub const LAYER_OCI_V1_TAR_ZSTD: &'static str =
        "application/vnd.oci.image.layer.v1.tar+zstd";

    /// Check if a media type is a compressed layer type.
    pub fn is_compressed(media_type: &str) -> bool {
        media_type.contains("gzip") || media_type.contains("zstd")
    }

    /// Check if a media type represents a manifest (not a manifest list).
    pub fn is_manifest(media_type: &str) -> bool {
        media_type == Self::IMAGE_MANIFEST_V1S2
            || media_type == Self::OCI_IMAGE_MANIFEST_V1
            || media_type == Self::IMAGE_CONFIG
    }

    /// Check if a media type represents a manifest list / image index.
    pub fn is_index(media_type: &str) -> bool {
        media_type == Self::IMAGE_MANIFEST_LIST_V2S2
            || media_type == Self::OCI_IMAGE_INDEX_V1
    }

    /// Extract the vendor prefix from a media type.
    ///
    /// Returns "docker" or "oci" depending on the vendor.
    /// Analogous to Go: `executor.extractMediaTypeVendor()`.
    pub fn extract_vendor_prefix(media_type: &str) -> &'static str {
        if media_type.contains("oci") {
            "oci"
        } else {
            "docker"
        }
    }

    /// Convert a layer media type between Docker and OCI formats.
    ///
    /// This is needed when the image being built uses a different format
    /// than the base image layers. For example, when building an OCI
    /// image from a Docker base image, layers need to be converted.
    ///
    /// Analogous to Go: `executor.convertMediaType()`.
    /// Reference: <https://github.com/opencontainers/image-spec/blob/main/media-types.md#compatibility-matrix>
    pub fn convert_layer_media_type(media_type: &str, target_vendor: &str, use_zstd: bool) -> Option<String> {
        let current_vendor = Self::extract_vendor_prefix(media_type);
        if current_vendor == target_vendor {
            // Same vendor, no conversion needed (unless zstd is requested)
            if target_vendor == "oci" && use_zstd {
                return Some(Self::LAYER_OCI_V1_TAR_ZSTD.to_string());
            }
            return Some(media_type.to_string());
        }

        match media_type {
            // Docker → OCI
            Self::LAYER_DOCKER_V2_TAR => {
                if use_zstd {
                    Some(Self::LAYER_OCI_V1_TAR_ZSTD.to_string())
                } else {
                    Some(Self::LAYER_OCI_V1_TAR.to_string())
                }
            }
            Self::LAYER_DOCKER_V2_TAR_GZIP => {
                if use_zstd {
                    Some(Self::LAYER_OCI_V1_TAR_ZSTD.to_string())
                } else {
                    Some(Self::LAYER_OCI_V1_TAR_GZIP.to_string())
                }
            }
            Self::LAYER_DOCKER_V2_TAR_ZSTD => {
                Some(Self::LAYER_OCI_V1_TAR_ZSTD.to_string())
            }

            // OCI → Docker
            Self::LAYER_OCI_V1_TAR => {
                Some(Self::LAYER_DOCKER_V2_TAR.to_string())
            }
            Self::LAYER_OCI_V1_TAR_GZIP => {
                Some(Self::LAYER_DOCKER_V2_TAR_GZIP.to_string())
            }
            Self::LAYER_OCI_V1_TAR_ZSTD => {
                Some(Self::LAYER_DOCKER_V2_TAR_ZSTD.to_string())
            }

            _ => None,
        }
    }

    /// Convert a manifest-level media type between Docker and OCI formats.
    ///
    /// Analogous to Go: `executor.convertMediaType()`.
    pub fn convert_manifest_media_type(media_type: &str) -> Option<String> {
        match media_type {
            // Docker → OCI
            Self::IMAGE_MANIFEST_V1S2 => Some(Self::OCI_IMAGE_MANIFEST_V1.to_string()),
            Self::IMAGE_MANIFEST_LIST_V2S2 => Some(Self::OCI_IMAGE_INDEX_V1.to_string()),
            Self::LAYER_DOCKER_V2_TAR => Some(Self::LAYER_OCI_V1_TAR.to_string()),
            Self::LAYER_DOCKER_V2_TAR_GZIP => Some(Self::LAYER_OCI_V1_TAR_GZIP.to_string()),
            Self::IMAGE_CONFIG => Some(Self::OCI_IMAGE_CONFIG_V1.to_string()),

            // OCI → Docker
            Self::OCI_IMAGE_MANIFEST_V1 => Some(Self::IMAGE_MANIFEST_V1S2.to_string()),
            Self::OCI_IMAGE_INDEX_V1 => Some(Self::IMAGE_MANIFEST_LIST_V2S2.to_string()),
            Self::OCI_IMAGE_CONFIG_V1 => Some(Self::IMAGE_CONFIG.to_string()),
            Self::LAYER_OCI_V1_TAR => Some(Self::LAYER_DOCKER_V2_TAR.to_string()),
            Self::LAYER_OCI_V1_TAR_GZIP => Some(Self::LAYER_DOCKER_V2_TAR_GZIP.to_string()),
            Self::LAYER_OCI_V1_TAR_ZSTD => Some(Self::LAYER_DOCKER_V2_TAR_ZSTD.to_string()),

            _ => None,
        }
    }
}

impl std::fmt::Display for MediaType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", Self::OCI_IMAGE_MANIFEST_V1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manifest_new() {
        let m = Manifest::new();
        assert_eq!(m.schema_version, 2);
        assert!(m.layers.is_empty());
    }

    #[test]
    fn test_manifest_serde_roundtrip() {
        let m = Manifest::new();
        let json = serde_json::to_string(&m).unwrap();
        let deserialized: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, deserialized);
    }

    #[test]
    fn test_media_type_is_compressed() {
        assert!(MediaType::is_compressed(MediaType::LAYER_DOCKER_V2_TAR_GZIP));
        assert!(MediaType::is_compressed(MediaType::LAYER_OCI_V1_TAR_GZIP));
        assert!(!MediaType::is_compressed(MediaType::LAYER_OCI_V1_TAR));
    }

    #[test]
    fn test_extract_vendor_prefix() {
        assert_eq!(MediaType::extract_vendor_prefix(MediaType::LAYER_DOCKER_V2_TAR_GZIP), "docker");
        assert_eq!(MediaType::extract_vendor_prefix(MediaType::LAYER_OCI_V1_TAR_GZIP), "oci");
        assert_eq!(MediaType::extract_vendor_prefix(MediaType::OCI_IMAGE_MANIFEST_V1), "oci");
        assert_eq!(MediaType::extract_vendor_prefix(MediaType::IMAGE_MANIFEST_V1S2), "docker");
    }

    #[test]
    fn test_convert_layer_media_type_docker_to_oci() {
        let result = MediaType::convert_layer_media_type(
            MediaType::LAYER_DOCKER_V2_TAR_GZIP, "oci", false);
        assert_eq!(result, Some(MediaType::LAYER_OCI_V1_TAR_GZIP.to_string()));

        let result = MediaType::convert_layer_media_type(
            MediaType::LAYER_DOCKER_V2_TAR, "oci", true);
        assert_eq!(result, Some(MediaType::LAYER_OCI_V1_TAR_ZSTD.to_string()));
    }

    #[test]
    fn test_convert_layer_media_type_oci_to_docker() {
        let result = MediaType::convert_layer_media_type(
            MediaType::LAYER_OCI_V1_TAR_GZIP, "docker", false);
        assert_eq!(result, Some(MediaType::LAYER_DOCKER_V2_TAR_GZIP.to_string()));

        let result = MediaType::convert_layer_media_type(
            MediaType::LAYER_OCI_V1_TAR_ZSTD, "docker", false);
        assert_eq!(result, Some(MediaType::LAYER_DOCKER_V2_TAR_ZSTD.to_string()));
    }

    #[test]
    fn test_convert_layer_same_vendor() {
        let result = MediaType::convert_layer_media_type(
            MediaType::LAYER_OCI_V1_TAR_GZIP, "oci", false);
        assert_eq!(result, Some(MediaType::LAYER_OCI_V1_TAR_GZIP.to_string()));
    }

    #[test]
    fn test_convert_manifest_media_type() {
        assert_eq!(
            MediaType::convert_manifest_media_type(MediaType::IMAGE_MANIFEST_V1S2),
            Some(MediaType::OCI_IMAGE_MANIFEST_V1.to_string())
        );
        assert_eq!(
            MediaType::convert_manifest_media_type(MediaType::OCI_IMAGE_MANIFEST_V1),
            Some(MediaType::IMAGE_MANIFEST_V1S2.to_string())
        );
        assert_eq!(
            MediaType::convert_manifest_media_type(MediaType::IMAGE_CONFIG),
            Some(MediaType::OCI_IMAGE_CONFIG_V1.to_string())
        );
    }
}