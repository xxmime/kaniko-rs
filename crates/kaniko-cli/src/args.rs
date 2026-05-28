//! CLI argument definitions.
//!
//! Compatible with the original kaniko executor flags.

use clap::Parser;

/// kaniko-rs executor — build container images without a daemon.
#[derive(Parser, Debug)]
#[command(name = "kaniko-executor", version, about = "Build container images in Kubernetes")]
pub struct Cli {
    /// Path to the Dockerfile.
    #[arg(short, long, default_value = "Dockerfile")]
    pub dockerfile: Option<String>,

    /// Path to the build context directory.
    #[arg(short, long, default_value = ".")]
    pub context: Option<String>,

    /// Image destination(s) to push to.
    #[arg(short, long)]
    pub destination: Vec<String>,

    /// Cache repo for layer caching.
    #[arg(long)]
    pub cache_repo: Option<String>,

    /// Enable layer caching.
    #[arg(long)]
    pub cache: bool,

    /// Path to docker config.json.
    #[arg(long)]
    pub docker_config: Option<String>,

    /// Do not push image to registry.
    #[arg(long)]
    pub no_push: bool,

    /// Path to write the image tar.
    #[arg(long)]
    pub tar_path: Option<String>,

    /// Path to write the image digest.
    #[arg(long)]
    pub digest_file: Option<String>,

    /// Use single snapshot mode.
    #[arg(long)]
    pub single_snapshot: bool,

    /// Snapshot mode (full or redo).
    #[arg(long, default_value = "full")]
    pub snapshot_mode: String,

    /// Skip TLS certificate verification.
    #[arg(long)]
    pub skip_tls_verify: bool,

    /// Use insecure registry (HTTP).
    #[arg(long)]
    pub insecure: bool,

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

    /// Force build metadata.
    #[arg(long)]
    pub force_build_metadata: bool,

    /// OCI layout path for output.
    #[arg(long)]
    pub oci_layout_path: Option<String>,
}

/// Parse build argument in KEY=VALUE format.
fn parse_build_arg(arg: &str) -> Result<(String, String), String> {
    arg.split_once('=')
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .ok_or_else(|| "Build arguments must be in KEY=VALUE format".to_string())
}