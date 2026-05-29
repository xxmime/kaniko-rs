//! Build context module for kaniko-rs.
//!
//! Provides support for various build context sources:
//! - `dir://` — local directory
//! - `tar://` — tar archive
//! - `git://` — Git repository
//! - `https://` — HTTPS URL
//!
//! Analogous to Go: `pkg/buildcontext/buildcontext.go`.

use std::path::{Path, PathBuf};

/// Build context options for Git sources.
#[derive(Debug, Clone)]
pub struct GitBuildOptions {
    /// Git branch to checkout.
    pub branch: Option<String>,
    /// Whether to clone a single branch only.
    pub single_branch: bool,
    /// Whether to recurse into submodules.
    pub recurse_submodules: bool,
    /// Whether to skip TLS verification.
    pub insecure_skip_tls: bool,
}

impl Default for GitBuildOptions {
    fn default() -> Self {
        Self {
            branch: None,
            single_branch: false,
            recurse_submodules: false,
            insecure_skip_tls: false,
        }
    }
}

/// Build context trait — unifies calls to download and unpack the build context.
///
/// Analogous to Go: `buildcontext.BuildContext` interface.
pub trait BuildContext {
    /// Unpack the build context and return the directory where it resides.
    fn unpack(&self) -> Result<PathBuf, BuildContextError>;
}

/// Errors during build context operations.
#[derive(Debug, thiserror::Error)]
pub enum BuildContextError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported build context prefix: {0}")]
    UnsupportedPrefix(String),
    #[error("invalid context path: {0}")]
    InvalidPath(String),
    #[error("tar error: {0}")]
    Tar(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("download error: {0}")]
    Download(String),
}

/// Resolve a build context source string to a local directory path.
///
/// Supports the following prefixes:
/// - `dir://path` — local directory
/// - `tar://path` — tar archive
/// - `git://url` — Git repository (requires git binary)
/// - `https://url` — HTTPS URL (downloaded as tar)
/// - No prefix — treated as local directory
///
/// Analogous to Go: `buildcontext.GetBuildContext(srcContext, opts)`.
pub fn resolve_build_context(
    src_context: &str,
    git_opts: &GitBuildOptions,
) -> Result<Box<dyn BuildContext>, BuildContextError> {
    if let Some((prefix, context)) = src_context.split_once("://") {
        match prefix {
            "dir" => Ok(Box::new(DirBuildContext {
                context: context.to_string(),
            })),
            "tar" => Ok(Box::new(TarBuildContext {
                context: context.to_string(),
            })),
            "git" => Ok(Box::new(GitBuildContext {
                context: context.to_string(),
                opts: git_opts.clone(),
            })),
            "https" | "http" => Ok(Box::new(HttpsBuildContext {
                context: src_context.to_string(),
            })),
            _ => Err(BuildContextError::UnsupportedPrefix(format!(
                "unknown build context prefix: {}, please use one of: dir://, tar://, git://, https://",
                prefix
            ))),
        }
    } else {
        // No prefix — treat as local directory
        Ok(Box::new(DirBuildContext {
            context: src_context.to_string(),
        }))
    }
}

/// Local directory build context.
///
/// Analogous to Go: `buildcontext.Dir`.
pub struct DirBuildContext {
    context: String,
}

impl BuildContext for DirBuildContext {
    fn unpack(&self) -> Result<PathBuf, BuildContextError> {
        let path = Path::new(&self.context);
        if !path.exists() {
            return Err(BuildContextError::InvalidPath(format!(
                "directory does not exist: {}",
                self.context
            )));
        }
        if !path.is_dir() {
            return Err(BuildContextError::InvalidPath(format!(
                "not a directory: {}",
                self.context
            )));
        }
        Ok(path.to_path_buf())
    }
}

/// Tar archive build context.
///
/// Analogous to Go: `buildcontext.Tar`.
pub struct TarBuildContext {
    context: String,
}

impl BuildContext for TarBuildContext {
    fn unpack(&self) -> Result<PathBuf, BuildContextError> {
        let tar_path = Path::new(&self.context);
        if !tar_path.exists() {
            return Err(BuildContextError::InvalidPath(format!(
                "tar file does not exist: {}",
                self.context
            )));
        }

        // Create a temporary directory for extraction
        let tmp_dir = std::env::temp_dir().join(format!("kaniko-context-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir)?;

        // Extract the tar archive
        let file = std::fs::File::open(tar_path)?;
        let mut archive = tar::Archive::new(file);
        archive.unpack(&tmp_dir)
            .map_err(|e| BuildContextError::Tar(format!("failed to extract tar: {}", e)))?;

        Ok(tmp_dir)
    }
}

/// Git repository build context.
///
/// Analogous to Go: `buildcontext.Git`.
/// Uses the `git` command-line tool for cloning.
pub struct GitBuildContext {
    context: String,
    opts: GitBuildOptions,
}

impl BuildContext for GitBuildContext {
    fn unpack(&self) -> Result<PathBuf, BuildContextError> {
        // Parse context: git://repo.url#branch
        let (repo_url, branch) = if let Some((url, br)) = self.context.split_once('#') {
            (url.to_string(), Some(br.to_string()))
        } else {
            (self.context.clone(), self.opts.branch.clone())
        };

        // Create a temporary directory for cloning
        let tmp_dir = std::env::temp_dir().join(format!("kaniko-git-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir)?;

        // Use git command to clone the repository
        let mut cmd = std::process::Command::new("git");
        cmd.arg("clone");

        if self.opts.single_branch {
            cmd.arg("--single-branch");
        }

        if self.opts.recurse_submodules {
            cmd.arg("--recurse-submodules");
        }

        if let Some(ref br) = branch {
            cmd.arg("--branch").arg(br);
        }

        if self.opts.insecure_skip_tls {
            // Disable SSL verification
            cmd.env("GIT_SSL_NO_VERIFY", "1");
        }

        cmd.arg(&repo_url).arg(&tmp_dir);

        let output = cmd.output()
            .map_err(|e| BuildContextError::Git(format!("failed to execute git: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BuildContextError::Git(format!(
                "git clone failed: {}",
                stderr
            )));
        }

        Ok(tmp_dir)
    }
}

/// HTTPS URL build context.
///
/// Downloads a tar archive from a URL and extracts it.
/// Analogous to Go: `buildcontext.HTTPSTar`.
///
/// Note: Uses tokio runtime for async HTTP download since reqwest's
/// blocking feature is not enabled in this project.
pub struct HttpsBuildContext {
    context: String,
}

impl BuildContext for HttpsBuildContext {
    fn unpack(&self) -> Result<PathBuf, BuildContextError> {
        // Create a temporary directory for extraction
        let tmp_dir = std::env::temp_dir().join(format!("kaniko-https-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir)?;

        // Use tokio runtime to download the tar file asynchronously
        let url = self.context.clone();
        let data = tokio::runtime::Handle::current().block_on(async {
            let response = reqwest::get(&url).await
                .map_err(|e| BuildContextError::Download(format!("failed to download: {}", e)))?;

            if !response.status().is_success() {
                return Err(BuildContextError::Download(format!(
                    "download failed with status: {}",
                    response.status()
                )));
            }

            let bytes = response.bytes().await
                .map_err(|e| BuildContextError::Download(format!("failed to read response: {}", e)))?;
            Ok::<_, BuildContextError>(bytes.to_vec())
        })?;

        // Extract the tar archive
        let mut archive = tar::Archive::new(data.as_slice());
        archive.unpack(&tmp_dir)
            .map_err(|e| BuildContextError::Tar(format!("failed to extract tar: {}", e)))?;

        Ok(tmp_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_dir_build_context() {
        let tmp_dir = std::env::temp_dir().join("kaniko-test-dir");
        fs::create_dir_all(&tmp_dir).unwrap();

        let ctx = DirBuildContext {
            context: tmp_dir.to_string_lossy().to_string(),
        };
        let result = ctx.unpack();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), tmp_dir);

        fs::remove_dir(&tmp_dir).ok();
    }

    #[test]
    fn test_dir_build_context_nonexistent() {
        let ctx = DirBuildContext {
            context: "/nonexistent/path".to_string(),
        };
        assert!(ctx.unpack().is_err());
    }

    #[test]
    fn test_resolve_dir_context() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("dir:///tmp/test", &opts);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_resolve_tar_context() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("tar:///tmp/test.tar", &opts);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_resolve_git_context() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("git://github.com/user/repo", &opts);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_resolve_https_context() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("https://example.com/context.tar", &opts);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_resolve_unsupported_prefix() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("s3://bucket/context", &opts);
        assert!(ctx.is_err());
    }

    #[test]
    fn test_resolve_no_prefix() {
        let opts = GitBuildOptions::default();
        let ctx = resolve_build_context("/tmp/test", &opts);
        assert!(ctx.is_ok());
    }

    #[test]
    fn test_git_build_options_default() {
        let opts = GitBuildOptions::default();
        assert!(opts.branch.is_none());
        assert!(!opts.single_branch);
        assert!(!opts.recurse_submodules);
        assert!(!opts.insecure_skip_tls);
    }
}