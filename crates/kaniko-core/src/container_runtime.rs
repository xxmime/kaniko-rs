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

/// Prepare the rootfs for chroot execution.
///
/// This sets up the essential files and directories needed for commands
/// inside the chroot to work correctly:
///
/// 1. Copy /etc/resolv.conf and /etc/hosts from host for DNS resolution
/// 2. Create /etc/hostname if missing
/// 3. Write APT sandbox config to prevent seteuid failures
/// 4. Create basic /dev entries (null, zero, random, urandom, tty)
/// 5. Copy Docker credentials into rootfs for private registry access
///
/// Note: We do NOT attempt to mount proc/sys/dev here because:
/// - In a user namespace, mounting sysfs is forbidden by the kernel
/// - Mounting proc requires proper mount namespace ownership
/// - These mounts are attempted inside the per-command unshare instead
pub fn prepare_rootfs(root_dir: &PathBuf) {
    if !is_sandbox_active() {
        return;
    }

    // Copy DNS configuration from host
    copy_host_file("/etc/resolv.conf", &root_dir.join("etc/resolv.conf"));
    copy_host_file("/etc/hosts", &root_dir.join("etc/hosts"));

    // Create /etc/hostname if not present
    let hostname_path = root_dir.join("etc/hostname");
    if !hostname_path.exists() {
        let _ = std::fs::write(&hostname_path, "kaniko-builder\n");
    }

    // Create /etc/mtab symlink if not present (many tools expect this)
    let mtab_path = root_dir.join("etc/mtab");
    if !mtab_path.exists() {
        let _ = std::fs::remove_file(&mtab_path);
        let _ = std::os::unix::fs::symlink("/proc/mounts", &mtab_path);
    }

    // Create basic /dev entries
    create_minimal_dev(&root_dir.join("dev"));

    // Write APT config to prevent sandboxing failures
    let apt_conf_dir = root_dir.join("etc/apt/apt.conf.d");
    if let Ok(()) = std::fs::create_dir_all(&apt_conf_dir) {
        let apt_conf_path = apt_conf_dir.join("99kaniko-sandbox");
        if let Err(e) = std::fs::write(&apt_conf_path, "APT::Sandbox::User \"root\";\n") {
            tracing::debug!("sandbox: failed to write APT config: {}", e);
        }
    }

    // Copy Docker credentials into rootfs for private registry access
    // inside chroot. This allows processes like pip, apt, curl, etc.
    // that need to authenticate with private registries inside the chroot.
    copy_docker_credentials(root_dir);
}

/// Copy a file from the host system, preserving its content.
/// If the source doesn't exist, create a minimal default.
fn copy_host_file(src: &str, dest: &std::path::PathBuf) {
    // Create parent directory
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if std::path::Path::new(src).exists() {
        match std::fs::copy(src, dest) {
            Ok(_) => {
                tracing::debug!("sandbox: copied {} to rootfs", src);
            }
            Err(e) => {
                tracing::debug!("sandbox: failed to copy {}: {}", src, e);
            }
        }
    }
}

/// Copy Docker credentials from the host into the rootfs.
///
/// This allows processes running inside the chroot to authenticate with
/// private registries (e.g., `pip install` from a private PyPI, or
/// `apt-get` from a private repo that requires auth).
///
/// Copies from the following host locations (in priority order):
/// 1. /kaniko/.docker/ → rootfs/kaniko/.docker/
/// 2. $DOCKER_CONFIG/ → rootfs/<same-path>/
/// 3. $HOME/.docker/ → rootfs/root/.docker/
fn copy_docker_credentials(root_dir: &PathBuf) {
    // Collect candidate source directories for Docker credentials
    let mut sources: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();

    // 1. /kaniko/.docker (standard kaniko container location)
    let kaniko_docker = std::path::PathBuf::from("/kaniko/.docker");
    if kaniko_docker.join("config.json").exists() {
        let dest = root_dir.join("kaniko/.docker");
        sources.push((kaniko_docker, dest));
    }

    // 2. $DOCKER_CONFIG (explicit environment variable)
    if let Ok(config_dir) = std::env::var("DOCKER_CONFIG") {
        let src = std::path::PathBuf::from(&config_dir);
        if src.join("config.json").exists() {
            let dest = root_dir.join(&config_dir.trim_start_matches('/'));
            sources.push((src, dest));
        }
    }

    // 3. $HOME/.docker
    if let Ok(home) = std::env::var("HOME") {
        let src = std::path::PathBuf::from(&home).join(".docker");
        if src.join("config.json").exists() {
            let dest = root_dir.join("root/.docker");
            sources.push((src, dest));
        }
    }

    // 4. /root/.docker
    let root_docker = std::path::PathBuf::from("/root/.docker");
    if root_docker.join("config.json").exists() {
        let dest = root_dir.join("root/.docker");
        sources.push((root_docker, dest));
    }

    for (src_dir, dest_dir) in sources {
        if dest_dir.join("config.json").exists() {
            // Already copied (e.g., from a previous source or base image)
            continue;
        }

        if let Err(e) = std::fs::create_dir_all(&dest_dir) {
            tracing::debug!("sandbox: failed to create docker config dir {}: {}", dest_dir.display(), e);
            continue;
        }

        // Copy config.json
        let src_config = src_dir.join("config.json");
        let dest_config = dest_dir.join("config.json");
        if src_config.exists() {
            match std::fs::copy(&src_config, &dest_config) {
                Ok(_) => tracing::debug!("sandbox: copied Docker credentials from {} to rootfs", src_dir.display()),
                Err(e) => tracing::debug!("sandbox: failed to copy Docker credentials: {}", e),
            }
        }

        // Copy credential helper binaries if they exist in the .docker directory
        // (some setups store helpers alongside config.json)
        for entry in std::fs::read_dir(&src_dir).unwrap_or_else(|_| std::fs::read_dir("/dev/null").unwrap()) {
            if let Ok(entry) = entry {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("docker-credential-") {
                    let src_file = entry.path();
                    let dest_file = dest_dir.join(&*name);
                    if !dest_file.exists() {
                        let _ = std::fs::copy(&src_file, &dest_file);
                        // Make executable
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(&dest_file, std::fs::Permissions::from_mode(0o755));
                        }
                    }
                }
            }
        }
    }
}

/// Create a minimal /dev with essential device nodes.
/// In a user namespace we can't create real device nodes (mknod requires CAP_MKNOD),
/// so we create symlinks or empty files as placeholders.
fn create_minimal_dev(dev_dir: &std::path::PathBuf) {
    let _ = std::fs::create_dir_all(dev_dir);

    // Try to create device nodes; if mknod fails (user namespace), create empty files
    let devices = [
        ("null", "1", "3"),
        ("zero", "1", "5"),
        ("random", "1", "8"),
        ("urandom", "1", "9"),
        ("tty", "5", "0"),
    ];

    for (name, major, minor) in &devices {
        let path = dev_dir.join(name);
        if path.exists() {
            continue;
        }

        // Try mknod first (works when we have CAP_MKNOD)
        let mknod_output = std::process::Command::new("mknod")
            .arg(&path)
            .arg("c")
            .arg(major)
            .arg(minor)
            .output();

        match mknod_output {
            Ok(o) if o.status.success() => continue,
            _ => {
                // Fallback: create empty file as placeholder
                let _ = std::fs::File::create(&path);
            }
        }
    }

    // Create /dev/pts and /dev/shm directories
    let _ = std::fs::create_dir_all(dev_dir.join("pts"));
    let _ = std::fs::create_dir_all(dev_dir.join("shm"));

    // /dev/null must be writable; try chmod
    let _ = std::process::Command::new("chmod")
        .arg("666")
        .arg(dev_dir.join("null"))
        .output();
}

/// Clean up the rootfs after chroot execution.
///
/// Removes the APT sandbox config and any temporary files we created.
/// Note: We don't need to unmount proc/sys/dev since we use per-command
/// unshare namespaces (mounts are automatically cleaned up when the
/// namespace is destroyed).
pub fn cleanup_rootfs(root_dir: &PathBuf) {
    if !is_sandbox_active() {
        return;
    }

    // Remove the APT sandbox config we created
    let apt_conf_path = root_dir.join("etc/apt/apt.conf.d/99kaniko-sandbox");
    if apt_conf_path.exists() {
        let _ = std::fs::remove_file(&apt_conf_path);
    }

    // Remove Docker credentials we copied into rootfs
    // These should not persist in the final image
    let docker_paths = [
        root_dir.join("kaniko/.docker/config.json"),
        root_dir.join("root/.docker/config.json"),
    ];
    for path in &docker_paths {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    // Unmount any filesystems that might have been mounted inside
    // per-command namespaces (these should already be gone, but clean up
    // just in case)
    for (_, _, name) in ROOTFS_MOUNTS.iter().rev() {
        let dest = root_dir.join(name);
        let check = std::process::Command::new("mountpoint")
            .arg("-q")
            .arg(&dest)
            .status();

        if let Ok(status) = check {
            if status.success() {
                let _ = std::process::Command::new("umount")
                    .arg(&dest)
                    .output();
            }
        }
    }
}

/// Execute a command in a container-like environment.
///
/// When sandbox is active and root_dir is set, we use `unshare --mount`
/// to create a per-command mount namespace where we can:
/// 1. Set mount propagation to slave
/// 2. Mount /proc, /sys, /dev into the rootfs
/// 3. chroot into the rootfs and execute the command
///
/// When the child process exits, the mount namespace is destroyed and
/// all mounts are automatically cleaned up.
///
/// If sandbox is not active, falls back to unshare+chroot or direct execution.
pub async fn execute_in_container(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    let use_chroot = cfg!(target_os = "linux") && config.root_dir != PathBuf::from("/");

    if use_chroot && is_sandbox_active() {
        // Sandbox path: prepare rootfs files, then execute in a per-command
        // mount namespace with chroot
        prepare_rootfs(&config.root_dir);
        let result = execute_sandbox_chroot(program, args, config).await;
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

/// Execute a command inside a per-command mount namespace with chroot.
///
/// This uses `unshare --mount` to create an isolated mount namespace for
/// each RUN command. Inside that namespace, we:
/// 1. Set mount propagation to slave (matching Go chrootarchive)
/// 2. Mount /proc, /sys, /dev into the rootfs
/// 3. chroot into the rootfs
/// 4. Execute the command
///
/// The mount namespace is destroyed when the process exits, automatically
/// cleaning up all mounts.
async fn execute_sandbox_chroot(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    // Build a shell script that:
    // 1. Makes mount tree slave
    // 2. Mounts proc/sys/dev into rootfs
    // 3. chroots into rootfs and runs the command
    let root_dir = &config.root_dir;
    let root_dir_str = root_dir.to_string_lossy();

    // Build the command string for inside chroot
    let inner_cmd = if let Some(ref user) = config.user {
        format!("su - {} -c {}", shell_quote(user), shell_quote(&format_command(program, args)))
    } else {
        format!("{} {}", shell_quote(program), args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "))
    };

    // Build env exports
    let mut env_exports = String::new();
    for (k, v) in &config.env {
        env_exports.push_str(&format!("export {}={}\n", shell_quote(k), shell_quote(v)));
    }
    if !config.env.contains_key("PATH") {
        env_exports.push_str("export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n");
    }
    if !config.env.contains_key("DEBIAN_FRONTEND") {
        env_exports.push_str("export DEBIAN_FRONTEND=noninteractive\n");
    }
    // Ensure HOME is set for credential lookup inside chroot
    if !config.env.contains_key("HOME") {
        env_exports.push_str("export HOME=/root\n");
    }
    // Ensure DOCKER_CONFIG is set inside chroot so that processes
    // can find Docker credentials for private registry access
    if !config.env.contains_key("DOCKER_CONFIG") {
        // Check which credential path exists in the rootfs
        let kaniko_config = root_dir.join("kaniko/.docker");
        let root_config = root_dir.join("root/.docker");
        if kaniko_config.join("config.json").exists() {
            env_exports.push_str(&format!("export DOCKER_CONFIG={}\n", shell_quote("/kaniko/.docker")));
        } else if root_config.join("config.json").exists() {
            env_exports.push_str(&format!("export DOCKER_CONFIG={}\n", shell_quote("/root/.docker")));
        } else if let Ok(docker_config) = std::env::var("DOCKER_CONFIG") {
            // Fall back to host DOCKER_CONFIG (might not exist in chroot, but try)
            let chroot_path = format!("/{}", docker_config.trim_start_matches('/'));
            env_exports.push_str(&format!("export DOCKER_CONFIG={}\n", shell_quote(&chroot_path)));
        }
    }

    let script = format!(
r#"#!/bin/sh
set -e

# Make mount tree slave — matching Go chrootarchive MakeRSlave("/")
mount --make-rslave / 2>/dev/null || true

# Mount kernel filesystems into rootfs (best-effort)
mkdir -p {root}/proc {root}/sys {root}/dev 2>/dev/null || true
mount -t proc proc {root}/proc 2>/dev/null || echo "sandbox: /proc mount failed (non-fatal)" >&2
mount -t sysfs sysfs {root}/sys 2>/dev/null || echo "sandbox: /sys mount failed (non-fatal)" >&2
mount --bind /dev {root}/dev 2>/dev/null || echo "sandbox: /dev bind mount failed (non-fatal)" >&2

# Set up environment
{env_exports}

# Execute inside chroot
chroot {root} /bin/sh -c {cmd}
"#,
        root = root_dir_str,
        env_exports = env_exports,
        cmd = shell_quote(&inner_cmd),
    );

    let mut cmd = Command::new("unshare");
    cmd.arg("--mount")
        .arg("--propagation")
        .arg("slave")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg(&script);

    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute sandbox chroot: {}", e)))?;

    Ok(output)
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

/// Shell-quote a string for safe use in shell commands.
/// Uses single quotes with proper escaping.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    // If the string only contains safe characters, no quoting needed
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/') {
        return s.to_string();
    }
    // Use single quotes, escaping any embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
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