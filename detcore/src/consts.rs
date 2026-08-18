/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Constant values.

#![allow(unused)]

use reverie::Pid;

use crate::types::DetInode;
use crate::types::DetPid;
use crate::types::DetTid;

/// A separate offset for special devices, including file descriptors 0,1,2
/// stdin/stdout/stderr.
pub static DET_SPECIAL_INODE_OFFSET: DetInode = DetInode::mint(1000);

/// The starting point for deterministic inodes *other* than those addressed by
/// `DET_SPECIAL_INODE_OFFSET`.
pub static DET_INODE_OFFSET: DetInode = DetInode::mint(9000);

pub const DEFAULT_HOSTNAME: &str = "hermetic-container.local";

/// The pipe capacity every guest pipe is given, in bytes.
///
/// Linux sizes a new pipe from the *host's* per-user pipe-page accounting: a
/// user under `/proc/sys/fs/pipe-user-pages-soft` gets 65536, and a user over
/// it gets `PIPE_MIN_DEF_SIZE` (two pages). That threshold depends on every
/// other process on the machine, and a parallel validate crosses it using only
/// its own concurrent guests. Guests do not merely observe the value; they size
/// their work from it (`tests/c/writev_determinism.c` reads `F_GETPIPE_SZ` and
/// then allocates and writes that many bytes), so the host's answer changes the
/// guest's schedule.
///
/// Two pages is the one capacity that is always reachable. Shrinking an empty
/// pipe cannot fail, while growing one back to 65536 is refused for an
/// unprivileged user over the soft limit -- which is exactly the condition being
/// determinized. Picking the larger value would leave the pressured host unable
/// to honour it.
pub const DET_PIPE_CAPACITY: i32 = 8192;

/// A convention of how we set up our PID namespace leaves us with a starting pid of 3.
pub const ROOT_DETPID: DetPid = DetPid::from_raw(3);
