//! Dockerfile command trait and implementations.
//!
//! Analogous to Go: `pkg/commands/commands.go` — `DockerCommand` interface.

use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use oci_image::layer::Layer;
use oci_image::mutate::MutableImage;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

/// Errors during command execution.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("walk error: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("command failed: {0}")]
    Failed(String),
}

/// Result type for command operations.
pub type Result<T> = std::result::Result<T, CommandError>;

/// Build arguments passed to commands.
#[derive(Debug, Clone, Default)]
pub struct BuildArgs {
    /// ARG key-value pairs.
    pub args: Vec<(String, Option<String>)>,
    /// Resolved environment variables.
    pub env: Vec<(String, String)>,
    /// Build-time ARG overrides (--build-arg KEY=VALUE).
    pub build_args: HashMap<String, String>,
}

impl BuildArgs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve environment variables with ARG replacements
    pub fn replacement_envs(&self, env_vars: &[String]) -> Vec<String> {
        let mut result = Vec::new();
        
        for env_var in env_vars {
            if let Some(pos) = env_var.find('=') {
                let key = &env_var[..pos];
                let value = &env_var[pos + 1..];
                // Resolve ARG replacements in the value
                let resolved_value = self.resolve_arg_replacements(value);
                result.push(format!("{}={}", key, resolved_value));
            }
        }
        
        result
    }

    /// Resolve ARG replacements in a string
    fn resolve_arg_replacements(&self, value: &str) -> String {
        let mut result = value.to_string();
        
        // Simple ARG replacement: ${VAR} and $VAR
        for (arg_name, arg_value) in &self.args {
            let arg_val = arg_value.as_deref().unwrap_or("");
            
            // Replace ${VAR}
            result = result.replace(&format!("${{{}}}", arg_name), arg_val);
            // Replace $VAR (but not $VAR_ or $VAR_SUFFIX)
            let pattern = format!("${}", arg_name);
            result = result.replace(&pattern, arg_val);
        }
        
        // Also check build_args (CLI overrides)
        for (arg_name, arg_value) in &self.build_args {
            let pattern1 = format!("${{{}}}", arg_name);
            let pattern2 = format!("${}", arg_name);
            result = result.replace(&pattern1, arg_value);
            result = result.replace(&pattern2, arg_value);
        }
        
        result
    }
}

/// Dockerfile command execution trait.
///
/// Analogous to Go: `commands.DockerCommand`.
#[async_trait]
pub trait DockerCommand: Send + Sync + fmt::Debug {
    /// Execute the command: modify filesystem + update image config.
    async fn execute(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()>;

    /// String representation of the command.
    fn command_string(&self) -> String;

    /// Files that need to be snapshotted after execution.
    fn files_to_snapshot(&self) -> Option<Vec<PathBuf>>;

    /// Whether this command can provide a list of files to snapshot.
    fn provides_files_to_snapshot(&self) -> bool;

    /// Return a cache-aware implementation of this command, if available.
    fn cache_command(&self, cached_image: &MutableImage) -> Option<Box<dyn DockerCommand>>;

    /// Files used from the build context.
    fn files_used_from_context(
        &self,
        config: &ContainerConfig,
        args: &BuildArgs,
    ) -> Result<Vec<PathBuf>>;

    /// Whether this command only modifies metadata (no filesystem changes).
    fn metadata_only(&self) -> bool;

    /// Whether this command requires an unpacked filesystem.
    fn requires_unpacked_fs(&self) -> bool;

    /// Whether the output layer should be cached.
    fn should_cache_output(&self) -> bool;

    /// Whether this command could delete files.
    fn should_detect_deleted_files(&self) -> bool;

    /// Whether cache key computation needs ARGs/ENVs.
    fn is_args_envs_required_in_cache(&self) -> bool;

    /// Support downcasting for cache command detection.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Marker trait for cached commands.
pub trait CachedCommand: DockerCommand {
    fn layer(&self) -> Result<Layer>;
}

/// Composite cache key for layer caching.
/// Re-exported from the composite_key module for backward compatibility.
pub use crate::composite_key::CompositeCache;

// Sub-modules with individual command implementations.
mod env;
mod label;
mod expose;
mod user;
mod workdir;
mod copy;
mod add;
mod run;
mod cmd;
mod entrypoint;
mod volume;
mod arg;
mod shell;
mod stopsignal;
mod healthcheck;
mod onbuild;
mod base;
mod cache_command;
mod mount;
mod run_marker;

pub use base::BaseCommand;
pub use cache_command::{CachingCopyCommand, CachingRunCommand};
pub use mount::{MountSpec, MountType, NetworkMode, parse_mount, parse_network, apply_mount};
pub use run_marker::RunMarkerCommand;
pub use env::EnvCommand;
pub use label::LabelCommand;
pub use expose::ExposeCommand;
pub use user::UserCommand;
pub use workdir::WorkdirCommand;
pub use copy::CopyCommand;
pub use add::AddCommand;
pub use run::RunCommand;
pub use cmd::CmdCommand;
pub use entrypoint::EntrypointCommand;
pub use volume::VolumeCommand;
pub use arg::ArgCommand;
pub use shell::ShellCommand;
pub use stopsignal::StopSignalCommand;
pub use healthcheck::HealthCheckCommand;
pub use onbuild::OnBuildCommand;

/// Create a DockerCommand from a parsed Dockerfile instruction.
///
/// This is the main factory function for creating command objects from
/// the parser's instruction types. It maps each instruction variant
/// to its corresponding DockerCommand implementation.
///
/// Analogous to Go: `commands.GetCommand()`.
pub fn get_command(
    instruction: &dockerfile_parser::instruction::Instruction,
    context_dir: std::path::PathBuf,
    cache_copy: bool,
    cache_run: bool,
) -> Result<Box<dyn DockerCommand>> {
    use dockerfile_parser::instruction::Instruction;

    match instruction {
        Instruction::Run(run_inst) => {
            let cmd = if run_inst.is_shell_form {
                RunCommand::new_shell(run_inst.command.clone(), cache_run)
            } else {
                RunCommand::new_exec(run_inst.args.clone(), cache_run)
            };
            let mut cmd = cmd;
            for mount in &run_inst.mounts {
                cmd = cmd.with_mount(mount.clone());
            }
            if let Some(ref network) = run_inst.network {
                cmd = cmd.with_network(network.clone());
            }
            Ok(Box::new(cmd))
        }
        Instruction::Copy(copy_inst) => {
            let cmd = CopyCommand::with_flags(
                copy_inst.sources.clone(),
                copy_inst.destination.clone(),
                copy_inst.from.clone(),
                copy_inst.chown.clone(),
                copy_inst.chmod.clone(),
                copy_inst.link,
                context_dir,
                cache_copy,
            );
            Ok(Box::new(cmd))
        }
        Instruction::Add(add_inst) => {
            let cmd = AddCommand::with_flags(
                add_inst.sources.clone(),
                add_inst.destination.clone(),
                add_inst.chown.clone(),
                add_inst.chmod.clone(),
                add_inst.link,
                context_dir,
                cache_copy,
            );
            Ok(Box::new(cmd))
        }
        Instruction::Env(env_inst) => {
            Ok(Box::new(EnvCommand::new(env_inst.key.clone(), env_inst.value.clone())))
        }
        Instruction::Label(label_inst) => {
            Ok(Box::new(LabelCommand::new(label_inst.labels.clone())))
        }
        Instruction::Expose(expose_inst) => {
            Ok(Box::new(ExposeCommand::new(expose_inst.ports.clone())))
        }
        Instruction::User(user_inst) => {
            Ok(Box::new(UserCommand::new(user_inst.user.clone())))
        }
        Instruction::Workdir(workdir_inst) => {
            Ok(Box::new(WorkdirCommand::new(workdir_inst.path.clone())))
        }
        Instruction::Cmd(cmd_inst) => {
            if cmd_inst.is_shell_form {
                Ok(Box::new(CmdCommand::new_shell(cmd_inst.command.first().cloned().unwrap_or_default())))
            } else {
                Ok(Box::new(CmdCommand::new_exec(cmd_inst.command.clone())))
            }
        }
        Instruction::Entrypoint(ep_inst) => {
            if ep_inst.is_shell_form {
                Ok(Box::new(EntrypointCommand::new_shell(ep_inst.command.first().cloned().unwrap_or_default())))
            } else {
                Ok(Box::new(EntrypointCommand::new_exec(ep_inst.command.clone())))
            }
        }
        Instruction::Volume(vol_inst) => {
            Ok(Box::new(VolumeCommand::new(vol_inst.paths.clone())))
        }
        Instruction::Arg(arg_inst) => {
            Ok(Box::new(ArgCommand::new(arg_inst.name.clone(), arg_inst.default_value.clone())))
        }
        Instruction::Shell(shell_inst) => {
            Ok(Box::new(ShellCommand::new(shell_inst.shell.clone())))
        }
        Instruction::StopSignal(ss_inst) => {
            Ok(Box::new(StopSignalCommand::new(ss_inst.signal.clone())))
        }
        Instruction::Healthcheck(hc_inst) => {
            if hc_inst.is_none {
                Ok(Box::new(HealthCheckCommand::none()))
            } else {
                let test = hc_inst.cmd.as_ref()
                    .map(|c| vec![c.clone()])
                    .unwrap_or_default();
                Ok(Box::new(HealthCheckCommand::new(
                    test,
                    hc_inst.interval.clone(),
                    hc_inst.timeout.clone(),
                    hc_inst.start_period.clone(),
                    hc_inst.retries,
                )))
            }
        }
        Instruction::Onbuild(ob_inst) => {
            // ONBUILD stores the trigger as a string representation of the inner instruction
            let inner_cmd = get_command(&ob_inst.instruction, context_dir, cache_copy, cache_run);
            let trigger = match inner_cmd {
                Ok(cmd) => cmd.command_string(),
                Err(_) => format!("{:?}", ob_inst.instruction),
            };
            Ok(Box::new(OnBuildCommand::new(trigger)))
        }
        Instruction::Maintainer(m_inst) => {
            // MAINTAINER is deprecated, skip with a warning
            tracing::warn!("MAINTAINER is deprecated, skipping: {}", m_inst.name);
            Err(CommandError::Failed(format!("MAINTAINER is deprecated: {}", m_inst.name)))
        }
        Instruction::From(_) => {
            Err(CommandError::Failed("FROM instruction should not be converted to a command".to_string()))
        }
        Instruction::Comment(_) => {
            Err(CommandError::Failed("Comment should not be converted to a command".to_string()))
        }
    }
}