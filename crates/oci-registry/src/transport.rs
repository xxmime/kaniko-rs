//! HTTP transport with retry logic and TLS configuration for registry operations.

use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("max retries exceeded for {url}: last error: {last_error}")]
    MaxRetries { url: String, last_error: String },
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
}