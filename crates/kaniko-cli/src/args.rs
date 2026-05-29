//! CLI argument definitions.
//!
//! Compatible with the original kaniko executor flags.
//! Analogous to Go: `cmd/executor/cmd/root.go` — `addKanikoOptionsFlags()`.

use clap::Parser;
use std::time::Duration;

/// Compression algorithm for layer data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Compression {
    /// Gzip compression (default, Docker v2 layer format).
    Gzip,
    /// Zstd compression (OCI layer format).
    Zstd,
}

impl std::fmt::Display for Compression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Compression::Gzip => write!(f, "gzip"),
            Compression::Zstd => write!(f, "zstd"),
        }
    }
}

/// kaniko-rs executor — build container images without a daemon.
#[derive(Parser, Debug)]
#[command(name = "kaniko-executor", version, about = "Build container images in Kubernetes")]
pub struct Cli {
    // ===== Core build options =====

    /// Path to the Dockerfile.
    #[arg(short, long, default_value = "Dockerfile")]
    pub dockerfile: Option<String>,

    /// Path to the build context directory.
    #[arg(short, long, default_value = ".")]
    pub context: Option<String>,

    /// Image destination(s) to push to.
    #[arg(short, long)]
    pub destination: Vec<String>,

    // ===== Cache options =====

    /// Cache repo for layer caching.
    /// When prefixed with 'oci:' the repository will be written in OCI image layout format.
    #[arg(long)]
    pub cache_repo: Option<String>,

    /// Enable layer caching.
    #[arg(long)]
    pub cache: bool,

    /// Cache directory for local layout cache.
    #[arg(long, default_value = "/cache")]
    pub cache_dir: String,

    /// Cache TTL (e.g. "6h", "336h"). Defaults to two weeks.
    #[arg(long, default_value = "336h")]
    pub cache_ttl: String,

    /// Cache COPY layers.
    #[arg(long)]
    pub cache_copy_layers: bool,

    /// Cache RUN layers (default: true).
    #[arg(long, default_value_t = true)]
    pub cache_run_layers: bool,

    /// Do not push cache layers to registry.
    #[arg(long)]
    pub no_push_cache: bool,

    /// Compress the cached layers. Decreases build time, but increases memory usage.
    #[arg(long, default_value_t = true)]
    pub compressed_caching: bool,

    // ===== Registry options =====

    /// Path to docker config.json.
    #[arg(long)]
    pub docker_config: Option<String>,

    /// Push to insecure registry using plain HTTP.
    #[arg(long)]
    pub insecure: bool,

    /// Push to insecure registry ignoring TLS verify.
    #[arg(long)]
    pub skip_tls_verify: bool,

    /// Pull from insecure registry using plain HTTP.
    #[arg(long)]
    pub insecure_pull: bool,

    /// Pull from insecure registry ignoring TLS verify.
    #[arg(long)]
    pub skip_tls_verify_pull: bool,

    /// Insecure registry using plain HTTP to push and pull.
    #[arg(long)]
    pub insecure_registry: Vec<String>,

    /// Insecure registry ignoring TLS verify to push and pull.
    #[arg(long)]
    pub skip_tls_verify_registry: Vec<String>,

    /// Registry mirror to use as pull-through cache instead of docker.io.
    #[arg(long)]
    pub registry_mirror: Vec<String>,

    /// Number of retries for the push operation.
    #[arg(long, default_value_t = 0)]
    pub push_retry: u32,

    /// If true, known tag immutability errors are ignored.
    #[arg(long)]
    pub push_ignore_immutable_tag_errors: bool,

    /// Number of retries for image FS extraction.
    #[arg(long, default_value_t = 0)]
    pub image_fs_extract_retry: u32,

    /// Number of retries for downloading the remote image.
    #[arg(long, default_value_t = 0)]
    pub image_download_retry: u32,

    /// Skip check of the push permission.
    #[arg(long)]
    pub skip_push_permission_check: bool,

    // ===== Output options =====

    /// Do not push image to registry.
    #[arg(long)]
    pub no_push: bool,

    /// Path to save the image as a tarball instead of pushing.
    #[arg(long)]
    pub tar_path: Option<String>,

    /// Path to write the image digest.
    #[arg(long)]
    pub digest_file: Option<String>,

    /// File to save the image name with digest of the built image to.
    #[arg(long)]
    pub image_name_with_digest_file: Option<String>,

    /// File to save the image name with image tag with digest of the built image to.
    #[arg(long)]
    pub image_name_tag_with_digest_file: Option<String>,

    /// Path to save the OCI image layout of the built image.
    #[arg(long)]
    pub oci_layout_path: Option<String>,

    // ===== Build behavior options =====

    /// Build arguments in KEY=VALUE format.
    #[arg(long, value_parser = parse_build_arg)]
    pub build_arg: Vec<(String, String)>,

    /// Labels to add to the image.
    #[arg(long)]
    pub label: Vec<String>,

    /// Target stage to build.
    #[arg(long)]
    pub target: Option<String>,

    /// Platform(s) to build for.
    #[arg(long)]
    pub platform: Vec<String>,

    /// Use single snapshot mode.
    #[arg(long)]
    pub single_snapshot: bool,

    /// Snapshot mode (full or redo).
    #[arg(long, default_value = "full")]
    pub snapshot_mode: String,

    /// Strip timestamps out of the image to make it reproducible.
    #[arg(long)]
    pub reproducible: bool,

    /// Force build metadata.
    #[arg(long)]
    pub force_build_metadata: bool,

    /// Clean the filesystem at the end.
    #[arg(long)]
    pub cleanup: bool,

    /// Ignore /var/run directory when taking image snapshot.
    #[arg(long, default_value_t = true)]
    pub ignore_var_run: bool,

    /// Ignore these paths when taking a snapshot.
    #[arg(long)]
    pub ignore_path: Vec<String>,

    /// Build only used stages if defined to true.
    #[arg(long)]
    pub skip_unused_stages: bool,

    /// Use the experimental run implementation (RunMarkerCommand) for detecting changes.
    #[arg(long)]
    pub use_new_run: bool,

    /// Compression algorithm (gzip, zstd).
    #[arg(long, value_enum, default_value = "gzip")]
    pub compression: Compression,

    /// Compression level (-1 = default, 0 = none, 1-9 = level).
    #[arg(long, default_value_t = -1)]
    pub compression_level: i32,

    // ===== Sandbox options =====

    /// Build the image filesystem inside /kaniko/sandbox instead of the container root filesystem.
    #[arg(long)]
    pub sandbox: bool,

    /// Skip unpacking the base image filesystem when the first stage is already unpacked.
    #[arg(long)]
    pub initial_fs_unpacked: bool,

    // ===== Logging options =====

    /// Log level (trace, debug, info, warn, error).
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// Log format (text, json).
    #[arg(long, default_value = "text")]
    pub log_format: String,

    /// Force building outside of a container.
    #[arg(long)]
    pub force: bool,
}

/// Parse build argument in KEY=VALUE format.
fn parse_build_arg(arg: &str) -> Result<(String, String), String> {
    arg.split_once('=')
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .ok_or_else(|| "Build arguments must be in KEY=VALUE format".to_string())
}

impl Cli {
    /// Parse cache TTL string into a Duration.
    pub fn cache_ttl_duration(&self) -> Duration {
        parse_duration(&self.cache_ttl).unwrap_or(Duration::from_secs(336 * 3600))
    }
}

/// Parse a human-readable duration string (e.g. "6h", "336h", "30m", "2d").
fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_part, unit) = if let Some(rest) = s.strip_suffix('d') {
        (rest, 'd')
    } else if let Some(rest) = s.strip_suffix('h') {
        (rest, 'h')
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, 'm')
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, 's')
    } else {
        (s, 's')
    };

    let num: u64 = num_part.parse().ok()?;
    match unit {
        'd' => Some(Duration::from_secs(num * 86400)),
        'h' => Some(Duration::from_secs(num * 3600)),
        'm' => Some(Duration::from_secs(num * 60)),
        's' => Some(Duration::from_secs(num)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("6h"), Some(Duration::from_secs(6 * 3600)));
        assert_eq!(parse_duration("336h"), Some(Duration::from_secs(336 * 3600)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(30 * 60)));
        assert_eq!(parse_duration("2d"), Some(Duration::from_secs(2 * 86400)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn test_compression_display() {
        assert_eq!(Compression::Gzip.to_string(), "gzip");
        assert_eq!(Compression::Zstd.to_string(), "zstd");
    }
}