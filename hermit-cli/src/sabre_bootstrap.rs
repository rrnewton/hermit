/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Authenticated initial-image random ingress at existing SaBRe ptrace stops.
//! No RPC readiness, scheduling operation or additional stop is introduced.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::ensure;
use detcore::random::InitialImage;
use detcore::random::LoaderState;
use detcore::random::encode_continuation;
use detcore::random::encode_initial_state;
use detcore::random::getrandom;
use detcore::random::initialize_auxv;
use detcore::random::root_prng;
use nix::unistd::Pid;
use object::Object;
use object::ObjectSegment;
use object::ObjectSymbol;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;
// Versioned loader wire protocol. No plugin crate is linked into the CLI:
// that crate owns a global allocator and guest-local runtime state.
mod bootstrap {
    pub const PRCTL_OPTION: u64 = 0x53425242;
    pub const VERSION: u64 = 1;
    pub const IMAGE: u64 = 1;
    pub const GETRANDOM: u64 = 2;
    pub const TAKE_STATE: u64 = 3;
    pub const MAX_STATE_BYTES: usize = 4096;
    pub const SYSCALL_SYMBOL: &str = "sbr_bootstrap_syscall_v1";
}
pub(super) const ENVIRONMENT: &str = "REVERIE_SABRE_BOOTSTRAP_V1";
const LAYOUT_SYMBOL: &str = "sbr_bootstrap_frame_layout_v1";

const MAX_FILE: usize = 128 * 1024 * 1024;
const MAX_MAPS: usize = 1024 * 1024;
const MAX_OBJECTS: usize = 16;
const PAGE: usize = 4096;

fn bounded_file(mut file: &File, maximum: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.by_ref()
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= maximum,
        "bootstrap input exceeds declared bound"
    );
    Ok(bytes)
}

fn read_path(path: impl AsRef<Path>, maximum: usize) -> Result<Vec<u8>> {
    bounded_file(&File::open(path)?, maximum)
}

fn word(bytes: &[u8], offset: usize) -> Result<usize> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| anyhow!("word offset overflow"))?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or_else(|| anyhow!("short word"))?
            .try_into()?,
    ) as usize)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Map {
    start: usize,
    end: usize,
    offset: usize,
    device: String,
    inode: u64,
    path: Vec<u8>,
    permissions: String,
}

impl Map {
    fn contains(&self, address: usize, size: usize) -> bool {
        address >= self.start && address.checked_add(size).is_some_and(|end| end <= self.end)
    }

    fn unescaped_path(&self) -> Result<&str> {
        let path = std::str::from_utf8(&self.path)?;
        ensure!(
            !path.contains('\\'),
            "escaped mapped paths unsupported for bootstrap"
        );
        Ok(path)
    }
}

fn maps(pid: Pid) -> Result<Vec<Map>> {
    let bytes = read_path(format!("/proc/{pid}/maps"), MAX_MAPS)?;
    bytes
        .strip_suffix(b"\n")
        .unwrap_or(&bytes)
        .split(|byte| *byte == b'\n')
        .map(|line| {
            // Only the five structural fields are whitespace-separated text.
            // The remaining pathname may contain spaces, kernel escapes or
            // non-UTF8 bytes in an unrelated mapping. Retain those bytes and
            // apply object-path restrictions only when selecting that object.
            let mut rest = line;
            let mut fields = Vec::with_capacity(5);
            for _ in 0..5 {
                rest = rest.trim_ascii_start();
                let end = rest
                    .iter()
                    .position(u8::is_ascii_whitespace)
                    .unwrap_or(rest.len());
                ensure!(end > 0, "missing bootstrap mapping field");
                fields.push(std::str::from_utf8(&rest[..end])?);
                rest = &rest[end..];
            }
            let (start, end) = fields[0]
                .split_once('-')
                .ok_or_else(|| anyhow!("bad map range"))?;
            let row = Map {
                start: usize::from_str_radix(start, 16)?,
                end: usize::from_str_radix(end, 16)?,
                offset: usize::from_str_radix(fields[2], 16)?,
                device: fields[3].to_owned(),
                inode: fields[4].parse()?,
                path: rest.trim_ascii_start().to_vec(),
                permissions: fields[1].to_owned(),
            };
            ensure!(
                row.start < row.end && row.permissions.len() == 4,
                "invalid bootstrap map"
            );
            Ok(row)
        })
        .collect()
}

fn containing(rows: &[Map], address: usize, size: usize) -> Result<&Map> {
    let mut found = rows.iter().filter(|row| row.contains(address, size));
    let first = found
        .next()
        .ok_or_else(|| anyhow!("bootstrap extent is not in one mapping"))?;
    ensure!(found.next().is_none(), "ambiguous bootstrap extent");
    Ok(first)
}

fn open_under_root(pid: Pid, path: &Path) -> Result<File> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow!("non-UTF8 bootstrap object path"))?;
    ensure!(
        path.starts_with('/') && !path.contains('\\') && !path.ends_with(" (deleted)"),
        "ambiguous bootstrap object path"
    );
    let components: Vec<_> = path[1..].split('/').collect();
    ensure!(
        !components.is_empty()
            && components
                .iter()
                .all(|c| !c.is_empty() && *c != "." && *c != ".."),
        "noncanonical bootstrap object path"
    );
    // Follow only the proc magic link to this owned task's root. Every actual
    // path component thereafter is no-follow and relative to a held directory.
    let mut file = File::open(format!("/proc/{pid}/root"))?;
    for (i, part) in components.iter().enumerate() {
        let name = CString::new(*part)?;
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if i + 1 < components.len() {
                libc::O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { libc::openat(file.as_raw_fd(), name.as_ptr(), flags, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        file = unsafe { File::from_raw_fd(fd) };
    }
    Ok(file)
}

struct HeldObject {
    path: PathBuf,
    file: File,
    bytes: Vec<u8>,
    device: String,
    inode: u64,
}

impl HeldObject {
    fn open(path: &Path) -> Result<Self> {
        let held = Self::open_file(path)?;
        let elf = object::File::parse(held.bytes.as_slice())?;
        ensure!(
            elf.format() == object::BinaryFormat::Elf
                && elf.architecture() == object::Architecture::X86_64
                && elf.is_little_endian(),
            "bootstrap requires x86-64 ELF"
        );
        Ok(held)
    }

    fn open_file(path: &Path) -> Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let file = File::open(&path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() > 0 && meta.len() <= MAX_FILE as u64,
            "unsupported bootstrap ELF object size"
        );
        let bytes = bounded_file(&file, MAX_FILE)?;
        // Compare maps-device to maps-device for this exact held FD. Btrfs's
        // maps superblock device need not equal its subvolume st_dev.
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                PAGE,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        ensure!(
            address != libc::MAP_FAILED,
            "cannot map held bootstrap object"
        );
        let observed = (|| {
            let own = maps(Pid::this())?;
            let row = containing(&own, address as usize, 1)?;
            ensure!(
                row.unescaped_path()?.as_bytes() == path.as_os_str().as_bytes()
                    && row.inode == meta.ino()
                    && row.offset == 0,
                "held bootstrap FD mapping mismatch"
            );
            Ok::<_, anyhow::Error>(row.device.clone())
        })();
        let unmapped = unsafe { libc::munmap(address, PAGE) };
        ensure!(unmapped == 0, "failed to unmap bootstrap identity mapping");
        Ok(Self {
            path,
            file,
            bytes,
            device: observed?,
            inode: meta.ino(),
        })
    }

    fn authenticate(&self, pid: Pid, row: &Map) -> Result<()> {
        ensure!(
            row.unescaped_path()?.as_bytes() == self.path.as_os_str().as_bytes()
                && row.inode == self.inode
                && row.device == self.device,
            "bootstrap mapped object identity mismatch"
        );
        self.authenticate_path(pid)
    }

    fn authenticate_path(&self, pid: Pid) -> Result<()> {
        let other = open_under_root(pid, &self.path)?;
        let a = self.file.metadata()?;
        let b = other.metadata()?;
        ensure!(
            (a.dev(), a.ino(), a.len()) == (b.dev(), b.ino(), b.len()),
            "bootstrap root-relative object changed"
        );
        ensure!(
            bounded_file(&other, MAX_FILE)? == self.bytes,
            "bootstrap mapped pathname content changed"
        );
        Ok(())
    }

    fn bias(&self, pid: Pid, rows: &[Map]) -> Result<usize> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let mut common: Option<BTreeSet<usize>> = None;
        for row in rows
            .iter()
            .filter(|r| r.path == self.path.as_os_str().as_bytes())
        {
            self.authenticate(pid, row)?;
            if !row.permissions.contains('x') {
                continue;
            }
            let mut candidates = BTreeSet::new();
            for segment in elf.segments() {
                let object::SegmentFlags::Elf { p_flags } = segment.flags() else {
                    continue;
                };
                if p_flags & object::elf::PF_X == 0 {
                    continue;
                }
                let (offset, size) = segment.file_range();
                if size == 0 {
                    continue;
                }
                let lo = offset as usize & !(PAGE - 1);
                let end = (offset as usize)
                    .checked_add(size as usize)
                    .and_then(|n| n.checked_add(PAGE - 1))
                    .ok_or_else(|| anyhow!("ELF segment overflow"))?
                    & !(PAGE - 1);
                if row.offset >= lo && row.offset < end {
                    let virtual_start = (segment.address() as usize & !(PAGE - 1))
                        .checked_add(row.offset - lo)
                        .ok_or_else(|| anyhow!("ELF bias overflow"))?;
                    if let Some(bias) = row.start.checked_sub(virtual_start) {
                        candidates.insert(bias);
                    }
                }
            }
            ensure!(
                !candidates.is_empty(),
                "executable mapping is not backed by an executable ELF load segment"
            );
            common = Some(match common {
                None => candidates,
                Some(old) => old.intersection(&candidates).copied().collect(),
            });
        }
        let biases =
            common.ok_or_else(|| anyhow!("missing executable bootstrap object mapping"))?;
        ensure!(biases.len() == 1, "ambiguous bootstrap ELF load bias");
        Ok(*biases.first().unwrap())
    }

    fn at_virtual(&self, address: usize, length: usize, readonly: bool) -> Result<&[u8]> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let end = address
            .checked_add(length)
            .ok_or_else(|| anyhow!("ELF extent overflow"))?;
        let mut found = None;
        for segment in elf.segments() {
            let (offset, size) = segment.file_range();
            let lo = segment.address() as usize;
            let hi = lo
                .checked_add(size as usize)
                .ok_or_else(|| anyhow!("ELF segment overflow"))?;
            if address >= lo && end <= hi {
                let object::SegmentFlags::Elf { p_flags } = segment.flags() else {
                    continue;
                };
                ensure!(
                    !readonly
                        || (p_flags & object::elf::PF_R != 0 && p_flags & object::elf::PF_W == 0),
                    "bootstrap descriptor is in a writable ELF load segment"
                );
                ensure!(found.is_none(), "ambiguous ELF file extent");
                let start = (offset as usize)
                    .checked_add(address - lo)
                    .ok_or_else(|| anyhow!("ELF file offset overflow"))?;
                found = self.bytes.get(
                    start
                        ..start
                            .checked_add(length)
                            .ok_or_else(|| anyhow!("ELF file range overflow"))?,
                );
            }
        }
        found.ok_or_else(|| anyhow!("bootstrap ELF extent is not fully file-backed"))
    }

    fn layout(&self) -> Result<FrameLayout> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let definitions: BTreeSet<_> = elf
            .symbols()
            .chain(elf.dynamic_symbols())
            .filter(|s| s.name().ok() == Some(LAYOUT_SYMBOL) && !s.is_undefined())
            .map(|s| {
                (
                    s.address() as usize,
                    s.size() as usize,
                    s.kind() == object::SymbolKind::Data,
                )
            })
            .collect();
        ensure!(
            definitions.len() == 1,
            "missing/ambiguous bootstrap frame descriptor"
        );
        let (address, size, data) = *definitions.first().unwrap();
        ensure!(
            data && size == 9 * 8,
            "wrong bootstrap frame descriptor type/extent"
        );
        FrameLayout::decode(self.at_virtual(address, size, true)?)
    }

    fn symbol(&self, name: &str) -> Result<usize> {
        let elf = object::File::parse(self.bytes.as_slice())?;
        let symbols: BTreeSet<_> = elf
            .symbols()
            .chain(elf.dynamic_symbols())
            .filter(|s| s.name().ok() == Some(name) && !s.is_undefined())
            .map(|s| s.address() as usize)
            .collect();
        ensure!(
            symbols.len() == 1,
            "missing/ambiguous bootstrap ELF symbol {name}"
        );
        Ok(*symbols.first().unwrap())
    }
}

#[derive(Clone, Copy)]
struct RemoteMemory(Pid);
impl MemoryAccess for RemoteMemory {
    fn read_vectored(
        &self,
        remote: &[IoSlice],
        local: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        let n = unsafe {
            libc::process_vm_readv(
                self.0.as_raw(),
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        };
        if n < 0 {
            Err(Errno::last())
        } else {
            Ok(n as usize)
        }
    }
    fn write_vectored(
        &mut self,
        local: &[IoSlice],
        remote: &mut [IoSliceMut],
    ) -> std::result::Result<usize, Errno> {
        let n = unsafe {
            libc::process_vm_writev(
                self.0.as_raw(),
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        };
        if n < 0 {
            Err(Errno::last())
        } else {
            Ok(n as usize)
        }
    }
}

fn remote_bytes(pid: Pid, address: usize, length: usize) -> Result<Vec<u8>> {
    ensure!(
        length <= MAX_MAPS && address.checked_add(length).is_some(),
        "bootstrap remote read exceeds bound"
    );
    let pointer =
        reverie::syscalls::Addr::from_raw(address).ok_or_else(|| anyhow!("null bootstrap read"))?;
    let mut bytes = vec![0; length];
    RemoteMemory(pid).read_exact(pointer, &mut bytes)?;
    Ok(bytes)
}

fn generation(pid: Pid) -> Result<u64> {
    let bytes = read_path(format!("/proc/{pid}/stat"), 4096)?;
    let text = std::str::from_utf8(&bytes)?;
    let (prefix, fields) = text
        .rsplit_once(") ")
        .ok_or_else(|| anyhow!("malformed bootstrap process stat"))?;
    ensure!(
        prefix
            .split_once(" (")
            .ok_or_else(|| anyhow!("missing process stat owner"))?
            .0
            .parse::<i32>()?
            == pid.as_raw(),
        "bootstrap stat owner mismatch"
    );
    Ok(fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow!("missing process generation"))?
        .parse()?)
}

/// Launch inputs are retained before the owned child can execute the loader.
pub(super) struct Launch {
    loader: HeldObject,
    program: HeldObject,
    interpreter: Option<HeldObject>,
    script: Option<HeldObject>,
    config: detcore::Config,
    layout: FrameLayout,
}
impl Launch {
    pub(super) fn new(loader: &Path, program: &Path, config: &detcore::Config) -> Result<Self> {
        let loader = HeldObject::open(loader)?;
        loader.symbol(bootstrap::SYSCALL_SYMBOL)?;
        let layout = loader.layout()?;
        let input = HeldObject::open_file(program)?;
        // Match the loader's single BINPRM_BUF_SIZE/fgets/strtok shebang
        // resolution. Retain the script too so the actual loader cannot read
        // a different interpreter selection after our classification.
        let (program, script) = if input.bytes.starts_with(b"#!") {
            let line = &input.bytes[..input.bytes.len().min(255)];
            let line = &line[..line.iter().position(|b| *b == b'\n').unwrap_or(line.len())];
            let path = line[2..]
                .split(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
                .find(|part| !part.is_empty())
                .ok_or_else(|| anyhow!("missing SaBRe script interpreter"))?;
            ensure!(!path.contains(&0), "NUL in SaBRe script interpreter");
            let interpreter = HeldObject::open(Path::new(std::str::from_utf8(path)?))?;
            (interpreter, Some(input))
        } else {
            let elf = object::File::parse(input.bytes.as_slice())?;
            ensure!(
                elf.format() == object::BinaryFormat::Elf
                    && elf.architecture() == object::Architecture::X86_64
                    && elf.is_little_endian(),
                "bootstrap requires x86-64 ELF"
            );
            (input, None)
        };
        // PT_INTERP is a program header, not a PT_LOAD segment.
        let phoff = word(&program.bytes, 32)?;
        let phnum = u16::from_le_bytes(
            program
                .bytes
                .get(56..58)
                .ok_or_else(|| anyhow!("short ELF header"))?
                .try_into()?,
        ) as usize;
        ensure!(
            phnum <= 128 && program.bytes.get(54..56) == Some(&56u16.to_le_bytes()),
            "unsupported ELF program headers"
        );
        ensure!(
            phnum > 0
                && phoff
                    .checked_add(phnum * 56)
                    .is_some_and(|end| end <= program.bytes.len()),
            "program headers outside held ELF"
        );
        let mut path = None;
        for i in 0..phnum {
            let p = phoff
                .checked_add(i * 56)
                .ok_or_else(|| anyhow!("program header overflow"))?;
            if program.bytes.get(p..p + 4) == Some(&3u32.to_le_bytes()) {
                ensure!(path.is_none(), "duplicate guest interpreter");
                let offset = word(&program.bytes, p + 8)?;
                let size = word(&program.bytes, p + 32)?;
                ensure!((2..=4096).contains(&size), "invalid guest interpreter path");
                let bytes = program
                    .bytes
                    .get(
                        offset
                            ..offset
                                .checked_add(size)
                                .ok_or_else(|| anyhow!("interpreter extent overflow"))?,
                    )
                    .ok_or_else(|| anyhow!("short interpreter path"))?;
                ensure!(
                    bytes.last() == Some(&0) && !bytes[..size - 1].contains(&0),
                    "invalid interpreter terminator"
                );
                path = Some(PathBuf::from(std::str::from_utf8(&bytes[..size - 1])?));
            }
        }
        let interpreter = path.as_deref().map(HeldObject::open).transpose()?;
        Ok(Self {
            loader,
            program,
            interpreter,
            script,
            config: config.clone(),
            layout,
        })
    }

    pub(super) fn initializes_random(&self) -> bool {
        self.interpreter.is_some()
    }

    fn authenticate_script(&self, pid: Pid) -> Result<()> {
        if let Some(script) = &self.script {
            script.authenticate_path(pid)?;
        }
        Ok(())
    }
}

struct SigillOrigin {
    generation: u64,
    registers: libc::user_regs_struct,
    mapping: Map,
}

/// State is single-root/single-image until TAKE. Ordinary post-handoff process
/// and robust-exit accounting remains in the existing supervisor.
pub(super) struct Bootstrap {
    launch: Launch,
    root: Pid,
    generation: u64,
    image: Option<InitialImage>,
    prng: rand_pcg::Pcg64Mcg,
    taken: bool,
    sigill: Option<SigillOrigin>,
    initial_random: usize,
    vdso: Map,
    vdso_bytes: Vec<u8>,
    other_objects: Vec<HeldObject>,
    // Only real kernel EXEC events create these entries. They are removed on
    // successful TAKE or final physical exit, never copied across fork.
    continuations: BTreeMap<Pid, InitialImage>,
    static_reexec_observed: bool,
}

#[derive(Clone, Copy, Debug)]
struct FrameLayout {
    size: usize,
    rdi: usize,
    rsi: usize,
    rdx: usize,
    architectural_return: usize,
    scratch_return: usize,
}
impl FrameLayout {
    fn decode(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() == 72
                && word(bytes, 0)? == 1
                && word(bytes, 56)? == 8
                && word(bytes, 64)? == 9,
            "unsupported bootstrap frame descriptor"
        );
        let size = word(bytes, 8)?;
        ensure!(
            (8..=4096).contains(&size) && size.is_multiple_of(8),
            "invalid bootstrap full frame size"
        );
        let offsets = [
            word(bytes, 16)?,
            word(bytes, 24)?,
            word(bytes, 32)?,
            word(bytes, 40)?,
            word(bytes, 48)?,
        ];
        ensure!(
            offsets
                .iter()
                .all(|o| o.is_multiple_of(8) && o.checked_add(8).is_some_and(|end| end <= size)),
            "bootstrap frame field outside extent"
        );
        ensure!(
            offsets.iter().copied().collect::<BTreeSet<_>>().len() == offsets.len(),
            "overlapping bootstrap frame fields"
        );
        Ok(Self {
            size,
            rdi: offsets[0],
            rsi: offsets[1],
            rdx: offsets[2],
            architectural_return: offsets[3],
            scratch_return: offsets[4],
        })
    }
}

fn auxv_random(bytes: &[u8]) -> Result<usize> {
    ensure!(bytes.len().is_multiple_of(16), "short initial kernel auxv");
    let mut random = None;
    for pair in bytes.as_chunks::<16>().0 {
        let kind = word(pair, 0)?;
        let value = word(pair, 8)?;
        if kind == libc::AT_NULL as usize {
            ensure!(value == 0, "invalid auxv terminator");
            return random.ok_or_else(|| anyhow!("initial kernel auxv has no AT_RANDOM"));
        }
        if kind == libc::AT_RANDOM as usize {
            ensure!(
                value != 0 && random.replace(value).is_none(),
                "ambiguous initial kernel AT_RANDOM"
            );
        }
    }
    Err(anyhow!("unterminated initial kernel auxv"))
}

impl Bootstrap {
    pub(super) fn new(root: Pid, launch: Launch) -> Result<Self> {
        let rows = maps(root)?;
        let candidates: Vec<_> = rows
            .iter()
            .filter(|row| row.path == b"[vdso]")
            .cloned()
            .collect();
        ensure!(
            candidates.len() == 1,
            "missing/ambiguous initial kernel vDSO"
        );
        let vdso = candidates.into_iter().next().unwrap();
        ensure!(
            vdso.permissions == "r-xp" && vdso.inode == 0 && vdso.offset == 0,
            "unsupported initial vDSO mapping"
        );
        let vdso_bytes = remote_bytes(root, vdso.start, vdso.end - vdso.start)?;
        let elf = object::File::parse(vdso_bytes.as_slice())?;
        ensure!(
            elf.architecture() == object::Architecture::X86_64 && elf.is_little_endian(),
            "invalid initial kernel vDSO ELF"
        );
        let initial_random = auxv_random(&read_path(format!("/proc/{root}/auxv"), 4096)?)?;
        let generation = generation(root)?;
        ensure!(generation != 0, "invalid initial process generation");
        let prng = root_prng(launch.config.rng_seed());
        Ok(Self {
            launch,
            root,
            generation,
            image: None,
            prng,
            taken: false,
            sigill: None,
            initial_random,
            vdso,
            vdso_bytes,
            other_objects: Vec::new(),
            continuations: BTreeMap::new(),
            static_reexec_observed: false,
        })
    }

    fn same_owner(&self, pid: Pid) -> Result<()> {
        ensure!(
            pid == self.root && generation(pid)? == self.generation,
            "bootstrap owner or process generation changed"
        );
        Ok(())
    }

    fn syscall_site(&self, pid: Pid, rows: &[Map], site: usize) -> Result<()> {
        let loader = &self.launch.loader;
        let bias = loader.bias(pid, rows)?;
        let symbol = loader.symbol(bootstrap::SYSCALL_SYMBOL)?;
        ensure!(
            site == bias
                .checked_add(symbol)
                .ok_or_else(|| anyhow!("loader syscall address overflow"))?,
            "private bootstrap request is not at the retained loader instruction"
        );
        let row = containing(rows, site, 2)?;
        ensure!(
            row.permissions.contains('x'),
            "private bootstrap instruction is not executable"
        );
        loader.authenticate(pid, row)?;
        ensure!(
            loader.at_virtual(symbol, 2, false)? == [0x0f, 0x05]
                && remote_bytes(pid, site, 2)? == [0x0f, 0x05],
            "private bootstrap instruction bytes changed"
        );
        // Recheck the complete immutable descriptor in its live readonly load
        // mapping as well as the pre-launch held-file extraction.
        let descriptor = loader.symbol(LAYOUT_SYMBOL)?;
        let address = bias
            .checked_add(descriptor)
            .ok_or_else(|| anyhow!("descriptor address overflow"))?;
        let mapping = containing(rows, address, 72)?;
        ensure!(
            mapping.permissions.starts_with("r-") && !mapping.permissions.contains('w'),
            "live bootstrap descriptor is writable"
        );
        loader.authenticate(pid, mapping)?;
        ensure!(
            remote_bytes(pid, address, 72)? == loader.at_virtual(descriptor, 72, true)?,
            "live bootstrap descriptor differs from held object"
        );
        Ok(())
    }

    fn image(&mut self, pid: Pid, rows: &[Map], stack: usize, entry: usize) -> Result<i64> {
        ensure!(
            self.image.is_none() && !self.taken,
            "duplicate bootstrap IMAGE"
        );
        ensure!(stack.is_multiple_of(16), "unaligned final guest stack");
        let stack_map = containing(rows, stack, 8)?;
        ensure!(
            stack_map.permissions.starts_with("rw") && stack_map.path == b"[stack]",
            "final guest stack is not the owned writable stack"
        );
        let length = (stack_map.end - stack).min(64 * 1024);
        let bytes = remote_bytes(pid, stack, length)?;
        let argc = word(&bytes, 0)?;
        ensure!(argc > 0 && argc <= 4096, "unsupported bootstrap argc");
        let mut at = 8;
        for _ in 0..argc {
            let pointer = word(&bytes, at)?;
            ensure!(
                stack_map.contains(pointer, 1),
                "argv pointer leaves initial stack"
            );
            at += 8;
        }
        ensure!(word(&bytes, at)? == 0, "missing final argv terminator");
        at += 8;
        let mut env_count = 0;
        loop {
            let pointer = word(&bytes, at)?;
            at += 8;
            if pointer == 0 {
                break;
            }
            env_count += 1;
            ensure!(
                env_count <= 4096 && stack_map.contains(pointer, 1),
                "unsupported final environment extent"
            );
        }
        let mut aux = std::collections::BTreeMap::new();
        let mut terminated = false;
        for _ in 0..128 {
            let kind = word(&bytes, at)?;
            let value = word(&bytes, at + 8)?;
            at += 16;
            if kind == libc::AT_NULL as usize {
                ensure!(value == 0, "bad final auxv terminator");
                terminated = true;
                break;
            }
            ensure!(
                aux.insert(kind, value).is_none(),
                "duplicate final auxv entry"
            );
        }
        ensure!(terminated, "unterminated final auxv");
        let get = |key: u64| {
            aux.get(&(key as usize))
                .copied()
                .ok_or_else(|| anyhow!("missing final auxv field {key}"))
        };
        let random = get(libc::AT_RANDOM)?;
        ensure!(
            random == self.initial_random && stack_map.contains(random, 16),
            "final AT_RANDOM is not the original owned writable target"
        );
        let program = &self.launch.program;
        let bias = program.bias(pid, rows)?;
        let elf = object::File::parse(program.bytes.as_slice())?;
        let expected_entry = bias
            .checked_add(elf.entry() as usize)
            .ok_or_else(|| anyhow!("guest entry overflow"))?;
        ensure!(
            get(libc::AT_ENTRY)? == expected_entry,
            "final guest entry differs from held ELF"
        );
        let entry_map = containing(rows, expected_entry, 1)?;
        program.authenticate(pid, entry_map)?;
        ensure!(
            entry_map.permissions.contains('x'),
            "guest entry not executable"
        );
        let phoff = word(&program.bytes, 32)?;
        let phnum = u16::from_le_bytes(program.bytes[56..58].try_into()?) as usize;
        ensure!(
            get(libc::AT_PHNUM)? == phnum && get(libc::AT_PHENT)? == 56,
            "final program header count/size changed"
        );
        let phaddr = get(libc::AT_PHDR)?;
        let phlen = phnum
            .checked_mul(56)
            .ok_or_else(|| anyhow!("program header extent overflow"))?;
        let phmap = containing(rows, phaddr, phlen)?;
        program.authenticate(pid, phmap)?;
        let phvirtual = phaddr
            .checked_sub(bias)
            .ok_or_else(|| anyhow!("program header bias underflow"))?;
        let expected = program
            .bytes
            .get(
                phoff
                    ..phoff
                        .checked_add(phlen)
                        .ok_or_else(|| anyhow!("program header range overflow"))?,
            )
            .ok_or_else(|| anyhow!("short program headers"))?;
        ensure!(
            program.at_virtual(phvirtual, phlen, false)? == expected
                && remote_bytes(pid, phaddr, phlen)? == expected,
            "final program headers differ from held ELF"
        );
        self.launch.authenticate_script(pid)?;
        let interpreter = self
            .launch
            .interpreter
            .as_ref()
            .ok_or_else(|| anyhow!("initial static image must not request random IMAGE"))?;
        let interp_bias = interpreter.bias(pid, rows)?;
        let interp = object::File::parse(interpreter.bytes.as_slice())?;
        ensure!(
            entry
                == interp_bias
                    .checked_add(interp.entry() as usize)
                    .ok_or_else(|| anyhow!("interpreter entry overflow"))?,
            "loader final entry is not held guest interpreter"
        );
        let row = containing(rows, entry, 1)?;
        interpreter.authenticate(pid, row)?;
        ensure!(
            row.permissions.contains('x'),
            "interpreter entry not executable"
        );
        initialize_auxv(
            &mut self.prng,
            RemoteMemory(pid),
            AddrMut::from_raw(random).ok_or_else(|| anyhow!("null final AT_RANDOM"))?,
            detcore::types::DetTid::from_raw(pid.as_raw()),
        )?;
        self.image = Some(InitialImage {
            pid: pid.as_raw(),
            start_time_ticks: self.generation,
            at_random: random,
        });
        Ok(0)
    }

    fn original_bytes(
        &mut self,
        pid: Pid,
        rows: &[Map],
        site: usize,
        length: usize,
    ) -> Result<Vec<u8>> {
        ensure!(length <= 128, "original instruction read exceeds bound");
        let row = containing(rows, site, length)?.clone();
        ensure!(
            row.permissions.contains('x'),
            "early getrandom source is not executable"
        );
        if row.path == b"[vdso]" {
            ensure!(row == self.vdso, "initial kernel vDSO mapping changed");
            let offset = site - self.vdso.start;
            Ok(self
                .vdso_bytes
                .get(offset..offset + length)
                .ok_or_else(|| anyhow!("short original vDSO instruction range"))?
                .to_vec())
        } else {
            let object = if row.path == self.launch.program.path.as_os_str().as_bytes() {
                &self.launch.program
            } else if let Some(interpreter) = self.launch.interpreter.as_ref()
                && row.path == interpreter.path.as_os_str().as_bytes()
            {
                interpreter
            } else {
                ensure!(
                    row.unescaped_path()?.starts_with('/') && row.inode != 0,
                    "unsupported anonymous early getrandom source"
                );
                if !self
                    .other_objects
                    .iter()
                    .any(|object| object.path.as_os_str().as_bytes() == row.path)
                {
                    ensure!(
                        self.other_objects.len() < MAX_OBJECTS,
                        "too many early bootstrap source objects"
                    );
                    self.other_objects
                        .push(HeldObject::open(Path::new(row.unescaped_path()?))?);
                }
                self.other_objects
                    .iter()
                    .find(|object| object.path.as_os_str().as_bytes() == row.path)
                    .unwrap()
            };
            object.authenticate(pid, &row)?;
            let bias = object.bias(pid, rows)?;
            let relative = site
                .checked_sub(bias)
                .ok_or_else(|| anyhow!("source bias underflow"))?;
            Ok(object.at_virtual(relative, length, false)?.to_vec())
        }
    }

    fn original_syscall(&mut self, pid: Pid, rows: &[Map], site: usize) -> Result<Map> {
        ensure!(
            self.original_bytes(pid, rows, site, 2)? == [0x0f, 0x05],
            "early source was not a syscall in the authenticated image"
        );
        Ok(containing(rows, site, 2)?.clone())
    }

    /// Save only a real kernel signal-delivery observation. A later private
    /// request's argument values or supplied stack pointer cannot create it.
    pub(super) fn signal(&mut self, pid: Pid, signal: nix::sys::signal::Signal) -> Result<()> {
        ensure!(
            self.sigill.take().is_none(),
            "intervening signal invalidated bootstrap SIGILL provenance"
        );
        if self.taken
            || !self.launch.initializes_random()
            || signal != nix::sys::signal::Signal::SIGILL
        {
            return Ok(());
        }
        self.same_owner(pid)?;
        let regs = nix::sys::ptrace::getregs(pid)?;
        if regs.rax != libc::SYS_getrandom as u64
            || remote_bytes(pid, regs.rip as usize, 2)? != [0x0f, 0xff]
        {
            return Ok(());
        }
        ensure!(
            self.image.is_some(),
            "getrandom SIGILL before authenticated IMAGE"
        );
        let info = nix::sys::ptrace::getsiginfo(pid)?;
        ensure!(
            info.si_code > 0 && unsafe { info.si_addr() } as usize == regs.rip as usize,
            "SIGILL is not a kernel fault at the rewritten source"
        );
        let rows = maps(pid)?;
        let mapping = self.original_syscall(pid, &rows, regs.rip as usize)?;
        self.sigill = Some(SigillOrigin {
            generation: self.generation,
            registers: regs,
            mapping,
        });
        Ok(())
    }

    fn origin(
        &mut self,
        pid: Pid,
        rows: &[Map],
        arguments: [usize; 3],
        wrapper: usize,
    ) -> Result<()> {
        let layout = self.launch.layout;
        if let Some(saved) = self.sigill.take() {
            ensure!(
                saved.generation == self.generation,
                "stale bootstrap SIGILL generation"
            );
            let regs = saved.registers;
            ensure!(
                [regs.rdi as usize, regs.rsi as usize, regs.rdx as usize] == arguments,
                "SIGILL original arguments differ from forwarded request"
            );
            ensure!(
                self.original_syscall(pid, rows, regs.rip as usize)? == saved.mapping
                    && remote_bytes(pid, regs.rip as usize, 2)? == [0x0f, 0xff],
                "SIGILL source changed before forwarding"
            );
            let address = wrapper
                .checked_add(layout.scratch_return)
                .ok_or_else(|| anyhow!("SIGILL return pointer overflow"))?;
            let row = containing(rows, address, 8)?;
            ensure!(
                row.path == b"[stack]" && row.permissions.starts_with("rw"),
                "SIGILL return word is not on owned stack"
            );
            let returned = word(&remote_bytes(pid, address, 8)?, 0)?;
            ensure!(
                returned == regs.rip as usize + 2,
                "SIGILL return word differs from saved kernel continuation"
            );
            return Ok(());
        }
        let row = containing(rows, wrapper, layout.size)?;
        ensure!(
            row.path == b"[stack]"
                && row.permissions.starts_with("rw")
                && wrapper.is_multiple_of(8),
            "ordinary bootstrap frame is not on owned stack"
        );
        let frame = remote_bytes(pid, wrapper, layout.size)?;
        ensure!(
            [
                word(&frame, layout.rdi)?,
                word(&frame, layout.rsi)?,
                word(&frame, layout.rdx)?
            ] == arguments,
            "assembly frame arguments differ from forwarded request"
        );
        let returned = word(&frame, layout.architectural_return)?;
        let site = returned
            .checked_sub(2)
            .ok_or_else(|| anyhow!("architectural return underflow"))?;
        self.original_syscall(pid, rows, site)?;
        let scratch = word(&frame, layout.scratch_return)?;
        // Real rewriter.c's syscall trampoline: saved return points at the
        // red-zone-restoring LEA, 33 bytes after its PUSH/LEA/body begins.
        let start = scratch
            .checked_sub(33)
            .ok_or_else(|| anyhow!("scratch return underflow"))?;
        let mapping = containing(rows, start, 41)?;
        ensure!(
            mapping.permissions.contains('x'),
            "scratch continuation is not executable"
        );
        let code = remote_bytes(pid, start, 41)?;
        ensure!(
            code[..4] == [0x50, 0x48, 0x8d, 0x05]
                && code[8..11] == [0x50, 0x48, 0xb8]
                && code[19..33]
                    == [
                        0x50, 0x48, 0x8d, 0x05, 0x06, 0, 0, 0, 0x48, 0x87, 0x44, 0x24, 0x10, 0xc3
                    ]
                && code[33..] == [0x48, 0x8d, 0xa4, 0x24, 0x80, 0, 0, 0],
            "unrecognized full-frame scratch trampoline"
        );
        let displacement = i32::from_le_bytes(code[4..8].try_into()?) as i64;
        ensure!(
            (start as i64)
                .checked_add(8)
                .and_then(|n| n.checked_add(displacement))
                == Some(returned as i64),
            "scratch trampoline names a different syscall continuation"
        );
        let handler = word(&code, 11)?;
        let bias = self.launch.loader.bias(pid, rows)?;
        let mut handlers = BTreeSet::new();
        for symbol in ["handle_syscall", "handle_syscall_loader"] {
            handlers.insert(
                bias.checked_add(self.launch.loader.symbol(symbol)?)
                    .ok_or_else(|| anyhow!("handler address overflow"))?,
            );
        }
        ensure!(
            handlers.contains(&handler),
            "scratch trampoline does not call the held loader wrapper"
        );
        // Bind the source jump to this exact scratch body, including the real
        // SYSCALL clobber/red-zone prefix and both relocated byte sequences.
        // The production rewriter keeps five instructions in its ring; four
        // preceding/following x86 instructions occupy at most 4 * 15 bytes.
        // A matching frame and an unrelated executable byte pattern alone are
        // not evidence that this original instruction enters that trampoline.
        let prefix = start
            .checked_sub(17)
            .ok_or_else(|| anyhow!("scratch prefix underflow"))?;
        let prefix_bytes = remote_bytes(pid, prefix, 17)?;
        ensure!(
            prefix_bytes[..13]
                == [
                    0x48, 0x8d, 0x64, 0x24, 0x80, 0x90, 0x90, 0x9c, 0x41, 0x5b, 0x48, 0x8d, 0x0d,
                ]
                && (start as i64)
                    .checked_add(i32::from_le_bytes(prefix_bytes[13..17].try_into()?) as i64)
                    == Some(returned as i64),
            "scratch prefix does not preserve the original SYSCALL continuation"
        );
        let source_map = containing(rows, site, 2)?;
        let mut matched = 0;
        for pre in 0..=60 {
            let Some(jump) = site.checked_sub(pre) else {
                continue;
            };
            let Some(destination) = prefix.checked_sub(pre) else {
                continue;
            };
            if !source_map.contains(jump, 5) {
                continue;
            }
            let entry = remote_bytes(pid, jump, 5)?;
            if entry[0] != 0xe9
                || (jump as i64 + 5).checked_add(i32::from_le_bytes(entry[1..5].try_into()?) as i64)
                    != Some(destination as i64)
            {
                continue;
            }
            ensure!(
                containing(rows, destination, pre + 17)?
                    .permissions
                    .contains('x'),
                "scratch entry is not executable"
            );
            if pre != 0
                && self.original_bytes(pid, rows, jump, pre)?
                    != remote_bytes(pid, destination, pre)?
            {
                continue;
            }
            for post in 0..=60 {
                if pre + 2 + post < 5 || !source_map.contains(jump, pre + 2 + post) {
                    continue;
                }
                let after = start + 41 + post;
                if !mapping.contains(after, 5) {
                    continue;
                }
                let exit = remote_bytes(pid, after, 5)?;
                if exit[0] != 0xe9
                    || (after as i64 + 5)
                        .checked_add(i32::from_le_bytes(exit[1..5].try_into()?) as i64)
                        != Some((returned + post) as i64)
                {
                    continue;
                }
                if post != 0
                    && self.original_bytes(pid, rows, returned, post)?
                        != remote_bytes(pid, start + 41, post)?
                {
                    continue;
                }
                if pre + 2 + post > 5
                    && remote_bytes(pid, jump + 5, pre + 2 + post - 5)?
                        .iter()
                        .any(|byte| *byte != 0x90)
                {
                    continue;
                }
                matched += 1;
            }
        }
        ensure!(
            matched == 1,
            "missing/ambiguous rewritten source-to-scratch linkage"
        );
        ensure!(
            remote_bytes(pid, site, 2)? != [0x0f, 0x05]
                && remote_bytes(pid, site, 2)? != [0x0f, 0xff],
            "ordinary assembly path lacks a rewritten source site"
        );
        Ok(())
    }

    /// None leaves the existing supervisor path untouched. Protocol ownership,
    /// phase and shape errors abort the run; syscall errors remain Linux results.
    pub(super) fn request(
        &mut self,
        pid: Pid,
        regs: &libc::user_regs_struct,
    ) -> Result<Option<i64>> {
        let private =
            regs.orig_rax == libc::SYS_prctl as u64 && regs.rdi == bootstrap::PRCTL_OPTION;
        if !private {
            ensure!(
                self.sigill.take().is_none(),
                "intervening syscall invalidated bootstrap SIGILL provenance"
            );
            return Ok(None);
        }
        let continuation = self.continuations.get(&pid).copied();
        if let Some(image) = continuation {
            ensure!(
                generation(pid)? == image.start_time_ticks,
                "continuation owner or process generation changed"
            );
        } else {
            self.same_owner(pid)?;
        }
        let rows = maps(pid)?;
        let site = (regs.rip as usize)
            .checked_sub(2)
            .ok_or_else(|| anyhow!("private syscall RIP underflow"))?;
        self.syscall_site(pid, &rows, site)?;
        let initial_static = !self.taken && !self.launch.initializes_random();
        if continuation.is_some() || initial_static {
            ensure!(
                self.sigill.is_none()
                    && regs.rsi == bootstrap::TAKE_STATE
                    && regs.r8 == bootstrap::VERSION
                    && regs.r9 == 0,
                "legacy continuation requires the exact TAKE protocol"
            );
            let image = if let Some(image) = continuation {
                image
            } else {
                // Static clients initialize their plugin before final-stack
                // compaction. Authenticate the actual mapped held program,
                // while leaving their ordinary auxv/ThreadState path alone.
                self.launch.authenticate_script(pid)?;
                let program = &self.launch.program;
                let bias = program.bias(pid, &rows)?;
                let elf = object::File::parse(program.bytes.as_slice())?;
                let entry = bias
                    .checked_add(elf.entry() as usize)
                    .ok_or_else(|| anyhow!("static entry overflow"))?;
                let row = containing(&rows, entry, 1)?;
                ensure!(
                    row.permissions.contains('x'),
                    "held static entry not executable"
                );
                program.authenticate(pid, row)?;
                InitialImage {
                    pid: pid.as_raw(),
                    start_time_ticks: self.generation,
                    at_random: self.initial_random,
                }
            };
            self.check_current_auxv(pid, &rows, image)?;
            let state = if initial_static {
                LoaderState::InitialStaticLegacy
            } else {
                LoaderState::ObservedExecContinuation
            };
            let bytes = encode_continuation(&self.launch.config, image, state)?;
            let result = Self::write_take(pid, regs, &bytes)?;
            if result > 0 {
                if initial_static {
                    self.taken = true;
                }
                self.continuations.remove(&pid);
            }
            return Ok(Some(result));
        }
        ensure!(!self.taken, "bootstrap state already handed off");
        let result = match regs.rsi {
            bootstrap::IMAGE => {
                ensure!(
                    self.sigill.is_none() && regs.r8 == bootstrap::VERSION && regs.r9 == 0,
                    "invalid IMAGE protocol shape"
                );
                self.image(pid, &rows, regs.rdx as usize, regs.r10 as usize)?
            }
            bootstrap::GETRANDOM => {
                ensure!(self.image.is_some(), "early getrandom before IMAGE");
                let args = [regs.rdx as usize, regs.r10 as usize, regs.r8 as usize];
                self.origin(pid, &rows, args, regs.r9 as usize)?;
                let call = Syscall::from_raw(
                    Sysno::getrandom,
                    SyscallArgs::new(args[0], args[1], args[2], 0, 0, 0),
                );
                let Syscall::Getrandom(call) = call else {
                    unreachable!()
                };
                match getrandom(
                    &mut self.prng,
                    RemoteMemory(pid),
                    detcore::types::DetTid::from_raw(pid.as_raw()),
                    call,
                ) {
                    Ok(n) => n,
                    Err(e) => -(e.into_raw() as i64),
                }
            }
            bootstrap::TAKE_STATE => {
                ensure!(
                    self.sigill.is_none() && regs.r8 == bootstrap::VERSION && regs.r9 == 0,
                    "invalid TAKE protocol shape"
                );
                let image = self.image.ok_or_else(|| anyhow!("TAKE before IMAGE"))?;
                self.check_current_auxv(pid, &rows, image)?;
                let bytes = encode_initial_state(&self.launch.config, image, &self.prng)?;
                let result = Self::write_take(pid, regs, &bytes)?;
                if result > 0 {
                    self.taken = true;
                }
                result
            }
            _ => return Err(anyhow!("unknown loader bootstrap operation")),
        };
        Ok(Some(result))
    }

    fn write_take(pid: Pid, regs: &libc::user_regs_struct, bytes: &[u8]) -> Result<i64> {
        let capacity = regs.r10 as usize;
        let destination = AddrMut::from_raw(regs.rdx as usize);
        if capacity == 0 || capacity > bootstrap::MAX_STATE_BYTES || destination.is_none() {
            return Ok(-libc::EINVAL as i64);
        }
        if bytes.len() > capacity {
            return Ok(-libc::EMSGSIZE as i64);
        }
        Ok(
            match RemoteMemory(pid).write_exact(destination.unwrap(), bytes) {
                Ok(()) => bytes.len() as i64,
                Err(e) => -(e.into_raw() as i64),
            },
        )
    }

    fn check_current_auxv(&self, pid: Pid, rows: &[Map], image: InitialImage) -> Result<()> {
        ensure!(
            generation(pid)? == image.start_time_ticks
                && pid.as_raw() == image.pid
                && auxv_random(&read_path(format!("/proc/{pid}/auxv"), 4096)?)? == image.at_random,
            "loader handoff image or kernel auxv changed"
        );
        let row = containing(rows, image.at_random, 16)?;
        ensure!(
            row.path == b"[stack]" && row.permissions.starts_with("rw"),
            "loader handoff AT_RANDOM is not the owned writable stack"
        );
        Ok(())
    }

    pub(super) fn forget(&mut self, pid: Pid) {
        self.continuations.remove(&pid);
    }

    /// Called only for a real kernel event on the supervisor's owned lineage,
    /// before it resumes that image. An exec replaces any unconsumed earlier
    /// image proof; a mere matching pid/start or user-supplied payload cannot
    /// create one. Entries live no longer than the existing owned tracee set.
    pub(super) fn event(&mut self, pid: Pid, event: libc::c_int) -> Result<()> {
        ensure!(
            self.sigill.take().is_none(),
            "intervening ptrace event invalidated bootstrap SIGILL provenance"
        );
        if event == libc::PTRACE_EVENT_EXEC {
            let image = InitialImage {
                pid: pid.as_raw(),
                start_time_ticks: generation(pid)?,
                at_random: auxv_random(&read_path(format!("/proc/{pid}/auxv"), 4096)?)?,
            };
            self.check_current_auxv(pid, &maps(pid)?, image)?;
            if self.taken {
                self.continuations.insert(pid, image);
                return Ok(());
            }
            if !self.launch.initializes_random() && !self.static_reexec_observed {
                self.same_owner(pid)?;
                // The static loader may exec its dynamic linker once to
                // preload the plugin. This does not authorize arbitrary initial
                // dynamic execution: TAKE must still prove the original held
                // static program and exact loader mapping before succeeding.
                self.static_reexec_observed = true;
                self.initial_random = image.at_random;
                return Ok(());
            }
        }
        ensure!(
            self.taken
                || !matches!(
                    event,
                    libc::PTRACE_EVENT_CLONE
                        | libc::PTRACE_EVENT_FORK
                        | libc::PTRACE_EVENT_VFORK
                        | libc::PTRACE_EVENT_EXEC
                ),
            "clone/fork/exec before initial random handoff is unsupported"
        );
        Ok(())
    }
}
