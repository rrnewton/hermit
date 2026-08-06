/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! RDRAND/RDSEED determinization by instruction rewriting.
//!
//! # Why this exists
//!
//! Detcore's CPUID table (`detcore::cpuid`) reports the `RDRAND` and `RDSEED`
//! feature bits as *absent*. That steers well-behaved software onto a
//! deterministic fallback such as `getrandom(2)`, which Detcore determinizes.
//! It does nothing at all to a guest that issues the instruction without
//! consulting CPUID — hand-written assembly, a binary compiled with `-mrdrnd`
//! for a known target, a JIT emitting the opcode, or a crypto library that
//! probes-or-just-uses. Such a guest receives **raw hardware entropy** and
//! diverges between runs while every syscall succeeds. Masking the feature bit
//! is concealment, not determinization.
//!
//! # Mechanism
//!
//! x86-64 has no user-space fault control for `RDRAND` (unlike `CPUID`, which
//! has CPUID faulting, and `RDTSC`, which has `CR4.TSD`). The only way to trap
//! it outside of hardware virtualization is to not let it execute. So Detcore
//! rewrites each site:
//!
//! 1. Every file-backed executable mapping of the guest is located through
//!    `/proc/<pid>/maps` and its ELF image is linearly disassembled to recover
//!    real instruction boundaries (a raw byte-pattern search is not safe: the
//!    three-byte `0f c7 f0..ff` pattern occurs by chance roughly once per
//!    megabyte inside larger instructions and in embedded data).
//! 2. Each recovered `RDRAND`/`RDSEED` site is re-checked against an
//!    independent hand-written encoding recognizer ([`recognize`]) applied to
//!    the bytes actually present *in guest memory*. Both recognizers must agree
//!    on kind, destination register, operand width, and length before anything
//!    is written. A disagreement means the mapping is not what we think it is,
//!    and the site is refused rather than patched.
//! 3. The site is overwritten with `ud2` plus `nop` padding.
//! 4. When the guest executes it, the resulting `SIGILL` is trapped by
//!    `Tool::handle_signal_event`, the destination register is filled from the
//!    thread's deterministic PRNG, the flags are set per the Intel-defined
//!    success semantics, `RIP` is advanced past the original instruction, and
//!    the signal is swallowed.
//!
//! The guest therefore observes a *successful* `RDRAND` returning a value from
//! the same deterministic stream that backs `getrandom(2)` and `/dev/urandom`.
//!
//! # Determinism argument
//!
//! The rewritten site set is a pure function of the ELF images the guest maps,
//! so it is identical across runs of the same program. The emulated value comes
//! from `ThreadState::thread_prng`, the per-thread `Pcg64Mcg` seeded from
//! `--rng-seed`, which is the same generator that already backs `getrandom(2)`,
//! `/dev/urandom`, and `AT_RANDOM`; drawing from it consumes a deterministic
//! amount of that stream at a deterministic point in the thread's execution.
//! The trap itself is a `SIGILL` that Detcore already schedules through the
//! ordinary `ResourceID::InboundSignal` request, so the interleaving of the
//! emulated instruction against other threads is decided by the deterministic
//! scheduler exactly as any other signal is. No host state reaches the guest:
//! runtime addresses are used to locate sites but are never logged and never
//! influence the emulated value.
//!
//! # Backend feasibility
//!
//! See `docs/DETERMINISM_ARGUMENT.md`. This module is the *ptrace* mechanism;
//! it needs only guest-memory writes, register access, and signal
//! interception, all of which are part of the abstract Reverie `Guest` and
//! `Tool` interfaces, so any backend providing them can use it unchanged.
//!
//! # Known limits
//!
//! Code that is not part of a file-backed executable mapping at the time it is
//! scanned is not covered: JIT-emitted `RDRAND` in anonymous executable memory,
//! and code the guest itself writes into its own text after we scan it. Those
//! sites still execute natively. [`Config::determinize_rdrand`] does not claim
//! to cover them; see `unscanned_executable_anonymous_mappings`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::section_header;
use iced_x86::Decoder;
use iced_x86::DecoderOptions;
use iced_x86::Mnemonic;
use iced_x86::Register;
use procfs::process::MMPermissions;
use reverie::Errno;
use reverie::Guest;
use reverie::Tool;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use serde::Deserialize;
use serde::Serialize;

use crate::procmaps::MMapPath;
use crate::procmaps::MemoryMap;

/// `ud2`, the two-byte undefined instruction used as the trap.
pub const UD2: [u8; 2] = [0x0f, 0x0b];

/// One-byte `nop`, used to pad the tail of a rewritten site.
pub const NOP: u8 = 0x90;

/// The shortest legal `RDRAND`/`RDSEED` encoding (`0f c7 /6`, no prefixes).
pub const MIN_ENCODED_LEN: usize = 3;

/// The longest encoding this module accepts: `66` + `REX` + `0f c7` + ModRM.
pub const MAX_ENCODED_LEN: usize = 5;

/// Which hardware entropy instruction occupies a site.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize
)]
pub enum RandInsn {
    /// `RDRAND`: read from the on-chip DRBG.
    Rdrand,
    /// `RDSEED`: read from the on-chip entropy conditioner.
    Rdseed,
}

impl RandInsn {
    /// The lowercase mnemonic, for logs and diagnostics.
    pub const fn mnemonic(self) -> &'static str {
        match self {
            RandInsn::Rdrand => "rdrand",
            RandInsn::Rdseed => "rdseed",
        }
    }
}

/// A destination general-purpose register, in x86 encoding order.
///
/// The index is the architectural register number (`rax` = 0 … `r15` = 15),
/// which is what the ModRM `r/m` field plus `REX.B` produces.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize
)]
pub struct GpReg(pub u8);

impl GpReg {
    /// The architectural register number, 0..=15.
    pub const fn index(self) -> u8 {
        self.0
    }

    /// The 64-bit register name, for logs and diagnostics.
    pub const fn name(self) -> &'static str {
        match self.0 {
            0 => "rax",
            1 => "rcx",
            2 => "rdx",
            3 => "rbx",
            4 => "rsp",
            5 => "rbp",
            6 => "rsi",
            7 => "rdi",
            8 => "r8",
            9 => "r9",
            10 => "r10",
            11 => "r11",
            12 => "r12",
            13 => "r13",
            14 => "r14",
            _ => "r15",
        }
    }
}

/// A decoded `RDRAND`/`RDSEED` instruction: everything needed to emulate it.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize
)]
pub struct RandOperation {
    /// Which of the two entropy instructions this is.
    pub insn: RandInsn,
    /// Destination general-purpose register.
    pub reg: GpReg,
    /// Operand width in bytes: 2, 4, or 8.
    pub width: u8,
    /// Total encoded length in bytes, 3..=5.
    pub len: u8,
}

/// A site located in an ELF image, before it is bound to a runtime address.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Serialize,
    Deserialize
)]
pub struct RandSite {
    /// Byte offset of the instruction from the start of the ELF file.
    pub file_offset: u64,
    /// The decoded instruction at that offset.
    pub op: RandOperation,
}

/// Independent, hand-written recognizer for the `RDRAND`/`RDSEED` encodings.
///
/// This deliberately does **not** share code with the `iced_x86` decoder used
/// to find candidate sites. It is the second of two agreeing recognizers, and
/// it runs against the bytes present in guest memory rather than the bytes on
/// disk, so it also catches a wrong load bias or a mapping that does not
/// correspond to the file we scanned.
///
/// The accepted encoding is `[66] [REX] 0f c7 /6` (RDRAND) or `/7` (RDSEED)
/// with `mod == 0b11`. `REX` must follow the operand-size prefix and
/// immediately precede the opcode, which is the only ordering a legal x86-64
/// encoder emits.
pub fn recognize(bytes: &[u8]) -> Option<RandOperation> {
    let mut cursor = 0usize;
    let mut operand_size_override = false;
    let mut rex_w = false;
    let mut rex_b = false;

    if bytes.get(cursor) == Some(&0x66) {
        operand_size_override = true;
        cursor += 1;
    }
    if let Some(&byte) = bytes.get(cursor)
        && (0x40..=0x4f).contains(&byte)
    {
        rex_w = byte & 0x08 != 0;
        rex_b = byte & 0x01 != 0;
        cursor += 1;
    }

    if bytes.get(cursor) != Some(&0x0f) || bytes.get(cursor + 1) != Some(&0xc7) {
        return None;
    }
    cursor += 2;

    let modrm = *bytes.get(cursor)?;
    cursor += 1;

    // Require mod == 0b11 (register direct). Any other mod turns `0f c7 /6`
    // into a memory-operand instruction that is not RDRAND.
    if modrm & 0xc0 != 0xc0 {
        return None;
    }
    let insn = match modrm & 0x38 {
        0x30 => RandInsn::Rdrand,
        0x38 => RandInsn::Rdseed,
        _ => return None,
    };

    let reg = GpReg((modrm & 0x07) | (u8::from(rex_b) << 3));
    let width = if rex_w {
        8
    } else if operand_size_override {
        2
    } else {
        4
    };

    Some(RandOperation {
        insn,
        reg,
        width,
        len: u8::try_from(cursor).ok()?,
    })
}

/// Build the replacement bytes for a site of `len` bytes: `ud2` then `nop`s.
pub fn trap_bytes(len: u8) -> Vec<u8> {
    let mut bytes = vec![NOP; usize::from(len)];
    bytes[..UD2.len()].copy_from_slice(&UD2);
    bytes
}

/// Convert an `iced_x86` destination register to an architectural index and
/// operand width, or `None` if it is not a general-purpose register this
/// module knows how to write back.
fn gp_register(register: Register) -> Option<(GpReg, u8)> {
    let width = u8::try_from(register.size()).ok()?;
    if !matches!(width, 2 | 4 | 8) {
        return None;
    }
    let index = match register.full_register() {
        Register::RAX => 0,
        Register::RCX => 1,
        Register::RDX => 2,
        Register::RBX => 3,
        Register::RSP => 4,
        Register::RBP => 5,
        Register::RSI => 6,
        Register::RDI => 7,
        Register::R8 => 8,
        Register::R9 => 9,
        Register::R10 => 10,
        Register::R11 => 11,
        Register::R12 => 12,
        Register::R13 => 13,
        Register::R14 => 14,
        Register::R15 => 15,
        _ => return None,
    };
    Some((GpReg(index), width))
}

#[derive(Debug, Clone, Copy)]
struct ExecutableRange {
    file_offset: u64,
    address: u64,
    size: u64,
}

/// Linearly disassemble every executable range of an ELF image and return the
/// `RDRAND`/`RDSEED` sites it contains, sorted by file offset.
///
/// Returns an empty vector — not an error — for an image that is not an
/// x86-64/x86 ELF, so that a guest mapping something unexpected simply gets no
/// coverage instead of failing the run.
pub fn scan_elf(bytes: &[u8]) -> Result<Vec<RandSite>, String> {
    let elf = Elf::parse(bytes).map_err(|err| format!("failed to parse ELF image: {err}"))?;
    let bitness = match elf.header.e_machine {
        header::EM_386 => 32,
        header::EM_X86_64 => 64,
        _ => return Ok(Vec::new()),
    };

    let mut sites = Vec::new();
    for range in executable_ranges(&elf) {
        let (Ok(start), Ok(size)) = (
            usize::try_from(range.file_offset),
            usize::try_from(range.size),
        ) else {
            continue;
        };
        let Some(code) = start
            .checked_add(size)
            .and_then(|end| bytes.get(start..end))
        else {
            // A range that does not lie inside the file is not something we can
            // scan; skip it rather than reject the whole image.
            continue;
        };
        // Linear disassembly is the expensive part, and the overwhelming
        // majority of executable sections cannot contain the instruction at
        // all. `may_contain_rand_encoding` is a necessary condition, so
        // skipping on a negative cannot lose a site.
        if !may_contain_rand_encoding(code) {
            continue;
        }
        scan_range(code, range, bitness, &mut sites);
    }
    sites.sort_unstable();
    sites.dedup();
    Ok(sites)
}

/// Cheap necessary condition for a byte range to hold a `RDRAND`/`RDSEED`.
///
/// Every encoding ends in the three bytes `0f c7` followed by a ModRM in
/// `f0..=ff` (`mod == 0b11`, `reg` 6 or 7), regardless of prefixes. A range
/// with no such triple anywhere — at an instruction boundary or not — provably
/// contains no site, so the linear disassembly can be skipped. This is a
/// *filter*, never a locator: a positive result only means the range is worth
/// disassembling, because the same bytes occur by chance inside longer
/// instructions and in embedded data.
fn may_contain_rand_encoding(bytes: &[u8]) -> bool {
    bytes
        .windows(3)
        .any(|window| window[0] == 0x0f && window[1] == 0xc7 && window[2] >= 0xf0)
}

fn scan_range(bytes: &[u8], range: ExecutableRange, bitness: u32, sites: &mut Vec<RandSite>) {
    let mut decoder = Decoder::with_ip(bitness, bytes, range.address, DecoderOptions::NONE);
    while decoder.can_decode() {
        let instruction = decoder.decode();
        let insn = match instruction.mnemonic() {
            Mnemonic::Rdrand => RandInsn::Rdrand,
            Mnemonic::Rdseed => RandInsn::Rdseed,
            _ => continue,
        };
        let Some((reg, width)) = gp_register(instruction.op0_register()) else {
            continue;
        };
        let Ok(len) = u8::try_from(instruction.len()) else {
            continue;
        };
        if !(MIN_ENCODED_LEN..=MAX_ENCODED_LEN).contains(&usize::from(len)) {
            continue;
        }
        let Some(relative) = instruction.ip().checked_sub(range.address) else {
            continue;
        };
        let Some(file_offset) = range.file_offset.checked_add(relative) else {
            continue;
        };
        sites.push(RandSite {
            file_offset,
            op: RandOperation {
                insn,
                reg,
                width,
                len,
            },
        });
    }
}

/// Executable byte ranges of an ELF image, preferring section headers (which
/// exclude non-code padding) and falling back to `PT_LOAD` segments when a
/// stripped image has none. Mirrors `hermit_cli::instruction_map`.
fn executable_ranges(elf: &Elf<'_>) -> Vec<ExecutableRange> {
    let mut ranges: Vec<ExecutableRange> = elf
        .section_headers
        .iter()
        .filter(|section| {
            section.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
                && section.sh_type != section_header::SHT_NOBITS
                && section.sh_size != 0
        })
        .map(|section| ExecutableRange {
            file_offset: section.sh_offset,
            address: section.sh_addr,
            size: section.sh_size,
        })
        .collect();

    if ranges.is_empty() {
        ranges.extend(
            elf.program_headers
                .iter()
                .filter(|segment| {
                    segment.p_type == program_header::PT_LOAD
                        && segment.p_flags & program_header::PF_X != 0
                        && segment.p_filesz != 0
                })
                .map(|segment| ExecutableRange {
                    file_offset: segment.p_offset,
                    address: segment.p_vaddr,
                    size: segment.p_filesz,
                }),
        );
    }

    ranges.sort_unstable_by_key(|range| (range.file_offset, range.address, range.size));
    ranges
}

/// Write `value` into the architectural register `op.reg` of `regs`, honoring
/// x86-64 partial-register semantics: a 64-bit or 32-bit destination replaces
/// the whole 64-bit register (32-bit writes zero-extend), while a 16-bit
/// destination leaves the upper 48 bits untouched.
pub fn write_destination(regs: &mut libc::user_regs_struct, op: RandOperation, value: u64) {
    let slot: &mut u64 = match op.reg.index() {
        0 => &mut regs.rax,
        1 => &mut regs.rcx,
        2 => &mut regs.rdx,
        3 => &mut regs.rbx,
        4 => &mut regs.rsp,
        5 => &mut regs.rbp,
        6 => &mut regs.rsi,
        7 => &mut regs.rdi,
        8 => &mut regs.r8,
        9 => &mut regs.r9,
        10 => &mut regs.r10,
        11 => &mut regs.r11,
        12 => &mut regs.r12,
        13 => &mut regs.r13,
        14 => &mut regs.r14,
        _ => &mut regs.r15,
    };
    *slot = match op.width {
        8 => value,
        4 => value & 0xffff_ffff,
        _ => (*slot & !0xffffu64) | (value & 0xffff),
    };
}

/// Read the current value of `op.reg`, used only by tests and tracing.
pub fn read_destination(regs: &libc::user_regs_struct, op: RandOperation) -> u64 {
    let raw = match op.reg.index() {
        0 => regs.rax,
        1 => regs.rcx,
        2 => regs.rdx,
        3 => regs.rbx,
        4 => regs.rsp,
        5 => regs.rbp,
        6 => regs.rsi,
        7 => regs.rdi,
        8 => regs.r8,
        9 => regs.r9,
        10 => regs.r10,
        11 => regs.r11,
        12 => regs.r12,
        13 => regs.r13,
        14 => regs.r14,
        _ => regs.r15,
    };
    match op.width {
        8 => raw,
        4 => raw & 0xffff_ffff,
        _ => raw & 0xffff,
    }
}

/// `CF`, set by `RDRAND`/`RDSEED` to report that a value was produced.
const EFLAGS_CF: u64 = 1 << 0;
/// `PF`, `AF`, `ZF`, `SF`, `OF`: cleared by `RDRAND`/`RDSEED` unconditionally.
const EFLAGS_CLEARED_BY_RDRAND: u64 = (1 << 2) | (1 << 4) | (1 << 6) | (1 << 7) | (1 << 11);

/// Apply the Intel-defined flag effects of a successful `RDRAND`/`RDSEED`:
/// `CF = 1`, and `OF = SF = ZF = AF = PF = 0`.
pub fn set_success_flags(regs: &mut libc::user_regs_struct) {
    regs.eflags &= !EFLAGS_CLEARED_BY_RDRAND;
    regs.eflags |= EFLAGS_CF;
}

/// The per-address-space table of rewritten sites.
///
/// Shared between threads of one address space and deep-copied on `fork`,
/// exactly like `ThreadState::memory_metadata`, because a forked child inherits
/// the parent's already-patched text through copy-on-write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RandSiteTable {
    /// Runtime address of a rewritten `ud2` to the site it replaced.
    patched: BTreeMap<u64, RandSite>,
    /// Executable mappings already considered, keyed by
    /// `(device, inode, file offset, start address)`, so that re-scanning after
    /// each `mmap` is idempotent.
    scanned: BTreeSet<(u64, u64, u64, u64)>,
    /// Sites found but refused because the two recognizers disagreed or the
    /// rewrite could not be written back.
    refused: u64,
    /// Sites successfully rewritten.
    rewritten: u64,
    /// Emulated executions of a rewritten site.
    emulated: u64,
}

impl RandSiteTable {
    /// An empty table, for an address space nothing has been rewritten in yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a faulting address. Returns the site that used to live there,
    /// or `None` if this `SIGILL` is not ours.
    pub fn lookup(&self, address: u64) -> Option<RandSite> {
        self.patched.get(&address).copied()
    }

    /// Record a mapping as considered. Returns `false` if it already was,
    /// which is what makes re-scanning after every `mmap` cheap and idempotent.
    pub fn mark_scanned(&mut self, key: (u64, u64, u64, u64)) -> bool {
        self.scanned.insert(key)
    }

    /// Remember that `address` now holds a trap standing in for `site`.
    pub fn record_rewrite(&mut self, address: u64, site: RandSite) {
        if self.patched.insert(address, site).is_none() {
            self.rewritten += 1;
        }
    }

    /// Count a site that was found but could not be rewritten.
    pub fn record_refusal(&mut self) {
        self.refused += 1;
    }

    /// Count one emulated execution of a rewritten site.
    pub fn record_emulation(&mut self) {
        self.emulated += 1;
    }

    /// How many sites have been rewritten in this address space.
    pub fn rewritten(&self) -> u64 {
        self.rewritten
    }

    /// How many sites were found but left live.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// How many rewritten sites have been executed and emulated.
    pub fn emulated(&self) -> u64 {
        self.emulated
    }

    /// Whether any site has been rewritten at all.
    pub fn is_empty(&self) -> bool {
        self.patched.is_empty()
    }
}

/// Outcome of one rewriting pass over the guest's executable mappings.
#[derive(Debug, Default, Clone)]
pub struct RewriteReport {
    /// Newly considered file-backed executable mappings.
    pub mappings_scanned: usize,
    /// Sites rewritten to `ud2` during this pass (not cumulative).
    pub rewritten: u64,
    /// Sites found but refused; see [`RewriteReport::refusals`].
    pub refused: u64,
    /// Human-readable refusal reasons, bound to image + file offset (never a
    /// runtime address, which is host state).
    pub refusals: Vec<String>,
    /// Executable mappings with no backing file, which cannot be scanned. A
    /// nonzero count means `RDRAND` coverage is incomplete for this guest.
    pub unscannable_anonymous_mappings: usize,
}

impl RewriteReport {
    /// Whether this pass actually rewrote or refused a site. Scanning an image
    /// that contains none is a no-op not worth recording.
    pub fn touched_a_site(&self) -> bool {
        self.rewritten > 0 || self.refused > 0
    }
}

/// Read the ELF image behind a mapping. Prefers `/proc/<pid>/map_files/<range>`
/// because it resolves to the real inode regardless of the guest's mount
/// namespace or chroot, and falls back to the path `/proc/<pid>/maps` reported.
fn read_mapped_image(pid: reverie::Pid, map: &MemoryMap) -> Result<Vec<u8>, String> {
    let (start, end) = map.address;
    let link = format!("/proc/{}/map_files/{:x}-{:x}", pid.as_raw(), start, end);
    match std::fs::read(&link) {
        Ok(bytes) => Ok(bytes),
        Err(err) => {
            if let MMapPath::Path(path) = &map.pathname {
                return std::fs::read(path).map_err(|fallback| {
                    format!(
                        "cannot read mapped image: {link}: {err}; {}: {fallback}",
                        path.display()
                    )
                });
            }
            Err(format!("cannot read mapped image {link}: {err}"))
        }
    }
}

/// Overwrite `bytes` at guest address `address`.
///
/// Writes are done as read-modify-write of naturally aligned 8-byte words
/// because `safeptrace`'s `MemoryAccess::write` only takes the `PTRACE_POKEDATA`
/// path for exactly-8-byte writes, and `PTRACE_POKEDATA` is the only mechanism
/// that can write a read-only text page. Any other size falls through to
/// `process_vm_writev`, which honors page protection and would fail with
/// `EFAULT` on `r-x` text.
fn write_over_text<M: MemoryAccess>(
    memory: &mut M,
    address: u64,
    bytes: &[u8],
) -> Result<(), Errno> {
    let first_word = address & !7;
    let last_word = (address + bytes.len() as u64 - 1) & !7;
    let mut word_address = first_word;
    while word_address <= last_word {
        let addr = Addr::<u64>::from_raw(word_address as usize).ok_or(Errno::EFAULT)?;
        let mut word = memory.read_value::<_, u64>(addr)?.to_le_bytes();
        for (index, slot) in word.iter_mut().enumerate() {
            let byte_address = word_address + index as u64;
            if byte_address >= address && byte_address < address + bytes.len() as u64 {
                *slot = bytes[(byte_address - address) as usize];
            }
        }
        let addr = AddrMut::<u64>::from_raw(word_address as usize).ok_or(Errno::EFAULT)?;
        memory.write_value(addr, &u64::from_le_bytes(word))?;
        word_address += 8;
    }
    Ok(())
}

/// Scan every file-backed executable mapping of the guest that has not been
/// seen before, and rewrite each `RDRAND`/`RDSEED` site it contains to `ud2`.
///
/// Idempotent: mappings are remembered in `table`, so this may be called after
/// `execve` and again after each `mmap` that adds executable memory.
pub fn rewrite_new_executable_mappings<T, G>(
    guest: &mut G,
    table: &mut RandSiteTable,
) -> Result<RewriteReport, reverie::Error>
where
    T: Tool,
    G: Guest<T>,
{
    let pid = guest.pid();
    let maps = crate::procmaps::from_pid(pid, |map| {
        map.perms.contains(MMPermissions::EXECUTE) && map.address.1 > map.address.0
    })?;

    let mut report = RewriteReport::default();
    for map in maps {
        let (start, end) = map.address;
        if !matches!(map.pathname, MMapPath::Path(_)) {
            // vDSO, vsyscall, and JIT/anonymous executable memory have no ELF
            // image on disk to disassemble. The vDSO is Reverie-controlled and
            // contains no RDRAND; anonymous executable memory is the real
            // coverage hole and is counted so callers can report it.
            if matches!(map.pathname, MMapPath::Anonymous) {
                report.unscannable_anonymous_mappings += 1;
            }
            continue;
        }
        let key = (
            map.dev.0 as u64 | ((map.dev.1 as u64) << 32),
            map.inode,
            map.offset,
            start,
        );
        if !table.mark_scanned(key) {
            continue;
        }
        report.mappings_scanned += 1;

        let image = match read_mapped_image(pid, &map) {
            Ok(image) => image,
            Err(reason) => {
                report.refused += 1;
                table.record_refusal();
                report.refusals.push(reason);
                continue;
            }
        };
        let sites = match scan_elf(&image) {
            Ok(sites) => sites,
            Err(reason) => {
                // Not an ELF we can disassemble. Only a problem if it turns out
                // to contain RDRAND, which we cannot know; report it so a
                // fail-closed caller can refuse the run.
                report.refused += 1;
                table.record_refusal();
                report
                    .refusals
                    .push(format!("{}: {reason}", display_image_name(&map.pathname)));
                continue;
            }
        };

        let span = end - start;
        for site in sites {
            // Only sites whose file offset falls inside this mapping's slice of
            // the file are resident at a known address here.
            if site.file_offset < map.offset || site.file_offset - map.offset >= span {
                continue;
            }
            let address = start + (site.file_offset - map.offset);
            match rewrite_site(guest, address, site.op) {
                Ok(()) => {
                    table.record_rewrite(address, site);
                    report.rewritten += 1;
                }
                Err(reason) => {
                    report.refused += 1;
                    table.record_refusal();
                    report.refusals.push(format!(
                        "{}+{:#x}: {} not rewritten: {reason}",
                        display_image_name(&map.pathname),
                        site.file_offset,
                        site.op.insn.mnemonic(),
                    ));
                }
            }
        }
    }
    Ok(report)
}

fn display_image_name(path: &MMapPath) -> String {
    match path {
        MMapPath::Path(path) => path.display().to_string(),
        other => format!("{other:?}"),
    }
}

/// Rewrite one site, after confirming the bytes in guest memory really are the
/// instruction the ELF scan said they were.
fn rewrite_site<T, G>(guest: &mut G, address: u64, expected: RandOperation) -> Result<(), String>
where
    T: Tool,
    G: Guest<T>,
{
    let len = usize::from(expected.len);
    let mut memory = guest.memory();
    let mut present = [0u8; MAX_ENCODED_LEN];
    let addr = Addr::<u8>::from_raw(address as usize)
        .ok_or_else(|| "site resolves to a null guest address".to_owned())?;
    memory
        .read_exact(addr, &mut present[..len])
        .map_err(|err| format!("cannot read the site out of guest memory: {err}"))?;

    // The second, independent recognizer. Disagreement means the bytes in
    // memory are not the bytes we disassembled — a wrong load bias, a relocated
    // or prelinked image, or a mapping that does not correspond to this file.
    // Refuse rather than corrupt the guest.
    match recognize(&present[..len]) {
        Some(actual) if actual == expected => {}
        Some(actual) => {
            return Err(format!(
                "guest memory holds {} (width {}, {}), the ELF scan expected {} (width {}, {})",
                actual.insn.mnemonic(),
                actual.width,
                actual.reg.name(),
                expected.insn.mnemonic(),
                expected.width,
                expected.reg.name(),
            ));
        }
        None => {
            return Err(format!(
                "guest memory holds {:02x?}, which is not a {} encoding",
                &present[..len],
                expected.insn.mnemonic()
            ));
        }
    }

    let trap = trap_bytes(expected.len);
    write_over_text(&mut memory, address, &trap)
        .map_err(|err| format!("cannot write the trap into guest text: {err}"))?;

    // Read back: a POKEDATA that silently did not land would otherwise leave a
    // live RDRAND that we believe is trapped, which is exactly the
    // false-assurance failure this whole change exists to remove.
    let mut readback = [0u8; MAX_ENCODED_LEN];
    memory
        .read_exact(addr, &mut readback[..len])
        .map_err(|err| format!("cannot read back the rewritten site: {err}"))?;
    if readback[..len] != trap[..] {
        return Err(format!(
            "trap did not land: guest memory holds {:02x?}, expected {:02x?}",
            &readback[..len],
            &trap[..]
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_regs() -> libc::user_regs_struct {
        // SAFETY: `user_regs_struct` is a plain repr(C) integer struct.
        unsafe { std::mem::zeroed() }
    }

    #[test]
    fn recognizes_every_operand_width_and_both_instructions() {
        // rdrand %ecx  => 0f c7 f1
        assert_eq!(
            recognize(&[0x0f, 0xc7, 0xf1]),
            Some(RandOperation {
                insn: RandInsn::Rdrand,
                reg: GpReg(1),
                width: 4,
                len: 3
            })
        );
        // rdrand %rcx  => 48 0f c7 f1
        assert_eq!(
            recognize(&[0x48, 0x0f, 0xc7, 0xf1]),
            Some(RandOperation {
                insn: RandInsn::Rdrand,
                reg: GpReg(1),
                width: 8,
                len: 4
            })
        );
        // rdrand %cx   => 66 0f c7 f1  (the 0x66 prefix counts toward the length)
        assert_eq!(
            recognize(&[0x66, 0x0f, 0xc7, 0xf1]),
            Some(RandOperation {
                insn: RandInsn::Rdrand,
                reg: GpReg(1),
                width: 2,
                len: 4
            })
        );
        // rdseed %rcx  => 48 0f c7 f9
        assert_eq!(
            recognize(&[0x48, 0x0f, 0xc7, 0xf9]),
            Some(RandOperation {
                insn: RandInsn::Rdseed,
                reg: GpReg(1),
                width: 8,
                len: 4
            })
        );
        // rdrand %r13  => 49 0f c7 f5  (REX.B selects the extended register)
        assert_eq!(
            recognize(&[0x49, 0x0f, 0xc7, 0xf5]),
            Some(RandOperation {
                insn: RandInsn::Rdrand,
                reg: GpReg(13),
                width: 8,
                len: 4
            })
        );
        // rdseed %r13w => 66 41 0f c7 fd  (five bytes: the longest form)
        assert_eq!(
            recognize(&[0x66, 0x41, 0x0f, 0xc7, 0xfd]),
            Some(RandOperation {
                insn: RandInsn::Rdseed,
                reg: GpReg(13),
                width: 2,
                len: 5
            })
        );
    }

    #[test]
    fn refuses_near_misses_that_are_not_rdrand() {
        // mod != 0b11 makes `0f c7 /6` a memory-operand instruction.
        assert_eq!(recognize(&[0x0f, 0xc7, 0x30]), None);
        // reg field 1 is cmpxchg8b, not rdrand/rdseed.
        assert_eq!(recognize(&[0x0f, 0xc7, 0xc8]), None);
        // Truncated encodings.
        assert_eq!(recognize(&[0x0f, 0xc7]), None);
        assert_eq!(recognize(&[0x48, 0x0f]), None);
        assert_eq!(recognize(&[]), None);
        // A different two-byte opcode.
        assert_eq!(recognize(&[0x0f, 0x31]), None);
    }

    #[test]
    fn the_two_recognizers_agree_on_every_encoding_the_decoder_finds() {
        // Exhaustively cross-check the hand-written recognizer against the
        // iced_x86 decoder over the whole legal encoding space. This is the
        // property the runtime patcher relies on: if they ever disagree, the
        // patcher refuses the site instead of corrupting the guest.
        let mut checked = 0usize;
        for prefix66 in [false, true] {
            for rex in [None, Some(0x40u8), Some(0x41), Some(0x48), Some(0x49)] {
                for modrm in 0xf0u8..=0xff {
                    let mut bytes = Vec::new();
                    if prefix66 {
                        bytes.push(0x66);
                    }
                    if let Some(rex) = rex {
                        bytes.push(rex);
                    }
                    bytes.extend_from_slice(&[0x0f, 0xc7, modrm]);

                    let mut decoder = Decoder::with_ip(64, &bytes, 0x1000, DecoderOptions::NONE);
                    let instruction = decoder.decode();
                    let decoded = match instruction.mnemonic() {
                        Mnemonic::Rdrand => Some(RandInsn::Rdrand),
                        Mnemonic::Rdseed => Some(RandInsn::Rdseed),
                        _ => None,
                    }
                    .and_then(|insn| {
                        let (reg, width) = gp_register(instruction.op0_register())?;
                        Some(RandOperation {
                            insn,
                            reg,
                            width,
                            len: u8::try_from(instruction.len()).ok()?,
                        })
                    });

                    assert_eq!(
                        decoded,
                        recognize(&bytes),
                        "recognizers disagree on {bytes:02x?}"
                    );
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 2 * 5 * 16);
    }

    #[test]
    fn trap_bytes_are_ud2_then_nop_padding() {
        assert_eq!(trap_bytes(3), vec![0x0f, 0x0b, 0x90]);
        assert_eq!(trap_bytes(4), vec![0x0f, 0x0b, 0x90, 0x90]);
        assert_eq!(trap_bytes(5), vec![0x0f, 0x0b, 0x90, 0x90, 0x90]);
    }

    #[test]
    fn destination_write_honors_partial_register_semantics() {
        let op = |reg, width| RandOperation {
            insn: RandInsn::Rdrand,
            reg: GpReg(reg),
            width,
            len: 4,
        };

        // A 64-bit destination takes the whole value.
        let mut regs = zero_regs();
        regs.rcx = 0xdead_beef_dead_beef;
        write_destination(&mut regs, op(1, 8), 0x0123_4567_89ab_cdef);
        assert_eq!(regs.rcx, 0x0123_4567_89ab_cdef);

        // A 32-bit destination zero-extends into the upper half.
        let mut regs = zero_regs();
        regs.rcx = 0xdead_beef_dead_beef;
        write_destination(&mut regs, op(1, 4), 0x0123_4567_89ab_cdef);
        assert_eq!(regs.rcx, 0x89ab_cdef);

        // A 16-bit destination preserves the upper 48 bits.
        let mut regs = zero_regs();
        regs.rcx = 0xdead_beef_dead_beef;
        write_destination(&mut regs, op(1, 2), 0x0123_4567_89ab_cdef);
        assert_eq!(regs.rcx, 0xdead_beef_dead_cdef);

        // Every architectural index reaches a distinct register slot.
        let mut seen = BTreeSet::new();
        for index in 0..16u8 {
            let mut regs = zero_regs();
            write_destination(&mut regs, op(index, 8), 0xffff_ffff_ffff_ffff);
            let touched: Vec<&'static str> = [
                ("rax", regs.rax),
                ("rcx", regs.rcx),
                ("rdx", regs.rdx),
                ("rbx", regs.rbx),
                ("rsp", regs.rsp),
                ("rbp", regs.rbp),
                ("rsi", regs.rsi),
                ("rdi", regs.rdi),
                ("r8", regs.r8),
                ("r9", regs.r9),
                ("r10", regs.r10),
                ("r11", regs.r11),
                ("r12", regs.r12),
                ("r13", regs.r13),
                ("r14", regs.r14),
                ("r15", regs.r15),
            ]
            .into_iter()
            .filter(|(_, value)| *value != 0)
            .map(|(name, _)| name)
            .collect();
            assert_eq!(touched.len(), 1, "index {index} touched {touched:?}");
            assert!(seen.insert(touched[0]), "index {index} aliased a register");
            assert_eq!(touched[0], GpReg(index).name());
        }
        assert_eq!(seen.len(), 16);
    }

    #[test]
    fn success_flags_set_carry_and_clear_the_arithmetic_flags() {
        let mut regs = zero_regs();
        // Start with every affected flag in the wrong state, plus an unrelated
        // flag (IF, bit 9) that must survive.
        regs.eflags = EFLAGS_CLEARED_BY_RDRAND | (1 << 9);
        set_success_flags(&mut regs);
        assert_eq!(regs.eflags & EFLAGS_CF, EFLAGS_CF);
        assert_eq!(regs.eflags & EFLAGS_CLEARED_BY_RDRAND, 0);
        assert_eq!(regs.eflags & (1 << 9), 1 << 9);
    }

    #[test]
    fn scan_finds_rdrand_in_an_elf_and_skips_a_lookalike_in_data() {
        // `48 0f c7 f1` = rdrand %rcx, then `0f c7 f9` = rdseed %ecx, then a
        // `mov` whose 32-bit immediate happens to contain the byte pattern
        // `0f c7 f0` — a linear disassembly must not report the immediate.
        let code: Vec<u8> = vec![
            0x48, 0x0f, 0xc7, 0xf1, // rdrand %rcx
            0x0f, 0xc7, 0xf9, // rdseed %ecx
            0xb8, 0x0f, 0xc7, 0xf0, 0x00, // mov $0x00f0c70f,%eax
            0xc3, // ret
        ];
        let sites = scan_elf(&elf_with_executable_section(&code)).unwrap();
        assert_eq!(
            sites,
            vec![
                RandSite {
                    file_offset: CODE_OFFSET as u64,
                    op: RandOperation {
                        insn: RandInsn::Rdrand,
                        reg: GpReg(1),
                        width: 8,
                        len: 4
                    }
                },
                RandSite {
                    file_offset: CODE_OFFSET as u64 + 4,
                    op: RandOperation {
                        insn: RandInsn::Rdseed,
                        reg: GpReg(1),
                        width: 4,
                        len: 3
                    }
                },
            ],
            "the immediate operand at offset {} must not be reported",
            CODE_OFFSET + 8
        );
    }

    #[test]
    fn the_prefilter_is_a_necessary_condition_not_a_locator() {
        // Negative: a range with no `0f c7 f0..ff` triple is provably clean.
        assert!(!may_contain_rand_encoding(&[
            0xb8, 0x2a, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0xa2, 0xc3
        ]));
        // `cmpxchg16b (%rax)` is `f0 48 0f c7 08` — same opcode, ModRM below
        // 0xf0, so the filter still rejects it.
        assert!(!may_contain_rand_encoding(&[0xf0, 0x48, 0x0f, 0xc7, 0x08]));
        // Positive: a real site.
        assert!(may_contain_rand_encoding(&[0x48, 0x0f, 0xc7, 0xf1]));
        // Positive on a lookalike inside data, which is why it is only a
        // filter: the disassembler still has to decide.
        assert!(may_contain_rand_encoding(&[0xb8, 0x0f, 0xc7, 0xf0, 0x00]));
        // Every encoding the scanner can report must pass the filter.
        for prefix66 in [false, true] {
            for rex in [None, Some(0x40u8), Some(0x41), Some(0x48), Some(0x49)] {
                for modrm in 0xf0u8..=0xff {
                    let mut bytes = Vec::new();
                    if prefix66 {
                        bytes.push(0x66);
                    }
                    if let Some(rex) = rex {
                        bytes.push(rex);
                    }
                    bytes.extend_from_slice(&[0x0f, 0xc7, modrm]);
                    assert!(
                        may_contain_rand_encoding(&bytes),
                        "filter rejected {bytes:02x?}"
                    );
                }
            }
        }
    }

    #[test]
    fn scan_of_a_clean_program_finds_nothing() {
        let code: Vec<u8> = vec![
            0xb8, 0x2a, 0x00, 0x00, 0x00, // mov $42,%eax
            0x0f, 0x05, // syscall
            0x0f, 0xa2, // cpuid
            0xc3, // ret
        ];
        assert!(
            scan_elf(&elf_with_executable_section(&code))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn scan_rejects_a_non_elf_image() {
        assert!(scan_elf(b"not an elf at all").is_err());
    }

    // ---- Minimal ELF builder, mirroring hermit_cli::instruction_map tests ----

    const CODE_OFFSET: usize = 64;
    const CODE_ADDRESS: u64 = 0x0040_0000;
    const SECTION_HEADER_SIZE: usize = 64;

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn elf_with_executable_section(code: &[u8]) -> Vec<u8> {
        let section_table = (CODE_OFFSET + code.len() + 7) & !7;
        let mut bytes = vec![0; section_table + 2 * SECTION_HEADER_SIZE];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[6] = 1;
        write_u16(&mut bytes, 16, header::ET_EXEC);
        write_u16(&mut bytes, 18, header::EM_X86_64);
        write_u32(&mut bytes, 20, 1);
        write_u64(&mut bytes, 24, CODE_ADDRESS);
        write_u64(&mut bytes, 40, section_table as u64);
        write_u16(&mut bytes, 52, 64);
        write_u16(&mut bytes, 58, SECTION_HEADER_SIZE as u16);
        write_u16(&mut bytes, 60, 2);
        bytes[CODE_OFFSET..CODE_OFFSET + code.len()].copy_from_slice(code);

        let text = section_table + SECTION_HEADER_SIZE;
        write_u32(&mut bytes, text + 4, section_header::SHT_PROGBITS);
        write_u64(
            &mut bytes,
            text + 8,
            u64::from(section_header::SHF_ALLOC | section_header::SHF_EXECINSTR),
        );
        write_u64(&mut bytes, text + 16, CODE_ADDRESS);
        write_u64(&mut bytes, text + 24, CODE_OFFSET as u64);
        write_u64(&mut bytes, text + 32, code.len() as u64);
        write_u64(&mut bytes, text + 48, 1);
        bytes
    }
}
