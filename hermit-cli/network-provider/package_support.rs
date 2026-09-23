/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

//! Offline input and ELF preservation checks. These never issue BPF syscalls.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const MAX_ARTIFACT: usize = 1024 * 1024;
pub const MAX_INPUT: usize = 16 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let stat = file.metadata()?;
    ensure!(
        stat.is_file() && stat.len() <= limit as u64,
        "input is not a bounded regular file: {}",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "input grew beyond byte bound: {}",
        path.display()
    );
    Ok(bytes)
}

fn bytes(raw: &[u8], at: usize, count: usize) -> Result<&[u8]> {
    raw.get(at..at.checked_add(count).context("byte range overflow")?)
        .context("truncated binary input")
}

fn u16_at(raw: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(bytes(raw, at, 2)?.try_into()?))
}

fn u32_at(raw: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes(raw, at, 4)?.try_into()?))
}

fn u64_at(raw: &[u8], at: usize) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes(raw, at, 8)?.try_into()?))
}

fn terminated(raw: &[u8], at: usize) -> Result<&str> {
    let rest = raw.get(at..).context("string offset outside table")?;
    let end = rest
        .iter()
        .position(|v| *v == 0)
        .context("unterminated string")?;
    Ok(std::str::from_utf8(&rest[..end])?)
}

/// Payload bytes alone do not bound decoded names: different offsets can
/// name arbitrarily many long suffixes of one string table. Reserve every
/// owned string/representation copy before allocating it. The budget is not
/// replenished when temporary values are dropped.
struct StringBudget {
    limit: usize,
    used: usize,
}

impl StringBudget {
    fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    fn reserve(&mut self, lengths: &[usize]) -> Result<()> {
        let next = lengths.iter().try_fold(self.used, |used, length| {
            used.checked_add(*length)
                .context("decoded representation byte count overflow")
        })?;
        ensure!(
            next <= self.limit,
            "decoded representations exceed the byte bound"
        );
        self.used = next;
        Ok(())
    }

    fn copy(&mut self, text: &str) -> Result<String> {
        self.reserve(&[text.len()])?;
        Ok(text.to_owned())
    }

    fn prefixed(&mut self, prefix: &str, text: &str) -> Result<String> {
        self.reserve(&[prefix.len(), text.len()])?;
        let mut result = String::with_capacity(prefix.len() + text.len());
        result.push_str(prefix);
        result.push_str(text);
        Ok(result)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub schema: u32,
    pub abi_version: String,
    pub btf_sha256: String,
    pub maps: usize,
    pub programs: usize,
    pub links: usize,
    pub source_files: Vec<String>,
    pub hooks: BTreeMap<String, (Vec<usize>, usize, usize)>,
}

impl Contract {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let result: Self = serde_json::from_slice(raw)?;
        ensure!(
            result.schema == 1
                && result.maps > 0
                && result.programs > 0
                && result.links == result.programs,
            "unsupported provider contract"
        );
        ensure!(
            result.btf_sha256.len() == 64
                && result
                    .btf_sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "malformed BTF digest"
        );
        ensure!(
            !result.hooks.is_empty() && !result.source_files.is_empty(),
            "empty provider contract"
        );
        let mut seen = BTreeSet::new();
        for name in &result.source_files {
            ensure!(
                Path::new(name)
                    .components()
                    .all(|v| matches!(v, std::path::Component::Normal(_)))
                    && seen.insert(name),
                "invalid or repeated provider source path"
            );
        }
        Ok(result)
    }

    pub fn require_btf(&self, raw: &[u8]) -> Result<Value> {
        ensure!(
            digest(raw) == self.btf_sha256,
            "running BTF is not the reviewed provider kernel; matching CO-RE fields do not qualify another attach ABI"
        );
        Ok(json!({"btf_sha256":self.btf_sha256,"functions":check_btf(raw, &self.hooks)?}))
    }
}

struct BtfType {
    name: String,
    kind: u32,
    size: u32,
    payload: Vec<u8>,
}

fn type_width(types: &[BtfType], mut id: usize) -> Result<usize> {
    let mut seen = BTreeSet::new();
    loop {
        if id == 0 {
            return Ok(0);
        }
        ensure!(seen.insert(id), "BTF type cycle");
        let typ = types.get(id - 1).context("BTF type ID outside table")?;
        match typ.kind {
            8 | 9 | 10 | 11 | 18 => id = typ.size as usize,
            2 => return Ok(8),
            1 | 4 | 6 | 19 if typ.size > 0 && typ.size <= 16 => return Ok(typ.size as usize),
            _ => bail!("unsupported BTF argument or return type"),
        }
    }
}

fn check_btf(raw: &[u8], expected: &BTreeMap<String, (Vec<usize>, usize, usize)>) -> Result<Value> {
    ensure!((24..=MAX_INPUT).contains(&raw.len()), "BTF byte bound");
    ensure!(
        u16_at(raw, 0)? == 0xeb9f && raw[2] == 1 && raw[3] == 0,
        "unsupported BTF header"
    );
    let header = u32_at(raw, 4)? as usize;
    ensure!(header >= 24, "short BTF header");
    let type_start = header
        .checked_add(u32_at(raw, 8)? as usize)
        .context("BTF type overflow")?;
    let type_bytes = bytes(raw, type_start, u32_at(raw, 12)? as usize)?;
    let str_start = header
        .checked_add(u32_at(raw, 16)? as usize)
        .context("BTF string overflow")?;
    let strings = bytes(raw, str_start, u32_at(raw, 20)? as usize)?;
    let mut at = 0;
    let mut strings_budget = StringBudget::new(MAX_INPUT);
    let mut types = Vec::new();
    while at < type_bytes.len() {
        let name = strings_budget.copy(terminated(strings, u32_at(type_bytes, at)? as usize)?)?;
        let info = u32_at(type_bytes, at + 4)?;
        let size = u32_at(type_bytes, at + 8)?;
        let kind = (info >> 24) & 31;
        let count = (info & 65535) as usize;
        at += 12;
        let length = match kind {
            1 | 14 | 17 => 4,
            2 | 7 | 8 | 9 | 10 | 11 | 12 | 16 | 18 => 0,
            3 => 12,
            4 | 5 | 15 | 19 => 12 * count,
            6 | 13 => 8 * count,
            _ => bail!("unsupported BTF payload kind {kind}"),
        };
        types.push(BtfType {
            name,
            kind,
            size,
            payload: bytes(type_bytes, at, length)?.to_vec(),
        });
        at += length;
    }
    let mut found = BTreeMap::new();
    for typ in &types {
        if typ.kind != 12 || !expected.contains_key(&typ.name) {
            continue;
        }
        let prototype = types
            .get(
                (typ.size as usize)
                    .checked_sub(1)
                    .context("zero BTF function prototype")?,
            )
            .context("missing BTF function prototype")?;
        ensure!(
            prototype.kind == 13 && !found.contains_key(&typ.name),
            "duplicate or malformed BTF function"
        );
        let widths = prototype
            .payload
            .chunks_exact(8)
            .map(|parameter| type_width(&types, u32_at(parameter, 4)? as usize))
            .collect::<Result<Vec<_>>>()?;
        let slots = widths.iter().map(|n| n.div_ceil(8)).sum::<usize>();
        let result = type_width(&types, prototype.size as usize)?;
        ensure!(
            (&widths, slots, result)
                == (
                    &expected[&typ.name].0,
                    expected[&typ.name].1,
                    expected[&typ.name].2
                ),
            "unqualified hook layout: {}",
            typ.name
        );
        found.insert(
            strings_budget.copy(&typ.name)?,
            json!({"argument_bytes":widths,"return_slot":slots,"return_bytes":result}),
        );
    }
    ensure!(
        found.keys().eq(expected.keys()),
        "required BTF function absent"
    );
    for name in found.keys() {
        strings_budget.reserve(&[name.len()])?;
    }
    Ok(serde_json::to_value(found)?)
}

#[derive(Clone)]
struct Section {
    name: String,
    kind: u32,
    flags: u64,
    address: u64,
    data: Vec<u8>,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entry: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Symbol(String, u8, u8, String, u64, u64);

impl Symbol {
    fn bounded_clone(&self, budget: &mut StringBudget) -> Result<Self> {
        budget.reserve(&[self.0.len(), self.3.len()])?;
        Ok(self.clone())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Relocation(String, u64, u32, Symbol, Vec<u8>);

struct Elf {
    header: Vec<u8>,
    sections: Vec<Section>,
    named: BTreeMap<String, usize>,
    symbols: BTreeMap<usize, Vec<Symbol>>,
}

impl Elf {
    fn parse(raw: &[u8]) -> Result<Self> {
        Self::parse_with_string_limit(raw, MAX_INPUT)
    }

    fn parse_with_string_limit(raw: &[u8], limit: usize) -> Result<Self> {
        let mut strings_budget = StringBudget::new(limit);
        ensure!(raw.len() <= MAX_INPUT, "ELF input byte bound");
        ensure!(
            raw.len() >= 64
                && bytes(raw, 0, 6)? == b"\x7fELF\x02\x01"
                && u16_at(raw, 16)? == 1
                && u16_at(raw, 18)? == 247
                && u32_at(raw, 20)? == 1,
            "only ELF64 little-endian BPF REL supported"
        );
        ensure!(
            u64_at(raw, 32)? == 0 && u16_at(raw, 56)? == 0 && u16_at(raw, 58)? == 64,
            "program headers or unsupported section layout"
        );
        let count = u16_at(raw, 60)? as usize;
        let names_index = u16_at(raw, 62)? as usize;
        ensure!(
            count > 0 && names_index < count,
            "invalid ELF section population"
        );
        let start = usize::try_from(u64_at(raw, 40)?)?;
        let headers = bytes(raw, start, 64 * count)?;
        let mut sections = Vec::new();
        let mut name_offsets = Vec::new();
        let mut copied_bytes = 0usize;
        for row in headers.chunks_exact(64) {
            let kind = u32_at(row, 4)?;
            let size = u64_at(row, 32)?;
            let data = if kind == 8 {
                Vec::new()
            } else {
                let size = usize::try_from(size)?;
                copied_bytes = copied_bytes
                    .checked_add(size)
                    .context("aggregate ELF section bytes overflow")?;
                ensure!(
                    copied_bytes <= MAX_INPUT,
                    "aggregate ELF section bytes exceed the input byte bound"
                );
                bytes(raw, usize::try_from(u64_at(row, 24)?)?, size)?.to_vec()
            };
            name_offsets.push(u32_at(row, 0)? as usize);
            sections.push(Section {
                name: String::new(),
                kind,
                flags: u64_at(row, 8)?,
                address: u64_at(row, 16)?,
                data,
                size,
                link: u32_at(row, 40)?,
                info: u32_at(row, 44)?,
                align: u64_at(row, 48)?,
                entry: u64_at(row, 56)?,
            });
        }
        ensure!(
            sections[names_index].kind == 3,
            "section names are not a string table"
        );
        strings_budget.reserve(&[sections[names_index].data.len()])?;
        let names = sections[names_index].data.clone();
        let mut named = BTreeMap::new();
        for (index, section) in sections.iter_mut().enumerate() {
            section.name = strings_budget.copy(terminated(&names, name_offsets[index])?)?;
            ensure!(
                named
                    .insert(strings_budget.copy(&section.name)?, index)
                    .is_none(),
                "duplicate ELF section name"
            );
        }
        let mut symbols = BTreeMap::new();
        for (index, section) in sections.iter().enumerate().filter(|(_, s)| s.kind == 2) {
            ensure!(
                section.entry == 24 && section.data.len() % 24 == 0,
                "malformed ELF symbol table"
            );
            let strings = sections
                .get(section.link as usize)
                .context("symbol table link outside sections")?;
            ensure!(strings.kind == 3, "symbol names do not link a string table");
            let mut table = Vec::new();
            for row in section.data.chunks_exact(24) {
                let target = u16_at(row, 6)?;
                let target = if target >= 0xff00 {
                    // u16 decimal is at most five bytes; reserve the exact
                    // formatted length before allocating either representation.
                    let digits = if target == 0 {
                        1
                    } else {
                        target.ilog10() as usize + 1
                    };
                    strings_budget.reserve(&["special:".len(), digits])?;
                    format!("special:{target}")
                } else {
                    strings_budget.prefixed(
                        "section:",
                        &sections
                            .get(target as usize)
                            .context("symbol section outside table")?
                            .name,
                    )?
                };
                table.push(Symbol(
                    strings_budget.copy(terminated(&strings.data, u32_at(row, 0)? as usize)?)?,
                    row[4],
                    row[5],
                    target,
                    u64_at(row, 8)?,
                    u64_at(row, 16)?,
                ));
            }
            ensure!(
                section.info > 0 && section.info as usize <= table.len(),
                "invalid first global symbol"
            );
            ensure!(
                table
                    .iter()
                    .enumerate()
                    .all(|(i, s)| (s.1 >> 4 != 0) == (i >= section.info as usize)),
                "symbol binding/index split inconsistent"
            );
            symbols.insert(index, table);
        }
        let mut header = raw[..32].to_vec();
        header.extend_from_slice(&raw[48..54]);
        Ok(Self {
            header,
            sections,
            named,
            symbols,
        })
    }

    fn section(&self, index: u32) -> Result<&Section> {
        self.sections
            .get(index as usize)
            .context("section index outside table")
    }

    fn require_debug_relocation(&self, section: &Section) -> Result<()> {
        let target = self.section(section.info)?;
        ensure!(
            section.kind == 9
                && section.entry == 16
                && section.flags & (2 | 4) == 0
                && self.section(section.link)?.kind == 2
                && target.kind == 1
                && target.flags & (2 | 4) == 0
                && target.name.starts_with(".debug_")
                && section.name.strip_prefix(".rel") == Some(target.name.as_str()),
            "compressed relocation is not a non-ALLOC DWARF-target REL section"
        );
        Ok(())
    }

    fn symbols_for(&self, section: &Section) -> Result<&[Symbol]> {
        Ok(self
            .symbols
            .get(&(section.link as usize))
            .context("metadata does not link a symbol table")?)
    }

    fn relocations(&self, section: &Section, budget: &mut StringBudget) -> Result<Vec<Relocation>> {
        let symbols = self.symbols_for(section)?;
        let target = self.section(section.info)?;
        let width = if section.kind == 9 { 16 } else { 24 };
        ensure!(
            section.entry == width as u64 && section.data.len() % width == 0,
            "malformed relocation table"
        );
        let mut result = Vec::new();
        for row in section.data.chunks_exact(width) {
            let offset = u64_at(row, 0)?;
            let info = u64_at(row, 8)?;
            let kind = info as u32;
            let symbol = symbols
                .get((info >> 32) as usize)
                .context("relocation symbol outside table")?
                .bounded_clone(budget)?;
            let at = usize::try_from(offset)?;
            let addend = if section.kind == 4 {
                bytes(row, 16, 8)?.to_vec()
            } else {
                match kind {
                    1 => [
                        bytes(&target.data, at, 16)?[4..8].to_vec(),
                        target.data[at + 12..at + 16].to_vec(),
                    ]
                    .concat(),
                    2 => bytes(&target.data, at, 8)?.to_vec(),
                    3 | 4 => bytes(&target.data, at, 4)?.to_vec(),
                    10 => bytes(&target.data, at, 8)?[4..8].to_vec(),
                    _ => bail!("unsupported BPF relocation type {kind}"),
                }
            };
            result.push(Relocation(
                budget.copy(&target.name)?,
                offset,
                kind,
                symbol,
                addend,
            ));
        }
        Ok(result)
    }

    fn addrsig(&self, section: &Section, budget: &mut StringBudget) -> Result<Vec<Symbol>> {
        let symbols = self.symbols_for(section)?;
        let mut result = Vec::new();
        let (mut current, mut shift) = (0u64, 0);
        for b in &section.data {
            ensure!(shift < 63 || b & 127 <= 1, "oversized addrsig ULEB");
            current |= u64::from(b & 127) << shift;
            if b & 128 != 0 {
                shift += 7;
                ensure!(shift <= 63, "oversized addrsig ULEB");
            } else {
                result.push(
                    symbols
                        .get(usize::try_from(current)?)
                        .context("addrsig symbol outside table")?
                        .bounded_clone(budget)?,
                );
                current = 0;
                shift = 0;
            }
        }
        ensure!(shift == 0, "truncated addrsig ULEB");
        Ok(result)
    }
}

/// Compressing DWARF may repack metadata indices, but may not drop any section,
/// symbol, relocation or address-significance entry. Every logical debug byte
/// and its alignment is checked alongside unchanged BPF/BTF bytes.
pub fn prove_compression(before: &[u8], after: &[u8]) -> Result<Value> {
    let old = Elf::parse(before)?;
    let mut new = Elf::parse(after)?;
    ensure!(
        old.header == new.header && old.named.keys().eq(new.named.keys()),
        "complete ELF population/header changed"
    );
    for target in &new.sections {
        if target.flags & 0x800 != 0 {
            let prior = &old.sections[old.named[&target.name]];
            if prior.kind == 9 {
                old.require_debug_relocation(prior)?;
                new.require_debug_relocation(target)?;
            }
        }
    }
    let mut strings_budget = StringBudget::new(MAX_INPUT);
    let mut compressed = Vec::new();
    for target in &mut new.sections {
        let prior = &old.sections[old.named[&target.name]];
        if target.flags & 0x800 == 0 {
            continue;
        }
        if prior.kind == 9 {
            // Both original and output relation were checked before mutation.
            ensure!(
                target.kind == 9 && prior.flags & 0x800 == 0,
                "debug relocation type or prior compression changed"
            );
        } else {
            ensure!(
                target.name.starts_with(".debug")
                    && prior.flags & 0x800 == 0
                    && prior.kind == 1
                    && target.kind == 1,
                "only uncompressed DWARF PROGBITS may be compressed"
            );
        }
        ensure!(
            prior.flags & (2 | 4) == 0,
            "allocated or executable section cannot be debug-compressed"
        );
        ensure!(
            prior.size <= MAX_INPUT as u64,
            "uncompressed DWARF exceeds the input byte bound"
        );
        ensure!(
            target.flags == prior.flags | 0x800 && target.align == 8,
            "malformed compressed ELF flags/alignment"
        );
        ensure!(
            u32_at(&target.data, 0)? == 1
                && u32_at(&target.data, 4)? == 0
                && u64_at(&target.data, 8)? == prior.size
                && u64_at(&target.data, 16)? == prior.align,
            "zlib header changed uncompressed length/alignment"
        );
        let input = target.data.get(24..).context("short compression header")?;
        let mut expanded = vec![
            0;
            usize::try_from(prior.size)?
                .checked_add(1)
                .context("debug size overflow")?
        ];
        let mut decoder = Decompress::new(true);
        let status = decoder.decompress(input, &mut expanded, FlushDecompress::Finish)?;
        ensure!(
            status == Status::StreamEnd
                && decoder.total_in() == input.len() as u64
                && decoder.total_out() == prior.size,
            "zlib stream has trailing, incomplete, or oversized data"
        );
        expanded.truncate(usize::try_from(prior.size)?);
        ensure!(expanded == prior.data, "decompressed DWARF bytes changed");
        strings_budget.reserve(&[target.name.len()])?;
        compressed.push(json!({"section":target.name,"before_bytes":prior.size,"compressed_bytes":target.size,"original_alignment":prior.align,"uncompressed_sha256":digest(&expanded)}));
        target.data = expanded;
        target.size = prior.size;
        target.flags = prior.flags;
        target.align = prior.align;
    }
    ensure!(!compressed.is_empty(), "no actual DWARF compression");
    let mut evidence = Vec::new();
    for (name, index) in &old.named {
        let a = &old.sections[*index];
        let b = &new.sections[new.named[name]];
        ensure!(
            (a.kind, a.flags, a.address, a.align, a.entry)
                == (b.kind, b.flags, b.address, b.align, b.entry)
                && old.section(a.link)?.name == new.section(b.link)?.name,
            "section metadata/link changed: {name}"
        );
        let detail = match a.kind {
            2 => {
                let mut left = old.symbols[index]
                    .iter()
                    .map(|symbol| symbol.bounded_clone(&mut strings_budget))
                    .collect::<Result<Vec<_>>>()?;
                let mut right = new.symbols[&new.named[name]]
                    .iter()
                    .map(|symbol| symbol.bounded_clone(&mut strings_budget))
                    .collect::<Result<Vec<_>>>()?;
                left.sort();
                right.sort();
                ensure!(left == right, "complete symbol population/meaning changed");
                json!({"full_resolved_symbols":right.len()})
            }
            4 | 9 => {
                let left = old.relocations(a, &mut strings_budget)?;
                let right = new.relocations(b, &mut strings_budget)?;
                ensure!(
                    left == right,
                    "relocation target/offset/type/symbol/addend changed: {name}"
                );
                json!({"full_resolved_relocations":right.len()})
            }
            0x6fff4c03 => {
                let left = old.addrsig(a, &mut strings_budget)?;
                let right = new.addrsig(b, &mut strings_budget)?;
                ensure!(left == right, "complete addrsig meaning changed");
                json!({"full_resolved_addrsig_symbols":right.len()})
            }
            3 if a.flags & 2 == 0 => {
                json!({"all_string_consumers":"complete section/symbol/relocation names exact"})
            }
            0 | 1 | 3 | 8 => {
                ensure!(
                    a.data == b.data && a.size == b.size && a.info == b.info,
                    "section logical bytes changed: {name}"
                );
                json!({"logical_bytes":b.size,"sha256":digest(&b.data)})
            }
            kind => bail!("unsupported runtime section kind {kind}: {name}"),
        };
        strings_budget.reserve(&[name.len()])?;
        evidence.push(json!({"section":name,"proof":detail}));
    }
    for required in [
        ".BTF",
        ".BTF.ext",
        ".maps",
        ".symtab",
        ".llvm_addrsig",
        "license",
    ] {
        ensure!(
            new.named.contains_key(required),
            "required retained section absent: {required}"
        );
    }
    for row in compressed.iter().chain(&evidence) {
        strings_budget.reserve(&[row["section"].as_str().context("proof section name")?.len()])?;
    }
    Ok(
        json!({"passed":true,"before_bytes":before.len(),"after_bytes":after.len(),"full_section_count":new.sections.len(),"compressed":compressed,"sections":evidence}),
    )
}

pub fn validate_elf(raw: &[u8], machine: u16, kind: u16) -> Result<()> {
    ensure!(
        raw.len() >= 64
            && bytes(raw, 0, 6)? == b"\x7fELF\x02\x01"
            && u16_at(raw, 18)? == machine
            && u16_at(raw, 16)? == kind,
        "unexpected artifact ELF type or architecture"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_elf(names: &[u8], extra: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let count = 2 + extra.len();
        let table_end = 64 + count * 64;
        let mut raw = vec![0; table_end];
        raw[..6].copy_from_slice(b"\x7fELF\x02\x01");
        raw[16..18].copy_from_slice(&1u16.to_le_bytes());
        raw[18..20].copy_from_slice(&247u16.to_le_bytes());
        raw[20..24].copy_from_slice(&1u32.to_le_bytes());
        raw[40..48].copy_from_slice(&64u64.to_le_bytes());
        raw[58..60].copy_from_slice(&64u16.to_le_bytes());
        raw[60..62].copy_from_slice(&(count as u16).to_le_bytes());
        raw[62..64].copy_from_slice(&1u16.to_le_bytes());
        for (index, (name, kind, data)) in std::iter::once((1, 3, names.to_vec()))
            .chain(extra.iter().cloned())
            .enumerate()
        {
            let row = 64 + (index + 1) * 64;
            let offset = raw.len();
            raw[row..row + 4].copy_from_slice(&name.to_le_bytes());
            raw[row + 4..row + 8].copy_from_slice(&kind.to_le_bytes());
            raw[row + 24..row + 32].copy_from_slice(&(offset as u64).to_le_bytes());
            raw[row + 32..row + 40].copy_from_slice(&(data.len() as u64).to_le_bytes());
            raw.extend_from_slice(&data);
        }
        raw
    }

    #[test]
    fn distinct_nobits_suffix_names_share_one_decoded_budget() {
        let names = b"\0.names\0abcdefgh\0";
        let raw = small_elf(names, &[(8, 8, vec![]), (9, 8, vec![]), (10, 8, vec![])]);
        // One name-table snapshot, then both the Section and BTreeMap owners.
        let exact = names.len() + 2 * (6 + 8 + 7 + 6);
        assert_eq!(
            Elf::parse_with_string_limit(&raw, exact)
                .unwrap()
                .sections
                .len(),
            5
        );
        assert_eq!(
            Elf::parse_with_string_limit(&raw, exact - 1)
                .err()
                .unwrap()
                .to_string(),
            "decoded representations exceed the byte bound"
        );
    }

    #[test]
    fn symbol_names_and_formatted_section_targets_share_the_budget() {
        let names = b"\0.names\0target\0.symtab\0abcdefgh\0";
        let mut symbols = vec![0; 48];
        symbols[24..28].copy_from_slice(&23u32.to_le_bytes());
        symbols[28] = 0x10;
        symbols[30..32].copy_from_slice(&2u16.to_le_bytes());
        let mut raw = small_elf(names, &[(8, 8, vec![]), (15, 2, symbols)]);
        let row = 64 + 3 * 64;
        raw[row + 40..row + 44].copy_from_slice(&1u32.to_le_bytes());
        raw[row + 44..row + 48].copy_from_slice(&1u32.to_le_bytes());
        raw[row + 56..row + 64].copy_from_slice(&24u64.to_le_bytes());
        let exact = names.len() + 2 * (6 + 6 + 7) + 8 + 14 + 8;
        let elf = Elf::parse_with_string_limit(&raw, exact).unwrap();
        assert_eq!(elf.symbols[&3][1].0, "abcdefgh");
        assert_eq!(elf.symbols[&3][1].3, "section:target");
        assert_eq!(
            Elf::parse_with_string_limit(&raw, exact - 1)
                .err()
                .unwrap()
                .to_string(),
            "decoded representations exceed the byte bound"
        );
    }

    #[test]
    fn representation_copies_check_complete_length_before_allocating() {
        let symbol = Symbol("long-name".into(), 0, 0, "section:target".into(), 0, 0);
        let exact = symbol.0.len() + symbol.3.len();
        let mut budget = StringBudget::new(2 * exact);
        assert_eq!(symbol.bounded_clone(&mut budget).unwrap(), symbol);
        assert_eq!(symbol.bounded_clone(&mut budget).unwrap(), symbol);
        assert!(symbol.bounded_clone(&mut budget).is_err());
        assert_eq!(budget.used, 2 * exact);
        assert!(budget.reserve(&[usize::MAX]).is_err());
        assert_eq!(budget.used, 2 * exact);
    }

    #[test]
    fn relocation_and_addrsig_copies_cannot_repeat_unbounded_symbol_strings() {
        let fixture = std::env::var_os("HERMIT_PACKAGE_TEST_BEFORE")
            .expect("required uncompressed BPF fixture");
        let raw = read_regular(Path::new(&fixture), MAX_INPUT).unwrap();
        let elf = Elf::parse(&raw).unwrap();
        let relocation = elf
            .sections
            .iter()
            .find(|s| (s.kind == 4 || s.kind == 9) && !s.data.is_empty())
            .unwrap();
        let mut budget = StringBudget::new(MAX_INPUT);
        let entries = elf.relocations(relocation, &mut budget).unwrap();
        assert!(!entries.is_empty());
        assert!(budget.used > 0);
        let exact = budget.used;
        assert_eq!(
            elf.relocations(relocation, &mut StringBudget::new(exact))
                .unwrap(),
            entries
        );
        assert!(
            elf.relocations(relocation, &mut StringBudget::new(exact - 1))
                .is_err()
        );
        let addrsig = &elf.sections[elf.named[".llvm_addrsig"]];
        let mut budget = StringBudget::new(MAX_INPUT);
        let entries = elf.addrsig(addrsig, &mut budget).unwrap();
        assert!(!entries.is_empty());
        assert!(budget.used > 0);
        let exact = budget.used;
        assert_eq!(
            elf.addrsig(addrsig, &mut StringBudget::new(exact)).unwrap(),
            entries
        );
        assert!(
            elf.addrsig(addrsig, &mut StringBudget::new(exact - 1))
                .is_err()
        );
    }

    #[test]
    fn overlapping_section_payloads_cannot_multiply_the_copy_budget() {
        // Two distinct named sections can legally share file bytes. Bound the
        // aggregate copied payload before allocating, including that overlap.
        let names = b"\0.names\0.first\0.second\0";
        let data_at = 64 + 4 * 64 + names.len();
        let first_size = (MAX_INPUT - names.len()) / 2;
        let second_size = MAX_INPUT - names.len() - first_size;
        let mut raw = vec![0; data_at + second_size + 1];
        raw[..6].copy_from_slice(b"\x7fELF\x02\x01");
        raw[16..18].copy_from_slice(&1u16.to_le_bytes());
        raw[18..20].copy_from_slice(&247u16.to_le_bytes());
        raw[20..24].copy_from_slice(&1u32.to_le_bytes());
        raw[40..48].copy_from_slice(&64u64.to_le_bytes());
        raw[58..60].copy_from_slice(&64u16.to_le_bytes());
        raw[60..62].copy_from_slice(&4u16.to_le_bytes());
        raw[62..64].copy_from_slice(&1u16.to_le_bytes());
        raw[320..320 + names.len()].copy_from_slice(names);
        for (index, name, kind, offset, size) in [
            (1usize, 1u32, 3u32, 320usize, names.len()),
            (2, 8, 1, data_at, first_size),
            (3, 15, 1, data_at, second_size),
        ] {
            let row = 64 + index * 64;
            raw[row..row + 4].copy_from_slice(&name.to_le_bytes());
            raw[row + 4..row + 8].copy_from_slice(&kind.to_le_bytes());
            raw[row + 24..row + 32].copy_from_slice(&(offset as u64).to_le_bytes());
            raw[row + 32..row + 40].copy_from_slice(&(size as u64).to_le_bytes());
        }
        assert!(raw.len() < MAX_INPUT);
        let valid = Elf::parse(&raw).unwrap();
        assert_eq!(valid.sections.len(), 4);
        assert_eq!(
            valid.sections.iter().map(|s| s.data.len()).sum::<usize>(),
            MAX_INPUT
        );
        drop(valid);
        let last_size = 64 + 3 * 64 + 32;
        raw[last_size..last_size + 8].copy_from_slice(&((second_size + 1) as u64).to_le_bytes());
        let error = Elf::parse(&raw)
            .err()
            .expect("overlap must not amplify allocations");
        assert_eq!(
            error.to_string(),
            "aggregate ELF section bytes exceed the input byte bound"
        );
    }

    #[test]
    fn malformed_nobits_cannot_supply_a_decompression_allocation_size() {
        let before = std::env::var_os("HERMIT_PACKAGE_TEST_BEFORE")
            .expect("required uncompressed BPF fixture");
        let after =
            std::env::var_os("HERMIT_PACKAGE_TEST_AFTER").expect("required compressed BPF fixture");
        let before = read_regular(Path::new(&before), MAX_INPUT).unwrap();
        let after = read_regular(Path::new(&after), MAX_INPUT).unwrap();
        let old = Elf::parse(&before).unwrap();
        let new = Elf::parse(&after).unwrap();
        let compressed = new.sections.iter().find(|s| s.flags & 0x800 != 0).unwrap();
        let index = old.named[&compressed.name];
        let old_header = u64_at(&before, 40).unwrap() as usize + 64 * index;
        let new_header = u64_at(&after, 40).unwrap() as usize + 64 * new.named[&compressed.name];
        let compression_header = u64_at(&after, new_header + 24).unwrap() as usize;
        for declared_size in [compressed.size, 1u64 << 40] {
            let mut malformed_before = before.clone();
            let mut malformed_after = after.clone();
            malformed_before[old_header + 4..old_header + 8].copy_from_slice(&8u32.to_le_bytes());
            malformed_before[old_header + 32..old_header + 40]
                .copy_from_slice(&declared_size.to_le_bytes());
            malformed_after[compression_header + 8..compression_header + 16]
                .copy_from_slice(&declared_size.to_le_bytes());
            // NOBITS declares logical size without occupying bytes in the ELF.
            // The structural reader accepts it, but it must never authorize
            // allocating that size as a decompression buffer.
            assert!(Elf::parse(&malformed_before).is_ok());
            let error = prove_compression(&malformed_before, &malformed_after).unwrap_err();
            assert_eq!(
                error.to_string(),
                "only uncompressed DWARF PROGBITS may be compressed"
            );
        }
    }

    fn btf() -> (Vec<u8>, BTreeMap<String, (Vec<usize>, usize, usize)>) {
        let mut types = Vec::new();
        for word in [
            0u32,
            1 << 24,
            4,
            32,
            0,
            13 << 24 | 1,
            1,
            0,
            1,
            1,
            12 << 24,
            2,
        ] {
            types.extend_from_slice(&word.to_le_bytes());
        }
        let strings = b"\0hook\0";
        let mut result = vec![0x9f, 0xeb, 1, 0];
        for word in [
            24u32,
            0,
            types.len() as u32,
            types.len() as u32,
            strings.len() as u32,
        ] {
            result.extend_from_slice(&word.to_le_bytes());
        }
        result.extend(types);
        result.extend(strings);
        (
            result,
            BTreeMap::from([("hook".to_owned(), (vec![4], 1, 4))]),
        )
    }

    #[test]
    fn btf_exact_layout_and_digest_are_separate_required_gates() {
        let (raw, hooks) = btf();
        assert!(check_btf(&raw, &hooks).is_ok());
        let contract = Contract {
            schema: 1,
            abi_version: "test".into(),
            btf_sha256: digest(&raw),
            maps: 1,
            programs: 1,
            links: 1,
            source_files: vec!["test.c".into()],
            hooks,
        };
        assert!(contract.require_btf(&raw).is_ok());
        let mut changed = raw.clone();
        changed.push(0);
        assert!(check_btf(&changed, &contract.hooks).is_ok());
        assert!(contract.require_btf(&changed).is_err());
    }

    #[test]
    fn btf_wrong_argument_slot_return_missing_truncation_and_header_refuse() {
        let (raw, mut hooks) = btf();
        for expected in [
            (vec![8], 1, 4),
            (vec![4], 2, 4),
            (vec![4], 1, 8),
            (vec![4, 4], 2, 4),
        ] {
            hooks.insert("hook".into(), expected);
            assert!(check_btf(&raw, &hooks).is_err());
        }
        let (raw, hooks) = btf();
        let mut missing = hooks.clone();
        missing.insert("absent".into(), (vec![4], 1, 4));
        assert!(check_btf(&raw, &missing).is_err());
        for length in [0, 23, 24, raw.len() - 1] {
            assert!(check_btf(&raw[..length], &hooks).is_err());
        }
        for position in [0, 2, 3] {
            let mut changed = raw.clone();
            changed[position] ^= 1;
            assert!(check_btf(&changed, &hooks).is_err());
        }
    }

    #[test]
    fn compressed_debug_relocations_require_actual_target_link_flags_and_entry() {
        let before = read_regular(
            Path::new(&std::env::var_os("HERMIT_PACKAGE_TEST_BEFORE").unwrap()),
            MAX_INPUT,
        )
        .unwrap();
        let after = read_regular(
            Path::new(&std::env::var_os("HERMIT_PACKAGE_TEST_AFTER").unwrap()),
            MAX_INPUT,
        )
        .unwrap();
        let old = Elf::parse(&before).unwrap();
        let new = Elf::parse(&after).unwrap();
        let compressed = new
            .sections
            .iter()
            .find(|s| s.kind == 9 && s.flags & 0x800 != 0)
            .expect("required actual compressed debug REL fixture");
        println!(
            "COMPRESSION-PROOF {}",
            prove_compression(&before, &after).unwrap()
        );
        assert!(prove_compression(&before, &after).is_ok());
        let before_row = u64_at(&before, 40).unwrap() as usize + 64 * old.named[&compressed.name];
        let after_row = u64_at(&after, 40).unwrap() as usize + 64 * new.named[&compressed.name];
        let runtime_target = old
            .sections
            .iter()
            .position(|s| s.flags & 4 != 0 && !s.data.is_empty())
            .unwrap();
        // Apply the same counterfeit relation to both operands: equality alone
        // must not authorize compressing runtime relocations or malformed ELF.
        for (offset, replacement) in [
            (44, (runtime_target as u32).to_le_bytes().to_vec()),
            (40, 0u32.to_le_bytes().to_vec()),
            (56, 8u64.to_le_bytes().to_vec()),
        ] {
            let mut a = before.clone();
            let mut b = after.clone();
            a[before_row + offset..before_row + offset + replacement.len()]
                .copy_from_slice(&replacement);
            b[after_row + offset..after_row + offset + replacement.len()]
                .copy_from_slice(&replacement);
            assert!(prove_compression(&a, &b).is_err());
        }
        for bit in [2u64, 4] {
            let mut a = before.clone();
            let mut b = after.clone();
            let flags = u64_at(&a, before_row + 8).unwrap() | bit;
            a[before_row + 8..before_row + 16].copy_from_slice(&flags.to_le_bytes());
            let flags = u64_at(&b, after_row + 8).unwrap() | bit;
            b[after_row + 8..after_row + 16].copy_from_slice(&flags.to_le_bytes());
            assert!(prove_compression(&a, &b).is_err());
        }
        let mut b = after.clone();
        let at = u64_at(&b, after_row + 24).unwrap() as usize;
        b[at + 24] ^= 1;
        assert!(prove_compression(&before, &b).is_err());
    }

    #[test]
    fn actual_compression_and_corruption_controls() {
        // These inputs are explicitly provided by the offline package test
        // runner. Missing fixtures fail the test, never silently skip it.
        let before = std::env::var_os("HERMIT_PACKAGE_TEST_BEFORE")
            .expect("required uncompressed BPF fixture");
        let after =
            std::env::var_os("HERMIT_PACKAGE_TEST_AFTER").expect("required compressed BPF fixture");
        let before = read_regular(Path::new(&before), MAX_INPUT).unwrap();
        let after = read_regular(Path::new(&after), MAX_INPUT).unwrap();
        assert!(prove_compression(&before, &after).is_ok());
        let elf = Elf::parse(&after).unwrap();
        let header_at = u64_at(&after, 40).unwrap() as usize;
        let btf_index = elf.named[".BTF"];
        let btf_offset = u64_at(&after, header_at + 64 * btf_index + 24).unwrap() as usize;
        let mut changed = after.clone();
        changed[btf_offset] ^= 1;
        assert!(prove_compression(&before, &changed).is_err());
        let mut changed = after.clone();
        let addrsig = header_at + 64 * elf.named[".llvm_addrsig"] + 40;
        changed[addrsig..addrsig + 4].copy_from_slice(&0u32.to_le_bytes());
        assert!(prove_compression(&before, &changed).is_err());
        let index = elf
            .sections
            .iter()
            .position(|s| s.flags & 0x800 != 0)
            .unwrap();
        let offset = u64_at(&after, header_at + 64 * index + 24).unwrap() as usize;
        for at in [offset + 8, offset + 16, offset + 24] {
            let mut changed = after.clone();
            changed[at] ^= 1;
            assert!(prove_compression(&before, &changed).is_err());
        }
        let mut changed = after.clone();
        changed[60..62].copy_from_slice(&0u16.to_le_bytes());
        assert!(prove_compression(&before, &changed).is_err());
        assert!(prove_compression(&before, &before).is_err());
    }
}
