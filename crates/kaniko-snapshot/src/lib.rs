//! File system snapshot module for kaniko-rs.
//!
//! Tracks file system changes and generates OCI layers from diffs.

pub mod ignore_list;
pub mod hasher;
pub mod layered_map;
pub mod snapshotter;
pub mod volumes;
pub mod walker;

pub use ignore_list::{
    IgnoreListEntry, KANIKO_DIR,
    init_ignore_list, add_to_ignore_list, add_to_default_ignore_list,
    add_var_run_to_ignore_list, add_ignore_paths, get_ignore_list, is_in_ignore_list,
};
pub use hasher::{SnapshotMode, HasherError};
pub use layered_map::LayeredMap;
pub use snapshotter::Snapshotter;
pub use snapshotter::{check_snapshot_timeout, snapshot_timeout, parse_snapshot_timeout, DEFAULT_SNAPSHOT_TIMEOUT};
pub use volumes::{add_volume, add_volumes, volumes, is_volume, clear_volumes, add_volume_to_ignore_list};
pub use walker::{IgnorePattern, parse_dockerignore, read_dockerignore, walk_with_ignore, walk_for_snapshot};