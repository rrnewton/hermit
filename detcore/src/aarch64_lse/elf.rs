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
//! An executable section is not all instructions either: a literal pool or
//! jump table inside `.text` is data whose words can decode as a canonical
//! exclusive loop. The AArch64 mapping symbols the assembler emits (`$x`
//! for A64 code, `$d` for data) say which ranges of a section are which, so
//! only `$x` ranges are scanned. `$d` ranges, bytes before a section's
//! first mapping symbol, and a section with no mapping symbols at all are
//! left alone and reported, exactly as a headerless file is: what cannot
//! be told from data must not be rewritten as code.
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
        Refusal::NewValueAfterCompare => "new-value-after-compare",
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

/// Whether a symbol name is an AArch64 mapping symbol, and whether the
/// range it opens is A64 code. `$x` (optionally suffixed, as in `$x.1`)
/// opens code and `$d` opens data; any other `$`-symbol (`$a`, `$t`, ...)
/// opens something that is not A64 code either, so it closes a code range
/// just as `$d` does.
fn mapping_symbol_code(name: &str) -> Option<bool> {
    let suffixed = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    if suffixed("$x") {
        Some(true)
    } else if name.starts_with('$') {
        Some(false)
    } else {
        None
    }
}

/// The A64 code ranges of one section, from its mapping symbols as
/// `(address, is code)` marks in symbol-table order: each mark opens a
/// range ending at the next mark or at the section's end, and bytes
/// before the first mark belong to no range. `None` when the section
/// has no mapping symbols, so nothing in it can be told from data.
fn code_ranges(
    section_addr: u64,
    section_size: u64,
    marks: &[(u64, bool)],
) -> Option<Vec<(u64, u64)>> {
    let section_end = section_addr.saturating_add(section_size);
    let mut marks: Vec<(u64, bool)> = marks
        .iter()
        .copied()
        .filter(|(addr, _)| (section_addr..=section_end).contains(addr))
        .collect();
    if marks.is_empty() {
        return None;
    }
    // Stable, so where two marks name one address the later symbol wins.
    marks.sort_by_key(|(addr, _)| *addr);
    marks.dedup_by(|next, kept| {
        if next.0 == kept.0 {
            kept.1 = next.1;
            true
        } else {
            false
        }
    });
    let mut ranges = Vec::new();
    for (index, (addr, code)) in marks.iter().enumerate() {
        if !code {
            continue;
        }
        let end = marks
            .get(index + 1)
            .map_or(section_end, |(next, _)| *next)
            .min(section_end);
        if addr < &end {
            ranges.push((*addr, end));
        }
    }
    Some(ranges)
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
        if elf.section_headers.is_empty() {
            let mut report = FileReport::default();
            report
                .skipped_segments
                .push("the file has no section headers, so code cannot be told from data".into());
            return Ok(Some(FilePlan {
                report,
                patches: Vec::new(),
            }));
        }
        // The mapping symbols of each section, by section index, in
        // symbol-table order. They alone say which ranges of an
        // executable section are instructions and which are data in
        // code's clothing (a literal pool, a jump table).
        let mut mapping_marks: Vec<Vec<(u64, bool)>> = vec![Vec::new(); elf.section_headers.len()];
        for sym in elf.syms.iter() {
            let Some(code) = elf.strtab.get_at(sym.st_name).and_then(mapping_symbol_code) else {
                continue;
            };
            if let Some(marks) = mapping_marks.get_mut(sym.st_shndx) {
                marks.push((sym.st_value, code));
            }
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
                let ranges = code_ranges(sh.sh_addr, sh.sh_size, &mapping_marks[index]);
                (sh.sh_offset, sh.sh_size, sh.sh_addr, writable, ranges)
            })
            .collect::<Vec<_>>()
    };
    // The instruction words of one code range, and the file offset its
    // first word sits at. A range that starts off an instruction
    // boundary or runs past the file is malformed, and is preserved.
    let range_words = |offset: u64, vaddr: u64, range: (u64, u64)| -> Option<(usize, Vec<u32>)> {
        if !range.0.is_multiple_of(4) {
            return None;
        }
        let start = usize::try_from(offset.checked_add(range.0.checked_sub(vaddr)?)?).ok()?;
        let count = usize::try_from((range.1 - range.0) / 4).ok()?;
        if start.checked_add(4 * count)? > bytes.len() {
            return None;
        }
        let words: Vec<u32> = (0..count)
            .map(|i| {
                let at = start + 4 * i;
                u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            })
            .collect();
        Some((start, words))
    };
    // A direct branch in one code range can target a sequence in another,
    // so collect each range's targets before planning any. A range's OWN
    // branches are not external to it: the planner already knows them, and
    // its loops' own backward branches are among them.
    let range_targets: Vec<Vec<std::collections::HashSet<u64>>> = sections
        .iter()
        .map(|(offset, _, vaddr, _, ranges)| {
            ranges
                .as_ref()
                .map(|ranges| {
                    ranges
                        .iter()
                        .map(|range| {
                            range_words(*offset, *vaddr, *range)
                                .map_or_else(std::collections::HashSet::new, |(_, words)| {
                                    rewrite::direct_branch_targets(&words, range.0)
                                })
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect();
    let mut report = FileReport::default();
    let mut patches = Vec::new();
    if relr_unreadable {
        report
            .skipped_segments
            .push("a packed relative relocation section is truncated".into());
        return Ok(Some(FilePlan { report, patches }));
    }
    for (section, (offset, filesz, vaddr, writable, ranges)) in sections.iter().enumerate() {
        let (offset, filesz, vaddr, writable) = (*offset, *filesz, *vaddr, *writable);
        let start = offset as usize;
        let end = start.saturating_add(filesz as usize);
        if end > bytes.len() || !start.is_multiple_of(4) || !vaddr.is_multiple_of(4) {
            report.skipped_segments.push(format!(
                "executable section at file offset {offset:#x} is truncated or unaligned"
            ));
            continue;
        }
        if writable {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} is in a writable mapping"
            ));
            continue;
        }
        let Some(ranges) = ranges else {
            report.skipped_segments.push(format!(
                "executable section at {vaddr:#x} has no mapping symbols \
                 ($x/$d), so code cannot be told from data"
            ));
            continue;
        };
        for (range, range_addr) in ranges.iter().enumerate() {
            let external_targets: std::collections::HashSet<u64> = range_targets
                .iter()
                .enumerate()
                .flat_map(|(other, targets)| {
                    targets
                        .iter()
                        .enumerate()
                        .filter(move |(other_range, _)| other != section || *other_range != range)
                        .flat_map(|(_, targets)| targets.iter().copied())
                })
                .collect();
            let Some((range_start, words)) = range_words(offset, vaddr, *range_addr) else {
                report.skipped_segments.push(format!(
                    "code range at {:#x} is truncated or unaligned",
                    range_addr.0
                ));
                continue;
            };
            let vaddr = range_addr.0;
            let start = range_start;
            let mut plan = rewrite::plan_with_external_targets(&words, vaddr, &external_targets);
            // A relocation inside a sequence means the loader will write those
            // bytes itself, for the instruction that used to be there.
            if !relocated.is_empty() {
                let (keep, relocated_sites) =
                    plan.patched.into_iter().partition::<Vec<_>, _>(|site| {
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
    }
    Ok(Some(FilePlan { report, patches }))
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
    /// the file, and a `.text` section holding `words` at file offset and
    /// address 0x100. As every toolchain-emitted image does, it carries
    /// mapping symbols: `mapping` names each word index where an A64 code
    /// (`$x`, `true`) or data (`$d`, `false`) range starts, or `None` for an
    /// image with no symbol table at all.
    fn elf_with_text(words: &[u32]) -> Vec<u8> {
        elf_image(words, Some(&[(0, true)]))
    }

    fn elf_image(words: &[u32], mapping: Option<&[(usize, bool)]>) -> Vec<u8> {
        const TEXT: usize = 0x100;
        let text_len = 4 * words.len();
        // Symbol-name string table: "$x" at 1, "$d" at 4.
        let sym_names = b"\0$x\0$d\0";
        let sym_names_at = TEXT + text_len;
        let nsyms = mapping.map_or(0, |marks| 1 + marks.len());
        let symtab_at = (sym_names_at + sym_names.len()).next_multiple_of(8);
        let symtab_len = 24 * nsyms;
        let shstrtab = b"\0.text\0.symtab\0.strtab\0.shstrtab\0";
        let shstrtab_at = symtab_at + symtab_len;
        let shoff = (shstrtab_at + shstrtab.len()).next_multiple_of(8);
        let shnum = if mapping.is_some() { 5 } else { 3 };
        let total = shoff + shnum * 64;
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
        put(&mut bytes, 60, &(shnum as u16).to_le_bytes()); // e_shnum
        put(&mut bytes, 62, &((shnum - 1) as u16).to_le_bytes()); // e_shstrndx
        // PT_LOAD, PF_R | PF_X, the whole file at address 0.
        put(&mut bytes, 64, &1u32.to_le_bytes());
        put(&mut bytes, 68, &5u32.to_le_bytes());
        put(&mut bytes, 64 + 32, &(total as u64).to_le_bytes()); // p_filesz
        put(&mut bytes, 64 + 40, &(total as u64).to_le_bytes()); // p_memsz
        put(&mut bytes, 64 + 48, &0x1000u64.to_le_bytes());
        for (i, word) in words.iter().enumerate() {
            put(&mut bytes, TEXT + 4 * i, &word.to_le_bytes());
        }
        put(&mut bytes, sym_names_at, sym_names);
        if let Some(marks) = mapping {
            // Symbol 0 is the null symbol; the mapping symbols follow, all
            // local NOTYPE symbols in section 1 (.text).
            for (n, (index, code)) in marks.iter().enumerate() {
                let at = symtab_at + 24 * (n + 1);
                put(
                    &mut bytes,
                    at,
                    &if *code { 1u32 } else { 4u32 }.to_le_bytes(),
                );
                put(&mut bytes, at + 6, &1u16.to_le_bytes()); // st_shndx
                put(
                    &mut bytes,
                    at + 8,
                    &((TEXT + 4 * index) as u64).to_le_bytes(),
                );
            }
        }
        put(&mut bytes, shstrtab_at, shstrtab);
        // Section headers: null, .text (PROGBITS, ALLOC | EXECINSTR), then
        // with a symbol table .symtab and .strtab, and .shstrtab last.
        let text = shoff + 64;
        put(&mut bytes, text, &1u32.to_le_bytes());
        put(&mut bytes, text + 4, &1u32.to_le_bytes());
        put(&mut bytes, text + 8, &6u64.to_le_bytes());
        put(&mut bytes, text + 16, &(TEXT as u64).to_le_bytes());
        put(&mut bytes, text + 24, &(TEXT as u64).to_le_bytes());
        put(&mut bytes, text + 32, &(text_len as u64).to_le_bytes());
        put(&mut bytes, text + 48, &4u64.to_le_bytes());
        let mut next = shoff + 128;
        if mapping.is_some() {
            let symtab = next;
            put(&mut bytes, symtab, &7u32.to_le_bytes());
            put(&mut bytes, symtab + 4, &2u32.to_le_bytes()); // SHT_SYMTAB
            put(&mut bytes, symtab + 24, &(symtab_at as u64).to_le_bytes());
            put(&mut bytes, symtab + 32, &(symtab_len as u64).to_le_bytes());
            put(&mut bytes, symtab + 40, &3u32.to_le_bytes()); // sh_link: .strtab
            put(&mut bytes, symtab + 44, &(nsyms as u32).to_le_bytes());
            put(&mut bytes, symtab + 48, &8u64.to_le_bytes());
            put(&mut bytes, symtab + 56, &24u64.to_le_bytes());
            let strtab = symtab + 64;
            put(&mut bytes, strtab, &15u32.to_le_bytes());
            put(&mut bytes, strtab + 4, &3u32.to_le_bytes()); // SHT_STRTAB
            put(
                &mut bytes,
                strtab + 24,
                &(sym_names_at as u64).to_le_bytes(),
            );
            put(
                &mut bytes,
                strtab + 32,
                &(sym_names.len() as u64).to_le_bytes(),
            );
            put(&mut bytes, strtab + 48, &1u64.to_le_bytes());
            next = strtab + 64;
        }
        let names = next;
        put(&mut bytes, names, &23u32.to_le_bytes());
        put(&mut bytes, names + 4, &3u32.to_le_bytes());
        put(&mut bytes, names + 24, &(shstrtab_at as u64).to_le_bytes());
        put(
            &mut bytes,
            names + 32,
            &(shstrtab.len() as u64).to_le_bytes(),
        );
        put(&mut bytes, names + 48, &1u64.to_le_bytes());
        bytes
    }

    /// The plan names each rewritten sequence by file offset with its bytes
    /// before and after, those bytes are the rewriter's, and applying the
    /// plan is exactly what `rewrite_bytes` does to the file.
    #[test]
    fn the_plan_locates_each_sequence_and_matches_the_rewritten_file() {
        // nop; ldaxr w8, [x0]; add w8, w8, #1; stlxr w9, w8, [x0];
        // cbnz w9, <ldaxr>; ret
        let words = [
            0xd503_201f,
            0x885f_fc08,
            0x1100_0508,
            0x8809_fc08,
            0x35ff_ffa9,
            0xd65f_03c0,
        ];
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

    /// A literal pool in `.text` holds data, not instructions, even when
    /// its words decode as a canonical exclusive loop: the mapping
    /// symbols say which ranges are code, and data and unmarked ranges
    /// keep their bytes in both the file plan (`plan_bytes`, which the
    /// in-memory rewrite also consumes) and the rewritten file. A
    /// genuine flag-setting loop in a code range of the same section is
    /// still rewritten.
    #[test]
    fn literal_pool_data_is_preserved_and_the_code_loop_beside_it_is_rewritten() {
        // b <loop>; <pool: ldxr x4, [x3]; add x4, x4, #1; stlxr w5, x4,
        // [x3]; cbnz w5, <pool> as data>; <loop: ldxr x9, [x8];
        // subs x9, x9, #1; stlxr w10, x9, [x8]; cbnz w10, <loop>>;
        // b.ne <after>; ret
        let pool = [0xc85f_fc64, 0x9100_0484, 0xc805_fc64, 0x35ff_ffa5];
        let loop_words = [0xc85f_7d09, 0xf100_0529, 0xc80a_fd09, 0x35ff_ffaa];
        let mut words = vec![0x1400_0005];
        words.extend_from_slice(&pool);
        words.extend_from_slice(&loop_words);
        words.extend_from_slice(&[0x5400_00a1, 0xd65f_03c0]);
        let image = elf_image(&words, Some(&[(0, true), (1, false), (5, true)]));

        let plan = plan_bytes(&image).unwrap().expect("an aarch64 ELF image");
        assert_eq!(plan.report.patched_total(), 1, "{:?}", plan.report);
        assert_eq!(plan.patches.len(), 1, "{:?}", plan.patches);
        assert_eq!(
            plan.patches[0].offset, 0x114,
            "only the code loop is a patch"
        );
        assert_eq!(
            plan.report.exclusive_instructions, 2,
            "the pool's words are data, not exclusives: {:?}",
            plan.report
        );

        let mut rewritten = image.clone();
        rewrite_bytes(&mut rewritten).unwrap().unwrap();
        assert_eq!(
            &rewritten[0x104..0x114],
            &image[0x104..0x114],
            "the literal pool keeps its bytes"
        );
        assert_eq!(
            &rewritten[0x114..0x124],
            plan.patches[0].rewritten.as_slice(),
            "the genuine loop beside it is rewritten"
        );
        assert_ne!(&rewritten[0x114..0x124], &image[0x114..0x124]);

        // Without mapping symbols nothing in the section can be told
        // from data either, so the section is left alone and reported.
        let unmarked = elf_image(&words, None);
        let plan = plan_bytes(&unmarked)
            .unwrap()
            .expect("an aarch64 ELF image");
        assert_eq!(plan.patches.len(), 0, "{:?}", plan.report);
        assert!(
            plan.report
                .skipped_segments
                .iter()
                .any(|why| why.contains("mapping symbols")),
            "{:?}",
            plan.report
        );
        let mut rewritten = unmarked.clone();
        rewrite_bytes(&mut rewritten).unwrap().unwrap();
        assert_eq!(rewritten, unmarked);
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
