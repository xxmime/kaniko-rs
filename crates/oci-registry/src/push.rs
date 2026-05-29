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

/// User-Agent header value sent with all registry requests.
/// Analogous to Go: `go-containerregistry` transport UserAgent.
pub const USER_AGENT: &str = concat!("kaniko/", env!("CARGO_PKG_VERSION"));

/// Options for controlling push behavior.
#[derive(Debug, Clone)]
pub struct PushOptions {
    /// Maximum number of concurrent layer uploads.
    /// Defaults to 4. Set to 1 for sequential uploads.
    pub max_concurrent_uploads: usize,
    /// Whether to skip TLS verification.
    pub insecure: bool,
    /// Whether to ignore errors from pushing to an immutable tag.
    /// Some registries (e.g. ECR) return errors when pushing to a tag
    /// that already exists and is immutable. When true, these errors
    /// are logged as warnings instead of failing the push.
    /// Analogous to Go: `opts.PushIgnoreImmutableTagErrors`.
    pub ignore_immutable_tag_errors: bool,
    /// Custom User-Agent header. Defaults to `kaniko/<version>`.
    pub user_agent: String,
    /// Registry-specific options (TLS, mirrors, certificates).
    pub registry_options: Option<crate::transport::RegistryOptions>,
}

impl Default for PushOptions {
    fn default() -> Self {
        Self {
            max_concurrent_uploads: 4,
            insecure: false,
            ignore_immutable_tag_errors: false,
            user_agent: USER_AGENT.to_string(),
            registry_options: None,
        }
    }
}

impl PushOptions {
    /// Create options for sequential (non-parallel) uploads.
    pub fn sequential() -> Self {
        Self {
            max_concurrent_uploads: 1,
            ..Default::default()
        }
    }

    /// Create options with a specific concurrency limit.
    pub fn with_concurrency(max_concurrent: usize) -> Self {
        Self {
            max_concurrent_uploads: max_concurrent,
            ..Default::default()
        }
    }

    /// Set whether to ignore immutable tag errors.
    pub fn with_ignore_immutable_tag_errors(mut self, ignore: bool) -> Self {
        self.ignore_immutable_tag_errors = ignore;
        self
    }

    /// Set a custom User-Agent header.
    pub fn with_user_agent(mut self, ua: &str) -> Self {
        self.user_agent = ua.to_string();
        self
    }

    /// Set registry-specific options.
    pub fn with_registry_options(mut self, opts: crate::transport::RegistryOptions) -> Self {
        self.registry_options = Some(opts);
        self
    }
}

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
    push_image_with_options(image, destination, auth, PushOptions::default()).await
}

/// Push an image to a registry with configurable options.
///
/// Uses `PushOptions` to control concurrency and other behavior.
/// With `max_concurrent_uploads > 1`, layers are uploaded in parallel
/// using tokio concurrent tasks.
pub async fn push_image_with_options(
    image: &MutableImage,
    destination: &str,
    auth: &RegistryAuth,
    opts: PushOptions,
) -> Result<()> {
    let reference = Reference::parse(destination)?;

    // Determine if we should use insecure connection.
    let insecure = opts.insecure
        || opts
            .registry_options
            .as_ref()
            .map_or(false, |ro| ro.is_insecure(&reference.registry));

    let base_url = reference.base_url(insecure);

    // Build client with User-Agent and registry-specific TLS settings.
    let client = crate::transport::build_client_with_options(
        insecure,
        opts.registry_options.as_ref(),
        &reference.registry,
        &opts.user_agent,
    );

    tracing::info!(
        "Pushing image to {} (concurrency: {})",
        destination,
        opts.max_concurrent_uploads
    );

    // Step 1: Authenticate and get Bearer token
    let token = authenticate(&client, &base_url, &reference.repository, auth).await?;
    let auth_header = format_auth_header(&token);

    // Step 2: Upload layer blobs (parallel or sequential)
    push_layers_concurrent(
        &client,
        &base_url,
        &reference.repository,
        &auth_header,
        image,
        opts.max_concurrent_uploads,
    )
    .await?;

    // Step 3: Upload config blob
    let config_digest = image.config_digest().to_string();
    tracing::info!("Uploading config: {}", config_digest);

    if !blob_exists(&client, &base_url, &reference.repository, &config_digest, &auth_header).await?
    {
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

    let content_type = image
        .manifest
        .media_type
        .as_deref()
        .unwrap_or(MediaType::IMAGE_MANIFEST_V1S2);
    let resp = client
        .put(&manifest_url)
        .header("Authorization", &auth_header)
        .header("Content-Type", content_type)
        .header("User-Agent", &opts.user_agent)
        .body(manifest_json)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        // Handle immutable tag errors.
        // Some registries (e.g. ECR, GAR) return 400/409/405 when pushing
        // to a tag that already exists and is immutable.
        if opts.ignore_immutable_tag_errors && is_immutable_tag_error(status.as_u16(), &body) {
            tracing::warn!(
                "Tag {} is immutable in the destination registry, \
                 ignoring error as requested: HTTP {} - {}",
                reference.tag,
                status,
                body
            );
            return Ok(());
        }

        return Err(PushError::Failed(format!(
            "manifest push failed: HTTP {} - {}",
            status, body
        )));
    }

    tracing::info!("Successfully pushed {}", destination);
    Ok(())
}

/// Upload image layers with configurable concurrency.
///
/// When `max_concurrent > 1`, layers that don't already exist in the
/// registry are uploaded in parallel using tokio::JoinSet.
/// This significantly improves push speed for images with many layers.
async fn push_layers_concurrent(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    auth_header: &str,
    image: &MutableImage,
    max_concurrent: usize,
) -> Result<()> {
    use tokio::task::JoinSet;
    use std::sync::Arc;

    let total = image.layers.len();
    if total == 0 {
        return Ok(());
    }

    if max_concurrent <= 1 {
        // Sequential mode: upload one at a time
        for (i, layer) in image.layers.iter().enumerate() {
            let digest = layer.digest().to_string();
            tracing::info!("Uploading layer {}/{}: {}", i + 1, total, digest);

            if blob_exists(client, base_url, repository, &digest, auth_header).await? {
                tracing::debug!("Blob {} already exists, skipping upload", digest);
                continue;
            }

            upload_blob(
                client,
                base_url,
                repository,
                &digest,
                layer.data(),
                auth_header,
            )
            .await?;
        }
        return Ok(());
    }

    // Parallel mode: check which layers need uploading, then upload concurrently
    tracing::info!(
        "Checking {} layers for existence (max {} concurrent uploads)",
        total,
        max_concurrent
    );

    // First pass: check existence (sequential, fast HEAD requests)
    let mut layers_to_upload: Vec<(usize, String, Vec<u8>)> = Vec::new();
    for (i, layer) in image.layers.iter().enumerate() {
        let digest = layer.digest().to_string();
        if blob_exists(client, base_url, repository, &digest, auth_header).await? {
            tracing::debug!("Layer {}/{}: {} already exists", i + 1, total, digest);
        } else {
            layers_to_upload.push((i, digest, layer.data().to_vec()));
        }
    }

    let to_upload_count = layers_to_upload.len();
    if to_upload_count == 0 {
        tracing::info!("All {} layers already exist in registry", total);
        return Ok(());
    }

    tracing::info!(
        "Uploading {}/{} layers in parallel (max {} concurrent)",
        to_upload_count,
        total,
        max_concurrent
    );

    // Parallel upload using JoinSet with semaphore-like limiting
    let client = Arc::new(client.clone());
    let base_url = base_url.to_string();
    let repository = repository.to_string();
    let auth_header = auth_header.to_string();

    let mut join_set = JoinSet::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut uploaded = 0usize;

    for (idx, digest, data) in layers_to_upload {
        let client = Arc::clone(&client);
        let base_url = base_url.clone();
        let repository = repository.clone();
        let auth_header = auth_header.clone();
        let permit = semaphore.clone();

        join_set.spawn(async move {
            let _permit = permit.acquire().await.unwrap();
            tracing::info!("Uploading layer {}/{}: {}", idx + 1, total, digest);
            let result = upload_blob(
                &*client,
                &base_url,
                &repository,
                &digest,
                &data,
                &auth_header,
            )
            .await;
            (idx, digest, result)
        });
    }

    // Collect results
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((_idx, digest, Ok(()))) => {
                uploaded += 1;
                tracing::info!(
                    "Layer upload {}/{} complete: {}",
                    uploaded,
                    to_upload_count,
                    digest
                );
            }
            Ok((idx, digest, Err(e))) => {
                return Err(PushError::Failed(format!(
                    "layer {} ({}) upload failed: {}",
                    idx + 1,
                    digest,
                    e
                )));
            }
            Err(e) => {
                return Err(PushError::Failed(format!(
                    "layer upload task panicked: {}",
                    e
                )));
            }
        }
    }

    tracing::info!(
        "All {}/{} layer uploads complete",
        uploaded,
        to_upload_count
    );
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

/// Check if the HTTP response indicates an immutable tag error.
///
/// Different registries use different status codes and error messages:
/// - ECR: 400 with "IMMUTABLE_TAG" in the body
/// - GAR: 409 with "TAG_IMMUTABLE" in the body
/// - Generic: 405 Method Not Allowed for immutable tags
fn is_immutable_tag_error(status_code: u16, body: &str) -> bool {
    match status_code {
        400 | 409 | 405 => {
            let body_upper = body.to_uppercase();
            body_upper.contains("IMMUTABLE") || body_upper.contains("TAG_ALREADY_EXISTS")
        }
        _ => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_parse() {
        let ref1 = Reference::parse("gcr.io/my-project/my-app:latest").unwrap();
        assert_eq!(ref1.registry, "gcr.io");
        assert_eq!(ref1.repository, "my-project/my-app");
        assert_eq!(ref1.tag, "latest");

        let ref2 = Reference::parse("localhost:5000/my-app:v1").unwrap();
        assert_eq!(ref2.registry, "localhost:5000");
        assert_eq!(ref2.repository, "my-app");
        assert_eq!(ref2.tag, "v1");

        let ref3 = Reference::parse("registry.example.com/app").unwrap();
        assert_eq!(ref3.tag, "latest"); // default tag
    }

    #[test]
    fn test_reference_base_url() {
        let ref1 = Reference::parse("gcr.io/my-app:latest").unwrap();
        assert_eq!(ref1.base_url(false), "https://gcr.io/v2");
        assert_eq!(ref1.base_url(true), "http://gcr.io/v2");
    }

    #[test]
    fn test_push_options_default() {
        let opts = PushOptions::default();
        assert_eq!(opts.max_concurrent_uploads, 4);
        assert!(!opts.insecure);
    }

    #[test]
    fn test_push_options_sequential() {
        let opts = PushOptions::sequential();
        assert_eq!(opts.max_concurrent_uploads, 1);
    }

    #[test]
    fn test_push_options_with_concurrency() {
        let opts = PushOptions::with_concurrency(8);
        assert_eq!(opts.max_concurrent_uploads, 8);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode("user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64_encode("hello"), "aGVsbG8=");
    }

    #[test]
    fn test_format_auth_header() {
        assert_eq!(format_auth_header(""), "");
        assert_eq!(format_auth_header("Bearer abc123"), "Bearer abc123");
    }

    #[test]
    fn test_immutable_tag_error_detection() {
        // ECR-style: 400 with IMMUTABLE_TAG
        assert!(is_immutable_tag_error(400, r#"{"errors":[{"code":"IMMUTABLE_TAG"}]}"#));
        // GAR-style: 409 with TAG_IMMUTABLE
        assert!(is_immutable_tag_error(409, "TAG_IMMUTABLE: tag is immutable"));
        // Generic: 405 Method Not Allowed
        assert!(is_immutable_tag_error(405, "tag_already_exists: the tag already exists"));
        // Case insensitive
        assert!(is_immutable_tag_error(400, "immutable_tag: tag cannot be overwritten"));
        // Non-immutable errors
        assert!(!is_immutable_tag_error(400, "invalid manifest"));
        assert!(!is_immutable_tag_error(401, "IMMUTABLE")); // wrong status code
        assert!(!is_immutable_tag_error(404, "not found"));
        assert!(!is_immutable_tag_error(500, "internal server error"));
    }

    #[test]
    fn test_push_options_with_ignore_immutable() {
        let opts = PushOptions::default().with_ignore_immutable_tag_errors(true);
        assert!(opts.ignore_immutable_tag_errors);
    }

    #[test]
    fn test_push_options_with_user_agent() {
        let opts = PushOptions::default().with_user_agent("kaniko-test/1.0");
        assert_eq!(opts.user_agent, "kaniko-test/1.0");
    }

    #[test]
    fn test_push_options_with_registry_options() {
        let ro = crate::transport::RegistryOptions::new();
        let opts = PushOptions::default().with_registry_options(ro);
        assert!(opts.registry_options.is_some());
    }

    #[test]
    fn test_user_agent_constant() {
        assert!(USER_AGENT.starts_with("kaniko/"));
    }
}