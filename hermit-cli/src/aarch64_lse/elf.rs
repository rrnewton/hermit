/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Applying the exclusive-loop rewrite to the executable segments of an
//! aarch64 ELF file.
//!
//! The rewrite is in place and the same length, so the output file is the
//! input file with some instruction words replaced: every offset, segment,
//! symbol, relocation and build id stays as it was, and the loader maps it
//! exactly as it mapped the original.
//!
//! Only sections marked executable (`SHF_EXECINSTR`, so `.text`, `.plt`,
//! `.init` and the like) are scanned. An executable `PT_LOAD` segment also
//! carries the ELF header, the dynamic symbol and relocation tables and
//! read-only data, and those bytes decode as instructions as readily as code
//! does: measured on one Android test binary, scanning whole segments found
//! 69 "exclusive instructions" of which 66 were in data before `.text` even
//! began. A file with no section headers is left alone and reported, because
//! nothing in it can be told apart from data.
//!
//! Within those sections, only code is rewritten ([`super::mapping`]). An
//! executable section can hold data too (a literal pool, or OpenSSL's
//! constant tables in `.text`), and a table whose words happened to form a
//! canonical loop would otherwise be rewritten, corrupting it. What is taken
//! for code depends on what the file says, section by section:
//!
//! - Mapping symbols: exactly the ranges they mark `$x`. The AArch64 ELF ABI
//!   has an assembler mark data `$d`, so data marked as the ABI says is never
//!   taken for code.
//! - The unwind table alone (a section without mapping symbols, as in every
//!   stripped file): what control reaches from the entries of the functions
//!   its FDEs describe, following direct branches. A call falls through
//!   only where the callee can be shown to come back: a function here whose
//!   own body reaches a `RET`, or which tail-calls one that does, worked out
//!   over all of them until the answer stops growing; a function the image
//!   names as one that never returns (`exit`, `abort`, a panic entry point,
//!   reached directly or through its procedure linkage table entry) never
//!   does. An FDE covers its whole function, so a table the function
//!   branches over, keeps after a return, or places after a call that does
//!   not come back is not reached at all.
//!
//!   On top of that, whatever the code addresses as data (an `LDR`
//!   (literal), `ADR` or `ADRP` target, each completion of one `ADRP` being
//!   followed through an `ADD` into its own register) is taken for data
//!   from that address to the next instruction a branch reaches, or the
//!   function's end, and no sequence over it is rewritten
//!   (`addressed-data-inside-the-sequence`). An `ADRP` nothing completes
//!   gives only its page, and every function that page overlaps is taken
//!   for data. A pointer the image stores to such a table counts as well: a
//!   dynamic relocation's address, a plain word of a loaded, non-executable
//!   section in an executable with fixed addresses, or a symbol naming data
//!   in an executable section.
//!
//!   A literal or `ADR` that names a function's own entry is a pointer to
//!   the function, not to data, so an address-taken function's body is
//!   still code.
//!
//!   Several things can still be taken for code wrongly, each needing data
//!   that nothing in the image points at. Data after a call to an imported
//!   function that never returns and is not among the names this reader
//!   knows. Data an unresolved `ADRP`'s completion would reach beyond its
//!   own page (a scaled load reaches 64 KiB, an `ADD` with `LSL #12` reaches
//!   16 MiB). The rest of a table whose first word a branch could reach: a
//!   data word before the field the code addresses can decode as a branch
//!   into the table, and the next such target ends the data. What the code
//!   addresses through a register after a write that does not carry an
//!   `ADRP`'s base, since where that register then points is not in the
//!   window. A function that returns through a changed link register, or
//!   through an `SVC` the walk cannot follow, taken to return to its
//!   caller. And a call through a register (`BLR`), whose callee is
//!   unknown, taken to come back.
//! - Neither: nothing. Such a section is left alone and reported, and so is
//!   a whole file with neither mapping symbols nor an unwind table.
//!
//! Under any evidence, a sequence an `LDR` (literal) or `ADR` in the code
//! reaches is refused (`literal-inside-the-sequence`): an object linked
//! without mapping symbols after one with them inherits the earlier `$x`
//! range. The branch analysis still reads every word of every executable
//! section, so a word that might branch into a sequence still refuses it.
//!
//! A section in a segment that is both writable and executable is refused
//! too: writable text is either a loader quirk or a runtime that rewrites
//! itself, and rewriting it would race whatever writes it.
//!
//! A sequence any relocation points into is refused as well. Relocations are
//! read as 64-bit little-endian, the only aarch64 ELF this rewriter handles. A relocation is
//! the loader's licence to change those bytes at load time, which would
//! overwrite part of a rewritten sequence with something computed for the
//! original one.

use std::collections::BTreeMap;
use std::path::Path;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::section_header;

use super::insn;
use super::mapping;
use super::rewrite;
use super::rewrite::Idiom;
use super::rewrite::Refusal;

/// What a rewrite did to one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileReport {
    /// Exclusive instructions found in executable sections.
    pub exclusive_instructions: usize,
    /// Rewritten sequences, by idiom.
    pub patched: BTreeMap<&'static str, usize>,
    /// Sequences left alone, by reason.
    pub refused: BTreeMap<&'static str, usize>,
    /// Every sequence left alone, by address, with the reason, in the order
    /// the sections were planned: all of them, so that a listing of them
    /// (`hermit rewrite-exclusives --list-refusals`) is complete. A run logs
    /// only the first few.
    pub refused_sites: Vec<(u64, String)>,
    /// Executable sections skipped entirely, with the reason.
    pub skipped_segments: Vec<String>,
}

impl FileReport {
    pub fn patched_total(&self) -> usize {
        self.patched.values().sum()
    }

    pub fn refused_total(&self) -> usize {
        self.refused.values().sum()
    }

    /// Whether every exclusive loop in the file was rewritten. An exclusive
    /// instruction left behind means the program can still retry a store.
    pub fn complete(&self) -> bool {
        self.refused_total() == 0 && self.skipped_segments.is_empty()
    }
}

fn idiom_name(idiom: Idiom) -> &'static str {
    match idiom {
        Idiom::Rmw => "rmw",
        Idiom::Swap => "swap",
        Idiom::CasCompare => "cas-compare",
        Idiom::CasZero => "cas-zero",
    }
}

fn refusal_name(reason: &Refusal) -> &'static str {
    match reason {
        Refusal::ExclusivePair => "exclusive-pair",
        Refusal::NoPairedLoad => "no-paired-load",
        Refusal::Undecoded => "undecoded-instruction",
        Refusal::NotCanonical => "not-a-canonical-loop",
        Refusal::BranchIntoSequence => "branch-into-sequence",
        Refusal::Registers(_) => "register-constraint",
        Refusal::UnsupportedOperation => "unsupported-operation",
        Refusal::BranchOutOfRange => "branch-out-of-range",
        Refusal::Relocated => "relocation-inside-the-sequence",
        Refusal::LiteralInsideSequence => "literal-inside-the-sequence",
        Refusal::AddressedData => "addressed-data-inside-the-sequence",
    }
}

fn refusal_detail(reason: &Refusal) -> String {
    match reason {
        Refusal::Registers(why) => format!("register-constraint: {why}"),
        other => refusal_name(other).to_string(),
    }
}

/// Rewrite the executable segments of `bytes` in place.
///
/// Returns `Ok(None)` when the file is not an aarch64 ELF, which is not an
/// error: a caller that was handed the wrong file should say so and run the
/// original.
pub fn rewrite_bytes(bytes: &mut [u8]) -> Result<Option<FileReport>, goblin::error::Error> {
    // A guest that is not an ELF image at all -- a `#!` script, say -- is not
    // an error: it simply has nothing to rewrite. A file that starts like an
    // ELF and then does not parse IS an error, because something is wrong
    // with a file we are about to run.
    if bytes.len() < 4 || bytes[..4] != *goblin::elf::header::ELFMAG {
        return Ok(None);
    }
    // The byte ranges relocations cover, filled inside the block below, which
    // is where the parsed ELF lives.
    let relocated: Vec<(u64, u64)>;
    // Addresses a pointer stored somewhere in the image holds, which the
    // code can load and read as data ([`stored_pointers`]).
    let pointers: Vec<u64>;
    let mut relr_unreadable = false;
    let (sections, functions) = {
        let elf = Elf::parse(bytes)?;
        if elf.header.e_machine != header::EM_AARCH64 {
            return Ok(None);
        }
        let writable_executable: Vec<(u64, u64)> = elf
            .program_headers
            .iter()
            .filter(|ph| {
                ph.p_type == program_header::PT_LOAD && ph.is_executable() && ph.is_write()
            })
            .map(|ph| (ph.p_vaddr, ph.p_vaddr.saturating_add(ph.p_memsz)))
            .collect();
        // A relocation's own width is not in the entry, so take the widest
        // aarch64 relocation (8 bytes): any sequence those bytes touch is
        // refused.
        let mut relocations: Vec<(u64, u64)> = elf
            .shdr_relocs
            .iter()
            .flat_map(|(_, relocs)| relocs.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrelas.iter().map(|reloc| reloc.r_offset))
            .chain(elf.dynrels.iter().map(|reloc| reloc.r_offset))
            .chain(elf.pltrelocs.iter().map(|reloc| reloc.r_offset))
            .map(|offset| (offset, offset + 8))
            .collect();
        // SHT_RELR = 19: packed relative relocations, which the ELF reader
        // does not report, so they are decoded here and ADDED to that list.
        // The format is its own: an even entry is an address, and each odd
        // entry after it is a bitmap of the 63 words that follow. Decoding
        // them keeps the images that use them (every current aarch64 system
        // binary) rewritable instead of refusing them whole.
        for sh in &elf.section_headers {
            if sh.sh_type != 19 || sh.sh_size == 0 {
                continue;
            }
            let Some(end) = sh
                .sh_offset
                .checked_add(sh.sh_size)
                .and_then(|end| usize::try_from(end).ok())
                .filter(|end| *end <= bytes.len())
            else {
                relr_unreadable = true;
                continue;
            };
            let start = sh.sh_offset as usize;
            relocations.extend(decode_relr(&bytes[start..end]));
        }
        relocated = relocations;
        let skip_file = |why: &str| {
            let mut report = FileReport::default();
            report.skipped_segments.push(why.into());
            Ok(Some(report))
        };
        if elf.section_headers.is_empty() {
            return skip_file("the file has no section headers, so code cannot be told from data");
        }
        let evidence = file_code_evidence(bytes, &elf);
        // Where a call never comes back ([`never_returning_targets`]).
        let never_back = never_returning_targets(bytes, &elf);
        if evidence.symbols.is_empty() && evidence.unwind.is_none() {
            return skip_file(
                "the file has neither mapping symbols ($x, $d) nor an unwind table (.eh_frame), \
                 so code cannot be told from data in its executable sections",
            );
        }
        let sections = elf
            .section_headers
            .iter()
            .enumerate()
            .filter(|(_, sh)| {
                sh.sh_type == section_header::SHT_PROGBITS
                    && sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0
                    && sh.sh_size != 0
            })
            .map(|(index, sh)| {
                let writable = writable_executable
                    .iter()
                    .any(|(start, end)| sh.sh_addr >= *start && sh.sh_addr < *end);
                let start = usize::try_from(sh.sh_offset).ok();
                let words: Option<Vec<u32>> = start
                    .zip(usize::try_from(sh.sh_size).ok())
                    .and_then(|(start, size)| bytes.get(start..start.checked_add(size)?))
                    .filter(|_| sh.sh_offset.is_multiple_of(4) && sh.sh_addr.is_multiple_of(4))
                    .map(|section| {
                        section
                            .as_chunks::<4>()
                            .0
                            .iter()
                            .map(|word| u32::from_le_bytes(*word))
                            .collect()
                    });
                let held = evidence.classify(index, sh.sh_addr, sh.sh_size);
                // Under the unwind table alone, code is what control can
                // reach from the functions' entries: a table a function
                // branches over, or keeps after its return, is not.
                let (code, unknown) = match (&words, held.evidence) {
                    (Some(words), Some(mapping::Evidence::UnwindTable)) => {
                        let entries = evidence.unwind_entries(sh.sh_addr, sh.sh_size);
                        // Which of those functions can come back, so that a
                        // call to one that cannot (an exit or abort helper)
                        // does not make the bytes after it code.
                        let bounds: Vec<(u64, u64)> = evidence
                            .unwind
                            .iter()
                            .flatten()
                            .filter(|(entry, _)| entries.contains(entry))
                            .copied()
                            .collect();
                        let never = |target: u64| never_back.contains(&target);
                        let returning =
                            mapping::returning_functions(words, sh.sh_addr, &bounds, &never);
                        // A call out of this section, to a function no FDE
                        // here describes, is taken to return: the ABI's
                        // ordinary case, unless the image names it as one
                        // that never does.
                        let returns = |target: u64| {
                            !never_back.contains(&target)
                                && (!entries.contains(&target) || returning.contains(&target))
                        };
                        let code = mapping::reachable_code(
                            words, sh.sh_addr, &held.code, &entries, &returns,
                        );
                        let unknown = complement(&code, sh.sh_addr, sh.sh_addr + sh.sh_size);
                        (code, unknown)
                    }
                    _ => (held.code, held.unknown),
                };
                Section {
                    offset: sh.sh_offset,
                    vaddr: sh.sh_addr,
                    writable,
                    words,
                    evidence: held.evidence,
                    code,
                    unknown,
                }
            })
            .collect::<Vec<_>>();
        let unwind = evidence.unwind.unwrap_or_default();
        pointers = stored_pointers(bytes, &elf, &unwind);
        (sections, unwind)
    };
    // A direct branch in one executable section can target a sequence in
    // another, so collect each section's targets before planning any. A
    // section's OWN branches are not external to it: the planner already
    // knows them, and its loops' own backward branches are among them.
    let section_targets: Vec<std::collections::HashSet<u64>> = sections
        .iter()
        .map(|section| match &section.words {
            Some(words) => rewrite::direct_branch_targets(words, section.vaddr),
            None => std::collections::HashSet::new(),
        })
        .collect();
    // The bytes every `LDR` (literal) and `ADR` in the image's code reaches,
    // sorted: a sequence among them may be a literal pool.
    // A literal or `ADR` that names a function's own entry is a pointer
    // to the function, not to data, so it does not refuse that function's
    // first sequence.
    let entry_of = |target: u64| {
        functions
            .binary_search_by_key(&target, |(entry, _)| *entry)
            .is_ok()
    };
    let mut literals: Vec<(u64, u64)> = sections
        .iter()
        .filter_map(|section| Some((section, section.words.as_ref()?)))
        .flat_map(|(section, words)| {
            section.code.iter().flat_map(move |(start, end)| {
                let first = ((start - section.vaddr) / 4) as usize;
                let last = ((end - section.vaddr) / 4) as usize;
                mapping::literal_targets(&words[first..last.min(words.len())], *start)
            })
        })
        .filter(|(target, _)| !entry_of(*target))
        .collect();
    literals.sort_unstable();
    // Where only the unwind table tells code from data, the reachability
    // above takes every call to return, so a table placed after a call that
    // does not would pass for code. Whatever the code addresses as data is
    // taken for data from there to the next instruction a branch reaches
    // (`mapping::data_extents`), and no sequence over it is rewritten.
    let extents = {
        let mut pages: Vec<u64> = Vec::new();
        let mut starts = Vec::new();
        let mut branches = Vec::new();
        for section in &sections {
            let Some(words) = &section.words else {
                continue;
            };
            for (start, end) in &section.code {
                let first = ((start - section.vaddr) / 4) as usize;
                let last = (((end - section.vaddr) / 4) as usize).min(words.len());
                let addressed = mapping::addressed_starts(&words[first..last], *start);
                starts.extend(addressed.starts);
                // An `ADRP` this reader cannot complete names only a
                // page, which the reader hands back: every function that
                // page overlaps may hold the data.
                pages.extend(addressed.unresolved);
                branches.extend(mapping::direct_branches(&words[first..last], *start));
            }
        }
        let unwind_only = |addr: &u64| {
            sections.iter().any(|section| {
                section.evidence == Some(mapping::Evidence::UnwindTable)
                    && section.words.as_ref().is_some_and(|words| {
                        (section.vaddr..section.vaddr + 4 * words.len() as u64).contains(addr)
                    })
            })
        };
        starts.extend(pointers.iter().copied());
        starts.retain(unwind_only);
        // Whole functions the unresolved pages overlap, as data: their
        // address is somewhere in the page, and which function holds it is
        // not known.
        let mut extents = mapping::data_extents(&starts, &functions, &branches);
        pages.sort_unstable();
        pages.dedup();
        for page in pages {
            // `functions` is sorted by entry, so only the ones starting
            // before the page ends can overlap it, and of those only the
            // last few reach into it.
            let before = functions.partition_point(|(entry, _)| *entry < page + PAGE);
            extents.extend(
                functions[..before]
                    .iter()
                    .rev()
                    .take_while(|(_, end)| *end > page)
                    .filter(|(entry, _)| unwind_only(entry))
                    .copied(),
            );
        }
        extents.sort_unstable();
        extents.dedup();
        extents
    };
    let mut report = FileReport::default();
    if relr_unreadable {
        report
            .skipped_segments
            .push("a packed relative relocation section is truncated".into());
        return Ok(Some(report));
    }
    for (index, section) in sections.into_iter().enumerate() {
        let Section {
            offset,
            vaddr,
            writable,
            words,
            evidence,
            code,
            unknown,
        } = section;
        let external_targets: std::collections::HashSet<u64> = section_targets
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect();
        let Some(mut words) = words else {
            report.skipped_segments.push(format!(
                "executable section at file offset {offset:#x} is truncated or unaligned"
            ));
            continue;
        };
        if writable {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} is in a writable mapping"
            ));
            continue;
        }
        // Exclusive instructions in bytes neither known code nor marked
        // data are left alone, and said to be.
        let unknown = words
            .iter()
            .enumerate()
            .filter(|(index, word)| {
                is_exclusive(**word) && in_spans(&unknown, vaddr + 4 * *index as u64)
            })
            .count();
        if evidence.is_none() {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} has no mapping symbols and the file no unwind \
                 table, so its code cannot be told from data ({unknown} exclusive instructions \
                 left alone)"
            ));
            continue;
        }
        let mut plan = rewrite::plan_with_external_targets(&words, vaddr, &external_targets);
        restrict_to_code(&mut plan, &words, vaddr, &code);
        refuse_literals(&mut plan, &literals);
        refuse_overlapping(&mut plan, &extents, Refusal::AddressedData);
        if unknown != 0 {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x}: {unknown} exclusive instructions in bytes not \
                 known to be code were left alone"
            ));
        }
        // A relocation inside a sequence means the loader will write those
        // bytes itself, for the instruction that used to be there.
        if !relocated.is_empty() {
            let (keep, relocated_sites) = plan.patched.into_iter().partition::<Vec<_>, _>(|site| {
                let end = site.addr + 4 * site.words.len() as u64;
                !relocated
                    .iter()
                    .any(|(start, stop)| *start < end && site.addr < *stop)
            });
            plan.patched = keep;
            plan.refused.extend(
                relocated_sites
                    .into_iter()
                    .map(|site| rewrite::RefusedSite {
                        addr: site.addr,
                        reason: rewrite::Refusal::Relocated,
                    }),
            );
        }
        report.exclusive_instructions += plan.exclusive_instructions;
        for site in &plan.patched {
            *report.patched.entry(idiom_name(site.idiom)).or_default() += 1;
        }
        for refused in &plan.refused {
            *report
                .refused
                .entry(refusal_name(&refused.reason))
                .or_default() += 1;
            report
                .refused_sites
                .push((refused.addr, refusal_detail(&refused.reason)));
        }
        rewrite::apply(&mut words, vaddr, &plan);
        let start = offset as usize;
        for (i, word) in words.iter().enumerate() {
            bytes[start + 4 * i..start + 4 * i + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
    Ok(Some(report))
}

/// One executable section, as [`rewrite_bytes`] plans it.
struct Section {
    /// Its file offset and address.
    offset: u64,
    vaddr: u64,
    /// Whether a writable mapping holds it.
    writable: bool,
    /// Its instruction words; `None` when it is truncated or unaligned.
    words: Option<Vec<u32>>,
    /// What its code was found by; `None` when nothing speaks for it.
    evidence: Option<mapping::Evidence>,
    /// Its code, as sorted, disjoint address ranges.
    code: Vec<(u64, u64)>,
    /// Its bytes neither known code nor marked data, likewise.
    unknown: Vec<(u64, u64)>,
}

/// The ranges of `start..end` that `spans` (sorted, disjoint) leave.
fn complement(spans: &[(u64, u64)], start: u64, end: u64) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    let mut at = start;
    for (from, to) in spans {
        if at < *from {
            gaps.push((at, *from));
        }
        at = at.max(*to);
    }
    if at < end {
        gaps.push((at, end));
    }
    gaps
}

/// Whether `addr` lies in one of `spans` (sorted, disjoint).
fn in_spans(spans: &[(u64, u64)], addr: u64) -> bool {
    let after = spans.partition_point(|(start, _)| *start <= addr);
    after.checked_sub(1).is_some_and(|i| addr < spans[i].1)
}

/// Whether `word` is a load-exclusive, a store-exclusive or an exclusive
/// pair instruction.
fn is_exclusive(word: u32) -> bool {
    matches!(
        insn::decode(word),
        insn::Insn::LoadExcl { .. } | insn::Insn::StoreExcl { .. } | insn::Insn::ExclPair
    )
}

/// The names of functions that never return to their caller: a call to one
/// of these ends its path, whatever the unwind table says of the bytes
/// after it. The C library's and C++ runtime's exits, aborts and throws.
const NEVER_RETURNS: [&str; 26] = [
    "exit",
    "_exit",
    "_Exit",
    "quick_exit",
    "abort",
    "pthread_exit",
    "longjmp",
    "_longjmp",
    "siglongjmp",
    "__longjmp_chk",
    "__assert_fail",
    "__assert_perror_fail",
    "__stack_chk_fail",
    "__fortify_fail",
    "__chk_fail",
    "err",
    "errx",
    "verr",
    "verrx",
    "__cxa_throw",
    "__cxa_rethrow",
    "__cxa_bad_cast",
    "__cxa_bad_typeid",
    "__cxa_throw_bad_array_new_length",
    "_Unwind_Resume",
    "_ZSt9terminatev",
];

/// Name prefixes of the same, for symbols whose names carry a mangled
/// path: Rust's panic entry points, in both mangling schemes.
const NEVER_RETURNS_PREFIXES: [&str; 6] = [
    "_ZN4core9panicking",
    "_ZN3std9panicking11begin_panic",
    "_ZN4core6option13expect_failed",
    "_ZN4core6result13unwrap_failed",
    "rust_panic",
    "__rust_start_panic",
];

/// Whether `name` is a function that never returns.
fn never_returns(name: &str) -> bool {
    NEVER_RETURNS.contains(&name)
        || NEVER_RETURNS_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
        // Rust's v0 mangling, whose paths carry the crate and module.
        || (name.starts_with("_R") && name.contains("9panicking"))
}

/// The addresses a call to which never comes back: the functions
/// [`never_returns`] names, wherever the image says they are. A symbol
/// defined here gives its own address; an imported one is reached through
/// its procedure linkage table entry, so each entry's `ADRP`+`LDR` of a
/// global offset table slot is matched with the `R_AARCH64_JUMP_SLOT`
/// relocation that fills that slot, which names the symbol.
///
/// A symbol this does not know is taken to return, the ordinary case; the
/// residual is an imported function that never returns and is not named
/// above.
fn never_returning_targets(bytes: &[u8], elf: &Elf<'_>) -> std::collections::HashSet<u64> {
    const R_AARCH64_JUMP_SLOT: u32 = 1026;
    let named = |name: &str| never_returns(name);
    let mut targets: std::collections::HashSet<u64> = elf
        .dynsyms
        .iter()
        .filter(|symbol| symbol.st_value != 0)
        .filter(|symbol| elf.dynstrtab.get_at(symbol.st_name).is_some_and(named))
        .map(|symbol| symbol.st_value)
        .chain(
            elf.syms
                .iter()
                .filter(|symbol| symbol.st_value != 0)
                .filter(|symbol| elf.strtab.get_at(symbol.st_name).is_some_and(named))
                .map(|symbol| symbol.st_value),
        )
        .collect();
    let slots: std::collections::HashSet<u64> = elf
        .pltrelocs
        .iter()
        .filter(|reloc| reloc.r_type == R_AARCH64_JUMP_SLOT)
        .filter(|reloc| {
            elf.dynsyms
                .get(reloc.r_sym)
                .and_then(|symbol| elf.dynstrtab.get_at(symbol.st_name))
                .is_some_and(named)
        })
        .map(|reloc| reloc.r_offset)
        .collect();
    if slots.is_empty() {
        return targets;
    }
    for sh in elf
        .section_headers
        .iter()
        .filter(|sh| sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0)
    {
        let start = sh.sh_offset as usize;
        let Some(words) = usize::try_from(sh.sh_size)
            .ok()
            .and_then(|size| bytes.get(start..start.checked_add(size)?))
            .filter(|_| sh.sh_offset.is_multiple_of(4) && sh.sh_addr.is_multiple_of(4))
            .map(|section| {
                section
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|word| u32::from_le_bytes(*word))
                    .collect::<Vec<u32>>()
            })
        else {
            continue;
        };
        for (index, word) in words.iter().enumerate() {
            // An entry starts at its `ADRP`, and the slot it loads is
            // where the pairing puts it.
            if word & 0x9F00_0000 != 0x9000_0000 {
                continue;
            }
            let at = sh.sh_addr + 4 * index as u64;
            let reach = words.len().min(index + 4);
            let addressed = mapping::addressed_starts(&words[index..reach], at);
            if addressed.starts.iter().any(|slot| slots.contains(slot)) {
                targets.insert(at);
                // With branch target identification the entry starts at a
                // `BTI c` before the `ADRP`, and that is where a call
                // lands.
                const BTI_C: u32 = 0xD503_245F;
                if index > 0 && words[index - 1] == BTI_C {
                    targets.insert(at - 4);
                }
            }
        }
    }
    targets
}

/// The addresses pointers stored in the image hold that fall strictly
/// inside one of `functions`: where a table is, when the code reads it
/// through a pointer rather than addressing it itself (an argument, a
/// global, a vtable). A function's own entry is a pointer to the function
/// and says nothing.
///
/// Three places hold such a pointer, and each is read only where it is
/// unambiguous:
///
/// - a dynamic relocation that the loader fills with an address: the addend
///   of `R_AARCH64_RELATIVE`, the symbol's value plus the addend for
///   `R_AARCH64_ABS64`, and, for the packed relative relocations
///   (`SHT_RELR`), the link-time address the image already stores at the
///   place each one names;
/// - in an executable with fixed addresses (`ET_EXEC`), whose pointers need
///   no relocation, every aligned 64-bit word of a section that is loaded
///   and not executable;
/// - a symbol that names data in an executable section (`STT_OBJECT`), in
///   either symbol table.
///
/// What this cannot see is an address the code computes from an unrelated
/// base, which no stored pointer and no instruction of the file names.
fn stored_pointers(bytes: &[u8], elf: &Elf<'_>, functions: &[(u64, u64)]) -> Vec<u64> {
    const R_AARCH64_ABS64: u32 = 257;
    const R_AARCH64_RELATIVE: u32 = 1027;
    const SHT_RELR: u32 = 19;
    const STT_OBJECT: u8 = 1;
    let inside = |addr: u64| {
        let after = functions.partition_point(|(entry, _)| *entry <= addr);
        after
            .checked_sub(1)
            .is_some_and(|i| functions[i].0 < addr && addr < functions[i].1)
    };
    let word_at = |addr: u64| -> Option<u64> {
        // The file offset of an address, through the section that holds it.
        let sh = elf.section_headers.iter().find(|sh| {
            sh.sh_addr != 0
                && sh.sh_type != section_header::SHT_NOBITS
                && (sh.sh_addr..sh.sh_addr.saturating_add(sh.sh_size)).contains(&addr)
        })?;
        let at = usize::try_from(sh.sh_offset + (addr - sh.sh_addr)).ok()?;
        bytes
            .get(at..at.checked_add(8)?)
            .map(|word| u64::from_le_bytes(word.try_into().unwrap()))
    };
    let mut pointers = Vec::new();
    let symbol_value = |index: usize| elf.dynsyms.get(index).map(|symbol| symbol.st_value);
    for reloc in elf
        .dynrelas
        .iter()
        .chain(elf.dynrels.iter())
        .chain(elf.pltrelocs.iter())
    {
        let addend = reloc.r_addend.unwrap_or(0) as u64;
        match reloc.r_type {
            R_AARCH64_RELATIVE => pointers.push(addend),
            R_AARCH64_ABS64 => {
                if let Some(value) = symbol_value(reloc.r_sym) {
                    pointers.push(value.wrapping_add(addend));
                }
            }
            _ => {}
        }
    }
    for sh in elf
        .section_headers
        .iter()
        .filter(|sh| sh.sh_type == SHT_RELR && sh.sh_size != 0)
    {
        let start = sh.sh_offset as usize;
        let Some(end) = sh
            .sh_offset
            .checked_add(sh.sh_size)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|end| *end <= bytes.len())
        else {
            continue;
        };
        // Each place a packed relocation names already holds the
        // link-time address the loader will adjust.
        pointers.extend(
            decode_relr(&bytes[start..end])
                .into_iter()
                .filter_map(|(at, _)| word_at(at)),
        );
    }
    if elf.header.e_type == goblin::elf::header::ET_EXEC {
        for sh in elf.section_headers.iter().filter(|sh| {
            sh.sh_type != section_header::SHT_NOBITS
                && sh.sh_flags & u64::from(section_header::SHF_ALLOC) != 0
                && sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) == 0
                && sh.sh_addr.is_multiple_of(8)
        }) {
            let start = sh.sh_offset as usize;
            let Some(section) = usize::try_from(sh.sh_size)
                .ok()
                .and_then(|size| bytes.get(start..start.checked_add(size)?))
            else {
                continue;
            };
            pointers.extend(
                section
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|word| u64::from_le_bytes(*word)),
            );
        }
    }
    let executable: Vec<(u64, u64)> = elf
        .section_headers
        .iter()
        .filter(|sh| sh.sh_flags & u64::from(section_header::SHF_EXECINSTR) != 0)
        .map(|sh| (sh.sh_addr, sh.sh_addr.saturating_add(sh.sh_size)))
        .collect();
    pointers.extend(
        elf.dynsyms
            .iter()
            .chain(elf.syms.iter())
            .filter(|symbol| symbol.st_info & 0xf == STT_OBJECT)
            .map(|symbol| symbol.st_value)
            .filter(|value| {
                executable
                    .iter()
                    .any(|(start, end)| (*start..*end).contains(value))
            }),
    );
    pointers.retain(|address| inside(*address));
    pointers.sort_unstable();
    pointers.dedup();
    pointers
}

/// What tells an ELF image's code from its data ([`mapping::CodeEvidence`]):
/// nothing for a file whose section header table or tables lie outside it.
fn file_code_evidence(bytes: &[u8], elf: &Elf<'_>) -> mapping::CodeEvidence {
    let slice = |(offset, size): (u64, u64)| {
        let start = usize::try_from(offset).ok()?;
        bytes.get(start..start.checked_add(usize::try_from(size).ok()?)?)
    };
    let Some(table) = slice((
        elf.header.e_shoff,
        (elf.section_headers.len() as u64).saturating_mul(64),
    )) else {
        return mapping::CodeEvidence::default();
    };
    mapping::CodeEvidence::read(
        table,
        usize::from(elf.header.e_shstrndx),
        elf.header.e_type,
        |range| slice(range).map(<[u8]>::to_vec),
    )
}

/// Refuse each rewrite whose sequence's bytes an `LDR` (literal) or `ADR`
/// in the image's code reaches (`targets`, sorted): those bytes may be a
/// literal pool. Whatever the evidence: an object linked without mapping
/// symbols after one with them inherits the earlier `$x` range, and a real
/// loop is never a literal's target.
fn refuse_literals(plan: &mut rewrite::Report, targets: &[(u64, u64)]) {
    // No target is longer than 16 bytes (`LDR Qt`).
    const LONGEST: u64 = 16;
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let end = site.addr + 4 * site.words.len() as u64;
        let from = targets.partition_point(|(start, _)| start.saturating_add(LONGEST) <= site.addr);
        let reached = targets[from..]
            .iter()
            .take_while(|(start, _)| *start < end)
            .any(|(start, stop)| *start < end && site.addr < *stop);
        if reached {
            plan.refused.push(rewrite::RefusedSite {
                addr: site.addr,
                reason: Refusal::LiteralInsideSequence,
            });
        } else {
            plan.patched.push(site);
        }
    }
}

/// The page an `ADRP` names, and so what an unresolved one bounds, less
/// what a completion could add to it.
const PAGE: u64 = 4096;

/// Refuse, for `reason`, each rewrite whose sequence overlaps one of
/// `ranges` (sorted, disjoint).
fn refuse_overlapping(plan: &mut rewrite::Report, ranges: &[(u64, u64)], reason: Refusal) {
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let end = site.addr + 4 * site.words.len() as u64;
        let after = ranges.partition_point(|(start, _)| *start < end);
        if after
            .checked_sub(1)
            .is_some_and(|i| site.addr < ranges[i].1)
        {
            plan.refused.push(rewrite::RefusedSite {
                addr: site.addr,
                reason: reason.clone(),
            });
        } else {
            plan.patched.push(site);
        }
    }
}

/// Keep of a section's plan only what is in its code, the address ranges
/// `code` (sorted, disjoint): a site is placed by its store-exclusive, as
/// the planner reports a refusal. A site whose store-exclusive is in data
/// is not an instruction and goes, a rewrite whose sequence runs out of the
/// code becomes a refusal, and only exclusive instructions in the code are
/// counted.
fn restrict_to_code(plan: &mut rewrite::Report, words: &[u32], base: u64, code: &[(u64, u64)]) {
    let span_of = |addr: u64| {
        let after = code.partition_point(|(start, _)| *start <= addr);
        after
            .checked_sub(1)
            .map(|i| code[i])
            .filter(|(_, end)| addr < *end)
    };
    let at = |index: usize| base + 4 * index as u64;
    plan.exclusive_instructions = words
        .iter()
        .enumerate()
        .filter(|(index, word)| span_of(at(*index)).is_some() && is_exclusive(**word))
        .count();
    plan.refused.retain(|site| span_of(site.addr).is_some());
    let patched = std::mem::take(&mut plan.patched);
    for site in patched {
        let first = ((site.addr - base) / 4) as usize;
        let store = (first..first + site.words.len())
            .find(|index| matches!(insn::decode(words[*index]), insn::Insn::StoreExcl { .. }))
            .map_or(site.addr, at);
        let end = site.addr + 4 * site.words.len() as u64;
        // A sub-word compare against a register is rewritten only because
        // the instruction before the loop zero-extends that register
        // (`rewrite::cas_compare`), which must then be code too.
        let needs_before = site.idiom == Idiom::CasCompare
            && matches!(
                insn::decode(words[first]),
                insn::Insn::LoadExcl {
                    size: insn::Size::B | insn::Size::H,
                    ..
                }
            )
            && matches!(
                words.get(first + 1).map(|word| insn::decode(*word)),
                Some(insn::Insn::Cmp {
                    operand: insn::Operand::Reg(_),
                    ..
                })
            );
        match span_of(store) {
            None => {}
            Some((start, stop))
                if start <= site.addr
                    && end <= stop
                    && (!needs_before || start + 4 <= site.addr) =>
            {
                plan.patched.push(site)
            }
            Some((start, stop)) if start <= site.addr && end <= stop => {
                plan.refused.push(rewrite::RefusedSite {
                    addr: store,
                    reason: Refusal::Registers("a sub-word compare against a register"),
                })
            }
            Some(_) => plan.refused.push(rewrite::RefusedSite {
                addr: store,
                reason: Refusal::NotCanonical,
            }),
        }
    }
}

/// The byte ranges a packed relative relocation section covers.
///
/// An even entry is an address and covers its own eight bytes; each odd
/// entry after it is a bitmap whose set bits name the words following that
/// address, 63 at a time.
fn decode_relr(section: &[u8]) -> Vec<(u64, u64)> {
    let mut covered = Vec::new();
    let mut next = 0u64;
    for entry in section.as_chunks::<8>().0 {
        let entry = u64::from_le_bytes(*entry);
        if entry & 1 == 0 {
            covered.push((entry, entry.saturating_add(8)));
            next = entry.saturating_add(8);
            continue;
        }
        let mut bits = entry >> 1;
        let mut at = next;
        while bits != 0 {
            if bits & 1 == 1 {
                covered.push((at, at.saturating_add(8)));
            }
            bits >>= 1;
            at = at.saturating_add(8);
        }
        next = next.saturating_add(63 * 8);
    }
    covered
}

/// Write already-rewritten bytes to `output`, with `source`'s permissions.
///
/// The file is written whole under a temporary name the filesystem
/// guarantees is unused and renamed into place, so a failed or concurrent
/// write is never observed under the final name.
pub fn write_rewritten(output: &Path, bytes: &[u8], source: &Path) -> anyhow::Result<()> {
    use std::io::Write;

    let directory = output.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".rewrite-")
        .tempfile_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    std::fs::set_permissions(temporary.path(), std::fs::metadata(source)?.permissions())?;
    temporary.persist(output).map_err(|error| error.error)?;
    Ok(())
}

/// Read `input`, rewrite it, and write the result to `output` with `input`'s
/// permissions.
pub fn rewrite_file(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> anyhow::Result<Option<FileReport>> {
    let input = input.as_ref();
    let mut bytes = std::fs::read(input)?;
    let report = match rewrite_bytes(&mut bytes)? {
        Some(report) => report,
        None => return Ok(None),
    };
    write_rewritten(output.as_ref(), &bytes, input)?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal aarch64 ELF image: one read-and-execute `PT_LOAD` covering
    /// the file, a `.text` section (index 1) holding `words` at file offset
    /// and address 0x100, and, unless `marks` is `None`, a symbol table
    /// whose mapping symbols are `marks`: (section index, word index, code).
    fn elf_image(words: &[u32], marks: Option<&[(u16, usize, bool)]>) -> Vec<u8> {
        elf_image_with(words, marks, None)
    }

    /// [`elf_image`] with, unless `functions` is `None`, an unwind table
    /// whose FDEs cover `functions`: (first word, end word) of `.text`.
    fn elf_image_with(
        words: &[u32],
        marks: Option<&[(u16, usize, bool)]>,
        functions: Option<&[(usize, usize)]>,
    ) -> Vec<u8> {
        elf_image_full(words, marks, functions, &[])
    }

    /// [`elf_image_with`] plus, when `pointers` is not empty, a loaded,
    /// non-executable `.data` section holding the address of each named
    /// word of `.text`, as a program stores a pointer to a table.
    fn elf_image_full(
        words: &[u32],
        marks: Option<&[(u16, usize, bool)]>,
        functions: Option<&[(usize, usize)]>,
        pointers: &[usize],
    ) -> Vec<u8> {
        const TEXT: usize = 0x100;
        let text_len = 4 * words.len();
        let shstrtab = b"\0.text\0.shstrtab\0.symtab\0.strtab\0.eh_frame\0";
        let strtab = b"\0$x\0$d\0abort\0";
        let symbols: Vec<[u8; 24]> = marks
            .unwrap_or_default()
            .iter()
            .map(|(section, word, code)| {
                let name = if *code { 1 } else { 4 };
                mapping::tests::symbol(name, 0, *section, (TEXT + 4 * word) as u64)
            })
            .collect();
        let shstrtab_at = TEXT + text_len;
        let strtab_at = shstrtab_at + shstrtab.len();
        let symtab_at = (strtab_at + strtab.len()).next_multiple_of(8);
        let symtab_len = 24 * (symbols.len() + 1);
        let eh_frame_at = (symtab_at + symtab_len).next_multiple_of(8);
        let eh_frame = functions.map(|functions| {
            let functions: Vec<(u64, u64)> = functions
                .iter()
                .map(|(start, end)| ((TEXT + 4 * start) as u64, (TEXT + 4 * end) as u64))
                .collect();
            mapping::tests::eh_frame(eh_frame_at as u64, &functions)
        });
        let eh_frame_len = eh_frame.as_ref().map_or(0, Vec::len);
        let data_at = (eh_frame_at + eh_frame_len).next_multiple_of(8);
        let data_len = 8 * pointers.len();
        let sections = 3
            + if marks.is_some() { 2 } else { 0 }
            + usize::from(eh_frame.is_some())
            + usize::from(!pointers.is_empty());
        let shoff = (data_at + data_len).next_multiple_of(8);
        let total = shoff + sections * 64;
        let mut bytes = vec![0u8; total];
        let put = |bytes: &mut Vec<u8>, at: usize, value: &[u8]| {
            bytes[at..at + value.len()].copy_from_slice(value)
        };
        // ELF header: ELFCLASS64, little-endian, ET_DYN, EM_AARCH64.
        put(&mut bytes, 0, b"\x7fELF\x02\x01\x01");
        put(&mut bytes, 16, &3u16.to_le_bytes());
        put(&mut bytes, 18, &183u16.to_le_bytes());
        put(&mut bytes, 20, &1u32.to_le_bytes());
        put(&mut bytes, 32, &64u64.to_le_bytes()); // e_phoff
        put(&mut bytes, 40, &(shoff as u64).to_le_bytes());
        put(&mut bytes, 52, &64u16.to_le_bytes()); // e_ehsize
        put(&mut bytes, 54, &56u16.to_le_bytes()); // e_phentsize
        put(&mut bytes, 56, &1u16.to_le_bytes()); // e_phnum
        put(&mut bytes, 58, &64u16.to_le_bytes()); // e_shentsize
        put(&mut bytes, 60, &(sections as u16).to_le_bytes()); // e_shnum
        put(&mut bytes, 62, &2u16.to_le_bytes()); // e_shstrndx
        // PT_LOAD, PF_R | PF_X, the whole file at address 0.
        put(&mut bytes, 64, &1u32.to_le_bytes());
        put(&mut bytes, 68, &5u32.to_le_bytes());
        put(&mut bytes, 64 + 32, &(total as u64).to_le_bytes()); // p_filesz
        put(&mut bytes, 64 + 40, &(total as u64).to_le_bytes()); // p_memsz
        put(&mut bytes, 64 + 48, &0x1000u64.to_le_bytes());
        for (i, word) in words.iter().enumerate() {
            put(&mut bytes, TEXT + 4 * i, &word.to_le_bytes());
        }
        put(&mut bytes, shstrtab_at, shstrtab);
        put(&mut bytes, strtab_at, strtab);
        for (i, symbol) in symbols.iter().enumerate() {
            put(&mut bytes, symtab_at + 24 * (i + 1), symbol);
        }
        if let Some(eh_frame) = &eh_frame {
            put(&mut bytes, eh_frame_at, eh_frame);
        }
        for (i, word) in pointers.iter().enumerate() {
            put(
                &mut bytes,
                data_at + 8 * i,
                &((TEXT + 4 * word) as u64).to_le_bytes(),
            );
        }
        // Section headers: null, .text (PROGBITS, ALLOC | EXECINSTR),
        // .shstrtab, and with marks .symtab (linked to 4) and .strtab.
        let header = |bytes: &mut Vec<u8>, index: usize, fields: [u64; 8]| {
            let at = shoff + 64 * index;
            let [name, kind, flags, addr, offset, size, link, entsize] = fields;
            put(bytes, at, &(name as u32).to_le_bytes());
            put(bytes, at + 4, &(kind as u32).to_le_bytes());
            put(bytes, at + 8, &flags.to_le_bytes());
            put(bytes, at + 16, &addr.to_le_bytes());
            put(bytes, at + 24, &offset.to_le_bytes());
            put(bytes, at + 32, &size.to_le_bytes());
            put(bytes, at + 40, &(link as u32).to_le_bytes());
            put(bytes, at + 48, &4u64.to_le_bytes());
            put(bytes, at + 56, &entsize.to_le_bytes());
        };
        let (text, len) = (TEXT as u64, text_len as u64);
        header(&mut bytes, 1, [1, 1, 6, text, text, len, 0, 0]);
        header(
            &mut bytes,
            2,
            [7, 3, 0, 0, shstrtab_at as u64, shstrtab.len() as u64, 0, 0],
        );
        if marks.is_some() {
            let (at, len) = (symtab_at as u64, symtab_len as u64);
            header(&mut bytes, 3, [17, 2, 0, 0, at, len, 4, 24]);
            header(
                &mut bytes,
                4,
                [25, 3, 0, 0, strtab_at as u64, strtab.len() as u64, 0, 0],
            );
        }
        if eh_frame.is_some() {
            // .eh_frame: PROGBITS, ALLOC, mapped at its file offset.
            let (at, len) = (eh_frame_at as u64, eh_frame_len as u64);
            let index = sections - 1 - usize::from(!pointers.is_empty());
            header(&mut bytes, index, [33, 1, 2, at, at, len, 0, 0]);
        }
        if !pointers.is_empty() {
            // The pointers' section: PROGBITS, ALLOC, not executable,
            // mapped at its file offset. It borrows `.eh_frame`'s name,
            // which nothing here reads.
            let (at, len) = (data_at as u64, data_len as u64);
            header(&mut bytes, sections - 1, [33, 1, 2, at, at, len, 0, 0]);
        }
        bytes
    }

    /// nop; ldaxr w8, [x0]; add w8, w8, #1; stlxr w9, w8, [x0];
    /// cbnz w9, <ldaxr>; ret
    const LOOP: [u32; 6] = [
        0xd503_201f,
        0x885f_fc08,
        0x1100_0508,
        0x8809_fc08,
        0x35ff_ffa9,
        0xd65f_03c0,
    ];

    /// A relocation section's entries must be seen, including the packed
    /// relative ones the ELF reader does not report. The check here is on
    /// the decoder itself rather than a whole synthetic ELF: an address
    /// entry names its own eight bytes, and the odd entry after it is a
    /// bitmap of the 63 words that follow.
    #[test]
    fn packed_relative_relocations_decode_to_the_bytes_they_cover() {
        let section: Vec<u8> = [0x1000u64, (0b1011 << 1) | 1]
            .iter()
            .flat_map(|entry| entry.to_le_bytes())
            .collect();
        assert_eq!(
            decode_relr(&section),
            vec![
                (0x1000, 0x1008),
                // The bitmap starts at the word after the address, and its
                // set bits are words 0, 1 and 3 from there.
                (0x1008, 0x1010),
                (0x1010, 0x1018),
                (0x1020, 0x1028),
            ]
        );
    }

    /// The image's bytes after [`rewrite_bytes`], with its report.
    fn rewritten(image: &[u8]) -> (Vec<u8>, FileReport) {
        let mut bytes = image.to_vec();
        let report = rewrite_bytes(&mut bytes)
            .unwrap()
            .expect("an aarch64 ELF image");
        (bytes, report)
    }

    /// A literal pool in `.text` whose words form a canonical retry loop,
    /// laid out as an assembler and linker emit it (`b` over four `.word`s,
    /// then an `LDR` (literal) that loads them): `$x` at the branch, `$d`
    /// at the pool, `$x` after it. The pool keeps its bytes whatever tells
    /// code from data: its `$d` mark; the unwind table alone, whose FDE
    /// covers the whole function but whose entry never reaches the pool; or a
    /// lone `$x` over everything, where the literal load refuses it. The
    /// planner by itself would take it.
    #[test]
    fn a_literal_pool_shaped_like_a_loop_keeps_its_bytes() {
        let words = [
            0x1400_0005, // b <word 5>
            0xc85f_fc64, // the pool: ldaxr x4, [x3] as a word
            0x9100_0484, // add x4, x4, #1
            0xc805_fc64, // stlxr w5, x4, [x3]
            0x35ff_ffa5, // cbnz w5, <word 1>
            0x58ff_ff80, // ldr x0, <word 1>
            0xd65f_03c0, // ret
        ];
        // What the planner alone does with these words: it rewrites the
        // pool, which is what corrupted it.
        let raw = rewrite::plan(&words, 0x100);
        assert_eq!(raw.patched.len(), 1, "{raw:?}");
        assert_eq!(raw.patched[0].addr, 0x104);

        let marked = elf_image(&words, Some(&[(1, 0, true), (1, 1, false), (1, 5, true)]));
        let (bytes, report) = rewritten(&marked);
        assert_eq!(bytes, marked, "{report:?}");
        assert_eq!(report.exclusive_instructions, 0, "{report:?}");
        assert!(report.complete(), "data is not a refusal: {report:?}");

        let stripped = elf_image_with(&words, None, Some(&[(0, words.len())]));
        let (bytes, report) = rewritten(&stripped);
        assert_eq!(bytes, stripped, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.skipped_segments,
            vec![
                "executable section at 0x100: 2 exclusive instructions in bytes not known to be \
                 code were left alone"
                    .to_string()
            ]
        );

        let unmarked = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&unmarked);
        assert_eq!(bytes, unmarked, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
    }

    /// Every refused sequence is listed, not only the first sixteen: 17
    /// lone store-exclusives give 17 sites, in address order, which is what
    /// `--list-refusals` prints.
    #[test]
    fn every_refused_loop_is_listed() {
        // stlxr w9, w8, [x0] seventeen times, then ret.
        let mut words = vec![0x8809_fc08_u32; 17];
        words.push(0xd65f_03c0);
        let image = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.refused_total(), 17, "{report:?}");
        assert_eq!(
            report
                .refused_sites
                .iter()
                .map(|(addr, _)| *addr)
                .collect::<Vec<_>>(),
            (0..17).map(|i| 0x100 + 4 * i).collect::<Vec<u64>>(),
            "{report:?}"
        );
    }

    /// A constant table in `.text` whose words form a canonical loop is
    /// left as it is when the mapping symbols mark it data, and only the
    /// code's exclusive instructions are counted. Without the `$d` mark the
    /// same table would be rewritten, which is what the marks prevent.
    #[test]
    fn a_data_table_shaped_like_a_loop_is_not_rewritten() {
        // The loop as code at 0x100, then the same six words as a table at
        // 0x118 (its cbnz targets the table's own ldaxr).
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let marked = elf_image(&words, Some(&[(1, 0, true), (1, 6, false)]));
        let (bytes, report) = rewritten(&marked);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_eq!(report.refused_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 2, "{report:?}");
        assert!(report.skipped_segments.is_empty(), "{report:?}");
        assert_eq!(
            bytes[0x118..0x130],
            marked[0x118..0x130],
            "the table is untouched"
        );
        assert_ne!(
            bytes[0x104..0x114],
            marked[0x104..0x114],
            "the code is rewritten"
        );

        let unmarked = elf_image(&words, Some(&[(1, 0, true)]));
        let (_, report) = rewritten(&unmarked);
        assert_eq!(report.patched_total(), 2, "{report:?}");
        assert_eq!(report.exclusive_instructions, 4, "{report:?}");
    }

    /// A loop whose store-exclusive is code but whose branch back is in a
    /// `$d` range is refused, not rewritten; one whose store-exclusive is in
    /// data is not reported at all.
    #[test]
    fn a_loop_that_runs_into_data_is_refused() {
        // Code ends at the cbnz (word 4): the sequence runs into data.
        let image = elf_image(&LOOP, Some(&[(1, 0, true), (1, 4, false)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("not-a-canonical-loop", 1)]),
            "{report:?}"
        );
        assert_eq!(
            report.refused_sites,
            vec![(0x10c, "not-a-canonical-loop".into())]
        );
        // Data from the stlxr (word 3) on: the store is not an instruction.
        let image = elf_image(&LOOP, Some(&[(1, 0, true), (1, 3, false)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(report.refused_total(), 0, "{report:?}");
        assert_eq!(report.exclusive_instructions, 1, "the ldaxr is code");
    }

    /// Bytes not known to be code are never rewritten: a file with neither
    /// mapping symbols nor an unwind table is left alone whole, and so is an
    /// executable section the mapping symbols do not mark in a file without
    /// an unwind table.
    #[test]
    fn code_nothing_marks_is_left_alone() {
        let image = elf_image(&LOOP, None);
        let (bytes, stripped) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(stripped.exclusive_instructions, 0);
        assert_eq!(
            stripped.skipped_segments,
            vec![
                "the file has neither mapping symbols ($x, $d) nor an unwind table (.eh_frame), so \
                 code cannot be told from data in its executable sections"
                    .to_string()
            ]
        );
        assert!(!stripped.complete());
        // The only mapping symbol marks another section (.shstrtab, 2).
        let image = elf_image(&LOOP, Some(&[(2, 0, true)]));
        let (bytes, unmarked) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(
            unmarked.skipped_segments,
            vec![
                "executable section at 0x100 has no mapping symbols and the file no unwind table, \
                 so its code cannot be told from data (2 exclusive instructions left alone)"
                    .to_string()
            ]
        );
    }

    /// Without mapping symbols, the unwind table's FDEs say what is code: a
    /// stripped file's function is rewritten, and a loop-shaped table no FDE
    /// covers is not.
    #[test]
    fn a_stripped_file_is_rewritten_where_its_unwind_table_covers_code() {
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let image = elf_image_with(&words, None, Some(&[(0, 6)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_eq!(report.exclusive_instructions, 2, "{report:?}");
        assert_ne!(
            bytes[0x104..0x114],
            image[0x104..0x114],
            "the code is rewritten"
        );
        assert_eq!(
            bytes[0x118..0x130],
            image[0x118..0x130],
            "the table is untouched"
        );
        // The table's exclusives are left alone, and said to be.
        assert_eq!(
            report.skipped_segments,
            vec![
                "executable section at 0x100: 2 exclusive instructions in bytes not known to be \
                 code were left alone"
                    .to_string()
            ]
        );
        // Mapping symbols, where a section has them, take precedence.
        let marked = elf_image_with(&words, Some(&[(1, 0, true)]), Some(&[(0, 6)]));
        assert_eq!(rewritten(&marked).1.patched_total(), 2);
    }

    /// An `LDR` (literal) that reaches a sequence's bytes makes them a
    /// possible literal pool, and it is refused, whatever the evidence: an
    /// object linked without mapping symbols after one with them inherits
    /// the earlier `$x` range.
    #[test]
    fn a_literal_load_into_a_sequence_refuses_it() {
        // The code loop (words 0..6), `ldr x1, <word 9>`, `ret`, then a
        // loop-shaped pool at words 8..14, all under one `$x`.
        let mut words: Vec<u32> = LOOP.to_vec();
        words.extend([0x5800_0061, 0xd65f_03c0]);
        words.extend(LOOP);
        let image = elf_image(&words, Some(&[(1, 0, true)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(
            bytes[0x104..0x114],
            image[0x104..0x114],
            "the code is rewritten"
        );
        assert_eq!(bytes[0x120..], image[0x120..], "the pool is untouched");
        assert_eq!(
            report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{report:?}"
        );
        assert_eq!(
            report.refused_sites,
            vec![(0x124, "literal-inside-the-sequence".into())]
        );
    }

    /// Under the unwind table alone, code is what control reaches from the
    /// functions' entries: a constant table a function addresses with `ADR`
    /// and branches over is inside its FDE but is not reached, so it is not
    /// rewritten, however its words decode.
    #[test]
    fn a_table_a_function_branches_over_is_not_code_under_the_unwind_table() {
        let words = [
            0x1000_0061, // adr x1, <the table, word 3>
            0xb940_0423, // ldr w3, [x1, #4]
            0x1400_0006, // b <word 8>
            0x1122_3344, // the table: a word, then a loop's shape
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0, // ret
        ];
        // The branch over the table as B, and as the conditional forms that
        // always branch: B.AL, CBZ WZR and TBZ WZR.
        for branch in [0x1400_0006, 0x5400_00ce, 0x3400_00df, 0x3600_00df] {
            let mut words = words;
            words[2] = branch;
            let image = elf_image_with(&words, None, Some(&[(0, words.len())]));
            let (bytes, report) = rewritten(&image);
            assert_eq!(report.patched_total(), 0, "{branch:#010x}: {report:?}");
            assert_eq!(report.exclusive_instructions, 0, "{report:?}");
            assert_eq!(bytes, image, "{branch:#010x}: the table is untouched");
        }
        // Marked as code, the same words would be rewritten: the
        // reachability is what refuses them.
        let marked = elf_image(&words, Some(&[(1, 0, true)]));
        assert_eq!(rewritten(&marked).1.patched_total(), 1);
    }

    /// Under the unwind table alone, a call is taken to return, so a table
    /// placed after a call that does not return is reached as code. What the
    /// code addresses as data is taken for data from there to the next
    /// instruction a branch reaches, so a table whose words form a loop is
    /// refused, addressed either by `ADRP`+`ADD` or by `ADR`, and the same
    /// words marked `$x` are still rewritten.
    #[test]
    fn a_table_after_a_call_that_does_not_return_is_not_rewritten() {
        // <address the table at word 3>; bl <word 9>, which traps; a header
        // word; the loop; ret; brk.
        let table = [
            0x1234_5678, // the header the code loads
            0x885f_fc08, // ldaxr w8, [x0]
            0x1100_0508, // add w8, w8, #1
            0x8809_fc08, // stlxr w9, w8, [x0]
            0x35ff_ffa9, // cbnz w9, <ldaxr>
        ];
        let mut words = [0u32; 10];
        words[2] = 0x9400_0007; // bl <word 9>
        words[3..8].copy_from_slice(&table);
        words[8] = 0xd65f_03c0; // ret
        words[9] = 0xd420_0000; // brk
        let addressings = [
            // adrp x1, 0x0; add x1, x1, #0x10c (the table at word 3).
            ([0x9000_0001, 0x9104_3021], "ADRP+ADD"),
            // adr x1, <word 3>; ldr w3, [x1, #4] (the loop's first word).
            ([0x1000_0061, 0xb940_0423], "ADR"),
        ];
        for (addressing, how) in addressings {
            let mut words = words;
            words[..2].copy_from_slice(&addressing);
            assert_eq!(
                mapping::addressed_starts(&words[..3], 0x100).starts,
                vec![0x10c],
                "{how} addresses the table"
            );
            let image = elf_image_with(&words, None, Some(&[(0, 9), (9, 10)]));
            let (bytes, report) = rewritten(&image);
            assert_eq!(bytes, image, "{how}: the table is untouched");
            assert_eq!(report.patched_total(), 0, "{how}: {report:?}");
            // The callee traps, so nothing reaches the table at all; what
            // addresses it would refuse it in any case.
            assert_eq!(report.exclusive_instructions, 0, "{how}: {report:?}");
            // Marked as code, the same words are rewritten: only the unwind
            // table's weaker evidence needs this.
            let marked = elf_image(&words, Some(&[(1, 0, true)]));
            assert_eq!(rewritten(&marked).1.patched_total(), 1, "{how}");
        }
        // Nor is a table the code does not address, now that the call to
        // a function that cannot return does not fall through.
        let mut unaddressed = words;
        unaddressed[0] = 0xd503_201f;
        unaddressed[1] = 0xd503_201f;
        let image = elf_image_with(&unaddressed, None, Some(&[(0, 9), (9, 10)]));
        assert_eq!(rewritten(&image).1.patched_total(), 0);
        // A call to a function that does return falls through as before:
        // the same file with the callee returning rewrites the loop.
        let mut returning = unaddressed;
        returning[9] = 0xd65f_03c0; // ret in place of the trap
        let image = elf_image_with(&returning, None, Some(&[(0, 9), (9, 10)]));
        assert_eq!(rewritten(&image).1.patched_total(), 1);
    }

    /// One `ADRP` whose register completes two accesses: the table after the
    /// second call is addressed only by the second, and must keep its bytes.
    /// (Found reviewing this change against the census's own reader.)
    #[test]
    fn a_table_addressed_by_a_second_use_of_one_adrp_is_not_rewritten() {
        let mut words = [0u32; 14];
        words[0] = 0x9000_0001; // adrp x1, 0x0
        words[1] = 0x9104_5022; // add x2, x1, #0x114 (a header word at word 5)
        words[2] = 0x9104_8023; // add x3, x1, #0x120 (the table at word 8)
        words[3] = 0xb400_0060; // cbz x0, <word 6>
        words[4] = 0x9400_0009; // bl <word 13>, which traps
        words[5] = 0x1234_5678; // the header
        words[6] = 0xd503_201f; // nop: code again, a branch target
        words[7] = 0x9400_0006; // bl <word 13>
        words[8..12].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[12] = 0xd65f_03c0; // ret
        words[13] = 0xd420_0000; // brk
        assert_eq!(
            mapping::addressed_starts(&words[..3], 0x100).starts,
            vec![0x114, 0x120],
            "both completions of the one ADRP"
        );
        let image = elf_image_with(&words, None, Some(&[(0, 13), (13, 14)]));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// Only a local, untyped symbol is a mapping symbol: an ordinary global
    /// label named `$x…` inside a `$d` constant table does not start a code
    /// range there, so the table keeps its bytes. A real local `$x` there
    /// does, which is the control.
    #[test]
    fn a_global_symbol_named_like_a_mapping_symbol_does_not_split_a_table() {
        // Code at 0x100, then the same words as a `$d` table at 0x118,
        // with a third mark at 0x11c, inside the table.
        let words: Vec<u32> = LOOP.iter().chain(LOOP.iter()).copied().collect();
        let inside = 0x100 + 4 * 7;
        let split = elf_image(&words, Some(&[(1, 0, true), (1, 6, false), (1, 7, true)]));
        let (bytes, report) = rewritten(&split);
        assert_eq!(
            report.patched_total(),
            2,
            "a local $x inside the table makes its loop code: {report:?}"
        );
        assert_ne!(bytes[0x11c..0x130], split[0x11c..0x130]);

        // The same file with that symbol's binding changed to STB_GLOBAL:
        // an ordinary label, so the `$d` range runs on.
        let local = mapping::tests::symbol(1, 0, 1, inside);
        let at = split
            .windows(local.len())
            .position(|window| window == local)
            .expect("the mark is in the symbol table");
        let mut global = split.clone();
        global[at..at + local.len()].copy_from_slice(&mapping::tests::symbol(1, 0x10, 1, inside));
        let (bytes, report) = rewritten(&global);
        assert_eq!(
            report.patched_total(),
            1,
            "a global $x does not split the table: {report:?}"
        );
        assert_eq!(
            bytes[0x118..0x130],
            global[0x118..0x130],
            "the table keeps its bytes"
        );
    }

    /// A table the code reaches only through a pointer stored elsewhere: no
    /// instruction of the file addresses it, so only the pointer says where
    /// the data is. In an executable with fixed addresses that pointer is a
    /// plain word of a loaded, non-executable section.
    #[test]
    fn a_table_a_stored_pointer_names_is_not_rewritten() {
        // ldr w1, [x0]; mov w2, #0; cbz w2, <word 8>; the loop-shaped
        // table; ret; brk. Nothing addresses the table.
        let mut words = [0u32; 10];
        words[0] = 0xb940_0001; // ldr w1, [x0]: through the argument
        words[1] = 0x5280_0002; // mov w2, #0
        words[2] = 0x3400_00c2; // cbz w2, <word 8>: always taken, but not
        words[3..7].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[7] = 0xd503_201f; // nop
        words[8] = 0xd65f_03c0; // ret
        words[9] = 0xd420_0000; // brk
        let functions = [(0, 9), (9, 10)];
        // Without the pointer the table is taken for code, which is the
        // limit this test's fixture otherwise shares with the others.
        let bare = elf_image_with(&words, None, Some(&functions));
        assert_eq!(rewritten(&bare).1.patched_total(), 1);

        // An ET_EXEC image storing the table's address in its data.
        let mut image = elf_image_full(&words, None, Some(&functions), &[3]);
        image[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("addressed-data-inside-the-sequence", 1)]),
            "{report:?}"
        );

        // A stored pointer to a function's entry is a pointer to the
        // function, not to data.
        let mut entry = elf_image_full(&words, None, Some(&functions), &[0]);
        entry[16..18].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(rewritten(&entry).1.patched_total(), 1);
    }

    /// A call to a function the image names as one that never returns ends
    /// its path, so a table after it is not reached. Here the callee is a
    /// local symbol named `abort`, which is what a stripped library's
    /// imported one is through its procedure linkage table.
    #[test]
    fn a_call_to_a_function_that_never_returns_does_not_fall_through() {
        // bl <word 6>; the loop-shaped table; ret; the callee, which
        // returns, so only its name keeps the table from being code.
        let mut words = [0u32; 8];
        words[0] = 0x9400_0006; // bl <word 6>
        words[1..5].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[5] = 0xd65f_03c0; // ret
        words[6] = 0xd65f_03c0; // the callee: ret
        words[7] = 0xd503_201f;
        let functions = [(0, 6), (6, 8)];
        // A mark at the callee, which the lines below turn into the
        // symbol that names it.
        let image = elf_image_with(
            &words,
            Some(&[(1, 0, true), (1, 6, true)]),
            Some(&functions),
        );
        // With mapping symbols the table is code, which is the control
        // for the fixture itself.
        assert_eq!(rewritten(&image).1.patched_total(), 1);

        // The same words with no mapping symbols: the unwind table alone
        // speaks, and the callee returns, so the table is reached.
        let unnamed = elf_image_with(&words, None, Some(&functions));
        assert_eq!(rewritten(&unnamed).1.patched_total(), 1);

        // Now name the callee `abort`: a global function symbol at its
        // address, in a file with no mapping symbols.
        let mark = mapping::tests::symbol(1, 0, 1, 0x100 + 4 * 6);
        let at = image
            .windows(mark.len())
            .position(|window| window == mark)
            .expect("the mark is in the symbol table");
        let mut named = image.clone();
        // Name index 7 is "abort" in the builder's string table; STT_FUNC
        // with a global binding, as an ordinary symbol is.
        named[at..at + mark.len()].copy_from_slice(&mapping::tests::symbol(
            7,
            0x12,
            1,
            0x100 + 4 * 6,
        ));
        // And drop the section's other mapping symbol, so only the unwind
        // table tells its code from its data.
        let text = mapping::tests::symbol(1, 0, 1, 0x100);
        let at = named
            .windows(text.len())
            .position(|window| window == text)
            .expect("the first mark is in the symbol table");
        named[at..at + text.len()].copy_from_slice(&[0u8; 24]);
        let (bytes, report) = rewritten(&named);
        assert_eq!(bytes, named, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// The names of functions that never return, as the symbol tables
    /// spell them: the C library's and the C++ runtime's, and Rust's panic
    /// entry points in both mangling schemes. An ordinary name is not one.
    #[test]
    fn the_names_that_never_return_are_recognised() {
        for name in [
            "exit",
            "abort",
            "__stack_chk_fail",
            "_Unwind_Resume",
            "_ZSt9terminatev",
            "_ZN4core9panicking9panic_fmt17h0123456789abcdefE",
            "_ZN3std9panicking11begin_panic17hfedcba9876543210E",
            "_RNvNtCs1234_4core9panicking9panic_fmt",
            "__rust_start_panic",
        ] {
            assert!(never_returns(name), "{name}");
        }
        for name in [
            "exits",
            "abort_handler",
            "my_exit",
            "_ZN4core3fmt5write17h0123456789abcdefE",
            "main",
            "",
        ] {
            assert!(!never_returns(name), "{name}");
        }
    }

    /// A literal or `ADR` that names a function's own entry is a pointer
    /// to the function, not to data, so the function's first sequence is
    /// still rewritten. A pointer into its body is data, as before.
    #[test]
    fn a_pointer_to_a_functions_entry_does_not_make_it_data() {
        // worker: the loop; its caller takes its address and calls it.
        let mut words = [0u32; 9];
        words[0..4].copy_from_slice(&[0x885f_fc08, 0x1100_0508, 0x8809_fc08, 0x35ff_ffa9]);
        words[4] = 0xd65f_03c0; // ret
        words[5] = 0x1000_0001; // adr x1, <word 0>: see below
        words[6] = 0xd63f_0020; // blr x1
        words[7] = 0xd65f_03c0; // ret
        words[8] = 0xd503_201f;
        // adr x1, 0x100 from 0x114 is -0x14.
        words[5] = 0x1000_0001 | ((((-0x14i32 >> 2) & 0x7_FFFF) as u32) << 5);
        let functions = [(0, 5), (5, 8)];
        let image = elf_image_with(&words, None, Some(&functions));
        let (bytes, report) = rewritten(&image);
        assert_eq!(report.patched_total(), 1, "{report:?}");
        assert_ne!(bytes, image);

        // The same `ADR` one word into the function names data, and the
        // sequence over it is refused.
        let mut inside = words;
        inside[5] = 0x1000_0001 | ((((-0x10i32 >> 2) & 0x7_FFFF) as u32) << 5);
        let image = elf_image_with(&inside, None, Some(&functions));
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image, "{report:?}");
        assert_eq!(report.patched_total(), 0, "{report:?}");
    }

    /// A relocatable object's FDE starts are relocations to fill, so its
    /// unwind table is no evidence.
    #[test]
    fn a_relocatable_objects_unwind_table_is_no_evidence() {
        let mut image = elf_image_with(&LOOP, None, Some(&[(0, 6)]));
        image[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        let (bytes, report) = rewritten(&image);
        assert_eq!(bytes, image);
        assert_eq!(
            report.skipped_segments,
            vec![
                "the file has neither mapping symbols ($x, $d) nor an unwind table (.eh_frame), so \
                 code cannot be told from data in its executable sections"
                    .to_string()
            ]
        );
    }

    /// A sub-word compare against a register is rewritten only after the
    /// zero-extension before it, which must be code too.
    #[test]
    fn a_sub_word_compare_needs_its_zero_extension_in_the_code() {
        // uxtb w16, w0; ldaxrb w0, [x2]; cmp w0, w16; b.ne out;
        // stlxrb w17, w1, [x2]; cbnz w17, <ldaxrb>; out: ret
        let words = [
            0x5300_1c10,
            0x085f_fc40,
            0x6b10_001f,
            0x5400_0061,
            0x0811_fc41,
            0x35ff_ff91,
            0xd65f_03c0,
        ];
        let whole = elf_image(&words, Some(&[(1, 0, true)]));
        assert_eq!(rewritten(&whole).1.patched_total(), 1);
        // The zero-extension is data: the compare is not rewritten.
        let split = elf_image(&words, Some(&[(1, 0, false), (1, 1, true)]));
        let (bytes, report) = rewritten(&split);
        assert_eq!(bytes, split);
        assert_eq!(report.patched_total(), 0, "{report:?}");
        assert_eq!(
            report.refused,
            BTreeMap::from([("register-constraint", 1)]),
            "{report:?}"
        );
    }
}
