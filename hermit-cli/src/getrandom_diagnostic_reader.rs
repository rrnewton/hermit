// Coordinator-only reader used at existing SaBRe physical-exit stops.
// Linux x86-64 ELF64 little-endian only; unsupported/ambiguous input refuses.
use std::collections::BTreeSet;
use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ffi::c_void;
use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

const MAX_OBJECT: u64 = 128 * 1024 * 1024;
const MAX_MAPS: u64 = 1024 * 1024;
const MAX_BUFFER: usize = 1024 * 1024;
const MAX_READS: usize = 2048;
const PAGE: u64 = 4096;
const O_DIRECTORY: c_int = 0x10000;
const O_NOFOLLOW: c_int = 0x20000;
const O_CLOEXEC: c_int = 0x80000;

unsafe extern "C" {
    fn mmap(
        address: *mut c_void,
        length: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(address: *mut c_void, length: usize) -> c_int;
    fn openat(fd: c_int, path: *const c_char, flags: c_int, ...) -> c_int;
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
fn identity(file: &File) -> Result<Identity, String> {
    let m = file.metadata().map_err(|e| e.to_string())?;
    if !m.is_file() {
        return Err("object is not a regular file".into());
    }
    Ok(Identity {
        dev: m.dev(),
        ino: m.ino(),
        size: m.len(),
        mtime: m.mtime(),
        mtime_ns: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_ns: m.ctime_nsec(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Map {
    start: u64,
    end: u64,
    offset: u64,
    dev: String,
    ino: u64,
    perms: String,
    path: String,
}
fn maps(pid: u32) -> Result<Vec<Map>, String> {
    let mut text = String::new();
    File::open(format!("/proc/{pid}/maps"))
        .map_err(|e| e.to_string())?
        .take(MAX_MAPS + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_MAPS {
        return Err("maps capture too large".into());
    }
    let mut result = Vec::new();
    for line in text.lines() {
        let f: Vec<_> = line.split_whitespace().collect();
        if f.len() < 5 {
            return Err("malformed maps row".into());
        }
        let (a, b) = f[0].split_once('-').ok_or("malformed maps range")?;
        let start = u64::from_str_radix(a, 16).map_err(|e| e.to_string())?;
        let end = u64::from_str_radix(b, 16).map_err(|e| e.to_string())?;
        if start >= end {
            return Err("empty/reversed maps range".into());
        }
        result.push(Map {
            start,
            end,
            offset: u64::from_str_radix(f[2], 16).map_err(|e| e.to_string())?,
            dev: f[3].into(),
            ino: f[4].parse::<u64>().map_err(|e| e.to_string())?,
            perms: f[1].into(),
            path: f.get(5..).unwrap_or_default().join(" "),
        });
    }
    Ok(result)
}

fn bounded_contents(file: &File) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::FileExt;
    let size = identity(file)?.size;
    if size == 0 || size > MAX_OBJECT {
        return Err("ELF object length outside diagnostic bound".into());
    }
    let mut bytes = vec![0; size as usize];
    file.read_exact_at(&mut bytes, 0)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

struct LocalMap {
    address: u64,
    length: usize,
}
impl LocalMap {
    fn new(file: &File, length: usize) -> Result<Self, String> {
        // Parent-only read-only mapping of the held FD. No guest mmap is issued.
        let p = unsafe { mmap(std::ptr::null_mut(), length, 1, 2, file.as_raw_fd(), 0) };
        if p as isize == -1 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(Self {
            address: p as u64,
            length,
        })
    }
}
impl Drop for LocalMap {
    fn drop(&mut self) {
        unsafe {
            munmap(self.address as *mut c_void, self.length);
        }
    }
}

fn open_beneath_root(pid: u32, absolute: &str) -> Result<File, String> {
    if !absolute.starts_with('/')
        || absolute.len() > 4096
        || absolute.contains('\\')
        || absolute.ends_with(" (deleted)")
    {
        return Err("unsupported/deleted mapped path".into());
    }
    let components: Vec<_> = absolute[1..].split('/').collect();
    if components.is_empty()
        || components
            .iter()
            .any(|c| c.is_empty() || *c == "." || *c == "..")
    {
        return Err("non-canonical mapped path".into());
    }
    // The proc magic-link intentionally obtains this stopped task's root.
    // Every subsequent component is resolved by directory FD, never via an
    // intermediate absolute symlink in the coordinator's namespace.
    let mut directory = File::open(format!("/proc/{pid}/root")).map_err(|e| e.to_string())?;
    for (index, component) in components.iter().enumerate() {
        let name = CString::new(*component).map_err(|e| e.to_string())?;
        let flags = O_NOFOLLOW
            | O_CLOEXEC
            | if index + 1 < components.len() {
                O_DIRECTORY
            } else {
                0
            };
        let fd = unsafe { openat(directory.as_raw_fd(), name.as_ptr(), flags, 0) };
        if fd < 0 {
            return Err(format!(
                "root-anchored component open refused: {}",
                std::io::Error::last_os_error()
            ));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[derive(Clone, Debug)]
struct Load {
    offset: u64,
    address: u64,
    file_size: u64,
    flags: u32,
}
struct Elf {
    loads: Vec<Load>,
    reference: u64,
}
fn range(bytes: &[u8], offset: u64, len: u64) -> Result<&[u8], String> {
    let end = offset.checked_add(len).ok_or("ELF range overflow")?;
    bytes
        .get(
            usize::try_from(offset).map_err(|e| e.to_string())?
                ..usize::try_from(end).map_err(|e| e.to_string())?,
        )
        .ok_or_else(|| "ELF range outside held file".into())
}
fn u16at(b: &[u8], o: u64) -> Result<u16, String> {
    Ok(u16::from_le_bytes(range(b, o, 2)?.try_into().unwrap()))
}
fn u32at(b: &[u8], o: u64) -> Result<u32, String> {
    Ok(u32::from_le_bytes(range(b, o, 4)?.try_into().unwrap()))
}
fn u64at(b: &[u8], o: u64) -> Result<u64, String> {
    Ok(u64::from_le_bytes(range(b, o, 8)?.try_into().unwrap()))
}
fn parse_elf(bytes: &[u8], name: &str) -> Result<Elf, String> {
    if range(bytes, 0, 7)? != b"\x7fELF\x02\x01\x01"
        || u16at(bytes, 16)? != 3
        || u16at(bytes, 18)? != 62
        || u32at(bytes, 20)? != 1
        || u16at(bytes, 52)? != 64
    {
        return Err("requires ordinary x86-64 little-endian ELF64 ET_DYN".into());
    }
    let phoff = u64at(bytes, 32)?;
    let shoff = u64at(bytes, 40)?;
    let phnum = u16at(bytes, 56)? as u64;
    let shnum = u16at(bytes, 60)? as u64;
    if phnum == 0
        || phnum > 128
        || shnum == 0
        || shnum > 8192
        || u16at(bytes, 54)? != 56
        || u16at(bytes, 58)? != 64
    {
        return Err("unsupported ELF table shape".into());
    }
    range(bytes, phoff, phnum * 56)?;
    range(bytes, shoff, shnum * 64)?;
    let mut loads = Vec::new();
    for index in 0..phnum {
        let p = phoff + index * 56;
        if u32at(bytes, p)? != 1 {
            continue;
        }
        let l = Load {
            flags: u32at(bytes, p + 4)?,
            offset: u64at(bytes, p + 8)?,
            address: u64at(bytes, p + 16)?,
            file_size: u64at(bytes, p + 32)?,
        };
        if l.offset % PAGE != l.address % PAGE || l.file_size > u64at(bytes, p + 40)? {
            return Err("invalid ELF load alignment/size".into());
        }
        range(bytes, l.offset, l.file_size)?;
        l.address
            .checked_add(l.file_size)
            .ok_or("ELF virtual range overflow")?;
        loads.push(l);
    }
    let mut symbols = Vec::new();
    for index in 0..shnum {
        let s = shoff + index * 64;
        if u32at(bytes, s + 4)? != 11 {
            continue;
        }
        let offset = u64at(bytes, s + 24)?;
        let size = u64at(bytes, s + 32)?;
        let link = u32at(bytes, s + 40)? as u64;
        if u64at(bytes, s + 56)? != 24 || size % 24 != 0 || size / 24 > 65536 || link >= shnum {
            return Err("unsupported dynamic symbol table".into());
        }
        range(bytes, offset, size)?;
        let strings = shoff + link * 64;
        if u32at(bytes, strings + 4)? != 3 {
            return Err("dynamic symbol string link is not STRTAB".into());
        }
        let names = range(
            bytes,
            u64at(bytes, strings + 24)?,
            u64at(bytes, strings + 32)?,
        )?;
        for n in 0..size / 24 {
            let p = offset + n * 24;
            let begin = u32at(bytes, p)? as usize;
            let tail = names.get(begin..).ok_or("symbol string offset invalid")?;
            let end = tail
                .iter()
                .position(|b| *b == 0)
                .ok_or("unterminated symbol name")?;
            if &tail[..end] != name.as_bytes() {
                continue;
            }
            if bytes[(p + 4) as usize] != 0x11
                || bytes[(p + 5) as usize] != 0
                || u16at(bytes, p + 6)? == 0
                || u64at(bytes, p + 16)? != 8
            {
                return Err("reference symbol kind/binding/visibility/size mismatch".into());
            }
            symbols.push(u64at(bytes, p + 8)?);
        }
    }
    if symbols.len() != 1 {
        return Err("reference symbol absent or ambiguous".into());
    }
    Ok(Elf {
        loads,
        reference: symbols[0],
    })
}

fn down(x: u64) -> u64 {
    x / PAGE * PAGE
}
fn up(x: u64) -> Result<u64, String> {
    Ok(x.checked_add(PAGE - 1).ok_or("page range overflow")? / PAGE * PAGE)
}
fn generation(pid: u32) -> Result<u64, String> {
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
    text.rsplit_once(") ")
        .ok_or("malformed process stat")?
        .1
        .split_whitespace()
        .nth(19)
        .ok_or("short process stat")?
        .parse::<u64>()
        .map_err(|e| e.to_string())
}

pub struct Object {
    file: File,
    original: Identity,
    bytes: Vec<u8>,
    path: String,
    local: LocalMap,
    map_dev: String,
    elf: Elf,
}
impl Object {
    pub fn open(path: &Path, symbol: &str) -> Result<Self, String> {
        let canonical = fs::canonicalize(path).map_err(|e| e.to_string())?;
        let path = canonical.to_str().ok_or("non-UTF8 object path")?.to_owned();
        let file = open_beneath_root(std::process::id(), &path)?;
        let original = identity(&file)?;
        let bytes = bounded_contents(&file)?;
        let elf = parse_elf(&bytes, symbol)?;
        let local = LocalMap::new(&file, bytes.len())?;
        let local_rows = maps(std::process::id())?;
        let observed = local_rows
            .iter()
            .find(|m| m.start <= local.address && local.address < m.end)
            .ok_or("parent identity mapping absent")?;
        if observed.path != path
            || observed.ino != original.ino
            || !observed.perms.starts_with("r--")
            || identity(&file)? != original
        {
            return Err("parent held-file mapping changed/mismatched".into());
        }
        Ok(Self {
            file,
            original,
            bytes,
            path,
            local,
            map_dev: observed.dev.clone(),
            elf,
        })
    }

    fn target_maps(&self, pid: u32) -> Result<(Vec<Map>, File), String> {
        if fs::metadata(format!("/proc/{pid}/ns/mnt"))
            .map_err(|e| e.to_string())?
            .ino()
            != fs::metadata("/proc/self/ns/mnt")
                .map_err(|e| e.to_string())?
                .ino()
        {
            return Err("unproven different mount namespace".into());
        }
        let selected: Vec<_> = maps(pid)?
            .into_iter()
            .filter(|m| m.dev == self.map_dev && m.ino == self.original.ino)
            .collect();
        if selected.is_empty() || selected.len() > 128 {
            return Err("expected object mapping absent or too numerous".into());
        }
        for m in &selected {
            let link = fs::read_link(format!("/proc/{pid}/map_files/{:x}-{:x}", m.start, m.end))
                .map_err(|e| e.to_string())?;
            if m.path != self.path || link != Path::new(&self.path) {
                return Err("ambiguous/replaced/deleted mapped object path".into());
            }
        }
        let target = open_beneath_root(pid, &self.path)?;
        if identity(&target)? != self.original
            || bounded_contents(&target)? != self.bytes
            || identity(&target)? != self.original
            || identity(&self.file)? != self.original
        {
            return Err("mapped held-object stat/content identity mismatch".into());
        }
        Ok((selected, target))
    }

    fn bias(&self, rows: &[Map]) -> Result<u64, String> {
        let mut candidates = BTreeSet::new();
        for m in rows {
            for p in &self.elf.loads {
                if m.offset >= down(p.offset) && m.offset < up(p.offset + p.file_size)? {
                    let relative = down(p.address)
                        .checked_add(m.offset - down(p.offset))
                        .ok_or("load-relative address overflow")?;
                    if let Some(b) = m.start.checked_sub(relative) {
                        candidates.insert(b);
                    }
                }
            }
        }
        let mut valid = Vec::new();
        for b in candidates {
            let mut all = true;
            for m in rows {
                let mut fits = false;
                for p in &self.elf.loads {
                    let start = b
                        .checked_add(down(p.address))
                        .ok_or("load address overflow")?;
                    let end = b
                        .checked_add(up(p.address + p.file_size)?)
                        .ok_or("load end overflow")?;
                    if m.start >= start
                        && m.end <= end
                        && Some(m.offset) == down(p.offset).checked_add(m.start - start)
                    {
                        fits = true;
                    }
                }
                all &= fits;
            }
            if all {
                valid.push(b);
            }
        }
        if valid.len() != 1 {
            return Err("ambiguous or inconsistent ELF load mappings".into());
        }
        Ok(valid[0])
    }

    fn extent(
        &self,
        rows: &[Map],
        bias: u64,
        address: u64,
        size: usize,
        writable: bool,
    ) -> Result<(), String> {
        let end = address
            .checked_add(size as u64)
            .ok_or("live extent overflow")?;
        let mut eligible = false;
        for p in &self.elf.loads {
            let start = bias.checked_add(p.address).ok_or("live segment overflow")?;
            if address >= start
                && end
                    <= start
                        .checked_add(p.file_size)
                        .ok_or("live segment end overflow")?
                && p.flags & 4 != 0
                && (!writable || p.flags & 2 != 0)
            {
                eligible = true;
            }
        }
        if !eligible {
            return Err("reference/buffer extent outside required file-backed ELF load".into());
        }
        let mut cursor = address;
        while cursor < end {
            let m = rows
                .iter()
                .find(|m| m.start <= cursor && cursor < m.end)
                .ok_or("hole in complete buffer mapping")?;
            if !m.perms.starts_with('r') || (writable && m.perms.as_bytes().get(1) != Some(&b'w')) {
                return Err("reference/buffer mapping permission mismatch".into());
            }
            cursor = m.end.min(end);
        }
        Ok(())
    }

    pub fn prepare(&self, pid: u32, size: usize) -> Result<Prepared<'_>, String> {
        use std::os::unix::fs::FileExt;
        if size == 0 || size > MAX_BUFFER {
            return Err("buffer size outside diagnostic bound".into());
        }
        let start = generation(pid)?;
        let (rows, target) = self.target_maps(pid)?;
        let bias = self.bias(&rows)?;
        let reference = bias
            .checked_add(self.elf.reference)
            .ok_or("reference address overflow")?;
        if reference % 8 != 0 {
            return Err("unaligned reference symbol".into());
        }
        self.extent(&rows, bias, reference, 8, false)?;
        let memory = File::open(format!("/proc/{pid}/mem")).map_err(|e| e.to_string())?;
        let mut pointer = [0; 8];
        let n = memory
            .read_at(&mut pointer, reference)
            .map_err(|e| e.to_string())?;
        if n != 8 {
            return Err("short exported pointer read".into());
        }
        let address = u64::from_le_bytes(pointer);
        if address % 8 != 0 {
            return Err("unaligned exported buffer pointer".into());
        }
        self.extent(&rows, bias, address, size, true)?;
        if generation(pid)? != start || self.target_maps(pid)?.0 != rows {
            return Err("target identity/mappings changed during preparation".into());
        }
        Ok(Prepared {
            object: self,
            target,
            memory,
            pid,
            start,
            rows,
            reference,
            address,
            size,
        })
    }
}

pub struct Prepared<'a> {
    object: &'a Object,
    target: File,
    memory: File,
    pid: u32,
    start: u64,
    rows: Vec<Map>,
    reference: u64,
    address: u64,
    size: usize,
}
impl Prepared<'_> {
    pub fn description(&self) -> String {
        format!(
            "pid={} generation={} object={} identity={:?} parent_read_only_mapping={:#x}/{} maps_device={} reference={:#x} buffer={:#x} size={} loads={:?} target_maps={:?}",
            self.pid,
            self.start,
            self.object.path,
            self.object.original,
            self.object.local.address,
            self.object.local.length,
            self.object.map_dev,
            self.reference,
            self.address,
            self.size,
            self.object.elf.loads,
            self.rows
        )
    }
    pub fn finish(
        self,
        protocol: impl FnOnce(
            &mut dyn FnMut(usize, &mut [u8]) -> Result<(), String>,
        ) -> Result<Vec<u8>, String>,
    ) -> Result<Vec<u8>, String> {
        use std::os::unix::fs::FileExt;
        let mut calls = 0;
        let mut bytes = 0;
        let image = protocol(&mut |offset, out| {
            calls += 1;
            bytes += out.len();
            if calls > MAX_READS
                || bytes > 2 * self.size + 4096
                || offset
                    .checked_add(out.len())
                    .is_none_or(|end| end > self.size)
            {
                return Err("bounded remote read request exceeded".into());
            }
            let address = self
                .address
                .checked_add(offset as u64)
                .ok_or("remote address overflow")?;
            let n = self
                .memory
                .read_at(out, address)
                .map_err(|e| e.to_string())?;
            if n != out.len() {
                return Err(format!(
                    "short diagnostic read: requested{}, got{}",
                    out.len(),
                    n
                ));
            }
            Ok(())
        })?;
        let mut pointer = [0; 8];
        let n = self
            .memory
            .read_at(&mut pointer, self.reference)
            .map_err(|e| e.to_string())?;
        if n != 8 || u64::from_le_bytes(pointer) != self.address {
            return Err("exported pointer changed/short during capture".into());
        }
        if generation(self.pid)? != self.start
            || identity(&self.target)? != self.object.original
            || identity(&self.object.file)? != self.object.original
            || self.object.target_maps(self.pid)?.0 != self.rows
        {
            return Err("target/object/mappings changed during capture".into());
        }
        Ok(image)
    }
}
