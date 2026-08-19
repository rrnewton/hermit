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

/// A convention of how we set up our PID namespace leaves us with a starting pid of 3.
pub const ROOT_DETPID: DetPid = DetPid::from_raw(3);

/// Tracing target for the scheduler's per-turn banners.
///
/// These six emissions narrate every scheduling turn, and one of them (the
/// `poll_attempt > 0` skip in `scheduler.rs`) fires once per POLL ATTEMPT on an
/// already-blocked thread -- one line per unit of NOT making progress. On
/// 2026-08-17 that property let three `--log=info` runs write ~4.5 TB of stderr
/// each and fill the filesystem; in a sample of the surviving log, 133,290 of
/// 133,299 non-blank lines were these banners.
///
/// Giving them their own target means an env-filter can silence exactly them:
///
///     RUST_LOG=info,detcore::scheduler::turn=warn
///
/// which is NOT the same as `detcore::scheduler=warn` -- that blanket form also
/// drops the scheduler's startup, run-queue-empty, time-skip and
/// `logically_kill` lines, some of which `demos/lib/qemu-snapshot.sh` matches on.
///
/// DO NOT quiet this target when running with `--verify`. `hermit`'s log-diff
/// consumes `COMMIT` lines for the determinism comparison and only skips them
/// when `--skip-commit` is passed, which is NOT the default. Silencing these
/// banners under `--verify` would shrink the compared surface and make the
/// determinism check weaker while still reporting success. Bound the SINK
/// instead (see `ci-hub/bin/cap-log`), which costs no evidence.
pub const SCHED_TURN_TARGET: &str = "detcore::scheduler::turn";
