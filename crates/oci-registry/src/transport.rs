//! HTTP transport with retry logic and TLS configuration for registry operations.
//!
//! Supports:
//! - Per-registry TLS skip/insecure settings
//! - Registry mirrors (pull-through cache)
//! - Custom CA certificates
//! - Exponential backoff retry

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("max retries exceeded for {url}: last error: {last_error}")]
    MaxRetries { url: String, last_error: String },
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error("invalid configuration: {0}")]
    Config(String),
}

/// Registry-specific configuration options.
/// Analogous to Go: `config.RegistryOptions`.
#[derive(Debug, Clone, Default)]
pub struct RegistryOptions {
    /// Registries that should use insecure (HTTP) connections.
    pub insecure_registries: Vec<String>,
    /// Registries where TLS verification should be skipped.
    pub skip_tls_verify_registries: Vec<String>,
    /// Registry mirrors for pull-through caching (e.g. "docker.io" -> "mirror.example.com").
    pub registry_mirrors: HashMap<String, Vec<String>>,
    /// Skip default registry fallback when mirrors don't have the image.
    pub skip_default_registry_fallback: bool,
    /// Path to CA certificates per registry (registry -> cert_path).
    pub registry_certificates: HashMap<String, PathBuf>,
    /// Path to client certificates per registry (registry -> "cert_path,key_path").
    pub registry_client_certificates: HashMap<String, String>,
}

impl RegistryOptions {
    /// Create a new empty RegistryOptions.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if a registry should use insecure (HTTP) connection.
    pub fn is_insecure(&self, registry: &str) -> bool {
        self.insecure_registries.iter().any(|r| r.eq_ignore_ascii_case(registry))
    }

    /// Check if TLS verification should be skipped for a registry.
    pub fn should_skip_tls_verify(&self, registry: &str) -> bool {
        self.skip_tls_verify_registries.iter().any(|r| r.eq_ignore_ascii_case(registry))
    }

    /// Get the mirror URL for a registry, if configured.
    /// Returns the first mirror that is configured.
    /// Analogous to Go: `util.MakeTransport()` mirror resolution.
    pub fn get_mirror(&self, registry: &str) -> Option<&str> {
        self.registry_mirrors
            .get(registry)
            .and_then(|mirrors| mirrors.first())
            .map(|s| s.as_str())
    }

    /// Remap a registry reference to its mirror if configured.
    /// Returns the original reference if no mirror is configured.
    pub fn remap_reference(&self, reference: &str) -> String {
        // Parse "registry/repo:tag" format
        if let Some((host, _rest)) = reference.split_once('/') {
            if let Some(mirror) = self.get_mirror(host) {
                return reference.replacen(host, mirror, 1);
            }
        }
        reference.to_string()
    }
}

/// Retry configuration for HTTP requests.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub initial_backoff_ms: u64,
    pub max_backoff_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 5,
            initial_backoff_ms: 100,
            max_backoff_ms: 5000,
        }
    }
}

impl RetryConfig {
    /// Compute the backoff duration for a given retry attempt (0-based).
    /// Uses exponential backoff: initial * 2^attempt, capped at max.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let backoff_ms = self.initial_backoff_ms * (1 << attempt.min(30));
        let capped = backoff_ms.min(self.max_backoff_ms);
        Duration::from_millis(capped)
    }
}

/// Build a reqwest client with optional TLS verification skip.
pub fn build_client(skip_tls_verify: bool) -> reqwest::Client {
    let mut builder = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10));

    if skip_tls_verify {
        builder = builder.danger_accept_invalid_certs(true);
    }

    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Build a reqwest client with full registry options support.
///
/// This function considers:
/// - Per-registry TLS skip (from `RegistryOptions`)
/// - Custom CA certificates (from `RegistryOptions`)
/// - Custom User-Agent header
/// - `skip_tls_verify` as a fallback
pub fn build_client_with_options(
    skip_tls_verify: bool,
    registry_options: Option<&RegistryOptions>,
    registry: &str,
    user_agent: &str,
) -> reqwest::Client {
    let should_skip_tls = skip_tls_verify
        || registry_options
            .map_or(false, |ro| ro.should_skip_tls_verify(registry));

    let mut builder = reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(user_agent);

    if should_skip_tls {
        builder = builder.danger_accept_invalid_certs(true);
    }

    // Load custom CA certificate if configured for this registry.
    if let Some(ro) = registry_options {
        if let Some(cert_path) = ro.registry_certificates.get(registry) {
            match load_ca_certificate(cert_path) {
                Ok(cert) => {
                    builder = builder.add_root_certificate(cert);
                    tracing::info!(
                        "Loaded custom CA certificate for {} from {:?}",
                        registry,
                        cert_path
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load CA certificate for {} from {:?}: {}",
                        registry,
                        cert_path,
                        e
                    );
                }
            }
        }

        // Load client certificate (mTLS) if configured.
        if let Some(client_cert_spec) = ro.registry_client_certificates.get(registry) {
            match load_client_identity(client_cert_spec) {
                Ok(identity) => {
                    builder = builder.identity(identity);
                    tracing::info!(
                        "Loaded client certificate for {} from {}",
                        registry,
                        client_cert_spec
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load client certificate for {}: {}",
                        registry,
                        e
                    );
                }
            }
        }
    }

    builder.build().unwrap_or_else(|e| {
        tracing::warn!("Failed to build HTTP client with options: {}, using default", e);
        reqwest::Client::new()
    })
}

/// Load a CA certificate from a PEM file.
fn load_ca_certificate(path: &std::path::Path) -> Result<reqwest::Certificate, TransportError> {
    let pem_data = std::fs::read(path).map_err(|e| {
        TransportError::Certificate(format!(
            "failed to read CA certificate from {:?}: {}",
            path, e
        ))
    })?;
    reqwest::Certificate::from_pem(&pem_data).map_err(|e| {
        TransportError::Certificate(format!(
            "failed to parse CA certificate from {:?}: {}",
            path, e
        ))
    })
}

/// Load a client identity (certificate + key) for mTLS.
///
/// The `spec` format is "cert_path,key_path" (comma-separated).
fn load_client_identity(spec: &str) -> Result<reqwest::Identity, TransportError> {
    let parts: Vec<&str> = spec.splitn(2, ',').collect();
    if parts.len() != 2 {
        return Err(TransportError::Config(format!(
            "client certificate spec must be 'cert_path,key_path', got: {}",
            spec
        )));
    }

    let cert_path = parts[0];
    let key_path = parts[1];

    let cert_pem = std::fs::read(cert_path).map_err(|e| {
        TransportError::Certificate(format!(
            "failed to read client certificate from {}: {}",
            cert_path, e
        ))
    })?;
    let key_pem = std::fs::read(key_path).map_err(|e| {
        TransportError::Certificate(format!(
            "failed to read client key from {}: {}",
            key_path, e
        ))
    })?;

    // Combine cert and key into a single PKCS#12 identity.
    // reqwest requires PEM format with both cert and key.
    let mut identity_pem = cert_pem;
    identity_pem.extend_from_slice(&key_pem);

    reqwest::Identity::from_pem(&identity_pem).map_err(|e| {
        TransportError::Certificate(format!(
            "failed to create client identity from {} + {}: {}",
            cert_path, key_path, e
        ))
    })
}

/// Execute an HTTP request with retry logic.
///
/// Retries on connection errors and 5xx server errors.
/// Does not retry on 4xx client errors.
pub async fn retry_request(
    _client: &reqwest::Client,
    request_builder: reqwest::RequestBuilder,
    retry_config: &RetryConfig,
) -> Result<reqwest::Response, TransportError> {
    let mut last_error: Option<String> = None;
    let url_str = extract_url(&request_builder);

    for attempt in 0..=retry_config.max_retries {
        if attempt > 0 {
            let backoff = retry_config.backoff(attempt - 1);
            tracing::debug!(
                "Retry attempt {} for {}, backing off {}ms",
                attempt,
                url_str,
                backoff.as_millis(),
            );
            tokio::time::sleep(backoff).await;
        }

        // We need to rebuild the request each time since RequestBuilder is consumed
        let result = request_builder
            .try_clone()
            .ok_or_else(|| TransportError::MaxRetries {
                url: url_str.clone(),
                last_error: "request cannot be cloned for retry".to_string(),
            })?
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }
                if status.is_client_error() {
                    // 4xx errors — don't retry
                    return Ok(response);
                }
                // 5xx — retry
                last_error = Some(format!("server error: {}", status));
                tracing::warn!("Server error {} on attempt {} for {}", status, attempt, url_str);
            }
            Err(e) => {
                let is_retryable = e.is_connect() || e.is_timeout() || e.is_request();
                last_error = Some(e.to_string());
                if !is_retryable {
                    return Err(TransportError::Http(e));
                }
                tracing::warn!(
                    "Connection error on attempt {} for {}: {}",
                    attempt,
                    url_str,
                    e
                );
            }
        }
    }

    Err(TransportError::MaxRetries {
        url: url_str,
        last_error: last_error.unwrap_or_else(|| "unknown error".to_string()),
    })
}

/// Extract the URL string from a RequestBuilder for logging.
fn extract_url(_builder: &reqwest::RequestBuilder) -> String {
    // Unfortunately RequestBuilder doesn't expose the URL easily,
    // so we use a simple approach: try to inspect the inner URL.
    // Since we can't access it directly, we'll use a fallback.
    "(unknown url)".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_backoff_ms, 100);
        assert_eq!(config.max_backoff_ms, 5000);
    }

    #[test]
    fn test_retry_backoff_calculation() {
        let config = RetryConfig::default();
        // Attempt 0: 100ms
        assert_eq!(config.backoff(0), Duration::from_millis(100));
        // Attempt 1: 200ms
        assert_eq!(config.backoff(1), Duration::from_millis(200));
        // Attempt 2: 400ms
        assert_eq!(config.backoff(2), Duration::from_millis(400));
        // Attempt 3: 800ms
        assert_eq!(config.backoff(3), Duration::from_millis(800));
        // Attempt 4: 1600ms
        assert_eq!(config.backoff(4), Duration::from_millis(1600));
        // Attempt 5: 3200ms
        assert_eq!(config.backoff(5), Duration::from_millis(3200));
        // Attempt 6: capped at 5000ms
        assert_eq!(config.backoff(6), Duration::from_millis(5000));
    }

    #[test]
    fn test_build_client_normal() {
        let client = build_client(false);
        // Should successfully build a client
        assert!(client.get("https://example.com").try_clone().is_some());
    }

    #[test]
    fn test_build_client_skip_tls() {
        let client = build_client(true);
        assert!(client.get("https://example.com").try_clone().is_some());
    }

    #[test]
    fn test_build_client_with_options_basic() {
        let client = build_client_with_options(false, None, "gcr.io", "kaniko/0.1.0");
        assert!(client.get("https://example.com").try_clone().is_some());
    }

    #[test]
    fn test_build_client_with_options_skip_tls() {
        let client = build_client_with_options(true, None, "gcr.io", "kaniko/0.1.0");
        assert!(client.get("https://example.com").try_clone().is_some());
    }

    #[test]
    fn test_build_client_with_options_registry_tls() {
        let mut ro = RegistryOptions::new();
        ro.skip_tls_verify_registries.push("insecure.registry".to_string());
        let client = build_client_with_options(false, Some(&ro), "insecure.registry", "kaniko/0.1.0");
        assert!(client.get("https://example.com").try_clone().is_some());
    }

    #[test]
    fn test_load_ca_certificate_invalid_path() {
        let result = load_ca_certificate(std::path::Path::new("/nonexistent/cert.pem"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_client_identity_invalid_spec() {
        let result = load_client_identity("invalid_no_comma");
        assert!(result.is_err());
        match result {
            Err(TransportError::Config(msg)) => {
                assert!(msg.contains("cert_path,key_path"));
            }
            _ => panic!("Expected Config error"),
        }
    }

    #[test]
    fn test_registry_options_insecure() {
        let mut ro = RegistryOptions::new();
        ro.insecure_registries.push("my.registry".to_string());
        assert!(ro.is_insecure("my.registry"));
        assert!(ro.is_insecure("MY.REGISTRY")); // case insensitive
        assert!(!ro.is_insecure("other.registry"));
    }

    #[test]
    fn test_registry_options_skip_tls() {
        let mut ro = RegistryOptions::new();
        ro.skip_tls_verify_registries.push("skip-tls.registry".to_string());
        assert!(ro.should_skip_tls_verify("skip-tls.registry"));
        assert!(!ro.should_skip_tls_verify("other.registry"));
    }

    #[test]
    fn test_registry_options_mirror() {
        let mut ro = RegistryOptions::new();
        ro.registry_mirrors.insert(
            "docker.io".to_string(),
            vec!["mirror.example.com".to_string()],
        );
        assert_eq!(ro.get_mirror("docker.io"), Some("mirror.example.com"));
        assert_eq!(ro.get_mirror("gcr.io"), None);
    }

    #[test]
    fn test_registry_options_remap_reference() {
        let mut ro = RegistryOptions::new();
        ro.registry_mirrors.insert(
            "docker.io".to_string(),
            vec!["mirror.example.com".to_string()],
        );
        assert_eq!(
            ro.remap_reference("docker.io/library/nginx:latest"),
            "mirror.example.com/library/nginx:latest"
        );
        assert_eq!(
            ro.remap_reference("gcr.io/my-app:v1"),
            "gcr.io/my-app:v1" // no mirror configured
        );
    }
}