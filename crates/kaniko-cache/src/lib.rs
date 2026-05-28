//! Layer caching module for kaniko-rs.
//!
//! Supports both registry-based and local OCI Layout caches.

pub mod registry;
pub mod layout;

pub use registry::RegistryCache;
pub use layout::LayoutCache;