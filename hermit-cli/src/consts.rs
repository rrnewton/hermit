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

/// The name of the newline-delimited manifest listing every executable that was
/// `execve`'d during a recording. Replay uses this to stage all exec'd binaries
/// (not just the root program) into the replay chroot.
pub const EXECUTABLES_NAME: &str = "executables";
