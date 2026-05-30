//! Layer caching module for kaniko-rs.
//!
//! Supports both registry-based and local OCI Layout caches.
//! Provides cache destination computation and layer push utilities.
//!
//! Analogous to Go: `pkg/cache/` — cache package.

pub mod registry;
pub mod layout;
pub mod push;

pub use registry::RegistryCache;
pub use layout::{LayoutCache, locate_image, cached_image_from_path};
pub use push::{cache_destination, push_layer_to_cache, PushCacheError};