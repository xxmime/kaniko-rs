//! Image pull operations — OCI Distribution Spec implementation.
//!
//! Pull flow:
//! 1. Parse reference (registry/repo:tag)
//! 2. Authenticate / obtain Bearer token
//! 3. GET manifest
//! 4. GET config blob
//! 5. GET layer blobs
//! 6. Assemble MutableImage

use crate::auth::RegistryAuth;
use crate::push::Reference;
use crate::transport::{build_client, RetryConfig};
use oci_image::config::ImageConfig;
use oci_image::index::IndexManifest;
use oci_image::layer::Layer;
use oci_image::manifest::{Manifest, MediaType};
use oci_image::mutate::MutableImage;
use std::collections::HashMap;
use std::sync::Mutex;
use once_cell::sync::Lazy;

/// Global manifest cache — avoids re-pulling the same image in multi-stage builds.
/// Analogous to Go: `pkg/image/remote.manifestCache`.
static MANIFEST_CACHE: Lazy<Mutex<HashMap<String, MutableImage>>> = Lazy::new(|| {
    Mutex::new(HashMap::new())
});

/// Errors during pull operations.
#[derive(Debug, thiserror::Error)]
pub enum PullError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
    #[error("authentication error: {0}")]
    Auth(String),
    #[error("pull failed: {0}")]
    Failed(String),
    #[error("transport error: {0}")]
    Transport(#[from] crate::transport::TransportError),
}

/// Result type for pull operations.
pub type Result<T> = std::result::Result<T, PullError>;

/// Pull an image from a registry following the OCI Distribution Spec.
/// Results are cached globally — subsequent pulls for the same reference
/// return the cached image.
/// Analogous to Go: `pkg/image/remote.RetrieveRemoteImage()`.
pub async fn pull_image(
    reference_str: &str,
    auth: &RegistryAuth,
) -> Result<MutableImage> {
    pull_image_with_platform(reference_str, auth, None).await
}

/// Pull an image from a registry, with optional platform selection.
///
/// When a platform is specified (e.g., "linux/amd64") and the registry
/// returns a manifest list, the appropriate platform-specific manifest
/// is selected and used to pull the image.
///
/// Analogous to Go: `remote.Image(ref, remote.WithPlatform(platform))`.
pub async fn pull_image_with_platform(
    reference_str: &str,
    auth: &RegistryAuth,
    platform: Option<&str>,
) -> Result<MutableImage> {
    // Check cache first
    let cache_key = match platform {
        Some(p) => format!("{}@{}", reference_str, p),
        None => reference_str.to_string(),
    };
    if let Ok(cache) = MANIFEST_CACHE.lock() {
        if let Some(cached) = cache.get(&cache_key) {
            tracing::info!("Returning cached image manifest for {}", cache_key);
            return Ok(cached.clone());
        }
    }
    let reference = Reference::parse(reference_str)
        .map_err(|e| PullError::Failed(e.to_string()))?;
    let base_url = reference.base_url(auth.insecure);
    let client = build_client(auth.insecure);

    tracing::info!("Pulling image from {}", reference_str);

    // Step 1: Authenticate
    let token = authenticate_pull(&client, &base_url, &reference.repository, auth).await?;
    let auth_header = if token.is_empty() { String::new() } else { token };

    // Step 2: Get manifest (may be a manifest list)
    let manifest = get_manifest_with_platform(
        &client, &base_url, &reference.repository, &reference.tag, &auth_header, platform,
    ).await?;
    tracing::info!("Got manifest with {} layers", manifest.layers.len());

    // Step 3: Get config blob
    let config_bytes = get_blob(&client, &base_url, &reference.repository, &manifest.config.digest.to_string(), &auth_header).await?;
    let config: ImageConfig = serde_json::from_slice(&config_bytes)?;

    // Step 4: Get layer blobs
    let mut layers = Vec::with_capacity(manifest.layers.len());
    for (i, layer_desc) in manifest.layers.iter().enumerate() {
        tracing::info!("Pulling layer {}/{}: {}", i + 1, manifest.layers.len(), layer_desc.digest);
        let layer_data = get_blob(&client, &base_url, &reference.repository, &layer_desc.digest.to_string(), &auth_header).await?;
        let layer = Layer::from_bytes(layer_data, &layer_desc.media_type)?;
        layers.push(layer);
    }

    tracing::info!("Successfully pulled {} with {} layers", reference_str, layers.len());

    let image = MutableImage {
        manifest,
        config,
        config_bytes,
        layers,
    };

    // Cache the pulled image for future use
    if let Ok(mut cache) = MANIFEST_CACHE.lock() {
        cache.insert(cache_key, image.clone());
    }

    Ok(image)
}

/// Fetch the manifest for a tag/reference, with platform selection.
///
/// If the returned content is a manifest list (index), selects the
/// manifest matching the given platform (e.g., "linux/amd64").
///
/// Analogous to Go: `remote.Get(ref, WithPlatform(platform))`.
async fn get_manifest_with_platform(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    tag: &str,
    auth_header: &str,
    platform: Option<&str>,
) -> Result<Manifest> {
    let url = format!("{}/{}/manifests/{}", base_url, repository, tag);

    // Accept both single manifests and manifest lists
    let accept_header = format!(
        "{}, {}, {}, {}",
        MediaType::OCI_IMAGE_MANIFEST_V1,
        MediaType::IMAGE_MANIFEST_V1S2,
        MediaType::OCI_IMAGE_INDEX_V1,
        MediaType::IMAGE_MANIFEST_LIST_V2S2,
    );

    let resp = client
        .get(&url)
        .header("Authorization", auth_header)
        .header("Accept", &accept_header)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PullError::Failed(format!("manifest pull failed: HTTP {} - {}", status, body)));
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let body = resp.bytes().await?;

    // Check if this is a manifest list / index
    if content_type.contains("manifest.list")
        || content_type.contains("index.v1")
        || content_type.contains("image.index")
    {
        // Parse as OCI Image Index using typed IndexManifest
        let index: IndexManifest = serde_json::from_slice(&body)
            .map_err(|e| PullError::Failed(format!("failed to parse manifest index: {}", e)))?;

        // Parse target platform
        let (target_os, target_arch) = match platform {
            Some(p) => {
                let parts: Vec<&str> = p.split('/').collect();
                let os = parts.first().map(|s| *s).unwrap_or("linux");
                let arch = parts.get(1).map(|s| *s).unwrap_or("amd64");
                (os.to_string(), arch.to_string())
            }
            None => {
                // Default to current platform
                ("linux".to_string(), std::env::consts::ARCH.to_string())
            }
        };

        tracing::info!("Selecting platform {}/{} from manifest list", target_os, target_arch);

        // Find matching manifest descriptor using IndexManifest's find_manifest
        let selected = index.find_manifest(&target_arch, &target_os);
        let digest = selected
            .map(|d| d.digest.to_string())
            .ok_or_else(|| {
                PullError::Failed(format!(
                    "no manifest found for platform {}/{}",
                    target_os, target_arch
                ))
            })?;

        tracing::info!("Selected manifest digest: {}", digest);

        // Fetch the specific manifest by digest
        let url = format!("{}/{}/manifests/{}", base_url, repository, digest);
        let resp = client
            .get(&url)
            .header("Authorization", auth_header)
            .header("Accept", format!("{}, {}", MediaType::OCI_IMAGE_MANIFEST_V1, MediaType::IMAGE_MANIFEST_V1S2))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(PullError::Failed(format!(
                "platform manifest pull failed: HTTP {} - {}",
                status, body_text
            )));
        }

        let manifest: Manifest = resp.json().await?;
        Ok(manifest)
    } else {
        // Not a manifest list — parse as a single manifest
        let manifest: Manifest = serde_json::from_slice(&body)
            .map_err(|e| PullError::Failed(format!("failed to parse manifest: {}", e)))?;
        Ok(manifest)
    }
}

/// Fetch a blob by digest.
async fn get_blob(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    digest: &str,
    auth_header: &str,
) -> Result<Vec<u8>> {
    let url = format!("{}/{}/blobs/{}", base_url, repository, digest);
    let resp = client
        .get(&url)
        .header("Authorization", auth_header)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(PullError::Failed(format!("blob pull failed: HTTP {} - {}", status, body)));
    }

    let bytes = resp.bytes().await?;
    Ok(bytes.to_vec())
}

/// Authenticate for pull operations.
async fn authenticate_pull(
    client: &reqwest::Client,
    base_url: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
    // Try unauthenticated first
    let check_url = format!("{}/{}/blobs/", base_url, repository);
    let resp = client.get(&check_url).send().await;

    match resp {
        Ok(r) => {
            if let Some(www_auth) = r.headers().get("www-authenticate") {
                let www_auth_str = www_auth.to_str()
                    .map_err(|_| PullError::Auth("invalid www-authenticate header".into()))?;
                return obtain_bearer_token_pull(client, www_auth_str, repository, auth).await;
            }
            Ok(String::new())
        }
        Err(_) => {
            if auth.credential.username.is_empty() {
                return Err(PullError::Auth("no credentials available".into()));
            }
            Ok(format!("Basic {}", base64_encode(&format!("{}:{}", auth.credential.username, auth.credential.password))))
        }
    }
}

/// Obtain a Bearer token for pull operations.
async fn obtain_bearer_token_pull(
    client: &reqwest::Client,
    www_authenticate: &str,
    repository: &str,
    auth: &RegistryAuth,
) -> Result<String> {
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
        return Err(PullError::Auth("no realm in WWW-Authenticate".into()));
    }

    let scope = format!("repository:{}:pull", repository);
    let mut url = format!("{}?service={}&scope={}", realm, service, scope);

    if !auth.credential.username.is_empty() {
        url.push_str(&format!("&account={}", auth.credential.username));
    }

    let mut req = client.get(&url);
    if !auth.credential.username.is_empty() {
        let basic = base64_encode(&format!("{}:{}", auth.credential.username, auth.credential.password));
        req = req.header("Authorization", format!("Basic {}", basic));
    }

    let resp = req.send().await.map_err(|e| PullError::Auth(format!("token request failed: {}", e)))?;

    if !resp.status().is_success() {
        return Err(PullError::Auth(format!("token request failed: HTTP {}", resp.status())));
    }

    #[derive(serde::Deserialize)]
    struct TokenResponse {
        token: Option<String>,
        access_token: Option<String>,
    }

    let token_resp: TokenResponse = resp.json().await
        .map_err(|e| PullError::Auth(format!("token parse failed: {}", e)))?;

    let token = token_resp.token.or(token_resp.access_token)
        .ok_or_else(|| PullError::Auth("no token in response".into()))?;

    Ok(format!("Bearer {}", token))
}

fn base64_encode(s: &str) -> String {
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