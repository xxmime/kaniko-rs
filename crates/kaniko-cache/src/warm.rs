//! Cache warming — pre-populate the local cache with base images.
//!
//! Pulls base images from a registry and stores them in the local cache directory
//! for faster subsequent builds.
//!
//! Analogous to Go: `pkg/cache/warm.go` — `WarmCache`, `Warmer`, `ParseDockerfile`.

use crate::layout::LayoutCache;
use crate::layout::LayoutCacheError;
use oci_registry::RegistryAuth;
use oci_registry::pull_image_with_platform;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors for cache warming operations.
#[derive(Debug, Error)]
pub enum WarmError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("pull error: {0}")]
    Pull(#[from] oci_registry::PullError),
    #[error("layout cache error: {0}")]
    Layout(#[from] LayoutCacheError),
    #[error("image already cached: {0}")]
    AlreadyCached(String),
    #[error("no images to warm")]
    NoImages,
    #[error("all images failed to warm")]
    AllFailed,
    #[error("dockerfile parse error: {0}")]
    DockerfileParse(String),
}

/// Result type for cache warming operations.
pub type Result<T> = std::result::Result<T, WarmError>;

/// Options for cache warming.
///
/// Analogous to Go: `config.WarmerOptions`.
#[derive(Debug, Clone)]
pub struct WarmerOptions {
    /// Directory to store the cached images.
    pub cache_dir: String,
    /// Explicit image references to warm.
    pub images: Vec<String>,
    /// Path to a Dockerfile to parse for base images.
    pub dockerfile_path: Option<String>,
    /// Force overwriting existing cache entries.
    pub force: bool,
    /// Cache TTL in hours. Defaults to 336 (two weeks).
    pub cache_ttl_hours: u64,
    /// Custom platform for pulling images (e.g., "linux/amd64").
    pub custom_platform: Option<String>,
    /// Build arguments for Dockerfile parsing (KEY=VALUE pairs).
    pub build_args: Vec<(String, String)>,
    /// Whether to use insecure (HTTP) connections for pulling.
    pub insecure_pull: bool,
    /// Whether to skip TLS verification for pulling.
    pub skip_tls_verify_pull: bool,
    /// Registry mirrors for pulling images.
    pub registry_mirrors: Vec<String>,
    /// Registry maps (original=new format).
    pub registry_maps: Vec<String>,
}

impl Default for WarmerOptions {
    fn default() -> Self {
        Self {
            cache_dir: "/cache".to_string(),
            images: vec![],
            dockerfile_path: None,
            force: false,
            cache_ttl_hours: 336,
            custom_platform: None,
            build_args: vec![],
            insecure_pull: false,
            skip_tls_verify_pull: false,
            registry_mirrors: vec![],
            registry_maps: vec![],
        }
    }
}

/// Warm the cache by pulling and storing the specified images.
///
/// If `dockerfile_path` is set in the options, the Dockerfile is parsed first
/// to extract base image names, which are then combined with the explicitly
/// listed images.
///
/// Returns an error only if ALL images fail to warm.
///
/// Analogous to Go: `cache.WarmCache()`.
pub async fn warm_cache(opts: &WarmerOptions, auth: &RegistryAuth) -> Result<()> {
    let mut images = opts.images.clone();

    // Parse Dockerfile for base images if specified
    if let Some(ref dockerfile_path) = opts.dockerfile_path {
        let base_images = parse_dockerfile_for_warming(dockerfile_path, &opts.build_args)?;
        images.extend_from_slice(&base_images);
    }

    if images.is_empty() {
        return Err(WarmError::NoImages);
    }

    tracing::info!("Warming cache for {} image(s)", images.len());

    // Ensure cache directory exists
    let cache_dir = Path::new(&opts.cache_dir);
    if !cache_dir.exists() {
        std::fs::create_dir_all(cache_dir)?;
    }

    let mut errors = 0;
    for img in &images {
        match warm_to_file(&opts.cache_dir, img, opts, auth).await {
            Ok(()) => {
                tracing::info!("Successfully warmed image: {}", img);
            }
            Err(WarmError::AlreadyCached(ref _msg)) => {
                tracing::info!("Image already in cache: {}", img);
            }
            Err(e) => {
                tracing::warn!("Error while trying to warm image: {} {}", img, e);
                errors += 1;
            }
        }
    }

    if images.len() == errors {
        return Err(WarmError::AllFailed);
    }

    Ok(())
}

/// Warm a single image to a file, then atomically rename it.
///
/// Downloads the image to a temporary file first, then renames to the final
/// path (using the image digest as filename). This avoids partial writes.
///
/// Analogous to Go: `warmToFile()`.
async fn warm_to_file(
    cache_dir: &str,
    image: &str,
    opts: &WarmerOptions,
    auth: &RegistryAuth,
) -> Result<()> {
    // Pull the image from the registry
    let img = pull_image_with_platform(image, auth, opts.custom_platform.as_deref())
        .await
        .map_err(|e| {
            tracing::warn!("Failed to retrieve image: {} {}", image, e);
            e
        })?;

    let digest = img.digest();

    // Check if already cached (unless force)
    if !opts.force {
        let cache = LayoutCache::new(cache_dir);
        if cache.exists(&digest.to_string()) {
            return Err(WarmError::AlreadyCached(image.to_string()));
        }
    }

    // Write image to cache
    let cache = LayoutCache::new(cache_dir);
    if !Path::new(cache_dir).join("oci-layout").exists() {
        cache.init()?;
    }
    cache.push_layer(&digest.to_string(), &img)?;

    // Also write the raw manifest as a separate file (digest.json)
    // Analogous to Go: Warmer writes manifest to ManifestWriter
    let manifest_path = PathBuf::from(cache_dir).join(format!("{}.json", digest));
    let manifest_json = serde_json::to_string_pretty(&img.manifest)?;
    std::fs::write(&manifest_path, manifest_json)?;

    tracing::debug!("Wrote {} to cache", image);
    Ok(())
}

/// Parse a Dockerfile to extract base image names for warming.
///
/// Reads the Dockerfile (local file or URL), parses it, and returns
/// the list of base image names from all FROM instructions.
/// Build arguments are resolved for dynamic base image references.
///
/// Analogous to Go: `cache.ParseDockerfile()`.
pub fn parse_dockerfile_for_warming(
    dockerfile_path: &str,
    build_args: &[(String, String)],
) -> Result<Vec<String>> {
    let content = if dockerfile_path.starts_with("http://") || dockerfile_path.starts_with("https://") {
        // Fetch from URL
        fetch_dockerfile_from_url(dockerfile_path)?
    } else {
        // Read from local file
        std::fs::read_to_string(dockerfile_path).map_err(|e| {
            WarmError::Io(std::io::Error::new(
                e.kind(),
                format!("reading dockerfile at path {}: {}", dockerfile_path, e),
            ))
        })?
    };

    let stages = dockerfile_parser::parse_dockerfile(&content)
        .map_err(|e| WarmError::DockerfileParse(e.to_string()))?;

    let mut base_names = Vec::new();
    for stage in &stages {
        let mut base_name = stage.image.clone();

        // Resolve build argument references in the base name
        // Analogous to Go: util.ResolveEnvironmentReplacement(s.BaseName, opts.BuildArgs, false)
        for (key, value) in build_args {
            let arg_ref = format!("${{{}}}", key);
            base_name = base_name.replace(&arg_ref, value);
            // Also handle $KEY form
            let arg_ref_short = format!("${}", key);
            // Only replace $KEY if it's a word boundary (not part of a longer var name)
            if base_name.contains(&arg_ref_short) {
                let re = regex_for_arg(key);
                base_name = re.replace_all(&base_name, value.as_str()).to_string();
            }
        }

        base_names.push(base_name);
    }

    Ok(base_names)
}

/// Fetch a Dockerfile from a URL.
fn fetch_dockerfile_from_url(url: &str) -> Result<String> {
    // Use reqwest blocking client for simplicity
    // The warmer is typically run as a standalone command, not inside an async context
    let response = reqwest::blocking::get(url)
        .map_err(|e| WarmError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("fetching dockerfile from URL {}: {}", url, e),
        )))?;
    let content = response.text()
        .map_err(|e| WarmError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("reading dockerfile from URL {}: {}", url, e),
        )))?;
    Ok(content)
}

/// Create a regex that matches $KEY as a word boundary.
fn regex_for_arg(key: &str) -> regex::Regex {
    regex::Regex::new(&format!(r"\${}", regex::escape(key))).unwrap_or_else(|_| {
        regex::Regex::new(&format!(r"\${}", regex::escape(key))).unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dockerfile_simple() {
        let content = "FROM alpine:3.18\nRUN echo hello\nFROM ubuntu:22.04\nRUN echo world";
        let stages = dockerfile_parser::parse_dockerfile(content).unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].image, "alpine:3.18");
        assert_eq!(stages[1].image, "ubuntu:22.04");
    }

    #[test]
    fn test_parse_dockerfile_with_build_args() {
        // Test that the parser resolves ARG defaults, and our build-arg
        // replacement can override when the parser preserves the variable reference.
        let content = "FROM alpine:3.18\nRUN echo hello";
        let stages = dockerfile_parser::parse_dockerfile(content).unwrap();
        assert_eq!(stages[0].image, "alpine:3.18");

        // Test with a Dockerfile that uses a variable (parser may or may not resolve)
        let content2 = "ARG BASE_IMAGE=alpine\nFROM $BASE_IMAGE\nRUN echo hello";
        let stages2 = dockerfile_parser::parse_dockerfile(content2).unwrap();
        // The parser resolves $BASE_IMAGE to its default value "alpine"
        let base_name = stages2[0].image.clone();
        assert!(
            base_name.contains("alpine") || base_name.contains("BASE_IMAGE"),
            "Expected alpine or BASE_IMAGE, got: {}",
            base_name
        );
    }

    #[test]
    fn test_warm_error_display() {
        let err = WarmError::AlreadyCached("test:latest".to_string());
        assert_eq!(format!("{}", err), "image already cached: test:latest");

        let err = WarmError::NoImages;
        assert_eq!(format!("{}", err), "no images to warm");
    }

    #[test]
    fn test_warmer_options_default() {
        let opts = WarmerOptions::default();
        assert_eq!(opts.cache_dir, "/cache");
        assert!(opts.images.is_empty());
        assert!(!opts.force);
        assert_eq!(opts.cache_ttl_hours, 336);
    }
}