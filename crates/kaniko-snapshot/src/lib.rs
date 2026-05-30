//! File system snapshot module for kaniko-rs.
//!
//! Tracks file system changes and generates OCI layers from diffs.

pub mod ignore_list;
pub mod hasher;
pub mod layered_map;
pub mod snapshotter;
pub mod volumes;
pub mod walker;
pub mod fs_util;
pub mod container;
pub mod command_util;
pub mod tar_util;
pub mod resolve;

pub use ignore_list::{
    IgnoreListEntry, KANIKO_DIR,
    init_ignore_list, add_to_ignore_list, add_to_default_ignore_list,
    add_var_run_to_ignore_list, add_ignore_paths, get_ignore_list, is_in_ignore_list,
};
pub use hasher::{SnapshotMode, HasherError, cache_hasher, mtime_hasher, redo_hasher, lgetxattr, sha256_reader};
pub use layered_map::LayeredMap;
pub use snapshotter::Snapshotter;
pub use snapshotter::{check_snapshot_timeout, snapshot_timeout, parse_snapshot_timeout, DEFAULT_SNAPSHOT_TIMEOUT};
pub use volumes::{add_volume, add_volumes, volumes, is_volume, clear_volumes, add_volume_to_ignore_list};
pub use walker::{IgnorePattern, parse_dockerignore, read_dockerignore, walk_with_ignore, walk_for_snapshot};
pub use fs_util::{
    delete_filesystem, detect_filesystem_ignore_list, rooted_path,
    parent_directories, parent_directories_without_leading_slash,
    relative_files, destination_filepath, is_dest_dir, filepath_exists,
    create_file, is_src_remote_file_url, contains_wildcards, resolve_sources,
    KANIKO_ROOT_DIR,
    is_symlink, copy_file_or_symlink, copy_ownership, create_parent_directory,
    mkdir_all_with_permissions, set_file_permissions, set_file_times,
    filepath_has_prefix, check_cleaned_path_against_ignore_list, eval_symlink,
    FileContext, new_file_context_from_dockerfile,
    determine_target_file_ownership, copy_dir_with_exclude, copy_file_with_exclude,
};
pub use container::{
    ContainerRuntime, get_container_runtime, is_running_in_container,
};
pub use command_util::{
    resolve_environment_replacement, resolve_environment_replacement_list,
    is_srcs_valid, is_dest_dir_in_root, url_destination_filepath,
    get_user_group, get_chmod,
    DO_NOT_CHANGE_UID, DO_NOT_CHANGE_GID,
    update_config_env, docker_conf_location,
    resolve_env_and_wildcards,
};
pub use tar_util::{
    TarWriter, create_tarball_of_directory, tar_path_from_root,
    is_file_local_tar_archive, unpack_local_tar_archive, unpack_compressed_tar,
};
pub use resolve::{
    resolve_paths, files_with_parent_dirs, resolve_symlink_ancestor,
};