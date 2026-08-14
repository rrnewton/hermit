/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Contract fixture: every clock source resolves from ONE continuous virtual clock.
//!
//! WHY THIS IS A FIXTURE AND NOT A SWEEP. A sweep reports the state of the world
//! on the day it ran, and its finding decays silently as the code moves. These
//! assertions fail the build instead. That matters most for the two failure
//! shapes below, because neither one announces itself:
//!
//! 1. A NEW vDSO SYMBOL. The vDSO is the path that gets missed: it services
//!    calls entirely in userspace, so a determinism strategy built on syscall
//!    interception never sees it, while every *intercepted* call still looks
//!    perfect. reverie neutralizes the vDSO by overwriting each symbol with a
//!    stub that forces the real syscall -- but only for the symbols on its
//!    fixed list, and its patch loop silently skips anything not on it. So a
//!    kernel that grows a new vDSO entry point quietly reopens a hole.
//!    `vdso_exports_are_all_accounted_for` enumerates what the host ACTUALLY
//!    exports and refuses to pass on a symbol nobody has classified.
//!
//! 2. A FROZEN OR ROUNDED CLOCK. Equality across two runs is easy to satisfy
//!    the wrong way: a clock that always answers the same constant is perfectly
//!    "deterministic" and completely useless, and so is one rounded so coarse
//!    that nothing measurable happens between two reads. Every check here is
//!    therefore two-sided -- identical across runs AND strictly advancing at
//!    fine granularity within a run. A constant-returning "fix" fails
//!    `advances_fine_grained`, by construction.
//!
//! WHAT BINDS THESE CHECKS TO WHAT THEY CLAIM. The vDSO test does not read a
//! configuration flag, a feature bit, or reverie's symbol list; it reads the
//! bytes of the vDSO mapping as they exist inside the running guest and asks
//! whether they are a syscall stub. That is the mechanism itself rather than
//! something correlated with it.

use std::mem::MaybeUninit;

// ---------------------------------------------------------------------------
// vDSO: what the host exports vs. what is actually neutralized in the guest
// ---------------------------------------------------------------------------

/// x86_64 patch stub reverie writes over a vDSO entry point:
/// `mov $SYS_x,%eax; syscall; ret` -- i.e. `b8 <imm32> 0f 05 c3`.
fn is_syscall_stub(bytes: &[u8; 8]) -> bool {
    bytes[0] == 0xb8 && bytes[5] == 0x0f && bytes[6] == 0x05 && bytes[7] == 0xc3
}

/// Symbols that MUST be neutralized. Each one answers a clock query without
/// entering the kernel when left alone, so an unpatched entry here means the
/// guest is reading the host clock while every intercepted syscall still looks
/// correct.
const REQUIRED_PATCHED: &[&str] = &[
    "__vdso_clock_gettime",
    "__vdso_gettimeofday",
    "__vdso_time",
    "__vdso_clock_getres",
    // Not a clock, but a per-run varying host property reached the same way.
    "__vdso_getcpu",
];

/// Symbols knowingly left unpatched, each with the reason. This list exists so
/// that an unpatched symbol is a RECORDED DECISION rather than an accident --
/// and so that anything NOT on either list stops the build.
const KNOWN_UNPATCHED: &[(&str, &str)] = &[
    (
        "__vdso_getrandom",
        "GAP, not a clock: Linux 6.11+ vgetrandom generates entropy in userspace \
         from per-thread state, so no syscall is issued and syscall interception \
         cannot determinize it. Latent rather than active wherever libc is older \
         than glibc 2.41 (such libcs still call the syscall), but live for any \
         guest that resolves the symbol itself. Tracked separately; do NOT move \
         this entry to REQUIRED_PATCHED without a patch that actually lands.",
    ),
    (
        "__vdso_sgx_enter_enclave",
        "Enclave entry trampoline. Reports no time and no entropy, so leaving it \
         intact does not expose a nondeterministic value.",
    ),
];

struct VdsoSymbol {
    name: String,
    first8: [u8; 8],
}

/// Walk the live vDSO's dynamic symbol table via `AT_SYSINFO_EHDR`.
///
/// Only `__vdso_*` names are returned. The kernel also exports unprefixed
/// aliases (`clock_gettime`, `time`, ...) at the SAME addresses, so reporting
/// both would double-count one entry point rather than cover more ground.
fn vdso_symbols() -> Vec<VdsoSymbol> {
    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: i64 = 0;
    const DT_HASH: i64 = 4;
    const DT_STRTAB: i64 = 5;
    const DT_SYMTAB: i64 = 6;

    #[repr(C)]
    struct Dyn {
        tag: i64,
        val: u64,
    }

    let mut out = Vec::new();
    // SAFETY: the vDSO is a well-formed ELF image mapped by the kernel for the
    // lifetime of the process. Every offset used below is taken from that
    // image's own headers, and nothing is written.
    unsafe {
        let base = libc::getauxval(libc::AT_SYSINFO_EHDR) as usize;
        if base == 0 {
            return out;
        }
        let ehdr = base as *const libc::Elf64_Ehdr;
        let phoff = (*ehdr).e_phoff as usize;
        let phnum = (*ehdr).e_phnum as usize;

        let mut dynamic: *const Dyn = std::ptr::null();
        // Difference between mapped addresses and the link-time vaddrs.
        let mut slide: usize = 0;
        for i in 0..phnum {
            let ph = (base + phoff + i * std::mem::size_of::<libc::Elf64_Phdr>())
                as *const libc::Elf64_Phdr;
            match (*ph).p_type {
                PT_DYNAMIC => dynamic = (base + (*ph).p_offset as usize) as *const Dyn,
                PT_LOAD => {
                    slide = base + (*ph).p_offset as usize - (*ph).p_vaddr as usize;
                }
                _ => {}
            }
        }
        if dynamic.is_null() {
            return out;
        }

        let mut strtab: *const u8 = std::ptr::null();
        let mut symtab: *const libc::Elf64_Sym = std::ptr::null();
        let mut nsyms: usize = 0;
        let mut d = dynamic;
        while (*d).tag != DT_NULL {
            match (*d).tag {
                DT_STRTAB => strtab = (slide + (*d).val as usize) as *const u8,
                DT_SYMTAB => symtab = (slide + (*d).val as usize) as *const libc::Elf64_Sym,
                DT_HASH => {
                    // Elf64 DT_HASH is a pair of u32s (nbucket, nchain) followed
                    // by the tables; nchain is the symbol count.
                    let h = (slide + (*d).val as usize) as *const u32;
                    nsyms = *h.add(1) as usize;
                }
                _ => {}
            }
            d = d.add(1);
        }
        if strtab.is_null() || symtab.is_null() || nsyms == 0 {
            return out;
        }

        for i in 0..nsyms {
            let sym = &*symtab.add(i);
            if sym.st_name == 0 || sym.st_value == 0 {
                continue;
            }
            let namep = strtab.add(sym.st_name as usize) as *const libc::c_char;
            let name = std::ffi::CStr::from_ptr(namep)
                .to_string_lossy()
                .into_owned();
            if !name.starts_with("__vdso_") {
                continue;
            }
            let code = (slide + sym.st_value as usize) as *const u8;
            let mut first8 = [0u8; 8];
            std::ptr::copy_nonoverlapping(code, first8.as_mut_ptr(), 8);
            out.push(VdsoSymbol { name, first8 });
        }
    }
    out
}

/// Runs INSIDE the guest, under the tool.
fn check_vdso_exports() {
    let syms = vdso_symbols();

    // Fail closed. If enumeration silently returned nothing, every assertion
    // below would hold vacuously and this fixture would report success while
    // checking exactly nothing -- the failure mode it exists to prevent.
    assert!(
        !syms.is_empty(),
        "could not enumerate any __vdso_* symbol. This is a FAILURE, not a skip: \
         an empty enumeration makes every check below vacuous. Either the guest \
         has no vDSO (then this fixture needs an explicit guard) or the walk is \
         broken (then it needs fixing)."
    );

    for s in &syms {
        println!(
            "vdso {:<26} {:02x?} {}",
            s.name,
            s.first8,
            if is_syscall_stub(&s.first8) {
                "patched"
            } else {
                "unpatched"
            }
        );
    }

    // (a) Everything that must be neutralized is present AND neutralized.
    for want in REQUIRED_PATCHED {
        let sym = syms.iter().find(|s| &s.name == want);
        match sym {
            None => panic!(
                "{want} is not exported by this host's vDSO. Either the guest lost the \
                 vDSO mapping, or this entry point moved/renamed -- in which case the \
                 REPLACEMENT must be patched and listed here. Silently dropping it \
                 would leave that call reading the host clock."
            ),
            Some(s) => assert!(
                is_syscall_stub(&s.first8),
                "{want} is NOT patched (first bytes {:02x?}). It is executing real \
                 kernel code in userspace, so it answers from host state and issues \
                 NO syscall for the tool to intercept. Nothing downstream can notice: \
                 the guest simply receives a host value on this path.",
                s.first8
            ),
        }
    }

    // (b) Nothing is unclassified. This is the clause that catches a kernel
    //     growing a new vDSO entry point: an unknown symbol is a decision
    //     somebody has to make, not something to discover in production.
    for s in &syms {
        if REQUIRED_PATCHED.contains(&s.name.as_str()) {
            continue;
        }
        if let Some((_, reason)) = KNOWN_UNPATCHED.iter().find(|(n, _)| *n == s.name) {
            // Pin the recorded reason too: if a listed symbol starts arriving
            // patched, the note explaining why it isn't has gone stale.
            if is_syscall_stub(&s.first8) {
                println!(
                    "note: {} is now patched though listed as unpatched ({reason}); \
                     move it to REQUIRED_PATCHED",
                    s.name
                );
            }
            continue;
        }
        panic!(
            "UNCLASSIFIED vDSO symbol {} (first bytes {:02x?}, currently {}).\n\
             This host exports a vDSO entry point that nobody has ruled on. Decide \
             which it is and say so in code:\n  * it can return a clock value, a \
             random value, or any other per-run varying host property -> it must be \
             patched to force a syscall; add it to REQUIRED_PATCHED and to reverie's \
             VDSO_SYMBOLS.\n  * it cannot -> add it to KNOWN_UNPATCHED with the reason.\n\
             Do not delete this assertion to get green: an unpatched entry point \
             services the guest entirely in userspace, so nothing downstream will \
             notice the value came from the host.",
            s.name,
            s.first8,
            if is_syscall_stub(&s.first8) {
                "patched"
            } else {
                "unpatched"
            }
        );
    }
}

#[test]
fn vdso_exports_are_all_accounted_for() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    reverie_ptrace::testing::check_fn_with_config::<detcore::Detcore, _>(
        check_vdso_exports,
        config,
        true,
    );
}

// ---------------------------------------------------------------------------
// The clock family: one continuous virtual clock behind every source
// ---------------------------------------------------------------------------

/// Every clockid a guest can ask for. Detcore answers them from a single
/// logical clock rather than dispatching per id, which is exactly the property
/// pinned here -- enumerated in full so that adding a clockid to the guest
/// surface without answering it deterministically shows up as a failure.
const CLOCKS: &[(&str, libc::clockid_t)] = &[
    ("CLOCK_REALTIME", libc::CLOCK_REALTIME),
    ("CLOCK_MONOTONIC", libc::CLOCK_MONOTONIC),
    ("CLOCK_BOOTTIME", libc::CLOCK_BOOTTIME),
    ("CLOCK_PROCESS_CPUTIME_ID", libc::CLOCK_PROCESS_CPUTIME_ID),
    ("CLOCK_THREAD_CPUTIME_ID", libc::CLOCK_THREAD_CPUTIME_ID),
    ("CLOCK_MONOTONIC_RAW", libc::CLOCK_MONOTONIC_RAW),
    ("CLOCK_REALTIME_COARSE", libc::CLOCK_REALTIME_COARSE),
    ("CLOCK_MONOTONIC_COARSE", libc::CLOCK_MONOTONIC_COARSE),
];

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// Read through libc, which takes the vDSO fast path when one exists.
fn read_via_vdso(id: libc::clockid_t) -> Option<i128> {
    let mut tp: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    // SAFETY: `tp` is a valid, correctly typed, writable destination.
    let rc = unsafe { libc::clock_gettime(id, tp.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: initialized by the successful call above.
    let tp = unsafe { tp.assume_init() };
    Some(tp.tv_sec as i128 * NANOS_PER_SEC + tp.tv_nsec as i128)
}

/// Force the syscall, bypassing any vDSO fast path.
fn read_via_syscall(id: libc::clockid_t) -> Option<i128> {
    let mut tp: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    // SAFETY: as above; `SYS_clock_gettime` writes one `timespec`.
    let rc = unsafe { libc::syscall(libc::SYS_clock_gettime, id, tp.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: initialized by the successful call above.
    let tp = unsafe { tp.assume_init() };
    Some(tp.tv_sec as i128 * NANOS_PER_SEC + tp.tv_nsec as i128)
}

/// The core two-sided property, applied to one clockid.
///
/// Both halves matter. Equality across runs alone is satisfied by a constant,
/// and advancement alone is satisfied by the host clock; only together do they
/// say "virtual AND running".
fn check_one_clock(name: &str, id: libc::clockid_t) {
    let vdso_first = read_via_vdso(id);
    let sys_first = read_via_syscall(id);

    // A clockid the kernel rejects is reported, not silently skipped -- an
    // unsupported clock and a broken clock must not look the same here.
    let (v1, s1) = match (vdso_first, sys_first) {
        (Some(v), Some(s)) => (v, s),
        _ => {
            println!("{name}: UNSUPPORTED (vdso={vdso_first:?} syscall={sys_first:?})");
            return;
        }
    };

    let v2 = read_via_vdso(id).expect("second vdso read failed after the first succeeded");
    let s2 = read_via_syscall(id).expect("second syscall read failed after the first succeeded");

    // The four reads were issued in the order v1, s1, v2, s2. If both paths
    // resolve from the SAME clock, that is the order the values must be in.
    // If the vDSO path were still reading the host clock, these two families
    // would sit on unrelated timelines and the interleaving would break.
    assert!(
        v1 <= s1 && s1 <= v2 && v2 <= s2,
        "{name}: vDSO and syscall reads are not on one timeline \
         (v1={v1} s1={s1} v2={v2} s2={s2}). Interleaved reads from a single clock \
         must come back in issue order; these did not, so one path is resolving \
         from somewhere else."
    );

    // ADVANCES: rejects a frozen clock, the shape that passes an equality-only
    // determinism check while carrying no information at all.
    let delta = s2 - v1;
    assert!(
        delta > 0,
        "{name}: clock did NOT advance across four reads (v1={v1} s2={s2}). A clock \
         that always answers the same value is deterministic and useless; equality \
         across runs must never be bought by freezing it."
    );

    // FINE-GRAINED: rejects the other cheap way to be stable -- rounding the
    // clock so coarsely that ordinary work fits inside one tick.
    assert!(
        delta < NANOS_PER_SEC,
        "{name}: four consecutive reads spanned {delta} ns (>= 1s). That is far too \
         coarse to order events within a run and suggests the clock is rounded or \
         quantized rather than continuous."
    );

    println!("{name}: v1={v1} s1={s1} v2={v2} s2={s2} delta={delta}");
}

/// Runs INSIDE the guest.
fn check_clock_family() {
    for (name, id) in CLOCKS {
        check_one_clock(name, *id);
    }

    // gettimeofday: same clock, microsecond struct.
    let tod = |()| -> i128 {
        let mut tv: MaybeUninit<libc::timeval> = MaybeUninit::uninit();
        // SAFETY: valid writable destination; NULL timezone is allowed.
        assert_eq!(
            unsafe { libc::gettimeofday(tv.as_mut_ptr(), std::ptr::null_mut()) },
            0
        );
        // SAFETY: initialized by the successful call above.
        let tv = unsafe { tv.assume_init() };
        tv.tv_sec as i128 * NANOS_PER_SEC + tv.tv_usec as i128 * 1_000
    };
    let g1 = tod(());
    let g2 = tod(());
    assert!(
        g2 >= g1,
        "gettimeofday went backwards: {g1} then {g2}. It must resolve from the same \
         monotonically advancing virtual clock as clock_gettime."
    );
    println!("gettimeofday: t1={g1} t2={g2}");

    // time(2): one-second granularity by definition, so it is checked for
    // CONSISTENCY with the same clock rather than for advancement. Asserting
    // that it advances would be asserting something Linux does not promise.
    // SAFETY: NULL `tloc` is valid; the result is returned.
    let secs = unsafe { libc::time(std::ptr::null_mut()) } as i128;
    let realtime = read_via_syscall(libc::CLOCK_REALTIME).expect("CLOCK_REALTIME must be readable");
    assert_eq!(
        secs,
        realtime / NANOS_PER_SEC,
        "time() and CLOCK_REALTIME disagree ({secs} vs {}). Both must resolve from \
         the one virtual clock; a mismatch means a second time source has appeared.",
        realtime / NANOS_PER_SEC
    );
    println!("time(): {secs}");

    // times(2): elapsed ticks derived from the logical clock, plus per-process
    // CPU accounting. Included because it is a distinct entry point that
    // returns a host-derived value when left unhandled.
    let mut tms: MaybeUninit<libc::tms> = MaybeUninit::uninit();
    // SAFETY: valid writable destination.
    let ticks = unsafe { libc::times(tms.as_mut_ptr()) };
    assert!(
        ticks != -1i64 as libc::clock_t,
        "times() failed; it must answer from the logical clock."
    );
    // SAFETY: initialized by the successful call above.
    let tms = unsafe { tms.assume_init() };
    println!(
        "times(): ret={ticks} utime={} stime={} cutime={} cstime={}",
        tms.tms_utime, tms.tms_stime, tms.tms_cutime, tms.tms_cstime
    );

    // clock_getres: pinned only for success, since the reported resolution is a
    // fixed constant whose exact value already has its own test.
    let mut res: MaybeUninit<libc::timespec> = MaybeUninit::uninit();
    // SAFETY: valid writable destination.
    assert_eq!(
        unsafe { libc::clock_getres(libc::CLOCK_MONOTONIC, res.as_mut_ptr()) },
        0
    );
}

// Deliberately NOT the "all" variant set. The "bottom" configuration sets
// `virtualize_time: false`, so the guest reads the HOST clock -- and the host's
// CLOCK_*_COARSE really is quantized to a timer tick, which makes four
// consecutive reads legitimately return one value. Asserting fine-grained
// advancement there would be asserting something Linux does not provide, and
// the failure would say nothing about Detcore. `bottom` is not dropped for
// convenience: it is picked up below as the negative control that proves these
// checks can tell virtual time from host time at all.
mod clock_family_is_deterministic_and_continuous {
    detcore_testutils::basic_det_test!(
        super::check_clock_family,
        |cfg: &detcore::Config| cfg.virtualize_time,
        "default",
        "middle",
        "top"
    );
}

// ---------------------------------------------------------------------------
// One clock behind every clockid -- asserted, and bracketed both ways
// ---------------------------------------------------------------------------

/// Largest spread tolerated between two clockids read microseconds apart.
/// Generous on purpose: the point is to separate "the same clock" from
/// "different clocks", and the host's clockids sit whole decades apart
/// (CLOCK_MONOTONIC counts uptime while CLOCK_REALTIME counts from 1970), so
/// no plausible tightening changes any verdict.
const SAME_CLOCK_TOLERANCE_NS: i128 = NANOS_PER_SEC;

fn spread_across_clockids() -> Option<(i128, i128)> {
    let mut lo = i128::MAX;
    let mut hi = i128::MIN;
    for (_, id) in CLOCKS {
        if let Some(v) = read_via_syscall(*id) {
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if lo == i128::MAX {
        None
    } else {
        Some((lo, hi))
    }
}

/// Runs INSIDE the guest, with virtualization ON.
fn check_one_clock_behind_every_clockid() {
    let (lo, hi) = spread_across_clockids().expect("no clockid was readable");
    println!("virtualized spread: lo={lo} hi={hi} spread={}", hi - lo);
    assert!(
        hi - lo < SAME_CLOCK_TOLERANCE_NS,
        "clockids span {} ns, so they are NOT resolving from one clock. Detcore \
         answers every clockid from a single logical clock; a large spread means \
         some clockid has acquired its own time source -- which is where a host \
         value leaks back in.",
        hi - lo
    );
}

#[test]
fn all_clockids_resolve_from_one_virtual_clock() {
    let config = detcore::Config {
        virtualize_time: true,
        ..Default::default()
    };
    reverie_ptrace::testing::check_fn_with_config::<detcore::Detcore, _>(
        check_one_clock_behind_every_clockid,
        config,
        true,
    );
}

/// Runs INSIDE the guest, with virtualization OFF.
fn check_clockids_are_separate_without_virtualization() {
    let (lo, hi) = spread_across_clockids().expect("no clockid was readable");
    println!("host spread: lo={lo} hi={hi} spread={}", hi - lo);
    assert!(
        hi - lo > SAME_CLOCK_TOLERANCE_NS,
        "with virtualize_time OFF the guest should be reading the host's SEPARATE \
         clocks (CLOCK_MONOTONIC counts uptime, CLOCK_REALTIME counts from 1970), \
         yet they span only {} ns. Either virtualization is on when it should not \
         be, or this bound no longer discriminates -- and if it cannot tell the two \
         regimes apart here, then it is not really testing anything in \
         `all_clockids_resolve_from_one_virtual_clock` either.",
        hi - lo
    );
}

/// The negative half of the bracket.
///
/// Without it, `all_clockids_resolve_from_one_virtual_clock` would keep passing
/// even if `SAME_CLOCK_TOLERANCE_NS` were widened to something meaningless, or
/// if every clockid started returning a constant. This test fails in exactly
/// those cases, so the pair together shows the bound separates the two regimes
/// rather than merely being satisfiable.
#[test]
fn clockids_are_separate_host_timelines_without_virtualization() {
    let config = detcore::Config {
        virtualize_time: false,
        ..Default::default()
    };
    reverie_ptrace::testing::check_fn_with_config::<detcore::Detcore, _>(
        check_clockids_are_separate_without_virtualization,
        config,
        true,
    );
}

// ---------------------------------------------------------------------------
// An interval, and a branch taken on it
// ---------------------------------------------------------------------------

/// Measuring `t2 - t1` and BRANCHING on the result is a strictly stronger
/// demand than printing a timestamp. A single timestamp only has to be equal
/// across runs; an interval that steers control flow has to be equal AND
/// meaningful, so the guest's behaviour -- not merely its output -- depends on
/// the clock being both reproducible and real.
fn check_interval_branch() {
    let t1 = read_via_syscall(libc::CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC must be readable");

    // Deliberately data-dependent work, kept away from the clock itself so the
    // interval measures something other than the two reads.
    let mut acc: u64 = 0;
    for i in 0..20_000u64 {
        acc = acc.wrapping_add(i.wrapping_mul(2_654_435_761));
        if acc.is_multiple_of(7) {
            acc = acc.rotate_left(3);
        }
    }

    let t2 = read_via_syscall(libc::CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC must be readable");
    let delta = t2 - t1;

    assert!(
        delta > 0,
        "interval measured as {delta} ns across real work. A frozen clock makes every \
         duration zero, so every timing-dependent branch in every guest collapses to \
         one side -- reproducibly, and wrongly."
    );
    assert!(
        delta < NANOS_PER_SEC,
        "interval of {delta} ns (>= 1s) for a short compute loop: the clock is \
         advancing far too coarsely to time anything inside a run."
    );

    // The branch. Which side is taken is pinned by the double-run comparison of
    // this printed line, so no threshold has to be hardcoded here -- the
    // fixture notices a change in timing behaviour without pretending to know
    // the right answer in advance.
    let bucket = match delta {
        d if d < 1_000 => "sub-microsecond",
        d if d < 1_000_000 => "sub-millisecond",
        _ => "sub-second",
    };
    let branched = if delta % 2 == 0 { acc } else { acc ^ 0xffff };

    println!("interval: delta={delta} bucket={bucket} branched={branched}");
}

mod interval_measurement_branches_deterministically {
    detcore_testutils::basic_det_test!(
        super::check_interval_branch,
        |cfg: &detcore::Config| cfg.virtualize_time,
        "all"
    );
}
