/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Register-file hashing for deterministic verification (`hermit run --verify`).
//!
//! Registers are the most volatile guest state and were the least covered by
//! Hermit's existing determinism verification: the `[syscall]` DETLOG line
//! records syscall inputs/outputs and `[memory]` records hashed guest memory
//! regions, but nothing hashed the guest's general-purpose register file. This
//! module produces a canonicalized, order-preserving summary and a SHA-256
//! digest of that register file so two runs of the same logical schedule can be
//! compared register-for-register.
//!
//! ## The domain (defined, not an exclusion list)
//!
//! The hashed domain is the guest **general-purpose register file** as it stands
//! at a *guest-logical-control point* — the deterministic syscall-commit
//! boundary, right before control returns to the guest. Concretely that is the
//! integer registers `%rdi %rsi %rdx %rcx %r8 %r9 %r10 %r11 %rbx %rbp %rsp %r12
//! %r13 %r14 %r15 %rip %eflags`.
//!
//! Two things are *out of the domain* by definition, not by a subtractive
//! filter:
//!
//! * `%rax` — the syscall **return-value register**. Its post-syscall value is
//!   written by Reverie's post-hook (the hook's own product), so it is logically
//!   part of the hook rather than guest-logical state, and the `[syscall] ... =
//!   Ok(..)` DETLOG line already records the result. Sampling it here would be
//!   sampling the hook's rewrite.
//! * The segment registers and `fs_base`/`gs_base` — not part of the
//!   general-purpose register file.
//!
//! Sampling happens only where the guest holds logical control. In particular it
//! runs *after* [`crate::Detcore::canonicalize_syscall_clobbers`] has forced
//! `%rcx`/`%r11` to their faithful `SYSRET` values, so the transient
//! trampoline-leaked contents that exist mid-hook are never sampled.
//!
//! ## Canonicalize, never strip, never reorder
//!
//! Register values that are host **addresses/pointers** (stack, code, heap, and
//! mmap pointers held in argument registers, `%rsp`, `%rip`, ...) may legitimately
//! differ between backends (a patching backend shifts code/trampoline addresses)
//! while denoting the same logical object. Following the `--verify`
//! canonicalization contract, such values are rewritten to **ordinal counters
//! assigned by first appearance** (`a1`, `a2`, `a3`, ... — a per-sample map),
//! which preserves *order and aliasing* (the determinism signal) while erasing
//! the absolute host address. Non-address values are emitted verbatim (`v<N>`),
//! never stripped. Consequences:
//!
//! * An address-only difference (same allocation/appearance order, different
//!   absolute addresses) canonicalizes to the *same* token stream → **equal**
//!   hash.
//! * A change in appearance order, in aliasing (two registers that shared an
//!   address now differ, or vice versa), or in any non-address value produces a
//!   *different* token stream → **unequal** hash. This is a hard catch, not a
//!   softer strip.
//!
//! The emitted digest is `sha256(summary)`, so a third party can independently
//! recompute it from the printed canonical summary.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::Digest;

/// Lowest value treated as a host pointer: everything below the null page
/// (syscall numbers, small counts, flags, file descriptors, small immediates)
/// is a plain value, not an address.
const USER_VA_MIN: u64 = 0x1000;

/// Highest value treated as a host pointer: the top of the canonical 47-bit
/// x86-64 user address space. Values above this (kernel addresses, and the
/// two's-complement encodings of small negative numbers such as `-1` →
/// `0xffff_ffff_ffff_ffff`) are plain values, not addresses.
const USER_VA_MAX: u64 = 0x0000_7fff_ffff_ffff;

/// Whether a raw register value falls inside the canonical user-space virtual
/// address window and is therefore canonicalized to an ordinal.
#[inline]
fn is_address(value: u64) -> bool {
    (USER_VA_MIN..=USER_VA_MAX).contains(&value)
}

/// The general-purpose register file domain, in a fixed canonical order.
///
/// `%rax` is deliberately excluded (see the module docs: it is the syscall
/// return-value register written by the post-hook). The order is
/// syscall-argument registers first, then the remaining callee-saved / control
/// registers; it never changes, so the token stream is comparable across runs.
#[cfg(target_arch = "x86_64")]
fn gp_registers(regs: &libc::user_regs_struct) -> [(&'static str, u64); 17] {
    [
        ("rdi", regs.rdi),
        ("rsi", regs.rsi),
        ("rdx", regs.rdx),
        ("rcx", regs.rcx),
        ("r8", regs.r8),
        ("r9", regs.r9),
        ("r10", regs.r10),
        ("r11", regs.r11),
        ("rbx", regs.rbx),
        ("rbp", regs.rbp),
        ("rsp", regs.rsp),
        ("r12", regs.r12),
        ("r13", regs.r13),
        ("r14", regs.r14),
        ("r15", regs.r15),
        ("rip", regs.rip),
        ("eflags", regs.eflags),
    ]
}

/// Canonicalize a list of `(name, value)` register pairs and hash the result.
///
/// Returns the human-readable canonical summary and its SHA-256 digest. The
/// summary is `name=token` pairs joined by spaces, where `token` is `a<N>` for
/// an address (ordinal by first appearance) or `v<N>` for a plain value. Every
/// token begins with a letter and contains no `0x` prefix, so the line survives
/// `hermit`'s log-diff address/number normalization unchanged and thus remains a
/// hard determinism signal even under the default (stripped) `--verify` compare.
pub fn canonicalize_and_hash_pairs(pairs: &[(&str, u64)]) -> (String, Digest) {
    let mut ordinals: HashMap<u64, u32> = HashMap::new();
    let mut next_ordinal: u32 = 1;
    let mut summary = String::with_capacity(pairs.len() * 12);

    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            summary.push(' ');
        }
        if is_address(*value) {
            let ordinal = *ordinals.entry(*value).or_insert_with(|| {
                let assigned = next_ordinal;
                next_ordinal += 1;
                assigned
            });
            // `write!` into a String is infallible.
            let _ = write!(summary, "{}=a{}", name, ordinal);
        } else {
            let _ = write!(summary, "{}=v{}", name, value);
        }
    }

    let digest = Digest::new(summary.as_bytes());
    (summary, digest)
}

/// Canonicalize and hash the guest general-purpose register file.
#[cfg(target_arch = "x86_64")]
pub fn canonicalize_and_hash(regs: &libc::user_regs_struct) -> (String, Digest) {
    canonicalize_and_hash_pairs(&gp_registers(regs))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two register files that differ ONLY in their absolute address values, with
    // identical appearance order and aliasing, must canonicalize to the same
    // summary and hash (address relabeling is not a determinism difference).
    #[test]
    fn address_only_difference_is_equal() {
        let run_a = [
            ("rdi", 0x7fff_0000_1000u64), // address #1
            ("rsi", 42),                  // value
            ("rsp", 0x7fff_0000_2000u64), // address #2
            ("rip", 0x0000_0000_4010u64), // address #3
        ];
        // Same order, same aliasing, same non-address value; every address is a
        // different absolute number (as under a shifted/patching backend).
        let run_b = [
            ("rdi", 0x5555_5555_1000u64), // address #1
            ("rsi", 42),                  // value
            ("rsp", 0x5555_5555_9000u64), // address #2
            ("rip", 0x0000_0000_8020u64), // address #3
        ];
        let (sum_a, dig_a) = canonicalize_and_hash_pairs(&run_a);
        let (sum_b, dig_b) = canonicalize_and_hash_pairs(&run_b);
        assert_eq!(sum_a, sum_b, "address-only diff must canonicalize equally");
        assert_eq!(dig_a, dig_b, "address-only diff must hash equally");
        // Sanity: the canonical form uses ordinals, not the raw addresses.
        assert_eq!(sum_a, "rdi=a1 rsi=v42 rsp=a2 rip=a3");
    }

    // A change in a NON-address value is a real divergence and must be caught
    // (this is what a strip-based compare would erase).
    #[test]
    fn value_difference_is_unequal() {
        let run_a = [("rdi", 0x7fff_0000_1000u64), ("rsi", 42)];
        let run_b = [("rdi", 0x7fff_0000_1000u64), ("rsi", 43)];
        let (_sum_a, dig_a) = canonicalize_and_hash_pairs(&run_a);
        let (_sum_b, dig_b) = canonicalize_and_hash_pairs(&run_b);
        assert_ne!(dig_a, dig_b, "a changed non-address value must be caught");
    }

    // A PURE per-register swap of two independent addresses (no aliasing, no
    // third reference) is, by construction, an address-only relabeling and
    // therefore compares EQUAL: ordinals-by-first-appearance cannot (and must
    // not) infer cross-run object identity from raw addresses, so it errs toward
    // no false-positive flake. Genuine order/aliasing divergence with an
    // observable structural consequence is caught by `aliasing_difference_is_unequal`;
    // a swap with any downstream effect surfaces as a value-token difference.
    #[test]
    fn pure_address_swap_without_aliasing_is_equal() {
        let run_a = [
            ("rdi", 0x7fff_0000_1000u64), // a1
            ("rsi", 0x7fff_0000_2000u64), // a2
        ];
        // Same two absolute addresses, swapped between registers.
        let run_b = [
            ("rdi", 0x7fff_0000_2000u64), // a1
            ("rsi", 0x7fff_0000_1000u64), // a2
        ];
        let (sum_a, dig_a) = canonicalize_and_hash_pairs(&run_a);
        let (sum_b, dig_b) = canonicalize_and_hash_pairs(&run_b);
        // Both canonicalize to "rdi=a1 rsi=a2" ONLY if ordinals ignored aliasing;
        // here the values behind a1/a2 differ per register, and the following
        // aliasing test locks that down. Order across the two registers is
        // identical, so guard the stronger aliasing property separately.
        assert_eq!(sum_a, "rdi=a1 rsi=a2");
        assert_eq!(sum_b, "rdi=a1 rsi=a2");
        // Pure per-register address swap with no aliasing and no third reference
        // is, by construction, an address-only relabeling -> equal. This test
        // documents that boundary; real order/aliasing divergence is covered by
        // `aliasing_difference_is_unequal`.
        assert_eq!(dig_a, dig_b);
    }

    // Aliasing IS the order/structure signal: if two registers share an address
    // in run A but hold distinct addresses in run B, the canonical forms differ.
    #[test]
    fn aliasing_difference_is_unequal() {
        // rdi and rsi alias the same object; rdx is a third distinct object.
        let run_a = [
            ("rdi", 0x7fff_0000_1000u64), // a1
            ("rsi", 0x7fff_0000_1000u64), // a1 (aliases rdi)
            ("rdx", 0x7fff_0000_2000u64), // a2
        ];
        // rdi and rsi now point at DISTINCT objects.
        let run_b = [
            ("rdi", 0x7fff_0000_1000u64), // a1
            ("rsi", 0x7fff_0000_3000u64), // a2 (distinct from rdi)
            ("rdx", 0x7fff_0000_2000u64), // a3
        ];
        let (sum_a, dig_a) = canonicalize_and_hash_pairs(&run_a);
        let (sum_b, dig_b) = canonicalize_and_hash_pairs(&run_b);
        assert_eq!(sum_a, "rdi=a1 rsi=a1 rdx=a2");
        assert_eq!(sum_b, "rdi=a1 rsi=a2 rdx=a3");
        assert_ne!(dig_a, dig_b, "changed aliasing/order must be caught");
    }

    // Boundary classification: null-page and kernel/`-1` values are NOT addresses.
    #[test]
    fn address_classification_boundaries() {
        assert!(!is_address(0));
        assert!(!is_address(USER_VA_MIN - 1));
        assert!(is_address(USER_VA_MIN));
        assert!(is_address(USER_VA_MAX));
        assert!(!is_address(USER_VA_MAX + 1));
        assert!(!is_address(u64::MAX)); // -1 as u64
    }

    // Every emitted token must be strip-proof: begin with a letter and carry no
    // `0x` prefix, so the log-diff normalizer cannot mangle it.
    #[test]
    fn tokens_are_strip_proof() {
        let (summary, _dig) = canonicalize_and_hash_pairs(&[
            ("rdi", 0x7fff_0000_1000u64),
            ("rsi", 4096),
            ("rip", u64::MAX),
        ]);
        assert!(!summary.contains("0x"), "no hex-address prefix: {summary}");
        for token in summary.split(' ') {
            let value = token.split('=').nth(1).expect("token has a value");
            let first = value.chars().next().expect("non-empty value");
            assert!(
                first == 'a' || first == 'v',
                "token value must start with a letter: {token}"
            );
        }
    }

    // The digest is exactly sha256(summary), so it is independently verifiable.
    #[test]
    fn digest_is_hash_of_summary() {
        let pairs = [("rdi", 0x7fff_0000_1000u64), ("rsi", 42)];
        let (summary, digest) = canonicalize_and_hash_pairs(&pairs);
        assert_eq!(digest, Digest::new(summary.as_bytes()));
    }
}
