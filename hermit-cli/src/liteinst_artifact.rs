use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use goblin::elf::Elf;
use goblin::elf::dynamic::*;
use goblin::elf::header::*;
use goblin::elf::program_header::*;
use goblin::elf::reloc::*;
use goblin::elf::sym::*;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

pub const RUNTIME_NAME: &str = "libhermit_liteinst_detcore.so";
pub const DESCRIPTOR_NAME: &str = "hermit_liteinst_detcore_descriptor_v1";
#[path = "liteinst_artifact_private.rs"]
pub mod private;
const PAGE_SIZE: u64 = 4096;

pub fn runtime_candidate_from(
    parent: &Path,
    mut resource: impl FnMut(&str) -> io::Result<Option<PathBuf>>,
) -> io::Result<PathBuf> {
    let present = |path: &Path| match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot inspect LiteInst runtime candidate {}: {error}",
                path.display()
            ),
        )),
    };
    for name in [private::RUNTIME_NAME, RUNTIME_NAME] {
        for path in [parent.join(name), parent.join("deps").join(name)] {
            if present(&path)? {
                return Ok(path);
            }
        }
        if let Some(path) = resource(name)?
            && present(&path)?
        {
            return Ok(path);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "actual shared Detcore LiteInst runtime was not staged",
    ))
}

#[cfg(test)]
#[path = "liteinst_artifact_tests.rs"]
mod tests;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn require(condition: bool, message: &str) -> io::Result<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn word(bytes: &[u8], offset: usize) -> io::Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| invalid("word overflow"))?;
    let array = bytes
        .get(offset..end)
        .ok_or_else(|| invalid("truncated word"))?;
    Ok(u64::from_le_bytes(array.try_into().unwrap()))
}

fn end(address: u64, size: u64) -> io::Result<u64> {
    address
        .checked_add(size)
        .ok_or_else(|| invalid("ELF range overflow"))
}

fn contains(start: u64, size: u64, address: u64, length: u64) -> bool {
    address >= start
        && end(address, length)
            .ok()
            .zip(end(start, size).ok())
            .is_some_and(|(limit, outer)| limit <= outer)
}

struct Image<'a> {
    bytes: &'a [u8],
    elf: Elf<'a>,
    tags: BTreeMap<u64, u64>,
}

impl<'a> Image<'a> {
    fn validate_loads(&self) -> io::Result<()> {
        let mut pages = Vec::new();
        for segment in &self.elf.program_headers {
            if segment.p_type != PT_LOAD {
                continue;
            }
            require(
                segment.p_filesz <= segment.p_memsz,
                "segment file size exceeds memory size",
            )?;
            require(
                segment.p_vaddr % PAGE_SIZE == segment.p_offset % PAGE_SIZE
                    && (segment.p_align <= 1
                        || (segment.p_align.is_power_of_two()
                            && segment.p_vaddr % segment.p_align
                                == segment.p_offset % segment.p_align)),
                "unsupported load alignment",
            )?;
            require(
                end(segment.p_offset, segment.p_filesz)? <= self.bytes.len() as u64,
                "load exceeds file",
            )?;
            if segment.p_memsz == 0 {
                continue;
            }
            let start = segment.p_vaddr & !(PAGE_SIZE - 1);
            let limit =
                end(end(segment.p_vaddr, segment.p_memsz)?, PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
            for &(other, length) in &pages {
                require(
                    !overlap(start, limit - start, other, length)?,
                    "overlapping load pages",
                )?;
            }
            pages.push((start, limit - start));
        }
        Ok(())
    }

    fn read_only(&self, address: u64, size: u64) -> io::Result<bool> {
        require(
            self.elf
                .program_headers
                .iter()
                .filter(|segment| segment.p_type == PT_GNU_RELRO)
                .count()
                <= 1,
            "multiple PT_GNU_RELRO headers",
        )?;
        self.mapped(address, size)?;
        let load = self
            .elf
            .program_headers
            .iter()
            .find(|segment| {
                segment.p_type == PT_LOAD
                    && contains(segment.p_vaddr, segment.p_filesz, address, size)
            })
            .ok_or_else(|| invalid("unmapped protected range"))?;
        if load.p_flags & PF_W == 0 {
            return Ok(true);
        }
        for segment in &self.elf.program_headers {
            if segment.p_type != PT_GNU_RELRO {
                continue;
            }
            require(
                contains(load.p_vaddr, load.p_memsz, segment.p_vaddr, segment.p_memsz)
                    && segment.p_filesz <= segment.p_memsz,
                "RELRO is not contained in descriptor load",
            )?;
            require(
                self.file_offset(segment.p_vaddr, segment.p_filesz)? == segment.p_offset,
                "inconsistent RELRO mapping",
            )?;
            let start = segment.p_vaddr & !(PAGE_SIZE - 1);
            let limit = end(segment.p_vaddr, segment.p_memsz)? & !(PAGE_SIZE - 1);
            if contains(start, limit - start, address, size) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn mapped(&self, address: u64, size: u64) -> io::Result<&'a [u8]> {
        let offset = self.file_offset(address, size)?;
        let limit = end(offset, size)?;
        let start = usize::try_from(offset).map_err(|_| invalid("offset too large"))?;
        let limit = usize::try_from(limit).map_err(|_| invalid("range too large"))?;
        self.bytes
            .get(start..limit)
            .ok_or_else(|| invalid("ELF range beyond file"))
    }

    fn file_offset(&self, address: u64, size: u64) -> io::Result<u64> {
        let mut mappings = self.elf.program_headers.iter().filter(|segment| {
            segment.p_type == PT_LOAD && contains(segment.p_vaddr, segment.p_filesz, address, size)
        });
        let segment = mappings
            .next()
            .ok_or_else(|| invalid("unmapped ELF range"))?;
        require(mappings.next().is_none(), "ambiguous ELF mapping")?;
        end(segment.p_offset, address - segment.p_vaddr)
    }

    fn tag(&self, tag: u64) -> io::Result<u64> {
        self.tags
            .get(&tag)
            .copied()
            .ok_or_else(|| invalid(format!("missing dynamic tag {tag}")))
    }

    fn symbol(&self, index: usize) -> io::Result<(u32, u8, u8, u16, u64, u64)> {
        require(
            index < self.elf.dynsyms.len(),
            "dynamic symbol index out of bounds",
        )?;
        require(self.tag(DT_SYMENT)? == 24, "unsupported symbol size")?;
        let address = end(
            self.tag(DT_SYMTAB)?,
            (index as u64)
                .checked_mul(24)
                .ok_or_else(|| invalid("symbol overflow"))?,
        )?;
        let bytes = self.mapped(address, 24)?;
        Ok((
            u32::from_le_bytes(bytes[..4].try_into().unwrap()),
            bytes[4],
            bytes[5],
            u16::from_le_bytes(bytes[6..8].try_into().unwrap()),
            word(bytes, 8)?,
            word(bytes, 16)?,
        ))
    }

    fn symbol_name(&self, offset: u32) -> io::Result<&'a str> {
        let strings = self.mapped(self.tag(DT_STRTAB)?, self.tag(DT_STRSZ)?)?;
        let suffix = strings
            .get(offset as usize..)
            .ok_or_else(|| invalid("invalid symbol name"))?;
        let size = suffix
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| invalid("unterminated name"))?;
        std::str::from_utf8(&suffix[..size]).map_err(|_| invalid("invalid symbol name encoding"))
    }

    fn relocations(&self) -> io::Result<Vec<Relocation>> {
        require(
            !self.tags.contains_key(&36)
                && !self.tags.contains_key(&35)
                && !self.tags.contains_key(&37),
            "packed RELR unsupported",
        )?;
        let mut relocations = Vec::new();
        let mut ranges = Vec::new();
        for (address_tag, size_tag, entry_tag, rela) in [
            (DT_RELA, DT_RELASZ, DT_RELAENT, true),
            (DT_REL, DT_RELSZ, DT_RELENT, false),
        ] {
            let count_tag = if rela { DT_RELACOUNT } else { DT_RELCOUNT };
            if [address_tag, size_tag, entry_tag, count_tag]
                .iter()
                .any(|tag| self.tags.contains_key(tag))
            {
                let stride = if rela { 24 } else { 16 };
                require(
                    self.tag(entry_tag)? == stride,
                    "unsupported relocation size",
                )?;
                let address = self.tag(address_tag)?;
                let size = self.tag(size_tag)?;
                ranges.push((address, size));
                let start = relocations.len();
                self.read_relocations(address, size, rela, false, &mut relocations)?;
                let count = self.tags.get(&count_tag).copied().unwrap_or(0);
                require(
                    count <= size / stride,
                    "relative prefix exceeds relocation table",
                )?;
                for relocation in &relocations[start..start + count as usize] {
                    require(
                        relocation.kind == R_X86_64_RELATIVE && relocation.symbol == 0,
                        "relative prefix contains a non-relative relocation",
                    )?;
                }
            }
        }
        if [DT_JMPREL, DT_PLTRELSZ, DT_PLTREL]
            .iter()
            .any(|tag| self.tags.contains_key(tag))
        {
            let kind = self.tag(DT_PLTREL)?;
            require(kind == DT_RELA || kind == DT_REL, "unsupported PLT table")?;
            let address = self.tag(DT_JMPREL)?;
            let size = self.tag(DT_PLTRELSZ)?;
            ranges.push((address, size));
            self.read_relocations(address, size, kind == DT_RELA, true, &mut relocations)?;
        }
        for (index, (address, size)) in ranges.iter().enumerate() {
            for (other, length) in &ranges[..index] {
                require(
                    !overlap(*address, *size, *other, *length)?,
                    "overlapping relocation tables",
                )?;
            }
        }
        Ok(relocations)
    }

    fn read_relocations(
        &self,
        address: u64,
        size: u64,
        rela: bool,
        plt: bool,
        output: &mut Vec<Relocation>,
    ) -> io::Result<()> {
        let stride = if rela { 24 } else { 16 };
        require(
            address.is_multiple_of(8) && size.is_multiple_of(stride),
            "misaligned relocation table",
        )?;
        for bytes in self.mapped(address, size)?.chunks_exact(stride as usize) {
            let info = word(bytes, 8)?;
            output.push(Relocation {
                address: word(bytes, 0)?,
                kind: info as u32,
                symbol: (info >> 32) as usize,
                addend: if rela {
                    Some(word(bytes, 16)? as i64)
                } else {
                    None
                },
                plt,
            });
        }
        Ok(())
    }

    fn pointer(&self, slot: u64, relocations: &[Relocation]) -> io::Result<u64> {
        let matching: Vec<_> = relocations
            .iter()
            .filter(|relocation| relocation.address == slot)
            .collect();
        require(
            matching.len() == 1,
            "pointer needs exactly one dynamic relocation",
        )?;
        let relocation = matching[0];
        require(!relocation.plt, "PLT constructor relocation unsupported")?;
        let addend = match relocation.addend {
            Some(addend) => i128::from(addend),
            None => i128::from(word(self.mapped(slot, 8)?, 0)? as i64),
        };
        let base = match relocation.kind {
            R_X86_64_RELATIVE => {
                require(relocation.symbol == 0, "RELATIVE symbol must be zero")?;
                0
            }
            R_X86_64_64 => {
                let (_, info, visibility, section, value, _) = self.symbol(relocation.symbol)?;
                let binding = info >> 4;
                let visibility = visibility & 3;
                require(
                    info & 15 == STT_FUNC && section != 0 && section < 0xff00,
                    "constructor symbol must be ordinary defined function",
                )?;
                require(
                    binding == STB_LOCAL
                        || (binding == STB_GLOBAL
                            && (visibility == STV_HIDDEN || visibility == STV_INTERNAL)),
                    "preemptible constructor symbol",
                )?;
                i128::from(value)
            }
            _ => return Err(invalid("unsupported constructor relocation")),
        };
        let target =
            u64::try_from(base + addend).map_err(|_| invalid("relocated pointer overflow"))?;
        self.mapped(target, 1)?;
        require(
            self.elf.program_headers.iter().any(|segment| {
                segment.p_type == PT_LOAD
                    && segment.p_flags & PF_X != 0
                    && contains(segment.p_vaddr, segment.p_filesz, target, 1)
            }),
            "constructor is not executable",
        )?;
        Ok(target)
    }
}

struct Relocation {
    address: u64,
    kind: u32,
    symbol: usize,
    addend: Option<i64>,
    plt: bool,
}

fn overlap(address: u64, size: u64, other: u64, length: u64) -> io::Result<bool> {
    Ok(size != 0 && length != 0 && address < end(other, length)? && other < end(address, size)?)
}

pub fn validate_constructor(bytes: &[u8]) -> io::Result<()> {
    validate_constructor_kind(bytes, false)
}

pub fn validate_private_runtime(bytes: &[u8]) -> io::Result<()> {
    validate_constructor_kind(bytes, true)
}

fn validate_constructor_kind(bytes: &[u8], private: bool) -> io::Result<()> {
    let elf = Elf::parse(bytes).map_err(|error| invalid(error.to_string()))?;
    require(
        elf.header.e_type == ET_DYN
            && elf.header.e_machine == EM_X86_64
            && elf.is_64
            && elf.little_endian,
        "expected little-endian x86-64 ET_DYN",
    )?;
    let mut image = Image {
        bytes,
        elf,
        tags: BTreeMap::new(),
    };
    image.validate_loads()?;
    require(
        private || image.elf.entry == 0,
        "preload runtime must not claim a private executable entry",
    )?;
    if private {
        require(
            bytes.len() <= 512 * 1024 * 1024 && image.elf.entry != 0,
            "private runtime size/entry invalid",
        )?;
        require(
            image.elf.program_headers.iter().all(|segment| {
                segment.p_type != PT_INTERP
                    && (segment.p_type != PT_LOAD
                        || segment.p_flags & (PF_W | PF_X) != (PF_W | PF_X))
            }),
            "private runtime has interpreter or writable executable load",
        )?;
        let mut entries = Vec::new();
        for name in ["pe_kernel_entry", "pe_private_crt_entry"] {
            let symbols: Vec<_> = image
                .elf
                .syms
                .iter()
                .filter(|symbol| image.elf.strtab.get_at(symbol.st_name) == Some(name))
                .collect();
            require(
                symbols.len() == 1,
                "private runtime entry symbol missing/ambiguous",
            )?;
            let symbol = &symbols[0];
            require(
                symbol.st_type() == STT_FUNC
                    && symbol.st_shndx > 0
                    && symbol.st_shndx < 0xff00
                    && symbol.st_size > 0,
                "invalid private entry symbol",
            )?;
            image.mapped(symbol.st_value, symbol.st_size)?;
            require(
                image.elf.program_headers.iter().any(|segment| {
                    segment.p_type == PT_LOAD
                        && segment.p_flags & PF_X != 0
                        && contains(
                            segment.p_vaddr,
                            segment.p_filesz,
                            symbol.st_value,
                            symbol.st_size,
                        )
                }),
                "private entry is not executable",
            )?;
            entries.push((symbol.st_value, symbol.st_size));
        }
        require(
            entries[0].0 == image.elf.entry
                && !overlap(entries[0].0, entries[0].1, entries[1].0, entries[1].1)?,
            "private kernel/CRT entry mismatch",
        )?;
    }
    let dynamic: Vec<_> = image
        .elf
        .program_headers
        .iter()
        .filter(|header| header.p_type == PT_DYNAMIC)
        .collect();
    require(dynamic.len() == 1, "expected one PT_DYNAMIC")?;
    let dynamic = dynamic[0];
    require(
        dynamic.p_filesz.is_multiple_of(16),
        "invalid dynamic table size",
    )?;
    let table = image.mapped(dynamic.p_vaddr, dynamic.p_filesz)?;
    require(
        dynamic.p_offset == image.file_offset(dynamic.p_vaddr, dynamic.p_filesz)?,
        "inconsistent PT_DYNAMIC mapping",
    )?;
    let mut terminated = false;
    for entry in table.as_chunks::<16>().0 {
        let tag = word(entry, 0)?;
        if tag == DT_NULL {
            terminated = true;
            break;
        }
        if tag != DT_NEEDED {
            require(
                image.tags.insert(tag, word(entry, 8)?).is_none(),
                "duplicate dynamic tag",
            )?;
        } else {
            require(!private, "private runtime has dynamic dependency")?;
        }
    }
    require(terminated, "unterminated PT_DYNAMIC")?;
    let array = image.tag(DT_INIT_ARRAY)?;
    let size = image.tag(DT_INIT_ARRAYSZ)?;
    require(
        size != 0 && size.is_multiple_of(8) && array.is_multiple_of(8),
        "invalid dynamic init array",
    )?;
    image.mapped(array, size)?;
    for section in &image.elf.section_headers {
        if section.sh_type == goblin::elf::section_header::SHT_INIT_ARRAY {
            require(
                section.sh_addr == array && section.sh_size == size,
                "section contradicts dynamic init array",
            )?;
            require(
                section.sh_offset == image.file_offset(array, size)?,
                "init section mapping mismatch",
            )?;
        }
    }
    let mut descriptors = Vec::new();
    if private {
        for symbol in &image.elf.syms {
            if image.elf.strtab.get_at(symbol.st_name) == Some(DESCRIPTOR_NAME) {
                require(
                    symbol.st_type() == STT_OBJECT
                        && symbol.st_shndx > 0
                        && symbol.st_shndx < 0xff00
                        && symbol.st_size == 32,
                    "invalid private descriptor symbol",
                )?;
                descriptors.push(symbol.st_value);
            }
        }
    } else {
        for index in 0..image.elf.dynsyms.len() {
            let (name, info, _, section, address, length) = image.symbol(index)?;
            if image.symbol_name(name)? == DESCRIPTOR_NAME {
                require(
                    info & 15 == STT_OBJECT && section != 0 && section < 0xff00 && length == 32,
                    "invalid descriptor symbol",
                )?;
                descriptors.push(address);
            }
        }
    }
    require(descriptors.len() == 1, "expected one Detcore descriptor")?;
    let descriptor = descriptors[0];
    require(descriptor.is_multiple_of(8), "unaligned descriptor")?;
    let header = image.mapped(descriptor, 32)?;
    require(
        &header[..8] == b"HLI_DSO1"
            && word(header, 8)? == (32u64 << 32) | 1
            && word(header, 16)? == 1,
        "invalid Detcore descriptor",
    )?;
    require(
        image.read_only(descriptor, 32)?,
        "descriptor is not read-only/RELRO",
    )?;
    let relocations = image.relocations()?;
    let pointer_slot = end(descriptor, 24)?;
    let target = image.pointer(pointer_slot, &relocations)?;
    let mut selected = None;
    for slot in (array..end(array, size)?).step_by(8) {
        if image.pointer(slot, &relocations)? == target {
            require(
                selected.replace(slot).is_none(),
                "duplicate constructor entry",
            )?;
        }
    }
    let selected = selected.ok_or_else(|| invalid("constructor absent from dynamic init array"))?;
    for relocation in &relocations {
        if relocation.kind == R_X86_64_NONE {
            continue;
        }
        let width = if relocation.kind == R_X86_64_COPY {
            image.symbol(relocation.symbol)?.5
        } else if relocation.kind == R_X86_64_TLSDESC {
            16
        } else {
            8
        };
        if overlap(relocation.address, width, descriptor, 32)?
            || overlap(relocation.address, width, selected, 8)?
        {
            require(
                (relocation.address == pointer_slot || relocation.address == selected)
                    && matches!(relocation.kind, R_X86_64_RELATIVE | R_X86_64_64),
                "conflicting relocation touches constructor descriptor/entry",
            )?;
        }
    }
    Ok(())
}

pub fn validate_provenance(
    bytes: &[u8],
    sidecar: &[u8],
    pin: &str,
    source: &str,
    diagnostic: bool,
) -> io::Result<()> {
    let record: Value =
        serde_json::from_slice(sidecar).map_err(|error| invalid(error.to_string()))?;
    let object = record
        .as_object()
        .ok_or_else(|| invalid("provenance must be object"))?;
    let fields = [
        "schema",
        "artifact",
        "runtime_abi",
        "bootstrap_selector",
        "syscall_mode",
        "dso_size",
        "dso_sha256",
        "declared_reverie_rev",
        "resolved_reverie_rev",
        "source_kind",
        "source_pair_sha256",
    ];
    require(
        object.len() == fields.len() && fields.iter().all(|field| object.contains_key(*field)),
        "provenance fields mismatch",
    )?;
    let private = record["schema"] == 2;
    require(
        (record["schema"] == 1 || private)
            && record["runtime_abi"] == 1
            && record["artifact"]
                == if private {
                    "hermit-liteinst-detcore-private-runtime"
                } else {
                    "hermit-liteinst-detcore-runtime"
                }
            && record["bootstrap_selector"] == "hermit-detcore-liteinst-v1"
            && record["syscall_mode"] == "user_dispatch_without_patching",
        "provenance ABI mismatch",
    )?;
    require(
        hex_digest(pin, 40) && hex_digest(source, 64),
        "unknown compiled identity",
    )?;
    require(
        record["declared_reverie_rev"] == pin && record["source_pair_sha256"] == source,
        "provenance source identity mismatch",
    )?;
    let resolved = record["resolved_reverie_rev"]
        .as_str()
        .ok_or_else(|| invalid("missing resolved revision"))?;
    require(hex_digest(resolved, 40), "invalid resolved revision")?;
    require(
        if diagnostic {
            record["source_kind"] == "local-diagnostic"
        } else {
            record["source_kind"] == "pinned-git" && resolved == pin
        },
        "provenance mode mismatch",
    )?;
    require(
        record["dso_size"].as_u64() == Some(bytes.len() as u64)
            && record["dso_sha256"] == digest(bytes),
        "DSO bytes do not match provenance",
    )?;
    if private {
        validate_private_runtime(bytes)
    } else {
        validate_constructor(bytes)
    }
}

fn hex_digest(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn validate_file(path: &Path, pin: &str, source: &str, diagnostic: bool) -> io::Result<()> {
    require(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "runtime must be regular file",
    )?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".provenance.json");
    require(
        fs::symlink_metadata(&name)?.file_type().is_file(),
        "provenance must be regular file",
    )?;
    validate_provenance(&fs::read(path)?, &fs::read(name)?, pin, source, diagnostic)
}

pub struct SourceInputs<'a> {
    pub hermit: &'a Path,
    pub reverie: &'a Path,
    pub cli_manifest: &'a Path,
    pub dso_manifest: &'a Path,
    pub config: Option<&'a Path>,
    pub evidence: &'a Path,
    pub pin: &'a str,
    pub diagnostic: bool,
}

pub fn validate_file_identity(
    path: &Path,
    pin: &str,
    source: &str,
    diagnostic: bool,
    resolved: &str,
) -> io::Result<()> {
    require(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "runtime must be regular file",
    )?;
    validate_snapshot_identity(path, &fs::read(path)?, pin, source, diagnostic, resolved)
}

pub(crate) fn validate_snapshot_identity(
    path: &Path,
    bytes: &[u8],
    pin: &str,
    source: &str,
    diagnostic: bool,
    resolved: &str,
) -> io::Result<()> {
    require(
        fs::symlink_metadata(path)?.file_type().is_file(),
        "runtime must be regular file",
    )?;
    let mut name = path.as_os_str().to_os_string();
    name.push(".provenance.json");
    require(
        fs::symlink_metadata(&name)?.file_type().is_file(),
        "provenance must be regular file",
    )?;
    let sidecar = fs::read(name)?;
    validate_provenance(bytes, &sidecar, pin, source, diagnostic)?;
    let record: Value = serde_json::from_slice(&sidecar)?;
    require(
        hex_digest(resolved, 40) && record["resolved_reverie_rev"] == resolved,
        "resolved Reverie revision differs from compiled source record",
    )
}

fn git(root: &Path, args: &[&str]) -> io::Result<Vec<u8>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()?;
    require(output.status.success(), "source identity Git read failed")?;
    Ok(output.stdout)
}

fn source_files(root: &Path) -> io::Result<Value> {
    let paths = git(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "--deduplicate",
            "-z",
        ],
    )?;
    let mut files = serde_json::Map::new();
    for path in paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        let relative =
            std::str::from_utf8(path).map_err(|_| invalid("source path is not UTF-8"))?;
        let path = root.join(relative);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                git(root, &["ls-files", "--error-unmatch", "--", relative])?;
                files.insert(relative.to_owned(), serde_json::json!({"deleted": true}));
                continue;
            }
            Err(error) => return Err(error),
        };
        let identity = if metadata.file_type().is_symlink() {
            serde_json::json!({"symlink": fs::read_link(path)?.to_str().ok_or_else(|| invalid("non-UTF8 symlink"))?})
        } else if metadata.is_file() {
            serde_json::json!({"sha256": digest(&fs::read(path)?)})
        } else {
            let index = git(root, &["ls-files", "--stage", "--", relative])?;
            require(
                index.starts_with(b"160000 "),
                "unexpected non-regular source",
            )?;
            serde_json::json!({"gitlink_index": String::from_utf8(index).map_err(|_| invalid("invalid gitlink"))?})
        };
        files.insert(relative.to_owned(), identity);
    }
    Ok(Value::Object(files))
}

#[derive(Debug)]
struct SourceMode {
    kind: &'static str,
    hermit_dirty: bool,
    reverie_dirty: bool,
}

fn source_mode(
    diagnostic: bool,
    pin: &str,
    reverie_head: &str,
    hermit_status: &[u8],
    reverie_status: &[u8],
) -> io::Result<SourceMode> {
    let mode = SourceMode {
        kind: if diagnostic {
            "local-diagnostic"
        } else {
            "pinned-git"
        },
        hermit_dirty: !hermit_status.is_empty(),
        reverie_dirty: !reverie_status.is_empty(),
    };
    if !diagnostic {
        require(!mode.hermit_dirty, "normal source requires clean Hermit")?;
        require(
            reverie_head == pin && !mode.reverie_dirty,
            "normal source must match clean pinned Reverie",
        )?;
    }
    Ok(mode)
}

fn normalized(value: &mut Value, inputs: &SourceInputs<'_>) {
    match value {
        Value::String(text) => {
            for (root, role) in [
                (inputs.hermit, "$HERMIT"),
                (inputs.reverie, "$REVERIE"),
                (inputs.evidence, "$EVIDENCE"),
            ] {
                *text = text.replace(&root.to_string_lossy().to_string(), role);
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| normalized(value, inputs)),
        Value::Object(values) => values
            .values_mut()
            .for_each(|value| normalized(value, inputs)),
        _ => {}
    }
}

fn resolved_graph(inputs: &SourceInputs<'_>, manifest: &Path, role: &str) -> io::Result<Value> {
    let mut command =
        std::process::Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command.arg("metadata");
    let private = private::requested()?;
    if private && role == "dso" {
        command.args(["--no-default-features", "--features", "private-crt"]);
    }
    if let Some(config) = inputs.config {
        command.arg("--config").arg(config);
    }
    let output = command
        .args([
            "--offline",
            "--locked",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(manifest)
        .env("CARGO_NET_OFFLINE", "true")
        .output()?;
    require(
        output.status.success(),
        &format!(
            "{role} dependency graph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    let mut graph: Value =
        serde_json::from_slice(&output.stdout).map_err(|error| invalid(error.to_string()))?;
    let packages = graph["packages"]
        .as_array()
        .ok_or_else(|| invalid("metadata packages missing"))?;
    let mut shared = 0;
    let mut reverie_count = 0;
    let mut names = std::collections::BTreeSet::new();
    for package in packages {
        let name = package["name"]
            .as_str()
            .ok_or_else(|| invalid("metadata name missing"))?;
        let manifest = Path::new(
            package["manifest_path"]
                .as_str()
                .ok_or_else(|| invalid("metadata manifest missing"))?,
        )
        .canonicalize()?;
        if name == "hermit-detcore" {
            require(
                manifest == inputs.hermit.join("detcore/Cargo.toml").canonicalize()?,
                "graph selected foreign Detcore",
            )?;
            shared += 1;
        }
        if name.starts_with("reverie-") || name == "safeptrace" {
            require(names.insert(name), "mixed/duplicate Reverie package")?;
            require(
                manifest.starts_with(inputs.reverie.canonicalize()?),
                "graph selected foreign Reverie",
            )?;
            if inputs.diagnostic {
                require(
                    package["source"].is_null(),
                    "diagnostic graph contains unpatched Reverie",
                )?;
            } else {
                let source = package["source"]
                    .as_str()
                    .ok_or_else(|| invalid("normal graph requires pinned Git source"))?;
                require(
                    source.starts_with("git+https://github.com/rrnewton/reverie.git?")
                        && source.ends_with(&format!("#{}", inputs.pin)),
                    "normal graph revision mismatch",
                )?;
            }
            reverie_count += 1;
        }
    }
    require(
        shared == 1 && reverie_count > 0,
        "graph lacks unique shared Detcore/Reverie",
    )?;
    let selected_manifest = manifest.canonicalize()?;
    let selected = packages
        .iter()
        .find(|package| {
            package["manifest_path"].as_str().is_some_and(|path| {
                Path::new(path).canonicalize().ok().as_ref() == Some(&selected_manifest)
            })
        })
        .ok_or_else(|| invalid("selected graph package missing"))?;
    let (package_name, source_path) = if role == "cli" {
        ("hermit", inputs.hermit.join("hermit-cli/src/lib.rs"))
    } else {
        (
            "hermit-liteinst-detcore-runtime",
            inputs
                .hermit
                .join("liteinst-runtime-build/detcore-runtime/src/lib.rs"),
        )
    };
    require(
        selected["name"] == package_name
            && selected["targets"].as_array().is_some_and(|targets| {
                targets.iter().any(|target| {
                    target["src_path"].as_str().is_some_and(|path| {
                        Path::new(path).canonicalize().ok() == source_path.canonicalize().ok()
                    })
                })
            }),
        "graph selected wrong consumer source",
    )?;
    let nodes = graph["resolve"]["nodes"]
        .as_array()
        .ok_or_else(|| invalid("metadata resolution missing"))?;
    let selected_node = nodes
        .iter()
        .find(|node| node["id"] == selected["id"])
        .ok_or_else(|| invalid("consumer resolution missing"))?;
    if private && role == "dso" {
        for node in nodes {
            let package = packages
                .iter()
                .find(|package| package["id"] == node["id"])
                .ok_or_else(|| invalid("resolved package missing"))?;
            require(
                !matches!(
                    package["name"].as_str(),
                    Some("reverie-liteinst" | "reverie-ptrace" | "safeptrace")
                ),
                "private graph contains host facade/tracer",
            )?;
            if package["name"] == "reverie-liteinst-runtime"
                || package["name"] == "hermit-liteinst-detcore-runtime"
            {
                require(
                    node["features"] == serde_json::json!(["private-crt"]),
                    "private graph feature mismatch",
                )?;
            }
        }
    }
    let detcore = packages
        .iter()
        .find(|package| package["name"] == "hermit-detcore")
        .unwrap();
    require(
        selected_node["deps"].as_array().is_some_and(|deps| {
            deps.iter().any(|dependency| {
                dependency["name"] == "detcore" && dependency["pkg"] == detcore["id"]
            })
        }),
        "consumer does not directly depend on shared Detcore",
    )?;
    let metadata_path = inputs
        .evidence
        .join(format!("{role}-metadata-{}.json", digest(&output.stdout)));
    if !metadata_path.exists() {
        fs::write(metadata_path, &output.stdout)?;
    }
    if let Some(object) = graph.as_object_mut() {
        object.remove("target_directory");
    }
    normalized(&mut graph, inputs);
    Ok(graph)
}

pub fn source_record(inputs: &SourceInputs<'_>) -> io::Result<Value> {
    require(hex_digest(inputs.pin, 40), "unknown declared pin")?;
    let hermit_head = String::from_utf8(git(inputs.hermit, &["rev-parse", "HEAD"])?)
        .map_err(|_| invalid("invalid HEAD"))?;
    let reverie_head = String::from_utf8(git(inputs.reverie, &["rev-parse", "HEAD"])?)
        .map_err(|_| invalid("invalid HEAD"))?;
    let hermit_status = git(
        inputs.hermit,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    let reverie_status = git(
        inputs.reverie,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )?;
    let mode = source_mode(
        inputs.diagnostic,
        inputs.pin,
        reverie_head.trim(),
        &hermit_status,
        &reverie_status,
    )?;
    let cli = resolved_graph(inputs, inputs.cli_manifest, "cli")?;
    let dso = resolved_graph(inputs, inputs.dso_manifest, "dso")?;
    let config = match inputs.config {
        Some(path) => {
            let mut config = Value::String(fs::read_to_string(path)?);
            normalized(&mut config, inputs);
            digest(&serde_json::to_vec(&config)?)
        }
        None => digest(b""),
    };
    let mut record = serde_json::json!({
        "schema": 1, "source_kind": mode.kind,
        "declared_reverie_rev": inputs.pin, "resolved_reverie_rev": reverie_head.trim(),
        "hermit_head": hermit_head.trim(), "hermit_dirty": mode.hermit_dirty,
        "reverie_dirty": mode.reverie_dirty,
        "hermit_files": source_files(inputs.hermit)?, "reverie_files": source_files(inputs.reverie)?,
        "cli_graph_sha256": digest(&serde_json::to_vec(&cli)?),
        "dso_graph_sha256": digest(&serde_json::to_vec(&dso)?), "config_sha256": config
    });
    if private::requested()? {
        record["runtime_kind"] = serde_json::json!("private-crt");
        record["native_inputs"] = private::environment_inputs()?.identity;
    }
    Ok(record)
}

pub fn verify_source_record(path: &Path, inputs: &SourceInputs<'_>) -> io::Result<(String, Value)> {
    let bytes = fs::read(path)?;
    let expected: Value = serde_json::from_slice(&bytes)?;
    require(
        expected == source_record(inputs)?,
        "source record disagrees with actual sources/dependency graphs",
    )?;
    Ok((digest(&bytes), expected))
}

pub fn provenance(bytes: &[u8], source_digest: &str, record: &Value) -> Value {
    let private = record["runtime_kind"] == "private-crt";
    serde_json::json!({ "schema": if private { 2 } else { 1 }, "artifact": if private { "hermit-liteinst-detcore-private-runtime" } else { "hermit-liteinst-detcore-runtime" }, "runtime_abi": 1,
        "bootstrap_selector": "hermit-detcore-liteinst-v1", "syscall_mode": "user_dispatch_without_patching",
        "dso_size": bytes.len(), "dso_sha256": digest(bytes),
        "declared_reverie_rev": record["declared_reverie_rev"], "resolved_reverie_rev": record["resolved_reverie_rev"],
        "source_kind": record["source_kind"], "source_pair_sha256": source_digest })
}

pub fn stage_pair(
    destination: &Path,
    bytes: &[u8],
    provenance: &[u8],
    pin: &str,
    identity: &str,
    diagnostic: bool,
) -> io::Result<()> {
    use std::io::Write;
    validate_provenance(bytes, provenance, pin, identity, diagnostic)?;
    let mut name = destination.as_os_str().to_os_string();
    name.push(".provenance.json");
    let mut output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    output.write_all(bytes)?;
    if serde_json::from_slice::<Value>(provenance)?["schema"] == 2 {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(0o555))?;
    }
    output.sync_all()?;
    let mut sidecar = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&name)?;
    sidecar.write_all(provenance)?;
    sidecar.sync_all()?;
    validate_file(destination, pin, identity, diagnostic)
}
