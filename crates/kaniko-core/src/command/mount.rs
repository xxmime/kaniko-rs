//! RUN --mount BuildKit extension support.
//!
//! Parses and applies mount specifications for RUN commands.
//! Supported mount types:
//! - `type=bind`: Bind-mount a directory from the build context or stage
//! - `type=cache`: Persistent cache mount across builds
//! - `type=tmpfs`: Temporary filesystem mount
//! - `type=secret`: Secret mount (limited support)
//!
//! See: https://docs.docker.com/engine/reference/builder/#run---mount

use crate::command::{CommandError, Result};
use std::collections::HashMap;
use std::path::Path;

/// Parsed mount specification.
#[derive(Debug, Clone)]
pub struct MountSpec {
    /// The mount type (bind, cache, tmpfs, secret).
    pub mount_type: MountType,
    /// Target path inside the container.
    pub target: String,
    /// Source path (for bind mounts).
    pub source: Option<String>,
    /// Whether the mount is read-only.
    pub read_only: bool,
    /// Cache sharing mode (for cache mounts): shared, private, locked.
    pub cache_sharing: Option<String>,
    /// Cache ID for cache mounts.
    pub cache_id: Option<String>,
    /// UID for tmpfs mounts.
    pub uid: Option<u32>,
    /// GID for tmpfs mounts.
    pub gid: Option<u32>,
    /// Mode for tmpfs mounts.
    pub mode: Option<u32>,
    /// Size limit for tmpfs mounts (in bytes).
    pub size: Option<u64>,
}

/// Type of mount.
#[derive(Debug, Clone, PartialEq)]
pub enum MountType {
    /// Bind mount from build context or stage.
    Bind,
    /// Persistent cache mount.
    Cache,
    /// Temporary filesystem.
    Tmpfs,
    /// Secret mount.
    Secret,
}

impl std::fmt::Display for MountType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountType::Bind => write!(f, "bind"),
            MountType::Cache => write!(f, "cache"),
            MountType::Tmpfs => write!(f, "tmpfs"),
            MountType::Secret => write!(f, "secret"),
        }
    }
}

/// Parse a mount specification string.
///
/// Format: `type=TYPE,key=value,...`
/// Example: `type=bind,source=/app,target=/app,readonly`
/// Example: `type=cache,target=/root/.cache,id=mycache`
/// Example: `type=tmpfs,target=/tmp,size=1g,uid=1000`
pub fn parse_mount(spec: &str) -> Result<MountSpec> {
    let mut params: HashMap<String, String> = HashMap::new();

    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((key, value)) = part.split_once('=') {
            params.insert(key.to_lowercase(), value.to_string());
        } else {
            // Handle boolean flags like "readonly"
            params.insert(part.to_lowercase(), "true".to_string());
        }
    }

    let mount_type_str = params.get("type")
        .ok_or_else(|| CommandError::Failed("mount spec requires 'type' parameter".to_string()))?;

    let mount_type = match mount_type_str.as_str() {
        "bind" => MountType::Bind,
        "cache" => MountType::Cache,
        "tmpfs" => MountType::Tmpfs,
        "secret" => MountType::Secret,
        other => return Err(CommandError::Failed(format!("unknown mount type: {}", other))),
    };

    let target = params.get("target")
        .or(params.get("dst"))
        .cloned()
        .ok_or_else(|| CommandError::Failed("mount spec requires 'target' parameter".to_string()))?;

    let source = params.get("source")
        .or(params.get("src"))
        .cloned();

    let read_only = params.contains_key("readonly") || params.contains_key("ro");

    let cache_sharing = params.get("sharing").cloned();
    let cache_id = params.get("id").cloned();

    let uid = params.get("uid").and_then(|v| v.parse().ok());
    let gid = params.get("gid").and_then(|v| v.parse().ok());
    let mode = params.get("mode").and_then(|v| u32::from_str_radix(v, 8).ok());
    let size = params.get("size").and_then(|v| parse_size(v));

    Ok(MountSpec {
        mount_type,
        target,
        source,
        read_only,
        cache_sharing,
        cache_id,
        uid,
        gid,
        mode,
        size,
    })
}

/// Parse a size string (e.g., "1g", "512m", "1024k") into bytes.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    if s.ends_with('g') {
        s.trim_end_matches('g').parse::<u64>().ok().map(|v| v * 1024 * 1024 * 1024)
    } else if s.ends_with('m') {
        s.trim_end_matches('m').parse::<u64>().ok().map(|v| v * 1024 * 1024)
    } else if s.ends_with('k') {
        s.trim_end_matches('k').parse::<u64>().ok().map(|v| v * 1024)
    } else {
        s.parse().ok()
    }
}

/// Network mode for RUN --network.
#[derive(Debug, Clone, PartialEq)]
pub enum NetworkMode {
    /// Default network access.
    Default,
    /// No network access.
    None,
    /// Use the host's network namespace.
    Host,
}

impl std::fmt::Display for NetworkMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkMode::Default => write!(f, "default"),
            NetworkMode::None => write!(f, "none"),
            NetworkMode::Host => write!(f, "host"),
        }
    }
}

/// Parse network mode from string.
pub fn parse_network(s: &str) -> Result<NetworkMode> {
    match s.to_lowercase().as_str() {
        "default" => Ok(NetworkMode::Default),
        "none" => Ok(NetworkMode::None),
        "host" => Ok(NetworkMode::Host),
        other => Err(CommandError::Failed(format!("unknown network mode: {}", other))),
    }
}

/// Apply a mount specification by setting up the mount point.
/// This creates the target directory and, for tmpfs/cache mounts,
/// sets up the appropriate filesystem.
pub fn apply_mount(mount: &MountSpec) -> Result<()> {
    let target = Path::new(&mount.target);

    match mount.mount_type {
        MountType::Bind => {
            // For bind mounts, we just ensure the target directory exists.
            // The actual binding happens at the filesystem level during the build.
            if !target.exists() {
                std::fs::create_dir_all(target)?;
                tracing::debug!("Created bind mount target: {}", mount.target);
            }
            if let Some(ref src) = mount.source {
                tracing::info!("Bind mount: {} -> {} (readonly={})", src, mount.target, mount.read_only);
            }
        }
        MountType::Cache => {
            // Cache mounts: create a persistent directory.
            // In kaniko (non-daemon), cache mounts are ephemeral per build,
            // but we can use a well-known cache directory.
            let cache_dir = mount.cache_id.as_ref()
                .map(|id| format!("/kaniko/cache/{}", id))
                .unwrap_or_else(|| format!("/kaniko/cache/{}", mount.target.trim_start_matches('/').replace('/', "_")));

            if !Path::new(&cache_dir).exists() {
                std::fs::create_dir_all(&cache_dir)?;
            }

            // Create symlink from target to cache dir
            if target.exists() {
                // Target already exists; don't overwrite
                tracing::debug!("Cache mount target already exists: {}", mount.target);
            } else {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&cache_dir, target)?;
                }
                #[cfg(not(unix))]
                {
                    // Fallback: just create the directory
                    std::fs::create_dir_all(target)?;
                }
            }
            tracing::info!("Cache mount: {} -> {} (id={:?})", cache_dir, mount.target, mount.cache_id);
        }
        MountType::Tmpfs => {
            // For tmpfs mounts, create a temporary directory.
            // In a container build, we can't actually mount tmpfs,
            // so we create a regular temp directory as a best-effort.
            if !target.exists() {
                std::fs::create_dir_all(target)?;
            }

            // Apply mode if specified
            if let Some(mode) = mount.mode {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))?;
                }
            }

            tracing::info!("Tmpfs mount: {} (size={:?}, mode={:?})", mount.target, mount.size, mount.mode);
        }
        MountType::Secret => {
            // Secret mounts: create target directory but don't populate.
            // Secrets are typically provided via --secret flag, which kaniko
            // doesn't fully support yet.
            if !target.exists() {
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(target, "")?;
            }
            tracing::info!("Secret mount: {} (limited support)", mount.target);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bind_mount() {
        let spec = parse_mount("type=bind,source=/app,target=/app,readonly").unwrap();
        assert_eq!(spec.mount_type, MountType::Bind);
        assert_eq!(spec.source, Some("/app".to_string()));
        assert_eq!(spec.target, "/app");
        assert!(spec.read_only);
    }

    #[test]
    fn test_parse_cache_mount() {
        let spec = parse_mount("type=cache,target=/root/.cache,id=npm-cache,sharing=shared").unwrap();
        assert_eq!(spec.mount_type, MountType::Cache);
        assert_eq!(spec.target, "/root/.cache");
        assert_eq!(spec.cache_id, Some("npm-cache".to_string()));
        assert_eq!(spec.cache_sharing, Some("shared".to_string()));
        assert!(!spec.read_only);
    }

    #[test]
    fn test_parse_tmpfs_mount() {
        let spec = parse_mount("type=tmpfs,target=/tmp,size=1g,uid=1000,mode=1777").unwrap();
        assert_eq!(spec.mount_type, MountType::Tmpfs);
        assert_eq!(spec.target, "/tmp");
        assert_eq!(spec.size, Some(1024 * 1024 * 1024));
        assert_eq!(spec.uid, Some(1000));
        assert_eq!(spec.mode, Some(0o1777));
    }

    #[test]
    fn test_parse_bind_with_src_alias() {
        let spec = parse_mount("type=bind,src=/host/path,target=/container/path").unwrap();
        assert_eq!(spec.mount_type, MountType::Bind);
        assert_eq!(spec.source, Some("/host/path".to_string()));
        assert_eq!(spec.target, "/container/path");
    }

    #[test]
    fn test_parse_mount_missing_type() {
        let result = parse_mount("target=/app");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mount_missing_target() {
        let result = parse_mount("type=bind");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_mount_unknown_type() {
        let result = parse_mount("type=unknown,target=/app");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_network() {
        assert_eq!(parse_network("default").unwrap(), NetworkMode::Default);
        assert_eq!(parse_network("none").unwrap(), NetworkMode::None);
        assert_eq!(parse_network("host").unwrap(), NetworkMode::Host);
    }

    #[test]
    fn test_parse_network_case_insensitive() {
        assert_eq!(parse_network("None").unwrap(), NetworkMode::None);
        assert_eq!(parse_network("HOST").unwrap(), NetworkMode::Host);
    }

    #[test]
    fn test_parse_network_unknown() {
        assert!(parse_network("bridge").is_err());
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("512m"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1024k"), Some(1024 * 1024));
        assert_eq!(parse_size("2048"), Some(2048));
    }

    #[test]
    fn test_mount_type_display() {
        assert_eq!(MountType::Bind.to_string(), "bind");
        assert_eq!(MountType::Cache.to_string(), "cache");
        assert_eq!(MountType::Tmpfs.to_string(), "tmpfs");
        assert_eq!(MountType::Secret.to_string(), "secret");
    }

    #[test]
    fn test_network_mode_display() {
        assert_eq!(NetworkMode::Default.to_string(), "default");
        assert_eq!(NetworkMode::None.to_string(), "none");
        assert_eq!(NetworkMode::Host.to_string(), "host");
    }
}