//! Container runtime implementation for RUN command execution.
//!
//! Sandbox flow (matching Go kaniko):
//! 1. `apply_sandbox()` re-executes the process inside `unshare(CLONE_NEWUSER|CLONE_NEWNS)`
//!    so we gain CAP_SYS_ADMIN in the new namespaces.
//! 2. Before each RUN command, `prepare_rootfs()` mounts /proc, /sys, /dev
//!    into the rootfs directory.
//! 3. `execute_in_container()` uses `chroot(rootfs)` to run the command.
//! 4. After the command, `cleanup_rootfs()` unmounts those filesystems.
//! 5. If mount fails, we still try chroot (many commands work without /proc).

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

/// Filesystems to mount into the rootfs.
/// (mount_type, source, mount_point_name)
const ROOTFS_MOUNTS: &[(&str, &str, &str)] = &[
    ("proc", "proc", "proc"),       // mount -t proc proc /sandbox/proc
    ("sysfs", "sysfs", "sys"),      // mount -t sysfs sysfs /sandbox/sys
    ("bind", "/dev", "dev"),        // mount --bind /dev /sandbox/dev
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

/// Prepare the rootfs for chroot execution by mounting kernel filesystems.
///
/// This mirrors Go's `mountSandboxKernelFilesystems`:
/// - /proc: `mount -t proc` (new procfs instance, not bind-mount)
/// - /sys:  `mount -t sysfs` (new sysfs instance)
/// - /dev:  `mount --bind /dev` (bind-mount host /dev)
///
/// Returns `true` if at least /proc was mounted successfully.
pub fn prepare_rootfs(root_dir: &PathBuf) -> bool {
    if !is_sandbox_active() {
        return false;
    }

    let mut proc_mounted = false;

    for (mount_type, source, name) in ROOTFS_MOUNTS {
        let dest = root_dir.join(name);
        // Create the mount point if it doesn't exist
        if !dest.exists() {
            if let Err(e) = std::fs::create_dir_all(&dest) {
                tracing::debug!("sandbox: failed to create {}: {}", dest.display(), e);
                continue;
            }
        }

        let output = if *mount_type == "bind" {
            std::process::Command::new("mount")
                .arg("--bind")
                .arg(source)
                .arg(&dest)
                .output()
        } else {
            // mount -t <type> <source> <dest>
            std::process::Command::new("mount")
                .arg("-t")
                .arg(mount_type)
                .arg(source)
                .arg(&dest)
                .output()
        };

        match output {
            Ok(o) if o.status.success() => {
                tracing::debug!("sandbox: mounted {} ({}) -> {}", source, mount_type, dest.display());
                if *name == "proc" {
                    proc_mounted = true;
                }
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                tracing::warn!("sandbox: mount {} ({}) failed: {}", source, mount_type, stderr.trim());
            }
            Err(e) => {
                tracing::warn!("sandbox: mount {} ({}) error: {}", source, mount_type, e);
            }
        }
    }

    if !proc_mounted {
        tracing::warn!("sandbox: /proc mount failed, some commands may not work");
    }

    proc_mounted
}

/// Clean up the rootfs by unmounting kernel filesystems.
pub fn cleanup_rootfs(root_dir: &PathBuf) {
    if !is_sandbox_active() {
        return;
    }

    // Unmount in reverse order
    for (_, _, name) in ROOTFS_MOUNTS.iter().rev() {
        let dest = root_dir.join(name);
        // Check if it's actually a mount point before trying to unmount
        let check = std::process::Command::new("mountpoint")
            .arg("-q")
            .arg(&dest)
            .status();

        match check {
            Ok(s) if s.success() => {
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
            _ => {
                tracing::debug!("sandbox: {} is not a mount point, skipping", dest.display());
            }
        }
    }
}

/// Execute a command in a container-like environment.
///
/// When sandbox is active and root_dir is set, we:
/// 1. Prepare rootfs (mount /proc, /sys, /dev)
/// 2. chroot into rootfs and execute
/// 3. Clean up mounts
///
/// If sandbox is not active, falls back to unshare+chroot or direct execution.
pub async fn execute_in_container(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let use_chroot = cfg!(target_os = "linux") && config.root_dir != PathBuf::from("/");

    if use_chroot && is_sandbox_active() {
        // Sandbox path: prepare rootfs, then chroot directly
        // (we're already in the user namespace from apply_sandbox)
        let _proc_mounted = prepare_rootfs(&config.root_dir);

        // Always chroot — the build MUST run inside the container rootfs.
        // Even without /proc mounted, many commands still work.
        // Commands that need /proc will produce warnings but can often
        // continue (e.g. apt with APT::Sandbox::User=root).
        let result = execute_chroot(program, args, config).await;
        cleanup_rootfs(&config.root_dir);
        result
    } else if use_chroot {
        // Non-sandbox path: use unshare+chroot
        let result = execute_unshare_chroot(program, args, config).await;
        match result {
            Ok(output) if output.status.success() => Ok(output),
            Ok(_) | Err(_) => {
                tracing::warn!("chroot execution failed, falling back to direct execution");
                execute_direct(program, args, config).await
            }
        }
    } else {
        execute_direct(program, args, config).await
    }
}

/// Execute a command using chroot.
async fn execute_chroot(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let mut cmd = Command::new("chroot");
    cmd.arg(&config.root_dir);

    if let Some(ref user) = config.user {
        cmd.arg("su").arg("-").arg(user);
        cmd.arg("-c");
        cmd.arg(&format_command(program, args));
    } else {
        cmd.arg(program).args(args);
    }

    cmd.envs(&config.env);

    // In sandbox mode (user namespace), only UID 0 (root) is mapped.
    // Prevent apt from trying to sandbox itself by switching to the _apt user,
    // which would fail with seteuid/setgroups errors.
    if is_sandbox_active() {
        cmd.env("APT_CONFIG", "APT::Sandbox::User \"root\";");
    }

    if !config.env.contains_key("PATH") {
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }

    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute chroot command: {}", e)))?;

    Ok(output)
}

/// Execute a command using unshare + chroot (non-sandbox path).
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

    if let Some(ref user) = config.user {
        cmd.arg("su").arg("-").arg(user);
    }

    cmd.arg(program).args(args);

    cmd.envs(&config.env);

    // In sandbox mode (user namespace), only UID 0 (root) is mapped.
    // Prevent apt from trying to sandbox itself by switching to the _apt user,
    // which would fail with seteuid/setgroups errors.
    if is_sandbox_active() {
        cmd.env("APT_CONFIG", "APT::Sandbox::User \"root\";");
    }

    if !config.env.contains_key("PATH") {
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
    }

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

/// Execute a command directly (no chroot, no namespace isolation).
async fn execute_direct(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.envs(&config.env);

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
    if let Some((uid_str, gid_str)) = user_str.split_once(':') {
        let uid = uid_str.parse::<u32>()
            .map_err(|_| CommandError::Failed(format!("Invalid UID: {}", uid_str)))?;
        let gid = gid_str.parse::<u32>()
            .map_err(|_| CommandError::Failed(format!("Invalid GID: {}", gid_str)))?;
        return Ok((uid, gid, vec![]));
    }

    if let Ok(uid) = user_str.parse::<u32>() {
        return Ok((uid, uid, vec![]));
    }

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

            let mut groups = vec![gid];
            let mut ngroups = 0;

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

/// Add default HOME environment variable if not present.
pub fn add_default_home(user_str: &str, envs: &mut HashMap<String, String>) -> Result<()> {
    if envs.contains_key("HOME") {
        return Ok(());
    }

    if user_str.is_empty() || user_str == "root" || user_str == "0" {
        envs.insert("HOME".to_string(), "/root".to_string());
        return Ok(());
    }

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

    let home = if user_str.parse::<u32>().is_ok() {
        "/".to_string()
    } else {
        format!("/home/{}", user_str)
    };

    envs.insert("HOME".to_string(), home);
    Ok(())
}