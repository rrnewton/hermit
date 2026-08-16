/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

// RUN: %me

//! Detcore suppresses the *effect* of sched_setattr, because Linux scheduler
//! attributes cannot influence Detcore's replacement scheduler. Suppressing the
//! effect must not extend to accepting arguments Linux rejects: a guest probing
//! for EINVAL/E2BIG has to observe the same refusal it observes natively.
//!
//! The same assertions run natively (`RUN: %me`) and under Hermit
//! (`hermit-run.lit`), so the two are bracketed against each other rather than
//! against a hard-coded expectation.

use std::io::Error;
use std::ptr;

/// The base `sched_attr` descriptor, `SCHED_ATTR_SIZE_VER0`.
#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
}

/// Returns `Ok(())` when the call succeeded, otherwise the reported errno.
fn sched_setattr(attr: *mut SchedAttr) -> Result<(), i32> {
    // SAFETY: `attr` is either null or a valid pointer to a `SchedAttr` living
    // in this frame; the kernel reads at most `attr.size` bytes from it.
    let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, attr, 0) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn main() {
    let mut attr = SchedAttr {
        size: std::mem::size_of::<SchedAttr>() as u32,
        ..Default::default()
    };

    // A null attribute pointer is EINVAL, not EFAULT: the kernel rejects it
    // before attempting any access.
    assert_eq!(
        sched_setattr(ptr::null_mut()),
        Err(libc::EINVAL),
        "null sched_attr must be rejected with EINVAL"
    );

    // A buffer too small to hold the base descriptor is E2BIG.
    attr.size = 1;
    assert_eq!(
        sched_setattr(&mut attr),
        Err(libc::E2BIG),
        "undersized sched_attr must be rejected with E2BIG"
    );

    // A policy Linux does not define is EINVAL. 99 is not a policy; note that 4
    // would not be either, since SCHED_ISO is reserved and never valid.
    attr.size = std::mem::size_of::<SchedAttr>() as u32;
    attr.sched_policy = 99;
    assert_eq!(
        sched_setattr(&mut attr),
        Err(libc::EINVAL),
        "undefined scheduling policy must be rejected with EINVAL"
    );

    // A well-formed request is still accepted, so the checks above reject
    // malformed input rather than disabling the syscall.
    attr.sched_policy = libc::SCHED_OTHER as u32;
    assert_eq!(
        sched_setattr(&mut attr),
        Ok(()),
        "a well-formed SCHED_OTHER request must be accepted"
    );
}
