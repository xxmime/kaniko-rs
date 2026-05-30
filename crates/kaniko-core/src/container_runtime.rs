//! Container runtime implementation for RUN command execution.
//!
//! This module provides a more complete container runtime implementation
//! that matches Go's SysProcAttr.Credential and chroot functionality.

use crate::command::{CommandError, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

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

/// Execute a command in a container-like environment
pub async fn execute_in_container(
    program: &str,
    args: &[String],
    config: &ContainerRuntimeConfig,
) -> Result<std::process::Output> {
    // Use unshare for better isolation if available
    let mut cmd = if cfg!(target_os = "linux") && config.root_dir != PathBuf::from("/") {
        // Use unshare for namespace isolation + chroot
        let mut unshare_cmd = Command::new("unshare");
        unshare_cmd
            .arg("--mount")
            .arg("--uts")
            .arg("--ipc")
            .arg("--net")
            .arg("--pid")
            .arg("--fork")
            .arg("--map-root-user");
        
        // Add chroot if root_dir is not "/"
        if config.root_dir != PathBuf::from("/") {
            unshare_cmd
                .arg("chroot")
                .arg(&config.root_dir);
        }
        
        // Add user switching if specified
        if let Some(ref user) = config.user {
            unshare_cmd.arg("su").arg("-").arg(user);
        }
        
        unshare_cmd.arg("--").arg(program).args(args);
        
        unshare_cmd
    } else {
        // Fallback to basic command execution
        let mut basic_cmd = Command::new(program);
        basic_cmd.args(args);
        basic_cmd
    };

    // Set environment variables
    cmd.envs(&config.env);

    // Set working directory
    if let Some(ref workdir) = config.working_dir {
        cmd.current_dir(workdir);
    }

    // Capture output
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let output = cmd
        .output()
        .await
        .map_err(|e| CommandError::Failed(format!("Failed to execute command: {}", e)))?;

    Ok(output)
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