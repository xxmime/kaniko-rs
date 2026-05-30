//! Constants for kaniko-rs.
//!
//! Analogous to Go: `pkg/constants/constants.go`.
//!
//! Provides shared constants for root directories, snapshot modes,
//! build context prefixes, and Docker-related defaults.

/// Root directory path.
pub const ROOT_DIR: &str = "/";

/// Path to mount info file for filesystem ignore list detection.
pub const MOUNT_INFO_PATH: &str = "/proc/self/mountinfo";

/// Default kaniko working directory.
pub const DEFAULT_KANIKO_PATH: &str = "/kaniko";

/// Default sandbox path.
pub const DEFAULT_SANDBOX_PATH: &str = "/kaniko/sandbox";

/// Image author field.
pub const AUTHOR: &str = "kaniko";

/// Default name of the tar uploaded to GCS buckets.
pub const CONTEXT_TAR: &str = "context.tar.gz";

/// Snapshot mode: use full content hashing.
pub const SNAPSHOT_MODE_FULL: &str = "full";

/// Snapshot mode: use mtime-only hashing (fast but may miss changes).
pub const SNAPSHOT_MODE_TIME: &str = "time";

/// Snapshot mode: always re-snapshot.
pub const SNAPSHOT_MODE_REDO: &str = "redo";

/// No base image (scratch).
pub const NO_BASE_IMAGE: &str = "scratch";

// Build context prefixes
/// GCS build context prefix.
pub const GCS_BUILD_CONTEXT_PREFIX: &str = "gs://";

/// S3 build context prefix.
pub const S3_BUILD_CONTEXT_PREFIX: &str = "s3://";

/// Local directory build context prefix.
pub const LOCAL_DIR_BUILD_CONTEXT_PREFIX: &str = "dir://";

/// Git build context prefix.
pub const GIT_BUILD_CONTEXT_PREFIX: &str = "git://";

/// HTTPS build context prefix.
pub const HTTPS_BUILD_CONTEXT_PREFIX: &str = "https://";

/// HOME environment variable name.
pub const HOME: &str = "HOME";

/// Default HOME value (Docker default).
pub const DEFAULT_HOME_VALUE: &str = "/root";

/// Root user name.
pub const ROOT_USER: &str = "root";

/// Docker CMD command name.
pub const CMD: &str = "CMD";

/// Docker ENTRYPOINT command name.
pub const ENTRYPOINT: &str = "ENTRYPOINT";

/// Name of the .dockerignore file.
pub const DOCKERIGNORE: &str = ".dockerignore";

/// S3 custom endpoint environment variable.
pub const AWS_ENDPOINT_URL_S3: &str = "AWS_ENDPOINT_URL_S3";

/// Default registry (Docker Hub).
pub const DEFAULT_REGISTRY: &str = "index.docker.io";

/// Whiteout prefix for OCI layer entries.
pub const WHITEOUT_PREFIX: &str = ".wh.";

/// Opaque whiteout prefix for OCI layer entries.
pub const OPAQUE_WHITEOUT_PREFIX: &str = ".wh..wh..opq";

/// Default snapshot timeout duration in minutes.
pub const DEFAULT_SNAPSHOT_TIMEOUT_MINUTES: u64 = 90;

/// Environment variable for snapshot timeout.
pub const SNAPSHOT_TIMEOUT_ENV: &str = "SNAPSHOT_TIMEOUT";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants_not_empty() {
        assert!(!ROOT_DIR.is_empty());
        assert!(!DEFAULT_KANIKO_PATH.is_empty());
        assert!(!NO_BASE_IMAGE.is_empty());
        assert!(!DEFAULT_REGISTRY.is_empty());
    }

    #[test]
    fn test_snapshot_modes() {
        assert_eq!(SNAPSHOT_MODE_FULL, "full");
        assert_eq!(SNAPSHOT_MODE_TIME, "time");
        assert_eq!(SNAPSHOT_MODE_REDO, "redo");
    }

    #[test]
    fn test_context_prefixes() {
        assert!(GCS_BUILD_CONTEXT_PREFIX.starts_with("gs://"));
        assert!(S3_BUILD_CONTEXT_PREFIX.starts_with("s3://"));
        assert!(LOCAL_DIR_BUILD_CONTEXT_PREFIX.starts_with("dir://"));
        assert!(GIT_BUILD_CONTEXT_PREFIX.starts_with("git://"));
        assert!(HTTPS_BUILD_CONTEXT_PREFIX.starts_with("https://"));
    }
}