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
//! Within those sections, only code is rewritten ([`super::mapping`]): the
//! ranges the file's mapping symbols mark `$x`, or, in a section without
//! mapping symbols (every section of a stripped file), the functions its
//! unwind table's FDEs cover. An executable section can hold data too
//! (OpenSSL keeps its constant tables in `.text`), and a table whose words
//! happened to form a canonical loop would otherwise be rewritten,
//! corrupting it. Where only the FDEs say what is code, a sequence an `LDR`
//! (literal) or `ADR` in the section reaches is refused as well, since an
//! FDE covers its whole function and so any literal pool placed inside it.
//! A file with neither is left alone and reported: bytes not known to be
//! code are never taken for it. The branch analysis still reads every word
//! of every executable section, as before, so a word that might branch into
//! a sequence still refuses it.
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
    /// Addresses of the first few refusals, for reporting.
    pub refused_examples: Vec<(u64, String)>,
    /// Executable sections skipped entirely, with the reason.
    pub skipped_segments: Vec<String>,
}

impl FileReport {
    /// The rewritten sequences.
    pub fn patched_total(&self) -> usize {
        self.patched.values().sum()
    }

    /// The sequences left alone.
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
    }
}

fn refusal_detail(reason: &Refusal) -> String {
    match reason {
        Refusal::Registers(why) => format!("register-constraint: {why}"),
        other => refusal_name(other).to_string(),
    }
}

/// One rewritten sequence of a file: where it is, and its bytes before and
/// after. Same length, so applying it moves nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    /// File offset of the sequence's first instruction.
    pub offset: u64,
    /// The sequence as the file holds it.
    pub original: Vec<u8>,
    /// The replacement.
    pub rewritten: Vec<u8>,
}

/// What [`plan_bytes`] decided for one file: the report, and every
/// replacement it would make.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilePlan {
    /// The counts [`rewrite_bytes`] reports.
    pub report: FileReport,
    /// The replacements, in file order within each section.
    pub patches: Vec<FilePatch>,
}

/// Rewrite the executable segments of `bytes` in place.
///
/// Returns `Ok(None)` when the file is not an aarch64 ELF, which is not an
/// error: a caller that was handed the wrong file should say so and run the
/// original.
pub fn rewrite_bytes(bytes: &mut [u8]) -> Result<Option<FileReport>, goblin::error::Error> {
    let Some(plan) = plan_bytes(bytes)? else {
        return Ok(None);
    };
    for patch in &plan.patches {
        let start = patch.offset as usize;
        bytes[start..start + patch.rewritten.len()].copy_from_slice(&patch.rewritten);
    }
    Ok(Some(plan.report))
}

/// Decide the rewrite of `bytes` without applying it: the report
/// [`rewrite_bytes`] gives, and each replacement as a [`FilePatch`]. A
/// caller that applies the replacements somewhere other than the file (a
/// process's mapped pages) gets exactly the bytes a rewritten file would
/// hold.
pub fn plan_bytes(bytes: &[u8]) -> Result<Option<FilePlan>, goblin::error::Error> {
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
    let mut relr_unreadable = false;
    let sections = {
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
            Ok(Some(FilePlan {
                report,
                patches: Vec::new(),
            }))
        };
        if elf.section_headers.is_empty() {
            return skip_file("the file has no section headers, so code cannot be told from data");
        }
        let evidence = file_code_evidence(bytes, &elf);
        if evidence.symbols.is_empty() && evidence.unwind.is_none() {
            return skip_file(
                "the file has neither mapping symbols ($x, $d) nor an unwind table (.eh_frame), \
                 so code cannot be told from data in its executable sections",
            );
        }
        elf.section_headers
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
                        let code = mapping::reachable_code(
                            words,
                            sh.sh_addr,
                            &held.code,
                            &evidence.unwind_entries(sh.sh_addr, sh.sh_size),
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
            .collect::<Vec<_>>()
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
        .collect();
    literals.sort_unstable();
    let mut report = FileReport::default();
    let mut patches = Vec::new();
    if relr_unreadable {
        report
            .skipped_segments
            .push("a packed relative relocation section is truncated".into());
        return Ok(Some(FilePlan { report, patches }));
    }
    for (index, section) in sections.iter().enumerate() {
        let Section {
            offset,
            vaddr,
            writable,
            ref words,
            evidence,
            ref code,
            ref unknown,
        } = *section;
        let external_targets: std::collections::HashSet<u64> = section_targets
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .flat_map(|(_, targets)| targets.iter().copied())
            .collect();
        let Some(words) = words else {
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
                is_exclusive(**word) && in_spans(unknown, vaddr + 4 * *index as u64)
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
        let mut plan = rewrite::plan_with_external_targets(words, vaddr, &external_targets);
        restrict_to_code(&mut plan, words, vaddr, code);
        refuse_literals(&mut plan, &literals);
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
            if report.refused_examples.len() < 16 {
                report
                    .refused_examples
                    .push((refused.addr, refusal_detail(&refused.reason)));
            }
        }
        // Each site replaces exactly its own words (`rewrite::apply`), so
        // the patch is those words before and after.
        let start = offset as usize;
        for site in &plan.patched {
            let index = ((site.addr - vaddr) / 4) as usize;
            let to_bytes = |words: &[u32]| -> Vec<u8> {
                words.iter().flat_map(|word| word.to_le_bytes()).collect()
            };
            patches.push(FilePatch {
                offset: (start + 4 * index) as u64,
                original: to_bytes(&words[index..index + site.words.len()]),
                rewritten: to_bytes(&site.words),
            });
        }
    }
    Ok(Some(FilePlan { report, patches }))
}

/// One executable section, as [`plan_bytes`] plans it.
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
    let is_exclusive = |word: u32| {
        matches!(
            insn::decode(word),
            insn::Insn::LoadExcl { .. } | insn::Insn::StoreExcl { .. } | insn::Insn::ExclPair
        )
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
        const TEXT: usize = 0x100;
        let text_len = 4 * words.len();
        let shstrtab = b"\0.text\0.shstrtab\0.symtab\0.strtab\0.eh_frame\0";
        let strtab = b"\0$x\0$d\0";
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
        let sections = 3 + if marks.is_some() { 2 } else { 0 } + usize::from(eh_frame.is_some());
        let shoff = (eh_frame_at + eh_frame_len).next_multiple_of(8);
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
            header(&mut bytes, sections - 1, [33, 1, 2, at, at, len, 0, 0]);
        }
        bytes
    }

    /// [`elf_image`] whose `.text` is all code.
    fn elf_with_text(words: &[u32]) -> Vec<u8> {
        elf_image(words, Some(&[(1, 0, true)]))
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

    /// The plan names each rewritten sequence by file offset with its bytes
    /// before and after, those bytes are the rewriter's, and applying the
    /// plan is exactly what `rewrite_bytes` does to the file.
    #[test]
    fn the_plan_locates_each_sequence_and_matches_the_rewritten_file() {
        let words = LOOP;
        let image = elf_with_text(&words);
        let plan = plan_bytes(&image).unwrap().expect("an aarch64 ELF image");
        let expected = rewrite::plan(&words, 0x100);
        assert_eq!(expected.patched.len(), 1, "{expected:?}");
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.patches.len(), 1);

        let patch = &plan.patches[0];
        assert_eq!(patch.offset, 0x104, "the loop starts after the nop");
        let words_of = |bytes: &[u8]| -> Vec<u32> {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_le_bytes(*word))
                .collect()
        };
        assert_eq!(words_of(&patch.original), words[1..5].to_vec());
        assert_eq!(words_of(&patch.rewritten), expected.patched[0].words);

        let mut rewritten = image.clone();
        let report = rewrite_bytes(&mut rewritten).unwrap().unwrap();
        assert_eq!(report, plan.report);
        let mut applied = image;
        applied[0x104..0x104 + patch.rewritten.len()].copy_from_slice(&patch.rewritten);
        assert_eq!(applied, rewritten);
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
        let plan = plan_bytes(&marked).unwrap().expect("an aarch64 ELF image");
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.report.refused_total(), 0, "{:?}", plan.report);
        assert_eq!(plan.report.exclusive_instructions, 2, "{:?}", plan.report);
        assert!(plan.report.skipped_segments.is_empty(), "{:?}", plan.report);
        assert_eq!(plan.patches.len(), 1);
        assert_eq!(plan.patches[0].offset, 0x104);
        let mut rewritten = marked.clone();
        rewrite_bytes(&mut rewritten).unwrap().unwrap();
        assert_eq!(
            rewritten[0x118..0x130],
            marked[0x118..0x130],
            "the table is untouched"
        );
        assert_ne!(
            rewritten[0x104..0x114],
            marked[0x104..0x114],
            "the code is rewritten"
        );

        let unmarked = elf_image(&words, Some(&[(1, 0, true)]));
        let plan = plan_bytes(&unmarked).unwrap().unwrap();
        assert_eq!(plan.report.patched_total(), 2, "{:?}", plan.report);
        assert_eq!(plan.report.exclusive_instructions, 4, "{:?}", plan.report);
    }

    /// A loop whose store-exclusive is code but whose branch back is in a
    /// `$d` range is refused, not rewritten; one whose store-exclusive is in
    /// data is not reported at all.
    #[test]
    fn a_loop_that_runs_into_data_is_refused() {
        // Code ends at the cbnz (word 4): the sequence runs into data.
        let plan = plan_bytes(&elf_image(&LOOP, Some(&[(1, 0, true), (1, 4, false)])))
            .unwrap()
            .unwrap();
        assert_eq!(plan.report.patched_total(), 0, "{:?}", plan.report);
        assert_eq!(
            plan.report.refused,
            BTreeMap::from([("not-a-canonical-loop", 1)]),
            "{:?}",
            plan.report
        );
        assert_eq!(
            plan.report.refused_examples,
            vec![(0x10c, "not-a-canonical-loop".into())]
        );
        // Data from the stlxr (word 3) on: the store is not an instruction.
        let plan = plan_bytes(&elf_image(&LOOP, Some(&[(1, 0, true), (1, 3, false)])))
            .unwrap()
            .unwrap();
        assert_eq!(plan.report.patched_total(), 0, "{:?}", plan.report);
        assert_eq!(plan.report.refused_total(), 0, "{:?}", plan.report);
        assert_eq!(plan.report.exclusive_instructions, 1, "the ldaxr is code");
    }

    /// Bytes not known to be code are never rewritten: a file without
    /// mapping symbols (a stripped one) is left alone whole, and so is an
    /// executable section the mapping symbols do not mark.
    #[test]
    fn code_the_mapping_symbols_do_not_mark_is_left_alone() {
        let stripped = plan_bytes(&elf_image(&LOOP, None)).unwrap().unwrap();
        assert!(stripped.patches.is_empty());
        assert_eq!(stripped.report.exclusive_instructions, 0);
        assert_eq!(
            stripped.report.skipped_segments,
            vec![
                "the file has neither mapping symbols ($x, $d) nor an unwind table (.eh_frame), so \
                 code cannot be told from data in its executable sections"
                    .to_string()
            ]
        );
        assert!(!stripped.report.complete());
        // The only mapping symbol marks another section (.shstrtab, 2).
        let unmarked = plan_bytes(&elf_image(&LOOP, Some(&[(2, 0, true)])))
            .unwrap()
            .unwrap();
        assert!(unmarked.patches.is_empty());
        assert_eq!(
            unmarked.report.skipped_segments,
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
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.report.exclusive_instructions, 2, "{:?}", plan.report);
        assert_eq!(plan.patches[0].offset, 0x104);
        // The table's exclusives are left alone, and said to be.
        assert_eq!(
            plan.report.skipped_segments,
            vec![
                "executable section at 0x100: 2 exclusive instructions in bytes not known to be \
                 code were left alone"
                    .to_string()
            ]
        );
        // Mapping symbols, where a section has them, take precedence.
        let marked = elf_image_with(&words, Some(&[(1, 0, true)]), Some(&[(0, 6)]));
        assert_eq!(
            plan_bytes(&marked).unwrap().unwrap().report.patched_total(),
            2
        );
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
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.patches[0].offset, 0x104);
        assert_eq!(
            plan.report.refused,
            BTreeMap::from([("literal-inside-the-sequence", 1)]),
            "{:?}",
            plan.report
        );
        assert_eq!(
            plan.report.refused_examples,
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
            let plan = plan_bytes(&image).unwrap().unwrap();
            assert!(plan.patches.is_empty(), "{branch:#010x}: {:?}", plan.report);
            assert_eq!(plan.report.exclusive_instructions, 0, "{:?}", plan.report);
            let mut rewritten = image.clone();
            rewrite_bytes(&mut rewritten).unwrap().unwrap();
            assert_eq!(rewritten, image, "{branch:#010x}: the table is untouched");
        }
        // Marked as code, the same words would be rewritten: the
        // reachability is what refuses them.
        let marked = elf_image(&words, Some(&[(1, 0, true)]));
        assert_eq!(
            plan_bytes(&marked).unwrap().unwrap().report.patched_total(),
            1
        );
    }

    /// A relocatable object's FDE starts are relocations to fill, so its
    /// unwind table is no evidence.
    #[test]
    fn a_relocatable_objects_unwind_table_is_no_evidence() {
        let mut image = elf_image_with(&LOOP, None, Some(&[(0, 6)]));
        image[16..18].copy_from_slice(&1u16.to_le_bytes()); // ET_REL
        let plan = plan_bytes(&image).unwrap().unwrap();
        assert!(plan.patches.is_empty());
        assert_eq!(
            plan.report.skipped_segments,
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
        assert_eq!(
            plan_bytes(&whole).unwrap().unwrap().report.patched_total(),
            1
        );
        // The zero-extension is data: the compare is not rewritten.
        let split = elf_image(&words, Some(&[(1, 0, false), (1, 1, true)]));
        let plan = plan_bytes(&split).unwrap().unwrap();
        assert_eq!(plan.report.patched_total(), 0, "{:?}", plan.report);
        assert_eq!(
            plan.report.refused,
            BTreeMap::from([("register-constraint", 1)]),
            "{:?}",
            plan.report
        );
    }

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
}
