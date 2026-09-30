/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `libc` that Detcore and detcore-model see when they are built without
//! `std`, for the Narf kernel target (`x86_64-unknown-none`), where the
//! `libc` crate is empty. On `target_os = "none"` they bind this crate as
//! `libc`. On every other target this crate is empty and has no
//! dependencies: the host build uses the `libc` crate.
//!
//! - Reverie's own no-std `libc` module, re-exported through reverie-core
//!   (`reverie::syscalls::libc`), the one Reverie crate Detcore may depend
//!   on. Detcore passes libc values to and from Reverie's syscall types, so
//!   it must name the same types: a second copy of `fd_set` or `siginfo_t`
//!   would be a different type.
//! - The constants, typedefs and structs Detcore and detcore-model name that
//!   Reverie's module lacks, copied from libc 0.2.189.
//! - `SiginfoExt`: libc's `siginfo_t::si_pid`, as a trait method.
//! - The six libc functions they call that are pure arithmetic in libc
//!   itself (`makedev`, `major`, `minor`, `WIFSIGNALED`, `WIFEXITED`,
//!   `WEXITSTATUS`), copied from libc.
//!
//! libc's C-library functions (`fcntl`, `kill`, `mmap`, `sigfillset`, ...)
//! are absent: without `std`, Detcore must not call them.
#![no_std]
#![cfg(target_os = "none")]
#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

pub use reverie::syscalls::libc::*;

mod delta;
mod siginfo;

pub use delta::*;
pub use pure::*;
pub use siginfo::SiginfoExt;

/// libc 0.2.189's own definitions (src/unix/linux_like/linux_l4re_shared.rs
/// and src/unix/linux_like/mod.rs, as `rustc -Zunpretty=expanded` prints
/// them).
mod pure {
    use super::*;

    #[inline]
    pub const extern "C" fn makedev(major: c_uint, minor: c_uint) -> dev_t {
        let major = major as dev_t;
        let minor = minor as dev_t;
        let mut dev = 0;
        dev |= (major & 0x00000fff) << 8;
        dev |= (major & 0xfffff000) << 32;
        dev |= (minor & 0x000000ff) << 0;
        dev |= (minor & 0xffffff00) << 12;
        dev
    }

    #[inline]
    pub const extern "C" fn major(dev: dev_t) -> c_uint {
        let mut major = 0;
        major |= (dev & 0x00000000000fff00) >> 8;
        major |= (dev & 0xfffff00000000000) >> 32;
        major as c_uint
    }

    #[inline]
    pub const extern "C" fn minor(dev: dev_t) -> c_uint {
        let mut minor = 0;
        minor |= (dev & 0x00000000000000ff) >> 0;
        minor |= (dev & 0x00000ffffff00000) >> 12;
        minor as c_uint
    }

    #[inline]
    pub const extern "C" fn WIFSIGNALED(status: c_int) -> bool {
        ((status & 0x7f) + 1) as i8 >= 2
    }

    #[inline]
    pub const extern "C" fn WIFEXITED(status: c_int) -> bool {
        (status & 0x7f) == 0
    }

    #[inline]
    pub const extern "C" fn WEXITSTATUS(status: c_int) -> c_int {
        (status >> 8) & 0xff
    }
}
