/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `/proc` directories Detcore serves itself, each on a placeholder
//! descriptor: the caller's own descriptor directory, `/proc/<pid>/fd`, from
//! Detcore's own descriptor table (package K slice F1, toward
//! <https://github.com/rrnewton/hermit/issues/4604>), and `/proc` itself, from
//! Detcore's procfs model (`procfs::listing`, package K slice P1). The models,
//! and the parts of Linux they follow, are in [`crate::procfs::fd_directory`].
//!
//! An open of such a directory injects `openat(AT_FDCWD, <placeholder>,
//! O_RDONLY | O_DIRECTORY)`, with the caller's `O_CLOEXEC`. The placeholder is
//! the empty, read-only tmpfs hermit-cli mounts at the container's
//! `/proc/fs/nfsd` (`DetConfig::directory_placeholder`), procfs's own
//! permanently empty mount point, which Detcore's `--proc` level table hides
//! at every level, so no guest path reaches it. Its identity, `(dev, ino)`,
//! is the same for every placeholder descriptor and no other object has it.
//! The descriptor's description records which directory it stands for
//! (`DetFd::placeholder`), and Detcore answers every call through it from its
//! model ([`Detcore::answer_placeholder_call`]): `getdents` and `getdents64`,
//! `lseek`, `fstat` and `statx`, `fstatfs`, the fdinfo, `fcntl`'s status flags,
//! the `O_PATH` refusals, `fchdir` (an injected `chdir` to the directory's own
//! path), `flock` (each directory its own lock, as each has an inode of its own
//! on Linux), and every call procfs refuses, with procfs's errno. A name looked
//! up below the descriptor is looked up at the directory's absolute spelling
//! (`/proc/x`, `/proc/self/fd/N`) instead, for each injection that carries it;
//! a name that is not respelled is looked up in the empty placeholder and not
//! found. No call through the descriptor reaches the placeholder's
//! filesystem, whose answers are tmpfs's.
//!
//! The table a descriptor directory lists is Detcore's own, which the tool
//! keeps for the guest (ptrace, KVM) and F0a made complete: every descriptor
//! it models, and every one it knows is open but does not model
//! (`FileMetadata::present_unmodelled`). Where a receive may have installed
//! descriptors Detcore cannot name (`FileMetadata::unaccounted_receipts`),
//! and for another process's or thread's table (a descriptor a fork's child
//! inherited), the kernel beneath the backend lists `/proc/<pid>/fd`, as
//! before.
//!
//! Where Detcore runs in the guest process and discovers descriptors only as
//! they are used (`BackendCapabilities::tool_shares_guest_descriptor_table`:
//! DBT, SaBRe, LiteInst, in-guest-trap), the guest's table also holds the
//! backend runtime's own descriptors, which no fact names but the one
//! `Guest::is_runtime_owned_fd` reports (DBT's RPC socket): measured,
//! DynamoRIO's 197 to 201 under DBT, and SaBRe's 3, 101 and 1025, in a
//! listing of the guest's table. A listing of that table could not be
//! Linux's, so there both directories stay the kernel's, as before (a
//! capability-keyed residual, until a provider fact names the runtime's
//! descriptors). Without a placeholder (`--no-namespace`, library callers,
//! record and replay) Detcore serves neither directory either, and each is
//! the backend's own.

// TODO-HUMAN-REVIEW(#4604): Review the Detcore-served descriptor directory and
// `/proc` root, a new determinization strategy: on ptrace their listings,
// metadata and the calls through their descriptors replace the kernel's.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nix::fcntl::OFlag;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Errno;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::PathPtr;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::StatxMask;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;
use tracing::warn;

use super::DirectoryOrder;
use super::GetdentsCall;
use super::StatTarget;
use crate::dirents::DirEntry;
use crate::dirents::DirentFormat;
use crate::fd::FdType;
use crate::fd::PlaceholderState;
use crate::procfs::fd_directory;
use crate::procfs::fd_directory::DescriptorDirectoryBinding;
use crate::procfs::fd_directory::DescriptorDirectoryTable;
use crate::procfs::fd_directory::PlaceholderDirectory;
use crate::procfs_inode;
use crate::procfs_inode::DirectoryInodes;
use crate::procfs_inode::ProcfsDirectory;
use crate::record_or_replay::RecordOrReplay;
use crate::stat::DetStat;
use crate::stat::StatxAttributes;
use crate::tool_local::Detcore;

/// The identities `(dev, ino)` the placeholder directory has as the guest's
/// backend reports it, learnt at each placeholder open in this tool process:
/// every placeholder descriptor is open on it, no other object has it, and no
/// guest path reaches it. A received descriptor of this identity is a
/// placeholder whose tag says which directory it stands for
/// ([`Detcore::recognize_received_placeholder`]), and no guest descriptor of
/// it is ever seeked into the tags' range ([`Detcore::seek_would_place_a_tag`]).
static PLACEHOLDER_IDENTITIES: Mutex<BTreeSet<(u64, u64)>> = Mutex::new(BTreeSet::new());

/// Whether this tool process has opened a placeholder: until it has, no path
/// can lead below one and no received descriptor can be one, and nothing is
/// read or injected to look ([`Detcore::may_respell_below_descriptor_directory`],
/// [`Detcore::recognize_received_placeholder`]).
static OPENED_ANY: AtomicBool = AtomicBool::new(false);

/// The directory an open names.
#[derive(Debug, Clone, Copy)]
pub(super) enum OpenedDirectory {
    /// One of the caller's own descriptor directories, by its spelling.
    Own(DescriptorDirectoryTable),
    /// `/proc` itself, by any spelling that reaches it ([`names_procfs_root`]).
    Root,
    /// The directory a descriptor link of the caller's (`/proc/self/fd/N`)
    /// reaches: the placeholder directory `N` is.
    Reopened(PlaceholderDirectory),
}

/// The calls whose path operands are each looked up relative to a directory
/// descriptor operand, as (descriptor, path) argument indices. Not `execve`
/// or `execveat` (named residual: a name below a placeholder reaches the
/// placeholder there, where it is not found).
fn relative_operands(sysno: Sysno) -> &'static [(usize, usize)] {
    match sysno {
        Sysno::openat
        | Sysno::openat2
        | Sysno::newfstatat
        | Sysno::statx
        | Sysno::faccessat
        | Sysno::faccessat2
        | Sysno::readlinkat
        | Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::unlinkat
        | Sysno::fchmodat
        | Sysno::fchmodat2
        | Sysno::fchownat
        | Sysno::utimensat
        | Sysno::futimesat
        | Sysno::name_to_handle_at => &[(0, 1)],
        Sysno::symlinkat => &[(1, 2)],
        Sysno::renameat | Sysno::renameat2 | Sysno::linkat => &[(0, 1), (2, 3)],
        _ => &[],
    }
}

/// The path operands of the calls that look a path up from the working
/// directory or the root, by argument index. With [`relative_operands`]'s
/// paths, an absolute one may lead through a descriptor link to a
/// placeholder (`/proc/self/fd/D/N`), which
/// [`Detcore::respelled_below_descriptor_directory`] spells as Linux resolves
/// it.
fn absolute_operands(sysno: Sysno) -> &'static [usize] {
    match sysno {
        Sysno::open
        | Sysno::creat
        | Sysno::stat
        | Sysno::lstat
        | Sysno::access
        | Sysno::readlink
        | Sysno::chdir
        | Sysno::mkdir
        | Sysno::rmdir
        | Sysno::unlink
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::truncate
        | Sysno::utime
        | Sysno::utimes
        | Sysno::mknod
        | Sysno::statfs
        | Sysno::getxattr
        | Sysno::lgetxattr
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::listxattr
        | Sysno::llistxattr
        | Sysno::removexattr
        | Sysno::lremovexattr => &[0],
        Sysno::link | Sysno::rename => &[0, 1],
        Sysno::symlink | Sysno::inotify_add_watch => &[1],
        _ => &[],
    }
}

/// `openat2`'s `RESOLVE_*` flags that scope a lookup to the directory it
/// starts at, or to that directory's mount: `RESOLVE_BENEATH`,
/// `RESOLVE_IN_ROOT` and `RESOLVE_NO_XDEV`. A respelled lookup starts at the
/// root instead, from `AT_FDCWD`, where none of them would mean what the
/// caller asked, so an `openat2` with one of them below a placeholder is
/// refused (`EXDEV`, the answer each gives a lookup that leaves its scope:
/// named residual). Detcore answers every guest `openat2` with `ENOSYS`
/// today (`lib.rs`), so this guards the respelling for any later caller.
const SCOPED_RESOLVE: u64 = libc::RESOLVE_BENEATH | libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_XDEV;

/// The argument `index` of a call's raw arguments.
fn argument(args: &syscalls::SyscallArgs, index: usize) -> usize {
    match index {
        0 => args.arg0,
        1 => args.arg1,
        2 => args.arg2,
        3 => args.arg3,
        4 => args.arg4,
        _ => args.arg5,
    }
}

/// `relative`, a path looked up below a placeholder directory, below `base`,
/// the directory's absolute spelling: with its `.` components and repeated
/// slashes left out, which name nothing (so that a valid path of `./`
/// repeats, up to Linux's limit, stays one), keeping a final `/` where the
/// path ends in one or in `.` (Linux then requires a directory and follows
/// a final link; a final `..` is a directory already). A path of only `.`
/// components, or none, is the directory itself, `base/.`. The bytes of each
/// component are kept as they are, whatever they hold.
fn below(base: &Path, relative: &[u8]) -> Vec<u8> {
    let components = relative
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    let mut out = base.as_os_str().as_bytes().to_vec();
    if components.is_empty() {
        out.extend_from_slice(b"/.");
        return out;
    }
    for component in &components {
        out.push(b'/');
        out.extend_from_slice(component);
    }
    let last = relative
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    if relative.ends_with(b"/") || last == b"." {
        out.push(b'/');
    }
    out
}

/// What one component of a path below `/proc` is, for
/// [`resolved_procfs_dotdot`]: a directory every procfs has, which a `..`
/// can leave as Linux's walk does, by kind; or anything else, which it
/// cannot (a file, a link that leads elsewhere, a name that may not exist).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ProcfsLevel {
    /// `/proc`.
    Proc,
    /// A process's directory: `self`, or a number.
    Process,
    /// A process's `task`.
    Tasks,
    /// A thread's directory: a number in `task`.
    Thread,
    /// `thread-self`'s thread, which spells back as `thread-self`.
    ThreadSelf,
    /// A process's or thread's `fd`.
    Descriptors,
    /// Anything else.
    Opaque,
}

/// `path`, absolute, with each `..` resolved as Linux's walk resolves it
/// where every component a `..` leaves is a directory every procfs has on
/// the way down from `/proc`, so no symbolic link or file can be on the way:
/// `self` (a link to the caller's process directory, one level down) and a
/// numbered process directory, `thread-self` (a link to `<pid>/task/<tid>`,
/// three levels down), a process's `task` and a numbered thread in it, and
/// `fd`. A `..` at the root stays there. `None` for a path without `..`, and
/// for one whose `..` follows anything else, which only the kernel's walk
/// can resolve.
pub(super) fn resolved_procfs_dotdot(path: &Path) -> Option<PathBuf> {
    let bytes = path.as_os_str().as_bytes();
    if !bytes.starts_with(b"/") {
        return None;
    }
    let components = bytes
        .split(|byte| *byte == b'/')
        .filter(|component| !component.is_empty() && *component != b".")
        .collect::<Vec<_>>();
    if !components.contains(&b"..".as_slice()) {
        return None;
    }
    let number =
        |name: &[u8]| !name.is_empty() && name[0] != b'0' && name.iter().all(u8::is_ascii_digit);
    // Each component kept, with what it is and whether `thread-self`'s
    // expansion pushed it.
    let mut kept: Vec<(&[u8], ProcfsLevel, bool)> = Vec::new();
    for name in components {
        if name == b".." {
            match kept.last() {
                None => {}
                Some((_, ProcfsLevel::Opaque, _)) => return None,
                Some(_) => {
                    kept.pop();
                }
            }
            continue;
        }
        let level = match (kept.last().map(|(_, level, _)| *level), name) {
            (None, b"proc") => ProcfsLevel::Proc,
            (Some(ProcfsLevel::Proc), b"self") => ProcfsLevel::Process,
            (Some(ProcfsLevel::Proc), b"thread-self") => {
                kept.push((b"self", ProcfsLevel::Process, true));
                kept.push((b"task", ProcfsLevel::Tasks, true));
                kept.push((b"thread-self", ProcfsLevel::ThreadSelf, true));
                continue;
            }
            (Some(ProcfsLevel::Proc), name) if number(name) => ProcfsLevel::Process,
            (Some(ProcfsLevel::Process), b"task") => ProcfsLevel::Tasks,
            (Some(ProcfsLevel::Tasks), name) if number(name) => ProcfsLevel::Thread,
            (Some(ProcfsLevel::Process | ProcfsLevel::Thread | ProcfsLevel::ThreadSelf), b"fd") => {
                ProcfsLevel::Descriptors
            }
            _ => ProcfsLevel::Opaque,
        };
        kept.push((name, level, false));
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index < kept.len() {
        // `thread-self`'s expansion, whole, spells back as itself.
        if kept[index].2
            && kept
                .get(index + 2)
                .is_some_and(|(_, level, _)| *level == ProcfsLevel::ThreadSelf)
        {
            out.extend_from_slice(b"/thread-self");
            index += 3;
            continue;
        }
        out.push(b'/');
        out.extend_from_slice(kept[index].0);
        index += 1;
    }
    if out.is_empty() {
        out.push(b'/');
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

/// Whether the absolute guest path `path` names `/proc` itself: its
/// components, with the empty and `.` ones left out, are `proc` alone, or
/// reach it back through `..` from directories every procfs has
/// ([`resolved_procfs_dotdot`]). Any other spelling is the kernel's to
/// resolve.
fn names_procfs_root(path: &Path) -> bool {
    let resolved = resolved_procfs_dotdot(path).unwrap_or_else(|| path.to_path_buf());
    resolved.as_os_str().as_bytes().starts_with(b"/")
        && resolved
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .eq(Path::new("/proc").components())
}

/// The leading descriptor link and the rest of an absolute path that leads
/// through one (`/proc/self/fd/D/rest`, `/dev/fd/D/rest`,
/// `/proc/thread-self/fd/D/rest`, `/proc/<pid>/task/<tid>/fd/D/rest`), if
/// the path goes on past the link: a rest, or a final `/`, which follows the
/// link (an empty rest then, the directory itself). Empty and `.` components
/// before the descriptor's number name nothing, as in Linux's walk, and are
/// skipped (`//proc/self/fd/D/N`, `/proc//self/fd/D/N`,
/// `/proc/./self/fd/D/N`). The link's components are ASCII; the rest is kept
/// byte for byte, whatever it holds. Any other spelling (a `..` before the
/// number, a symbolic link of the guest's to `/proc/self/fd/D`) is not
/// recognized (named residual).
fn through_descriptor_link(path: &[u8]) -> Option<(PathBuf, Vec<u8>)> {
    if !path.starts_with(b"/") {
        return None;
    }
    let mut names: Vec<&str> = Vec::new();
    let mut at = 0;
    loop {
        while path.get(at) == Some(&b'/') {
            at += 1;
        }
        let start = at;
        while at < path.len() && path[at] != b'/' {
            at += 1;
        }
        if start == at {
            return None;
        }
        let name = &path[start..at];
        if name == b"." {
            continue;
        }
        if !name.is_ascii() {
            return None;
        }
        names.push(std::str::from_utf8(name).ok()?);
        match names.as_slice() {
            ["dev", "fd", _] | ["proc", _, "fd", _] | ["proc", _, "task", _, "fd", _] => break,
            ["dev"]
            | ["dev", "fd"]
            | ["proc"]
            | ["proc", _]
            | ["proc", _, "fd"]
            | ["proc", _, "task"]
            | ["proc", _, "task", _]
            | ["proc", _, "task", _, "fd"] => {}
            _ => return None,
        }
    }
    let rest = &path[at..];
    if rest.is_empty() {
        return None;
    }
    let rest = &rest[rest.iter().take_while(|byte| **byte == b'/').count()..];
    Some((
        PathBuf::from(format!("/{}", names.join("/"))),
        rest.to_vec(),
    ))
}

/// How many symbolic links Linux follows to reach the directory of the
/// descriptor link `link` ([`through_descriptor_link`]) before the link
/// itself: `/dev/fd` (a link to `/proc/self/fd`) and then `self`, 2;
/// `/proc/self` or `/proc/thread-self`, 1; a numbered task, 0. Each of these
/// counts against the lookup's `MAXSYMLINKS`, as the descriptor link does
/// (`pick_link`'s `total_link_count`).
fn selector_links(link: &Path) -> usize {
    let mut names = link.components().filter_map(|component| match component {
        std::path::Component::Normal(name) => Some(name),
        _ => None,
    });
    match (names.next(), names.next()) {
        (Some(first), _) if first == "dev" => 2,
        (_, Some(second)) if second == "self" || second == "thread-self" => 1,
        _ => 0,
    }
}

/// procfs's largest pathname, `PATH_MAX`, with its NUL.
const PATH_LIMIT: usize = libc::PATH_MAX as usize;

/// How many descriptor links to placeholder directories a respelled path may
/// lead through, as Linux's lookup follows at most `MAXSYMLINKS` links
/// (<https://github.com/torvalds/linux/blob/v6.17/include/linux/namei.h>).
const LINK_LIMIT: usize = 40;

/// One path operand of a call, respelled as Linux resolves it below a
/// placeholder directory ([`Detcore::respelled_below_descriptor_directory`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RespelledOperand {
    /// The index of the call's directory descriptor argument the path was
    /// relative to, which the respelled call gives as `AT_FDCWD`.
    pub(crate) dirfd: Option<usize>,
    /// The index of the path argument.
    pub(crate) path: usize,
    /// The respelled path, absolute, without its NUL.
    pub(crate) bytes: Vec<u8>,
}

/// The argument of a call that names the descriptor it operates on, for the
/// calls Detcore answers through a placeholder descriptor
/// ([`Detcore::answer_placeholder_call`]): `Some(index)` for a call that
/// operates on its descriptor operand `index`, the path calls among them that
/// name it with an empty path and `AT_EMPTY_PATH` (or, for `utimensat`, no
/// path at all). Every other call that names a descriptor reaches it as the
/// kernel's descriptor table does (`close`, the `dup`s, `fcntl`'s
/// table commands, `poll` and `epoll`, which see a directory with no `poll`
/// method alike on every file system), or looks a name up below it
/// ([`relative_operands`]).
fn placeholder_operand<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: &syscalls::SyscallArgs,
) -> Option<usize> {
    let names_itself = |path: usize, flags: usize| {
        args_flag(args, flags) & libc::AT_EMPTY_PATH != 0
            && PathPtr::from_ptr(argument(args, path) as *const libc::c_char).is_some_and(
                |pointer| {
                    pointer
                        .read(memory)
                        .is_ok_and(|path: PathBuf| path.as_os_str().is_empty())
                },
            )
    };
    match sysno {
        Sysno::getdents
        | Sysno::getdents64
        | Sysno::lseek
        | Sysno::fchdir
        | Sysno::fcntl
        | Sysno::read
        | Sysno::readv
        | Sysno::pread64
        | Sysno::preadv
        | Sysno::preadv2
        | Sysno::write
        | Sysno::writev
        | Sysno::pwrite64
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::ioctl
        | Sysno::fsync
        | Sysno::fdatasync
        | Sysno::syncfs
        | Sysno::flock
        | Sysno::fchmod
        | Sysno::fchown
        | Sysno::fgetxattr
        | Sysno::fsetxattr
        | Sysno::flistxattr
        | Sysno::fremovexattr
        | Sysno::ftruncate
        | Sysno::fallocate
        | Sysno::fadvise64
        | Sysno::readahead
        | Sysno::sync_file_range
        | Sysno::fstatfs => Some(0),
        // A file-backed mapping only: an anonymous one ignores its
        // descriptor argument (`ksys_mmap_pgoff`).
        Sysno::mmap if args.arg3 as i32 & libc::MAP_ANONYMOUS == 0 => Some(4),
        // `futimens`: no path at all names the descriptor itself.
        Sysno::utimensat if args.arg1 == 0 => Some(0),
        Sysno::utimensat if names_itself(1, 3) => Some(0),
        Sysno::fchmodat2 if names_itself(1, 3) => Some(0),
        Sysno::fchownat if names_itself(1, 4) => Some(0),
        Sysno::faccessat2 if names_itself(1, 3) => Some(0),
        Sysno::name_to_handle_at if names_itself(1, 4) => Some(0),
        _ => None,
    }
}

/// The flags argument `index` of a call, as an `int`.
fn args_flag(args: &syscalls::SyscallArgs, index: usize) -> i32 {
    argument(args, index) as i32
}

impl<T: RecordOrReplay> Detcore<T> {
    /// The caller's pid and tid as the guest numbers them, where Detcore
    /// serves placeholder directories now: where hermit-cli mounted the
    /// placeholder (`DetConfig::directory_placeholder`: not under
    /// `--no-namespace`, nor for a library caller), where Detcore keeps the
    /// guest's descriptor table (not the in-guest backends, above), renders
    /// procfs files (not under record and replay, nor after a guest
    /// `chroot`), and knows the caller's numbers (not in a nested PID
    /// namespace, nor before its process is known). Read from the thread's
    /// state alone, so that each injection can ask
    /// ([`Self::respelled_below_descriptor_directory`]).
    fn descriptor_directory_caller<G: Guest<Self>>(guest: &G) -> Option<(i32, i32)> {
        let config = guest.config();
        let state = guest.thread_state();
        if config.directory_placeholder.is_none()
            || config.backend.tool_shares_guest_descriptor_table
            || config.recordreplay_modes
            || state.changed_root.load(Ordering::SeqCst)
            || state.nested_pid_namespace
        {
            return None;
        }
        Some((state.detpid?.as_raw(), state.dettid.as_raw()))
    }

    /// Whether Detcore serves placeholder directories to the caller now
    /// ([`Self::descriptor_directory_caller`]).
    fn serves_descriptor_directories<G: Guest<Self>>(&self, guest: &G) -> bool {
        Self::descriptor_directory_caller(guest).is_some()
    }

    /// The placeholder state of descriptor `fd`, if it is one.
    pub(crate) fn placeholder_of<G: Guest<Self>>(guest: &G, fd: RawFd) -> Option<PlaceholderState> {
        if fd < 0 {
            return None;
        }
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.placeholder())
            .ok()
            .flatten()
    }

    /// The placeholder directory that the caller's descriptor link `path`
    /// (the whole of it: `/proc/self/fd/N`, `/dev/fd/N`, ...) reaches, if it
    /// is one. A process's link (`/proc/self/fd/N`) reads the thread group
    /// leader's table, so a thread that holds a table of its own (cloned
    /// without `CLONE_FILES`) does not answer it from its own; a thread's
    /// link (`/proc/thread-self/fd/N`) reads the caller's.
    fn own_link_directory<G: Guest<Self>>(guest: &G, path: &Path) -> Option<PlaceholderDirectory> {
        let own = Self::descriptor_directory_caller(guest)?;
        let link = crate::syscalls::namespace::own_descriptor_link(path, Some(own))?;
        if link.table == crate::syscalls::namespace::DescriptorTable::Process {
            let state = guest.thread_state();
            if state.dettid.as_raw() != own.0 && !state.holds_process_files() {
                return None;
            }
        }
        Self::placeholder_of(guest, link.fd).map(|state| state.directory)
    }

    /// The placeholder directory that an open of `observed` (the guest's
    /// absolute spelling) names, whose procfs spelling is `spelling`
    /// (`Detcore::procfs_spelling_at`), if Detcore serves it: one of the
    /// caller's own descriptor directories ([`fd_directory::classify`]),
    /// `/proc` itself by any spelling that reaches it ([`names_procfs_root`]),
    /// `/dev/fd` where it is the symbolic link to `/proc/self/fd` it is on
    /// Linux, or a descriptor link of the caller's to a placeholder. The last
    /// two are symbolic links: an open with `O_NOFOLLOW` that does not follow
    /// them anyway (a trailing `/`) opens the link itself (`O_PATH`) or fails
    /// with `ELOOP`, which the kernel answers.
    pub(super) async fn descriptor_directory_opened<G: Guest<Self>>(
        &self,
        guest: &mut G,
        observed: &Path,
        spelling: &Path,
        flags: OFlag,
    ) -> Option<Result<OpenedDirectory, Errno>> {
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let own = Self::descriptor_directory_caller(guest)?;
        // A spelling through a descriptor link to a placeholder directory
        // (`/proc/self/fd/D/.`, `/proc/self/fd/D/N`) names what Linux
        // resolves it to, with the links it followed on the way counted.
        let (decided, links) = match Self::decided_path(guest, observed) {
            Ok(decided) => decided,
            Err(errno) => return Some(Err(errno)),
        };
        let (observed, spelling) = if decided.as_path() == observed {
            (observed, spelling)
        } else {
            (decided.as_path(), decided.as_path())
        };
        if let Some(table) = fd_directory::classify(spelling, own) {
            return Some(Ok(OpenedDirectory::Own(table)));
        }
        if names_procfs_root(spelling) {
            return Some(Ok(OpenedDirectory::Root));
        }
        if super::opens_the_link_itself(observed, flags) {
            return None;
        }
        if let Some(directory) = Self::own_link_directory(guest, observed) {
            // The open follows this last link too, within the one budget.
            if links + selector_links(observed) + 1 > LINK_LIMIT {
                return Some(Err(Errno::ELOOP));
            }
            return Some(Ok(OpenedDirectory::Reopened(directory)));
        }
        let dev_fd = observed
            .components()
            .filter(|component| *component != std::path::Component::CurDir)
            .eq(Path::new("/dev/fd").components());
        if dev_fd
            && self
                .injected_link_target(guest, b"/dev/fd")
                .await
                .as_deref()
                == Some(Path::new("/proc/self/fd"))
        {
            return Some(Ok(OpenedDirectory::Own(DescriptorDirectoryTable::Process)));
        }
        None
    }

    /// The device that keys the inode numbers of the `/proc` directories
    /// Detcore serves, and their entries', with their paths: the device of
    /// the guest's procfs as the guest's own stat of it reports it, never the
    /// placeholder's, whose device the host chose. Where the backend's `/proc`
    /// beneath is the guest's kernel procfs
    /// (`procfs_inode::tool_can_name_guest_descriptors`: ptrace), the tool's
    /// own view of it (`rendered_procfs::proc_device`), which is the
    /// guest's, as the tool runs in the guest's mount namespace; elsewhere
    /// (KVM) the device that backend reports for its procfs files
    /// (`procfs_inode::KVM_SYNTHETIC_PROC_DEVICE`). Either way the device the
    /// descriptor on `/proc` itself reported before slice P1, so the numbers
    /// are the ones they were.
    pub(crate) fn procfs_key_device<G: Guest<Self>>(guest: &G) -> u64 {
        if procfs_inode::tool_can_name_guest_descriptors(&guest.config().backend) {
            super::rendered_procfs::proc_device().unwrap_or(0)
        } else {
            procfs_inode::KVM_SYNTHETIC_PROC_DEVICE
        }
    }

    /// The target of the symbolic link at the absolute guest path `link`, by
    /// one injected `readlinkat`, or `None`.
    async fn injected_link_target<G: Guest<Self>>(
        &self,
        guest: &mut G,
        link: &[u8],
    ) -> Option<PathBuf> {
        const LINK_CAPACITY: usize = 32;
        const TARGET_CAPACITY: usize = 64;
        let mut stack = guest.stack().await;
        let link = stack.push(super::rendered_procfs::c_string::<LINK_CAPACITY>(link));
        let target = stack.reserve::<[u8; TARGET_CAPACITY]>();
        let stack_guard = stack.commit().ok()?;
        let length = guest
            .inject(
                syscalls::Readlinkat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(link.as_raw() as *const libc::c_char))
                    .with_buf(Some(target.cast::<libc::c_char>()))
                    .with_buf_len(TARGET_CAPACITY),
            )
            .await;
        let bytes = match length.map(|length| length as usize) {
            Ok(length) if length < TARGET_CAPACITY => {
                let mut bytes = vec![0; length];
                guest
                    .memory()
                    .read_exact(target.cast::<u8>(), &mut bytes)
                    .ok()
                    .map(|()| bytes)
            }
            _ => None,
        };
        drop(stack_guard);
        Some(PathBuf::from(String::from_utf8(bytes?).ok()?))
    }

    /// What Detcore's process table says now of the task `binding` names,
    /// and whether that is still the task the directory was bound to (it
    /// exists and no other task has taken its numbers since).
    async fn bound_task<G: Guest<Self>>(
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
    ) -> (crate::procfs::listing::ProcfsTaskAnswer, bool) {
        let thread = (binding.table == DescriptorDirectoryTable::Thread).then_some(binding.tid);
        let answer = crate::tool_global::procfs_task(
            guest,
            crate::procfs::BoundTask {
                process: binding.pid,
                thread,
            },
        )
        .await;
        let current = answer.exists
            && answer.process_incarnation == binding.process_incarnation
            && answer.thread_incarnation == binding.thread_incarnation;
        (answer, current)
    }

    /// Whether the placeholder directory `directory` still names a directory:
    /// `/proc` always does; a descriptor directory while its task does
    /// (`get_proc_task`, [`Self::bound_task`]).
    async fn placeholder_current<G: Guest<Self>>(
        guest: &mut G,
        directory: &PlaceholderDirectory,
    ) -> bool {
        match directory {
            PlaceholderDirectory::Root => true,
            PlaceholderDirectory::Descriptors(binding) => Self::bound_task(guest, binding).await.1,
        }
    }

    /// Open the placeholder directory `opened` names, with the guest's open
    /// `flags`: refuse what Linux refuses for a directory, then inject the
    /// open of the placeholder that stands for it, record the descriptor and
    /// which directory it is, and register it for its tag.
    pub(super) async fn open_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        flags: OFlag,
        opened: OpenedDirectory,
    ) -> Result<i64, Error> {
        // As Linux refuses an open of a directory: invalid combinations
        // first (`build_open_flags`), then an existing name with
        // `O_CREAT|O_EXCL` (EEXIST), an `O_TMPFILE` below a directory that has
        // no `tmpfile` (EOPNOTSUPP), `O_CREAT` (`do_open`'s EISDIR), a write
        // or `O_TRUNC` (`may_open`'s EISDIR), and last `O_DIRECT`, which procfs
        // cannot do (`do_dentry_open`'s EINVAL). Measured on Linux 7.1.
        if let Some(errno) = super::rendered_procfs::open_flags_refused(flags) {
            return Err(errno.into());
        }
        let path_only = flags.contains(OFlag::O_PATH);
        if !path_only {
            if flags.contains(OFlag::O_CREAT | OFlag::O_EXCL) {
                return Err(Errno::EEXIST.into());
            }
            if flags.contains(OFlag::O_TMPFILE) {
                return Err(Errno::EOPNOTSUPP.into());
            }
            if flags.contains(OFlag::O_CREAT)
                || flags & OFlag::O_ACCMODE != OFlag::O_RDONLY
                || flags.contains(OFlag::O_TRUNC)
            {
                return Err(Errno::EISDIR.into());
            }
            if flags.contains(OFlag::O_DIRECT) {
                return Err(Errno::EINVAL.into());
            }
        }
        let (pid, tid) = Self::descriptor_directory_caller(guest).ok_or(Errno::ENOENT)?;
        let directory = match opened {
            OpenedDirectory::Own(table) => {
                let thread = (table == DescriptorDirectoryTable::Thread).then_some(tid);
                let answer = crate::tool_global::procfs_task(
                    guest,
                    crate::procfs::BoundTask {
                        process: pid,
                        thread,
                    },
                )
                .await;
                PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
                    table,
                    pid,
                    tid,
                    process_incarnation: answer.process_incarnation,
                    thread_incarnation: answer.thread_incarnation,
                    device: Self::procfs_key_device(guest),
                    files: (table == DescriptorDirectoryTable::Thread)
                        .then(|| guest.thread_state().files_id()),
                })
            }
            OpenedDirectory::Root => PlaceholderDirectory::Root,
            OpenedDirectory::Reopened(directory) => directory,
        };
        let placeholder = guest
            .config()
            .directory_placeholder
            .clone()
            .ok_or(Errno::ENOENT)?;
        let cloexec = flags & OFlag::O_CLOEXEC;
        let fd = {
            const PLACEHOLDER_CAPACITY: usize = 64;
            let bytes = placeholder.as_os_str().as_bytes();
            if bytes.len() >= PLACEHOLDER_CAPACITY {
                return Err(Errno::ENAMETOOLONG.into());
            }
            let mut stack = guest.stack().await;
            let path = stack.push(super::rendered_procfs::c_string::<PLACEHOLDER_CAPACITY>(
                bytes,
            ));
            let stack_guard = stack.commit()?;
            let opened = guest
                .inject(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(path.as_raw() as *const libc::c_char))
                        .with_flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | cloexec)
                        .with_mode(None),
                )
                .await;
            drop(stack_guard);
            opened? as RawFd
        };
        // The status flags Linux keeps: an `O_PATH` open keeps only
        // `O_PATH_FLAGS` (`build_open_how`); any other gets `O_LARGEFILE`
        // (`force_o_largefile`) and loses the open-time-only flags
        // (`do_dentry_open`).
        let status = if path_only {
            flags & (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC)
        } else {
            // The kernel's `O_LARGEFILE`, which the C library's value (0 on
            // x86_64) does not name.
            (flags & !(OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOCTTY | OFlag::O_TRUNC))
                | OFlag::from_bits_retain(0o100000)
        };
        let stat = match self
            .add_fd_with_stat(guest, fd, status, FdType::Regular)
            .await
        {
            Ok(Some(stat)) => Ok(stat),
            Ok(None) => self.inject_fstat(guest, fd).await,
            Err(errno) => Err(errno),
        };
        let stat = match stat {
            Ok(stat) => stat,
            Err(errno) => {
                guest.thread_state().remove_fd(fd);
                let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
                return Err(errno.into());
            }
        };
        PLACEHOLDER_IDENTITIES
            .lock()
            .unwrap()
            .insert((stat.st_dev, stat.st_ino));
        OPENED_ANY.store(true, Ordering::SeqCst);
        let tag = crate::tool_global::register_placeholder(guest, directory).await;
        // The kernel position tags the descriptor for whoever meets it without
        // its record. A backend that cannot seek it leaves it untagged.
        let tag = match tag {
            Some(tag) => guest
                .inject(
                    syscalls::Lseek::new()
                        .with_fd(fd)
                        .with_offset(tag as i64)
                        .with_whence(syscalls::Whence::SEEK_SET),
                )
                .await
                .ok()
                .filter(|position| *position as u64 == tag)
                .map(|_| tag),
            None => None,
        };
        let canonical = directory.canonical();
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(canonical.clone());
            detfd.set_placeholder(PlaceholderState {
                directory,
                tag,
                position: 0,
                path_only,
            });
        })?;
        tracing::debug!(
            "[detcore] descriptor {fd}: {} served on the placeholder directory {}",
            canonical.display(),
            placeholder.display()
        );
        Ok(fd as i64)
    }

    /// Make descriptor `fd`, which Detcore just recorded without knowing what
    /// it is (received over `SCM_RIGHTS`, `Detcore::record_received_descriptors`),
    /// a placeholder directory if it is one: its identity (`stat`) is the
    /// placeholder's, which no other object has, and its kernel position,
    /// read by one injected `lseek`, is a placeholder's tag, which says which
    /// directory it stands for. No guest descriptor of that identity is ever
    /// seeked by the guest ([`Self::seek_would_place_a_tag`]). Until a
    /// placeholder is opened in this tool process, whose global tool
    /// registers every tag, no received descriptor can be one, and nothing is
    /// injected to look.
    pub(crate) async fn recognize_received_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        stat: &libc::stat,
        status_flags: i32,
    ) {
        if stat.st_mode & libc::S_IFMT != libc::S_IFDIR
            || !OPENED_ANY.load(Ordering::SeqCst)
            || !self.serves_descriptor_directories(guest)
        {
            return;
        }
        if !PLACEHOLDER_IDENTITIES
            .lock()
            .unwrap()
            .contains(&(stat.st_dev, stat.st_ino))
        {
            return;
        }
        let position = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await;
        let Ok(position) = position.map(|position| position as u64) else {
            return;
        };
        if fd_directory::tag_index(position).is_none() {
            return;
        }
        let Some(directory) = crate::tool_global::placeholder(guest, position).await else {
            return;
        };
        let _ = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(directory.canonical());
            detfd.set_placeholder(PlaceholderState {
                directory,
                tag: Some(position),
                position: 0,
                path_only: status_flags & libc::O_PATH != 0,
            })
        });
    }

    /// Whether the guest's `lseek(fd, offset, whence)` of a descriptor that
    /// Detcore does not answer itself would move the kernel position of a
    /// placeholder descriptor into the tags' range, where it would pass for
    /// another placeholder's ([`Self::recognize_received_placeholder`]).
    /// `handle_lseek` refuses that seek with `EINVAL`. Every placeholder
    /// descriptor Detcore knows is answered from its model and never
    /// reaches here; one it does not know (a receive it could not
    /// recognize) is told by its identity, recorded or by one injected
    /// `fstat`: only the placeholder has it, and no guest path reaches the
    /// placeholder. Decided from the target position first, from the call
    /// alone (a negative or zero `SEEK_CUR` step cannot reach the range from
    /// a position below it, and a tagged one is in it already). Not under
    /// record and replay, nor where Detcore serves no placeholder.
    pub(crate) async fn seek_would_place_a_tag<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        offset: i64,
        whence: i32,
    ) -> bool {
        if !OPENED_ANY.load(Ordering::SeqCst) || !self.serves_descriptor_directories(guest) {
            return false;
        }
        let in_range = |target: i64| {
            u64::try_from(target)
                .ok()
                .and_then(fd_directory::tag_index)
                .is_some()
        };
        match whence {
            libc::SEEK_SET | libc::SEEK_END if !in_range(offset) => return false,
            libc::SEEK_SET | libc::SEEK_END => {}
            libc::SEEK_CUR if offset <= 0 => return false,
            libc::SEEK_CUR => {}
            _ => return false,
        }
        let recorded = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.stat())
            .ok()
            .flatten()
            .map(|stat| (stat.mode, stat.dev, stat.inode));
        let identity = match recorded {
            Some(identity) => identity,
            None => match self.inject_fstat(guest, fd).await {
                Ok(stat) => (stat.st_mode, stat.st_dev, stat.st_ino),
                // Not a descriptor: the kernel's own EBADF.
                Err(Errno::EBADF) => return false,
                // A descriptor that cannot be told is refused (fail closed).
                Err(_) => return true,
            },
        };
        identity.0 & libc::S_IFMT == libc::S_IFDIR
            && PLACEHOLDER_IDENTITIES
                .lock()
                .unwrap()
                .contains(&(identity.1, identity.2))
    }

    /// The descriptors of the table a descriptor directory bound to `binding`
    /// lists now, ascending, with the table's capacity, read by the caller:
    /// `Ok(None)` for a process whose thread group leader has exited (Linux
    /// has released its `files`), `ENOENT` for a task that is gone
    /// (`get_proc_task` finds none). The caller's own table (the process's
    /// for its process, or a thread's for a thread whose table it holds) comes
    /// from Detcore's model unless a receive may have installed descriptors
    /// the model cannot name; that, and another process's or thread's table,
    /// is listed by the kernel beneath the backend.
    async fn descriptor_directory_table<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
    ) -> Result<Option<(Vec<i32>, u64)>, Errno> {
        let caller = Self::descriptor_directory_caller(guest).ok_or(Errno::EACCES)?;
        let (answer, current) = Self::bound_task(guest, binding).await;
        if !current {
            return Err(Errno::ENOENT);
        }
        if binding.table == DescriptorDirectoryTable::Process && answer.without_mm {
            return Ok(None);
        }
        let own = {
            let state = guest.thread_state();
            match binding.table {
                DescriptorDirectoryTable::Process => {
                    caller.0 == binding.pid
                        && (state.dettid.as_raw() == caller.0 || state.holds_process_files())
                }
                DescriptorDirectoryTable::Thread => {
                    caller.1 == binding.tid || binding.files == Some(state.files_id())
                }
            }
        };
        if own {
            let (numbers, capacity, unaccounted) = guest.thread_state().descriptor_table();
            if !unaccounted {
                return Ok(Some((numbers, capacity)));
            }
            let path = match binding.table {
                DescriptorDirectoryTable::Process => b"/proc/self/fd".to_vec(),
                DescriptorDirectoryTable::Thread => b"/proc/thread-self/fd".to_vec(),
            };
            let listed = self.kernel_descriptor_numbers(guest, &path, true).await?;
            let capacity = capacity.max(fd_directory::capacity(&listed));
            return Ok(Some((listed, capacity)));
        }
        // Another table: the descriptor opened to read it is in the caller's,
        // and a descriptor of the same number in the one listed stays listed
        // (a fork's child listing the directory it inherited). (A process
        // that shares its table with another process, `CLONE_FILES` without
        // `CLONE_THREAD`, and lists that process's directory sees that
        // descriptor too: named.)
        let path = binding.canonical().into_os_string().into_encoded_bytes();
        let listed = self.kernel_descriptor_numbers(guest, &path, false).await?;
        let capacity = fd_directory::capacity(&listed);
        Ok(Some((listed, capacity)))
    }

    /// A descriptor table as the kernel beneath the backend lists the
    /// descriptor directory at the absolute guest `path`: one injected open,
    /// `getdents64` until the end into a buffer on Reverie's guest stack, and a
    /// close, leaving out the descriptor this opens to read it where the
    /// table listed is the caller's own (`own`), which holds it.
    async fn kernel_descriptor_numbers<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &[u8],
        own: bool,
    ) -> Result<Vec<i32>, Errno> {
        const PATH_CAPACITY: usize = 64;
        const BUFFER: usize = 512;
        if path.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG);
        }
        let mut stack = guest.stack().await;
        let spelled = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(path));
        let buffer = stack.reserve::<[u8; BUFFER]>();
        let stack_guard = stack.commit()?;
        let opened = guest
            .inject(
                syscalls::Openat::new()
                    .with_dirfd(libc::AT_FDCWD)
                    .with_path(PathPtr::from_ptr(spelled.as_raw() as *const libc::c_char))
                    .with_flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
                    .with_mode(None),
            )
            .await;
        let mut numbers = Vec::new();
        let mut result = opened.map(|fd| fd as RawFd);
        if let Ok(fd) = result {
            loop {
                let read = guest
                    .inject(
                        syscalls::Getdents64::new()
                            .with_fd(fd as u32)
                            .with_dirent(Some(buffer.cast()))
                            .with_count(BUFFER as u32),
                    )
                    .await;
                let length = match read {
                    Ok(0) => break,
                    Ok(length) => length as usize,
                    Err(errno) => {
                        result = Err(errno);
                        break;
                    }
                };
                let mut bytes = vec![0; length];
                if let Err(errno) = guest.memory().read_exact(buffer.cast::<u8>(), &mut bytes) {
                    result = Err(errno);
                    break;
                }
                numbers.extend(crate::util::descriptor_names(&bytes));
            }
            let _ = guest.inject(syscalls::Close::new().with_fd(fd)).await;
        }
        drop(stack_guard);
        let fd = result?;
        if own {
            numbers.retain(|number| *number != fd);
        }
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// The inode number of the directory a binding names itself (its `.`,
    /// and its `fstat` and `stat`), from Detcore's pool by the directory's
    /// path on the device of the guest's `/proc`: the key the entry `fd` of
    /// the process's directory is listed with (`modeled_directory_snapshot`),
    /// so the two agree.
    async fn directory_inode<G: Guest<Self>>(
        &self,
        guest: &mut G,
        device: u64,
        within: &Path,
        mark: crate::tool_global::SnapshotMark,
    ) -> u64 {
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within.to_path_buf())));
        let (host, name) = inodes.entry_inode(b".", procfs_inode::path_inode(device, within).ino);
        crate::tool_global::determinize_listed_inode(guest, host, name, mark)
            .await
            .0
            .as_raw()
    }

    /// Answer a `getdents` or `getdents64` of the descriptor directory open as
    /// `fd`, whose state is `state`, into the guest's buffer `buf` of
    /// `capacity` bytes, as `proc_readfd_common` answers it: the entries from
    /// the position on that fit, of the table as it is now, each with the
    /// inode number Detcore's pool gives its path, and the position moved
    /// past the entries copied (see [`fd_directory`]).
    async fn serve_descriptor_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        buf: Option<AddrMut<'_, u8>>,
        capacity: usize,
        format: DirentFormat,
    ) -> Result<i64, Error> {
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let PlaceholderDirectory::Descriptors(binding) = state.directory else {
            return Err(Errno::EBADF.into());
        };
        let table = self.descriptor_directory_table(guest, &binding).await?;
        let (entries, end) = fd_directory::entries_from(
            state.position,
            table
                .as_ref()
                .map(|(numbers, capacity)| (numbers.as_slice(), *capacity)),
        );
        let save = |guest: &mut G, position: u64| {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(current) = detfd.placeholder() {
                    detfd.set_placeholder(PlaceholderState {
                        position,
                        ..current
                    });
                }
            });
        };
        if entries.is_empty() {
            save(guest, end);
            return Ok(0);
        }
        let count = fd_directory::fitting(&entries, capacity, |name| format.record_len(name));
        let Some(buf) = buf.filter(|_| count > 0) else {
            save(guest, entries[0].position);
            return Err(if count == 0 {
                Errno::EINVAL
            } else {
                Errno::EFAULT
            }
            .into());
        };
        let offsets = fd_directory::offsets(&entries, count, end);
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let within = binding.within();
        let inodes =
            DirectoryInodes::new(binding.device, Some(ProcfsDirectory::new(within.clone())));
        let mut records = Vec::new();
        let mut names = Vec::with_capacity(count);
        for (entry, offset) in entries.iter().zip(&offsets) {
            let ino = if entry.name == b"." {
                self.directory_inode(guest, binding.device, &within, mark)
                    .await
            } else {
                let entry_path = match entry.name.as_slice() {
                    b".." => within.parent().unwrap_or(&within).to_path_buf(),
                    name => within.join(std::ffi::OsStr::from_bytes(name)),
                };
                let key = procfs_inode::path_inode(binding.device, &entry_path).ino;
                let (host, name) = inodes.entry_inode(&entry.name, key);
                crate::tool_global::determinize_listed_inode(guest, host, name, mark)
                    .await
                    .0
                    .as_raw()
            };
            let record = DirEntry {
                name: entry.name.clone(),
                ino: 0,
                off: *offset,
                ty: entry.ty,
            };
            format.encode(&record, ino, *offset, &mut records);
            names.push(entry.name.len());
        }
        let (passed, copied) = super::copy_records(
            &mut guest.memory(),
            buf,
            format,
            &records,
            &names,
            entries[0].position,
        );
        save(guest, fd_directory::position_after(&entries, passed, end));
        Ok(copied.map(|length| length as i64)?)
    }

    /// The entries of `/proc` as Detcore's procfs model lists them
    /// (`procfs::listing`, for the run's `--proc` level and the processes the
    /// scheduler lists), and how to key their inode numbers, for a `/proc`
    /// placeholder's directory stream: what `modeled_directory_snapshot`
    /// gives a descriptor on the kernel's `/proc`, but with every entry
    /// looked up by its absolute spelling (the placeholder holds none), no
    /// kernel cookies (the stream's positions are the guest's), and keyed by
    /// the procfs key device ([`Self::procfs_key_device`]). On the kernel's
    /// procfs an entry the lookup does not find is left out unless Detcore
    /// renders it, as there; on a backend's own `/proc` (KVM), whose lookups
    /// follow its executor at host-timed moments, a lookup decides no entry,
    /// and an entry it does not answer is numbered by its path's key.
    async fn root_placeholder_snapshot<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<(Vec<DirEntry>, DirectoryInodes, DirectoryOrder), Error> {
        use crate::procfs::listing;
        use crate::procfs::listing::ModeledDirectory;
        let directory = ModeledDirectory::System(Vec::new());
        let facts = crate::tool_global::procfs_listing(guest, None).await;
        let names = listing::entries(&directory, self.cfg.proc_level, &facts).unwrap_or_default();
        let device = Self::procfs_key_device(guest);
        let proc = Path::new("/proc");
        let spellings = names
            .iter()
            .map(|(name, _)| proc.join(OsStr::from_bytes(name)))
            .collect::<Vec<_>>();
        let looked_up = self
            .looked_up_inodes(guest, libc::AT_FDCWD, &spellings)
            .await?;
        let backend_procfs = !procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend);
        let within = listing::within(&directory);
        let session_files = self.rendered_files(guest);
        let entries = names
            .into_iter()
            .zip(looked_up)
            .filter_map(|((name, ty), looked_up)| {
                let entry = match name.as_slice() {
                    b"." => within.clone(),
                    b".." => within.parent().unwrap_or(&within).to_path_buf(),
                    name => within.join(OsStr::from_bytes(name)),
                };
                // A file Detcore renders exists whatever the kernel has. (Not
                // a task's `comm` where the kernel serves every one.)
                let rendered = crate::procfs::ProcfsFile::from_path(
                    &proc.join(entry.strip_prefix("/").unwrap_or(&entry)),
                )
                .is_some_and(|file| {
                    !self.kernel_serves_every_comm(&file)
                        && file.rendered_path(session_files).is_some()
                });
                let ino = match looked_up {
                    Ok(ino) => ino,
                    Err(Errno::ENOENT)
                        if name != b"." && name != b".." && !rendered && !backend_procfs =>
                    {
                        return None;
                    }
                    Err(_) => procfs_inode::path_inode(device, &entry).ino,
                };
                Some(DirEntry {
                    ino,
                    off: 0,
                    ty,
                    name,
                })
            })
            .collect::<Vec<_>>();
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within)));
        Ok((entries, inodes, DirectoryOrder::ProcfsIds))
    }

    /// Answer a `getdents` or `getdents64` of the `/proc` placeholder open as
    /// `fd` from its directory stream, shared by the description's aliases:
    /// the stream's snapshot of [`Self::root_placeholder_snapshot`], taken at
    /// the first read and at the first after a seek to 0, as for every
    /// directory Detcore lists (`DirectoryStream`). The kernel position stays
    /// at the tag (`Detcore::move_directory_kernel_position` moves no
    /// placeholder). A count that does not fit an `int` fits nothing: `EINVAL`,
    /// or 0 at the end.
    async fn serve_root_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        fd: RawFd,
        state: PlaceholderState,
    ) -> Result<i64, Error> {
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let (buf, capacity, format) = match call {
            Syscall::Getdents(c) => (
                c.dirent().map(|dirent| dirent.cast::<u8>()),
                c.count() as usize,
                DirentFormat::Legacy,
            ),
            Syscall::Getdents64(c) => (
                c.dirent().map(|dirent| dirent.cast::<u8>()),
                c.count() as usize,
                DirentFormat::Dirent64,
            ),
            _ => return Err(Errno::ENOSYS.into()),
        };
        let lock = guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.directory_lock())?;
        let _serving = lock.lock().await;
        if guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.directory_needs_snapshot())?
        {
            let mark = crate::tool_global::directory_snapshot_mark(guest).await;
            let (entries, inodes, order) = self.root_placeholder_snapshot(guest).await?;
            let mut entries = Some(entries);
            guest.thread_state().with_detfd(fd, |detfd| {
                detfd.install_directory_snapshot(
                    entries.take().unwrap_or_default(),
                    mark,
                    inodes.clone(),
                    order,
                )
            })?;
        }
        let fits = i32::try_from(capacity).is_ok();
        let batch = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.with_directory_stream(|stream| {
                stream.next_batch(format, if fits { capacity } else { 0 })
            })
        })??;
        match batch {
            Ok(batch) if batch.is_empty() => return Ok(0),
            Ok(_) if !fits => return Err(Errno::EINVAL.into()),
            Ok(_) => {}
            Err(errno) => return Err(errno.into()),
        }
        let Some(buf) = buf else {
            return Err(Errno::EFAULT.into());
        };
        let (sysno, _) = call.into_parts();
        let empty = Syscall::from_raw(
            sysno,
            super::procfs_levels::raw_args([fd as usize, 0, 0, 0, 0, 0]),
        );
        self.serve_next_batch(
            guest,
            GetdentsCall {
                call: *call,
                empty,
                fd,
                buf,
                capacity,
                format,
            },
        )
        .await
    }

    /// The metadata of a descriptor directory bound to `binding`, as Linux's
    /// `proc_fd_getattr` reports it: a directory of mode 0500
    /// (`S_IRUSR|S_IXUSR`, `tgid_base_stuff`), 2 links
    /// (`proc_pident_instantiate`), its task's owner (`task_dump_owner`, as for
    /// the files Detcore renders), procfs's 1024-byte blocks and device, the
    /// inode number Detcore's pool gives its path, and a size that is the
    /// number of open descriptors (`proc_readfd_count`).
    async fn descriptor_directory_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
        binding: &DescriptorDirectoryBinding,
    ) -> Result<DetStat, Error> {
        let open = match self.descriptor_directory_table(guest, binding).await {
            Ok(table) => table.map_or(0, |(numbers, _)| numbers.len()),
            Err(Errno::ENOENT) => return Err(Errno::ENOENT.into()),
            Err(_) => 0,
        };
        let owner_path = match binding.table {
            DescriptorDirectoryTable::Process => {
                PathBuf::from(format!("/proc/{}/stat", binding.pid))
            }
            DescriptorDirectoryTable::Thread => {
                PathBuf::from(format!("/proc/{}/task/{}/stat", binding.pid, binding.tid))
            }
        };
        let owner = self.rendered_file_owner(guest, &owner_path).await?;
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let ino = self
            .directory_inode(guest, binding.device, &binding.within(), mark)
            .await;
        let epoch = super::rendered_procfs::epoch_time(guest.config().epoch);
        Ok(DetStat {
            mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
            mode: libc::S_IFDIR | 0o500,
            uid: owner.0,
            gid: owner.1,
            attributes: StatxAttributes::empty(),
            attributes_mask: StatxAttributes::STATX_ATTR_AUTOMOUNT
                | StatxAttributes::STATX_ATTR_MOUNT_ROOT
                | StatxAttributes::STATX_ATTR_DAX,
            dev: procfs_inode::VIRTUAL_PROC_DEVICE,
            rdev: 0,
            inode: ino,
            nlink: 2,
            size: open as i64,
            blksize: procfs_inode::PROC_BLOCK_SIZE,
            blocks: 0,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
        })
    }

    /// The metadata of `/proc` itself, as Linux's `proc_root_getattr`
    /// reports it with Detcore's rules for procfs: a directory with the mode
    /// and owner the guest's procfs root has (0555, global root's, which the
    /// guest's user namespace does not map), one link (Detcore's number for
    /// procfs's root, whose own counts the host's processes), size 0,
    /// procfs's 1024-byte blocks and device, the mount root attribute, the
    /// epoch, and the inode number its `.` entry gets in a listing of it
    /// ([`Self::root_placeholder_snapshot`]): the number the kernel's procfs
    /// gives its root (1) where the backend's `/proc` is the kernel's, else
    /// its path's key.
    async fn root_placeholder_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
    ) -> Result<DetStat, Error> {
        let (mode, uid, gid) = super::rendered_procfs::guest_procfs_root()?;
        let device = Self::procfs_key_device(guest);
        let within = PathBuf::from("/");
        let raw = if procfs_inode::tool_can_name_guest_descriptors(&self.cfg.backend) {
            procfs_inode::PROC_ROOT_INO
        } else {
            procfs_inode::path_inode(device, &within).ino
        };
        let inodes = DirectoryInodes::new(device, Some(ProcfsDirectory::new(within)));
        let (host, name) = inodes.entry_inode(b".", raw);
        let mark = crate::tool_global::directory_snapshot_mark(guest).await;
        let ino = crate::tool_global::determinize_listed_inode(guest, host, name, mark)
            .await
            .0
            .as_raw();
        let epoch = super::rendered_procfs::epoch_time(guest.config().epoch);
        Ok(DetStat {
            mask: StatxMask::STATX_BASIC_STATS | StatxMask::STATX_MNT_ID,
            mode: libc::S_IFDIR | (mode & 0o7777),
            uid,
            gid,
            attributes: StatxAttributes::STATX_ATTR_MOUNT_ROOT,
            attributes_mask: StatxAttributes::STATX_ATTR_AUTOMOUNT
                | StatxAttributes::STATX_ATTR_MOUNT_ROOT
                | StatxAttributes::STATX_ATTR_DAX,
            dev: procfs_inode::VIRTUAL_PROC_DEVICE,
            rdev: 0,
            inode: ino,
            nlink: 1,
            size: 0,
            blksize: procfs_inode::PROC_BLOCK_SIZE,
            blocks: 0,
            atime: epoch,
            btime: epoch,
            ctime: epoch,
            mtime: epoch,
        })
    }

    /// The metadata of a placeholder directory: `/proc`'s, or a descriptor
    /// directory's.
    pub(super) async fn placeholder_metadata<G: Guest<Self>>(
        &self,
        guest: &mut G,
        directory: &PlaceholderDirectory,
    ) -> Result<DetStat, Error> {
        match directory {
            PlaceholderDirectory::Root => self.root_placeholder_metadata(guest).await,
            PlaceholderDirectory::Descriptors(binding) => {
                self.descriptor_directory_metadata(guest, binding).await
            }
        }
    }

    /// The fdinfo Linux gives placeholder descriptor `fd`, as the kernel's
    /// fdinfo for it would need to be (it describes the placeholder: its tag
    /// as the position, its tmpfs mount and inode): Detcore's position, the
    /// guest's procfs mount and inode 0, which the snapshot's sanitizer
    /// replaces with the directory's inode number (`procfs::sanitize_fdinfo`),
    /// returned beside it, as the placeholder's `fstat` numbers it. `None`
    /// for any other descriptor.
    pub(super) async fn placeholder_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
    ) -> Result<Option<(Vec<u8>, u64)>, Error> {
        let Some(state) = Self::placeholder_of(guest, fd) else {
            return Ok(None);
        };
        let position = match state.directory {
            PlaceholderDirectory::Root => guest
                .thread_state()
                .with_detfd(fd, |detfd| detfd.directory_stream_position())?,
            PlaceholderDirectory::Descriptors(_) => state.position,
        };
        let contents = format!(
            "pos:\t{position}\nflags:\t0\nmnt_id:\t{}\nino:\t0\n",
            super::rendered_procfs::guest_procfs_mount_id()?
        )
        .into_bytes();
        let inode = self
            .placeholder_metadata(guest, &state.directory)
            .await?
            .inode;
        Ok(Some((contents, inode)))
    }

    /// The metadata of the placeholder directory a stat names, if Detcore
    /// serves it: a descriptor that is one, a path that spells `/proc` or one
    /// of the caller's own descriptor directories, or, followed, a descriptor
    /// link of the caller's to a placeholder (`/proc/self/fd/D`; not
    /// followed, that is the link, which the kernel describes).
    pub(super) async fn placeholder_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: StatTarget<'_>,
    ) -> Result<Option<DetStat>, Error> {
        if !self.serves_descriptor_directories(guest) {
            return Ok(None);
        }
        let directory = match target {
            StatTarget::Descriptor(fd) => match Self::placeholder_of(guest, fd) {
                Some(state) => state.directory,
                None => return Ok(None),
            },
            StatTarget::Path {
                dirfd,
                path: Some(pointer),
                follow,
            } => {
                let path = pointer.read(&guest.memory())?;
                if path.as_os_str().is_empty() {
                    return Ok(None);
                }
                let spelling = self.procfs_spelling_at(guest, dirfd, &path).await?;
                let (spelling, links) = Self::decided_path(guest, &spelling)?;
                let Some((pid, tid)) = Self::descriptor_directory_caller(guest) else {
                    return Ok(None);
                };
                match fd_directory::classify(&spelling, (pid, tid)) {
                    Some(table) => {
                        let thread = (table == DescriptorDirectoryTable::Thread).then_some(tid);
                        let answer = crate::tool_global::procfs_task(
                            guest,
                            crate::procfs::BoundTask {
                                process: pid,
                                thread,
                            },
                        )
                        .await;
                        PlaceholderDirectory::Descriptors(DescriptorDirectoryBinding {
                            table,
                            pid,
                            tid,
                            process_incarnation: answer.process_incarnation,
                            thread_incarnation: answer.thread_incarnation,
                            device: Self::procfs_key_device(guest),
                            files: (table == DescriptorDirectoryTable::Thread)
                                .then(|| guest.thread_state().files_id()),
                        })
                    }
                    None if names_procfs_root(&spelling) => PlaceholderDirectory::Root,
                    None if follow => match Self::own_link_directory(guest, &spelling) {
                        // Following this last link too, within the one budget.
                        Some(_) if links + selector_links(&spelling) + 1 > LINK_LIMIT => {
                            return Err(Errno::ELOOP.into());
                        }
                        Some(directory) => directory,
                        None => return Ok(None),
                    },
                    None => return Ok(None),
                }
            }
            StatTarget::Path { path: None, .. } | StatTarget::Cwd | StatTarget::Overwritten => {
                return Ok(None);
            }
        };
        Ok(Some(self.placeholder_metadata(guest, &directory).await?))
    }

    /// Answer `call` if it operates on a placeholder descriptor
    /// ([`placeholder_operand`]), from Detcore's model, so that nothing
    /// through the descriptor reaches the placeholder's filesystem (a
    /// read-only tmpfs, whose answers are not procfs's). The calls, and their
    /// answers, as Linux answers them for procfs's root and for a descriptor
    /// directory (v6.17; measured on Linux 7.1):
    /// - `getdents`, `getdents64`: the listing (above); `lseek`:
    ///   `generic_file_llseek` on an empty directory (`fd_directory::seek`),
    ///   on the listing's position;
    /// - `fchdir`: an injected `chdir` to the directory's own path, which the
    ///   backend's own `/proc` resolves (a stale descriptor directory first
    ///   fails with `ENOENT`);
    /// - `fcntl`: `F_GETFL` and `F_SETFL` from the description's flags; the
    ///   table commands (`F_DUPFD`, `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`,
    ///   `F_DUPFD_QUERY`), the owner and signal commands and `F_CREATED_QUERY`
    ///   reach the descriptor, which every file system answers alike; the
    ///   record locks are refused by name (`ENOLCK`: named residual); a
    ///   lease is `EACCES` on global root's `/proc` and `EINVAL` on a task's
    ///   directory; the seals, `F_GET_SEALS` `EINVAL` and `F_ADD_SEALS`
    ///   `EPERM` on a read-only description; the pipe sizes `EBADF`;
    ///   `F_NOTIFY` succeeds (no event comes); the write hints `0` and
    ///   `EPERM` on `/proc`, its owner's on a task's directory (kept no
    ///   further: named); any other command `EINVAL`;
    /// - `read` and its relatives: `EISDIR` (`generic_read_dir`), after a
    ///   negative position's `EINVAL`; `write` and its relatives: `EBADF`, the
    ///   description cannot write, after a negative position's `EINVAL`;
    /// - `ioctl`: `FIOCLEX`, `FIONCLEX`, `FIONBIO` and `FIOASYNC` reach the
    ///   descriptor, answered alike everywhere; `FIGETBSZ` procfs's 1024;
    ///   `FIOQSIZE` 0; `FIFREEZE`, `FITHAW` and `FS_IOC_FIEMAP` `EOPNOTSUPP`;
    ///   `FICLONE`, `FICLONERANGE` and `FIDEDUPERANGE` `EISDIR` (or the
    ///   source's `EBADF`); every other, `FS_IOC_GETFLAGS` among them,
    ///   `ENOTTY`;
    /// - `fsync`, `fdatasync`: `EINVAL` (procfs has no `fsync`); `syncfs`: 0;
    /// - `flock`: each directory its own lock (`procfs::locks::LockLedger::
    ///   placeholder_flock`), as each is its own inode on Linux;
    /// - `fchmod` and the path-less `fchmodat2`: `EPERM`; `fchown` and the
    ///   path-less `fchownat`: 0, Detcore's virtual root's answer
    ///   (`handle_ownership_change_noop`) with no change to report;
    /// - the `f*xattr` calls: procfs's answers (no attribute handlers);
    /// - `ftruncate`: `EINVAL`; `fallocate`: `vfs_fallocate`'s checks, then
    ///   `EBADF`; `fadvise64`: `generic_fadvise` on a filesystem with no
    ///   backing device; `readahead`: `EINVAL`; `sync_file_range`: its argument
    ///   checks, then 0;
    /// - `fstatfs`: procfs's (`write_procfs_statfs`);
    /// - a file-backed `mmap`: as for a procfs file without `mmap`
    ///   (`answer_rendered_procfs_mapping`);
    /// - the path-less `utimensat`: `EPERM` with times or `EACCES` without on
    ///   global root's `/proc`, 0 on the caller's own directory;
    ///   `faccessat2`: `EACCES` for write access to `/proc`, else 0;
    ///   `name_to_handle_at`: `EOPNOTSUPP`, procfs exports no handles.
    ///
    /// Every call Linux refuses on an `O_PATH` descriptor fails with `EBADF`
    /// first (`check_fcntl_cmd` for `fcntl`). `None` for every other call.
    pub(crate) async fn answer_placeholder_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = (*call).into_parts();
        // Calls with two descriptor operands, either a placeholder.
        if matches!(
            sysno,
            Sysno::sendfile | Sysno::splice | Sysno::tee | Sysno::copy_file_range
        ) {
            return self.answer_placeholder_transfer(guest, sysno, &args).await;
        }
        let fd_position = placeholder_operand(&guest.memory(), sysno, &args)?;
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let fd = argument(&args, fd_position) as i32;
        let state = Self::placeholder_of(guest, fd)?;
        // `flock` ignores `LOCK_MAND` before it looks at the descriptor.
        if sysno == Sysno::flock && args.arg1 as i32 & LOCK_MAND != 0 {
            return Some(Ok(0));
        }
        // The calls that take an `O_PATH` descriptor (`fdget_raw`, or a
        // path lookup from it) and those that check their arguments first
        // decide `EBADF` themselves.
        let path_only_refused = state.path_only
            && !matches!(
                sysno,
                Sysno::fchdir
                    | Sysno::fcntl
                    | Sysno::fstatfs
                    | Sysno::flock
                    | Sysno::utimensat
                    | Sysno::fchmodat2
                    | Sysno::fchownat
                    | Sysno::faccessat2
                    | Sysno::name_to_handle_at
            );
        if path_only_refused {
            return Some(Err(Errno::EBADF.into()));
        }
        Some(match call {
            Syscall::Getdents(_) | Syscall::Getdents64(_)
                if state.directory == PlaceholderDirectory::Root =>
            {
                self.serve_root_placeholder(guest, call, fd, state).await
            }
            Syscall::Getdents(c) => {
                self.serve_descriptor_directory(
                    guest,
                    fd,
                    state,
                    c.dirent().map(|dirent| dirent.cast::<u8>()),
                    c.count() as usize,
                    DirentFormat::Legacy,
                )
                .await
            }
            Syscall::Getdents64(c) => {
                self.serve_descriptor_directory(
                    guest,
                    fd,
                    state,
                    c.dirent().map(|dirent| dirent.cast::<u8>()),
                    c.count() as usize,
                    DirentFormat::Dirent64,
                )
                .await
            }
            Syscall::Lseek(c) => {
                self.seek_placeholder(guest, fd, state, c.offset(), args.arg2 as i32)
            }
            Syscall::Fchdir(_) => self.enter_placeholder(guest, &state.directory).await,
            Syscall::Fcntl(c) => match self.placeholder_fcntl(guest, fd, state, c).await {
                Some(answer) => answer,
                None => return None,
            },
            Syscall::Read(_)
            | Syscall::Readv(_)
            | Syscall::Pread64(_)
            | Syscall::Preadv(_)
            | Syscall::Preadv2(_) => read_answer(&guest.memory(), sysno, &args).map_err(Into::into),
            Syscall::Pwrite64(_) | Syscall::Pwritev(_) | Syscall::Pwritev2(_) => {
                let position = args.arg3 as i64;
                if position < 0 && !(sysno == Sysno::pwritev2 && position == -1) {
                    Err(Errno::EINVAL.into())
                } else {
                    Err(Errno::EBADF.into())
                }
            }
            Syscall::Write(_) | Syscall::Writev(_) => Err(Errno::EBADF.into()),
            Syscall::Ioctl(_) => match self
                .placeholder_ioctl(guest, args.arg1 as u32, args.arg2)
                .await
            {
                Some(answer) => answer.map_err(Into::into),
                None => return None,
            },
            Syscall::Fsync(_) | Syscall::Fdatasync(_) => Err(Errno::EINVAL.into()),
            Syscall::Flock(c) => {
                self.placeholder_flock(guest, fd, state, c.operation())
                    .await
            }
            Syscall::Fchmod(_) => Err(Errno::EPERM.into()),
            Syscall::Fchown(_) => Ok(0),
            Syscall::Fgetxattr(_)
            | Syscall::Fsetxattr(_)
            | Syscall::Flistxattr(_)
            | Syscall::Fremovexattr(_) => {
                self.placeholder_xattr(guest, call, &state.directory).await
            }
            Syscall::Ftruncate(_) => Err(Errno::EINVAL.into()),
            Syscall::Fallocate(_) => {
                Err(fallocate_answer(args.arg1 as i32, args.arg2 as i64, args.arg3 as i64).into())
            }
            Syscall::Readahead(_) => Err(Errno::EINVAL.into()),
            Syscall::Fstatfs(c) => self.write_procfs_statfs(guest, c.buf()).map(|()| 0),
            Syscall::Mmap(_) => return None,
            _ => match sysno {
                Sysno::syncfs => Ok(0),
                Sysno::fadvise64 => {
                    fadvise_answer(args.arg2 as i64, args.arg3 as i32).map_err(Into::into)
                }
                Sysno::sync_file_range => {
                    sync_file_range_answer(args.arg1 as i64, args.arg2 as i64, args.arg3 as u32)
                        .map_err(Into::into)
                }
                Sysno::utimensat => {
                    self.placeholder_utimensat(
                        guest,
                        state,
                        args.arg1 == 0,
                        args.arg2,
                        args.arg3 as i32,
                    )
                    .await
                }
                // `do_fchmodat`'s flags, then procfs's answer to a mode change.
                Sysno::fchmodat2
                    if args.arg3 as i32 & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH)
                        != 0 =>
                {
                    Err(Errno::EINVAL.into())
                }
                Sysno::fchmodat2 => Err(Errno::EPERM.into()),
                // `do_fchownat`'s flags, then Detcore's virtual root's answer.
                Sysno::fchownat
                    if args.arg4 as i32 & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH)
                        != 0 =>
                {
                    Err(Errno::EINVAL.into())
                }
                Sysno::fchownat => Ok(0),
                Sysno::faccessat2 => self
                    .placeholder_access(&state.directory, args.arg2 as i32, args.arg3 as i32)
                    .map_err(Into::into),
                // Its flags, then procfs, which exports no handles.
                Sysno::name_to_handle_at => {
                    const AT_HANDLE_MNT_ID_UNIQUE: i32 = 0x001;
                    const AT_HANDLE_CONNECTABLE: i32 = 0x002;
                    const AT_HANDLE_FID: i32 = 0x200;
                    let flags = args.arg4 as i32;
                    if flags
                        & !(libc::AT_SYMLINK_FOLLOW
                            | libc::AT_EMPTY_PATH
                            | AT_HANDLE_FID
                            | AT_HANDLE_MNT_ID_UNIQUE
                            | AT_HANDLE_CONNECTABLE)
                        != 0
                        || flags & AT_HANDLE_CONNECTABLE != 0
                    {
                        Err(Errno::EINVAL.into())
                    } else {
                        Err(Errno::EOPNOTSUPP.into())
                    }
                }
                _ => return None,
            },
        })
    }

    /// `lseek` of a placeholder descriptor, which is `generic_file_llseek` on
    /// an empty directory ([`fd_directory::seek`]) on both: a descriptor
    /// directory's position, and `/proc`'s, the position of its directory
    /// stream, whose snapshot a seek to 0 drops, as `rewinddir` refreshes
    /// one (`DirectoryStream::seek`).
    fn seek_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        offset: i64,
        whence: i32,
    ) -> Result<i64, Error> {
        match state.directory {
            PlaceholderDirectory::Descriptors(_) => {
                let position = fd_directory::seek(state.position, offset, whence)?;
                guest.thread_state().with_detfd(fd, |detfd| {
                    detfd.set_placeholder(PlaceholderState { position, ..state })
                })?;
                Ok(position as i64)
            }
            PlaceholderDirectory::Root => {
                let current = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.directory_stream_position())?;
                let position = fd_directory::seek(current, offset, whence)?;
                guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.seek_directory_stream(position))?;
                Ok(position as i64)
            }
        }
    }

    /// `fchdir` to a placeholder: an injected `chdir` to the directory's own
    /// path, which Linux's `fchdir` enters (`/proc`, or the descriptor
    /// directory's `/proc/<pid>/fd`), so the working directory, `getcwd`,
    /// `/proc/self/cwd` and every name looked up from the working directory
    /// are the backend's own `/proc`'s, as they are after a `chdir` to that
    /// path. A descriptor directory whose task is gone is `ENOENT`, as its
    /// lookups are.
    async fn enter_placeholder<G: Guest<Self>>(
        &self,
        guest: &mut G,
        directory: &PlaceholderDirectory,
    ) -> Result<i64, Error> {
        const PATH_CAPACITY: usize = 64;
        if !Self::placeholder_current(guest, directory).await {
            return Err(Errno::ENOENT.into());
        }
        let canonical = directory.canonical();
        let bytes = canonical.as_os_str().as_bytes();
        if bytes.len() >= PATH_CAPACITY {
            return Err(Errno::ENAMETOOLONG.into());
        }
        let mut stack = guest.stack().await;
        let path = stack.push(super::rendered_procfs::c_string::<PATH_CAPACITY>(bytes));
        let stack_guard = stack.commit()?;
        let entered = guest
            .inject_with_retry(
                syscalls::Chdir::new()
                    .with_path(PathPtr::from_ptr(path.as_raw() as *const libc::c_char)),
            )
            .await;
        drop(stack_guard);
        Ok(entered?)
    }

    /// `fcntl` through a placeholder descriptor (see
    /// [`Self::answer_placeholder_call`]), or `None` for a command that
    /// reaches the descriptor.
    async fn placeholder_fcntl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        call: &syscalls::Fcntl,
    ) -> Option<Result<i64, Error>> {
        let root = state.directory == PlaceholderDirectory::Root;
        Some(match call.cmd() {
            FcntlCmd::F_GETFL => {
                let status = guest
                    .thread_state()
                    .with_detfd(fd, |detfd| detfd.status_flags())
                    .unwrap_or(libc::O_DIRECTORY);
                Ok(i64::from(status & !libc::O_CLOEXEC))
            }
            // The table commands, which the kernel's descriptor table
            // answers for every file.
            FcntlCmd::F_DUPFD(_)
            | FcntlCmd::F_DUPFD_CLOEXEC(_)
            | FcntlCmd::F_GETFD
            | FcntlCmd::F_SETFD(_) => return None,
            // `check_fcntl_cmd`: no other command is allowed on `O_PATH`.
            _ if state.path_only => Err(Errno::EBADF.into()),
            FcntlCmd::F_SETFL(requested) => {
                // `setfl` changes only `SETFL_MASK`; procfs has no direct
                // I/O (`EINVAL`).
                const SETFL_MASK: i32 = libc::O_APPEND
                    | libc::O_NONBLOCK
                    | libc::O_ASYNC
                    | libc::O_DIRECT
                    | libc::O_NOATIME;
                if requested & libc::O_DIRECT != 0 {
                    Err(Errno::EINVAL.into())
                } else {
                    let _ = guest.thread_state().with_detfd(fd, |detfd| {
                        let status = detfd.status_flags();
                        detfd.set_status_flags((status & !SETFL_MASK) | (requested & SETFL_MASK));
                    });
                    Ok(0)
                }
            }
            _ => {
                let (_, args) = Syscall::Fcntl(*call).into_parts();
                let command = args.arg1 as libc::c_int;
                match command {
                    // The owner, signal and query commands, which act on the
                    // description alike for every file.
                    libc::F_SETOWN
                    | libc::F_GETOWN
                    | F_SETSIG
                    | F_GETSIG
                    | F_SETOWN_EX
                    | F_GETOWN_EX
                    | F_GETOWNER_UIDS
                    | F_DUPFD_QUERY
                    | F_CREATED_QUERY => return None,
                    // Record locks on the one placeholder inode would conflict
                    // across directories, where Linux's inodes are each the
                    // directory's own: refused by name (named residual).
                    libc::F_GETLK
                    | libc::F_SETLK
                    | libc::F_SETLKW
                    | libc::F_OFD_GETLK
                    | libc::F_OFD_SETLK
                    | libc::F_OFD_SETLKW => {
                        warn!(
                            "[detcore] fcntl({fd}, {command}), a record lock on a /proc \
                             directory Detcore serves, is refused: ENOLCK"
                        );
                        self.refuse_unserviceable_operation(guest, Sysno::fcntl, Errno::ENOLCK)
                            .await
                    }
                    // A lease needs the file's owner (`vfs_setlease`), global
                    // root for `/proc`, then a regular file.
                    libc::F_SETLEASE if root => Err(Errno::EACCES.into()),
                    libc::F_SETLEASE => Err(Errno::EINVAL.into()),
                    libc::F_GETLEASE => Ok(i64::from(libc::F_UNLCK)),
                    // Adding seals needs a writable descriptor, before Linux
                    // looks for seals the file cannot have.
                    libc::F_ADD_SEALS => Err(Errno::EPERM.into()),
                    libc::F_GET_SEALS => Err(Errno::EINVAL.into()),
                    // Not a pipe (`get_pipe_info`).
                    libc::F_SETPIPE_SZ | libc::F_GETPIPE_SZ => Err(Errno::EBADF.into()),
                    // dnotify takes the directory; procfs never reports an
                    // event on it.
                    libc::F_NOTIFY => Ok(0),
                    // The inode's write hint: none set, and only the owner
                    // may set one (`fcntl_rw_hint`).
                    F_GET_RW_HINT => match AddrMut::<u64>::from_raw(args.arg2) {
                        Some(hint) => guest
                            .memory()
                            .write_value(hint, &0)
                            .map(|()| 0)
                            .map_err(Into::into),
                        None => Err(Errno::EFAULT.into()),
                    },
                    F_SET_RW_HINT if root => Err(Errno::EPERM.into()),
                    F_SET_RW_HINT => {
                        match reverie::syscalls::Addr::<u64>::from_raw(args.arg2)
                            .map(|hint| guest.memory().read_value(hint))
                        {
                            Some(Ok(hint)) if hint <= 5 => Ok(0),
                            Some(Ok(_)) => Err(Errno::EINVAL.into()),
                            _ => Err(Errno::EFAULT.into()),
                        }
                    }
                    _ => Err(Errno::EINVAL.into()),
                }
            }
        })
    }

    /// `ioctl` through a placeholder descriptor (see
    /// [`Self::answer_placeholder_call`]), or `None` for a command that
    /// reaches the descriptor.
    async fn placeholder_ioctl<G: Guest<Self>>(
        &self,
        guest: &mut G,
        command: u32,
        argument: usize,
    ) -> Option<Result<i64, Errno>> {
        Some(match command as usize {
            // FIONCLEX, FIOCLEX, FIOASYNC and FIONBIO, which the kernel
            // answers alike for every directory, on the description.
            0x5450..=0x5452 | 0x5421 => return None,
            // FIGETBSZ: procfs's block size.
            2 => match AddrMut::<libc::c_int>::from_raw(argument) {
                Some(address) => guest
                    .memory()
                    .write_value(address, &(procfs_inode::PROC_BLOCK_SIZE as libc::c_int))
                    .map(|()| 0),
                None => Err(Errno::EFAULT),
            },
            // FIOQSIZE: the bytes the directory uses, none.
            0x5460 => match AddrMut::<libc::loff_t>::from_raw(argument) {
                Some(address) => guest.memory().write_value(address, &0).map(|()| 0),
                None => Err(Errno::EFAULT),
            },
            // FIFREEZE and FITHAW: procfs cannot freeze; FS_IOC_FIEMAP:
            // procfs maps no extents.
            0xc004_5877 | 0xc004_5878 | 0xc020_660b => Err(Errno::EOPNOTSUPP),
            // FICLONE: the source descriptor first, then a directory
            // destination.
            0x4004_9409 => Self::placeholder_clone_answer(guest, argument as i32),
            // FICLONERANGE: the whole `struct file_clone_range` is read
            // first.
            0x4020_940d => match reverie::syscalls::Addr::<[u64; 4]>::from_raw(argument)
                .map(|address| guest.memory().read_value(address))
            {
                Some(Ok(range)) => Self::placeholder_clone_answer(guest, range[0] as i32),
                _ => Err(Errno::EFAULT),
            },
            // FIDEDUPERANGE: the source, this directory, is checked first.
            0xc018_9436 => Err(Errno::EISDIR),
            // Every other command, FS_IOC_GETFLAGS and FS_IOC_FSGETXATTR
            // among them: procfs has no `fileattr_get` and directories no
            // `unlocked_ioctl`.
            _ => Err(Errno::ENOTTY),
        })
    }

    /// Linux's answer to cloning descriptor `source` into a directory:
    /// `EBADF` for no such descriptor, else `EISDIR`
    /// (`generic_file_rw_checks`).
    fn placeholder_clone_answer<G: Guest<Self>>(guest: &G, source: RawFd) -> Result<i64, Errno> {
        if source < 0
            || !guest
                .thread_state()
                .with_detfd(source, |detfd| detfd.status_flags() & libc::O_PATH == 0)
                .unwrap_or(false)
        {
            Err(Errno::EBADF)
        } else {
            Err(Errno::EISDIR)
        }
    }

    /// `flock` through a placeholder descriptor (see
    /// [`Self::answer_placeholder_call`]): each directory is locked apart
    /// (`procfs::locks::LockLedger::placeholder_flock`), the description
    /// owning the lock as on Linux. A contended blocking request is refused
    /// as `Detcore::handle_flock` refuses one.
    async fn placeholder_flock<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        state: PlaceholderState,
        operation: i32,
    ) -> Result<i64, Error> {
        use crate::procfs::locks::FlockMode;
        use crate::procfs::locks::PlaceholderFlockOutcome;
        let mode = match operation & !libc::LOCK_NB {
            libc::LOCK_SH => Some(FlockMode::Shared),
            libc::LOCK_EX => Some(FlockMode::Exclusive),
            libc::LOCK_UN => None,
            _ => return Err(Errno::EINVAL.into()),
        };
        // An `O_PATH` descriptor, after the command (`flock_translate_cmd`).
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let open_file = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_lock_tracked();
            detfd.open_file_id()
        })?;
        let slots = guest.thread_state().slots_naming(open_file);
        let outcome = crate::tool_global::placeholder_flock(
            guest,
            crate::procfs::locks::PlaceholderFlock {
                key: state.directory.lock_key(),
                open_file,
                slots,
                mode,
                nonblocking: operation & libc::LOCK_NB != 0,
            },
        )
        .await;
        match outcome {
            PlaceholderFlockOutcome::Done => Ok(0),
            PlaceholderFlockOutcome::WouldBlock => Err(Errno::EWOULDBLOCK.into()),
            PlaceholderFlockOutcome::Contended => {
                let dettid = guest.thread_state().dettid;
                super::report_flock_refusal(&format!(
                    "[dtid {dettid}] blocking flock(fd={fd}, operation={operation:#x}) of a /proc \
                     directory Detcore serves refused: another open file holds a conflicting \
                     lock. {}",
                    super::FLOCK_REFUSAL_ADVICE
                ));
                self.refuse_unserviceable_operation(guest, Sysno::flock, Errno::ENOLCK)
                    .await
            }
        }
    }

    /// The extended-attribute calls through a placeholder descriptor:
    /// procfs's answers (`rendered_procfs::placeholder_xattr_answer`), as
    /// for a file global root owns on `/proc`, and for one the caller owns
    /// on its descriptor directory.
    async fn placeholder_xattr<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
        directory: &PlaceholderDirectory,
    ) -> Result<i64, Error> {
        let owned = match directory {
            PlaceholderDirectory::Root => None,
            PlaceholderDirectory::Descriptors(binding) => Some(binding.pid),
        };
        self.placeholder_xattr_answer(guest, call, owned).await
    }

    /// The `utimensat` of a placeholder itself, with no path (`futimens`) or
    /// an empty one and `AT_EMPTY_PATH`, in Linux's order (`utimensat`,
    /// `do_utimes_fd` or `do_utimes_path`, `vfs_utimes`, `notify_change`):
    /// both times omitted is 0 before anything else is looked at; then the
    /// flags (`futimens` takes none), an `O_PATH` descriptor without a path
    /// (`EBADF`), the times' nanoseconds (`EINVAL`); then, on global root's
    /// `/proc`, explicit times need its owner (`EPERM`, `setattr_prepare`)
    /// and the current time write access (`EACCES`, `may_setattr`); the
    /// caller's own descriptor directory is its own (0, the times kept no
    /// further: named).
    async fn placeholder_utimensat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        state: PlaceholderState,
        futimens: bool,
        times: usize,
        flags: i32,
    ) -> Result<i64, Error> {
        const UTIME_NOW: i64 = (1 << 30) - 1;
        const UTIME_OMIT: i64 = (1 << 30) - 2;
        let times = match reverie::syscalls::Addr::<[libc::timespec; 2]>::from_raw(times) {
            None => None,
            Some(address) => Some(guest.memory().read_value(address)?),
        };
        if times.is_some_and(|times| times.iter().all(|time| time.tv_nsec == UTIME_OMIT)) {
            return Ok(0);
        }
        if futimens && flags != 0
            || !futimens && flags & !(libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0
        {
            return Err(Errno::EINVAL.into());
        }
        if futimens && state.path_only {
            return Err(Errno::EBADF.into());
        }
        let valid = |nsec: i64| {
            nsec == UTIME_NOW || nsec == UTIME_OMIT || (0..1_000_000_000).contains(&nsec)
        };
        if times.is_some_and(|times| times.iter().any(|time| !valid(time.tv_nsec))) {
            return Err(Errno::EINVAL.into());
        }
        if state.directory != PlaceholderDirectory::Root {
            return Ok(0);
        }
        match times {
            Some(times) if times.iter().any(|time| time.tv_nsec != UTIME_NOW) => {
                Err(Errno::EPERM.into())
            }
            _ => Err(Errno::EACCES.into()),
        }
    }

    /// The path-less `faccessat2` of a placeholder: `/proc`'s mode (0555)
    /// grants no write, and global root's directory no override; the
    /// caller's own descriptor directory is open to it
    /// (`proc_fd_permission`).
    fn placeholder_access(
        &self,
        directory: &PlaceholderDirectory,
        mode: i32,
        flags: i32,
    ) -> Result<i64, Errno> {
        if mode & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0
            || flags & !(libc::AT_EACCESS | libc::AT_SYMLINK_NOFOLLOW | libc::AT_EMPTY_PATH) != 0
        {
            return Err(Errno::EINVAL);
        }
        if *directory == PlaceholderDirectory::Root && mode & libc::W_OK != 0 {
            return Err(Errno::EACCES);
        }
        Ok(0)
    }

    /// `sendfile`, `splice`, `tee` and `copy_file_range` with a placeholder
    /// descriptor operand: a directory cannot be read through a pipe
    /// (`EINVAL`: no `splice_read`), written (`EBADF`: the description cannot
    /// write), or copied (`EISDIR`). `None` where neither operand is one.
    async fn answer_placeholder_transfer<G: Guest<Self>>(
        &self,
        guest: &mut G,
        sysno: Sysno,
        args: &syscalls::SyscallArgs,
    ) -> Option<Result<i64, Error>> {
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let (input, output) = match sysno {
            Sysno::sendfile => (args.arg1, args.arg0),
            Sysno::splice | Sysno::copy_file_range => (args.arg0, args.arg2),
            _ => (args.arg0, args.arg1),
        };
        let input = Self::placeholder_of(guest, input as i32);
        let output = Self::placeholder_of(guest, output as i32);
        if input.is_none() && output.is_none() {
            return None;
        }
        let path_only = input.is_some_and(|state| state.path_only)
            || output.is_some_and(|state| state.path_only);
        Some(Err(if path_only {
            Errno::EBADF
        } else {
            match sysno {
                Sysno::copy_file_range if args.arg5 != 0 => Errno::EINVAL,
                Sysno::copy_file_range => Errno::EISDIR,
                Sysno::sendfile if output.is_some() => Errno::EBADF,
                Sysno::splice if output.is_some() => Errno::EBADF,
                _ => Errno::EINVAL,
            }
        }
        .into()))
    }

    /// Whether [`Self::respelled_below_descriptor_directory`] may respell
    /// anything in `call`: it looks a path up, a placeholder has been opened
    /// or recognized in this tool process (until then no path can lead below
    /// one), and Detcore serves placeholders to the caller. Asked before each
    /// injection that carries the guest's paths, so it reads nothing from the
    /// guest.
    pub(crate) fn may_respell_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        call: &Syscall,
    ) -> bool {
        let sysno = call.number();
        (!relative_operands(sysno).is_empty() || !absolute_operands(sysno).is_empty())
            && OPENED_ANY.load(Ordering::SeqCst)
            && Self::descriptor_directory_caller(guest).is_some()
    }

    /// The path operands of `call` that lead below a placeholder directory,
    /// each respelled as Linux resolves it: a path relative to a placeholder
    /// descriptor, which the placeholder would otherwise resolve, in the empty
    /// tmpfs, below the directory's absolute spelling
    /// ([`PlaceholderDirectory::lookup_base`], [`below`]); and a path through
    /// one of the caller's descriptor links to one (`/proc/self/fd/D/N`,
    /// which Linux resolves below `D`'s directory), in turn, through each
    /// such link it then leads through. The caller stages the respelled paths
    /// for one injection (`Detcore::procfs_translated_call`), before anything
    /// else runs. A stale descriptor directory (its task gone, or its numbers
    /// held by another task) fails the lookup with `ENOENT`, as
    /// `get_proc_task` does; more than `MAXSYMLINKS` such links, `ELOOP`; a
    /// respelled path longer than `PATH_MAX`, `ENAMETOOLONG`; an `openat2`
    /// whose resolution is scoped ([`SCOPED_RESOLVE`]), `EXDEV`. Empty when
    /// nothing leads below one.
    pub(crate) async fn respelled_below_descriptor_directory<G: Guest<Self>>(
        guest: &mut G,
        call: &Syscall,
    ) -> Result<Vec<RespelledOperand>, Errno> {
        let Some(caller) = Self::descriptor_directory_caller(guest) else {
            return Ok(Vec::new());
        };
        let (sysno, args) = call.into_parts();
        let read_operand = |guest: &G, pointer: usize| -> Option<Vec<u8>> {
            PathPtr::from_ptr(pointer as *const libc::c_char)
                .and_then(|pointer| pointer.read(&guest.memory()).ok())
                .map(|path: PathBuf| path.into_os_string().into_encoded_bytes())
        };
        // `openat2`'s resolution flags, in its `struct open_how`
        // (`{ flags, mode, resolve }`, at least 24 bytes).
        let resolve = if sysno == Sysno::openat2 {
            match reverie::syscalls::Addr::<[u64; 3]>::from_raw(args.arg2) {
                Some(how) if args.arg3 >= 24 => guest
                    .memory()
                    .read_value(how)
                    .map(|how| how[2])
                    .unwrap_or(0),
                _ => 0,
            }
        } else {
            0
        };
        let mut respelled = Vec::new();
        let operands = relative_operands(sysno)
            .iter()
            .map(|&(dirfd, path)| (Some(dirfd), path))
            .chain(absolute_operands(sysno).iter().map(|&path| (None, path)))
            .collect::<Vec<_>>();
        for (dirfd_index, path_index) in operands {
            let pointer = argument(&args, path_index);
            if pointer == 0 {
                continue;
            }
            let Some(path) = read_operand(guest, pointer) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            let (spelled, relative) = if path.starts_with(b"/") {
                (path, false)
            } else {
                let Some(state) = dirfd_index
                    .and_then(|index| Self::placeholder_of(guest, argument(&args, index) as i32))
                else {
                    continue;
                };
                if !Self::placeholder_current(guest, &state.directory).await {
                    return Err(Errno::ENOENT);
                }
                // With `RESOLVE_NO_SYMLINKS` the directory is spelled by
                // number, which crosses no symbolic link of its own.
                let base = if resolve & libc::RESOLVE_NO_SYMLINKS != 0 {
                    state.directory.canonical()
                } else {
                    state.directory.lookup_base(caller)
                };
                (below(&base, &path), true)
            };
            let (bytes, through_links) =
                Self::through_own_links(guest, spelled, caller, !relative).await?;
            if !relative && !through_links {
                continue;
            }
            if resolve & SCOPED_RESOLVE != 0 {
                return Err(Errno::EXDEV);
            }
            if bytes.len() >= PATH_LIMIT {
                return Err(Errno::ENAMETOOLONG);
            }
            respelled.push(RespelledOperand {
                dirfd: dirfd_index,
                path: path_index,
                bytes,
            });
        }
        Ok(respelled)
    }

    /// `path`, absolute, spelled as Linux resolves it where it leads through
    /// one of the caller's descriptor links to a placeholder directory and on
    /// below it ([`through_descriptor_link`]): the directory's absolute
    /// spelling joined with the rest, again for each such link the result
    /// leads through, and whether any did. `ENOENT` for a descriptor
    /// directory whose task is gone, `ELOOP` where the links Linux follows
    /// on the way (each descriptor link, and, where `guest_spelled` says the
    /// first one is the guest's own spelling, the selector links before it,
    /// [`selector_links`]) pass `MAXSYMLINKS`.
    async fn through_own_links<G: Guest<Self>>(
        guest: &mut G,
        mut path: Vec<u8>,
        caller: (i32, i32),
        guest_spelled: bool,
    ) -> Result<(Vec<u8>, bool), Errno> {
        let mut hops = 0;
        let mut links = 0;
        while let Some((link, rest)) = through_descriptor_link(&path) {
            let Some(directory) = Self::own_link_directory(guest, &link) else {
                break;
            };
            if hops == 0 && guest_spelled {
                links += selector_links(&link);
            }
            hops += 1;
            links += 1;
            if links > LINK_LIMIT {
                return Err(Errno::ELOOP);
            }
            if !Self::placeholder_current(guest, &directory).await {
                return Err(Errno::ENOENT);
            }
            path = below(&directory.lookup_base(caller), &rest);
        }
        Ok((path, hops > 0))
    }

    /// The absolute guest path an open of `path` relative to `dirfd`, whose
    /// guest spelling made absolute is `observed` (`Detcore::observed_path`),
    /// reaches where it leads below a placeholder directory Detcore serves: a
    /// name relative to a placeholder descriptor, or a path through the
    /// caller's descriptor links to one ([`Self::decided_path`]). `None` for
    /// any other path, which names what it spells.
    pub(super) fn path_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
        observed: &Path,
    ) -> Option<PathBuf> {
        Self::descriptor_directory_caller(guest)?;
        let relative_below = !path.is_absolute()
            && !path.as_os_str().is_empty()
            && Self::placeholder_of(guest, dirfd).is_some();
        let (decided, _) = Self::decided_path(guest, observed).ok()?;
        (relative_below || decided.as_path() != observed).then_some(decided)
    }

    /// The absolute guest `path` as Detcore decides about it (which
    /// directory an open, a `stat` or a `readlink` names): where it leads
    /// through the caller's descriptor links to placeholder directories,
    /// below each directory's canonical spelling (`/proc/<pid>/fd/rest`,
    /// `/proc/rest`), as Linux resolves it ([`Self::through_own_links`]);
    /// `path` itself otherwise; with the symbolic links Linux follows on the
    /// way (the selector links before the first descriptor link,
    /// [`selector_links`], and each descriptor link), or `ELOOP` where they
    /// pass `MAXSYMLINKS`. The canonical spellings it walks through name
    /// tasks by number, so only the guest's own spelling has selector links
    /// to count. Whether a binding is stale is left to the injection of the
    /// call, which respells the same path.
    pub(super) fn decided_path<G: Guest<Self>>(
        guest: &G,
        path: &Path,
    ) -> Result<(PathBuf, usize), Errno> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        let mut links = 0;
        let mut first = true;
        while let Some((link, rest)) = through_descriptor_link(&bytes) {
            let Some(directory) = Self::own_link_directory(guest, &link) else {
                break;
            };
            if first {
                links += selector_links(&link);
                first = false;
            }
            links += 1;
            if links > LINK_LIMIT {
                return Err(Errno::ELOOP);
            }
            bytes = below(&directory.canonical(), &rest);
        }
        Ok((PathBuf::from(std::ffi::OsString::from_vec(bytes)), links))
    }

    /// Answer a `readlink` of the absolute guest path `path` into the buffer
    /// `buf` of `buf_len` bytes if it is a descriptor link of the caller's to
    /// a placeholder directory ([`Self::own_link_directory`]): the directory's
    /// path (`/proc/<pid>/fd`, which `proc_fd_link` reads, or `/proc`),
    /// truncated to the buffer and without a NUL, as `do_readlinkat` copies
    /// it. The placeholder's own path never reaches the guest.
    pub(crate) async fn answer_descriptor_directory_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        path: &Path,
        buf: Option<AddrMut<'_, libc::c_char>>,
        buf_len: usize,
    ) -> Option<Result<i64, Error>> {
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let decided = match Self::decided_path(guest, path) {
            Ok((decided, _)) => decided,
            Err(errno) => return Some(Err(errno.into())),
        };
        let target = Self::own_link_directory(guest, &decided)?.canonical();
        let size = match crate::syscalls::namespace::readlink_size(buf_len) {
            Ok(size) => size,
            Err(errno) => return Some(Err(errno.into())),
        };
        let bytes = target.as_os_str().as_bytes();
        let length = bytes.len().min(size);
        let Some(buf) = buf else {
            return Some(Err(Errno::EFAULT.into()));
        };
        Some(
            guest
                .memory()
                .write_exact(buf.cast::<u8>(), &bytes[..length])
                .map(|()| length as i64)
                .map_err(Into::into),
        )
    }

    /// Whether the absolute guest path `observed`, followed, names a
    /// placeholder directory Detcore serves: `/proc` by any spelling, one of
    /// the caller's own descriptor directories, or a descriptor link of the
    /// caller's to a placeholder, as `statfs` reaches it.
    pub(super) fn names_placeholder_directory<G: Guest<Self>>(guest: &G, observed: &Path) -> bool {
        let Some(own) = Self::descriptor_directory_caller(guest) else {
            return false;
        };
        let Ok((decided, _)) = Self::decided_path(guest, observed) else {
            return false;
        };
        fd_directory::classify(&decided, own).is_some()
            || names_procfs_root(&decided)
            || Self::own_link_directory(guest, &decided).is_some()
    }

    /// The absolute spelling of `path` looked up from `dirfd` where `dirfd`
    /// is a placeholder descriptor ([`below`] its lookup base), for the
    /// `readlinkat` of a name relative to one (`readlinkat(proc, "self/fd/D")`),
    /// whose link Detcore answers by its absolute spelling
    /// ([`Self::answer_descriptor_directory_link`]). `None` for any other
    /// `dirfd`, or an absolute or empty `path`.
    pub(crate) fn spelling_below_placeholder<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
    ) -> Option<PathBuf> {
        let caller = Self::descriptor_directory_caller(guest)?;
        if path.is_absolute() || path.as_os_str().is_empty() {
            return None;
        }
        let state = Self::placeholder_of(guest, dirfd)?;
        Some(PathBuf::from(std::ffi::OsString::from_vec(below(
            &state.directory.lookup_base(caller),
            path.as_os_str().as_bytes(),
        ))))
    }
}

/// A read of a directory (`read`, `readv`, `pread64`, `preadv`, `preadv2`),
/// in Linux's order of checks: a negative position (`EINVAL`, where one is
/// given; `preadv2`'s -1 reads at the description's position), a count
/// past `SSIZE_MAX` (`rw_verify_area`'s `EINVAL`), the vectors
/// (`import_iovec`: more than `UIO_MAXIOV` is `EINVAL`, an unreadable array
/// `EFAULT`, a length past `SSIZE_MAX` `EINVAL`), nothing to read (0), a
/// `preadv2` flag the loop does not take (`EOPNOTSUPP`), and then
/// `generic_read_dir`'s `EISDIR`.
fn read_answer<M: MemoryAccess>(
    memory: &M,
    sysno: Sysno,
    args: &syscalls::SyscallArgs,
) -> Result<i64, Errno> {
    const UIO_MAXIOV: usize = 1024;
    const RWF_HIPRI: u64 = 1;
    match sysno {
        Sysno::read | Sysno::pread64 => {
            if sysno == Sysno::pread64 && (args.arg3 as i64) < 0 {
                return Err(Errno::EINVAL);
            }
            if isize::try_from(args.arg2).is_err() {
                return Err(Errno::EINVAL);
            }
            Err(Errno::EISDIR)
        }
        _ => {
            if sysno != Sysno::readv {
                let position = args.arg3 as i64;
                if position < 0 && !(sysno == Sysno::preadv2 && position == -1) {
                    return Err(Errno::EINVAL);
                }
            }
            let count = args.arg2;
            if count > UIO_MAXIOV {
                return Err(Errno::EINVAL);
            }
            let mut total: u64 = 0;
            for index in 0..count {
                let Some(vector) = reverie::syscalls::Addr::<[u64; 2]>::from_raw(
                    args.arg1.wrapping_add(index * 16),
                ) else {
                    return Err(Errno::EFAULT);
                };
                let [_, length] = memory.read_value(vector).map_err(|_| Errno::EFAULT)?;
                if i64::try_from(length).is_err() {
                    return Err(Errno::EINVAL);
                }
                total = total.saturating_add(length);
            }
            if total == 0 {
                return Ok(0);
            }
            if sysno == Sysno::preadv2 && args.arg5 as u64 & !RWF_HIPRI != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            Err(Errno::EISDIR)
        }
    }
}

/// `fallocate` of a directory Linux refuses in `vfs_fallocate`'s order: a
/// negative offset or a length that is not positive (`EINVAL`), an unknown
/// mode bit or a mode with a flag it does not take (`EOPNOTSUPP`), then a
/// description opened without write access (`EBADF`), as every directory's
/// is.
fn fallocate_answer(mode: i32, offset: i64, length: i64) -> Errno {
    const KEEP_SIZE: i32 = 0x01;
    const PUNCH_HOLE: i32 = 0x02;
    const COLLAPSE_RANGE: i32 = 0x08;
    const ZERO_RANGE: i32 = 0x10;
    const INSERT_RANGE: i32 = 0x20;
    const UNSHARE_RANGE: i32 = 0x40;
    const WRITE_ZEROES: i32 = 0x80;
    const MODE_MASK: i32 =
        PUNCH_HOLE | COLLAPSE_RANGE | ZERO_RANGE | INSERT_RANGE | UNSHARE_RANGE | WRITE_ZEROES;
    if offset < 0 || length <= 0 {
        return Errno::EINVAL;
    }
    if mode & !(MODE_MASK | KEEP_SIZE) != 0 {
        return Errno::EOPNOTSUPP;
    }
    match mode & MODE_MASK {
        0 | UNSHARE_RANGE | ZERO_RANGE => {}
        PUNCH_HOLE if mode & KEEP_SIZE == 0 => return Errno::EOPNOTSUPP,
        PUNCH_HOLE => {}
        COLLAPSE_RANGE | INSERT_RANGE | WRITE_ZEROES if mode & KEEP_SIZE != 0 => {
            return Errno::EOPNOTSUPP;
        }
        COLLAPSE_RANGE | INSERT_RANGE | WRITE_ZEROES => {}
        _ => return Errno::EOPNOTSUPP,
    }
    Errno::EBADF
}

/// `fadvise64` of a procfs directory (`generic_fadvise`): a negative length
/// is `EINVAL`; procfs has no backing device, so any of the six advices is
/// taken and ignored, and any other value is `EINVAL`.
fn fadvise_answer(length: i64, advice: i32) -> Result<i64, Errno> {
    if length < 0 {
        return Err(Errno::EINVAL);
    }
    match advice {
        libc::POSIX_FADV_NORMAL
        | libc::POSIX_FADV_RANDOM
        | libc::POSIX_FADV_SEQUENTIAL
        | libc::POSIX_FADV_WILLNEED
        | libc::POSIX_FADV_DONTNEED
        | libc::POSIX_FADV_NOREUSE => Ok(0),
        _ => Err(Errno::EINVAL),
    }
}

/// `sync_file_range` of a directory (`sync_file_range`): an unknown flag, a
/// negative offset or end, or an end that wraps is `EINVAL`; a directory
/// passes the file type check, and has nothing to write back.
fn sync_file_range_answer(offset: i64, bytes: i64, flags: u32) -> Result<i64, Errno> {
    const VALID_FLAGS: u32 = 1 | 2 | 4;
    if flags & !VALID_FLAGS != 0 {
        return Err(Errno::EINVAL);
    }
    let end = offset.wrapping_add(bytes);
    if offset < 0 || end < 0 || end < offset {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `F_DUPFD_QUERY` (Linux 6.10).
const F_DUPFD_QUERY: libc::c_int = 1027;
/// `F_CREATED_QUERY` (Linux 6.12).
const F_CREATED_QUERY: libc::c_int = 1028;
/// `F_GET_RW_HINT`.
const F_GET_RW_HINT: libc::c_int = 1035;
/// `F_SET_RW_HINT`.
const F_SET_RW_HINT: libc::c_int = 1036;
/// `F_GETOWNER_UIDS`.
const F_GETOWNER_UIDS: libc::c_int = 17;
/// `F_SETSIG`.
const F_SETSIG: libc::c_int = 10;
/// `F_GETSIG`.
const F_GETSIG: libc::c_int = 11;
/// `F_SETOWN_EX`.
const F_SETOWN_EX: libc::c_int = 15;
/// `F_GETOWN_EX`.
const F_GETOWN_EX: libc::c_int = 16;
/// `flock`'s `LOCK_MAND`, which Linux ignores (`SYSCALL_DEFINE2(flock)`).
const LOCK_MAND: libc::c_int = 32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_below_a_directory_keeps_what_linux_resolves() {
        let base = Path::new("/proc/self/fd");
        let spelled = |relative: &str| String::from_utf8(below(base, relative.as_bytes())).unwrap();
        assert_eq!(spelled("3"), "/proc/self/fd/3");
        assert_eq!(spelled("3/"), "/proc/self/fd/3/");
        assert_eq!(spelled("."), "/proc/self/fd/.");
        assert_eq!(spelled("./././."), "/proc/self/fd/.");
        assert_eq!(spelled(".//3//./x"), "/proc/self/fd/3/x");
        assert_eq!(spelled("3/."), "/proc/self/fd/3/");
        assert_eq!(spelled(".."), "/proc/self/fd/..");
        // A 4093-byte path of `./` repeats and a final `.`, valid on Linux,
        // stays short.
        let long = "./".repeat(2046) + ".";
        assert_eq!(long.len(), 4093);
        assert_eq!(spelled(&long), "/proc/self/fd/.");
        // Below `/proc` itself.
        let root = |relative: &str| {
            String::from_utf8(below(Path::new("/proc"), relative.as_bytes())).unwrap()
        };
        assert_eq!(root("self/status"), "/proc/self/status");
        assert_eq!(root(".."), "/proc/..");
        assert_eq!(root("."), "/proc/.");
    }

    #[test]
    fn a_path_through_a_descriptor_link_is_split_after_the_link() {
        let split = |path: &str| {
            through_descriptor_link(path.as_bytes())
                .map(|(link, rest)| (link, String::from_utf8(rest).unwrap()))
        };
        assert_eq!(
            split("/proc/self/fd/5/3"),
            Some((PathBuf::from("/proc/self/fd/5"), "3".to_string()))
        );
        assert_eq!(
            split("/dev/fd/5/3/x"),
            Some((PathBuf::from("/dev/fd/5"), "3/x".to_string()))
        );
        assert_eq!(
            split("/proc/7/task/9/fd/5/3"),
            Some((PathBuf::from("/proc/7/task/9/fd/5"), "3".to_string()))
        );
        assert_eq!(split("/proc/self/fd/5"), None);
        // A final `/` follows the link: the directory itself.
        assert_eq!(
            split("/proc/self/fd/5/"),
            Some((PathBuf::from("/proc/self/fd/5"), String::new()))
        );
        assert_eq!(split("/proc/self/status"), None);
        assert_eq!(split("/tmp/x/y/z/w"), None);
        // Empty and `.` components before the number name nothing.
        for spelling in [
            "//proc/self/fd/5/3",
            "/proc//self/fd/5/3",
            "/proc/./self/fd/5/3",
            "/proc/self/fd/./5//3",
            "/./proc/self//fd/5/3",
        ] {
            assert_eq!(
                split(spelling),
                Some((PathBuf::from("/proc/self/fd/5"), "3".to_string())),
                "{spelling}"
            );
        }
        // A `..` before the number is not recognized.
        assert_eq!(split("/proc/self/../self/fd/5/3"), None);
        assert_eq!(split("proc/self/fd/5/3"), None);
    }

    #[test]
    fn the_rest_of_a_path_through_a_link_keeps_its_bytes() {
        let path = b"/proc/self/fd/5/7/\xff\xfe/x";
        let (link, rest) = through_descriptor_link(path).unwrap();
        assert_eq!(link, PathBuf::from("/proc/self/fd/5"));
        assert_eq!(rest, b"7/\xff\xfe/x".to_vec());
        assert_eq!(
            below(Path::new("/proc/self/fd"), &rest),
            b"/proc/self/fd/7/\xff\xfe/x".to_vec()
        );
        // A link component that is not ASCII is no link.
        assert_eq!(through_descriptor_link(b"/proc/\xffself/fd/5/3"), None);
    }

    #[test]
    fn the_links_before_a_descriptor_link_are_counted() {
        assert_eq!(selector_links(Path::new("/proc/self/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/proc/thread-self/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/proc/self/task/9/fd/5")), 1);
        assert_eq!(selector_links(Path::new("/dev/fd/5")), 2);
        assert_eq!(selector_links(Path::new("/proc/7/fd/5")), 0);
        assert_eq!(selector_links(Path::new("/proc/7/task/9/fd/5")), 0);
    }

    /// `/proc` itself by every spelling Linux's walk takes there: trailing
    /// and repeated slashes, `.` components, and `..` back from the
    /// directories every procfs has below it; nothing else.
    #[test]
    fn every_spelling_of_proc_itself_is_the_root() {
        for spelling in [
            "/proc",
            "/proc/",
            "//proc",
            "/proc/.",
            "/./proc/./",
            "/proc/self/..",
            "/proc/self/fd/../..",
            "/proc/7/task/9/../../..",
            "/proc/thread-self/../../..",
            "/proc/self/task/7/fd/../../../..",
            "/../proc",
            "/proc/../proc",
        ] {
            assert!(names_procfs_root(Path::new(spelling)), "{spelling}");
        }
        for spelling in [
            "proc",
            "/",
            "/proc/self",
            "/proc/self/fd/..",
            "/proc/thread-self/..",
            "/proc/sys/..",
            "/proc/self/cwd/..",
            "/proc/self/fd/3/..",
            "/proc/self/status/..",
            "/tmp/../proc",
            "/proc/07/..",
        ] {
            assert!(!names_procfs_root(Path::new(spelling)), "{spelling}");
        }
    }

    /// `..` resolves back over the directories every procfs has, as the
    /// kernel's walk does, and not over anything else.
    #[test]
    fn a_dotdot_resolves_over_procfs_directories_only() {
        let resolved = |path: &str| {
            resolved_procfs_dotdot(Path::new(path)).map(|path| path.display().to_string())
        };
        assert_eq!(resolved("/proc/self/../fs"), Some("/proc/fs".to_string()));
        assert_eq!(
            resolved("/proc/7/fd/../../fs/nfsd"),
            Some("/proc/fs/nfsd".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/../stat"),
            Some("/proc/self/task/stat".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/fd/.."),
            Some("/proc/thread-self".to_string())
        );
        assert_eq!(
            resolved("/proc/thread-self/../../.."),
            Some("/proc".to_string())
        );
        assert_eq!(resolved("/../proc/./self/.."), Some("/proc".to_string()));
        assert_eq!(resolved("/proc/.."), Some("/".to_string()));
        assert_eq!(resolved("/proc/self/fd"), None, "no ..");
        assert_eq!(
            resolved("/proc/self/cwd/../x"),
            None,
            "a link leads elsewhere"
        );
        assert_eq!(resolved("/proc/self/status/.."), None, "a file");
        assert_eq!(resolved("/proc/sys/../fs"), None);
        assert_eq!(resolved("/tmp/../proc/fs"), None);
        assert_eq!(resolved("/proc/self/fd/3/.."), None, "a descriptor link");
    }

    #[test]
    fn every_redirected_call_has_its_operands() {
        assert_eq!(relative_operands(Sysno::fchmodat2), &[(0, 1)]);
        assert_eq!(relative_operands(Sysno::openat2), &[(0, 1)]);
        assert_eq!(relative_operands(Sysno::execveat), &[] as &[(usize, usize)]);
        assert_eq!(absolute_operands(Sysno::rename), &[0, 1]);
        assert_eq!(absolute_operands(Sysno::execve), &[] as &[usize]);
        assert_eq!(absolute_operands(Sysno::read), &[] as &[usize]);
    }

    /// The calls through a placeholder descriptor that never reach it are
    /// told by their descriptor operand; a path call only by an empty path
    /// with `AT_EMPTY_PATH`, or `futimens`'s none.
    #[test]
    fn the_answered_calls_name_their_descriptor() {
        let args = |values: [usize; 6]| syscalls::SyscallArgs {
            arg0: values[0],
            arg1: values[1],
            arg2: values[2],
            arg3: values[3],
            arg4: values[4],
            arg5: values[5],
        };
        let memory = reverie::syscalls::LocalMemory::new();
        for sysno in [
            Sysno::getdents64,
            Sysno::lseek,
            Sysno::fchdir,
            Sysno::fcntl,
            Sysno::read,
            Sysno::write,
            Sysno::ioctl,
            Sysno::fsync,
            Sysno::fdatasync,
            Sysno::syncfs,
            Sysno::flock,
            Sysno::fchmod,
            Sysno::fchown,
            Sysno::fgetxattr,
            Sysno::fsetxattr,
            Sysno::flistxattr,
            Sysno::fremovexattr,
            Sysno::ftruncate,
            Sysno::fallocate,
            Sysno::fadvise64,
            Sysno::readahead,
            Sysno::sync_file_range,
            Sysno::fstatfs,
        ] {
            assert_eq!(
                placeholder_operand(&memory, sysno, &args([3, 0, 0, 0, 0, 0])),
                Some(0),
                "{sysno}"
            );
        }
        // A file-backed mapping names its descriptor; an anonymous one does
        // not.
        assert_eq!(
            placeholder_operand(&memory, Sysno::mmap, &args([0, 4096, 1, 2, 3, 0])),
            Some(4)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::mmap,
                &args([
                    0,
                    4096,
                    1,
                    (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS) as usize,
                    3,
                    0
                ])
            ),
            None
        );
        // `futimens`.
        assert_eq!(
            placeholder_operand(&memory, Sysno::utimensat, &args([3, 0, 0, 0, 0, 0])),
            Some(0)
        );
        // A path call through the descriptor itself: an empty path with
        // `AT_EMPTY_PATH`; any name is a lookup below it.
        let empty = c"";
        let name = c"x";
        let at_empty = libc::AT_EMPTY_PATH as usize;
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchmodat2,
                &args([3, empty.as_ptr() as usize, 0o600, at_empty, 0, 0])
            ),
            Some(0)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchmodat2,
                &args([3, name.as_ptr() as usize, 0o600, at_empty, 0, 0])
            ),
            None
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::fchownat,
                &args([3, empty.as_ptr() as usize, 0, 0, at_empty, 0])
            ),
            Some(0)
        );
        assert_eq!(
            placeholder_operand(
                &memory,
                Sysno::faccessat2,
                &args([3, empty.as_ptr() as usize, 0, 0, 0, 0])
            ),
            None,
            "an empty path without AT_EMPTY_PATH names nothing"
        );
        // The descriptor-table calls are the kernel's.
        for sysno in [Sysno::close, Sysno::dup, Sysno::dup2, Sysno::dup3] {
            assert_eq!(
                placeholder_operand(&memory, sysno, &args([3, 4, 0, 0, 0, 0])),
                None
            );
        }
    }

    /// `fallocate`, `fadvise64` and `sync_file_range` of a directory, in
    /// Linux's order of checks (v6.17 `vfs_fallocate`, `generic_fadvise`,
    /// `sync_file_range`).
    #[test]
    fn the_refused_writes_of_a_directory_follow_linux_order() {
        assert_eq!(fallocate_answer(0, -1, 1), Errno::EINVAL);
        assert_eq!(fallocate_answer(0, 0, 0), Errno::EINVAL);
        assert_eq!(fallocate_answer(0x100, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x02, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x03, 0, 1), Errno::EBADF);
        assert_eq!(fallocate_answer(0x09, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0x08, 0, 1), Errno::EBADF);
        assert_eq!(fallocate_answer(0x0a, 0, 1), Errno::EOPNOTSUPP);
        assert_eq!(fallocate_answer(0, 0, 1), Errno::EBADF);
        assert_eq!(fadvise_answer(-1, 0), Err(Errno::EINVAL));
        assert_eq!(fadvise_answer(0, libc::POSIX_FADV_DONTNEED), Ok(0));
        assert_eq!(fadvise_answer(0, 99), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(0, 0, 8), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(-1, 0, 0), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(i64::MAX, 1, 0), Err(Errno::EINVAL));
        assert_eq!(sync_file_range_answer(0, 4096, 7), Ok(0));
    }
}
