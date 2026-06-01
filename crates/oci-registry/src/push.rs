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
use crate::reference::{Reference, ReferenceError};
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
    #[error("invalid reference: {0}")]
    Reference(#[from] ReferenceError),
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
    let push_start = std::time::Instant::now();
    let reference = Reference::parse(destination)?;

    // Determine if we should use insecure connection.
    let insecure = opts.insecure
        || opts
            .registry_options
            .as_ref()
            .map_or(false, |ro| ro.is_insecure(&reference.registry));

    let base_url = reference.base_url(insecure);

    tracing::info!(
        "Pushing image to {} (registry={}, repository={}, tag={}, insecure={}, concurrency={})",
        destination,
        reference.registry,
        reference.repository,
        reference.tag,
        insecure,
        opts.max_concurrent_uploads
    );
    tracing::debug!("Push base URL: {}", base_url);

    // Build client with User-Agent and registry-specific TLS settings.
    let client = crate::transport::build_client_with_options(
        insecure,
        opts.registry_options.as_ref(),
        &reference.registry,
        &opts.user_agent,
    );

    let total_layers = image.layers.len();
    let total_layer_bytes: usize = image.layers.iter().map(|l| l.data().len()).sum();
    tracing::info!(
        "Image stats: {} layers, {} bytes total layer data, config {} bytes",
        total_layers,
        total_layer_bytes,
        image.config_bytes.len()
    );

    // Validate manifest-layer consistency before uploading.
    // If manifest references blobs that aren't in image.layers or if
    // manifest.config doesn't match config_bytes, the registry will
    // reject the push with MANIFEST_BLOB_UNKNOWN.
    validate_image_consistency(image)?;

    // Step 1: Authenticate and get Bearer token
    tracing::info!("Authenticating with registry {}...", reference.registry);
    let auth_start = std::time::Instant::now();
    let token = authenticate(&client, &base_url, &reference.repository, auth).await?;
    let auth_elapsed = auth_start.elapsed();
    let auth_type = if token.starts_with("Bearer ") {
        "Bearer token"
    } else if token.is_empty() {
        "none (anonymous)"
    } else {
        "Basic"
    };
    tracing::info!(
        "Authentication succeeded for {} (method={}, elapsed={:.2}s)",
        reference.registry,
        auth_type,
        auth_elapsed.as_secs_f64()
    );
    let auth_header = format_auth_header(&token);

    // Step 2: Upload layer blobs (parallel or sequential)
    tracing::info!("Uploading layers...");
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
    // Always upload the config blob — it's small and some registries
    // return false positives on HEAD requests (claiming the blob exists
    // when it doesn't), which causes MANIFEST_BLOB_UNKNOWN errors.
    // Re-uploading an existing blob is a no-op for the registry.
    let config_digest = image.config_digest().to_string();
    tracing::info!("Uploading config blob: {} ({} bytes)", config_digest, image.config_bytes.len());
    upload_blob(
        &client,
        &base_url,
        &reference.repository,
        &config_digest,
        &image.config_bytes,
        &auth_header,
    )
    .await?;

    // Step 4: Push manifest
    tracing::info!("Pushing manifest to {}/{}/manifests/{} (size={} bytes)",
        base_url, reference.repository, reference.tag, image.config_bytes.len());
    let manifest_json = serde_json::to_vec(&image.manifest)?;
    tracing::debug!("Manifest content size: {} bytes", manifest_json.len());
    let manifest_url = format!(
        "{}/{}/manifests/{}",
        base_url, reference.repository, reference.tag
    );

    let content_type = image
        .manifest
        .media_type
        .as_deref()
        .unwrap_or(MediaType::IMAGE_MANIFEST_V1S2);
    tracing::debug!("Manifest media type: {}", content_type);

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

    let push_elapsed = push_start.elapsed();
    tracing::info!(
        "Successfully pushed {} in {:.2}s ({} layers, {} bytes)",
        destination,
        push_elapsed.as_secs_f64(),
        total_layers,
        total_layer_bytes
    );
    Ok(())
}

/// Upload image layers with configurable concurrency.
///
/// Always uploads all layers without checking existence first.
/// This is more reliable than checking with HEAD requests because:
/// - Some registries (e.g. Alibaba Cloud) return false positives on HEAD
///   (claiming blobs exist when they don't), causing MANIFEST_BLOB_UNKNOWN
/// - OCI spec guarantees blob upload is idempotent (re-uploading is safe)
/// - The upload_blob function handles "already exists" responses gracefully
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

    tracing::info!(
        "Uploading {} layers (max {} concurrent uploads)",
        total,
        max_concurrent
    );

    if max_concurrent <= 1 {
        // Sequential mode: upload one at a time
        for (i, layer) in image.layers.iter().enumerate() {
            let digest = layer.digest().to_string();
            tracing::info!("Uploading layer {}/{}: {} ({} bytes)", i + 1, total, digest, layer.data().len());

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

    // Parallel upload: upload all layers concurrently
    let client = Arc::new(client.clone());
    let base_url = base_url.to_string();
    let repository = repository.to_string();
    let auth_header = auth_header.to_string();

    let mut join_set = JoinSet::new();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut uploaded = 0usize;

    for (i, layer) in image.layers.iter().enumerate() {
        let client = Arc::clone(&client);
        let base_url = base_url.clone();
        let repository = repository.clone();
        let auth_header = auth_header.clone();
        let permit = semaphore.clone();
        let digest = layer.digest().to_string();
        let data = layer.data().to_vec();

        join_set.spawn(async move {
            let _permit = permit.acquire().await.unwrap();
            tracing::info!("Uploading layer {}/{}: {} ({} bytes)", i + 1, total, digest, data.len());
            let result = upload_blob(
                &*client,
                &base_url,
                &repository,
                &digest,
                &data,
                &auth_header,
            )
            .await;
            (i, digest, result)
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
                    total,
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
        "All {} layer uploads complete",
        uploaded
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
    tracing::debug!("Checking blob existence: HEAD {}", digest);
    let resp = client
        .head(&url)
        .header("Authorization", auth_header)
        .send()
        .await?;

    let exists = resp.status().is_success();
    if exists {
        tracing::debug!("Blob {} already exists in registry", digest);
    } else {
        tracing::debug!("Blob {} not found in registry (HTTP {})", digest, resp.status());
    }
    Ok(exists)
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
    let upload_start = std::time::Instant::now();
    let data_len = data.len();
    tracing::debug!("Starting blob upload: digest={}, size={} bytes", digest, data_len);

    // Initiate blob upload session
    let init_url = format!("{}/{}/blobs/uploads/", base_url, repository);
    tracing::debug!("POST {} to initiate blob upload", init_url);
    let resp = client
        .post(&init_url)
        .header("Authorization", auth_header)
        .send()
        .await?;

    // Some registries return 202 Accepted (standard), some return 201 Created.
    // If the blob already exists, some registries return 200 with Location
    // pointing to the existing blob — treat this as success.
    if resp.status().is_success() && !matches!(resp.status().as_u16(), 202 | 201) {
        // Blob might already exist (200 OK) or was created directly (201 Created)
        tracing::debug!(
            "Blob upload init returned HTTP {} (blob may already exist), considering upload complete",
            resp.status()
        );
        let elapsed = upload_start.elapsed();
        tracing::info!(
            "Blob {} upload complete (already existed, HTTP {}) in {:.2}s",
            digest,
            resp.status(),
            elapsed.as_secs_f64()
        );
        return Ok(());
    }

    if !resp.status().is_success() && resp.status().as_u16() != 202 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(
            "Blob upload init failed for {}: HTTP {} - {}",
            digest, status, body
        );
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

    tracing::debug!("Blob upload session created, upload URL: {}", upload_url);

    // Single PUT with digest query parameter.
    // Set a per-request timeout based on data size: minimum 60s, plus 30s per MB.
    // This prevents timeout on slow networks (e.g. cross-region registry uploads)
    // while still having a reasonable upper bound.
    let sep = if upload_url.contains('?') { "&" } else { "?" };
    let put_url = format!("{}{}digest={}", upload_url, sep, digest);
    let upload_timeout = std::time::Duration::from_secs(
        60 + (data_len as u64 / 1_048_576) * 30
    );
    tracing::debug!(
        "Blob upload PUT: {} bytes, timeout={:?}",
        data_len,
        upload_timeout
    );

    let resp = client
        .put(&put_url)
        .timeout(upload_timeout)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", data_len)
        .body(data.to_vec())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        tracing::error!(
            "Blob upload PUT failed for {}: HTTP {} - {}",
            digest, status, body
        );
        return Err(PushError::Failed(format!(
            "blob upload failed: HTTP {} - {}",
            status, body
        )));
    }

    let elapsed = upload_start.elapsed();
    let throughput_mbps = if elapsed.as_secs_f64() > 0.0 {
        (data_len as f64 / 1024.0 / 1024.0) / elapsed.as_secs_f64()
    } else {
        0.0
    };
    tracing::info!(
        "Uploaded blob {} ({} bytes in {:.2}s, {:.2} MB/s)",
        digest,
        data_len,
        elapsed.as_secs_f64(),
        throughput_mbps
    );
    Ok(())
}

/// Authenticate with the registry and obtain a Bearer token.
///
/// Follows the OCI Distribution Spec authentication flow:
/// 1. Check `/v2/` endpoint to determine auth requirements
/// 2. If WWW-Authenticate header present, follow the challenge
/// 3. If we have credentials but no challenge, proactively try to obtain
///    a Bearer token (many registries like Alibaba Cloud return 404 on
///    specific endpoints but still require auth for push operations)
/// 4. If no auth required and no credentials, proceed anonymously
async fn authenticate(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // First, check the /v2/ endpoint to determine auth requirements.
    // Per OCI Distribution Spec, GET /v2/ returns:
    //   - 200: no auth required (or already authenticated)
    //   - 401 with WWW-Authenticate: auth required, follow challenge
    //   - 403: auth required but not permitted
    // Using /v2/ instead of /v2/<repo>/blobs/ because:
    //   - Repositories may not exist yet (push creates them), returning 404
    //   - 404 without WWW-Authenticate was misinterpreted as "no auth needed"
    //   - This caused push failures on Alibaba Cloud and similar registries
    let check_url = format!("{}/", base_url); // e.g., https://registry.example.com/v2/
    tracing::debug!("Checking registry auth requirements: GET {}", check_url);
    let resp = client.get(&check_url).send().await;

    match resp {
        Ok(r) => {
            let status = r.status();
            tracing::debug!("Registry /v2/ check response: HTTP {}", status);

            // Check for WWW-Authenticate header (standard auth challenge)
            if let Some(www_auth) = r.headers().get("www-authenticate") {
                let www_auth_str = www_auth.to_str().map_err(|_| PushError::Auth("invalid www-authenticate header".into()))?;
                tracing::info!(
                    "Registry requires authentication (HTTP {}), scheme: {}",
                    status,
                    www_auth_str.split_whitespace().next().unwrap_or("unknown")
                );
                tracing::debug!("WWW-Authenticate header: {}", www_auth_str);
                return obtain_bearer_token(client, www_auth_str, repository, auth).await;
            }

            if status == reqwest::StatusCode::UNAUTHORIZED {
                // Got 401 but no WWW-Authenticate header — likely bad credentials
                if auth.credential.is_anonymous() {
                    tracing::warn!("Registry returned 401 Unauthorized — no credentials provided. \
                                     Check your Docker config.json or use --docker-config flag.");
                } else {
                    tracing::warn!(
                        "Registry returned 401 Unauthorized for user '{}' — credentials may be invalid or expired",
                        auth.credential.username
                    );
                }
                return Err(PushError::Auth(format!(
                    "HTTP 401 Unauthorized — credentials may be invalid for registry {}",
                    auth.registry
                )));
            }

            if status.is_success() {
                // 200 OK on /v2/ — no auth required for the registry itself.
                // However, the repository may still require auth for push.
                // If we have credentials, proactively obtain a Bearer token
                // to ensure push operations succeed.
                if !auth.credential.is_anonymous() {
                    tracing::info!(
                        "Registry /v2/ returned 200 (no global auth), but credentials available — \
                         proactively obtaining Bearer token for push operations"
                    );
                    // Try to get a token by probing the repository-specific endpoint
                    return proactive_auth(client, base_url, repository, auth).await;
                }
                tracing::info!("Registry {} does not require authentication (HTTP {})", auth.registry, status);
                Ok(String::new())
            } else {
                // Other status codes (403, 404, etc.) — try with credentials if available
                tracing::debug!(
                    "Registry /v2/ returned HTTP {} — trying with credentials if available",
                    status
                );
                if !auth.credential.is_anonymous() {
                    return proactive_auth(client, base_url, repository, auth).await;
                }
                // No credentials, and registry returned non-success — proceed anyway
                // (some registries return unexpected status codes but still work)
                tracing::warn!(
                    "Registry /v2/ returned HTTP {} and no credentials available",
                    status
                );
                Ok(String::new())
            }
        }
        Err(e) => {
            // If we can't reach the registry without auth, try with credentials
            tracing::debug!("Registry unreachable without auth: {}", e);
            if auth.credential.username.is_empty() {
                return Err(PushError::Auth(format!(
                    "no credentials available for registry {}: {}",
                    auth.registry, e
                )));
            }
            tracing::info!("Using Basic auth for registry {} (direct fallback)", auth.registry);
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

/// Proactively authenticate with a registry by requesting a repository-scoped
/// token, even when the /v2/ endpoint doesn't return a WWW-Authenticate challenge.
///
/// This is needed for registries (like Alibaba Cloud, AWS ECR, etc.) that:
/// - Return 200 on /v2/ (no auth challenge)
/// - But require Bearer tokens for push/pull operations on specific repositories
///
/// The strategy is:
/// 1. Try GET /v2/<repo>/blobs/ with Basic auth to trigger a WWW-Authenticate
/// 2. If that returns a challenge, follow it to get a Bearer token
/// 3. If no challenge, fall back to Basic auth with the provided credentials
async fn proactive_auth(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // Try to trigger an auth challenge by hitting a repository-specific endpoint
    // with Basic auth. Many registries will respond with 401 + WWW-Authenticate
    // even if the /v2/ endpoint returned 200.
    let probe_url = format!("{}/{}/blobs/", base_url, repository);
    tracing::debug!("Probing repository endpoint for auth challenge: GET {}", probe_url);

    let basic_header = format!(
        "Basic {}",
        base64_encode(&format!(
            "{}:{}",
            auth.credential.username, auth.credential.password
        ))
    );

    let resp = client
        .get(&probe_url)
        .header("Authorization", &basic_header)
        .send()
        .await;

    match resp {
        Ok(r) => {
            let status = r.status();
            tracing::debug!("Repository probe response: HTTP {}", status);

            // Check for WWW-Authenticate header
            if let Some(www_auth) = r.headers().get("www-authenticate") {
                let www_auth_str = www_auth.to_str()
                    .map_err(|_| PushError::Auth("invalid www-authenticate header".into()))?;
                tracing::info!(
                    "Repository requires Bearer auth (HTTP {}), scheme: {}",
                    status,
                    www_auth_str.split_whitespace().next().unwrap_or("unknown")
                );
                return obtain_bearer_token(client, www_auth_str, repository, auth).await;
            }

            if status == reqwest::StatusCode::UNAUTHORIZED {
                // 401 without WWW-Authenticate — fall back to Basic auth
                tracing::info!("Registry returned 401 without WWW-Authenticate, using Basic auth");
                return Ok(basic_header);
            }

            // If we got here with any status, the Basic auth might work directly
            // (some registries accept Basic auth on all endpoints)
            tracing::info!(
                "Repository endpoint returned HTTP {} — using Basic auth for push",
                status
            );
            Ok(basic_header)
        }
        Err(e) => {
            tracing::debug!("Repository probe failed: {} — falling back to Basic auth", e);
            Ok(basic_header)
        }
    }
}

/// Parse WWW-Authenticate header and obtain a Bearer token.
///
/// Parses headers in the format:
///   `Bearer realm="https://auth.example.com/token",service="registry.example.com",scope="..."`
///
/// Also handles the `Basic realm="..."` format by falling back to Basic auth.
async fn obtain_bearer_token(
    client: &reqwest::Client,
    www_authenticate: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // Strip the auth scheme prefix (e.g., "Bearer " or "Basic ")
    let params_str = if let Some(stripped) = www_authenticate.strip_prefix("Bearer ") {
        stripped
    } else if let Some(stripped) = www_authenticate.strip_prefix("bearer ") {
        stripped
    } else if www_authenticate.starts_with("Basic ") || www_authenticate.starts_with("basic ") {
        // Basic auth challenge — use credentials directly
        tracing::info!("Registry uses Basic auth, using credentials directly");
        if auth.credential.is_anonymous() {
            return Err(PushError::Auth("Basic auth required but no credentials available".into()));
        }
        return Ok(format!(
            "Basic {}",
            base64_encode(&format!(
                "{}:{}",
                auth.credential.username, auth.credential.password
            ))
        ));
    } else {
        // Try to find the first space (scheme boundary)
        www_authenticate.split_whitespace().nth(1).unwrap_or("")
    };

    // Parse key="value" pairs from the remaining parameters.
    // Handles various formats:
    //   realm="https://auth.example.com/token",service="registry.example.com"
    //   realm="https://auth.example.com/token",service="registry.example.com",scope="..."
    let mut realm = String::new();
    let mut service = String::new();

    for part in params_str.split(',') {
        let part = part.trim();
        // Handle key="value" format
        if let Some(eq_pos) = part.find('=') {
            let key = part[..eq_pos].trim();
            let value_part = part[eq_pos + 1..].trim();
            // Strip surrounding quotes
            let value = value_part
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(value_part);

            match key {
                "realm" => realm = value.to_string(),
                "service" => service = value.to_string(),
                _ => {} // ignore other keys (scope, etc.)
            }
        }
    }

    if realm.is_empty() {
        tracing::error!(
            "Failed to parse realm from WWW-Authenticate header: {}",
            www_authenticate
        );
        return Err(PushError::Auth("no realm in WWW-Authenticate".into()));
    }

    let scope = format!("repository:{}:push,pull", repository);
    let mut url = format!("{}?service={}&scope={}", realm, service, scope);

    tracing::info!(
        "Requesting Bearer token from realm={} (service={}, scope={})",
        realm, service, scope
    );

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
        let status = resp.status();
        tracing::error!(
            "Bearer token request failed: HTTP {} for realm={}",
            status, realm
        );
        return Err(PushError::Auth(format!("token request failed: HTTP {}", status)));
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

/// Validate that the image's manifest is consistent with its data.
///
/// Checks:
/// 1. Every layer digest in `manifest.layers` must exist in `image.layers`
/// 2. `manifest.config.digest` must match `config_bytes`'s actual digest
///
/// Returns an error if inconsistencies are found, preventing a push that
/// would fail with MANIFEST_BLOB_UNKNOWN.
fn validate_image_consistency(image: &MutableImage) -> Result<()> {
    // Check 1: manifest.config.digest must match the actual config bytes digest
    let actual_config_digest = image.config_digest();
    if image.manifest.config.digest != actual_config_digest {
        tracing::error!(
            "Manifest config digest mismatch: manifest says {}, but config_bytes hash is {}. \
             This will cause MANIFEST_BLOB_UNKNOWN. The config was likely modified without \
             updating the manifest descriptor.",
            image.manifest.config.digest,
            actual_config_digest
        );
        return Err(PushError::Failed(format!(
            "manifest config descriptor is stale: manifest.digest={} but actual={}. \
             Config was modified without updating manifest.config. \
             Call recalculate_config_descriptor() after config changes.",
            image.manifest.config.digest,
            actual_config_digest
        )));
    }

    // Check 2: every layer in manifest must have a matching layer in image.layers
    let layer_digests: std::collections::HashSet<String> = image
        .layers
        .iter()
        .map(|l| l.digest().to_string())
        .collect();

    for (i, desc) in image.manifest.layers.iter().enumerate() {
        let desc_digest = desc.digest.to_string();
        if !layer_digests.contains(&desc_digest) {
            tracing::error!(
                "Manifest layer {} references digest {} which is not in image.layers. \
                 This will cause MANIFEST_BLOB_UNKNOWN. \
                 manifest has {} layers but image.layers has {} entries.",
                i,
                desc_digest,
                image.manifest.layers.len(),
                image.layers.len()
            );
            return Err(PushError::Failed(format!(
                "manifest layer {} (digest={}) has no matching data in image.layers. \
                 manifest.layers.len()={} but image.layers.len()={}. \
                 Layers may have been lost during the build.",
                i,
                desc_digest,
                image.manifest.layers.len(),
                image.layers.len()
            )));
        }
    }

    // Check 3: image.layers should not have more entries than manifest.layers
    // (a layer was added to image.layers but not to the manifest)
    if image.layers.len() > image.manifest.layers.len() {
        tracing::warn!(
            "image.layers has {} entries but manifest.layers has {}. \
             Extra layers in image.layers will be uploaded but not referenced in the manifest.",
            image.layers.len(),
            image.manifest.layers.len()
        );
    }

    tracing::debug!(
        "Image consistency check passed: {} layers in manifest, {} in image.layers, config digest matches",
        image.manifest.layers.len(),
        image.layers.len()
    );
    Ok(())
}