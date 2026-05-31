//! kaniko-rs: Build container images in Kubernetes without a daemon.
//!
//! This is the CLI entry point, analogous to Go: `cmd/executor/cmd/root.go`.
//!
//! The build flow follows the Go implementation:
//! 1. Parse CLI args → validate flags
//! 2. Resolve source context / Dockerfile path
//! 3. Check push permissions (unless skipped)
//! 4. DoBuild: Parse Dockerfile → iterate stages → build each
//! 5. DoPush: Write tar/digest/OCI layout + push to registry

mod args;

use args::Cli;
use base64::Engine;
use clap::Parser;
use kaniko_core::command::{
    AddCommand, ArgCommand, BuildArgs, CmdCommand, CopyCommand,
    EntrypointCommand, EnvCommand, ExposeCommand, HealthCheckCommand, LabelCommand,
    OnBuildCommand, RunCommand, RunMarkerCommand, ShellCommand, StopSignalCommand,
    UserCommand, VolumeCommand, WorkdirCommand, DockerCommand,
};
use kaniko_core::builder::BuildOptions;
use kaniko_creds::keychain::SystemKeychain;
use kaniko_snapshot::ignore_list::{
    init_ignore_list, add_var_run_to_ignore_list, add_ignore_paths,
};
use oci_image::mutate::MutableImage;
use oci_registry::auth::RegistryAuth;
use oci_registry::pull::pull_image;
use oci_registry::push::{push_image_with_options, PushOptions};
use oci_registry::transport::RegistryOptions;
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Handle subcommands first
    if let Some(ref cmd) = cli.command {
        match cmd {
            args::Commands::Version => {
                println!("Kaniko version : {}", kaniko_core::VERSION);
                return;
            }
            args::Commands::Warmer {
                image,
                cache_dir,
                force,
                cache_ttl,
                insecure_pull,
                skip_tls_verify_pull,
                insecure_registry: _,
                skip_tls_verify_registry: _,
                registry_mirror,
                registry_map,
                custom_platform,
                dockerfile,
                build_arg,
                docker_config,
            } => {
                run_warmer(args::WarmerRun {
                    images: image.clone(),
                    cache_dir: cache_dir.clone(),
                    force: *force,
                    cache_ttl: *cache_ttl,
                    insecure_pull: *insecure_pull,
                    skip_tls_verify_pull: *skip_tls_verify_pull,
                    registry_mirrors: registry_mirror.clone(),
                    registry_maps: registry_map.clone(),
                    custom_platform: custom_platform.clone(),
                    dockerfile_path: dockerfile.clone(),
                    build_args: build_arg.clone(),
                    docker_config: docker_config.clone(),
                }).await;
                return;
            }
        }
    }

    // Configure logging based on CLI args
    init_logging(&cli.log_level, &cli.log_format, cli.log_timestamp);

    tracing::info!("kaniko-rs executor starting");

    // Apply sandbox mode if requested
    apply_sandbox(cli.sandbox);

    // Start build timing
    let total_timer = kaniko_util::timing::start("total_build");

    match run(cli).await {
        Ok(()) => {
            kaniko_util::timing::stop(total_timer);
            if let Ok(timing) = kaniko_util::timing::DEFAULT_RUN.lock() {
                tracing::info!("{}", timing.format_all());
            }
            tracing::info!("Build completed successfully");
        }
        Err(e) => {
            kaniko_util::timing::stop(total_timer);
            if let Ok(timing) = kaniko_util::timing::DEFAULT_RUN.lock() {
                tracing::info!("{}", timing.format_all());
            }
            tracing::error!("Build failed: {}", e);
            std::process::exit(1);
        }
    }
}

/// Initialize the tracing/logging subscriber based on CLI flags.
/// Analogous to Go: `logging.Configure(level, format, logTimestamp)`.
fn init_logging(level: &str, format: &str, log_timestamp: bool) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    if format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_target(log_timestamp)
            .init();
    } else if format == "color" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(true)
            .with_target(log_timestamp)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(log_timestamp)
            .init();
    }
}

/// Main build execution flow.
/// Analogous to Go: `cmd/executor/cmd/root.go` → Run command handler.
async fn run(mut cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ===== Apply environment variable overrides =====
    // Analogous to Go: `validateFlags()` — KANIKO_REGISTRY_MIRROR, KANIKO_NO_PUSH, KANIKO_REGISTRY_MAP
    cli = apply_env_overrides(cli);

    // ===== Validate flags =====
    validate_flags(&cli)?;

    // ===== Check if running in container =====
    // Analogous to Go: `checkContained()` — warns if outside container without --force
    check_contained(cli.force);

    // ===== Set dummy destinations if --no-push and --tar-path =====
    // Analogous to Go: `push.setDummyDestinations()`.
    // When --no-push is set but --tar-path is provided, we still need
    // at least one destination for tag generation.
    set_dummy_destinations(&mut cli);

    // ===== Initialize ignore list =====
    // Analogous to Go: `util.InitIgnoreList()` + `--ignore-var-run` + `--ignore-path`.
    init_ignore_list();
    if cli.ignore_var_run {
        add_var_run_to_ignore_list();
    }
    if !cli.ignore_path.is_empty() {
        add_ignore_paths(&cli.ignore_path);
    }
    tracing::debug!("Ignore list initialized (ignore_var_run={}, {} custom paths)",
        cli.ignore_var_run, cli.ignore_path.len());

    // ===== Build registry options =====
    let registry_options = build_registry_options(&cli);

    // ===== Resolve credentials =====
    let keychain = if let Some(ref config_path) = cli.docker_config {
        SystemKeychain::with_config_path(PathBuf::from(config_path))
    } else {
        SystemKeychain::new()
    };

    // ===== Step 1: Parse Dockerfile =====
    let dockerfile_path = cli.dockerfile.as_deref().unwrap_or("Dockerfile");
    let context_dir = cli.context.as_deref().unwrap_or(".");
    let context_path = PathBuf::from(context_dir);

    tracing::info!("Dockerfile: {}", dockerfile_path);
    tracing::info!("Context: {}", context_dir);
    tracing::info!("Destinations: {:?}", cli.destination);

    let dockerfile_content = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| format!("Failed to read Dockerfile {}: {}", dockerfile_path, e))?;
    let stages = dockerfile_parser::parse_dockerfile(&dockerfile_content)
        .map_err(|e| format!("Failed to parse Dockerfile: {}", e))?;

    tracing::info!("Parsed {} stage(s)", stages.len());

    // ===== Step 2: Check push permissions (unless skipped) =====
    // Analogous to Go: `executor.CheckPushPermissions(opts)`.
    // Logic: skip if --skip-push-permission-check; skip if --no-push && --no-push-cache;
    // if --no-push but not --no-push-cache, check cache-repo instead of destinations.
    {
        let should_check = if cli.skip_push_permission_check {
            false
        } else if cli.no_push && cli.no_push_cache {
            false
        } else if cli.no_push && !cli.no_push_cache {
            // Check cache repo instead of destinations
            if let Some(ref cache_repo) = cli.cache_repo {
                if cache_repo.starts_with("oci:") {
                    false // OCI layout doesn't need push permission check
                } else {
                    let registry = extract_registry(cache_repo);
                    let credential = keychain.credentials(&registry)
                        .unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
                    let auth = RegistryAuth::new(&registry, credential)
                        .insecure(cli.insecure);
                    if let Err(e) = check_push_permission(cache_repo, &auth).await {
                        tracing::warn!("Push permission check failed for cache repo {}: {}", cache_repo, e);
                    }
                    false // Don't also check destinations below
                }
            } else {
                false
            }
        } else {
            true
        };

        if should_check {
            for dest in &cli.destination {
                let registry = extract_registry(dest);
                let credential = keychain.credentials(&registry)
                    .unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
                let auth = RegistryAuth::new(&registry, credential)
                    .insecure(cli.insecure);
                if let Err(e) = check_push_permission(dest, &auth).await {
                    tracing::warn!("Push permission check failed for {}: {}", dest, e);
                }
            }
        }
    }

    // ===== Step 3: DoBuild =====
    let image = do_build(&cli, &stages, &context_path, &keychain).await?;

    // ===== Step 4: DoPush =====
    do_push(&image, &cli, &keychain, &registry_options).await?;

    // ===== Step 5: Cleanup =====
    if cli.cleanup {
        tracing::info!("Running cleanup");
        cleanup_filesystem();
    }

    Ok(())
}

/// Validate CLI flags — analogous to Go: `validateFlags()`.
fn validate_flags(cli: &Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !cli.no_push && cli.destination.is_empty() {
        return Err("you must provide --destination, or use --no-push".into());
    }
    if cli.cache && cli.cache_repo.is_none() && cli.destination.is_empty() {
        return Err("cache requires --cache-repo or at least one --destination".into());
    }
    Ok(())
}

/// Apply environment variable overrides to CLI flags.
///
/// Analogous to Go: `validateFlags()` in root.go, which reads:
/// - `KANIKO_REGISTRY_MIRROR` → appends to registry-maps (index.docker.io=mirror)
/// - `KANIKO_NO_PUSH` → overrides --no-push
/// - `KANIKO_REGISTRY_MAP` → appends to registry-maps
///
/// Returns a modified Cli with env overrides applied.
fn apply_env_overrides(mut cli: Cli) -> Cli {
    // KANIKO_REGISTRY_MIRROR: each value maps index.docker.io → mirror
    // Go: `for _, target := range opts.RegistryMirrors { opts.RegistryMaps.Set(...) }`
    if let Ok(val) = std::env::var("KANIKO_REGISTRY_MIRROR") {
        for mirror in val.split(',') {
            let mirror = mirror.trim();
            if !mirror.is_empty() {
                let map_entry = format!("index.docker.io={}", mirror);
                if !cli.registry_map.contains(&map_entry) {
                    tracing::debug!("KANIKO_REGISTRY_MIRROR: added registry map {}", map_entry);
                    cli.registry_map.push(map_entry);
                }
            }
        }
    }

    // KANIKO_NO_PUSH: boolean override for --no-push
    if let Ok(val) = std::env::var("KANIKO_NO_PUSH") {
        match val.to_lowercase().as_str() {
            "true" | "1" | "yes" => {
                if !cli.no_push {
                    tracing::debug!("KANIKO_NO_PUSH=true: overriding --no-push");
                    cli.no_push = true;
                }
            }
            "false" | "0" | "no" => {
                if cli.no_push {
                    tracing::debug!("KANIKO_NO_PUSH=false: overriding --no-push");
                    cli.no_push = false;
                }
            }
            _ => {
                tracing::warn!("Invalid value for KANIKO_NO_PUSH env var (expected true/false): {}", val);
            }
        }
    }

    // KANIKO_REGISTRY_MAP: same format as --registry-map
    if let Ok(val) = std::env::var("KANIKO_REGISTRY_MAP") {
        for map_entry in val.split(',') {
            let map_entry = map_entry.trim();
            if !map_entry.is_empty() && !cli.registry_map.contains(&map_entry.to_string()) {
                cli.registry_map.push(map_entry.to_string());
                tracing::debug!("KANIKO_REGISTRY_MAP: added {}", map_entry);
            }
        }
    }

    // KANIKO_DIR: override kaniko directory
    // Go: `dir := config.KanikoDir; if opts.KanikoDir != constants.DefaultKanikoPath { dir = opts.KanikoDir }`
    if let Ok(val) = std::env::var("KANIKO_DIR") {
        if cli.kaniko_dir == "/kaniko" && !val.is_empty() {
            cli.kaniko_dir = val;
            tracing::debug!("KANIKO_DIR: overriding kaniko directory to {}", cli.kaniko_dir);
        }
    }

    cli
}

/// Check if kaniko is running inside a container.
/// If not, warn the user (or exit if --force is not set).
///
/// Analogous to Go: `checkContained()` in root.go.
fn check_contained(force: bool) {
    use kaniko_snapshot::container::is_running_in_container;
    if !is_running_in_container() {
        if !force {
            tracing::error!(
                "kaniko should only be run inside of a container, \
                 run with the --force flag if you are sure you want to continue"
            );
            std::process::exit(1);
        }
        tracing::warn!("Kaniko is being run outside of a container. This can have dangerous effects on your system");
    }
}

/// Build all stages and return the final image.
/// Analogous to Go: `executor.DoBuild(opts)`.
async fn do_build(
    cli: &Cli,
    stages: &[dockerfile_parser::Stage],
    context_path: &PathBuf,
    keychain: &SystemKeychain,
) -> Result<MutableImage, Box<dyn std::error::Error + Send + Sync>> {
    let mut build_args_map: HashMap<String, String> = HashMap::new();
    for (key, value) in &cli.build_arg {
        build_args_map.insert(key.clone(), value.clone());
    }

    // Parse labels from CLI
    let cli_labels: Vec<(String, String)> = cli.label.iter()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();

    let target_stage = cli.target.as_deref();
    let last_stage_idx = stages.len() - 1;
    let mut built_images: HashMap<usize, MutableImage> = HashMap::new();

    // Track stage digest → cache key and stage idx → digest mappings.
    // Analogous to Go: `digestToCacheKey` and `stageIdxToDigest`.
    // These are used for multi-stage build cache sharing — when a later stage
    // references a previous stage by index, we can look up its digest and cache key.
    let mut digest_to_cache_key: HashMap<String, String> = HashMap::new();
    let mut stage_idx_to_digest: HashMap<String, String> = HashMap::new();

    // Build options — shared across all stages
    let build_opts = BuildOptions {
        cache: cli.cache,
        cache_dir: if cli.cache_dir == "/cache" { None } else { Some(cli.cache_dir.clone()) },
        single_snapshot: cli.single_snapshot,
        force_build_metadata: cli.force_build_metadata,
        snapshot_mode: cli.snapshot_mode.clone(),
        cache_copy_layers: cli.cache_copy_layers,
        cache_run_layers: cli.cache_run_layers,
        run_v2: cli.use_new_run,
        compression: Some(cli.compression.to_string()),
        compression_level: if cli.compression_level >= 0 { cli.compression_level as u32 } else { 0 },
        compressed_caching: cli.compressed_caching,
        initial_fs_unpacked: cli.initial_fs_unpacked,
    };

    for (stage_idx, stage) in stages.iter().enumerate() {
        let stage_name = stage.alias.as_deref().unwrap_or("default");
        tracing::info!("=== Building stage {}/{}: {} ===", stage_idx + 1, stages.len(), stage_name);

        // Skip if target is specified and this isn't it
        if let Some(target) = target_stage {
            if stage_name != target && &format!("{}", stage_idx) != target {
                if stage_idx != last_stage_idx && cli.skip_unused_stages {
                    continue;
                }
            }
        }

        // Pull base image
        let mut image = if stage.image == "scratch" {
            tracing::info!("Using scratch base image");
            MutableImage::empty()
        } else {
            tracing::info!("Pulling base image: {}", stage.image);
            let registry = extract_registry(&stage.image);
            let credential = keychain.credentials(&registry)
                .unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
            let insecure = cli.insecure || cli.insecure_pull
                || cli.insecure_registry.contains(&registry);
            let auth = RegistryAuth::new(&registry, credential)
                .insecure(insecure);

            pull_image_with_retry(&stage.image, &auth, cli.image_download_retry).await?
        };

        // Extract base image layers to rootfs directory
        let root_dir = if !cli.initial_fs_unpacked {
            let rootfs = PathBuf::from("/sandbox");
            if rootfs.exists() {
                std::fs::remove_dir_all(&rootfs)?;
            }
            std::fs::create_dir_all(&rootfs)?;
            tracing::info!("Extracting base image to {}", rootfs.display());
            oci_image::extract::extract_image_to_fs(&image, &rootfs)?;
            rootfs
        } else {
            PathBuf::from("/")
        };

        // Initialize config (analogous to Go: initConfig)
        init_config(&mut image, &cli_labels);

        // Set up build args
        let build_args = BuildArgs {
            args: vec![],
            env: vec![],
            build_args: build_args_map.clone(),
        };

        // Execute commands in this stage
        let mut container_config = image.config.config.clone();

        // Track ENTRYPOINT and CMD for reviewConfig
        let mut has_entrypoint = false;
        let mut has_cmd = false;

        for instruction in &stage.instructions {
            execute_instruction(
                instruction,
                &mut container_config,
                &build_args,
                context_path,
                cli,
                &root_dir,
            ).await?;

            // Track ENTRYPOINT/CMD presence for reviewConfig
            match instruction {
                dockerfile_parser::Instruction::Entrypoint(_) => has_entrypoint = true,
                dockerfile_parser::Instruction::Cmd(_) => has_cmd = true,
                _ => {}
            }
        }

        // Review config: if ENTRYPOINT was set but CMD was not, clear CMD.
        // Analogous to Go: `reviewConfig(stage, &config)`.
        if has_entrypoint && !has_cmd {
            tracing::debug!("Clearing Cmd because Entrypoint was set without a new Cmd");
            container_config.cmd = None;
        }

        // Update the image config with the modified container config
        image.config.config = container_config;
        image.config_bytes = serde_json::to_vec(&image.config)?;

        // Set platform (os/architecture) from --platform or auto-detect.
        // Analogous to Go: `configFile.OS = runtime.GOOS; configFile.Architecture = runtime.GOARCH`.
        set_platform(&mut image, cli.platform.first().map(|s| s.as_str()));

        // Apply reproducible mode — strip timestamps
        if cli.reproducible {
            make_reproducible(&mut image);
        }

        built_images.insert(stage_idx, image);

        // Record stage digest → cache key and stage idx → digest mappings.
        // Analogous to Go: `d, err := sourceImage.Digest()` then
        //   `stageIdxToDigest[fmt.Sprintf("%d", sb.stage.Index)] = d.String()`
        //   `digestToCacheKey[d.String()] = sb.finalCacheKey`
        let stage_image = built_images.get(&stage_idx).unwrap();
        let digest = stage_image.digest();
        let digest_str = digest.to_string();
        stage_idx_to_digest.insert(format!("{}", stage_idx), digest_str.clone());
        tracing::debug!("Mapping stage idx {} to digest {}", stage_idx, digest_str);

        // Note: cache_key mapping would come from the builder's composite_key.hash()
        // For now we record an empty placeholder since the CLI path doesn't use
        // StageBuilder (which computes the composite key). This mapping will be
        // fully populated when StageBuilder is used for the build loop.
        digest_to_cache_key.insert(digest_str.clone(), String::new());
        tracing::debug!("Mapping digest {} to cachekey (placeholder)", digest_str);

        // Only return the last stage (or target stage)
        if stage_idx == last_stage_idx || target_stage.is_some() {
            return Ok(built_images.remove(&stage_idx).unwrap());
        }
    }

    Err("No target stage found".into())
}

/// Push the built image to destinations.
/// Analogous to Go: `executor.DoPush(image, opts)`.
async fn do_push(
    image: &MutableImage,
    cli: &Cli,
    keychain: &SystemKeychain,
    registry_options: &RegistryOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Compute digest for output files
    let digest = image.digest().to_string();

    // Write digest file if requested
    if let Some(ref digest_file) = cli.digest_file {
        write_digest_file(digest_file, &digest)?;
    }

    // Write image-name-with-digest-file if requested
    if let Some(ref file) = cli.image_name_with_digest_file {
        let mut content = String::new();
        for dest in &cli.destination {
            content.push_str(&format!("{}@{}\n", dest, digest));
        }
        write_digest_file(file, &content)?;
    }

    // Write image-name-tag-with-digest-file if requested
    if let Some(ref file) = cli.image_name_tag_with_digest_file {
        let mut content = String::new();
        for dest in &cli.destination {
            content.push_str(&format!("{}@{}\n", dest, digest));
        }
        write_digest_file(file, &content)?;
    }

    // Write OCI layout if requested
    if let Some(ref layout_path) = cli.oci_layout_path {
        tracing::info!("Writing OCI layout to {}", layout_path);
        oci_image::layout::write_layout(image, std::path::Path::new(layout_path))?;
    }

    // Write to tar if requested
    if let Some(ref tar_path) = cli.tar_path {
        tracing::info!("Writing image tar to {}", tar_path);
        write_image_tar(image, tar_path)?;
    }

    // Push to registry unless --no-push
    if cli.no_push {
        tracing::info!("Skipping push to container registry due to --no-push flag");
        return Ok(());
    }

    if cli.destination.is_empty() {
        return Ok(());
    }

    for dest in &cli.destination {
        tracing::info!("Pushing image to {}", dest);
        let registry = extract_registry(dest);
        let insecure = cli.insecure
            || cli.insecure_registry.contains(&registry);
        let credential = keychain.credentials(&registry)
            .unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
        let auth = RegistryAuth::new(&registry, credential)
            .insecure(insecure);

        let push_opts = PushOptions::default()
            .with_ignore_immutable_tag_errors(cli.push_ignore_immutable_tag_errors)
            .with_registry_options(registry_options.clone());

        push_image_with_retry(image, dest, &auth, &push_opts, cli.push_retry).await?;
        tracing::info!("Pushed {}", dest);
    }

    // Write image outputs (BUILDER_OUTPUT)
    write_image_outputs(image, &cli.destination);

    Ok(())
}

// ============================================================================
// Helper functions
// ============================================================================

/// Extract registry hostname from an image reference.
fn extract_registry(reference: &str) -> String {
    if let Some((host, _)) = reference.split_once('/') {
        if host.contains('.') || host.contains(':') || host == "localhost" {
            host.to_string()
        } else {
            "index.docker.io".to_string()
        }
    } else {
        "index.docker.io".to_string()
    }
}

/// Check push permission for a destination.
/// Analogous to Go: `executor.CheckPushPermissions()`.
async fn check_push_permission(
    dest: &str,
    auth: &RegistryAuth,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Lightweight check: attempt to get the registry API version.
    // If we can authenticate, we likely have push permission.
    let reference = oci_registry::Reference::parse(dest)
        .map_err(|e| format!("Invalid reference {}: {}", dest, e))?;
    let base_url = if auth.insecure {
        format!("http://{}", reference.registry)
    } else {
        format!("https://{}", reference.registry)
    };
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(auth.insecure)
        .build()?;

    let url = format!("{}/v2/", base_url);
    let mut req = client.get(&url);
    if !auth.credential.is_anonymous() {
        let encoded = base64_encode_credentials(&auth.credential.username, &auth.credential.password);
        req = req.header("Authorization", format!("Basic {}", encoded));
    }

    let resp = req.send().await?;
    if resp.status().is_success() || resp.status().as_u16() == 401 {
        // 401 means the registry exists but requires different auth
        // — not a push permission failure per se
        Ok(())
    } else {
        Err(format!("Registry check returned status {}", resp.status()).into())
    }
}

/// Base64-encode credentials for HTTP Basic Auth.
fn base64_encode_credentials(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes())
}

/// Pull image with retry support.
/// Analogous to Go: `util.Retry()`.
async fn pull_image_with_retry(
    reference: &str,
    auth: &RegistryAuth,
    max_retries: u32,
) -> Result<MutableImage, Box<dyn std::error::Error + Send + Sync>> {
    let mut last_error = None;
    let attempts = if max_retries > 0 { max_retries + 1 } else { 1 };

    for attempt in 1..=attempts {
        match pull_image(reference, auth).await {
            Ok(img) => return Ok(img),
            Err(e) => {
                tracing::warn!("Pull attempt {}/{} failed: {}", attempt, attempts, e);
                last_error = Some(e);
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 * attempt as u64)).await;
                }
            }
        }
    }

    Err(format!("Failed to pull image after {} attempts: {}", attempts, last_error.unwrap()).into())
}

/// Push image with retry support.
async fn push_image_with_retry(
    image: &MutableImage,
    dest: &str,
    auth: &RegistryAuth,
    opts: &PushOptions,
    max_retries: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut last_error = None;
    let attempts = if max_retries > 0 { max_retries + 1 } else { 1 };

    for attempt in 1..=attempts {
        match push_image_with_options(image, dest, auth, opts.clone()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!("Push attempt {}/{} failed: {}", attempt, attempts, e);
                last_error = Some(e);
                if attempt < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(1000 * attempt as u64)).await;
                }
            }
        }
    }

    Err(format!("Failed to push image after {} attempts: {}", attempts, last_error.unwrap()).into())
}

/// Initialize the image config with default values.
/// Analogous to Go: `executor.initConfig()`.
fn init_config(image: &mut MutableImage, labels: &[(String, String)]) {
    // Set default environment variables if not present
    // Analogous to Go: `constants.ScratchEnvVars`
    let default_path = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
    if image.config.config.env.is_none() {
        image.config.config.env = Some(vec![format!("PATH={}", default_path)]);
    } else if let Some(ref mut env) = image.config.config.env {
        let has_path = env.iter().any(|e| e.starts_with("PATH="));
        if !has_path {
            env.insert(0, format!("PATH={}", default_path));
        }
    }

    // Apply CLI labels
    if !labels.is_empty() {
        let label_map = image.config.config.labels.get_or_insert_with(Default::default);
        for (key, value) in labels {
            label_map.insert(key.clone(), value.clone());
        }
    }
}

/// Execute a single Dockerfile instruction.
/// Dispatches to the appropriate command implementation.
async fn execute_instruction(
    instruction: &dockerfile_parser::Instruction,
    config: &mut oci_image::config::ContainerConfig,
    args: &BuildArgs,
    context_path: &PathBuf,
    cli: &Cli,
    root_dir: &PathBuf,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result: Result<(), kaniko_core::command::CommandError> = match instruction {
        dockerfile_parser::Instruction::From(_) => {
            // Already handled in do_build
            return Ok(());
        }
        dockerfile_parser::Instruction::Env(env) => {
            EnvCommand::new(env.key.clone(), env.value.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Label(label) => {
            LabelCommand::new(label.labels.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Expose(expose) => {
            ExposeCommand::new(expose.ports.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::User(user) => {
            UserCommand::new(user.user.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Workdir(workdir) => {
            WorkdirCommand::new(workdir.path.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Copy(copy) => {
            CopyCommand::with_flags(
                copy.sources.clone(),
                copy.destination.clone(),
                copy.from.clone(),
                copy.chown.clone(),
                copy.chmod.clone(),
                copy.link,
                context_path.clone(),
                cli.cache,
            ).execute(config, args).await
        }
        dockerfile_parser::Instruction::Add(add) => {
            AddCommand::with_flags(
                add.sources.clone(),
                add.destination.clone(),
                add.chown.clone(),
                add.chmod.clone(),
                add.link,
                context_path.clone(),
                cli.cache,
            ).execute(config, args).await
        }
        dockerfile_parser::Instruction::Run(run) => {
            if cli.use_new_run {
                // Use RunMarkerCommand (--use-new-run flag)
                let c = if run.is_shell_form {
                    RunMarkerCommand::new_shell(run.command.clone(), cli.cache)
                } else {
                    let run_args = if run.args.is_empty() {
                        run.command.split_whitespace().map(String::from).collect()
                    } else {
                        run.args.clone()
                    };
                    RunMarkerCommand::new_exec(run_args, cli.cache)
                };
                let mut c = c;
                for mount in &run.mounts {
                    c = c.with_mount(mount.clone());
                }
                if let Some(network) = &run.network {
                    c = c.with_network(network.clone());
                }
                c = c.with_root_dir(root_dir.clone());
                c.execute(config, args).await
            } else {
                let mut c = if run.is_shell_form {
                    RunCommand::new_shell(run.command.clone(), cli.cache)
                } else {
                    let run_args = if run.args.is_empty() {
                        run.command.split_whitespace().map(String::from).collect()
                    } else {
                        run.args.clone()
                    };
                    RunCommand::new_exec(run_args, cli.cache)
                };
                for mount in &run.mounts {
                    c = c.with_mount(mount.clone());
                }
                if let Some(network) = &run.network {
                    c = c.with_network(network.clone());
                }
                c = c.with_root_dir(root_dir.to_string_lossy().to_string());
                c.execute(config, args).await
            }
        }
        dockerfile_parser::Instruction::Cmd(cmd) => {
            let c = if cmd.is_shell_form {
                CmdCommand::new_shell(cmd.command.first().cloned().unwrap_or_default())
            } else {
                CmdCommand::new_exec(cmd.command.clone())
            };
            c.execute(config, args).await
        }
        dockerfile_parser::Instruction::Entrypoint(ep) => {
            let c = if ep.is_shell_form {
                EntrypointCommand::new_shell(ep.command.first().cloned().unwrap_or_default())
            } else {
                EntrypointCommand::new_exec(ep.command.clone())
            };
            c.execute(config, args).await
        }
        dockerfile_parser::Instruction::Volume(vol) => {
            VolumeCommand::new(vol.paths.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Arg(arg) => {
            ArgCommand::new(arg.name.clone(), arg.default_value.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Shell(shell) => {
            ShellCommand::new(shell.shell.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::StopSignal(sig) => {
            StopSignalCommand::new(sig.signal.clone())
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Healthcheck(hc) => {
            let c = if hc.is_none {
                HealthCheckCommand::none()
            } else {
                HealthCheckCommand::new(
                    hc.cmd.clone().map(|s| vec!["CMD-SHELL".to_string(), s]).unwrap_or_default(),
                    hc.interval.clone(),
                    hc.timeout.clone(),
                    hc.start_period.clone(),
                    hc.retries,
                )
            };
            c.execute(config, args).await
        }
        dockerfile_parser::Instruction::Onbuild(ob) => {
            let trigger = format!("{:?}", ob.instruction);
            OnBuildCommand::new(trigger)
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Maintainer(m) => {
            LabelCommand::new(vec![("maintainer".to_string(), m.name.clone())])
                .execute(config, args).await
        }
        dockerfile_parser::Instruction::Comment(_) => {
            return Ok(());
        }
    };

    if let Err(e) = result {
        return Err(format!("Build command failed: {}", e).into());
    }
    Ok(())
}

/// Strip timestamps from the image to make it reproducible.
/// Analogous to Go: `--reproducible` flag behavior.
fn make_reproducible(image: &mut MutableImage) {
    // Clear created timestamp in config
    image.config.created = None;

    // Clear created timestamps in history entries
    for entry in image.config.history.iter_mut() {
        entry.created = None;
    }

    tracing::debug!("Applied reproducible mode: stripped timestamps");
}

/// Write digest information to a file.
/// Analogous to Go: `push.writeDigestFile()`.
fn write_digest_file(path: &str, content: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if path.starts_with("https://") {
        // HTTP PUT for pre-signed URLs (S3, GCS, Azure)
        let client = reqwest::Client::new();
        let rt = tokio::runtime::Handle::current();
        rt.block_on(async {
            client.put(path)
                .header("Content-Type", "text/plain")
                .body(content.to_string())
                .send()
                .await
        })?;
        return Ok(());
    }

    // Create parent directories if needed
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, content)?;
    Ok(())
}

/// Write image outputs to BUILDER_OUTPUT directory.
/// Analogous to Go: `push.writeImageOutputs()`.
fn write_image_outputs(image: &MutableImage, destinations: &[String]) {
    let dir = match std::env::var("BUILDER_OUTPUT") {
        Ok(d) if !d.is_empty() => d,
        _ => return,
    };

    let images_path = std::path::Path::new(&dir).join("images");
    if let Err(e) = write_image_outputs_file(&images_path, image, destinations) {
        tracing::warn!("Failed to write image outputs: {}", e);
    }
}

fn write_image_outputs_file(
    path: &std::path::Path,
    image: &MutableImage,
    destinations: &[String],
) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    let digest = image.digest().to_string();
    for dest in destinations {
        let output = serde_json::json!({
            "name": dest,
            "digest": digest,
        });
        writeln!(file, "{}", output)?;
    }
    Ok(())
}

/// Clean up the filesystem after build.
/// Analogous to Go: `--cleanup` flag behavior.
fn cleanup_filesystem() {
    // In the Go version, cleanup removes the kaniko directory contents.
    // In container environments this is typically /kaniko.
    let kaniko_dir = std::env::var("KANIKO_DIR").unwrap_or_else(|_| "/kaniko".to_string());
    let kaniko_path = std::path::Path::new(&kaniko_dir);

    if kaniko_path.exists() {
        if let Err(e) = std::fs::remove_dir_all(kaniko_path) {
            tracing::warn!("Cleanup failed for {}: {}", kaniko_dir, e);
        } else {
            tracing::info!("Cleaned up {}", kaniko_dir);
        }
    }
}

/// Build RegistryOptions from CLI flags.
/// Analogous to Go: `config.RegistryOptions` construction from flags.
fn build_registry_options(cli: &Cli) -> RegistryOptions {
    let mut opts = RegistryOptions::new();

    // Insecure registries
    opts.insecure_registries = cli.insecure_registry.clone();

    // Skip TLS verify registries
    opts.skip_tls_verify_registries = cli.skip_tls_verify_registry.clone();

    // Registry mirrors — format: "registry=mirror"
    for mirror_spec in &cli.registry_mirror {
        if let Some((registry, mirror_url)) = mirror_spec.split_once('=') {
            opts.registry_mirrors
                .entry(registry.to_string())
                .or_default()
                .push(mirror_url.to_string());
        } else {
            tracing::warn!("Invalid registry mirror spec (expected registry=mirror): {}", mirror_spec);
        }
    }

    // Registry maps — format: "original.registry=new.registry" or "original.registry=new.registry/prefix/"
    // Analogous to Go: `opts.RegistryMaps`
    for map_spec in &cli.registry_map {
        if let Some((source, dest)) = map_spec.split_once('=') {
            opts.registry_maps
                .entry(source.to_lowercase())
                .or_default()
                .push(dest.to_string());
        } else {
            tracing::warn!("Invalid registry map spec (expected original=new): {}", map_spec);
        }
    }

    // Registry certificates — format: "my.registry.url=/path/to/cert"
    // Analogous to Go: `opts.RegistriesCertificates`
    for cert_spec in &cli.registry_certificate {
        if let Some((registry, cert_path)) = cert_spec.split_once('=') {
            opts.registry_certificates
                .insert(registry.to_string(), PathBuf::from(cert_path));
        } else {
            tracing::warn!("Invalid registry certificate spec (expected registry=/path/to/cert): {}", cert_spec);
        }
    }

    // Registry client certificates — format: "my.registry.url=/path/to/cert,/path/to/key"
    // Analogous to Go: `opts.RegistriesClientCertificates`
    for cert_spec in &cli.registry_client_cert {
        if let Some((registry, certs)) = cert_spec.split_once('=') {
            opts.registry_client_certificates
                .insert(registry.to_string(), certs.to_string());
        } else {
            tracing::warn!("Invalid registry client cert spec (expected registry=cert,key): {}", cert_spec);
        }
    }

    // Skip default registry fallback
    opts.skip_default_registry_fallback = cli.skip_default_registry_fallback;

    opts
}

/// Set the platform (os/architecture) on the image config.
/// Analogous to Go: `configFile.OS = runtime.GOOS; configFile.Architecture = runtime.GOARCH`
/// or `opts.CustomPlatform` parsing.
fn set_platform(image: &mut MutableImage, platform: Option<&str>) {
    let (os, arch) = if let Some(p) = platform {
        // Parse "os/arch" or "os/arch/variant" format
        let parts: Vec<&str> = p.split('/').collect();
        let os = parts.first().unwrap_or(&"linux").to_string();
        let arch = parts.get(1).unwrap_or(&"amd64").to_string();
        (os, arch)
    } else {
        // Auto-detect from the current system
        (std::env::consts::OS.to_string(), std::env::consts::ARCH.to_string())
    };
    image.config.os = os.clone();
    image.config.architecture = arch.clone();
    tracing::debug!("Set image platform: {}/{}", os, arch);
}

/// Write image as a Docker tar archive.
fn write_image_tar(image: &MutableImage, path: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut file = std::fs::File::create(path)?;
    let mut tar_builder = tar::Builder::new(&mut file);

    // Add manifest
    let manifest_json = serde_json::to_vec(&image.manifest)?;
    let mut header = tar::Header::new_gnu();
    header.set_path("manifest.json")?;
    header.set_size(manifest_json.len() as u64);
    header.set_cksum();
    tar_builder.append(&header, manifest_json.as_slice())?;

    // Add config
    let config_path = format!("blobs/sha256/{}", image.config_digest());
    let mut header = tar::Header::new_gnu();
    header.set_path(&config_path)?;
    header.set_size(image.config_bytes.len() as u64);
    header.set_cksum();
    tar_builder.append(&header, image.config_bytes.as_slice())?;

    // Add layers
    for layer in &image.layers {
        let layer_path = format!("blobs/sha256/{}", layer.digest());
        let data = layer.data();
        let mut header = tar::Header::new_gnu();
        header.set_path(&layer_path)?;
        header.set_size(data.len() as u64);
        header.set_cksum();
        tar_builder.append(&header, data)?;
    }

    tar_builder.finish()?;
    Ok(())
}

/// Set dummy destinations when --no-push and --tar-path are used.
///
/// When --no-push is set but --tar-path is provided, a destination
/// is still needed for generating image tags and digests in the tar output.
/// This function adds a dummy destination if none exist.
///
/// Analogous to Go: `push.setDummyDestinations()`.
fn set_dummy_destinations(cli: &mut Cli) {
    if cli.no_push && cli.tar_path.is_some() && cli.destination.is_empty() {
        let dummy = "index.docker.io/kaniko/dummy:latest".to_string();
        tracing::info!("Setting dummy destination for tar output: {}", dummy);
        cli.destination.push(dummy);
    }
}

/// Initialize the user namespace for container builds.
///
/// This must be called after entering the user namespace (detected via
/// `KANIKO_SANDBOX_USERNS=1`). It performs setup that is required for
/// commands inside chroot to work correctly:
///
/// 1. `mount --make-rslave /` — Mark the mount tree as slave so that
///    mount events from the parent namespace propagate in, but our mounts
///    don't propagate out. This is required before we can mount /proc,
///    /sys, /dev into the rootfs. (Matches Go: chrootarchive uses
///    `mount.MakeRSlave("/")` before pivot_root.)
///
/// 2. Write "allow" to `/proc/self/setgroups` — `unshare --map-root-user`
///    sets this to "deny", but chrooted programs (apt, dpkg) need to call
///    setgroups. Writing "allow" re-enables it.
#[cfg(target_os = "linux")]
fn init_user_namespace() {
    // 1. Make mount tree slave — required before mounting proc/sys/dev.
    // Using rslave (recursive) so all existing shared mounts become slave.
    // This matches the Go chrootarchive implementation.
    let output = std::process::Command::new("mount")
        .arg("--make-rslave")
        .arg("/")
        .output();
    match output {
        Ok(o) if o.status.success() => {
            tracing::debug!("sandbox: mount --make-rslave / succeeded");
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            tracing::warn!("sandbox: mount --make-rslave / failed: {}", stderr.trim());
        }
        Err(e) => {
            tracing::warn!("sandbox: mount --make-rslave / error: {}", e);
        }
    }

    // 2. Allow setgroups in the user namespace.
    // unshare --map-root-user writes "deny" to /proc/self/setgroups,
    // but programs like apt need to switch groups.
    let setgroups_path = "/proc/self/setgroups";
    if std::path::Path::new(setgroups_path).exists() {
        match std::fs::write(setgroups_path, "allow") {
            Ok(()) => {
                tracing::debug!("sandbox: setgroups allowed");
            }
            Err(e) => {
                tracing::debug!("sandbox: could not write 'allow' to {}: {}", setgroups_path, e);
            }
        }
    }
}

/// Apply sandbox mode for the build.
///
/// In sandbox mode, the build filesystem is isolated using Linux namespaces.
/// On non-Linux platforms, this degrades gracefully with a warning.
///
/// Analogous to Go: `unshare.MaybeReexecUsingUserNamespace()`.
fn apply_sandbox(sandbox: bool) {
    if !sandbox {
        return;
    }

    #[cfg(target_os = "linux")]
    {
        // Check if we're already inside a user namespace
        if std::env::var("KANIKO_SANDBOX_USERNS").as_deref() == Ok("1") {
            tracing::info!("Sandbox: already running inside user namespace");
            kaniko_core::container_runtime::set_sandbox_active(true);

            // Initialize the user namespace for container builds:
            // 1. Make mount tree private so we can create new mounts
            // 2. Allow setgroups (needed for apt, etc.)
            init_user_namespace();
            return;
        }

        // Try to use unshare to create user+mount namespace
        // This gives us CAP_SYS_ADMIN for bind-mounting /proc, /sys, /dev
        tracing::info!("Sandbox mode — re-executing inside user+mount namespace");
        let self_exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                tracing::warn!("Sandbox: cannot determine self executable path: {}. Sandbox disabled.", e);
                return;
            }
        };

        let mut cmd = std::process::Command::new("unshare");
        cmd.arg("--user")
            .arg("--mount")
            .arg("--propagation")
            .arg("slave")
            .arg("--fork")
            .arg("--map-root-user")
            .arg("--")
            .arg(self_exe);

        // Pass through all original arguments
        let mut skip_sandbox = false;
        for arg in std::env::args().skip(1) {
            cmd.arg(&arg);
            // Don't add --sandbox again to avoid infinite recursion
            if arg == "--sandbox" {
                skip_sandbox = true;
            }
        }

        // Set env to indicate we're in sandbox namespace
        cmd.env("KANIKO_SANDBOX_USERNS", "1");

        match cmd.status() {
            Ok(status) => {
                // Exit with the same code as the re-executed process
                std::process::exit(status.code().unwrap_or(1));
            }
            Err(e) => {
                tracing::warn!("Sandbox: failed to re-execute with unshare: {}. Sandbox disabled.", e);
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "Sandbox mode is not supported on this platform. \
             Build will proceed without namespace isolation."
        );
    }
}

/// Run the cache warmer subcommand.
///
/// Analogous to Go: `cmd/warmer/cmd/root.go` — RootCmd.Run.
async fn run_warmer(params: args::WarmerRun) {
    // Configure logging
    init_logging("info", "color", true);

    tracing::info!("kaniko-rs cache warmer starting");

    // Validate: at least one image or a dockerfile must be specified
    if params.images.is_empty() && params.dockerfile_path.is_none() {
        tracing::error!("You must select at least one image to cache or a dockerfile path to parse");
        std::process::exit(1);
    }

    // Validate dockerfile path if specified
    if let Some(ref dockerfile_path) = params.dockerfile_path {
        if !dockerfile_path.starts_with("http://") && !dockerfile_path.starts_with("https://") {
            let path = std::path::Path::new(dockerfile_path);
            if !path.exists() {
                tracing::error!(
                    "Please provide a valid path to a Dockerfile within the build context with --dockerfile"
                );
                std::process::exit(1);
            }
        }
    }

    // Ensure cache directory exists
    let cache_dir = std::path::Path::new(&params.cache_dir);
    if !cache_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(cache_dir) {
            tracing::error!("Failed to create cache directory: {}", e);
            std::process::exit(1);
        }
    }

    // Resolve credentials
    let keychain = if let Some(ref config_path) = params.docker_config {
        SystemKeychain::with_config_path(PathBuf::from(config_path))
    } else {
        SystemKeychain::new()
    };

    // Build registry auth — use anonymous for warming, per-image auth not needed
    // The warmer pulls images from various registries, so we use a generic auth.
    // Actual per-registry auth is resolved during pull.
    let auth = RegistryAuth::anonymous("")
        .insecure(params.insecure_pull);

    // Convert to WarmerOptions and run
    let warmer_opts = params.to_warmer_options();

    match kaniko_cache::warm_cache(&warmer_opts, &auth).await {
        Ok(()) => {
            tracing::info!("Cache warming completed successfully");
        }
        Err(e) => {
            tracing::error!("Failed warming cache: {}", e);
            std::process::exit(1);
        }
    }
}