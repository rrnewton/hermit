//! Resources for the one authorized, ordinary Unix acceptance attempt.
//! No setup occurs during build/discovery. All errors retain evidence and the
//! original test outcome; later recovery cannot certify the original close.
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;

#[path = "../detcore/src/network_runtime/capability_unit.rs"]
mod capability_unit;
#[path = "../hermit-cli/src/unix_guard_process.rs"]
mod process;
use capability_unit::CapabilityServiceKind;
use capability_unit::CapabilityServiceLifetime;
use capability_unit::CapabilityUnitLaunch;
use process::CommandFlight;
use process::UnitIdentity;
use process::pause;
use process::within;

pub const BPFFS_ENV: &str = "HERMIT_PREPARED_NETWORK_GUARD_BPFFS";
pub const RECOVERY_ENV: &str = "HERMIT_PREPARED_NETWORK_GUARD_RECOVERY";
pub const EVIDENCE_ENV: &str = "HERMIT_NETWORK_UNIX_EVIDENCE";
const MAX_FILE: u64 = 1024 * 1024;
const MAX_FILES: usize = 32;
const RECOVERY_BYTES: u64 = MAX_FILES as u64 * MAX_FILE;
type Id = (u32, u32);
fn err(text: impl Into<String>) -> io::Error {
    io::Error::other(text.into())
}
fn read(path: &Path, bound: u64) -> io::Result<Vec<u8>> {
    let mut f = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.len() > bound {
        return Err(err("regular file bound"));
    }
    let mut data = Vec::new();
    (&mut f).take(bound + 1).read_to_end(&mut data)?;
    if data.len() as u64 > bound {
        return Err(err("file grew past bound"));
    }
    Ok(data)
}
fn digest(path: &Path, bound: u64) -> io::Result<String> {
    let mut f = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !f.metadata()?.is_file() || f.metadata()?.len() > bound {
        return Err(err("artifact size/type"));
    }
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = f.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > bound {
            return Err(err("artifact grew past bound"));
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn publish(path: &Path, data: &Value) -> io::Result<()> {
    let mut f = File::options()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    serde_json::to_writer_pretty(&mut f, data)?;
    f.write_all(b"\n")?;
    f.sync_all()
}
fn private(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}
fn safe_path(path: &Path) -> io::Result<&str> {
    let s = path
        .to_str()
        .ok_or_else(|| err("non-UTF8 deployment path"))?;
    if !path.is_absolute()
        || s.bytes()
            .any(|b| b.is_ascii_whitespace() || b"\\\"'%".contains(&b))
        || s.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err(err("unsafe deployment path"));
    }
    Ok(s)
}
#[derive(Clone)]
struct Mount {
    path: PathBuf,
    record: String,
    device: u64,
    inode: u64,
}
fn mount_record(path: &Path) -> io::Result<Option<String>> {
    let path = safe_path(path)?;
    let text = String::from_utf8(read(Path::new("/proc/self/mountinfo"), 1024 * 1024)?)
        .map_err(|error| err(error.to_string()))?;
    let found: Vec<_> = text
        .lines()
        .filter(|line| line.split_whitespace().nth(4) == Some(path))
        .collect();
    match found.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some((*one).into())),
        _ => Err(err("stacked mount target")),
    }
}
impl Mount {
    fn capture(path: &Path, kind: &str) -> io::Result<Self> {
        let record = mount_record(path)?.ok_or_else(|| err("new mount missing"))?;
        if record
            .split(" - ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            != Some(kind)
        {
            return Err(err("mount filesystem differs"));
        }
        let m = fs::symlink_metadata(path)?;
        if !m.is_dir() {
            return Err(err("mount root is not a directory"));
        }
        Ok(Self {
            path: path.into(),
            record,
            device: m.dev(),
            inode: m.ino(),
        })
    }
    fn verify(&self) -> io::Result<()> {
        let m = fs::symlink_metadata(&self.path)?;
        if mount_record(&self.path)?.as_deref() != Some(&self.record)
            || (m.dev(), m.ino()) != (self.device, self.inode)
        {
            return Err(err("owned mount replaced"));
        }
        Ok(())
    }
}
#[derive(Default)]
struct Inventory {
    ids: BTreeSet<Id>,
    launched: BTreeSet<u64>,
    complete: BTreeSet<u64>,
    units: BTreeSet<String>,
    terminal: BTreeMap<u64, BTreeSet<Id>>,
    actors: BTreeMap<String, Value>,
    errors: Vec<String>,
}
fn valid_unit(unit: &str) -> bool {
    let tail = unit
        .strip_prefix("hermit-unix-readback-")
        .or_else(|| unit.strip_prefix("hermit-unix-"));
    tail.and_then(|s| s.strip_suffix(".service"))
        .is_some_and(|s| {
            s.len() == 32
                && s.bytes().any(|b| b != b'0')
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}
fn pairs(value: &Value) -> io::Result<BTreeSet<Id>> {
    let values = value
        .as_array()
        .ok_or_else(|| err("missing ID inventory"))?;
    if values.len() > 72 {
        return Err(err("inventory population exceeds 72"));
    }
    let mut ids = BTreeSet::new();
    for p in values {
        let a = p
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| err("ID pair"))?;
        let k = a[0]
            .as_u64()
            .filter(|k| *k <= 2)
            .ok_or_else(|| err("ID kind"))?;
        let n = a[1]
            .as_u64()
            .filter(|n| *n > 0 && *n <= u32::MAX as u64)
            .ok_or_else(|| err("ID value"))?;
        if !ids.insert((k as u32, n as u32)) {
            return Err(err("duplicate ID"));
        }
    }
    Ok(ids)
}
fn full(ids: &BTreeSet<Id>) -> bool {
    ids.len() == 72 && [0, 1, 2].map(|k| ids.iter().filter(|i| i.0 == k).count()) == [10, 31, 31]
}
fn recovery_actor(unit: &str, text: &str) -> io::Result<Option<Value>> {
    let Some((line, _)) = text.split_once('\n') else {
        return Ok(None);
    };
    let parts: Vec<_> = line.split_whitespace().collect();
    if parts.len() != 5 || parts[0] != "recovery-actor-v1" {
        return Err(err("missing actual recovery launch identity"));
    }
    let invocation = parts[1];
    let device = parts[2].parse::<u64>().map_err(|_| err("actor device"))?;
    let inode = parts[3].parse::<u64>().map_err(|_| err("actor inode"))?;
    let cgroup = Path::new(parts[4]);
    if !valid_unit(unit)
        || invocation.len() != 32
        || !invocation.bytes().all(|b| b.is_ascii_hexdigit())
        || invocation.bytes().all(|b| b == b'0')
        || inode == 0
        || !cgroup.starts_with("/sys/fs/cgroup")
        || cgroup.file_name().and_then(|s| s.to_str()) != Some(unit)
        || cgroup.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err(err("recovery launch identity differs"));
    }
    Ok(Some(
        json!({"unit":unit,"invocation":invocation,"device":device,"inode":inode,"cgroup":cgroup}),
    ))
}
fn stop_owned<T>(
    expected: Option<&Value>,
    identity: &UnitIdentity,
    stop: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    let actor = expected.ok_or_else(|| err("unit launch identity unknown; refusing stop"))?;
    if actor["unit"].as_str() != Some(identity.unit.as_str())
        || actor["invocation"].as_str() != Some(identity.invocation.as_str())
        || actor["inode"].as_u64() != Some(identity.inode)
        || actor["device"].as_u64() != Some(identity.device)
        || actor["cgroup"].as_str() != identity.cgroup.to_str()
    {
        return Err(err(
            "unit replaced since retained original receipt; refusing stop",
        ));
    }
    stop()
}
// A captured live identity remains authority after systemd removes its empty
// cgroup. Never recapture from a terminal name lookup; preserve the held inode.
fn held_unit_state(identity: &UnitIdentity, state: &str) -> io::Result<()> {
    let p = process::properties(state)?;
    if p.get("Id") != Some(&identity.unit.as_str())
        || p.get("LoadState") != Some(&"loaded")
        || p.get("InvocationID") != Some(&identity.invocation.as_str())
    {
        return Err(err("retained recovery unit invocation differs"));
    }
    let held = identity.directory.metadata()?;
    if (held.dev(), held.ino()) != (identity.device, identity.inode) {
        return Err(err("retained recovery cgroup handle differs"));
    }
    let group = p
        .get("ControlGroup")
        .ok_or_else(|| err("missing recovery cgroup"))?;
    if group.is_empty() {
        if !helper_exited(state)? || held.nlink() != 0 {
            return Err(err("recovery cgroup missing before proven helper exit"));
        }
        match identity.cgroup.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            _ => Err(err("retired recovery cgroup replaced or unreadable")),
        }
    } else {
        if PathBuf::from(format!("/sys/fs/cgroup{group}")) != identity.cgroup {
            return Err(err("retained recovery cgroup path differs"));
        }
        let current = identity.cgroup.symlink_metadata()?;
        if (current.dev(), current.ino()) != (identity.device, identity.inode) || held.nlink() == 0
        {
            return Err(err("retained recovery cgroup replaced"));
        }
        Ok(())
    }
}
fn recovery_channel() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut pair = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) })
}
fn send_recovery_parent(channel: &OwnedFd, deadline: Instant) -> io::Result<()> {
    within(deadline)?;
    let raw = unsafe {
        libc::syscall(
            libc::SYS_pidfd_open,
            libc::syscall(libc::SYS_gettid),
            libc::O_EXCL,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let parent = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let byte = 0u8;
    let mut io = libc::iovec {
        iov_base: (&byte as *const u8).cast_mut().cast(),
        iov_len: 1,
    };
    // Native CMSG alignment, one borrowed exact parent capability, never a PID number.
    let mut control = [0usize; 4];
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut io;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) } as usize;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&message);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), parent.as_raw_fd());
        if libc::sendmsg(
            channel.as_raw_fd(),
            &message,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        ) != 1
        {
            return Err(io::Error::last_os_error());
        }
    }
    within(deadline)
}
fn release_recovery(
    channel: &OwnedFd,
    actor: &Value,
    identity: &UnitIdentity,
    deadline: Instant,
) -> io::Result<()> {
    within(deadline)?;
    stop_owned(Some(actor), identity, || {
        let byte = 1u8;
        let n = unsafe {
            libc::send(
                channel.as_raw_fd(),
                (&byte as *const u8).cast(),
                1,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n != 1 {
            return Err(io::Error::last_os_error());
        }
        within(deadline)
    })
}
fn helper_exited(state: &str) -> io::Result<bool> {
    Ok(process::properties(state)?
        .get("MainPID")
        .ok_or_else(|| err("missing recovery helper terminal state"))?
        .parse::<u32>()
        .map_err(|_| err("invalid recovery helper PID"))?
        == 0)
}
impl Inventory {
    fn row(&mut self, row: &Value) -> io::Result<()> {
        if row["stage"] == "before_launch" {
            let inc = row["incarnation"]
                .as_u64()
                .filter(|i| *i != 0)
                .ok_or_else(|| err("launch incarnation"))?;
            self.launched.insert(inc);
            let unit = row["loader_unit"]
                .as_str()
                .filter(|s| {
                    valid_unit(s)
                        && s.starts_with("hermit-unix-")
                        && !s.starts_with("hermit-unix-readback-")
                })
                .ok_or_else(|| err("launch unit identity"))?;
            let suffix = unit.strip_prefix("hermit-unix-").expect("validated unit");
            if u64::from_str_radix(&suffix[..16], 16).ok() != Some(inc) {
                return Err(err("unit/incarnation binding differs"));
            }
            self.units.insert(unit.into());
            self.units
                .insert(unit.replacen("hermit-unix-", "hermit-unix-readback-", 1));
        } else if row["stage"] == "terminal" {
            let inc = row["guard"]["incarnation"]
                .as_u64()
                .filter(|i| *i != 0)
                .ok_or_else(|| err("terminal incarnation"))?;
            for role in ["loader", "query"] {
                let actor = &row[role];
                let unit = actor["unit"]
                    .as_str()
                    .filter(|s| valid_unit(s))
                    .ok_or_else(|| err("terminal actor unit"))?;
                let path = PathBuf::from(
                    actor["cgroup"]
                        .as_str()
                        .ok_or_else(|| err("terminal actor cgroup"))?,
                );
                let invocation = actor["invocation"]
                    .as_str()
                    .ok_or_else(|| err("terminal actor invocation"))?;
                if !self.units.contains(unit)
                    || !path.starts_with("/sys/fs/cgroup")
                    || path.file_name().and_then(|s| s.to_str()) != Some(unit)
                    || path.components().any(|c| {
                        matches!(
                            c,
                            std::path::Component::ParentDir | std::path::Component::CurDir
                        )
                    })
                    || invocation.len() != 32
                    || !invocation.bytes().all(|b| b.is_ascii_hexdigit())
                    || invocation.bytes().all(|b| b == b'0')
                    || actor["inode"].as_u64().unwrap_or(0) == 0
                    || actor["device"].as_u64().is_none()
                {
                    return Err(err("terminal actor identity differs"));
                }
                if self.actors.insert(unit.into(), actor.clone()).is_some() {
                    return Err(err("duplicate terminal actor"));
                }
            }
            let ids = pairs(&row["guard"]["inventory"]["ids"])?;
            self.ids.extend(&ids);
            if full(&ids) {
                self.complete.insert(inc);
            }
            if self.terminal.insert(inc, ids).is_some() {
                return Err(err("duplicate terminal incarnation"));
            }
        } else if row["stage"] == "terminal_failed" && !row["ids"].is_null() {
            self.ids.extend(pairs(&row["ids"])?);
        }
        Ok(())
    }
    fn journal(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        let u64at = |r: &[u8], n| u64::from_le_bytes(r[n..n + 8].try_into().unwrap());
        let u32at = |r: &[u8], n| u32::from_le_bytes(r[n..n + 4].try_into().unwrap());
        let mut incarnation = None;
        let mut originals = BTreeSet::new();
        let mut ready = false;
        for (i, r) in data.chunks_exact(104).enumerate() {
            let inc = u64at(r, 8);
            if u64at(r, 0) != 0x554750494e303031
                || inc == 0
                || u64at(r, 16) != i as u64 + 1
                || u32at(r, 48) != 3
                || u32at(r, 68) != 0
                || name != format!("ug-{inc:016x}")
                || incarnation.is_some_and(|old| old != inc)
            {
                return Err(err("journal identity/ordinal"));
            }
            incarnation = Some(inc);
            self.launched.insert(inc);
            let (phase, kind, id) = (u32at(r, 52), u32at(r, 56), u32at(r, 60));
            if !(1..=22).contains(&phase) {
                return Err(err("unknown journal phase"));
            }
            if phase == 18 {
                if kind > 2 || id == 0 || !originals.insert((kind, id)) {
                    return Err(err("original ID journal record"));
                }
                self.ids.insert((kind, id));
            } else if [2, 3, 14, 15, 16, 19].contains(&phase) {
                if kind > 1 || id == 0 {
                    return Err(err("pin ID journal record"));
                }
                self.ids.insert((if kind == 0 { 0 } else { 2 }, id));
            } else if phase == 20 {
                ready = true;
            }
        }
        if data.len() % 104 != 0 {
            return Err(err("partial journal tail; valid prefix retained"));
        }
        let inc = incarnation.ok_or_else(|| err("empty journal"))?;
        if ready || full(&originals) {
            self.complete.insert(inc);
        }
        if self
            .terminal
            .get(&inc)
            .is_some_and(|terminal| *terminal != originals)
        {
            return Err(err("terminal and journal original IDs differ"));
        }
        Ok(())
    }
    fn scan(root: &Path) -> io::Result<Self> {
        let mut out = Self::default();
        let mut files = Vec::new();
        for entry in fs::read_dir(root)? {
            files.push(entry?.path());
            if files.len() > MAX_FILES {
                return Err(err("recovery file census exceeded"));
            }
        }
        files.sort();
        // Terminal rows precede journals so comparison never depends on names.
        for path in &files {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| err("recovery filename"))?;
            if name.ends_with(".terminal.jsonl") {
                match read(path, MAX_FILE) {
                    Ok(data) => {
                        for line in data.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                            let result = serde_json::from_slice::<Value>(line)
                                .map_err(io::Error::from)
                                .and_then(|row| out.row(&row));
                            if let Err(e) = result {
                                out.errors.push(format!("{}: {e}", path.display()));
                            }
                        }
                    }
                    Err(e) => out.errors.push(e.to_string()),
                }
            } else if !name.starts_with("ug-") {
                let stem = name
                    .strip_suffix(".stdout.log")
                    .or_else(|| name.strip_suffix(".stderr.log"));
                if !stem.is_some_and(|s| valid_unit(&format!("hermit-unix-{s}.service"))) {
                    out.errors.push(format!("unexpected recovery file: {name}"));
                } else if let Err(error) = read(path, MAX_FILE) {
                    out.errors.push(error.to_string());
                }
            }
        }
        for path in &files {
            let name = path.file_name().unwrap().to_str().unwrap();
            if name.starts_with("ug-") {
                if let Err(e) = read(path, MAX_FILE).and_then(|data| out.journal(name, &data)) {
                    out.errors.push(format!("{}: {e}", path.display()));
                }
            }
        }
        for inc in out.launched.difference(&out.complete) {
            out.errors.push(format!(
                "incarnation {inc} lacks complete original inventory"
            ));
        }
        Ok(out)
    }
}

pub struct Owner {
    root: PathBuf,
    bpffs: PathBuf,
    recovery: PathBuf,
    evidence: PathBuf,
    mounts: Vec<Mount>,
    commands: Vec<CommandFlight>,
    command_argv: Vec<String>,
    bindings: BTreeMap<PathBuf, String>,
    readback: PathBuf,
    pub child_launched: bool,
    pub test_drained: bool,
    pub cleanup_deadline: Option<Instant>,
    finished: bool,
    sequence: u64,
}
impl Owner {
    pub fn new(terminal: &Path) -> io::Result<Self> {
        let parent = terminal
            .parent()
            .ok_or_else(|| err("terminal parent"))?
            .canonicalize()?;
        let name = terminal
            .file_name()
            .ok_or_else(|| err("terminal name"))?
            .to_string_lossy();
        let root = parent.join(format!("{name}.unix-owner"));
        safe_path(&root)?;
        private(&root)?;
        Ok(Self {
            bpffs: root.join("bpffs"),
            recovery: root.join("recovery"),
            evidence: root.join("evidence"),
            root,
            mounts: Vec::new(),
            commands: Vec::new(),
            command_argv: Vec::new(),
            bindings: BTreeMap::new(),
            readback: PathBuf::new(),
            child_launched: false,
            test_drained: false,
            cleanup_deadline: None,
            finished: false,
            sequence: 0,
        })
    }
    fn start_command(&mut self, command: &mut Command, deadline: Instant) -> io::Result<usize> {
        self.start_command_with_stdin(command, None, deadline)
    }
    fn start_command_with_stdin(
        &mut self,
        command: &mut Command,
        stdin: Option<Stdio>,
        deadline: Instant,
    ) -> io::Result<usize> {
        within(deadline)?;
        if self.commands.len() >= 128 {
            return Err(err("owner command census exceeded"));
        }
        command
            .env_clear()
            .envs(capability_unit::CAPABILITY_ENVIRONMENT.iter().copied());
        let flight = match stdin {
            Some(stdin) => CommandFlight::start_with_stdin(command, stdin)?,
            None => CommandFlight::start(command)?,
        };
        self.command_argv.push(format!("{command:?}"));
        self.commands.push(flight);
        Ok(self.commands.len() - 1)
    }
    fn wait_command(&mut self, index: usize, deadline: Instant) -> io::Result<(i32, String)> {
        let f = &mut self.commands[index];
        loop {
            if let Some(status) = f.poll(deadline)? {
                return Ok((status.code().unwrap_or(128), f.output()?));
            }
            if let Err(e) = pause(deadline) {
                let _ = f.kill_group();
                return Err(e);
            }
        }
    }
    fn command(&mut self, command: &mut Command, deadline: Instant) -> io::Result<(i32, String)> {
        let index = self.start_command(command, deadline)?;
        self.wait_command(index, deadline)
    }
    fn sudo(&mut self, args: &[OsString], deadline: Instant) -> io::Result<String> {
        let (code, text) = self.command(
            Command::new(capability_unit::CAPABILITY_SUDO)
                .arg("-n")
                .args(args),
            deadline,
        )?;
        if code != 0 {
            return Err(err(format!("owned administrative command failed: {code}")));
        }
        Ok(text)
    }
    fn show(&mut self, unit: &str, deadline: Instant) -> io::Result<String> {
        if !valid_unit(unit) {
            return Err(err("foreign unit name"));
        }
        let (code, text) = self.command(
            Command::new("/usr/bin/systemctl").args([
                "show",
                "--no-pager",
                "--property=Id,LoadState,ControlGroup,InvocationID,MainPID",
                unit,
            ]),
            deadline,
        )?;
        if ![0, 1].contains(&code) {
            return Err(err("unit show failed"));
        }
        let p = process::properties(&text)?;
        if p.get("Id") != Some(&unit) {
            return Err(err("unit show identity"));
        }
        Ok(text)
    }
    fn retire_unit(
        &mut self,
        unit: &str,
        deadline: Instant,
        expected: Option<&Value>,
    ) -> io::Result<Value> {
        // A before-launch intent cannot authorize even a fresh name lookup.
        // Without an actual actor receipt the loaded unit may be a collision.
        let actor = expected.ok_or_else(|| err("unit launch identity unknown; refusing stop"))?;
        if actor["unit"].as_str() != Some(unit) {
            return Err(err("retained actor unit differs"));
        }
        let text = self.show(unit, deadline)?;
        if process::properties(&text)?.get("LoadState") == Some(&"not-found") {
            let cgroup = actor["cgroup"]
                .as_str()
                .ok_or_else(|| err("missing original cgroup"))?;
            match Path::new(cgroup).symlink_metadata() {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => return Err(err("original actor cgroup still present or unreadable")),
            }
            return Ok(json!({"unit":unit,"state":"already-absent"}));
        }
        let identity = UnitIdentity::capture(unit, &text)?;
        stop_owned(Some(actor), &identity, || {
            self.sudo(
                &[
                    "/usr/bin/systemctl".into(),
                    "--no-ask-password".into(),
                    "stop".into(),
                    unit.into(),
                ],
                deadline,
            )
        })?;
        loop {
            if identity.drained(&self.show(unit, deadline)?)? {
                return Ok(json!({"state":"drained","identity":identity.receipt()}));
            }
            pause(deadline)?;
        }
    }
    fn retire_captured_unit(
        &mut self,
        identity: &UnitIdentity,
        actor: &Value,
        deadline: Instant,
    ) -> io::Result<Value> {
        let state = self.show(&identity.unit, deadline)?;
        if process::properties(&state)?.get("LoadState") == Some(&"not-found") {
            if !identity.drained(&state)? {
                return Err(err("captured recovery unit not drained"));
            }
        } else {
            held_unit_state(identity, &state)?;
            stop_owned(Some(actor), identity, || {
                self.sudo(
                    &[
                        "/usr/bin/systemctl".into(),
                        "--no-ask-password".into(),
                        "stop".into(),
                        identity.unit.clone().into(),
                    ],
                    deadline,
                )
            })?;
            loop {
                if identity.drained(&self.show(&identity.unit, deadline)?)? {
                    break;
                }
                pause(deadline)?;
            }
        }
        within(deadline)?;
        Ok(json!({"state":"drained","identity":identity.receipt()}))
    }
    pub fn prepare(&mut self, cli: &Path, deadline: Instant) -> io::Result<()> {
        within(deadline)?;
        let base = cli.parent().ok_or_else(|| err("CLI directory"))?;
        let candidates = [
            base.join("network-provider/unix-guard"),
            base.parent()
                .ok_or_else(|| err("CLI prefix"))?
                .join("lib/hermit/network-provider/unix-guard"),
        ];
        let candidates: Vec<_> = candidates.into_iter().filter(|p| p.is_dir()).collect();
        if candidates.len() != 1 {
            return Err(err("exactly one maintained Unix package required"));
        }
        let package = &candidates[0];
        let manifest_path = package.join("manifest.json");
        let manifest: Value = serde_json::from_slice(&read(&manifest_path, 32768)?)?;
        if manifest["schema"] != 1
            || manifest["kind"] != "hermit-unix-guard"
            || manifest["maps"] != 10
            || manifest["programs"] != 31
            || manifest["links"] != 31
        {
            return Err(err("Unix package contract differs"));
        }
        self.bindings
            .insert(cli.into(), digest(cli, 1024 * 1024 * 1024)?);
        self.bindings
            .insert(manifest_path.clone(), digest(&manifest_path, 32768)?);
        for (name, key) in [
            ("unix-guard.bpf.o", "object_sha256"),
            ("hermit-unix-keeper", "helper_sha256"),
            ("hermit-unix-readback", "readback_sha256"),
        ] {
            let path = package.join(name);
            let actual = digest(&path, 64 * 1024 * 1024)?;
            if manifest[key].as_str() != Some(actual.as_str()) {
                return Err(err("package artifact binding differs"));
            }
            self.bindings.insert(path, actual);
        }
        self.readback = package.join("hermit-unix-readback");
        publish(
            &self.root.join("bindings.json"),
            &json!({"schema":1,"artifacts":self.bindings,"package_manifest":manifest,"cli":cli,"recovery_bytes":RECOVERY_BYTES,"original_cleanup_seconds":15}),
        )?;
        for path in [&self.bpffs, &self.recovery] {
            private(path)?;
            if mount_record(path)?.is_some() {
                return Err(err("deployment target already mounted"));
            }
        }
        let recovery = self.recovery.clone();
        let bpffs = self.bpffs.clone();
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        if uid != unsafe { libc::geteuid() } || gid != unsafe { libc::getegid() } {
            return Err(err("owner credentials differ"));
        }
        self.sudo(
            &[
                "/usr/bin/mount".into(),
                "-t".into(),
                "tmpfs".into(),
                "-o".into(),
                format!("size={RECOVERY_BYTES},nosuid,nodev,noexec,mode=0700,uid={uid},gid={gid}")
                    .into(),
                "hermit-unix-recovery".into(),
                recovery.as_os_str().into(),
            ],
            deadline,
        )?;
        self.mounts.push(Mount::capture(&recovery, "tmpfs")?);
        self.sudo(
            &[
                "/usr/bin/mount".into(),
                "-t".into(),
                "bpf".into(),
                "-o".into(),
                format!("nosuid,nodev,noexec,uid={uid},gid={gid},mode=0700").into(),
                "bpf".into(),
                bpffs.as_os_str().into(),
            ],
            deadline,
        )?;
        self.mounts.push(Mount::capture(&bpffs, "bpf")?);
        for mount in &self.mounts {
            mount.verify()?;
            let m = fs::metadata(&mount.path)?;
            if (m.uid(), m.gid(), m.mode() & 0o777) != (uid, gid, 0o700) {
                return Err(err("deployment ownership differs"));
            }
        }
        // The bpffs mount root carries the kernel's sticky bit. Give the
        // guard the same private child-directory shape as normal per-user
        // deployment, while retaining the mount root for exact unmount custody.
        let pins = self.bpffs.join("pins");
        private(&pins)?;
        let mount = self
            .mounts
            .iter()
            .find(|mount| mount.path == self.bpffs)
            .ok_or_else(|| err("missing bpffs mount identity"))?;
        mount.verify()?;
        let m = fs::symlink_metadata(&pins)?;
        if !m.is_dir()
            || (m.uid(), m.gid(), m.mode() & 0o7777) != (uid, gid, 0o700)
            || m.dev() != mount.device
        {
            return Err(err("private pin directory ownership or filesystem differs"));
        }
        publish(
            &self.root.join("pin-root.json"),
            &json!({"path":pins,"device":m.dev(),"inode":m.ino(),"uid":m.uid(),"gid":m.gid(),"mode":m.mode(),"mount_path":mount.path,"mount_device":mount.device,"mount_inode":mount.inode}),
        )?;
        within(deadline)
    }
    pub fn configure(&self, command: &mut Command) {
        command
            .env(BPFFS_ENV, self.bpffs.join("pins"))
            .env(RECOVERY_ENV, &self.recovery)
            .env(EVIDENCE_ENV, &self.evidence);
    }
    fn recovery_query(&mut self, ids: &[Id], deadline: Instant) -> io::Result<Value> {
        if ids.is_empty() || ids.len() > 72 {
            return Err(err("recovery query requires retained population"));
        }
        within(deadline)?;
        let mut now: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let absolute = now.tv_sec as u64 * 1_000_000_000
            + now.tv_nsec as u64
            + deadline
                .saturating_duration_since(Instant::now())
                .as_nanos() as u64;
        let mut random = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        if random == [0; 16] {
            return Err(err("zero recovery unit identity"));
        }
        let unit = format!(
            "hermit-unix-readback-{}.service",
            random.map(|b| format!("{b:02x}")).join("")
        );
        let mut arguments: Vec<OsString> = vec![
            "--recover-ids-before-ns".into(),
            absolute.to_string().into(),
        ];
        arguments.extend(ids.iter().map(|(k, id)| format!("{k}:{id}").into()));
        let launch = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::UnixReadback,
            unit: &unit,
            executable: &self.readback,
            arguments: &arguments,
            lifetime: CapabilityServiceLifetime::Bounded(15),
            writable_directories: &[],
        };
        let args = launch.arguments()?;
        // This is only a launch intent, never stop authority. Actual identity
        // must arrive on this invocation's private helper stdout below.
        self.sequence += 1;
        publish(
            &self
                .root
                .join(format!("recovery-query-{}.json", self.sequence)),
            &json!({"unit":unit,"ids":ids,"deadline_ns":absolute,"purpose":"later-recovery-only"}),
        )?;
        let (channel, helper) = recovery_channel()?;
        let index = self.start_command_with_stdin(
            Command::new(capability_unit::CAPABILITY_SUDO).args(args),
            Some(Stdio::from(helper)),
            deadline,
        )?;
        let mut actor = None;
        let mut identity = None;
        let result = (|| -> io::Result<()> {
            send_recovery_parent(&channel, deadline)?;
            loop {
                within(deadline)?;
                let text = self.commands[index].output()?;
                if actor.is_none() {
                    actor = recovery_actor(&unit, &text)?;
                    if let Some(actor) = &actor {
                        publish(
                            &self
                                .root
                                .join(format!("recovery-query-{}-actor.json", self.sequence)),
                            actor,
                        )?;
                        // The helper cannot query or exit normally before this
                        // capture/release. Retain identity before the send: even
                        // a lost release/result still belongs to this owner.
                        let captured = UnitIdentity::capture(&unit, &self.show(&unit, deadline)?)?;
                        stop_owned(Some(actor), &captured, || Ok(()))?;
                        identity = Some(captured);
                        release_recovery(&channel, actor, identity.as_ref().unwrap(), deadline)?;
                    }
                }
                // RemainAfterExit may keep systemd-run --wait alive. A final
                // line alone does not prove exit: stopping then could kill the
                // helper between its last write and exit(0). Wait for MainPID=0
                // before stopping this proven actor; launcher status is still
                // required after the stop.
                if text.lines().count() == 2 && text.ends_with('\n') {
                    let state = self.show(&unit, deadline)?;
                    held_unit_state(
                        identity
                            .as_ref()
                            .ok_or_else(|| err("query has no captured owner"))?,
                        &state,
                    )?;
                    if helper_exited(&state)? {
                        return Ok(());
                    }
                }
                if self.commands[index].poll(deadline)?.is_some() {
                    return Ok(());
                }
                pause(deadline)?;
            }
        })();
        // Even a failed query retires only an authenticated partial launch.
        // A collision or ambiguous launch with no actor remains UNKNOWN.
        let retired = match (&identity, &actor) {
            (Some(identity), Some(actor)) => self.retire_captured_unit(identity, actor, deadline),
            _ => self.retire_unit(&unit, deadline, actor.as_ref()),
        };
        if result.is_err() || retired.is_err() {
            let _ = self.commands[index].kill_group();
        }
        let waited = self.wait_command(index, deadline);
        result?;
        let retired = retired?;
        let (code, text) = waited?;
        let (actor_line, text) = text
            .split_once('\n')
            .ok_or_else(|| err("missing actual recovery actor"))?;
        if recovery_actor(&unit, &format!("{actor_line}\n"))? != actor {
            return Err(err("recovery actor changed during query"));
        }
        let parts: Vec<_> = text.split_whitespace().collect();
        if code != 0 || parts.len() != 6 || parts[0] != "recovery-v1" {
            return Err(err("recovery observation failed"));
        }
        let n: Vec<u64> = parts[1..]
            .iter()
            .map(|n| n.parse().map_err(|_| err("recovery response integer")))
            .collect::<io::Result<_>>()?;
        if n[0] != ids.len() as u64 || n[2] != absolute || n[1] >= n[3] || n[3] >= n[2] || n[4] != 2
        {
            return Err(err("recovery observation identity/deadline"));
        }
        Ok(json!({"ids":ids,"raw":text,"unit":retired,"purpose":"later-recovery-only"}))
    }
    pub fn finish(&mut self, deadline: Instant) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        let result = self.finish_once(deadline);
        let commands: Vec<_> = self.commands.iter().zip(&self.command_argv).map(|(f, argv)| {
            let log = |file: &File| { let mut bytes = vec![0; 8192];
                match file.read_at(&mut bytes, 0) { Ok(n) => { bytes.truncate(n); json!({"text":String::from_utf8_lossy(&bytes),"bytes":file.metadata().ok().map(|m| m.len())}) }, Err(e) => json!({"error":e.to_string()}) } };
            json!({"argv":argv,"pid":f.child.id(),"status":f.status.map(|s| s.to_string()),"adopted_waits":f.adopted,"stdout":log(&f.stdout),"stderr":log(&f.stderr)})
        }).collect();
        let mounts: Vec<_> = self
            .mounts
            .iter()
            .map(|m| json!({"path":m.path,"mountinfo":m.record,"device":m.device,"inode":m.inode}))
            .collect();
        let report = json!({"schema":1,"commands":commands,"mounts":mounts,"cleanup":if result.is_ok(){"complete"}else{"UNKNOWN"},"error":result.as_ref().err().map(ToString::to_string),"root":self.root,"child_launched":self.child_launched,"original_certificate_replaced":false});
        let saved = publish(&self.root.join("owner-terminal.json"), &report);
        self.finished = true;
        result.and(saved)
    }
    fn finish_once(&mut self, deadline: Instant) -> io::Result<()> {
        // Even an expired join window must signal commands whose exclusive
        // unreaped identities are still owned. It cannot claim their absence.
        for flight in &self.commands {
            if flight.status.is_none() {
                let _ = flight.kill_group();
            }
        }
        within(deadline)?;
        for f in &mut self.commands {
            if f.status.is_none() {
                f.kill_group()?;
                while f.poll(deadline)?.is_none() {
                    pause(deadline)?;
                }
            }
        }
        // Foreign/uncaptured mounts cannot be removed, even after a setup error.
        for path in [&self.bpffs, &self.recovery] {
            if mount_record(path)?.is_some() && !self.mounts.iter().any(|m| m.path == *path) {
                return Err(err("uncaptured mount; retain for recovery"));
            }
        }
        for mount in &self.mounts {
            mount.verify()?;
        }
        let mut inventory = if self.recovery.exists() {
            Inventory::scan(&self.recovery)?
        } else {
            Inventory::default()
        };
        if self.child_launched && !self.test_drained {
            return Err(err("test cgroup drain not proven; roots retained"));
        }
        let mut units = Vec::new();
        let mut unit_errors = Vec::new();
        for unit in inventory.units.clone() {
            let retired = self.retire_unit(&unit, deadline, inventory.actors.get(&unit))?;
            if let Some(actor) = inventory.actors.get(&unit) {
                let path = Path::new(actor["cgroup"].as_str().expect("validated actor path"));
                match path.symlink_metadata() {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    _ => unit_errors.push(format!(
                        "original actor cgroup still present or unreadable: {}",
                        path.display()
                    )),
                }
                if retired["state"] == "drained"
                    && (retired["identity"]["invocation"] != actor["invocation"]
                        || retired["identity"]["inode"] != actor["inode"]
                        || retired["identity"]["device"] != actor["device"])
                {
                    unit_errors.push(format!(
                        "unit identity changed since original receipt: {unit}"
                    ));
                }
                units.push(json!({"observed":retired,"original":actor}));
            } else {
                if retired["state"] == "already-absent" {
                    unit_errors.push(format!(
                        "unit absent without retained original cgroup identity: {unit}"
                    ));
                }
                units.push(retired);
            }
        }
        // Stopping helpers may append their last durable original-ID records.
        if self.recovery.exists() {
            inventory = Inventory::scan(&self.recovery)?;
        }
        inventory.errors.extend(unit_errors);
        if self.child_launched && inventory.launched.is_empty() {
            inventory
                .errors
                .push("test started but no launch inventory: cleanup UNKNOWN".into());
        }
        publish(
            &self.root.join("recovery-inventory.json"),
            &json!({"ids":inventory.ids,"launched":inventory.launched,"complete":inventory.complete,"errors":inventory.errors,"units":units}),
        )?;
        if !inventory.errors.is_empty() {
            return Err(err(inventory.errors.join("; ")));
        }
        // Preserve bounded journal/receipt bytes before retiring the tmpfs.
        let retained = self.root.join("recovery-retained");
        private(&retained)?;
        if self.recovery.exists() {
            for entry in fs::read_dir(&self.recovery)? {
                let path = entry?.path();
                let data = read(&path, MAX_FILE)?;
                let mut file = File::options()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(retained.join(path.file_name().unwrap()))?;
                file.write_all(&data)?;
                file.sync_all()?;
            }
        }
        if let Some(mount) = self.mounts.iter().find(|m| m.path == self.bpffs).cloned() {
            mount.verify()?;
            self.sudo(
                &["/usr/bin/umount".into(), mount.path.as_os_str().into()],
                deadline,
            )?;
            if mount_record(&mount.path)?.is_some() {
                return Err(err("bpffs mount remains"));
            }
        }
        let ids: Vec<_> = inventory.ids.iter().copied().collect();
        let mut observations = Vec::new();
        for batch in ids.chunks(72) {
            observations.push(self.recovery_query(batch, deadline)?);
        }
        publish(
            &self.root.join("recovery-observations.json"),
            &json!({"purpose":"later-recovery-only","original_certificate_replaced":false,"ids":ids.len(),"observations":observations}),
        )?;
        if let Some(mount) = self
            .mounts
            .iter()
            .find(|m| m.path == self.recovery)
            .cloned()
        {
            mount.verify()?;
            self.sudo(
                &["/usr/bin/umount".into(), mount.path.as_os_str().into()],
                deadline,
            )?;
            if mount_record(&mount.path)?.is_some() {
                return Err(err("recovery mount remains"));
            }
        }
        for (path, before) in &self.bindings {
            if digest(path, 1024 * 1024 * 1024)? != *before {
                return Err(err("prepared artifact changed during attempt"));
            }
        }
        within(deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const TEST_UNIT: &str = "hermit-unix-readback-01000000000000000000000000000000.service";
    #[test]
    fn final_output_does_not_authorize_killing_a_still_running_helper() {
        assert!(!helper_exited("MainPID=123\n").unwrap());
        assert!(helper_exited("MainPID=0\n").unwrap());
        for state in ["", "MainPID=\n", "MainPID=-1\n", "MainPID=0\nMainPID=0\n"] {
            assert!(helper_exited(state).is_err());
        }
    }

    fn test_identity() -> UnitIdentity {
        UnitIdentity {
            unit: TEST_UNIT.into(),
            invocation: "1234567890abcdef1234567890abcdef".into(),
            cgroup: PathBuf::from(format!("/sys/fs/cgroup/system.slice/{TEST_UNIT}")),
            directory: tempfile::tempfile().unwrap(),
            device: 7,
            inode: 8,
        }
    }
    #[test]
    fn collision_or_failed_submission_never_authorizes_a_stop() {
        let root = PathBuf::from(std::env::var_os("PROVISION_TEST_ROOT").unwrap());
        let test = tempfile::tempdir_in(root).unwrap();
        let mut owner = Owner::new(&test.path().join("terminal")).unwrap();
        assert!(
            owner
                .retire_unit(
                    TEST_UNIT,
                    Instant::now() + std::time::Duration::from_secs(1),
                    None
                )
                .is_err()
        );
        // No systemctl show, sudo, or stop command can run for a mere intent.
        assert!(owner.commands.is_empty());
        assert!(owner.command_argv.is_empty());
        let identity = test_identity();
        let mut mutations = 0;
        assert!(
            stop_owned(None, &identity, || {
                mutations += 1;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(mutations, 0);
    }
    #[test]
    fn retained_launch_authority_rejects_replacement_before_mutation() {
        let identity = test_identity();
        let original = serde_json::to_value(identity.receipt()).unwrap();
        for field in ["unit", "invocation", "cgroup", "device", "inode"] {
            let mut replaced = original.clone();
            replaced[field] = Value::Null;
            let mut mutations = 0;
            assert!(
                stop_owned(Some(&replaced), &identity, || {
                    mutations += 1;
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(mutations, 0, "{field}");
        }
    }
    #[test]
    fn actual_actor_can_retire_a_partial_failed_launch() {
        let identity = test_identity();
        let line = format!(
            "recovery-actor-v1 {} {} {} {}\n",
            identity.invocation,
            identity.device,
            identity.inode,
            identity.cgroup.display()
        );
        let actor = recovery_actor(TEST_UNIT, &line).unwrap().unwrap();
        let mut mutations = 0;
        stop_owned(Some(&actor), &identity, || {
            mutations += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(mutations, 1);
        // No successful recovery observation is minted by actor evidence alone.
        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains("recovery-v1 "));
    }
    #[test]
    fn actor_receipt_requires_complete_launch_bound_record() {
        assert!(
            recovery_actor(TEST_UNIT, "recovery-actor-v1 partial")
                .unwrap()
                .is_none()
        );
        for line in [
            "recovery-v1 72 1 2 3 2\n".to_owned(),
            format!(
                "recovery-actor-v1 {} 7 8 /sys/fs/cgroup/system.slice/{TEST_UNIT}\n",
                "0".repeat(32)
            ),
            format!(
                "recovery-actor-v1 {} 7 0 /sys/fs/cgroup/system.slice/{TEST_UNIT}\n",
                "1".repeat(32)
            ),
            format!(
                "recovery-actor-v1 {} 7 8 /sys/fs/cgroup/system.slice/foreign.service\n",
                "1".repeat(32)
            ),
        ] {
            assert!(recovery_actor(TEST_UNIT, &line).is_err(), "{line}");
        }
    }
    fn held_test_identity() -> (tempfile::TempDir, UnitIdentity, String) {
        let root = PathBuf::from(std::env::var_os("PROVISION_TEST_ROOT").unwrap());
        let path = tempfile::tempdir_in(root).unwrap();
        let directory = File::open(path.path()).unwrap();
        let meta = directory.metadata().unwrap();
        let identity = UnitIdentity {
            directory,
            device: meta.dev(),
            inode: meta.ino(),
            cgroup: path.path().to_owned(),
            ..test_identity()
        };
        let state = format!(
            "Id={}\nLoadState=loaded\nInvocationID={}\nControlGroup=\nMainPID=0\n",
            identity.unit, identity.invocation
        );
        (path, identity, state)
    }
    #[test]
    fn captured_live_directory_survives_natural_helper_cgroup_removal() {
        let (path, identity, state) = held_test_identity();
        assert!(held_unit_state(&identity, &state).is_err());
        fs::remove_dir(path.path()).unwrap();
        assert_eq!(identity.directory.metadata().unwrap().nlink(), 0);
        held_unit_state(&identity, &state).unwrap();
        // Reproduces why a late live capture cannot replace the held identity.
        assert!(UnitIdentity::capture(&identity.unit, &state).is_err());
        assert!(
            identity
                .drained(&format!("Id={}\nLoadState=not-found\n", identity.unit))
                .unwrap()
        );
    }
    #[test]
    fn captured_identity_rejects_replaced_invocation_path_and_live_helper() {
        let (path, identity, state) = held_test_identity();
        fs::remove_dir(path.path()).unwrap();
        for replaced in [
            state.replace(&identity.invocation, &"f".repeat(32)),
            state.replace(&identity.unit, "foreign.service"),
            state.replace("MainPID=0", "MainPID=123"),
            state.replace(
                "ControlGroup=",
                "ControlGroup=/system.slice/foreign.service",
            ),
            state.replace("LoadState=loaded", "LoadState=not-found"),
            format!("{state}MainPID=0\n"),
        ] {
            assert!(held_unit_state(&identity, &replaced).is_err(), "{replaced}");
        }
        fs::create_dir(path.path()).unwrap();
        assert!(held_unit_state(&identity, &state).is_err());
        assert!(
            identity
                .drained(&format!("Id={}\nLoadState=not-found\n", identity.unit))
                .is_err()
        );
    }
    #[test]
    fn collision_cannot_release_recovery_work() {
        let (owner, helper) = recovery_channel().unwrap();
        let identity = test_identity();
        let original = serde_json::to_value(identity.receipt()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        for key in ["unit", "invocation", "cgroup", "device", "inode"] {
            let mut replaced = original.clone();
            replaced[key] = Value::Null;
            assert!(release_recovery(&owner, &replaced, &identity, deadline).is_err());
            let mut byte = 0u8;
            assert_eq!(
                unsafe {
                    libc::recv(
                        helper.as_raw_fd(),
                        (&mut byte as *mut u8).cast(),
                        1,
                        libc::MSG_DONTWAIT,
                    )
                },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::EAGAIN)
            );
        }
        release_recovery(&owner, &original, &identity, deadline).unwrap();
        let mut byte = 0u8;
        assert_eq!(
            unsafe {
                libc::recv(
                    helper.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            1
        );
        assert_eq!(byte, 1);
    }
    #[test]
    fn expired_original_deadline_never_releases_recovery_work() {
        let (owner, helper) = recovery_channel().unwrap();
        let identity = test_identity();
        let original = serde_json::to_value(identity.receipt()).unwrap();
        assert!(release_recovery(&owner, &original, &identity, Instant::now()).is_err());
        let mut byte = 0u8;
        assert_eq!(
            unsafe {
                libc::recv(
                    helper.as_raw_fd(),
                    (&mut byte as *mut u8).cast(),
                    1,
                    libc::MSG_DONTWAIT,
                )
            },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN)
        );
    }
    fn record(inc: u64, ordinal: u64, phase: u32, kind: u32, id: u32) -> Vec<u8> {
        let mut r = vec![0; 104];
        for (offset, n) in [(0, 0x554750494e303031u64), (8, inc), (16, ordinal)] {
            r[offset..offset + 8].copy_from_slice(&n.to_le_bytes());
        }
        for (offset, n) in [(48, 3u32), (52, phase), (56, kind), (60, id)] {
            r[offset..offset + 4].copy_from_slice(&n.to_le_bytes());
        }
        r
    }
    #[test]
    fn interrupted_journal_retains_ids_but_cannot_claim_complete() {
        let mut inventory = Inventory::default();
        let mut bytes = record(7, 1, 18, 0, 41);
        bytes.extend([0, 1]);
        assert!(inventory.journal("ug-0000000000000007", &bytes).is_err());
        assert!(inventory.ids.contains(&(0, 41)));
        assert!(!inventory.complete.contains(&7));
    }
    #[test]
    fn complete_startup_failure_uses_actual_ready_record() {
        let mut inventory = Inventory::default();
        let mut bytes = record(7, 1, 18, 0, 41);
        bytes.extend(record(7, 2, 20, 0, 0));
        inventory.journal("ug-0000000000000007", &bytes).unwrap();
        assert_eq!(inventory.ids, BTreeSet::from([(0, 41)]));
        assert!(inventory.complete.contains(&7));
        assert!(!full(&inventory.ids)); // recovery is not the full gate certificate
    }
    #[test]
    fn filename_ordinal_and_terminal_disagreement_refuse() {
        let mut inventory = Inventory::default();
        assert!(
            inventory
                .journal("ug-0000000000000008", &record(7, 1, 18, 0, 41))
                .is_err()
        );
        assert!(
            inventory
                .journal("ug-0000000000000007", &record(7, 2, 18, 0, 41))
                .is_err()
        );
        inventory.terminal.insert(7, BTreeSet::from([(0, 42)]));
        let mut bytes = record(7, 1, 18, 0, 41);
        bytes.extend(record(7, 2, 20, 0, 0));
        assert!(inventory.journal("ug-0000000000000007", &bytes).is_err());
    }
    #[test]
    fn foreign_units_and_invalid_populations_refuse() {
        assert!(!valid_unit("unrelated.service"));
        assert!(!valid_unit(
            "hermit-unix-00000000000000000000000000000000.service"
        ));
        assert!(valid_unit(
            "hermit-unix-01000000000000000000000000000000.service"
        ));
        for value in [
            json!([[0, 0]]),
            json!([[3, 1]]),
            json!([[0, 1], [0, 1]]),
            json!([[0, 4294967296u64]]),
        ] {
            assert!(pairs(&value).is_err());
        }
    }
    #[test]
    fn failed_prepare_and_undrained_test_keep_primary_evidence() {
        let root = PathBuf::from(
            std::env::var_os("PROVISION_TEST_ROOT").expect("bounded test output root"),
        );
        let test = tempfile::tempdir_in(root).unwrap();
        let mut owner = Owner::new(&test.path().join("terminal")).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        assert!(owner.prepare(Path::new("/absent-cli"), deadline).is_err());
        owner.finish(deadline).unwrap();
        let row: Value = serde_json::from_slice(
            &read(&owner.root.join("owner-terminal.json"), MAX_FILE).unwrap(),
        )
        .unwrap();
        assert_eq!(row["child_launched"], false);
        assert_eq!(row["original_certificate_replaced"], false);
        let mut owner = Owner::new(&test.path().join("terminal-2")).unwrap();
        owner.child_launched = true;
        assert!(owner.finish(deadline).is_err());
        let row: Value = serde_json::from_slice(
            &read(&owner.root.join("owner-terminal.json"), MAX_FILE).unwrap(),
        )
        .unwrap();
        assert_eq!(row["cleanup"], "UNKNOWN");
        assert!(owner.root.exists());
    }
}
