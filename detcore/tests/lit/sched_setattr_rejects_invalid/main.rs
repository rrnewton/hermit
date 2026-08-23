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
//! effect must not extend to accepting arguments Linux rejects, nor to
//! rejecting arguments Linux accepts: a guest probing the ABI has to observe
//! the same answer it observes natively.
//!
//! The same assertions run natively (`RUN: %me`) and under Hermit
//! (`hermit-run.lit`), so the two are bracketed against each other rather than
//! against a hard-coded expectation.
//!
//! Two behaviours are deliberately left out, because asserting them here would
//! bake a property of the host kernel's *configuration* into a test that has to
//! agree with whatever kernel it runs on:
//!
//!   * `SCHED_EXT` (policy 7) is accepted by `valid_policy()` only on a kernel
//!     built with CONFIG_SCHED_CLASS_EXT. Hermit accepts it unconditionally --
//!     the call is suppressed, so no host scheduler class is involved, and a
//!     deterministic sandbox must not vary with the host's config. The unit
//!     test `sched_ext_is_a_valid_policy_and_sched_iso_is_not` pins that.
//!   * `SCHED_FLAG_UTIL_CLAMP` on a VER1 buffer is EOPNOTSUPP without
//!     CONFIG_UCLAMP_TASK and success with it. Hermit accepts it as the same
//!     no-op as any other well-formed request. Only the size rule below, which
//!     is pure ABI, is asserted.
//!
//! One divergence is known and deliberate: Linux resolves `pid` against the
//! live process table and answers ESRCH for an unknown one. That is a lookup
//! rather than argument validation, so Hermit does not reproduce it and this
//! test does not probe it.

use std::io::Error;
use std::ptr;

const SCHED_ATTR_SIZE_VER0: u32 = 48;
/// The kernel's own `sizeof(struct sched_attr)` since util-clamp was added in
/// 5.3. It is what the kernel stores back into `uattr->size` when it refuses a
/// request with E2BIG.
const SCHED_ATTR_KERNEL_SIZE: u32 = 56;

const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

/// A buffer large enough to hold an oversized descriptor, so the trailing-byte
/// rule can be probed with real memory behind the declared size.
const BUF: usize = 128;

/// Field offsets in the UAPI `struct sched_attr`.
const OFF_SIZE: usize = 0;
const OFF_POLICY: usize = 4;
const OFF_FLAGS: usize = 8;

/// A `sched_attr` held as raw bytes, so a declared size larger than the base
/// descriptor is expressible and the trailing bytes are addressable.
struct Attr([u8; BUF]);

impl Attr {
    fn new(size: u32, policy: u32, flags: u64) -> Self {
        let mut raw = [0u8; BUF];
        raw[OFF_SIZE..OFF_SIZE + 4].copy_from_slice(&size.to_ne_bytes());
        raw[OFF_POLICY..OFF_POLICY + 4].copy_from_slice(&policy.to_ne_bytes());
        raw[OFF_FLAGS..OFF_FLAGS + 8].copy_from_slice(&flags.to_ne_bytes());
        Self(raw)
    }

    fn size_field(&self) -> u32 {
        u32::from_ne_bytes(self.0[OFF_SIZE..OFF_SIZE + 4].try_into().unwrap())
    }

    fn as_ptr(&mut self) -> *mut libc::c_void {
        self.0.as_mut_ptr().cast()
    }
}

/// Returns `Ok(())` when the call succeeded, otherwise the reported errno.
fn sched_setattr_raw(pid: libc::pid_t, attr: *mut libc::c_void, flags: u32) -> Result<(), i32> {
    // SAFETY: `attr` is either null or a valid pointer to a buffer of BUF bytes
    // living in the caller's frame; the kernel reads at most the size the
    // descriptor itself declares, and every declared size used here is <= BUF.
    let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, pid, attr, flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::last_os_error().raw_os_error().unwrap_or(0))
    }
}

fn call(attr: &mut Attr) -> Result<(), i32> {
    let p = attr.as_ptr();
    sched_setattr_raw(0, p, 0)
}

fn main() {
    // --- the first clause: `if (!uattr || pid < 0 || flags)` -------------
    // All three are EINVAL, and a null pointer is EINVAL rather than EFAULT
    // because it is refused before any access is attempted.
    assert_eq!(
        sched_setattr_raw(0, ptr::null_mut(), 0),
        Err(libc::EINVAL),
        "null sched_attr must be rejected with EINVAL"
    );
    let mut attr = Attr::new(SCHED_ATTR_SIZE_VER0, 0, 0);
    assert_eq!(
        sched_setattr_raw(-1, attr.as_ptr(), 0),
        Err(libc::EINVAL),
        "a negative pid must be rejected with EINVAL"
    );
    assert_eq!(
        sched_setattr_raw(0, attr.as_ptr(), 1),
        Err(libc::EINVAL),
        "a nonzero flags argument must be rejected with EINVAL"
    );

    // --- the size rules in sched_copy_attr -------------------------------
    // `if (!size) size = SCHED_ATTR_SIZE_VER0;` is an explicit ABI
    // compatibility quirk, so a zero size is a well-formed VER0 request.
    let mut zero_size = Attr::new(0, 0, 0);
    assert_eq!(
        call(&mut zero_size),
        Ok(()),
        "size 0 is the kernel's VER0 compatibility quirk, not an error"
    );

    // A buffer too small to hold the base descriptor is E2BIG...
    let mut undersized = Attr::new(1, 0, 0);
    assert_eq!(
        call(&mut undersized),
        Err(libc::E2BIG),
        "undersized sched_attr must be rejected with E2BIG"
    );
    // ...and the refusal is not silent: `err_size` stores the kernel's own
    // struct size back into uattr->size so the guest learns what to send.
    assert_eq!(
        undersized.size_field(),
        SCHED_ATTR_KERNEL_SIZE,
        "E2BIG must store the kernel's sched_attr size back into uattr->size"
    );

    let mut just_under = Attr::new(SCHED_ATTR_SIZE_VER0 - 1, 0, 0);
    assert_eq!(call(&mut just_under), Err(libc::E2BIG), "VER0-1 is E2BIG");
    assert_eq!(
        just_under.size_field(),
        SCHED_ATTR_KERNEL_SIZE,
        "the size store-back applies to every E2BIG exit"
    );

    // A size past one page is E2BIG on the same path. BUF is far smaller than
    // the declared size, but the kernel refuses on the range check before it
    // reads anything past the size field.
    let mut oversized = Attr::new(4097, 0, 0);
    assert_eq!(
        call(&mut oversized),
        Err(libc::E2BIG),
        "a sched_attr larger than one page must be rejected with E2BIG"
    );
    assert_eq!(oversized.size_field(), SCHED_ATTR_KERNEL_SIZE);

    // A size between VER0 and one page is fine, and a short buffer is
    // zero-filled rather than refused.
    for size in [SCHED_ATTR_SIZE_VER0, SCHED_ATTR_KERNEL_SIZE] {
        let mut ok = Attr::new(size, 0, 0);
        assert_eq!(call(&mut ok), Ok(()), "size {} must be accepted", size);
    }

    // --- trailing bytes past the kernel's struct -------------------------
    // copy_struct_from_user requires every byte at or after the kernel's
    // sizeof(struct sched_attr) to be zero; a nonzero one means the guest is
    // sending a field this kernel does not know, which must not be dropped.
    let mut zero_tail = Attr::new(SCHED_ATTR_KERNEL_SIZE + 8, 0, 0);
    assert_eq!(
        call(&mut zero_tail),
        Ok(()),
        "an oversized descriptor with a zero tail must be accepted"
    );

    let mut dirty_tail = Attr::new(SCHED_ATTR_KERNEL_SIZE + 8, 0, 0);
    dirty_tail.0[SCHED_ATTR_KERNEL_SIZE as usize] = 0xff;
    assert_eq!(
        call(&mut dirty_tail),
        Err(libc::E2BIG),
        "a nonzero byte past the kernel's sched_attr must be rejected with E2BIG"
    );
    assert_eq!(
        dirty_tail.size_field(),
        SCHED_ATTR_KERNEL_SIZE,
        "the trailing-byte refusal takes the same store-back path"
    );

    // --- the descriptor's own content ------------------------------------
    // A policy Linux does not define is EINVAL. 99 is not a policy, and neither
    // is 4: SCHED_ISO is reserved and never valid.
    for policy in [4u32, 8, 99] {
        let mut bad = Attr::new(SCHED_ATTR_SIZE_VER0, policy, 0);
        assert_eq!(
            call(&mut bad),
            Err(libc::EINVAL),
            "policy {} must be rejected with EINVAL",
            policy
        );
    }

    // The policy is compared as a signed int, so the sign bit is refused.
    let mut negative = Attr::new(SCHED_ATTR_SIZE_VER0, 0x8000_0000, 0);
    assert_eq!(
        call(&mut negative),
        Err(libc::EINVAL),
        "a policy with the sign bit set must be rejected with EINVAL"
    );

    // SCHED_FLAG_KEEP_POLICY overwrites the policy with SETPARAM_POLICY before
    // valid_policy() would ever see it, so the field is genuinely ignored and a
    // value that is otherwise refused is accepted.
    let mut kept = Attr::new(SCHED_ATTR_SIZE_VER0, 99, SCHED_FLAG_KEEP_POLICY);
    assert_eq!(
        call(&mut kept),
        Ok(()),
        "KEEP_POLICY makes the policy field irrelevant"
    );
    // The signed check still runs first, so KEEP_POLICY cannot hide the sign
    // bit.
    let mut kept_negative = Attr::new(SCHED_ATTR_SIZE_VER0, 0x8000_0000, SCHED_FLAG_KEEP_POLICY);
    assert_eq!(
        call(&mut kept_negative),
        Err(libc::EINVAL),
        "KEEP_POLICY must not hide a negative policy"
    );

    // An undefined sched_flags bit is EINVAL.
    let mut bad_flag = Attr::new(SCHED_ATTR_SIZE_VER0, 0, 0x80);
    assert_eq!(
        call(&mut bad_flag),
        Err(libc::EINVAL),
        "an undefined sched_flags bit must be rejected with EINVAL"
    );

    // Util-clamp lives past the VER0 tail, so asking for it with a VER0 buffer
    // is incoherent. (What a VER1 buffer answers depends on CONFIG_UCLAMP_TASK,
    // so only this direction is asserted -- see the module comment.)
    let mut clamp_too_small = Attr::new(SCHED_ATTR_SIZE_VER0, 0, SCHED_FLAG_UTIL_CLAMP_MIN);
    assert_eq!(
        call(&mut clamp_too_small),
        Err(libc::EINVAL),
        "util-clamp on a VER0 buffer must be rejected with EINVAL"
    );

    // --- and a well-formed request is still accepted ----------------------
    // The checks above narrow what is accepted to what Linux accepts; they do
    // not disable the syscall.
    let mut good = Attr::new(SCHED_ATTR_SIZE_VER0, libc::SCHED_OTHER as u32, 0);
    assert_eq!(
        call(&mut good),
        Ok(()),
        "a well-formed SCHED_OTHER request must be accepted"
    );
}
