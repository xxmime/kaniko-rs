//! File system snapshot module for kaniko-rs.
//!
//! Tracks file system changes and generates OCI layers from diffs.

pub mod layered_map;
pub mod snapshotter;
pub mod walker;

pub use layered_map::LayeredMap;
pub use snapshotter::Snapshotter;
pub use walker::{IgnorePattern, parse_dockerignore, read_dockerignore, walk_with_ignore, walk_for_snapshot};