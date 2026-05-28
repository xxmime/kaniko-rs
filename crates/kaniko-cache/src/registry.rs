//! Registry-based layer cache.
//!
//! Stores cached layers as tags in a registry repository.
//! Analogous to Go: `pkg/cache.RegistryCache`.

use oci_image::layer::Layer;
use oci_image::manifest::Manifest;
use oci_image::mutate::MutableImage;
use oci_registry::Reference;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use std::collections::HashMap;
use thiserror::Error;

/// Errors for registry cache operations.
#[derive(Debug, Error)]
pub enum RegistryCacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("cache miss for key: {0}")]
    Miss(String),
    #[error("registry error: {0}")]
    Registry(String),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("reference parsing error: {0}")]
    Reference(#[from] oci_registry::PushError),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
}

/// Result type for registry cache operations.
pub type Result<T> = std::result::Result<T, RegistryCacheError>;

/// Authentication token response
#[derive(Debug, Deserialize)]
struct AuthTokenResponse {
    token: String,
}

/// Registry-based layer cache.
///
/// Caches intermediate build layers as tags in a registry repository.
/// The cache key is used as the tag name.
pub struct RegistryCache {
    /// The repository to use for caching (e.g., "gcr.io/my-project/cache").
    cache_repo: String,
    /// HTTP client for registry operations.
    client: reqwest::Client,
    /// Whether to use insecure (HTTP) connections.
    insecure: bool,
    /// Authentication token cache
    auth_tokens: HashMap<String, String>,
}

impl RegistryCache {
    /// Create a new registry cache.
    pub fn new(cache_repo: &str, insecure: bool) -> Self {
        Self {
            cache_repo: cache_repo.to_string(),
            client: reqwest::Client::new(),
            insecure,
            auth_tokens: HashMap::new(),
        }
    }

    /// Get the base URL for the registry.
    fn get_base_url(&self, registry: &str) -> String {
        let scheme = if self.insecure { "http" } else { "https" };
        format!("{}://{}/v2", scheme, registry)
    }

    /// Authenticate with the registry for a specific repository.
    async fn authenticate(&mut self, registry: &str, repository: &str) -> Result<String> {
        let cache_key = format!("{}:{}", registry, repository);
        
        if let Some(token) = self.auth_tokens.get(&cache_key) {
            return Ok(token.clone());
        }

        let scheme = if self.insecure { "http" } else { "https" };
        let auth_url = format!("{}://{}/v2", scheme, registry);
        
        // Try to get an anonymous token first
        let response = self.client.get(&auth_url).send().await?;
        
        if response.status() == 401 {
            if let Some(www_auth) = response.headers().get("WWW-Authenticate") {
                let auth_header = www_auth.to_str().map_err(|_| {
                    RegistryCacheError::Auth("Invalid WWW-Authenticate header".to_string())
                })?;
                
                // Parse the authentication challenge
                if auth_header.starts_with("Bearer ") {
                    let realm = self.extract_auth_param(auth_header, "realm");
                    let service = self.extract_auth_param(auth_header, "service");
                    let scope = self.extract_auth_param(auth_header, "scope");
                    
                    if let Some(realm) = realm {
                        let mut token_url = realm.to_string();
                        if let Some(service) = service {
                            token_url.push_str(&format!("?service={}", service));
                        }
                        if let Some(scope) = scope {
                            token_url.push_str(&format!("&scope={}", scope));
                        }
                        
                        let token_response = self.client.get(&token_url).send().await?;
                        if token_response.status().is_success() {
                            let auth_response: AuthTokenResponse = token_response.json().await?;
                            self.auth_tokens.insert(cache_key, auth_response.token.clone());
                            return Ok(auth_response.token);
                        }
                    }
                }
            }
            
            return Err(RegistryCacheError::Auth("Authentication required".to_string()));
        }
        
        // Use anonymous access
        let token = "anonymous".to_string();
        self.auth_tokens.insert(cache_key, token.clone());
        Ok(token)
    }

    /// Extract parameter from WWW-Authenticate header.
    fn extract_auth_param(&self, auth_header: &str, param: &str) -> Option<String> {
        let param_prefix = format!("{}=\"", param);
        if let Some(start) = auth_header.find(&param_prefix) {
            let start = start + param_prefix.len();
            if let Some(end) = auth_header[start..].find('"') {
                return Some(auth_header[start..start + end].to_string());
            }
        }
        None
    }

    /// Retrieve a cached layer by key.
    ///
    /// The key is used as a tag in the cache repository.
    /// Returns the cached image if found.
    pub async fn retrieve_layer(&mut self, key: &str) -> Result<MutableImage> {
        let reference = format!("{}:{}", self.cache_repo, key);
        let reference = Reference::parse(&reference)?;
        
        let base_url = self.get_base_url(&reference.registry);
        let token = self.authenticate(&reference.registry, &reference.repository).await?;
        
        // Get manifest
        let manifest_url = format!(
            "{}/{}/manifests/{}",
            base_url,
            reference.repository,
            reference.tag
        );
        
        let response = self.client
            .get(&manifest_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;
        
        if response.status() == 404 {
            return Err(RegistryCacheError::Miss(key.to_string()));
        }
        
        if !response.status().is_success() {
            return Err(RegistryCacheError::Registry(format!(
                "Failed to retrieve manifest: {}",
                response.status()
            )));
        }
        
        let manifest: Manifest = response.json().await?;
        
        // Get config blob
        let config_url = format!(
            "{}/{}/blobs/{}",
            base_url,
            reference.repository,
            manifest.config.digest
        );
        
        let config_response = self.client
            .get(&config_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;
        
        if !config_response.status().is_success() {
            return Err(RegistryCacheError::Registry(format!(
                "Failed to retrieve config: {}",
                config_response.status()
            )));
        }
        
        let config_bytes = config_response.bytes().await?;
        let config: oci_image::config::ImageConfig = serde_json::from_slice(&config_bytes)?;
        
        // Get layer blobs
        let mut layers = Vec::new();
        for layer_desc in &manifest.layers {
            let layer_url = format!(
                "{}/{}/blobs/{}",
                base_url,
                reference.repository,
                layer_desc.digest
            );
            
            let layer_response = self.client
                .get(&layer_url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await?;
            
            if !layer_response.status().is_success() {
                return Err(RegistryCacheError::Registry(format!(
                    "Failed to retrieve layer blob: {}",
                    layer_response.status()
                )));
            }
            
            let data = layer_response.bytes().await?.to_vec();
            let layer = Layer::from_bytes(data, &layer_desc.media_type)?;
            layers.push(layer);
        }
        
        Ok(MutableImage {
            manifest,
            config,
            config_bytes: config_bytes.to_vec(),
            layers,
        })
    }

    /// Check if a cached layer exists for the given key.
    pub async fn exists(&mut self, key: &str) -> Result<bool> {
        match self.retrieve_layer(key).await {
            Ok(_) => Ok(true),
            Err(RegistryCacheError::Miss(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Push a layer to the cache.
    ///
    /// The key is used as the tag name in the cache repository.
    pub async fn push_layer(&mut self, key: &str, image: MutableImage) -> Result<()> {
        let reference = format!("{}:{}", self.cache_repo, key);
        let reference = Reference::parse(&reference)?;
        
        let base_url = self.get_base_url(&reference.registry);
        let token = self.authenticate(&reference.registry, &reference.repository).await?;
        
        // Upload config blob
        let config_url = format!(
            "{}/{}/blobs/uploads/",
            base_url,
            reference.repository
        );
        
        let config_response = self.client
            .post(&config_url)
            .header("Authorization", format!("Bearer {}", token))
            .send()
            .await?;
        
        if !config_response.status().is_success() {
            return Err(RegistryCacheError::Registry(format!(
                "Failed to start config upload: {}",
                config_response.status()
            )));
        }
        
        let upload_url = config_response.headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(&config_url);
        
        let config_digest = image.config_digest().to_string();
        let config_response = self.client
            .put(upload_url)
            .header("Authorization", format!("Bearer {}", token))
            .header(CONTENT_TYPE, "application/octet-stream")
            .header("Content-Length", image.config_bytes.len().to_string())
            .query(&[("digest", &config_digest)])
            .body(image.config_bytes.clone())
            .send()
            .await?;
        
        if !config_response.status().is_success() {
            return Err(RegistryCacheError::Registry(format!(
                "Failed to upload config: {}",
                config_response.status()
            )));
        }
        
        // Upload layer blobs
        let layers_to_upload: Vec<_> = image.layers.iter().map(|layer| {
            (layer.digest().to_string(), layer.data().to_vec())
        }).collect();
        
        for (layer_digest, layer_data) in layers_to_upload {
            let layer_url = format!(
                "{}/{}/blobs/uploads/",
                base_url,
                reference.repository
            );
            
            let layer_response = self.client
                .post(&layer_url)
                .header("Authorization", format!("Bearer {}", token))
                .send()
                .await?;
            
            if !layer_response.status().is_success() {
                return Err(RegistryCacheError::Registry(format!(
                    "Failed to start layer upload: {}",
                    layer_response.status()
                )));
            }
            
            let upload_url = layer_response.headers()
                .get("Location")
                .and_then(|v| v.to_str().ok())
                .unwrap_or(&layer_url);
            
            let layer_response = self.client
                .put(upload_url)
                .header("Authorization", format!("Bearer {}", token))
                .header(CONTENT_TYPE, "application/octet-stream")
                .header("Content-Length", layer_data.len().to_string())
                .query(&[("digest", &layer_digest)])
                .body(layer_data)
                .send()
                .await?;
            
            if !layer_response.status().is_success() {
                return Err(RegistryCacheError::Registry(format!(
                    "Failed to upload layer: {}",
                    layer_response.status()
                )));
            }
        }
        
        // Push manifest
        let manifest_url = format!(
            "{}/{}/manifests/{}",
            base_url,
            reference.repository,
            reference.tag
        );
        
        let manifest_json = serde_json::to_string(&image.manifest)?;
        let manifest_response = self.client
            .put(&manifest_url)
            .header("Authorization", format!("Bearer {}", token))
            .header(CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")
            .body(manifest_json)
            .send()
            .await?;
        
        if !manifest_response.status().is_success() {
            return Err(RegistryCacheError::Registry(format!(
                "Failed to push manifest: {}",
                manifest_response.status()
            )));
        }
        
        tracing::info!("Successfully pushed cache layer with key {} to {}", key, self.cache_repo);
        Ok(())
    }

    /// Get the full reference for a cache key.
    pub fn reference_for_key(&self, key: &str) -> String {
        format!("{}:{}", self.cache_repo, key)
    }

    /// Clear the authentication token cache.
    pub fn clear_auth_cache(&mut self) {
        self.auth_tokens.clear();
    }

    /// Get cache statistics.
    pub fn get_stats(&self) -> CacheStats {
        CacheStats {
            auth_tokens_cached: self.auth_tokens.len(),
            cache_repo: self.cache_repo.clone(),
            insecure: self.insecure,
        }
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of cached authentication tokens
    pub auth_tokens_cached: usize,
    /// Cache repository
    pub cache_repo: String,
    /// Whether using insecure connections
    pub insecure: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_registry_cache_creation() {
        let cache = RegistryCache::new("localhost:5000/cache", true);
        assert_eq!(cache.cache_repo, "localhost:5000/cache");
        assert!(cache.insecure);
    }

    #[tokio::test]
    async fn test_reference_for_key() {
        let cache = RegistryCache::new("gcr.io/project/cache", false);
        let reference = cache.reference_for_key("test-key");
        assert_eq!(reference, "gcr.io/project/cache:test-key");
    }

    #[tokio::test]
    async fn test_extract_auth_param() {
        let cache = RegistryCache::new("localhost:5000/cache", true);
        let auth_header = "Bearer realm=\"https://auth.example.com/token\",service=\"registry.example.com\",scope=\"repository:myrepo:pull\"";
        let realm = cache.extract_auth_param(auth_header, "realm");
        let service = cache.extract_auth_param(auth_header, "service");
        let scope = cache.extract_auth_param(auth_header, "scope");
        
        assert_eq!(realm, Some("https://auth.example.com/token".to_string()));
        assert_eq!(service, Some("registry.example.com".to_string()));
        assert_eq!(scope, Some("repository:myrepo:pull".to_string()));
    }

    #[tokio::test]
    async fn test_cache_stats() {
        let cache = RegistryCache::new("localhost:5000/cache", true);
        let stats = cache.get_stats();
        
        assert_eq!(stats.cache_repo, "localhost:5000/cache");
        assert!(stats.insecure);
        assert_eq!(stats.auth_tokens_cached, 0);
    }

    #[tokio::test]
    async fn test_clear_auth_cache() {
        let mut cache = RegistryCache::new("localhost:5000/cache", true);
        cache.auth_tokens.insert("test".to_string(), "token".to_string());
        
        assert_eq!(cache.auth_tokens.len(), 1);
        cache.clear_auth_cache();
        assert_eq!(cache.auth_tokens.len(), 0);
    }

    #[tokio::test]
    async fn test_get_base_url() {
        let cache = RegistryCache::new("localhost:5000/cache", true);
        assert_eq!(cache.get_base_url("localhost:5000"), "http://localhost:5000/v2");
        
        let cache_secure = RegistryCache::new("gcr.io/project/cache", false);
        assert_eq!(cache_secure.get_base_url("gcr.io"), "https://gcr.io/v2");
    }
}