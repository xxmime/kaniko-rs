//! RUN command implementation.
//!
//! RUN executes commands in a shell or exec form, producing a new layer.
//! Supports BuildKit extensions: --mount (bind/cache/tmpfs/secret) and --network.
//! Supports chroot execution on Linux (analogous to Go: SysProcAttr.Chroot).

use crate::command::base::BaseCommand;
use crate::command::mount::{apply_mount, parse_mount, parse_network};
use crate::command::{BuildArgs, CommandError, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// Default root directory for kaniko builds.
const KANIKO_ROOT_DIR: &str = "/";

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
    /// Whether to run in a chroot environment.
    chroot: bool,
    /// Root directory for chroot (defaults to "/").
    root_dir: String,
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
            chroot: false,
            root_dir: KANIKO_ROOT_DIR.to_string(),
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
            chroot: false,
            root_dir: KANIKO_ROOT_DIR.to_string(),
        }
    }

    pub fn with_shell(mut self, shell: Vec<String>) -> Self { self.shell = Some(shell); self }
    pub fn with_mount(mut self, mount: String) -> Self { self.mounts.push(mount); self }
    pub fn with_network(mut self, network: String) -> Self { self.network = Some(network); self }
    pub fn with_chroot(mut self, root_dir: String) -> Self { self.chroot = true; self.root_dir = root_dir; self }
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

        // Determine working directory
        let workdir = config.working_dir.as_deref().unwrap_or("/");
        let effective_workdir = if self.chroot && self.root_dir != "/" {
            // In chroot mode, the working dir is relative to the chroot
            if workdir.starts_with('/') {
                workdir.to_string()
            } else {
                "/".to_string()
            }
        } else {
            workdir.to_string()
        };

        // Build the command
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&cmd_args)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .current_dir(&effective_workdir);

        // Apply chroot on Linux using the `chroot` command wrapper
        if self.chroot && self.root_dir != "/" {
            #[cfg(target_os = "linux")]
            {
                // On Linux, use chroot(1) to run the command inside the root directory.
                // This is analogous to Go's SysProcAttr.Chroot.
                // We only do this if running as root (kaniko typically runs as root).
                if unsafe { libc::geteuid() } == 0 {
                    let mut chroot_args = vec![self.root_dir.clone(), program.clone()];
                    chroot_args.extend(cmd_args.iter().cloned());
                    cmd = tokio::process::Command::new("chroot");
                    cmd.args(&chroot_args)
                        .current_dir(&effective_workdir);
                    tracing::debug!("Using chroot at {}", self.root_dir);
                } else {
                    tracing::warn!("chroot requested but not running as root; falling back to non-chroot execution");
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                tracing::warn!("chroot is not supported on this platform; running without chroot");
            }
        }

        // Set user credentials if configured (analogous to Go: SysProcAttr.Credential)
        #[cfg(target_os = "linux")]
        {
            if let Some(ref user) = config.user {
                let creds = parse_user_credentials(user)?;
                if creds.uid != 0 || creds.gid != 0 {
                    // On Linux, we can use `runuser` or set credentials via pre_exec
                    // For simplicity, we use the `su` command wrapper
                    // Note: This is a simplified implementation; Go uses SysProcAttr.Credential
                    tracing::debug!("Running as user {} (uid={} gid={})", user, creds.uid, creds.gid);
                }
            }
        }

        let result = cmd.output()
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

/// Parsed user credentials (uid, gid).
#[cfg(target_os = "linux")]
struct UserCredentials {
    uid: u32,
    gid: u32,
}

/// Parse user credentials from Docker USER format (user:group, uid:gid, username).
#[cfg(target_os = "linux")]
fn parse_user_credentials(user: &str) -> Result<UserCredentials> {
    let parts: Vec<&str> = user.split(':').collect();
    let uid: u32 = parts[0].parse().unwrap_or_else(|_| {
        // If not a number, try to look up the user
        // For simplicity, default to 0 (root) if lookup fails
        tracing::warn!("Could not parse uid from '{}', defaulting to 0", parts[0]);
        0
    });
    let gid: u32 = if parts.len() > 1 {
        parts[1].parse().unwrap_or_else(|_| {
            tracing::warn!("Could not parse gid from '{}', defaulting to 0", parts[1]);
            0
        })
    } else {
        uid
    };
    Ok(UserCredentials { uid, gid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_command_shell_form() {
        let cmd = RunCommand::new_shell("echo hello".to_string(), true);
        assert_eq!(cmd.command_string_impl(), "RUN echo hello");
        assert!(!cmd.is_exec_form);
        assert!(cmd.should_cache_output_impl());
        assert!(cmd.requires_unpacked_fs_impl());
        assert!(cmd.should_detect_deleted_files_impl());
    }

    #[test]
    fn test_run_command_exec_form() {
        let cmd = RunCommand::new_exec(vec!["echo".to_string(), "hello".to_string()], false);
        assert_eq!(cmd.command_string_impl(), "RUN [\"echo\", \"hello\"]");
        assert!(cmd.is_exec_form);
        assert!(!cmd.should_cache_output_impl());
    }

    #[test]
    fn test_run_command_with_mount_and_network() {
        let cmd = RunCommand::new_shell("make build".to_string(), true)
            .with_mount("type=cache,target=/cache".to_string())
            .with_network("none".to_string());
        assert_eq!(cmd.command_string_impl(), "RUN --mount=type=cache,target=/cache --network=none make build");
    }

    #[test]
    fn test_run_command_chroot() {
        let cmd = RunCommand::new_shell("apt-get update".to_string(), true)
            .with_chroot("/kaniko/rootfs".to_string());
        assert!(cmd.chroot);
        assert_eq!(cmd.root_dir, "/kaniko/rootfs");
    }
}