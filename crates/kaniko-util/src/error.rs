//! Unified error type for kaniko-rs.
//!
//! Provides a single `KanikoError` enum that can represent errors from
//! any sub-crate, enabling clean error propagation across module boundaries.

use thiserror::Error;

/// The unified error type for the entire kaniko-rs project.
///
/// This enum wraps all sub-crate error types into a single type,
/// enabling error propagation across module boundaries while
/// preserving the original error context.
#[derive(Debug, Error)]
pub enum KanikoError {
    /// Dockerfile parsing error.
    #[error("dockerfile parse error: {0}")]
    Parse(String),

    /// Build execution error.
    #[error("build error: {0}")]
    Build(String),

    /// OCI image error.
    #[error("oci image error: {0}")]
    OciImage(String),

    /// OCI registry error.
    #[error("oci registry error: {0}")]
    OciRegistry(String),

    /// Snapshot error.
    #[error("snapshot error: {0}")]
    Snapshot(String),

    /// Cache error.
    #[error("cache error: {0}")]
    Cache(String),

    /// Credential error.
    #[error("credential error: {0}")]
    Credential(String),

    /// IO error.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Command execution error.
    #[error("command failed: {command} (exit code: {code:?})")]
    CommandFailed {
        /// The command that failed.
        command: String,
        /// The exit code, if available.
        code: Option<i32>,
    },

    /// Invalid argument or configuration.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Image not found.
    #[error("image not found: {0}")]
    ImageNotFound(String),

    /// Layer error.
    #[error("layer error: {0}")]
    Layer(String),

    /// Digest/hash error.
    #[error("digest error: {0}")]
    Digest(String),

    /// Generic error with context.
    #[error("{0}")]
    Other(String),
}

impl KanikoError {
    /// Create a parse error.
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    /// Create a build error.
    pub fn build(msg: impl Into<String>) -> Self {
        Self::Build(msg.into())
    }

    /// Create an OCI image error.
    pub fn oci_image(msg: impl Into<String>) -> Self {
        Self::OciImage(msg.into())
    }

    /// Create a command failed error.
    pub fn command_failed(command: impl Into<String>, code: Option<i32>) -> Self {
        Self::CommandFailed {
            command: command.into(),
            code,
        }
    }

    /// Create an invalid argument error.
    pub fn invalid_argument(msg: impl Into<String>) -> Self {
        Self::InvalidArgument(msg.into())
    }

    /// Check if this error is a command failure.
    pub fn is_command_failure(&self) -> bool {
        matches!(self, Self::CommandFailed { .. })
    }

    /// Check if this error is a cache miss.
    pub fn is_cache_miss(&self) -> bool {
        matches!(self, Self::Cache(msg) if msg.contains("cache miss"))
    }
}

/// Result type alias using KanikoError.
pub type Result<T> = std::result::Result<T, KanikoError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_construction() {
        let err = KanikoError::parse("unexpected token");
        assert!(err.to_string().contains("dockerfile parse error"));
        assert!(err.to_string().contains("unexpected token"));
    }

    #[test]
    fn test_command_failed() {
        let err = KanikoError::command_failed("/bin/sh -c make", Some(1));
        assert!(err.is_command_failure());
        assert!(err.to_string().contains("/bin/sh -c make"));
        assert!(err.to_string().contains("1"));
    }

    #[test]
    fn test_cache_miss_check() {
        let err = KanikoError::Cache("cache miss for key: abc123".to_string());
        assert!(err.is_cache_miss());

        let err2 = KanikoError::Cache("connection refused".to_string());
        assert!(!err2.is_cache_miss());
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: KanikoError = io_err.into();
        assert!(err.to_string().contains("IO error"));
    }

    #[test]
    fn test_serde_error_conversion() {
        let serde_err = serde_json::from_str::<i32>("not a number").unwrap_err();
        let err: KanikoError = serde_err.into();
        assert!(err.to_string().contains("serialization error"));
    }
}