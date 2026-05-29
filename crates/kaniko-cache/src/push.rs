//! Cache push utilities.
//!
//! Computes the cache destination and pushes layers to the cache.
//! Analogous to Go: `pkg/executor/push.go` — `pushLayerToCache`,
//! and `pkg/cache/cache.go` — `Destination`.

use crate::layout::LayoutCache;
use crate::registry::RegistryCache;
use oci_image::config::HistoryEntry;
use oci_image::layer::Layer;
use oci_image::mutate::MutableImage;
use std::path::Path;
use thiserror::Error;

/// Errors for cache push operations.
#[derive(Debug, Error)]
pub enum PushCacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
    #[error("mutation error: {0}")]
    Mutate(#[from] oci_image::mutate::MutateError),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("registry cache error: {0}")]
    Registry(#[from] crate::registry::RegistryCacheError),
    #[error("layout cache error: {0}")]
    Layout(#[from] crate::layout::LayoutCacheError),
    #[error("cache destination error: {0}")]
    Destination(String),
}

/// Result type for cache push operations.
pub type Result<T> = std::result::Result<T, PushCacheError>;

/// Compute the cache destination reference.
///
/// If `cache_repo` is specified, the destination is `{cache_repo}:{cache_key}`.
/// Otherwise, it is inferred from the first destination as `{dest_repo}:{cache_key}`.
///
/// Analogous to Go: `cache.Destination()`.
pub fn cache_destination(
    cache_repo: &Option<String>,
    destinations: &[String],
    cache_key: &str,
) -> Result<String> {
    if let Some(repo) = cache_repo {
        Ok(format!("{}:{}", repo, cache_key))
    } else {
        let dest = destinations
            .first()
            .ok_or_else(|| PushCacheError::Destination("no destinations specified".to_string()))?;
        // Parse "registry/repository:tag" → "registry/repository:cache_key"
        let (repo_part, _) = dest
            .rsplit_once(':')
            .unwrap_or((dest, "latest"));
        Ok(format!("{}:{}", repo_part, cache_key))
    }
}

/// Check if a destination is an OCI layout path.
///
/// OCI layout destinations start with "oci:" prefix.
pub fn is_oci_layout(destination: &str) -> bool {
    destination.starts_with("oci:")
}

/// Push a layer to the cache.
///
/// Creates an empty image with the given layer and pushes it to the
/// cache destination (registry or OCI layout).
///
/// Analogous to Go: `pushLayerToCache()`.
pub async fn push_layer_to_cache(
    cache_repo: &Option<String>,
    destinations: &[String],
    cache_key: &str,
    layer: Layer,
    created_by: &str,
    cache_dir: &Option<String>,
    insecure: bool,
    no_push: bool,
) -> Result<()> {
    let cache_dest = cache_destination(cache_repo, destinations, cache_key)?;

    tracing::info!("Pushing layer {} to cache now", cache_dest);

    // Build a minimal image with just this layer
    // Analogous to Go: create empty.Image, mutate.Append with layer + history
    let image = MutableImage::empty();
    let history = HistoryEntry {
        created: None,
        created_by: Some(created_by.to_string()),
        author: Some("kaniko".to_string()),
        comment: None,
        empty_layer: Some(false),
    };
    let image = oci_image::mutate::append_layer_with_history(image, layer, history)?;

    if is_oci_layout(&cache_dest) {
        // Push to OCI layout on local filesystem
        let layout_path = cache_dest.strip_prefix("oci:").unwrap_or(&cache_dest);
        let cache = LayoutCache::new(layout_path);
        cache.init()?;
        cache.push_layer(cache_key, &image)?;
        tracing::info!("Pushed cache layer to OCI layout: {}", layout_path);
    } else if !no_push {
        // Push to registry cache
        let mut registry_cache = RegistryCache::new(&cache_dest, insecure);
        registry_cache.push_layer(cache_key, image).await?;
        tracing::info!("Pushed cache layer to registry: {}", cache_dest);
    } else {
        tracing::debug!("Skipping cache push (no_push=true)");
    }

    Ok(())
}

/// Push a layer to local cache only (no registry push).
///
/// Used when --no-push-cache is set or when only local caching is desired.
pub fn push_layer_to_local_cache(
    cache_dir: &Option<String>,
    cache_key: &str,
    image: &MutableImage,
) -> Result<()> {
    if let Some(dir) = cache_dir {
        let cache = LayoutCache::new(dir);
        if !Path::new(dir).join("oci-layout").exists() {
            cache.init()?;
        }
        cache.push_layer(cache_key, image)?;
        tracing::info!("Cached layer {} to local cache", cache_key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_destination_with_cache_repo() {
        let cache_repo = Some("gcr.io/project/cache".to_string());
        let destinations = vec!["gcr.io/project/app:v1".to_string()];
        let dest = cache_destination(&cache_repo, &destinations, "abc123").unwrap();
        assert_eq!(dest, "gcr.io/project/cache:abc123");
    }

    #[test]
    fn test_cache_destination_inferred() {
        let cache_repo: Option<String> = None;
        let destinations = vec!["gcr.io/project/app:v1".to_string()];
        let dest = cache_destination(&cache_repo, &destinations, "abc123").unwrap();
        assert_eq!(dest, "gcr.io/project/app:abc123");
    }

    #[test]
    fn test_cache_destination_no_destinations() {
        let cache_repo: Option<String> = None;
        let destinations: Vec<String> = vec![];
        let result = cache_destination(&cache_repo, &destinations, "abc123");
        assert!(result.is_err());
    }

    #[test]
    fn test_is_oci_layout() {
        assert!(is_oci_layout("oci:/tmp/cache"));
        assert!(is_oci_layout("oci:./cache"));
        assert!(!is_oci_layout("gcr.io/project/cache"));
        assert!(!is_oci_layout("localhost:5000/cache"));
    }

    #[test]
    fn test_cache_destination_with_tag_in_dest() {
        let cache_repo: Option<String> = None;
        let destinations = vec!["myregistry.com/myimage:latest".to_string()];
        let dest = cache_destination(&cache_repo, &destinations, "key123").unwrap();
        assert_eq!(dest, "myregistry.com/myimage:key123");
    }

    #[test]
    fn test_cache_destination_with_port() {
        let cache_repo = Some("localhost:5000/cache".to_string());
        let destinations = vec!["localhost:5000/app:test".to_string()];
        let dest = cache_destination(&cache_repo, &destinations, "key").unwrap();
        assert_eq!(dest, "localhost:5000/cache:key");
    }
}