//! OCI Image Spec implementation for kaniko-rs.
//!
//! This crate provides the core data model for OCI images, including:
//! - Image Manifest and Config
//! - Layer abstraction with tar operations
//! - Image mutation (append layers, update config)
//! - OCI whiteout specification
//! - SHA-256 digest computation
//!
//! This is the central crate in the kaniko-rs workspace, analogous to
//! `go-containerregistry/pkg/v1` in the Go implementation.

pub mod config;
pub mod digest;
pub mod index;
pub mod layer;
pub mod manifest;
pub mod mutate;
pub mod whiteout;

pub use config::{ContainerConfig, HistoryEntry, ImageConfig, RootFs};
pub use digest::Sha256Digest;
pub use index::{ImageIndex, IndexManifest};
pub use layer::{Layer, LayerReader};
pub use manifest::{Descriptor, Manifest, MediaType};
pub use whiteout::WhiteoutEntry;