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

/// Reach entropy **through the vDSO specifically**, with no syscall of our own
/// for a tracer to intercept.
///
/// This is a distinct source from `getrandom(2)`, not a wrapper around it. The
/// kernel's `__vdso_getrandom` seeds a per-thread userspace CSPRNG and then
/// generates from it in user space, so once seeded there is nothing on the
/// syscall boundary to see. Measured on this host: under the ptrace backend the
/// seeding *is* an intercepted `getrandom(2)`, so the stream comes out
/// deterministic — but that makes the guarantee **transitive and fragile**. It
/// holds only while the seeding syscall is intercepted, which is why a backend
/// that misses it produces varying output while a `getrandom(2)`-only probe
/// still reports success.
///
/// The full ABI is used rather than a libc wrapper, because whether libc routes
/// `getrandom` through the vDSO is a libc-version detail and would make the
/// coverage silently optional.
fn vdso_getrandom() -> String {
    /// Filled by the query call; layout fixed by the kernel ABI.
    #[repr(C)]
    #[derive(Default)]
    struct OpaqueParams {
        size_of_opaque_state: u32,
        mmap_prot: u32,
        mmap_flags: u32,
        reserved: [u32; 13],
    }
    type VdsoGetrandom =
        unsafe extern "C" fn(*mut c_void, usize, libc::c_uint, *mut c_void, usize) -> isize;

    let Some(symbol) = vdso_symbol("__vdso_getrandom") else {
        return "NOSYM".to_owned();
    };
    // SAFETY: the symbol was resolved out of the kernel-supplied vDSO image.
    let getrandom: VdsoGetrandom = unsafe { std::mem::transmute(symbol) };

    // Query mode: opaque_len == !0 asks the vDSO to describe its state block.
    let mut params = OpaqueParams::default();
    let rc = unsafe {
        getrandom(
            std::ptr::null_mut(),
            0,
            0,
            &mut params as *mut OpaqueParams as *mut c_void,
            !0usize,
        )
    };
    if rc != 0 {
        return format!("QUERY:{rc}");
    }

    let state = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            params.size_of_opaque_state as usize,
            params.mmap_prot as libc::c_int,
            params.mmap_flags as libc::c_int,
            -1,
            0,
        )
    };
    if state == libc::MAP_FAILED {
        return "MMAP_FAIL".to_owned();
    }

    let mut buf = [0u8; DRAW];
    let n = unsafe {
        getrandom(
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
            state,
            params.size_of_opaque_state as usize,
        )
    };
    unsafe { libc::munmap(state, params.size_of_opaque_state as usize) };
    if n != buf.len() as isize {
        // A negative return here means the vDSO declined and the caller is
        // expected to fall back to the syscall. Report it rather than falling
        // back, so the fixture never silently measures getrandom(2) twice.
        return format!("DECLINED:{n}");
    }
    hex(&buf)
}

/// `Elf64_Dyn`, which the `libc` crate does not expose. Layout is fixed by the
/// ELF64 ABI.
#[repr(C)]
struct Elf64Dyn {
    d_tag: i64,
    d_val: u64,
}

const DT_NULL: i64 = 0;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;

/// Resolve a symbol in the kernel-supplied vDSO image.
///
/// Hand-parsed rather than looked up with `dlsym`, because the vDSO is not in
/// the loader's global namespace.
fn vdso_symbol(want: &str) -> Option<*const c_void> {
    let base = unsafe { libc::getauxval(libc::AT_SYSINFO_EHDR) } as *const u8;
    if base.is_null() {
        return None;
    }
    // SAFETY: AT_SYSINFO_EHDR points at a well-formed ELF image the kernel maps.
    unsafe {
        let ehdr = base as *const libc::Elf64_Ehdr;
        let phdr = base.add((*ehdr).e_phoff as usize) as *const libc::Elf64_Phdr;
        let mut load_bias: isize = 0;
        let mut dynamic: *const Elf64Dyn = std::ptr::null();
        for i in 0..(*ehdr).e_phnum as usize {
            let ph = phdr.add(i);
            match (*ph).p_type {
                libc::PT_LOAD => {
                    load_bias = base as isize - ((*ph).p_vaddr as isize - (*ph).p_offset as isize);
                }
                libc::PT_DYNAMIC => {
                    dynamic = base.add((*ph).p_offset as usize) as *const Elf64Dyn;
                }
                _ => {}
            }
        }
        if dynamic.is_null() {
            return None;
        }
        let (mut strtab, mut symtab, mut count) = (
            std::ptr::null::<u8>(),
            std::ptr::null::<libc::Elf64_Sym>(),
            0usize,
        );
        let mut dyn_entry = dynamic;
        while (*dyn_entry).d_tag != DT_NULL {
            let addr = ((*dyn_entry).d_val as isize + load_bias) as usize;
            match (*dyn_entry).d_tag {
                DT_STRTAB => strtab = addr as *const u8,
                DT_SYMTAB => symtab = addr as *const libc::Elf64_Sym,
                // DT_HASH's second word is the symbol count.
                DT_HASH => count = *((addr as *const u32).add(1)) as usize,
                _ => {}
            }
            dyn_entry = dyn_entry.add(1);
        }
        if strtab.is_null() || symtab.is_null() {
            return None;
        }
        for i in 0..count {
            let sym = symtab.add(i);
            let name = std::ffi::CStr::from_ptr(strtab.add((*sym).st_name as usize) as *const i8);
            if name.to_bytes() == want.as_bytes() {
                return Some(((*sym).st_value as isize + load_bias) as *const c_void);
            }
        }
        None
    }
}

/// Printed for the instruction sources when the *host* cannot execute them.
///
/// The decision is made by the caller, never here: under Hermit the guest's
/// CPUID deliberately reports the features as absent, so a probe that decided
/// for itself would print this on every Hermit run and the fixture would stop
/// covering the very instructions it exists to cover.
const SKIPPED_HOST: &str = "SKIPPED_HOST";

fn main() {
    // `--no-instructions` is passed by the fixture when the *real* host does not
    // advertise RDRAND/RDSEED, so that the probe does not take a #UD on a
    // machine that genuinely lacks them (portable CI runs on such hosts). Both
    // the native control and the Hermit run are given the same flag, so the two
    // source sets always agree.
    let instructions = !std::env::args().any(|arg| arg == "--no-instructions");

    // Stable order: the fixture compares the whole stream byte for byte.
    println!("getrandom {}", getrandom_raw());
    println!("urandom {}", read_device("/dev/urandom"));
    println!("random {}", read_device("/dev/random"));
    println!("at_random {}", at_random());
    println!("getentropy {}", getentropy());
    println!("vdso_getrandom {}", vdso_getrandom());
    println!("arc4random {}", arc4random());
    if instructions {
        // SAFETY: the caller has confirmed the host advertises these. Whether
        // the *guest's* CPUID advertises them is deliberately not consulted,
        // which is the point of the fixture.
        println!("rdrand {}", unsafe { rdrand_unconditional() });
        println!("rdseed {}", unsafe { rdseed_unconditional() });
    } else {
        println!("rdrand {SKIPPED_HOST}");
        println!("rdseed {SKIPPED_HOST}");
    }
}
