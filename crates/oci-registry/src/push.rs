//! Image push operations — OCI Distribution Spec implementation.
//!
//! Push flow:
//! 1. Parse destination reference (registry/repo:tag)
//! 2. Authenticate / obtain Bearer token
//! 3. Check if blobs already exist (HEAD /v2/<name>/blobs/<digest>)
//! 4. Upload missing blobs (POST + PUT chunked upload)
//! 5. Upload config blob
//! 6. Push manifest (PUT /v2/<name>/manifests/<reference>)

use crate::auth::RegistryAuth;
use crate::transport::{build_client, RetryConfig};
use oci_image::manifest::MediaType;
use oci_image::mutate::MutableImage;
use thiserror::Error;

/// Errors during push operations.
#[derive(Debug, Error)]
pub enum PushError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("push failed: {0}")]
    Failed(String),
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
}

/// Result type for push operations.
pub type Result<T> = std::result::Result<T, PushError>;

/// Parsed reference: registry / repository / tag.
#[derive(Debug, Clone)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

impl Reference {
    /// Parse a reference string like "gcr.io/my-project/my-app:latest".
    pub fn parse(reference: &str) -> Result<Self> {
        let (registry, rest) = reference
            .split_once('/')
            .ok_or_else(|| PushError::Failed(format!("invalid reference: {}", reference)))?;
        let (repository, tag) = if let Some((repo, t)) = rest.rsplit_once(':') {
            (repo.to_string(), t.to_string())
        } else {
            (rest.to_string(), "latest".to_string())
        };
        Ok(Self {
            registry: registry.to_string(),
            repository,
            tag,
        })
    }

    /// Base URL for the v2 API.
    pub fn base_url(&self, insecure: bool) -> String {
        let scheme = if insecure { "http" } else { "https" };
        format!("{}://{}/v2", scheme, self.registry)
    }
}

/// Push an image to a registry following the OCI Distribution Spec.
pub async fn push_image(
    image: &MutableImage,
    destination: &str,
    auth: &RegistryAuth,
) -> Result<()> {
    let reference = Reference::parse(destination)?;
    let base_url = reference.base_url(auth.insecure);
    let client = build_client(auth.insecure);
    let _retry = RetryConfig::default();

    tracing::info!("Pushing image to {}", destination);

    // Step 1: Authenticate and get Bearer token
    let token = authenticate(&client, &base_url, &reference.repository, auth).await?;
    let auth_header = format_auth_header(&token);

    // Step 2: Upload layer blobs
    for (i, layer) in image.layers.iter().enumerate() {
        let digest = layer.digest().to_string();
        tracing::info!("Uploading layer {}/{}: {}", i + 1, image.layers.len(), digest);

        // Check if blob already exists
        if blob_exists(&client, &base_url, &reference.repository, &digest, &auth_header).await? {
            tracing::debug!("Blob {} already exists, skipping upload", digest);
            continue;
        }

        // Upload blob
        upload_blob(
            &client,
            &base_url,
            &reference.repository,
            &digest,
            layer.data(),
            &auth_header,
        )
        .await?;
    }

    // Step 3: Upload config blob
    let config_digest = image.config_digest().to_string();
    tracing::info!("Uploading config: {}", config_digest);

    if !blob_exists(&client, &base_url, &reference.repository, &config_digest, &auth_header).await? {
        upload_blob(
            &client,
            &base_url,
            &reference.repository,
            &config_digest,
            &image.config_bytes,
            &auth_header,
        )
        .await?;
    }

    // Step 4: Push manifest
    tracing::info!("Pushing manifest with tag: {}", reference.tag);
    let manifest_json = serde_json::to_vec(&image.manifest)?;
    let manifest_url = format!(
        "{}/{}/manifests/{}",
        base_url, reference.repository, reference.tag
    );

    let content_type = image.manifest.media_type.as_deref().unwrap_or(MediaType::IMAGE_MANIFEST_V1S2);
    let resp = client
        .put(&manifest_url)
        .header("Authorization", &auth_header)
        .header("Content-Type", content_type)
        .body(manifest_json)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PushError::Failed(format!(
            "manifest push failed: HTTP {} - {}",
            status, body
        )));
    }

    tracing::info!("Successfully pushed {}", destination);
    Ok(())
}

/// Check if a blob already exists in the registry (HEAD request).
async fn blob_exists(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    digest: &str,
    auth_header: &str,
) -> Result<bool> {
    let url = format!("{}/{}/blobs/{}", base_url, repository, digest);
    let resp = client
        .head(&url)
        .header("Authorization", auth_header)
        .send()
        .await?;

    Ok(resp.status().is_success())
}

/// Upload a blob to the registry (POST + PUT single PUT).
async fn upload_blob(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    digest: &str,
    data: &[u8],
    auth_header: &str,
) -> Result<()> {
    // Initiate blob upload session
    let init_url = format!("{}/{}/blobs/uploads/", base_url, repository);
    let resp = client
        .post(&init_url)
        .header("Authorization", auth_header)
        .send()
        .await?;

    if !resp.status().is_success() && resp.status().as_u16() != 202 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PushError::Failed(format!(
            "blob upload init failed: HTTP {} - {}",
            status, body
        )));
    }

    // Get upload location from Location header
    let upload_url = resp
        .headers()
        .get("Location")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| PushError::Failed("no Location header in upload init response".into()))?;

    // Single PUT with digest query parameter
    let sep = if upload_url.contains('?') { "&" } else { "?" };
    let put_url = format!("{}{}digest={}", upload_url, sep, digest);

    let resp = client
        .put(&put_url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", data.len())
        .body(data.to_vec())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PushError::Failed(format!(
            "blob upload failed: HTTP {} - {}",
            status, body
        )));
    }

    tracing::debug!("Uploaded blob {} ({} bytes)", digest, data.len());
    Ok(())
}

/// Authenticate with the registry and obtain a Bearer token.
async fn authenticate(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // First, try an unauthenticated request to see if we need auth
    let check_url = format!("{}/{}/blobs/", base_url, repository);
    let resp = client.get(&check_url).send().await;

    match resp {
        Ok(r) => {
            // Check for WWW-Authenticate header
            if let Some(www_auth) = r.headers().get("www-authenticate") {
                let www_auth_str = www_auth.to_str().map_err(|_| PushError::Auth("invalid www-authenticate header".into()))?;
                return obtain_bearer_token(client, www_auth_str, repository, auth).await;
            }
            // No auth required
            Ok(String::new())
        }
        Err(_) => {
            // If we can't reach the registry without auth, try with credentials
            if auth.credential.username.is_empty() {
                return Err(PushError::Auth("no credentials available".into()));
            }
            // Basic auth
            Ok(format!(
                "Basic {}",
                base64_encode(&format!(
                    "{}:{}",
                    auth.credential.username, auth.credential.password
                ))
            ))
        }
    }
}

/// Parse WWW-Authenticate header and obtain a Bearer token.
async fn obtain_bearer_token(
    client: &reqwest::Client,
    www_authenticate: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // Parse: Bearer realm="...",service="...",scope="..."
    let mut realm = String::new();
    let mut service = String::new();

    for part in www_authenticate.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("realm=\"") {
            realm = val.trim_end_matches('"').to_string();
        } else if let Some(val) = part.strip_prefix("service=\"") {
            service = val.trim_end_matches('"').to_string();
        }
    }

    if realm.is_empty() {
        return Err(PushError::Auth("no realm in WWW-Authenticate".into()));
    }

    let scope = format!("repository:{}:push,pull", repository);
    let mut url = format!("{}?service={}&scope={}", realm, service, scope);

    // Add credentials if available
    if !auth.credential.username.is_empty() {
        url.push_str(&format!("&account={}", auth.credential.username));
    }

    let mut req = client.get(&url);
    if !auth.credential.username.is_empty() {
        let basic = base64_encode(&format!(
            "{}:{}",
            auth.credential.username, auth.credential.password
        ));
        req = req.header("Authorization", format!("Basic {}", basic));
    }

    let resp = req.send().await.map_err(|e| PushError::Auth(format!("token request failed: {}", e)))?;

    if !resp.status().is_success() {
        return Err(PushError::Auth(format!("token request failed: HTTP {}", resp.status())));
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        token: Option<String>,
        access_token: Option<String>,
    }

    let token_resp: TokenResponse = resp.json().await
        .map_err(|e| PushError::Auth(format!("token parse failed: {}", e)))?;

    let token = token_resp.token.or(token_resp.access_token)
        .ok_or_else(|| PushError::Auth("no token in response".into()))?;

    Ok(format!("Bearer {}", token))
}

fn format_auth_header(token: &str) -> String {
    if token.is_empty() { String::new() } else { token.to_string() }
}

fn base64_encode(s: &str) -> String {
    // Simple base64 encoding without external dependency
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = s.as_bytes();
    let mut result = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARSET[((n >> 18) & 0x3F) as usize] as char);
        result.push(CHARSET[((n >> 12) & 0x3F) as usize] as char);
        result.push(if chunk.len() > 1 { CHARSET[((n >> 6) & 0x3F) as usize] as char } else { '=' });
        result.push(if chunk.len() > 2 { CHARSET[(n & 0x3F) as usize] as char } else { '=' });
    }
    result
}