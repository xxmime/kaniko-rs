//! OCI Image Index (Manifest List).
//!
//! Implements the OCI Image Index specification for multi-arch images:
//! <https://github.com/opencontainers/image-spec/blob/main/image-index.md>
//!
//! Analogous to `go-containerregistry/pkg/v1/v1util`.

use crate::manifest::{Descriptor, MediaType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// OCI Image Index (also called Manifest List in Docker terms).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexManifest {
    /// Schema version (must be 2).
    pub schema_version: u32,

    /// The media type of this image index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,

    /// An array of manifests descriptors.
    pub manifests: Vec<Descriptor>,

    /// Arbitrary metadata attached to the image index.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl IndexManifest {
    /// Create a new empty image index.
    pub fn new() -> Self {
        Self {
            schema_version: 2,
            media_type: Some(MediaType::OCI_IMAGE_INDEX_V1.to_string()),
            manifests: vec![],
            annotations: BTreeMap::new(),
        }
    }

    /// Add a manifest descriptor for a specific platform.
    pub fn add_manifest(&mut self, descriptor: Descriptor) {
        self.manifests.push(descriptor);
    }

    /// Find a manifest descriptor matching the given architecture and OS.
    pub fn find_manifest(&self, architecture: &str, os: &str) -> Option<&Descriptor> {
        self.manifests.iter().find(|d| {
            d.platform
                .as_ref()
                .map_or(false, |p| {
                    p.architecture.as_deref() == Some(architecture)
                        && p.os.as_deref() == Some(os)
                })
        })
    }
}

impl Default for IndexManifest {
    fn default() -> Self {
        Self::new()
    }
}

/// An OCI Image Index wrapper.
#[derive(Debug, Clone)]
pub struct ImageIndex {
    /// The index manifest.
    pub manifest: IndexManifest,
}

impl ImageIndex {
    /// Create a new empty image index.
    pub fn new() -> Self {
        Self {
            manifest: IndexManifest::new(),
        }
    }
}

impl Default for ImageIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Platform;

    #[test]
    fn test_index_manifest_new() {
        let idx = IndexManifest::new();
        assert_eq!(idx.schema_version, 2);
        assert!(idx.manifests.is_empty());
    }

    #[test]
    fn test_index_manifest_find() {
        let mut idx = IndexManifest::new();
        idx.add_manifest(Descriptor {
            media_type: MediaType::OCI_IMAGE_MANIFEST_V1.to_string(),
            digest: crate::digest::Sha256Digest::from_bytes(b"test"),
            size: 100,
            annotations: Default::default(),
            platform: Some(Platform {
                architecture: Some("amd64".to_string()),
                os: Some("linux".to_string()),
                ..Default::default()
            }),
        });

        let found = idx.find_manifest("amd64", "linux");
        assert!(found.is_some());

        let not_found = idx.find_manifest("arm64", "linux");
        assert!(not_found.is_none());
    }
}