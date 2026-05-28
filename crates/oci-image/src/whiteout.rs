//! OCI Whiteout specification implementation.
//!
//! Whiteout files mark files/directories to be deleted in a layer.
//! They follow the OCI Image Spec convention:
//! - `.wh.<filename>` — whiteout a regular file/directory
//! - `.wh..wh..opq` — opaque whiteout (whiteout everything in the directory)
//!
//! Analogous to Go: `snapshot.removeObsoleteWhiteouts()`.

use std::path::{Path, PathBuf};

/// A whiteout entry representing a file or directory deletion.
#[derive(Debug, Clone, PartialEq)]
pub enum WhiteoutEntry {
    /// Whiteout a specific file or directory.
    /// The path is relative to the layer root.
    Regular {
        /// The parent directory of the whiteout file.
        parent: PathBuf,
        /// The name of the file/directory being whited out.
        name: String,
    },
    /// Opaque whiteout — hide everything in this directory from lower layers.
    Opaque {
        /// The directory containing the opaque whiteout marker.
        directory: PathBuf,
    },
}

impl WhiteoutEntry {
    /// Create a regular whiteout entry for a file/directory at the given path.
    pub fn regular(path: &Path) -> Self {
        let parent = path.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        Self::Regular { parent, name }
    }

    /// Create an opaque whiteout entry for a directory.
    pub fn opaque(directory: &Path) -> Self {
        Self::Opaque {
            directory: directory.to_path_buf(),
        }
    }

    /// Returns the path this whiteout entry would have in a tar archive.
    ///
    /// For regular whiteouts: `<parent>/.wh.<name>`
    /// For opaque whiteouts: `<directory>/.wh..wh..opq`
    pub fn tar_path(&self) -> PathBuf {
        match self {
            WhiteoutEntry::Regular { parent, name } => {
                parent.join(format!(".wh.{}", name))
            }
            WhiteoutEntry::Opaque { directory } => {
                directory.join(".wh..wh..opq")
            }
        }
    }

    /// Check if a filename represents a whiteout file.
    pub fn is_whiteout_filename(name: &str) -> bool {
        name.starts_with(".wh.")
    }

    /// Check if a filename represents an opaque whiteout.
    pub fn is_opaque_whiteout(name: &str) -> bool {
        name == ".wh..wh..opq"
    }

    /// Parse a whiteout file path into a WhiteoutEntry.
    ///
    /// Returns None if the path is not a valid whiteout.
    pub fn from_tar_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        if !Self::is_whiteout_filename(name) {
            return None;
        }

        let parent = path.parent()?;

        if Self::is_opaque_whiteout(name) {
            Some(Self::Opaque {
                directory: parent.to_path_buf(),
            })
        } else {
            // Strip ".wh." prefix to get the original filename
            let original_name = &name[4..];
            Some(Self::Regular {
                parent: parent.to_path_buf(),
                name: original_name.to_string(),
            })
        }
    }

    /// Get the original file path that this whiteout is deleting.
    pub fn original_path(&self) -> PathBuf {
        match self {
            WhiteoutEntry::Regular { parent, name } => parent.join(name),
            WhiteoutEntry::Opaque { directory } => directory.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regular_whiteout() {
        let entry = WhiteoutEntry::regular(Path::new("usr/local/bin/app"));
        assert_eq!(entry.tar_path(), PathBuf::from("usr/local/bin/.wh.app"));
    }

    #[test]
    fn test_opaque_whiteout() {
        let entry = WhiteoutEntry::opaque(Path::new("tmp/data"));
        assert_eq!(entry.tar_path(), PathBuf::from("tmp/data/.wh..wh..opq"));
    }

    #[test]
    fn test_parse_regular_whiteout() {
        let parsed = WhiteoutEntry::from_tar_path(Path::new("etc/.wh.resolv.conf"));
        assert!(parsed.is_some());
        let entry = parsed.unwrap();
        assert_eq!(entry.original_path(), PathBuf::from("etc/resolv.conf"));
    }

    #[test]
    fn test_parse_opaque_whiteout() {
        let parsed = WhiteoutEntry::from_tar_path(Path::new("tmp/.wh..wh..opq"));
        assert!(parsed.is_some());
        let entry = parsed.unwrap();
        match entry {
            WhiteoutEntry::Opaque { directory } => {
                assert_eq!(directory, PathBuf::from("tmp"));
            }
            _ => panic!("Expected opaque whiteout"),
        }
    }

    #[test]
    fn test_non_whiteout() {
        assert!(!WhiteoutEntry::is_whiteout_filename("regular_file"));
        assert!(WhiteoutEntry::is_whiteout_filename(".wh.something"));
        assert!(WhiteoutEntry::is_opaque_whiteout(".wh..wh..opq"));
        assert!(!WhiteoutEntry::is_opaque_whiteout(".wh.something"));
    }
}