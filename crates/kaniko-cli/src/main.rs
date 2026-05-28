//! kaniko-rs: Build container images in Kubernetes without a daemon.
//!
//! This is the CLI entry point, analogous to Go: `cmd/executor/main.go`.

mod args;

use args::Cli;
use clap::Parser;
use kaniko_core::command::{
    AddCommand, ArgCommand, BuildArgs, CmdCommand, CopyCommand,
    EntrypointCommand, EnvCommand, ExposeCommand, HealthCheckCommand, LabelCommand,
    OnBuildCommand, RunCommand, ShellCommand, StopSignalCommand, UserCommand,
    VolumeCommand, WorkdirCommand, DockerCommand,
};
use kaniko_creds::keychain::SystemKeychain;
use oci_image::mutate::MutableImage;
use oci_registry::auth::RegistryAuth;
use oci_registry::pull::pull_image;
use oci_registry::push::push_image;
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    tracing::info!("kaniko-rs executor starting");

    match run(cli).await {
        Ok(()) => {
            tracing::info!("Build completed successfully");
        }
        Err(e) => {
            tracing::error!("Build failed: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dockerfile_path = cli.dockerfile.as_deref().unwrap_or("Dockerfile");
    let context_dir = cli.context.as_deref().unwrap_or(".");
    let context_path = PathBuf::from(context_dir);

    // Parse build arguments - now already parsed as key-value pairs
    let mut build_args_map: HashMap<String, String> = HashMap::new();
    for (key, value) in &cli.build_arg {
        build_args_map.insert(key.clone(), value.clone());
    }

    tracing::info!("Dockerfile: {}", dockerfile_path);
    tracing::info!("Context: {}", context_dir);
    tracing::info!("Destinations: {:?}", cli.destination);

    // Step 1: Parse Dockerfile
    let dockerfile_content = std::fs::read_to_string(dockerfile_path)
        .map_err(|e| format!("Failed to read Dockerfile {}: {}", dockerfile_path, e))?;
    let stages = dockerfile_parser::parse_dockerfile(&dockerfile_content)
        .map_err(|e| format!("Failed to parse Dockerfile: {}", e))?;

    tracing::info!("Parsed {} stage(s)", stages.len());

    // Step 2: Resolve credentials
    let keychain = SystemKeychain::new();

    // Step 3: Build each stage
    let target_stage = cli.target.as_deref();
    let last_stage_idx = stages.len() - 1;

    for (stage_idx, stage) in stages.iter().enumerate() {
        let stage_name = stage.alias.as_deref().unwrap_or("default");
        tracing::info!("=== Building stage {}/{}: {} ===", stage_idx + 1, stages.len(), stage_name);

        // Skip if target is specified and this isn't it
        if let Some(target) = target_stage {
            if stage_name != target && &format!("{}", stage_idx) != target {
                if stage_idx != last_stage_idx {
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
            let credential = keychain.credentials(&registry).unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
            let auth = RegistryAuth::new(&registry, credential)
                .insecure(cli.insecure);

            match pull_image(&stage.image, &auth).await {
                Ok(img) => img,
                Err(e) => {
                    tracing::error!("Failed to pull base image {}: {}", stage.image, e);
                    return Err(format!("Failed to pull base image: {}", e).into());
                }
            }
        };

        // Execute commands in this stage
        let mut container_config = image.config.config.clone();
        let build_args = BuildArgs {
            args: vec![],
            env: vec![],
            build_args: build_args_map.clone(),
        };

        for instruction in &stage.instructions {
            let cmd_result = match instruction {
                dockerfile_parser::Instruction::From(_) => {
                    // Already handled above
                    continue;
                }
                dockerfile_parser::Instruction::Env(env) => {
                    let c = EnvCommand::new(env.key.clone(), env.value.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Label(label) => {
                    let c = LabelCommand::new(label.labels.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Expose(expose) => {
                    let c = ExposeCommand::new(expose.ports.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::User(user) => {
                    let c = UserCommand::new(user.user.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Workdir(workdir) => {
                    let c = WorkdirCommand::new(workdir.path.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Copy(copy) => {
                    let c = CopyCommand::with_flags(
                        copy.sources.clone(),
                        copy.destination.clone(),
                        copy.from.clone(),
                        copy.chown.clone(),
                        copy.chmod.clone(),
                        copy.link,
                        context_path.clone(),
                        cli.cache,
                    );
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Add(add) => {
                    let c = AddCommand::with_flags(
                        add.sources.clone(),
                        add.destination.clone(),
                        add.chown.clone(),
                        add.chmod.clone(),
                        add.link,
                        context_path.clone(),
                        cli.cache,
                    );
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Run(run) => {
                    let c = if run.is_shell_form {
                        RunCommand::new_shell(run.command.clone(), cli.cache)
                    } else {
                        // Exec form: split command by spaces for simple parsing
                        let args: Vec<String> = run.command.split_whitespace().map(String::from).collect();
                        RunCommand::new_exec(args, cli.cache)
                    };
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Cmd(cmd) => {
                    let c = if cmd.is_shell_form {
                        CmdCommand::new_shell(cmd.command.first().cloned().unwrap_or_default())
                    } else {
                        CmdCommand::new_exec(cmd.command.clone())
                    };
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Entrypoint(ep) => {
                    let c = if ep.is_shell_form {
                        EntrypointCommand::new_shell(ep.command.first().cloned().unwrap_or_default())
                    } else {
                        EntrypointCommand::new_exec(ep.command.clone())
                    };
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Volume(vol) => {
                    let c = VolumeCommand::new(vol.paths.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Arg(arg) => {
                    let c = ArgCommand::new(arg.name.clone(), arg.default_value.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Shell(shell) => {
                    let c = ShellCommand::new(shell.shell.clone());
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::StopSignal(sig) => {
                    let c = StopSignalCommand::new(sig.signal.clone());
                    c.execute(&mut container_config, &build_args).await
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
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Onbuild(ob) => {
                    let trigger = format!("{:?}", ob.instruction);
                    let c = OnBuildCommand::new(trigger);
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Maintainer(m) => {
                    let c = LabelCommand::new(vec![("maintainer".to_string(), m.name.clone())]);
                    c.execute(&mut container_config, &build_args).await
                }
                dockerfile_parser::Instruction::Comment(_) => {
                    // Skip comments
                    continue;
                }
            };

            if let Err(e) = cmd_result {
                tracing::error!("Command failed: {}", e);
                return Err(format!("Build command failed: {}", e).into());
            }
        }

        // Update the image config with the modified container config
        image.config.config = container_config;
        image.config_bytes = serde_json::to_vec(&image.config)?;

        // Only push the last stage (or target stage)
        if stage_idx == last_stage_idx || target_stage.is_some() {
            // Step 4: Push image to destinations
            if !cli.no_push && !cli.destination.is_empty() {
                for dest in &cli.destination {
                    tracing::info!("Pushing to {}", dest);
                    let registry = extract_registry(dest);
                    let credential = keychain.credentials(&registry)
                        .unwrap_or_else(|_| kaniko_creds::Credential::anonymous());
                    let auth = RegistryAuth::new(&registry, credential)
                        .insecure(cli.insecure);

                    push_image(&image, dest, &auth).await?;
                    tracing::info!("Successfully pushed to {}", dest);
                }
            }

            // Write to tar if requested
            if let Some(ref tar_path) = cli.tar_path {
                tracing::info!("Writing image tar to {}", tar_path);
                write_image_tar(&image, tar_path)?;
            }

            // Write digest file if requested
            if let Some(ref digest_file) = cli.digest_file {
                let digest = image.digest().to_string();
                std::fs::write(digest_file, &digest)?;
                tracing::info!("Image digest: {}", digest);
            }

            // Write to OCI layout if requested
            if let Some(ref layout_path) = cli.oci_layout_path {
                tracing::info!("Writing OCI layout to {}", layout_path);
                oci_image::layout::write_layout(&image, std::path::Path::new(layout_path))?;
            }

            break; // Only build one target
        }
    }

    Ok(())
}

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