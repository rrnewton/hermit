/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Opt-in core files for guest threads killed by a core-dumping signal.
//!
//! The ptrace backend offers each such thread at its exit stop
//! ([`reverie::GlobalTool::on_fatal_signal_exit`]): after the kernel's fatal
//! decision, while the thread's memory is still mapped. [`capture`] then
//! writes one ELF core per process, compressed as a sequence of zstd frames,
//! into [`FatalCoreCapture::dir`]. It only reads the stopped guest, through
//! procfs, so the guest observes nothing and its exit status is already
//! fixed.
//!
//! The capture is best effort and bounded. One core never exceeds
//! `max_core_bytes`; the regular files in the directory never exceed
//! `max_total_bytes` together, counted under a directory lock that every
//! writer takes. A core that would break either cap, or that takes too long,
//! is retried with less memory: first only the stack the thread was running
//! on, then only the notes (registers, signal, process identity, auxiliary
//! vector and mapped files) and the mapping layout. A core that cannot fit
//! even then is not written. Every failure is returned for the caller to log.
//!
//! The registers are those of the first thread of the process offered here,
//! which is not necessarily the thread that took the signal when several
//! threads were running.

use std::fs;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use ::procfs::process::MMPermissions;
use ::procfs::process::MMapPath;
use ::procfs::process::MemoryMap;
use ::procfs::process::Process;
use reverie::FatalSignalExit;
use ruzstd::encoding::CompressionLevel;

use crate::config::FatalCoreCapture;

/// Uncompressed bytes per zstd frame.
const CHUNK: usize = 1 << 20;
const PAGE: u64 = 4096;
static ZEROS: [u8; CHUNK] = [0; CHUNK];

/// What one [`capture`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// A core was written to `path`, `bytes` long after compression.
    Written {
        path: PathBuf,
        bytes: u64,
        tier: Tier,
    },
    /// This run already holds a core for the thread's process.
    AlreadyCaptured,
    /// No tier fitted in `budget` bytes within the time limit.
    NoRoom { budget: u64 },
}

/// How much memory a core holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    /// Every anonymous, writable, stack, heap and vDSO mapping, and the first
    /// page of each file mapped from offset 0 (its ELF header).
    Full,
    /// Only the mapping holding the stack pointer and the main stack, plus
    /// the ELF header pages.
    Stack,
    /// No memory: notes and the mapping layout only.
    NotesOnly,
}

/// Captures `exit` under `cfg` and reports the outcome on stderr. Never fails
/// the run.
///
/// The report goes to stderr, not to the tracing log: a `--verify` run compares
/// its two runs' logs, and a report that names the core's size or tier, which
/// depend on host timing, would make two identical runs differ.
pub(crate) fn capture_and_log(cfg: &FatalCoreCapture, exit: &FatalSignalExit) {
    let report = match capture(cfg, exit) {
        Ok(Outcome::Written { path, bytes, tier }) => format!(
            "kept as {} ({bytes} bytes, {tier:?})",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        Ok(Outcome::AlreadyCaptured) => return,
        Ok(Outcome::NoRoom { budget }) => format!(
            "not kept: no core fits in {budget} bytes within {:?}",
            cfg.time_limit
        ),
        Err(error) => format!("not kept: {error:#}"),
    };
    let line = format!(
        "hermit: fatal core for thread {} ({}): {report}\n",
        exit.tid, exit.signal
    );
    crate::nonwaiting_write::write_without_waiting(libc::STDERR_FILENO, line.as_bytes());
}

/// Writes a core for the thread `exit` names, held at its exit stop.
pub(crate) fn capture(cfg: &FatalCoreCapture, exit: &FatalSignalExit) -> anyhow::Result<Outcome> {
    let start = Instant::now();
    let tid = exit.tid.as_raw();
    let process = Process::new(tid)?;
    let snapshot = Snapshot::read(&process, exit)?;
    fs::create_dir_all(&cfg.dir)?;
    let _lock = DirLock::acquire(&cfg.dir, start + cfg.time_limit)?;
    let stem = format!("{}core.{}.", cfg.file_prefix, snapshot.tgid);
    if has_entry_with_prefix(&cfg.dir, &stem)? {
        return Ok(Outcome::AlreadyCaptured);
    }
    let used = regular_file_bytes(&cfg.dir)?;
    let budget = cfg
        .max_core_bytes
        .min(cfg.max_total_bytes.saturating_sub(used));
    let name = format!("{stem}{tid}.{}.zst", exit.signal.as_str());
    let path = cfg.dir.join(&name);
    let temp = cfg.dir.join(format!(".{name}.tmp"));
    let mem = File::open(format!("/proc/{tid}/mem"))?;
    for (tier, tenths) in [(Tier::Full, 7), (Tier::Stack, 9), (Tier::NotesOnly, 10)] {
        let deadline = start + cfg.time_limit * tenths / 10;
        let segments = snapshot.segments(tier);
        match write_core(&temp, budget, deadline, &snapshot, &segments, &mem) {
            Ok(bytes) => {
                fs::rename(&temp, &path)?;
                return Ok(Outcome::Written { path, bytes, tier });
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                match error {
                    WriteError::OverBudget | WriteError::Deadline => continue,
                    WriteError::Io(error) => return Err(error.into()),
                }
            }
        }
    }
    Ok(Outcome::NoRoom { budget })
}

/// An exclusive `flock` on the core directory itself, held for the whole
/// sizing and writing of one core, so two writers cannot both spend the same
/// remaining total. Locking the directory leaves no lock file behind.
struct DirLock {
    _held: File,
}

impl DirLock {
    fn acquire(dir: &Path, deadline: Instant) -> anyhow::Result<Self> {
        let file = File::open(dir)?;
        loop {
            // SAFETY: flock on a descriptor this function owns.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self { _held: file });
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error.into());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for the lock on {}", dir.display());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn has_entry_with_prefix(dir: &Path, prefix: &str) -> std::io::Result<bool> {
    for entry in fs::read_dir(dir)? {
        if entry?.file_name().to_string_lossy().starts_with(prefix) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Bytes held by the regular files directly in `dir`.
fn regular_file_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir)? {
        let metadata = entry?.metadata()?;
        if metadata.file_type().is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

/// Everything a core needs except memory contents, read before writing.
struct Snapshot {
    tgid: i32,
    stack_pointer: u64,
    notes: Vec<u8>,
    maps: Vec<MemoryMap>,
}

impl Snapshot {
    fn read(process: &Process, exit: &FatalSignalExit) -> anyhow::Result<Self> {
        let status = process.status()?;
        let stat = process.stat()?;
        let maps = process.maps()?.0;
        let cmdline = process.cmdline().unwrap_or_default().join(" ");
        let auxv = fs::read(format!("/proc/{}/auxv", exit.tid.as_raw())).ok();
        let identity = Identity {
            tid: exit.tid.as_raw(),
            tgid: status.tgid,
            ppid: stat.ppid,
            pgrp: stat.pgrp,
            sid: stat.session,
            uid: status.ruid,
            gid: status.rgid,
            sigpend: status.sigpnd | status.shdpnd,
            sighold: status.sigblk,
            state: stat.state,
            nice: stat.nice,
            flags: stat.flags,
            comm: stat.comm,
            cmdline,
        };
        let mut notes = Vec::new();
        note(
            &mut notes,
            NT_PRSTATUS,
            &arch::prstatus(exit.signal as i32, &identity, &exit.regs),
        );
        note(&mut notes, NT_PRPSINFO, &prpsinfo(&identity));
        if let Some(auxv) = auxv {
            note(&mut notes, NT_AUXV, &auxv);
        }
        note(&mut notes, NT_FILE, &nt_file(&maps));
        Ok(Self {
            tgid: status.tgid,
            stack_pointer: arch::stack_pointer(&exit.regs),
            notes,
            maps,
        })
    }

    fn segments(&self, tier: Tier) -> Vec<Segment> {
        self.maps
            .iter()
            .filter(|map| dumpable(map))
            .map(|map| {
                let (start, end) = map.address;
                let len = end - start;
                let all = match tier {
                    Tier::Full => {
                        map.inode == 0
                            || map.perms.contains(MMPermissions::WRITE)
                            || matches!(
                                map.pathname,
                                MMapPath::Stack
                                    | MMapPath::TStack(_)
                                    | MMapPath::Heap
                                    | MMapPath::Vdso
                            )
                    }
                    Tier::Stack => {
                        (start..end).contains(&self.stack_pointer)
                            || matches!(map.pathname, MMapPath::Stack)
                    }
                    Tier::NotesOnly => false,
                };
                let elf_header = tier != Tier::NotesOnly
                    && map.offset == 0
                    && matches!(map.pathname, MMapPath::Path(_));
                let filesz = if all {
                    len
                } else if elf_header {
                    len.min(PAGE)
                } else {
                    0
                };
                Segment {
                    vaddr: start,
                    memsz: len,
                    filesz,
                    flags: segment_flags(map.perms),
                }
            })
            .collect()
    }
}

/// Whether a mapping can appear in a core: procfs cannot read the vsyscall
/// page or the vvar pages, and an unreadable mapping has nothing to read.
fn dumpable(map: &MemoryMap) -> bool {
    map.perms.contains(MMPermissions::READ)
        && !matches!(map.pathname, MMapPath::Vvar | MMapPath::Vsyscall)
        && !matches!(&map.pathname, MMapPath::Other(name) if name.starts_with("vvar"))
}

fn segment_flags(perms: MMPermissions) -> u32 {
    let mut flags = 0;
    if perms.contains(MMPermissions::EXECUTE) {
        flags |= PF_X;
    }
    if perms.contains(MMPermissions::WRITE) {
        flags |= PF_W;
    }
    if perms.contains(MMPermissions::READ) {
        flags |= PF_R;
    }
    flags
}

/// One `PT_LOAD`: `filesz` bytes from `vaddr` are stored, the rest of
/// `memsz` is described but not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    vaddr: u64,
    memsz: u64,
    filesz: u64,
    flags: u32,
}

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;
const NT_PRSTATUS: u32 = 1;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_FILE: u32 = 0x4649_4c45;
const EHDR_SIZE: u64 = 64;
const PHDR_SIZE: u64 = 56;

/// The fields of the notes that do not come from registers.
struct Identity {
    tid: i32,
    tgid: i32,
    ppid: i32,
    pgrp: i32,
    sid: i32,
    uid: u32,
    gid: u32,
    sigpend: u64,
    sighold: u64,
    state: char,
    nice: i64,
    flags: u32,
    comm: String,
    cmdline: String,
}

fn note(out: &mut Vec<u8>, kind: u32, desc: &[u8]) {
    const NAME: &[u8] = b"CORE\0";
    out.extend_from_slice(&(NAME.len() as u32).to_le_bytes());
    out.extend_from_slice(&(desc.len() as u32).to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(NAME);
    pad4(out);
    out.extend_from_slice(desc);
    pad4(out);
}

fn pad4(out: &mut Vec<u8>) {
    out.resize(out.len().next_multiple_of(4), 0);
}

fn put(out: &mut [u8], offset: usize, bytes: &[u8]) {
    out[offset..offset + bytes.len()].copy_from_slice(bytes);
}

/// `struct elf_prpsinfo` for a 64-bit process (136 bytes).
fn prpsinfo(id: &Identity) -> Vec<u8> {
    let mut out = vec![0u8; 136];
    let state = id.state as u8;
    out[0] = b"RSDTZW".iter().position(|&c| c == state).unwrap_or(0) as u8;
    out[1] = state;
    out[2] = u8::from(state == b'Z');
    out[3] = id.nice as i8 as u8;
    put(&mut out, 8, &u64::from(id.flags).to_le_bytes());
    put(&mut out, 16, &id.uid.to_le_bytes());
    put(&mut out, 20, &id.gid.to_le_bytes());
    put(&mut out, 24, &id.tgid.to_le_bytes());
    put(&mut out, 28, &id.ppid.to_le_bytes());
    put(&mut out, 32, &id.pgrp.to_le_bytes());
    put(&mut out, 36, &id.sid.to_le_bytes());
    let comm = id.comm.as_bytes();
    put(&mut out, 40, &comm[..comm.len().min(15)]);
    let args = id.cmdline.as_bytes();
    put(&mut out, 56, &args[..args.len().min(79)]);
    out
}

/// `NT_FILE`: the file-backed mappings, so a debugger can find the files.
fn nt_file(maps: &[MemoryMap]) -> Vec<u8> {
    let files: Vec<(&MemoryMap, &Path)> = maps
        .iter()
        .filter_map(|map| match &map.pathname {
            MMapPath::Path(path) => Some((map, path.as_path())),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&(files.len() as u64).to_le_bytes());
    out.extend_from_slice(&PAGE.to_le_bytes());
    for (map, _) in &files {
        out.extend_from_slice(&map.address.0.to_le_bytes());
        out.extend_from_slice(&map.address.1.to_le_bytes());
        out.extend_from_slice(&(map.offset / PAGE).to_le_bytes());
    }
    for (_, path) in &files {
        use std::os::unix::ffi::OsStrExt;
        out.extend_from_slice(path.as_os_str().as_bytes());
        out.push(0);
    }
    out
}

#[cfg(target_arch = "x86_64")]
mod arch {
    use super::Identity;
    use super::put;

    pub(super) const MACHINE: u16 = 62; // EM_X86_64

    pub(super) fn stack_pointer(regs: &libc::user_regs_struct) -> u64 {
        regs.rsp
    }

    /// `struct elf_prstatus` for x86-64 (336 bytes).
    pub(super) fn prstatus(signal: i32, id: &Identity, regs: &libc::user_regs_struct) -> Vec<u8> {
        const REGS: usize = std::mem::size_of::<libc::user_regs_struct>();
        const _: () = assert!(REGS == 216);
        let mut out = vec![0u8; 336];
        put(&mut out, 0, &signal.to_le_bytes()); // pr_info.si_signo
        put(&mut out, 12, &(signal as i16).to_le_bytes()); // pr_cursig
        put(&mut out, 16, &id.sigpend.to_le_bytes());
        put(&mut out, 24, &id.sighold.to_le_bytes());
        put(&mut out, 32, &id.tid.to_le_bytes());
        put(&mut out, 36, &id.ppid.to_le_bytes());
        put(&mut out, 40, &id.pgrp.to_le_bytes());
        put(&mut out, 44, &id.sid.to_le_bytes());
        // SAFETY: user_regs_struct is plain old data of exactly REGS bytes.
        let bytes = unsafe { std::slice::from_raw_parts(regs as *const _ as *const u8, REGS) };
        put(&mut out, 112, bytes);
        out
    }
}

#[cfg(not(target_arch = "x86_64"))]
mod arch {
    use super::Identity;

    pub(super) const MACHINE: u16 = 0;

    pub(super) fn stack_pointer(regs: &libc::user_regs_struct) -> u64 {
        regs.sp
    }

    pub(super) fn prstatus(_: i32, _: &Identity, _: &libc::user_regs_struct) -> Vec<u8> {
        Vec::new()
    }
}

#[derive(Debug)]
enum WriteError {
    OverBudget,
    Deadline,
    Io(std::io::Error),
}

impl From<std::io::Error> for WriteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Compresses what is pushed into zstd frames of [`CHUNK`] bytes and appends
/// them to a file, refusing any frame that would pass `budget` or start after
/// `deadline`.
struct Compressed {
    file: File,
    buf: Vec<u8>,
    written: u64,
    budget: u64,
    deadline: Instant,
}

impl Compressed {
    fn push(&mut self, mut data: &[u8]) -> Result<(), WriteError> {
        while !data.is_empty() {
            let take = data.len().min(CHUNK - self.buf.len());
            self.buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buf.len() == CHUNK {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn push_zeros(&mut self, mut len: u64) -> Result<(), WriteError> {
        while len > 0 {
            let take = len.min(CHUNK as u64) as usize;
            self.push(&ZEROS[..take])?;
            len -= take as u64;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), WriteError> {
        if self.buf.is_empty() {
            return Ok(());
        }
        if Instant::now() >= self.deadline {
            return Err(WriteError::Deadline);
        }
        let frame = ruzstd::encoding::compress_to_vec(&self.buf[..], CompressionLevel::Fastest);
        self.buf.clear();
        if self.written + frame.len() as u64 > self.budget {
            return Err(WriteError::OverBudget);
        }
        self.file.write_all(&frame)?;
        self.written += frame.len() as u64;
        Ok(())
    }
}

/// The ELF header and program headers for `segments`, with their stored
/// bytes laid out page-aligned after `notes_len` bytes of notes.
fn headers(notes_len: u64, segments: &[Segment]) -> (Vec<u8>, u64) {
    let phnum = 1 + segments.len() as u64;
    let notes_offset = EHDR_SIZE + PHDR_SIZE * phnum;
    let data_offset = (notes_offset + notes_len).next_multiple_of(PAGE);
    let mut out = Vec::with_capacity(notes_offset as usize);
    out.extend_from_slice(b"\x7fELF\x02\x01\x01");
    out.resize(16, 0);
    out.extend_from_slice(&4u16.to_le_bytes()); // e_type: ET_CORE
    out.extend_from_slice(&arch::MACHINE.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // e_version
    out.extend_from_slice(&0u64.to_le_bytes()); // e_entry
    out.extend_from_slice(&EHDR_SIZE.to_le_bytes()); // e_phoff
    out.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    out.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    out.extend_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
    out.extend_from_slice(&(phnum as u16).to_le_bytes());
    out.extend_from_slice(&[0; 6]); // e_shentsize, e_shnum, e_shstrndx
    let mut phdr =
        |kind: u32, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64, align: u64| {
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&vaddr.to_le_bytes());
            out.extend_from_slice(&0u64.to_le_bytes()); // p_paddr
            out.extend_from_slice(&filesz.to_le_bytes());
            out.extend_from_slice(&memsz.to_le_bytes());
            out.extend_from_slice(&align.to_le_bytes());
        };
    phdr(PT_NOTE, 0, notes_offset, 0, notes_len, 0, 4);
    let mut offset = data_offset;
    for segment in segments {
        phdr(
            PT_LOAD,
            segment.flags,
            offset,
            segment.vaddr,
            segment.filesz,
            segment.memsz,
            PAGE,
        );
        offset += segment.filesz.next_multiple_of(PAGE);
    }
    (out, data_offset)
}

fn write_core(
    temp: &Path,
    budget: u64,
    deadline: Instant,
    snapshot: &Snapshot,
    segments: &[Segment],
    mem: &File,
) -> Result<u64, WriteError> {
    let _ = fs::remove_file(temp);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temp)?;
    let mut out = Compressed {
        file,
        buf: Vec::with_capacity(CHUNK),
        written: 0,
        budget,
        deadline,
    };
    let (headers, data_offset) = headers(snapshot.notes.len() as u64, segments);
    out.push(&headers)?;
    out.push(&snapshot.notes)?;
    out.push_zeros(data_offset - headers.len() as u64 - snapshot.notes.len() as u64)?;
    let mut page = vec![0u8; CHUNK];
    for segment in segments {
        copy_memory(&mut out, mem, segment.vaddr, segment.filesz, &mut page)?;
        out.push_zeros(segment.filesz.next_multiple_of(PAGE) - segment.filesz)?;
    }
    out.flush()?;
    Ok(out.written)
}

/// Pushes `len` bytes of guest memory from `addr`; a page procfs cannot read
/// is stored as zeros.
fn copy_memory(
    out: &mut Compressed,
    mem: &File,
    mut addr: u64,
    len: u64,
    buf: &mut [u8],
) -> Result<(), WriteError> {
    let end = addr + len;
    while addr < end {
        let want = (end - addr).min(buf.len() as u64) as usize;
        match read_fully(mem, &mut buf[..want], addr) {
            Ok(()) => out.push(&buf[..want])?,
            Err(_) => {
                for page in buf[..want].chunks_mut(PAGE as usize) {
                    let page_addr = addr;
                    addr += page.len() as u64;
                    if read_fully(mem, page, page_addr).is_err() {
                        page.fill(0);
                    }
                }
                out.push(&buf[..want])?;
                continue;
            }
        }
        addr += want as u64;
    }
    Ok(())
}

fn read_fully(mem: &File, buf: &mut [u8], addr: u64) -> std::io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match mem.read_at(&mut buf[done..], addr + done as u64)? {
            0 => return Err(std::io::ErrorKind::UnexpectedEof.into()),
            n => done += n,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_lay_out_notes_then_page_aligned_segments() {
        let segments = [
            Segment {
                vaddr: 0x1000,
                memsz: 0x3000,
                filesz: 0x3000,
                flags: PF_R | PF_W,
            },
            Segment {
                vaddr: 0x10000,
                memsz: 0x2000,
                filesz: 0,
                flags: PF_R,
            },
            Segment {
                vaddr: 0x20000,
                memsz: 0x5000,
                filesz: 0x1000,
                flags: PF_R | PF_X,
            },
        ];
        let (bytes, data_offset) = headers(100, &segments);
        assert_eq!(bytes.len() as u64, EHDR_SIZE + 4 * PHDR_SIZE);
        assert_eq!(data_offset, PAGE);
        assert_eq!(&bytes[..4], b"\x7fELF");
        let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        assert_eq!(u16_at(16), 4, "e_type is ET_CORE");
        assert_eq!(u16_at(56), 4, "e_phnum");
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let phdr = |i: u64| (EHDR_SIZE + i * PHDR_SIZE) as usize;
        // PT_NOTE right after the program headers.
        assert_eq!(u64_at(phdr(0) + 8), EHDR_SIZE + 4 * PHDR_SIZE);
        assert_eq!(u64_at(phdr(0) + 32), 100);
        // Stored segments follow each other at page granularity; an empty
        // one takes no file space.
        assert_eq!(u64_at(phdr(1) + 8), PAGE);
        assert_eq!(u64_at(phdr(2) + 8), PAGE + 0x3000);
        assert_eq!(u64_at(phdr(3) + 8), PAGE + 0x3000);
        assert_eq!(u64_at(phdr(3) + 32), 0x1000);
        assert_eq!(u64_at(phdr(3) + 40), 0x5000);
    }

    #[test]
    fn notes_are_four_byte_aligned() {
        let mut notes = Vec::new();
        note(&mut notes, NT_AUXV, &[1, 2, 3]);
        assert_eq!(notes.len(), 12 + 8 + 4);
        assert_eq!(&notes[12..17], b"CORE\0");
        assert_eq!(&notes[20..23], &[1, 2, 3]);
    }

    fn compressed(dir: &Path, budget: u64, deadline: Instant) -> Compressed {
        Compressed {
            file: File::create(dir.join("out")).unwrap(),
            buf: Vec::new(),
            written: 0,
            budget,
            deadline,
        }
    }

    #[test]
    fn frames_decode_back_to_the_input() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(
            dir.path(),
            u64::MAX,
            Instant::now() + Duration::from_secs(60),
        );
        let input: Vec<u8> = (0..(CHUNK * 2 + 77)).map(|i| (i % 251) as u8).collect();
        out.push(&input).unwrap();
        out.flush().unwrap();
        let stored = fs::read(dir.path().join("out")).unwrap();
        assert_eq!(stored.len() as u64, out.written);
        // One frame per chunk: decode them all.
        let mut decoded = Vec::with_capacity(input.len());
        ruzstd::decoding::FrameDecoder::new()
            .decode_all_to_vec(&stored, &mut decoded)
            .unwrap();
        assert_eq!(decoded, input);
    }

    #[test]
    fn a_frame_past_the_budget_is_refused_and_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(dir.path(), 64, Instant::now() + Duration::from_secs(60));
        let noise: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        out.push(&noise).unwrap();
        assert!(matches!(out.flush(), Err(WriteError::OverBudget)));
        assert_eq!(fs::metadata(dir.path().join("out")).unwrap().len(), 0);
    }

    #[test]
    fn a_frame_after_the_deadline_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = compressed(dir.path(), u64::MAX, Instant::now());
        out.push(b"late").unwrap();
        assert!(matches!(out.flush(), Err(WriteError::Deadline)));
    }

    #[test]
    fn regular_file_bytes_counts_only_regular_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), [0u8; 10]).unwrap();
        fs::write(dir.path().join("b"), [0u8; 5]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c"), [0u8; 100]).unwrap();
        assert_eq!(regular_file_bytes(dir.path()).unwrap(), 15);
    }
}
