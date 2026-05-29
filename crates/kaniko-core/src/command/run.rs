//! RUN command implementation.
//!
//! RUN executes commands in a shell or exec form, producing a new layer.
//! Supports BuildKit extensions: --mount (bind/cache/tmpfs/secret) and --network.

use crate::command::base::BaseCommand;
use crate::command::mount::{apply_mount, parse_mount, parse_network};
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// RUN instruction — executes a command during the build.
#[derive(Debug)]
pub struct RunCommand {
    /// The command to run (shell form as single string, exec form as separate args).
    command: Vec<String>,
    /// Whether this is in exec form (JSON array in Dockerfile).
    is_exec_form: bool,
    /// Shell to use when not in exec form (default: ["/bin/sh", "-c"]).
    shell: Option<Vec<String>>,
    /// Mount specifications (--mount flags).
    mounts: Vec<String>,
    /// Whether to cache this layer.
    should_cache: bool,
    /// Network mode for the RUN command (--network flag).
    network: Option<String>,
}

impl RunCommand {
    pub fn new_shell(command: String, should_cache: bool) -> Self {
        Self {
            command: vec![command],
            is_exec_form: false,
            shell: None,
            mounts: vec![],
            should_cache,
            network: None,
        }
    }

    pub fn new_exec(args: Vec<String>, should_cache: bool) -> Self {
        Self {
            command: args,
            is_exec_form: true,
            shell: None,
            mounts: vec![],
            should_cache,
            network: None,
        }
    }

    pub fn with_shell(mut self, shell: Vec<String>) -> Self { self.shell = Some(shell); self }
    pub fn with_mount(mut self, mount: String) -> Self { self.mounts.push(mount); self }
    pub fn with_network(mut self, network: String) -> Self { self.network = Some(network); self }
}

#[async_trait]
impl BaseCommand for RunCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        // Apply mount specifications before running the command
        for mount_spec in &self.mounts {
            let mount = parse_mount(mount_spec)?;
            apply_mount(&mount)?;
            tracing::info!("Applied RUN --mount: type={}, target={}", mount.mount_type, mount.target);
        }

        // Parse network mode (for logging; actual network isolation is limited in kaniko)
        if let Some(ref network) = self.network {
            let net_mode = parse_network(network)?;
            tracing::info!("RUN --network={}", net_mode);
            // Note: kaniko runs as a daemon-less builder, so network isolation
            // is limited. We log the mode but can't fully enforce it.
        }

        let cmd_str = if self.is_exec_form {
            format!("{:?}", self.command)
        } else {
            self.command.first().cloned().unwrap_or_default()
        };
        tracing::info!("RUN {}", cmd_str);

        let (program, cmd_args) = if self.is_exec_form {
            if self.command.is_empty() {
                return Err(CommandError::Failed("RUN exec form requires at least one argument".into()));
            }
            (self.command[0].clone(), self.command[1..].to_vec())
        } else {
            let default_shell = vec!["/bin/sh".to_string(), "-c".to_string()];
            let shell = self.shell.as_deref().unwrap_or(&default_shell);
            if shell.len() < 2 {
                return Err(CommandError::Failed("shell must have at least 2 elements".into()));
            }
            let mut full_args = shell[1..].to_vec();
            full_args.push(cmd_str.clone());
            (shell[0].clone(), full_args)
        };

        let result = tokio::process::Command::new(&program)
            .args(&cmd_args)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .current_dir(config.working_dir.as_deref().unwrap_or("/"))
            .output()
            .await
            .map_err(|e| CommandError::Failed(format!("RUN command failed: {}", e)))?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(CommandError::Failed(format!(
                "RUN command exited with {}: {}", result.status, stderr.trim()
            )));
        }

        if !result.stdout.is_empty() {
            tracing::debug!("RUN stdout: {}", String::from_utf8_lossy(&result.stdout).trim());
        }

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        let mut parts = Vec::new();
        for mount in &self.mounts {
            parts.push(format!("--mount={}", mount));
        }
        if let Some(ref network) = self.network {
            parts.push(format!("--network={}", network));
        }
        if self.is_exec_form {
            parts.push(format!("{:?}", self.command));
        } else {
            parts.push(self.command.first().cloned().unwrap_or_default());
        }
        format!("RUN {}", parts.join(" "))
    }

    fn metadata_only_impl(&self) -> bool { false }
    fn requires_unpacked_fs_impl(&self) -> bool { true }
    fn should_cache_output_impl(&self) -> bool { self.should_cache }
    fn should_detect_deleted_files_impl(&self) -> bool { true }
    fn is_args_envs_required_in_cache_impl(&self) -> bool { true }
    fn provides_files_to_snapshot_impl(&self) -> bool { false }
}