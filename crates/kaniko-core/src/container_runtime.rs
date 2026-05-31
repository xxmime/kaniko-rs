//! Container runtime implementation for RUN command execution.
//!
//! This module provides a more complete container runtime implementation
//! that matches Go's SysProcAttr.Credential and chroot functionality.
//!
//! Sandbox flow (matching Go kaniko):
//! 1. `apply_sandbox()` re-executes the process inside `unshare(CLONE_NEWUSER|CLONE_NEWNS)`
//!    so we gain CAP_SYS_ADMIN in the new namespaces.
//! 2. Before each RUN command, `prepare_rootfs()` bind-mounts /proc, /sys, /dev
//!    into the rootfs directory.
//! 3. `execute_in_container()` uses `chroot(rootfs)` to run the command.
//! 4. After the command, `cleanup_rootfs()` unmounts those filesystems.
//! 5. If any mount step fails, we gracefully fall back to running without chroot.

use crate::command::{CommandError, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;

/// Whether sandbox mode is active (set by `apply_sandbox`).
static SANDBOX_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark sandbox mode as active.
pub fn set_sandbox_active(active: bool) {
    SANDBOX_ACTIVE.store(active, Ordering::Relaxed);
}

/// Check if sandbox mode is active.
pub fn is_sandbox_active() -> bool {
    SANDBOX_ACTIVE.load(Ordering::Relaxed)
}

/// Kernel virtual filesystems to bind-mount into the rootfs.
const KERNEL_FS: &[(&str, &str)] = &[
    ("/proc", "proc"),
    ("/sys", "sys"),
    ("/dev", "dev"),
];

/// Container runtime configuration
#[derive(Debug, Clone)]
pub struct ContainerRuntimeConfig {
    /// Root directory for chroot
    pub root_dir: PathBuf,
    /// User to run as (format: "username" or "uid[:gid]")
    pub user: Option<String>,
    /// Environment variables
    pub env: HashMap<String, String>,
    /// Working directory
    pub working_dir: Option<PathBuf>,
    /// Additional groups for the user
    pub additional_groups: Vec<u32>,
}

impl Default for ContainerRuntimeConfig {
    fn default() -> Self {
        Self {
            root_dir: PathBuf::from("/"),
            user: None,
            env: HashMap::new(),
            working_dir: None,
            additional_groups: Vec::new(),
        }
    }
}

/// Prepare the rootfs for chroot execution by bind-mounting kernel filesystems.
///
/// This mirrors Go's `mountSandboxKernelFilesystems`. When sandbox mode is
/// active and we have CAP_SYS_ADMIN, we bind-mount /proc, /sys, /dev into
/// the rootfs so that chrooted processes can access them.
///
/// Returns `true` if at least /proc was mounted successfully.
pub fn prepare_rootfs(root_dir: &PathBuf) -> bool {
    if !is_sandbox_active() {
        return false;
    }

    let mut proc_mounted = false;

    for (src, name) in KERNEL_FS {
        let dest = root_dir.join(name);
        // Create the mount point if it doesn't exist
        if !dest.exists() {
            if let Err(e) = std::fs::create_dir_all(&dest) {
                tracing::debug!("sandbox: failed to create {}: {}", dest.display(), e);
                continue;
            }
        }

        // Try bind-mount
        let output = std::process::Command::new("mount")
            .arg("--bind")
            .arg(src)
            .arg(&dest)
            .output();

        match output {
            Ok(o) if o.status.success() => {
                tracing::debug!("sandbox: bind-mounted {} -> {}", src, dest.display());
                if *name == "proc" {
                    proc_mounted = true;
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::debug!("sandbox: mount {} failed: {}", src, stderr.trim());
            }
            Err(e) => {
                tracing::debug!("sandbox: mount {} error: {}", src, e);
            }
        }
    }

    if !proc_mounted {
        tracing::warn!("sandbox: /proc mount failed, chroot will be disabled for this command");
    }

    proc_mounted
}

/// Clean up the rootfs by unmounting kernel filesystems.
///
/// This mirrors Go's cleanup after a RUN command. Must be called after
/// `prepare_rootfs` to avoid leaking mount points.
pub fn cleanup_rootfs(root_dir: &PathBuf) {
    if !is_sandbox_active() {
        return;
    }

    // Unmount in reverse order
    for (_, name) in KERNEL_FS.iter().rev() {
        let dest = root_dir.join(name);
        if dest.exists() {
            let output = std::process::Command::new("umount")
                .arg(&dest)
                .output();

            match output {
                Ok(o) if o.status.success() => {
                    tracing::debug!("sandbox: unmounted {}", dest.display());
                }
                Ok(o) => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    tracing::debug!("sandbox: umount {} failed: {}", dest.display(), stderr.trim());
                }
                Err(e) => {
                    tracing::debug!("sandbox: umount {} error: {}", dest.display(), e);
                }
            }
        }
    }
}

/// Execute a command in a container-like environment
pub async fn execute_in_container(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let use_chroot = cfg!(target_os = "linux") && config.root_dir != PathBuf::from("/");

    if use_chroot {
        // Prepare rootfs (bind-mount /proc, /sys, /dev)
        let rootfs_ready = prepare_rootfs(&config.root_dir);

        let result = if rootfs_ready {
            // Use chroot with prepared rootfs
            execute_chroot(program, args, config).await
        } else {
            // Fallback: use unshare without chroot, or just run directly
            // Try unshare chroot first, then fall back to direct execution
            match execute_unshare_chroot(program, args, config).await {
                Ok(output) if output.status.success() => Ok(output),
                Ok(output) => {
                    // chroot failed, try without chroot but set PATH from rootfs
                    tracing::warn!("sandbox: chroot execution failed, falling back to direct execution");
                    execute_direct(program, args, config).await
                }
                Err(_) => {
                    tracing::warn!("sandbox: chroot failed, falling back to direct execution");
                    execute_direct(program, args, config).await
                }
            }
        };

        // Clean up mounts
        cleanup_rootfs(&config.root_dir);

        result
    } else {
        // No chroot needed — run directly
        execute_direct(program, args, config).await
    }
}

/// Execute a command using chroot (requires prepared rootfs with /proc etc.)
async fn execute_chroot(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let mut cmd = Command::new("chroot");
    cmd.arg(&config.root_dir);

    // Add user switching if specified
    if let Some(ref user) = config.user {
        // Run as specified user inside chroot
        cmd.arg("su").arg("-").arg(user);
        cmd.arg("-c");
        cmd.arg(&format_command(program, args));
    } else {
        cmd.arg("--");
        cmd.arg(program);
        cmd.args(args);
    }

    // Set environment variables
    cmd.envs(&config.env);

    // Ensure PATH includes common locations
    if !config.env.contains_key("PATH") {
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }

    // Set working directory
    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    // Capture output
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute chroot command: {}", e)))?;

    Ok(output)
}

/// Execute a command using unshare + chroot (fallback when rootfs is not prepared)
async fn execute_unshare_chroot(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let mut cmd = Command::new("unshare");
    cmd.arg("--mount")
        .arg("--uts")
        .arg("--ipc")
        .arg("--pid")
        .arg("--fork")
        .arg("--map-root-user")
        .arg("chroot")
        .arg(&config.root_dir);

    // Add user switching if specified
    if let Some(ref user) = config.user {
        cmd.arg("su").arg("-").arg(user);
    }

    cmd.arg("--").arg(program).args(args);

    // Set environment variables
    cmd.envs(&config.env);

    if !config.env.contains_key("PATH") {
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }

    // Set working directory
    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute unshare command: {}", e)))?;

    Ok(output)
}

/// Execute a command directly (no chroot, no namespace isolation)
async fn execute_direct(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);

    // Set environment variables
    cmd.envs(&config.env);

    // Set working directory
    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute command: {}", e)))?;

    Ok(output)
}

/// Format a command and its arguments as a shell string.
fn format_command(program: &str, args: &[String]) -> String {
    let mut parts = vec![program.to_string()];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

/// Parse user credentials from string (username or uid[:gid])
pub fn parse_user_credentials(user_str: &str) -> Result<(u32, u32, Vec<u32>)> {
    // Try parsing as uid:gid first
    if let Some((uid_str, gid_str)) = user_str.split_once(':') {
        let uid = uid_str.parse::<u32>()
            .map_err(|_| CommandError::Failed(format!("Invalid UID: {}", uid_str)))?;
        let gid = gid_str.parse::<u32>()
            .map_err(|_| CommandError::Failed(format!("Invalid GID: {}", gid_str)))?;
        return Ok((uid, gid, vec![]));
    }

    // Try parsing as numeric uid
    if let Ok(uid) = user_str.parse::<u32>() {
        return Ok((uid, uid, vec![]));
    }

    // Look up user by name
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        
        let c_user = CString::new(user_str)
            .map_err(|_| CommandError::Failed(format!("Invalid username: {}", user_str)))?;
        
        unsafe {
            let pwd = libc::getpwnam(c_user.as_ptr());
            if pwd.is_null() {
                return Err(CommandError::Failed(format!("User not found: {}", user_str)));
            }
            
            let uid = (*pwd).pw_uid;
            let gid = (*pwd).pw_gid;
            
            // Get supplementary groups
            let mut groups = vec![gid];
            let mut ngroups = 0;
            
            // First call to get the number of groups
            if libc::getgrouplist(c_user.as_ptr(), gid, std::ptr::null_mut(), &mut ngroups) == -1 {
                groups.reserve(ngroups as usize);
                groups.resize(ngroups as usize, 0);
                
                if libc::getgrouplist(c_user.as_ptr(), gid, groups.as_mut_ptr(), &mut ngroups) != -1 {
                    groups.truncate(ngroups as usize);
                }
            }
            
            Ok((uid, gid, groups))
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err(CommandError::Failed(format!("User lookup not supported on this platform: {}", user_str)))
    }
}

/// Add default HOME environment variable if not present
pub fn add_default_home(user_str: &str, envs: &mut HashMap<String, String>) -> Result<()> {
    // Check if HOME is already set
    if envs.contains_key("HOME") {
        return Ok(());
    }

    // Default HOME values
    if user_str.is_empty() || user_str == "root" || user_str == "0" {
        envs.insert("HOME".to_string(), "/root".to_string());
        return Ok(());
    }

    // Look up user's home directory
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        
        let c_user = CString::new(user_str)
            .map_err(|_| CommandError::Failed(format!("Invalid username: {}", user_str)))?;
        
        unsafe {
            let pwd = libc::getpwnam(c_user.as_ptr());
            if !pwd.is_null() {
                let home_dir = std::ffi::CStr::from_ptr((*pwd).pw_dir).to_string_lossy();
                envs.insert("HOME".to_string(), home_dir.to_string());
                return Ok(());
            }
        }
    }

    // Fallback: if username provided, use /home/username, otherwise /
    let home = if user_str.parse::<u32>().is_ok() {
        "/".to_string()
    } else {
        format!("/home/{}", user_str)
    };
    
    envs.insert("HOME".to_string(), home);
    Ok(())
}