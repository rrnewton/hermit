/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/// The name of the JSON metadata file that is saved for each recording.
pub const METADATA_NAME: &str = "metadata.json";

/// The name of the root executable.
pub const EXE_NAME: &str = "exe";

/// The name of the newline-separated file listing absolute paths of every
/// executable that the guest `execve`/`execveat`'d during a recording. The
/// replayer uses this to populate its chroot so that child processes can
/// re-exec the same binaries.
pub const EXEC_PATHS_NAME: &str = "exec_paths";

/// Directory containing content-addressed regular-file snapshots.
pub const FILES_DIR_NAME: &str = "files";

/// Inspectable metadata for files captured by `--record-fs`.
pub const FILES_MANIFEST_NAME: &str = "files.json";

/// Absolute path where snapshots are exposed inside the replay chroot.
pub const REPLAY_FILES_ROOT: &str = "/.hermit-recorded-files";
