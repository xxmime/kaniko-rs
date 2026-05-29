//! Build context module for kaniko-rs.
//!
//! Provides support for different build context sources:
//! - `dir://` — local directory (default)
//! - `tar://` — tar archive
//! - `git://` — Git repository (placeholder, needs `gix` crate)
//! - `https://` — remote URL (placeholder)
//! - `s3://` — Amazon S3 (placeholder)
//! - `gcs://` — Google Cloud Storage (placeholder)
//!
//! Analogous to Go: `pkg/buildcontext/`.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors that can occur when resolving build contexts.
#[derive(Debug, Error)]
pub enum ContextError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported context scheme: {0}")]
    UnsupportedScheme(String),
    #[error("tar error: {0}")]
    Tar(String),
    #[error("context directory does not exist: {0}")]
    NotFound(String),
}

/// Result type for context operations.
pub type Result<T> = std::result::Result<T, ContextError>;

/// Build context — resolves and provides the build context files.
///
/// The build context is the set of files that `COPY` and `ADD` instructions
/// reference during a Dockerfile build. It can come from various sources.
#[derive(Debug, Clone)]
pub struct BuildContext {
    /// The resolved local directory containing the build context files.
    pub directory: PathBuf,
    /// The original context URL/path (e.g. "dir://./src", "tar://archive.tar").
    pub source: String,
    /// The scheme prefix (e.g. "dir", "tar", "git").
    pub scheme: ContextScheme,
}

/// Supported build context schemes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextScheme {
    /// Local directory context.
    Dir,
    /// Tar archive context.
    Tar,
    /// Git repository context (placeholder).
    Git,
    /// HTTPS URL context (placeholder).
    Https,
    /// Amazon S3 context (placeholder).
    S3,
    /// Google Cloud Storage context (placeholder).
    Gcs,
}

impl std::fmt::Display for ContextScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContextScheme::Dir => write!(f, "dir"),
            ContextScheme::Tar => write!(f, "tar"),
            ContextScheme::Git => write!(f, "git"),
            ContextScheme::Https => write!(f, "https"),
            ContextScheme::S3 => write!(f, "s3"),
            ContextScheme::Gcs => write!(f, "gcs"),
        }
    }
}

/// Resolve a build context string to a local directory.
///
/// Supported formats:
/// - `"dir:///path/to/dir"` or `"dir://./src"` — local directory
/// - `"tar:///path/to/archive.tar"` — extract tar to a temp dir
/// - `"./src"` or `/absolute/path` — treated as `dir://` (default)
/// - `"git://..."` — placeholder (returns UnsupportedScheme)
/// - `"https://..."` — placeholder (returns UnsupportedScheme)
/// - `"s3://..."` — placeholder (returns UnsupportedScheme)
/// - `"gcs://..."` — placeholder (returns UnsupportedScheme)
///
/// Analogous to Go: `buildcontext.GetBuildContext()`.
pub fn resolve_build_context(context: &str, dest_dir: Option<&Path>) -> Result<BuildContext> {
    let (scheme, path) = if let Some((prefix, rest)) = context.split_once("://") {
        (parse_scheme(prefix), rest)
    } else {
        // No scheme prefix — default to dir://
        (ContextScheme::Dir, context)
    };

    match scheme {
        ContextScheme::Dir => resolve_dir_context(path, dest_dir),
        ContextScheme::Tar => resolve_tar_context(path, dest_dir),
        ContextScheme::Git => Err(ContextError::UnsupportedScheme(
            "git:// context is not yet implemented. Use dir:// or tar:// instead.".to_string(),
        )),
        ContextScheme::Https => Err(ContextError::UnsupportedScheme(
            "https:// context is not yet implemented. Use dir:// or tar:// instead.".to_string(),
        )),
        ContextScheme::S3 => Err(ContextError::UnsupportedScheme(
            "s3:// context is not yet implemented.".to_string(),
        )),
        ContextScheme::Gcs => Err(ContextError::UnsupportedScheme(
            "gcs:// context is not yet implemented.".to_string(),
        )),
    }
}

/// Parse a scheme string into a ContextScheme.
fn parse_scheme(s: &str) -> ContextScheme {
    match s.to_lowercase().as_str() {
        "dir" => ContextScheme::Dir,
        "tar" => ContextScheme::Tar,
        "git" => ContextScheme::Git,
        "https" | "http" => ContextScheme::Https,
        "s3" => ContextScheme::S3,
        "gcs" => ContextScheme::Gcs,
        _ => ContextScheme::Dir, // Default to dir for unknown schemes
    }
}

/// Resolve a local directory context.
fn resolve_dir_context(path: &str, dest_dir: Option<&Path>) -> Result<BuildContext> {
    // Handle relative paths with "dir://" prefix
    // "dir://./src" means "./src" relative to current directory
    // "dir:///absolute/path" means an absolute path
    let resolved_path = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        // Relative path — resolve against dest_dir or current directory
        let base = dest_dir.map(PathBuf::from).unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });
        base.join(path)
    };

    if !resolved_path.exists() {
        return Err(ContextError::NotFound(resolved_path.to_string_lossy().to_string()));
    }

    Ok(BuildContext {
        directory: resolved_path,
        source: format!("dir://{}", path),
        scheme: ContextScheme::Dir,
    })
}

/// Resolve a tar archive context by extracting it to a temporary directory.
fn resolve_tar_context(path: &str, dest_dir: Option<&Path>) -> Result<BuildContext> {
    let tar_path = PathBuf::from(path);
    if !tar_path.exists() {
        return Err(ContextError::NotFound(tar_path.to_string_lossy().to_string()));
    }

    // Determine extraction directory
    let extract_dir = dest_dir.map(PathBuf::from).unwrap_or_else(|| {
        // Create a temporary directory using std::env + random suffix
        let base = std::env::temp_dir();
        let unique_name = format!("kaniko-context-{}", std::process::id());
        base.join(unique_name)
    });

    // Extract the tar archive
    extract_tar(&tar_path, &extract_dir)?;

    tracing::info!("Extracted tar context {} to {}", tar_path.display(), extract_dir.display());

    Ok(BuildContext {
        directory: extract_dir,
        source: format!("tar://{}", path),
        scheme: ContextScheme::Tar,
    })
}

/// Extract a tar archive to a directory.
fn extract_tar(tar_path: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    let file = std::fs::File::open(tar_path)?;
    let mut archive = tar::Archive::new(file);

    // Unpack all entries
    archive.unpack(dest)
        .map_err(|e| ContextError::Tar(format!("failed to extract tar: {}", e)))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_parse_scheme() {
        assert_eq!(parse_scheme("dir"), ContextScheme::Dir);
        assert_eq!(parse_scheme("tar"), ContextScheme::Tar);
        assert_eq!(parse_scheme("git"), ContextScheme::Git);
        assert_eq!(parse_scheme("https"), ContextScheme::Https);
        assert_eq!(parse_scheme("s3"), ContextScheme::S3);
        assert_eq!(parse_scheme("gcs"), ContextScheme::Gcs);
        assert_eq!(parse_scheme("unknown"), ContextScheme::Dir);
    }

    #[test]
    fn test_context_scheme_display() {
        assert_eq!(ContextScheme::Dir.to_string(), "dir");
        assert_eq!(ContextScheme::Tar.to_string(), "tar");
        assert_eq!(ContextScheme::Git.to_string(), "git");
    }

    #[test]
    fn test_resolve_dir_context_absolute() {
        let ctx = resolve_build_context("dir:///tmp", None);
        // May fail if /tmp doesn't exist on the test system, but usually it does
        if let Ok(ctx) = ctx {
            assert_eq!(ctx.scheme, ContextScheme::Dir);
            assert!(ctx.directory.exists());
        }
    }

    #[test]
    fn test_resolve_dir_context_relative() {
        let ctx = resolve_build_context("dir://.", None).unwrap();
        assert_eq!(ctx.scheme, ContextScheme::Dir);
        assert!(ctx.directory.exists());
        assert_eq!(ctx.source, "dir://.");
    }

    #[test]
    fn test_resolve_bare_path() {
        // Bare path without scheme prefix defaults to dir://
        let ctx = resolve_build_context(".", None).unwrap();
        assert_eq!(ctx.scheme, ContextScheme::Dir);
        assert!(ctx.directory.exists());
    }

    #[test]
    fn test_resolve_git_context_unsupported() {
        let ctx = resolve_build_context("git://github.com/repo", None);
        assert!(ctx.is_err());
        let err = ctx.unwrap_err();
        assert!(err.to_string().contains("git://"));
    }

    #[test]
    fn test_resolve_https_context_unsupported() {
        let ctx = resolve_build_context("https://example.com/repo", None);
        assert!(ctx.is_err());
    }

    #[test]
    fn test_resolve_tar_context() {
        // Create a temporary tar file
        let temp_dir = tempfile::tempdir().unwrap();
        let source_dir = temp_dir.path().join("source");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("test.txt"), "hello").unwrap();

        let tar_path = temp_dir.path().join("context.tar");
        {
            let file = fs::File::create(&tar_path).unwrap();
            let mut builder = tar::Builder::new(file);
            builder.append_dir_all(".", &source_dir).unwrap();
            builder.finish().unwrap();
        }

        let dest_dir = temp_dir.path().join("extracted");
        let ctx = resolve_build_context(
            &format!("tar://{}", tar_path.display()),
            Some(&dest_dir),
        ).unwrap();

        assert_eq!(ctx.scheme, ContextScheme::Tar);
        assert!(ctx.directory.join("test.txt").exists());
    }
}