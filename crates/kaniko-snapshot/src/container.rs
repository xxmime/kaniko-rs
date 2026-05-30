//! Container runtime detection for kaniko-rs.
//!
//! Analogous to Go: `pkg/util/proc/proc.go`.
//!
//! Detects whether the current process is running inside a container,
//! and which container runtime is being used. This is used by kaniko
//! to warn if running outside a container (the `--force` flag bypasses this).

use std::fs;
use std::path::Path;

/// Container runtime types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerRuntime {
    Docker,
    Rkt,
    Nspawn,
    Lxc,
    LxcLibvirt,
    OpenVz,
    Kubernetes,
    Garden,
    Podman,
    GVisor,
    Firejail,
    Wsl,
    NotFound,
}

impl ContainerRuntime {
    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Rkt => "rkt",
            ContainerRuntime::Nspawn => "systemd-nspawn",
            ContainerRuntime::Lxc => "lxc",
            ContainerRuntime::LxcLibvirt => "lxc-libvirt",
            ContainerRuntime::OpenVz => "openvz",
            ContainerRuntime::Kubernetes => "kube",
            ContainerRuntime::Garden => "garden",
            ContainerRuntime::Podman => "podman",
            ContainerRuntime::GVisor => "gvisor",
            ContainerRuntime::Firejail => "firejail",
            ContainerRuntime::Wsl => "wsl",
            ContainerRuntime::NotFound => "not-found",
        }
    }
}

/// Detect the container runtime the process is running in.
///
/// Checks multiple sources in order:
/// 1. /proc/self/cgroup
/// 2. /proc/vz (OpenVZ)
/// 3. /__runsc_containers__ (gVisor)
/// 4. /proc/1/cmdline (firejail)
/// 5. /proc/version_signature (WSL)
/// 6. `container` environment variable
/// 7. /run/systemd/container
/// 8. Container-specific files
///
/// Analogous to Go: `GetContainerRuntime()`.
pub fn get_container_runtime() -> ContainerRuntime {
    // 1. Check /proc/self/cgroup
    if let Ok(content) = fs::read_to_string("/proc/self/cgroup") {
        let runtime = detect_runtime_from_cgroup(&content);
        if runtime != ContainerRuntime::NotFound {
            return runtime;
        }
    }

    // 2. Check /proc/vz (OpenVZ)
    if Path::new("/proc/vz").exists() && !Path::new("/proc/bc").exists() {
        return ContainerRuntime::OpenVz;
    }

    // 3. Check gVisor
    if Path::new("/__runsc_containers__").exists() {
        return ContainerRuntime::GVisor;
    }

    // 4. Check /proc/1/cmdline for firejail
    if let Ok(content) = fs::read_to_string("/proc/1/cmdline") {
        let runtime = detect_runtime_from_string(&content);
        if runtime != ContainerRuntime::NotFound {
            return runtime;
        }
    }

    // 5. Check WSL
    if let Ok(content) = fs::read_to_string("/proc/version_signature") {
        if content.starts_with("Microsoft") {
            return ContainerRuntime::Wsl;
        }
    }

    // 6. Check container env variable
    if let Ok(val) = std::env::var("container") {
        let runtime = detect_runtime_from_string(&val);
        if runtime != ContainerRuntime::NotFound {
            return runtime;
        }
    }

    // 7. Check /run/systemd/container
    if let Ok(content) = fs::read_to_string("/run/systemd/container") {
        let runtime = detect_runtime_from_string(content.trim());
        if runtime != ContainerRuntime::NotFound {
            return runtime;
        }
    }

    // 8. Check container-specific files
    let runtime = detect_container_files();
    if runtime != ContainerRuntime::NotFound {
        return runtime;
    }

    // 9. Check overlay mount on /
    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            if line.starts_with('/') && line.split_whitespace().nth(1) == Some("/") {
                if let Some(fs_type) = line.split_whitespace().nth(2) {
                    if fs_type == "overlay" {
                        return ContainerRuntime::Kubernetes;
                    }
                }
            }
        }
    }

    ContainerRuntime::NotFound
}

/// Detect runtime from cgroup content.
fn detect_runtime_from_cgroup(content: &str) -> ContainerRuntime {
    for line in content.lines() {
        let lower = line.to_lowercase();
        if lower.contains("docker") {
            return ContainerRuntime::Docker;
        }
        if lower.contains("kubepods") || lower.contains("kubepod") {
            return ContainerRuntime::Kubernetes;
        }
        if lower.contains("garden") {
            return ContainerRuntime::Garden;
        }
        if lower.contains("lxc") {
            return ContainerRuntime::Lxc;
        }
        if lower.contains("rkt") {
            return ContainerRuntime::Rkt;
        }
        if lower.contains("pod") && lower.contains("pod") {
            return ContainerRuntime::Podman;
        }
    }
    ContainerRuntime::NotFound
}

/// Detect runtime from a string (env var, cmdline, etc.).
fn detect_runtime_from_string(content: &str) -> ContainerRuntime {
    let lower = content.to_lowercase();
    if lower.contains("docker") {
        return ContainerRuntime::Docker;
    }
    if lower.contains("lxc") && lower.contains("libvirt") {
        return ContainerRuntime::LxcLibvirt;
    }
    if lower.contains("lxc") {
        return ContainerRuntime::Lxc;
    }
    if lower.contains("rkt") {
        return ContainerRuntime::Rkt;
    }
    if lower.contains("kube") {
        return ContainerRuntime::Kubernetes;
    }
    if lower.contains("garden") {
        return ContainerRuntime::Garden;
    }
    if lower.contains("podman") {
        return ContainerRuntime::Podman;
    }
    if lower.contains("systemd-nspawn") {
        return ContainerRuntime::Nspawn;
    }
    if lower.contains("firejail") {
        return ContainerRuntime::Firejail;
    }
    ContainerRuntime::NotFound
}

/// Detect container-specific files.
fn detect_container_files() -> ContainerRuntime {
    let checks = [
        (ContainerRuntime::Podman, "/run/.containerenv"),
        (ContainerRuntime::Docker, "/.dockerenv"),
    ];

    for (runtime, path) in &checks {
        if Path::new(path).exists() {
            return runtime.clone();
        }
    }

    ContainerRuntime::NotFound
}

/// Check if the current process is running inside a container.
///
/// Returns true if any container runtime is detected.
/// Analogous to Go: `checkContained()` in root.go.
pub fn is_running_in_container() -> bool {
    get_container_runtime() != ContainerRuntime::NotFound
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_container_runtime_as_str() {
        assert_eq!(ContainerRuntime::Docker.as_str(), "docker");
        assert_eq!(ContainerRuntime::Kubernetes.as_str(), "kube");
        assert_eq!(ContainerRuntime::NotFound.as_str(), "not-found");
    }

    #[test]
    fn test_detect_runtime_from_cgroup_docker() {
        let content = "12:memory:/docker/abc123\n";
        assert_eq!(detect_runtime_from_cgroup(content), ContainerRuntime::Docker);
    }

    #[test]
    fn test_detect_runtime_from_cgroup_k8s() {
        let content = "12:memory:/kubepods/besteffort/pod123\n";
        assert_eq!(detect_runtime_from_cgroup(content), ContainerRuntime::Kubernetes);
    }

    #[test]
    fn test_detect_runtime_from_cgroup_not_found() {
        let content = "12:memory:/user.slice\n";
        assert_eq!(detect_runtime_from_cgroup(content), ContainerRuntime::NotFound);
    }

    #[test]
    fn test_detect_runtime_from_string() {
        assert_eq!(detect_runtime_from_string("docker"), ContainerRuntime::Docker);
        assert_eq!(detect_runtime_from_string("lxc-libvirt"), ContainerRuntime::LxcLibvirt);
        assert_eq!(detect_runtime_from_string("something"), ContainerRuntime::NotFound);
    }
}