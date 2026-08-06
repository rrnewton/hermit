/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest program that reads **every** randomness source Hermit is expected to
//! determinize, and prints one line per source.
//!
//! This is the guest half of the randomness contract fixture
//! (`detcore/tests/misc/mod.rs::randomness_sources_are_determinized`). It exists
//! as a separate binary rather than an in-process closure because Detcore's
//! `RDRAND`/`RDSEED` rewriting is armed at `execve` and at executable `mmap`; a
//! forked-but-not-exec'd guest would never be rewritten, and the fixture would
//! report a hole that is really an artifact of how the test was launched.
//!
//! # Why each source is read at the lowest available layer
//!
//! A libc wrapper can quietly substitute one mechanism for another (glibc's
//! `getrandom` falls back to `/dev/urandom`; `arc4random` may be built on
//! `getentropy`). Reading through the wrapper would let one determinized source
//! stand in for an undeterminized one and hide the gap. So `getrandom` is issued
//! as a raw syscall, the character devices are opened and read directly,
//! `AT_RANDOM` is taken from the auxiliary vector, and `RDRAND`/`RDSEED` are
//! emitted as instructions.
//!
//! # The CPUID-ignoring requirement
//!
//! `RDRAND` and `RDSEED` are issued **without consulting CPUID**. Detcore
//! reports their feature bits as absent, so a probe that asks first would take a
//! fallback path and the fixture would pass while the instruction remained a
//! live entropy source. `#[target_feature(enable = "rdrand")]` makes the
//! compiler emit the instruction unconditionally, which is exactly the
//! real-world shape this pins down: hand-written assembly, a binary built with
//! `-mrdrnd`, a JIT, or a crypto library that probes-or-just-uses.

use std::ffi::CString;
use std::fmt::Write as _;
use std::os::raw::c_void;

/// Bytes drawn from each source. Enough to make a collision between two
/// independent draws implausible, small enough to keep the output readable.
const DRAW: usize = 16;

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `getrandom(2)` as a raw syscall, bypassing any libc fallback.
fn getrandom_raw() -> String {
    let mut buf = [0u8; DRAW];
    let rc = unsafe {
        libc::syscall(
            libc::SYS_getrandom,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
        )
    };
    if rc != buf.len() as i64 {
        return format!("ERRNO:{}", std::io::Error::last_os_error());
    }
    hex(&buf)
}

/// Read a character device directly rather than through any caching wrapper.
fn read_device(path: &str) -> String {
    let Ok(c_path) = CString::new(path) else {
        return "BADPATH".to_owned();
    };
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return format!("ERRNO:{}", std::io::Error::last_os_error());
    }
    let mut buf = [0u8; DRAW];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    unsafe { libc::close(fd) };
    if n != buf.len() as isize {
        return format!("SHORT:{n}");
    }
    hex(&buf)
}

/// The kernel-supplied `AT_RANDOM` bytes from the auxiliary vector.
fn at_random() -> String {
    let ptr = unsafe { libc::getauxval(libc::AT_RANDOM) } as *const u8;
    if ptr.is_null() {
        return "ABSENT".to_owned();
    }
    // AT_RANDOM always points at exactly 16 kernel-provided bytes.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, 16) };
    hex(bytes)
}

fn getentropy() -> String {
    let mut buf = [0u8; DRAW];
    let rc = unsafe { libc::getentropy(buf.as_mut_ptr() as *mut c_void, buf.len()) };
    if rc != 0 {
        return format!("ERRNO:{}", std::io::Error::last_os_error());
    }
    hex(&buf)
}

/// `arc4random_buf` resolved at runtime.
///
/// It only exists in glibc 2.36+, so it is looked up rather than linked. A
/// missing symbol is reported as `ABSENT` instead of being skipped silently:
/// the fixture records which sources it actually covered, so that "this
/// platform has no arc4random" can never be mistaken for "arc4random is
/// determinized here".
fn arc4random() -> String {
    type Arc4RandomBuf = unsafe extern "C" fn(*mut c_void, usize);
    let name = c"arc4random_buf";
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if symbol.is_null() {
        return "ABSENT".to_owned();
    }
    let arc4random_buf: Arc4RandomBuf = unsafe { std::mem::transmute(symbol) };
    let mut buf = [0u8; DRAW];
    unsafe { arc4random_buf(buf.as_mut_ptr() as *mut c_void, buf.len()) };
    hex(&buf)
}

/// Issue `RDRAND` with no CPUID check. See the module docs.
///
/// The instruction may legitimately report failure (`CF=0`) on real hardware,
/// so a bounded retry is used; exhausting it is reported rather than silently
/// yielding zeros.
#[target_feature(enable = "rdrand")]
unsafe fn rdrand_unconditional() -> String {
    let mut out = [0u8; DRAW];
    for chunk in out.chunks_mut(8) {
        let mut value: u64 = 0;
        let mut ok = 0;
        for _ in 0..64 {
            ok = core::arch::x86_64::_rdrand64_step(&mut value);
            if ok == 1 {
                break;
            }
        }
        if ok != 1 {
            return "CF0".to_owned();
        }
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    hex(&out)
}

/// Issue `RDSEED` with no CPUID check. See the module docs.
#[target_feature(enable = "rdseed")]
unsafe fn rdseed_unconditional() -> String {
    let mut out = [0u8; DRAW];
    for chunk in out.chunks_mut(8) {
        let mut value: u64 = 0;
        let mut ok = 0;
        for _ in 0..1024 {
            ok = core::arch::x86_64::_rdseed64_step(&mut value);
            if ok == 1 {
                break;
            }
        }
        if ok != 1 {
            return "CF0".to_owned();
        }
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    hex(&out)
}

fn main() {
    // Stable order: the fixture compares the whole stream byte for byte.
    println!("getrandom {}", getrandom_raw());
    println!("urandom {}", read_device("/dev/urandom"));
    println!("random {}", read_device("/dev/random"));
    println!("at_random {}", at_random());
    println!("getentropy {}", getentropy());
    println!("arc4random {}", arc4random());
    // SAFETY: x86-64 always has these instructions decodable; whether the CPU
    // *advertises* them is deliberately not consulted, which is the point.
    println!("rdrand {}", unsafe { rdrand_unconditional() });
    println!("rdseed {}", unsafe { rdseed_unconditional() });
}
