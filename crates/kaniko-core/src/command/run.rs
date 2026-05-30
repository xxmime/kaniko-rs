use crate::command::{BuildArgs, Result};
use crate::config::ContainerConfig;
use crate::container_runtime::{ContainerRuntimeConfig, execute_in_container, add_default_home};
use std::collections::HashMap;
use std::path::PathBuf;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct RunCommand {
    pub command: Vec<String>,
    pub is_exec_form: bool,
    pub shell: Option<Vec<String>>,
    pub mounts: Vec<String>,
    pub network: Option<String>,
    pub root_dir: String,
}

impl RunCommand {
    pub fn new(command: Vec<String>, is_exec_form: bool) -> Self {
        Self {
            command,
            is_exec_form,
            shell: None,
            mounts: Vec::new(),
            network: None,
            root_dir: "/".to_string(),
        }
    }

    pub fn new_exec(args: Vec<String>, _cache_run: bool) -> Self {
        Self {
            command: args,
            is_exec_form: true,
            shell: None,
            mounts: Vec::new(),
            network: None,
            root_dir: "/".to_string(),
        }
    }

    pub fn new_shell(command: String, _cache_run: bool) -> Self {
        Self {
            command: vec![command],
            is_exec_form: false,
            shell: None,
            mounts: Vec::new(),
            network: None,
            root_dir: "/".to_string(),
        }
    }

    pub fn with_shell(mut self, shell: Vec<String>) -> Self {
        self.shell = Some(shell);
        self
    }

    pub fn with_mount(mut self, mount: String) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn with_mounts(mut self, mounts: Vec<String>) -> Self {
        self.mounts = mounts;
        self
    }

    pub fn with_network(mut self, network: String) -> Self {
        self.network = Some(network);
        self
    }

    pub fn with_root_dir(mut self, root_dir: String) -> Self {
        self.root_dir = root_dir;
        self
    }
}

#[async_trait]
impl super::DockerCommand for RunCommand {
    async fn execute(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()> {
        self.execute_impl(config, args).await
    }

    fn command_string(&self) -> String {
        if self.is_exec_form {
            format!("RUN {:?}", self.command)
        } else {
            format!("RUN {}", self.command.join(" "))
        }
    }

    fn files_to_snapshot(&self) -> Option<Vec<PathBuf>> {
        None
    }

    fn provides_files_to_snapshot(&self) -> bool {
        false
    }

    fn cache_command(&self, _cached_image: &oci_image::mutate::MutableImage) -> Option<Box<dyn super::DockerCommand>> {
        None
    }

    fn files_used_from_context(
        &self,
        _config: &ContainerConfig,
        _args: &BuildArgs,
    ) -> Result<Vec<PathBuf>> {
        Ok(Vec::new())
    }

    fn metadata_only(&self) -> bool {
        false
    }

    fn requires_unpacked_fs(&self) -> bool {
        true
    }

    fn should_cache_output(&self) -> bool {
        false
    }

    fn should_detect_deleted_files(&self) -> bool {
        true
    }

    fn is_args_envs_required_in_cache(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl RunCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()> {
        // Log mount specifications (actual application is handled by container runtime)
        for mount_spec in &self.mounts {
            tracing::info!("RUN --mount: {}", mount_spec);
        }

        // Log network mode (actual network isolation is limited in kaniko)
        if let Some(ref network) = self.network {
            tracing::info!("RUN --network={}", network);
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
                return Err(crate::command::CommandError::Failed("RUN exec form requires at least one argument".into()));
            }
            (self.command[0].clone(), self.command[1..].to_vec())
        } else {
            let default_shell = vec!["/bin/sh".to_string(), "-c".to_string()];
            let shell = self.shell.as_deref().unwrap_or(&default_shell);
            if shell.len() < 2 {
                return Err(crate::command::CommandError::Failed("shell must have at least 2 elements".into()));
            }
            let mut full_args = shell[1..].to_vec();
            full_args.push(cmd_str.clone());
            (shell[0].clone(), full_args)
        };

        // Prepare environment variables
        let mut env_map = HashMap::new();
        if let Some(ref env_vars) = config.env {
            for env_var in env_vars {
                if let Some(pos) = env_var.find('=') {
                    let key = &env_var[..pos];
                    let value = &env_var[pos + 1..];
                    env_map.insert(key.to_string(), value.to_string());
                }
            }
        }

        // Add default HOME if not present
        add_default_home(
            config.user.as_deref().unwrap_or(""),
            &mut env_map
        )?;

        // Apply build args to environment
        if let Some(ref env_vars) = config.env {
            let replacement_envs = args.replacement_envs(env_vars);
            for env_var in &replacement_envs {
                if let Some(pos) = env_var.find('=') {
                    let key = &env_var[..pos];
                    let value = &env_var[pos + 1..];
                    env_map.insert(key.to_string(), value.to_string());
                }
            }
        }

        // Prepare container runtime configuration
        let container_config = ContainerRuntimeConfig {
            root_dir: PathBuf::from(&self.root_dir),
            user: config.user.clone(),
            env: env_map,
            working_dir: config.working_dir.as_ref().map(|s| PathBuf::from(s)),
            additional_groups: Vec::new(),
        };

        // Execute command with full container runtime support
        let result = execute_in_container(&program, &cmd_args, &container_config).await?;

        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(crate::command::CommandError::Failed(format!("RUN command exited with {}: {}", result.status, stderr.trim())));
        }

        if !result.stdout.is_empty() {
            tracing::debug!("RUN stdout: {}", String::from_utf8_lossy(&result.stdout).trim());
        }

        Ok(())
    }
}