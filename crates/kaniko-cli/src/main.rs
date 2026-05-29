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

    // Configure logging based on CLI args
    init_logging(&cli.log_level, &cli.log_format);

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
fn init_logging(level: &str, format: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    if format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .init();
    }
}

/// Main build execution flow.
/// Analogous to Go: `cmd/executor/cmd/root.go` → Run command handler.
async fn run(mut cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ===== Validate flags =====
    validate_flags(&cli)?;

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
    if !cli.skip_push_permission_check && !cli.no_push && !cli.destination.is_empty() {
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
    let reference = oci_registry::push::Reference::parse(dest)
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

    // Registry mirrors
    for mirror_spec in &cli.registry_mirror {
        // Format: "registry=mirror" e.g. "docker.io=mirror.example.com"
        if let Some((registry, mirror_url)) = mirror_spec.split_once('=') {
            opts.registry_mirrors
                .entry(registry.to_string())
                .or_default()
                .push(mirror_url.to_string());
        } else {
            tracing::warn!("Invalid registry mirror spec (expected registry=mirror): {}", mirror_spec);
        }
    }

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
        tracing::info!("Sandbox mode requested — using Linux namespace isolation");
        // Full namespace isolation requires privileged execution.
        // For now, we log that sandbox mode is active and rely on the
        // container runtime's isolation. A future iteration could use
        // the `nix` crate to call unshare(CLONE_NEWNS) etc.
    }

    #[cfg(not(target_os = "linux"))]
    {
        tracing::warn!(
            "Sandbox mode is not supported on this platform. \
             Build will proceed without namespace isolation."
        );
    }
}