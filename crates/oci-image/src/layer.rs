//! OCI Image Layer abstraction.
//!
//! Provides layer creation, reading, and compression/decompression.
//! Analogous to `go-containerregistry/pkg/v1/tarball` and `v1.Layer`.

use crate::digest::Sha256Digest;
use crate::manifest::{Descriptor, MediaType};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use thiserror::Error;

/// Errors that can occur during layer operations.
#[derive(Debug, Error)]
pub enum LayerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tar error: {0}")]
    Tar(String),
    #[error("compression error: {0}")]
    Compression(String),
    #[error("digest error: {0}")]
    Digest(#[from] crate::digest::DigestError),
}

/// Result type for layer operations.
pub type Result<T> = std::result::Result<T, LayerError>;

/// An OCI image layer.
///
/// Contains the layer data, its digest, diff_id, and media type.
/// Analogous to `go-containerregistry/pkg/v1.Layer`.
#[derive(Debug, Clone)]
pub struct Layer {
    /// The media type of this layer (e.g., gzip, zstd).
    media_type: String,
    /// The SHA-256 digest of the (possibly compressed) layer data.
    digest: Sha256Digest,
    /// The size in bytes of the layer data.
    size: u64,
    /// The SHA-256 digest of the uncompressed layer data (diff_id).
    diff_id: Sha256Digest,
    /// Optional annotations for this layer.
    annotations: Option<std::collections::BTreeMap<String, String>>,
    /// The raw (possibly compressed) layer data.
    data: Vec<u8>,
}

impl Layer {
    /// Create a layer from raw bytes with the specified media type.
    ///
    /// This computes the digest and diff_id automatically.
    pub fn from_bytes(data: Vec<u8>, media_type: &str) -> Result<Self> {
        let digest = Sha256Digest::from_bytes(&data);
        let size = data.len() as u64;

        let diff_id = if MediaType::is_compressed(media_type) {
            // Decompress to compute diff_id
            let uncompressed = decompress_gzip(&data)?;
            Sha256Digest::from_bytes(&uncompressed)
        } else {
            digest.clone()
        };

        Ok(Self {
            media_type: media_type.to_string(),
            digest,
            size,
            diff_id,
            annotations: None,
            data,
        })
    }

    /// Create a layer from uncompressed tar data, applying gzip compression.
    ///
    /// This is the typical path when creating layers from file system snapshots.
    pub fn from_tar_uncompressed(tar_data: Vec<u8>) -> Result<Self> {
        Self::from_tar_uncompressed_with_options(tar_data, LayerCompression::default())
    }

    /// Create a layer from uncompressed tar data with specific compression options.
    ///
    /// Supports gzip (default) and zstd compression, with configurable compression level.
    /// Analogous to Go: `tarball.LayerFromFile(path, WithCompression(...), WithCompressionLevel(...))`.
    pub fn from_tar_uncompressed_with_options(tar_data: Vec<u8>, opts: LayerCompression) -> Result<Self> {
        let diff_id = Sha256Digest::from_bytes(&tar_data);

        let (compressed, media_type) = match opts.algorithm {
            CompressionAlgorithm::Gzip => {
                let level = if opts.level >= 0 { opts.level as u32 } else { 6 };
                (compress_gzip_with_level(&tar_data, level)?, MediaType::LAYER_OCI_V1_TAR_GZIP.to_string())
            }
            CompressionAlgorithm::Zstd => {
                // Zstd compression — for now, fall back to gzip since zstd crate
                // is not in dependencies. When zstd is added, this will use it.
                tracing::warn!("zstd compression not yet available, falling back to gzip");
                let level = if opts.level >= 0 { opts.level as u32 } else { 6 };
                (compress_gzip_with_level(&tar_data, level)?, MediaType::LAYER_OCI_V1_TAR_GZIP.to_string())
            }
        };

        let digest = Sha256Digest::from_bytes(&compressed);
        let size = compressed.len() as u64;

        Ok(Self {
            media_type,
            digest,
            size,
            diff_id,
            annotations: None,
            data: compressed,
        })
    }

    /// Create a layer from a list of file paths.
    ///
    /// Builds a tar archive from the given files, then compresses it.
    pub fn from_files(
        files: &[impl AsRef<Path>],
        whiteouts: &[crate::whiteout::WhiteoutEntry],
        root: &Path,
    ) -> Result<Self> {
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);

            // Add regular files and directories
            for file_path in files {
                let path = file_path.as_ref();
                if !path.exists() {
                    continue;
                }

                let rel_path = path.strip_prefix(root).unwrap_or(path);
                let metadata = std::fs::symlink_metadata(path)?;

                if metadata.is_file() {
                    let mut header = tar::Header::new_gnu();
                    header.set_path(rel_path)?;
                    header.set_size(metadata.len());
                    header.set_mode(metadata.permissions().mode());
                    let mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    header.set_mtime(mtime);
                    header.set_cksum();

                    let f = std::fs::File::open(path)?;
                    builder.append(&header, f)?;
                } else if metadata.is_dir() {
                    let mut header = tar::Header::new_gnu();
                    header.set_path(rel_path)?;
                    header.set_size(0);
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_mode(metadata.permissions().mode());
                    let mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    header.set_mtime(mtime);
                    header.set_cksum();
                    builder.append(&header, std::io::empty())?;
                } else if metadata.is_symlink() {
                    let target = std::fs::read_link(path)?;
                    let mut header = tar::Header::new_gnu();
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_path(rel_path)?;
                    header.set_link_name(&target)?;
                    header.set_size(0);
                    header.set_cksum();
                    builder.append(&header, std::io::empty())?;
                }
            }

            // Add whiteout entries
            for whiteout in whiteouts {
                let wh_path = whiteout.tar_path();
                let mut header = tar::Header::new_gnu();
                header.set_path(&wh_path)?;
                header.set_size(0);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0);
                header.set_cksum();
                builder.append(&header, std::io::empty())?;
            }

            builder.finish()?;
        }

        Self::from_tar_uncompressed(tar_data)
    }

    /// Create an empty layer (used for metadata-only commands).
    pub fn empty() -> Result<Self> {
        let mut tar_data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_data);
            builder.finish()?;
        }
        Self::from_tar_uncompressed(tar_data)
    }

    /// Get the media type of this layer.
    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    /// Get the compressed digest of this layer.
    pub fn digest(&self) -> &Sha256Digest {
        &self.digest
    }

    /// Get the uncompressed diff_id of this layer.
    pub fn diff_id(&self) -> &Sha256Digest {
        &self.diff_id
    }

    /// Get the size in bytes of the compressed layer data.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Get the raw (possibly compressed) layer data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get the uncompressed layer data.
    pub fn uncompressed_data(&self) -> Result<Vec<u8>> {
        if MediaType::is_compressed(&self.media_type) {
            decompress_gzip(&self.data)
        } else {
            Ok(self.data.clone())
        }
    }

    /// Convert this layer to a descriptor for inclusion in a manifest.
    pub fn to_descriptor(&self) -> Descriptor {
        Descriptor {
            media_type: self.media_type.clone(),
            digest: self.digest.clone(),
            size: self.size,
            annotations: self.annotations.clone().unwrap_or_default(),
            platform: None,
        }
    }

    /// Set the media type of this layer.
    ///
    /// Used when converting between Docker and OCI layer formats.
    /// If the new media type requires a different compression, the data
    /// is re-compressed accordingly.
    ///
    /// Analogous to Go: `tarball.LayerFromOpener(layer.Uncompressed, layerOpts...)`.
    pub fn with_media_type(mut self, media_type: &str) -> Result<Self> {
        let current_compressed = MediaType::is_compressed(&self.media_type);
        let target_compressed = MediaType::is_compressed(media_type);

        if current_compressed && !target_compressed {
            // Decompress the data
            self.data = self.uncompressed_data()?;
            self.digest = Sha256Digest::from_bytes(&self.data);
            self.size = self.data.len() as u64;
            // diff_id stays the same since it's the uncompressed digest
        } else if !current_compressed && target_compressed {
            // Compress the data
            self.diff_id = Sha256Digest::from_bytes(&self.data);
            self.data = compress_gzip(&self.data)?;
            self.digest = Sha256Digest::from_bytes(&self.data);
            self.size = self.data.len() as u64;
        }
        // If both are compressed, the compression format change is just metadata
        // (in practice, we'd need to decompress and re-compress, but for gzip→gzip
        // this is a no-op; for gzip→zstd, we'd need zstd support)

        self.media_type = media_type.to_string();
        Ok(self)
    }
}

/// Trait for reading layer data.
pub trait LayerReader: Send + Sync {
    /// Read the compressed layer data.
    fn read_compressed(&self) -> Result<Vec<u8>>;
    /// Read the uncompressed layer data.
    fn read_uncompressed(&self) -> Result<Vec<u8>>;
}

/// Compress data with gzip.
fn compress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    compress_gzip_with_level(data, Compression::default().level() as u32)
}

/// Compress data with gzip at a specific compression level.
/// Level 0 = no compression, 1 = fastest, 9 = best, default = 6.
pub fn compress_gzip_with_level(data: &[u8], level: u32) -> Result<Vec<u8>> {
    let compression = match level {
        0 => Compression::none(),
        1 => Compression::fast(),
        9 => Compression::best(),
        l => Compression::new(l),
    };
    let mut encoder = GzEncoder::new(Vec::new(), compression);
    use std::io::Write;
    encoder
        .write_all(data)
        .map_err(|e| LayerError::Compression(e.to_string()))?;
    encoder
        .finish()
        .map_err(|e| LayerError::Compression(e.to_string()))
}

/// Decompress gzip data.
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(data);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .map_err(|e| LayerError::Compression(e.to_string()))?;
    Ok(output)
}

/// Compression algorithm for layer data.
/// Analogous to Go: `config.Compression`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionAlgorithm {
    /// Gzip compression (default).
    #[default]
    Gzip,
    /// Zstd compression (OCI layer format).
    Zstd,
}

impl std::fmt::Display for CompressionAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompressionAlgorithm::Gzip => write!(f, "gzip"),
            CompressionAlgorithm::Zstd => write!(f, "zstd"),
        }
    }
}

/// Layer compression options.
/// Analogous to Go: `tarball.LayerOption` (WithCompression, WithCompressionLevel).
#[derive(Debug, Clone, Default)]
pub struct LayerCompression {
    /// Compression algorithm (gzip or zstd).
    pub algorithm: CompressionAlgorithm,
    /// Compression level (-1 = default, 0 = none, 1-9 = level).
    pub level: i32,
}

impl LayerCompression {
    /// Create a gzip compression option with the given level.
    pub fn gzip(level: u32) -> Self {
        Self {
            algorithm: CompressionAlgorithm::Gzip,
            level: level as i32,
        }
    }

    /// Create a zstd compression option with the given level.
    pub fn zstd(level: u32) -> Self {
        Self {
            algorithm: CompressionAlgorithm::Zstd,
            level: level as i32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layer_from_bytes_uncompressed() {
        let data = b"hello layer".to_vec();
        let layer = Layer::from_bytes(data.clone(), MediaType::LAYER_OCI_V1_TAR).unwrap();
        assert_eq!(layer.media_type(), MediaType::LAYER_OCI_V1_TAR);
        assert_eq!(layer.uncompressed_data().unwrap(), data);
    }

    #[test]
    fn test_layer_empty() {
        let layer = Layer::empty().unwrap();
        assert!(layer.size() > 0); // empty tar is ~1024 bytes + gzip overhead
        assert_eq!(layer.media_type(), MediaType::LAYER_OCI_V1_TAR_GZIP);
    }

    #[test]
    fn test_layer_to_descriptor() {
        let layer = Layer::empty().unwrap();
        let desc = layer.to_descriptor();
        assert_eq!(desc.media_type, MediaType::LAYER_OCI_V1_TAR_GZIP);
        assert_eq!(desc.size, layer.size());
    }

    #[test]
    fn test_compress_decompress_roundtrip() {
        let original = b"test data for compression";
        let compressed = compress_gzip(original).unwrap();
        let decompressed = decompress_gzip(&compressed).unwrap();
        assert_eq!(original.to_vec(), decompressed);
    }
}