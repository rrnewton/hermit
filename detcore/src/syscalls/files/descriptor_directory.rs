/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The caller's own descriptor directory, `/proc/<pid>/fd`, served by
//! Detcore from its own descriptor table (package K slice F1, toward
//! <https://github.com/rrnewton/hermit/issues/4604>). The model, and the parts
//! of Linux it follows, are in [`crate::procfs::fd_directory`].
//!
//! An open of the directory injects `openat(AT_FDCWD, "/proc", O_RDONLY |
//! O_DIRECTORY)` (with the caller's `O_CLOEXEC`): on ptrace the kernel's
//! procfs root, on KVM its executor's existing `/proc` handle. The
//! descriptor's description records the task the directory is bound to
//! (`DetFd::descriptor_directory`), and Detcore answers from its model what
//! Linux answers from the directory: `getdents` and `getdents64` from the
//! table at each call, `lseek`, `fstat` and `statx`, `fcntl`'s status flags,
//! the `O_PATH` refusals, `fchdir` (refused by name, until the directory has
//! an object of its own: slice P of the package K design), and a name looked
//! up below it, which is looked up at the absolute spelling of the directory
//! (`/proc/self/fd/N`) instead.
//!
//! The table a listing reads is Detcore's own, which the tool keeps for the
//! guest (ptrace, KVM) and F0a made complete: every descriptor it models,
//! and every one it knows is open but does not model
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
//! Linux's, so there the directory stays the kernel's, as before (a
//! capability-keyed residual, until a provider fact names the runtime's
//! descriptors).

// TODO-HUMAN-REVIEW(#4604): Review the Detcore-served descriptor directory, a
// new determinization strategy: on ptrace its listing replaces the kernel's.

use std::collections::BTreeSet;
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

use super::StatTarget;
use crate::dirents::DirEntry;
use crate::dirents::DirentFormat;
use crate::fd::DescriptorDirectoryState;
use crate::fd::FdType;
use crate::procfs::fd_directory;
use crate::procfs::fd_directory::DescriptorDirectoryBinding;
use crate::procfs::fd_directory::DescriptorDirectoryTable;
use crate::procfs_inode;
use crate::procfs_inode::DirectoryInodes;
use crate::procfs_inode::ProcfsDirectory;
use crate::record_or_replay::RecordOrReplay;
use crate::stat::DetStat;
use crate::stat::StatxAttributes;
use crate::tool_local::Detcore;

/// The identities `(dev, ino)` of the descriptors on `/proc` this tool
/// process opened for descriptor directories: procfs's root, as the guest's
/// `/proc` has it. No guest descriptor on that root is ever seeked into the
/// tags' range ([`Detcore::seek_would_place_a_tag`]), whether or not a
/// descriptor directory was opened first, so a tag proves a descriptor
/// directory.
static ANCHOR_IDENTITIES: Mutex<BTreeSet<(u64, u64)>> = Mutex::new(BTreeSet::new());

/// Whether this tool process has opened a descriptor directory: until it
/// has, no path can lead below one and no received descriptor can be one,
/// and nothing is read or injected to look
/// ([`Detcore::may_respell_below_descriptor_directory`],
/// [`Detcore::recognize_received_descriptor_directory`]).
static OPENED_ANY: AtomicBool = AtomicBool::new(false);

/// The descriptor directory an open names.
#[derive(Debug, Clone, Copy)]
pub(super) enum OpenedDirectory {
    /// The caller's own, by its spelling.
    Own(DescriptorDirectoryTable),
    /// The one a descriptor link of the caller's (`/proc/self/fd/N`) reaches:
    /// the directory `N` is, bound to its task.
    Reopened(DescriptorDirectoryBinding),
}

/// The calls whose path operands are each looked up relative to a directory
/// descriptor operand, as (descriptor, path) argument indices. Not `execve`
/// or `execveat` (named residual: a name below a descriptor directory
/// reaches `/proc` there, as below).
fn relative_operands(sysno: Sysno) -> &'static [(usize, usize)] {
    match sysno {
        Sysno::openat
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
/// paths, an absolute one may lead through a descriptor link to a descriptor
/// directory (`/proc/self/fd/D/N`), which
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

/// `relative`, a path looked up below a descriptor directory, below `base`,
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

/// How many symbolic links Linux follows to reach the descriptor directory
/// of the descriptor link `link` ([`through_descriptor_link`]) before the
/// link itself: `/dev/fd` (a link to `/proc/self/fd`) and then `self`, 2;
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

/// How many descriptor links to descriptor directories a respelled path may
/// lead through, as Linux's lookup follows at most `MAXSYMLINKS` links
/// (<https://github.com/torvalds/linux/blob/v6.17/include/linux/namei.h>).
const LINK_LIMIT: usize = 40;

/// One path operand of a call, respelled as Linux resolves it below a
/// descriptor directory ([`Detcore::respelled_below_descriptor_directory`]).
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

impl<T: RecordOrReplay> Detcore<T> {
    /// The caller's pid and tid as the guest numbers them, where Detcore
    /// serves the caller's descriptor directories now: where it keeps the
    /// guest's descriptor table (not the in-guest backends, above), renders
    /// procfs files (not under record and replay, nor after a guest
    /// `chroot`), and knows the caller's numbers (not in a nested PID
    /// namespace, nor before its process is known). Read from the thread's
    /// state alone, so that each injection can ask
    /// ([`Self::respelled_below_descriptor_directory`]).
    fn descriptor_directory_caller<G: Guest<Self>>(guest: &G) -> Option<(i32, i32)> {
        let config = guest.config();
        let state = guest.thread_state();
        if config.backend.tool_shares_guest_descriptor_table
            || config.recordreplay_modes
            || state.changed_root.load(Ordering::SeqCst)
            || state.nested_pid_namespace
        {
            return None;
        }
        Some((state.detpid?.as_raw(), state.dettid.as_raw()))
    }

    /// Whether Detcore serves the caller's descriptor directories now
    /// ([`Self::descriptor_directory_caller`]).
    fn serves_descriptor_directories<G: Guest<Self>>(&self, guest: &G) -> bool {
        Self::descriptor_directory_caller(guest).is_some()
    }

    /// The descriptor directory state of descriptor `fd`, if it is one.
    pub(crate) fn descriptor_directory_of<G: Guest<Self>>(
        guest: &G,
        fd: RawFd,
    ) -> Option<DescriptorDirectoryState> {
        if fd < 0 {
            return None;
        }
        guest
            .thread_state()
            .with_detfd(fd, |detfd| detfd.descriptor_directory())
            .ok()
            .flatten()
    }

    /// The descriptor directory that the caller's descriptor link `path` (the
    /// whole of it: `/proc/self/fd/N`, `/dev/fd/N`, ...) reaches, if it is
    /// one. A process's link (`/proc/self/fd/N`) reads the thread group
    /// leader's table, so a thread that holds a table of its own (cloned
    /// without `CLONE_FILES`) does not answer it from its own; a thread's
    /// link (`/proc/thread-self/fd/N`) reads the caller's.
    fn own_link_directory<G: Guest<Self>>(
        guest: &G,
        path: &Path,
    ) -> Option<DescriptorDirectoryBinding> {
        let own = Self::descriptor_directory_caller(guest)?;
        let link = crate::syscalls::namespace::own_descriptor_link(path, Some(own))?;
        if link.table == crate::syscalls::namespace::DescriptorTable::Process {
            let state = guest.thread_state();
            if state.dettid.as_raw() != own.0 && !state.holds_process_files() {
                return None;
            }
        }
        Self::descriptor_directory_of(guest, link.fd).map(|state| state.binding)
    }

    /// The descriptor directory that an open of `observed` (the guest's
    /// absolute spelling) names, whose procfs spelling is `spelling`
    /// (`Detcore::procfs_spelling_at`), if Detcore serves it: one of the
    /// caller's own ([`fd_directory::classify`]), `/dev/fd` where it is the
    /// symbolic link to `/proc/self/fd` it is on Linux, or a descriptor link
    /// of the caller's to a descriptor directory. The last two are symbolic
    /// links: an open with `O_NOFOLLOW` that does not follow them anyway (a
    /// trailing `/`) opens the link itself (`O_PATH`) or fails with `ELOOP`,
    /// which the kernel answers.
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
        // A spelling through a descriptor link to a descriptor directory
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
        if super::opens_the_link_itself(observed, flags) {
            return None;
        }
        if let Some(binding) = Self::own_link_directory(guest, observed) {
            // The open follows this last link too, within the one budget.
            if links + selector_links(observed) + 1 > LINK_LIMIT {
                return Some(Err(Errno::ELOOP));
            }
            return Some(Ok(OpenedDirectory::Reopened(binding)));
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

    /// The device of `/proc` as the guest reaches it, which keys the inode of
    /// a descriptor directory a path names (as an open's descriptor's device
    /// keys it, [`Self::directory_inode`]): the device of a descriptor
    /// directory's descriptor this tool process opened, where there is one,
    /// which needs no descriptor; else one injected open of `/proc`, `fstat`
    /// and close (a backend's own `/proc` opens where it does not stat: KVM);
    /// else, without a descriptor to spare, the tool's own view of the
    /// guest's procfs (`rendered_procfs::proc_device`), which is the guest's
    /// on ptrace (on KVM, a path stat at the descriptor limit before any open
    /// of the directory numbers it otherwise than a later open: named).
    async fn guest_proc_device<G: Guest<Self>>(&self, guest: &mut G) -> u64 {
        if let Some((device, _)) = ANCHOR_IDENTITIES.lock().unwrap().iter().next() {
            return *device;
        }
        let opened = {
            let mut stack = guest.stack().await;
            let proc = stack.push(super::rendered_procfs::c_string::<8>(b"/proc"));
            let Ok(stack_guard) = stack.commit() else {
                return super::rendered_procfs::proc_device().unwrap_or(0);
            };
            let opened = guest
                .inject(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(proc.as_raw() as *const libc::c_char))
                        .with_flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
                        .with_mode(None),
                )
                .await;
            drop(stack_guard);
            opened
        };
        let Ok(fd) = opened else {
            return super::rendered_procfs::proc_device().unwrap_or(0);
        };
        let device = self
            .inject_fstat(guest, fd as RawFd)
            .await
            .map(|stat| stat.st_dev);
        let _ = guest
            .inject(syscalls::Close::new().with_fd(fd as RawFd))
            .await;
        device.unwrap_or_else(|_| super::rendered_procfs::proc_device().unwrap_or(0))
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

    /// Open the descriptor directory `opened` names, with the guest's open
    /// `flags`: refuse what Linux refuses for a directory, then inject the
    /// open of `/proc` that stands for it, record the descriptor, bind it to
    /// its task and register it for its tag.
    pub(super) async fn open_descriptor_directory<G: Guest<Self>>(
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
        let template = match opened {
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
                DescriptorDirectoryBinding {
                    table,
                    pid,
                    tid,
                    process_incarnation: answer.process_incarnation,
                    thread_incarnation: answer.thread_incarnation,
                    device: 0,
                    ino: 0,
                    files: (table == DescriptorDirectoryTable::Thread)
                        .then(|| guest.thread_state().files_id()),
                }
            }
            OpenedDirectory::Reopened(binding) => binding,
        };
        let cloexec = flags & OFlag::O_CLOEXEC;
        let fd = {
            let mut stack = guest.stack().await;
            let proc = stack.push(super::rendered_procfs::c_string::<8>(b"/proc"));
            let stack_guard = stack.commit()?;
            let opened = guest
                .inject(
                    syscalls::Openat::new()
                        .with_dirfd(libc::AT_FDCWD)
                        .with_path(PathPtr::from_ptr(proc.as_raw() as *const libc::c_char))
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
        let binding = match opened {
            OpenedDirectory::Own(_) => DescriptorDirectoryBinding {
                device: stat.st_dev,
                ino: stat.st_ino,
                ..template
            },
            OpenedDirectory::Reopened(binding) => binding,
        };
        ANCHOR_IDENTITIES
            .lock()
            .unwrap()
            .insert((stat.st_dev, stat.st_ino));
        OPENED_ANY.store(true, Ordering::SeqCst);
        let tag = crate::tool_global::register_descriptor_directory(guest, binding).await;
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
        guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(binding.canonical());
            detfd.set_descriptor_directory(DescriptorDirectoryState {
                binding,
                tag,
                position: 0,
                path_only,
            });
        })?;
        Ok(fd as i64)
    }

    /// Make descriptor `fd`, which Detcore just recorded without knowing what
    /// it is (received over `SCM_RIGHTS`, `Detcore::record_received_descriptors`),
    /// a descriptor directory if it is one: a directory whose kernel position,
    /// read by one injected `lseek`, is a descriptor directory's tag, and
    /// whose identity (`stat`) is that directory's descriptor's. A guest's own
    /// descriptor of that identity is never seeked into the tags' range
    /// ([`Self::seek_would_place_a_tag`]), and no other has it, so anything
    /// else stays the ordinary directory it is. Until a descriptor directory
    /// is opened in this tool process, whose global tool registers every tag,
    /// no received descriptor can be one, and nothing is injected to look.
    pub(crate) async fn recognize_received_descriptor_directory<G: Guest<Self>>(
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
        if !ANCHOR_IDENTITIES
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
        let Some(binding) = crate::tool_global::descriptor_directory(guest, position).await else {
            return;
        };
        if (binding.device, binding.ino) != (stat.st_dev, stat.st_ino) {
            return;
        }
        let _ = guest.thread_state().with_detfd(fd, |detfd| {
            detfd.set_path(binding.canonical());
            detfd.set_descriptor_directory(DescriptorDirectoryState {
                binding,
                tag: Some(position),
                position: 0,
                path_only: status_flags & libc::O_PATH != 0,
            })
        });
    }

    /// Whether the guest's `lseek(fd, offset, whence)` of a descriptor that
    /// Detcore does not answer itself would move the kernel position of a
    /// descriptor on procfs's root into the tags' range, where it would pass
    /// for a descriptor directory's
    /// ([`Self::recognize_received_descriptor_directory`]). `handle_lseek`
    /// refuses that seek with `EINVAL`; Linux accepts any position up to
    /// 2^31 - 1 there, and lists nothing from one this high (named
    /// difference).
    ///
    /// Decided whether or not a descriptor directory has been opened yet,
    /// whether or not Detcore recorded the descriptor's metadata
    /// (`--no-virtualize-metadata`), and whether or not the guest's table
    /// has a free slot, for nothing is opened to decide: the target position
    /// first, from the call alone (a negative or zero `SEEK_CUR` step cannot
    /// reach the range from a position below it, which every position of
    /// procfs's root is, this guard included), then the descriptor itself
    /// ([`Self::on_procfs_root`]). A descriptor whose kind or filesystem
    /// cannot be told is refused (fail closed). Not under record and replay,
    /// where no descriptor directory is served or recognized.
    pub(crate) async fn seek_would_place_a_tag<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: RawFd,
        offset: i64,
        whence: i32,
    ) -> bool {
        if guest.config().recordreplay_modes {
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
                Err(_) => return true,
            },
        };
        if identity.0 & libc::S_IFMT != libc::S_IFDIR {
            return false;
        }
        let filesystem = Self::inject_fstatfs_type(guest, fd).await;
        if !Self::on_procfs_root(
            (identity.1, identity.2),
            filesystem,
            &ANCHOR_IDENTITIES.lock().unwrap(),
        ) {
            return false;
        }
        if whence != libc::SEEK_CUR {
            return true;
        }
        let current = guest
            .inject(
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(syscalls::Whence::SEEK_CUR),
            )
            .await;
        match current {
            Ok(current) => in_range(current.wrapping_add(offset)),
            Err(_) => false,
        }
    }

    /// Whether a directory descriptor of identity `identity` (`(dev, ino)`,
    /// as the kernel beneath the backend reports it), on a filesystem of
    /// type `filesystem` (its `fstatfs` `f_type`, or why that failed), may be
    /// on procfs's root: its identity is one this tool process's descriptor
    /// directories' descriptors have (`anchors`), or it is procfs
    /// (`PROC_SUPER_MAGIC`) at the root inode (`PROC_ROOT_INO`, 1), or its
    /// filesystem cannot be told (fail closed: KVM's executor refuses
    /// `fstatfs` of its `/proc` handle with `EACCES`).
    fn on_procfs_root(
        identity: (u64, u64),
        filesystem: Result<i64, Errno>,
        anchors: &BTreeSet<(u64, u64)>,
    ) -> bool {
        const PROC_ROOT_INO: u64 = 1;
        anchors.contains(&identity)
            || match filesystem {
                Ok(f_type) => {
                    f_type as u64 == reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as u64
                        && identity.1 == PROC_ROOT_INO
                }
                Err(_) => true,
            }
    }

    /// The filesystem type (`f_type`) of descriptor `fd`, by one injected
    /// `fstatfs` whose buffer is in Reverie's guest scratch or, where that
    /// cannot hold it, in a transient private page unmapped before the guest
    /// resumes (as `Detcore::inject_fstat`,
    /// <https://github.com/rrnewton/hermit/issues/3328>). No descriptor is
    /// opened.
    async fn inject_fstatfs_type<G: Guest<Self>>(guest: &mut G, fd: RawFd) -> Result<i64, Errno> {
        let fstatfs = |buf: AddrMut<'_, libc::statfs>| {
            syscalls::Fstatfs::new().with_fd(fd).with_buf(Some(buf))
        };
        let on_stack = {
            let mut stack = guest.stack().await;
            let buf = stack.reserve::<libc::statfs>();
            match stack.commit() {
                Ok(guard) => {
                    let result = match guest.inject_with_retry(fstatfs(buf)).await {
                        Ok(_) => guest.memory().read_value(buf).map(|statfs| statfs.f_type),
                        Err(errno) => Err(errno),
                    };
                    drop(guard);
                    Some(result)
                }
                Err(_) => None,
            }
        };
        match on_stack {
            Some(Err(Errno::EFAULT)) | None => {}
            Some(result) => return result,
        }
        let length = std::mem::size_of::<libc::statfs>();
        let page = super::procfs_levels::map_transient(guest, length)
            .await
            .ok_or(Errno::ENOMEM)?;
        let buf = page.cast::<libc::statfs>();
        let result = match guest.inject_with_retry(fstatfs(buf)).await {
            Ok(_) => guest.memory().read_value(buf).map(|statfs| statfs.f_type),
            Err(errno) => Err(errno),
        };
        super::procfs_levels::unmap_transient(guest, page, length).await;
        result
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
        state: DescriptorDirectoryState,
        buf: Option<AddrMut<'_, u8>>,
        capacity: usize,
        format: DirentFormat,
    ) -> Result<i64, Error> {
        if state.path_only {
            return Err(Errno::EBADF.into());
        }
        let binding = state.binding;
        let table = self.descriptor_directory_table(guest, &binding).await?;
        let (entries, end) = fd_directory::entries_from(
            state.position,
            table
                .as_ref()
                .map(|(numbers, capacity)| (numbers.as_slice(), *capacity)),
        );
        let save = |guest: &mut G, position: u64| {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(current) = detfd.descriptor_directory() {
                    detfd.set_descriptor_directory(DescriptorDirectoryState {
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

    /// The metadata of the descriptor directory a stat names, if Detcore
    /// serves it: a descriptor that is one, a path that spells one of the
    /// caller's own, or, followed, a descriptor link of the caller's to one
    /// (`/proc/self/fd/D`; not followed, that is the link, which the kernel
    /// describes).
    pub(super) async fn descriptor_directory_stat<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target: StatTarget<'_>,
    ) -> Result<Option<DetStat>, Error> {
        if !self.serves_descriptor_directories(guest) {
            return Ok(None);
        }
        let binding = match target {
            StatTarget::Descriptor(fd) => match Self::descriptor_directory_of(guest, fd) {
                Some(state) => state.binding,
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
                        DescriptorDirectoryBinding {
                            table,
                            pid,
                            tid,
                            process_incarnation: answer.process_incarnation,
                            thread_incarnation: answer.thread_incarnation,
                            device: self.guest_proc_device(guest).await,
                            ino: 0,
                            files: (table == DescriptorDirectoryTable::Thread)
                                .then(|| guest.thread_state().files_id()),
                        }
                    }
                    None if follow => match Self::own_link_directory(guest, &spelling) {
                        // Following this last link too, within the one budget.
                        Some(_) if links + selector_links(&spelling) + 1 > LINK_LIMIT => {
                            return Err(Errno::ELOOP.into());
                        }
                        Some(binding) => binding,
                        None => return Ok(None),
                    },
                    None => return Ok(None),
                }
            }
            StatTarget::Path { path: None, .. } | StatTarget::Cwd | StatTarget::Overwritten => {
                return Ok(None);
            }
        };
        Ok(Some(
            self.descriptor_directory_metadata(guest, &binding).await?,
        ))
    }

    /// Answer `call` if it operates on a descriptor directory as a
    /// descriptor: `getdents` and `getdents64`, `lseek`, `fchdir` (refused by
    /// name: the descriptor is on `/proc`, where the working directory would
    /// go), `fcntl`'s status flags (`F_GETFL`, `F_SETFL`), and for one opened
    /// with `O_PATH`, every call Linux refuses on such a descriptor with
    /// `EBADF` (it allows only `close`, `fchdir`, `fstat`, `fstatfs`, the
    /// `dup`s, the `*at` calls, and `fcntl`'s `F_DUPFD`, `F_DUPFD_CLOEXEC`,
    /// `F_GETFD`, `F_SETFD` and `F_GETFL`, `check_fcntl_cmd`). `None` for
    /// every other call, which the descriptor on `/proc` answers.
    pub(crate) async fn answer_descriptor_directory_call<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: &Syscall,
    ) -> Option<Result<i64, Error>> {
        let (sysno, args) = (*call).into_parts();
        let fd_position = match sysno {
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
            | Sysno::sync_file_range => 0,
            // A file-backed mapping only: an anonymous one ignores its
            // descriptor argument (`ksys_mmap_pgoff`).
            Sysno::mmap if args.arg3 as i32 & libc::MAP_ANONYMOUS == 0 => 4,
            _ => return None,
        };
        if !self.serves_descriptor_directories(guest) {
            return None;
        }
        let fd = argument(&args, fd_position) as i32;
        let state = Self::descriptor_directory_of(guest, fd)?;
        let set_position = |guest: &mut G, position: u64| {
            let _ = guest.thread_state().with_detfd(fd, |detfd| {
                if let Some(current) = detfd.descriptor_directory() {
                    detfd.set_descriptor_directory(DescriptorDirectoryState {
                        position,
                        ..current
                    });
                }
            });
        };
        Some(match call {
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
                if state.path_only {
                    return Some(Err(Errno::EBADF.into()));
                }
                match fd_directory::seek(state.position, c.offset(), args.arg2 as i32) {
                    Ok(position) => {
                        set_position(guest, position);
                        Ok(position as i64)
                    }
                    Err(errno) => Err(errno.into()),
                }
            }
            Syscall::Fchdir(_) => {
                warn!(
                    "[detcore] fchdir to descriptor {fd}, a descriptor directory Detcore serves \
                     from a descriptor on /proc, is refused: EACCES"
                );
                Err(Errno::EACCES.into())
            }
            Syscall::Fcntl(c) => match c.cmd() {
                FcntlCmd::F_GETFL => {
                    let status = guest
                        .thread_state()
                        .with_detfd(fd, |detfd| detfd.status_flags())
                        .unwrap_or(libc::O_DIRECTORY);
                    Ok(i64::from(status & !libc::O_CLOEXEC))
                }
                FcntlCmd::F_SETFL(_) if state.path_only => Err(Errno::EBADF.into()),
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
                            detfd.set_status_flags(
                                (status & !SETFL_MASK) | (requested & SETFL_MASK),
                            );
                        });
                        Ok(0)
                    }
                }
                FcntlCmd::F_DUPFD(_)
                | FcntlCmd::F_DUPFD_CLOEXEC(_)
                | FcntlCmd::F_GETFD
                | FcntlCmd::F_SETFD(_) => return None,
                _ if state.path_only => Err(Errno::EBADF.into()),
                _ => return None,
            },
            _ if state.path_only => Err(Errno::EBADF.into()),
            _ => return None,
        })
    }

    /// Whether [`Self::respelled_below_descriptor_directory`] may respell
    /// anything in `call`: it looks a path up, a descriptor directory has
    /// been opened or recognized in this tool process (until then no path
    /// can lead below one), and Detcore serves the caller's. Asked before
    /// each injection that carries the guest's paths, so it reads nothing
    /// from the guest.
    pub(crate) fn may_respell_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        call: &Syscall,
    ) -> bool {
        let sysno = call.number();
        (!relative_operands(sysno).is_empty() || !absolute_operands(sysno).is_empty())
            && OPENED_ANY.load(Ordering::SeqCst)
            && Self::descriptor_directory_caller(guest).is_some()
    }

    /// The path operands of `call` that lead below a descriptor directory,
    /// each respelled as Linux resolves it: a path relative to a descriptor
    /// directory's descriptor, which its descriptor on `/proc` would
    /// otherwise resolve below `/proc`, below the directory's absolute
    /// spelling ([`DescriptorDirectoryBinding::lookup_base`], [`below`]), and
    /// a path through one of the caller's descriptor links to one
    /// (`/proc/self/fd/D/N`, which Linux resolves below `D`'s directory), in
    /// turn, through each such link it then leads through. The caller stages
    /// the respelled paths for one injection
    /// (`Detcore::procfs_translated_call`), in a transient private page
    /// mapped for that injection and unmapped after it, before anything
    /// else runs. A stale binding (its task gone, or its numbers held by
    /// another task) fails the lookup with `ENOENT`, as `get_proc_task`
    /// does; more than `MAXSYMLINKS` such links, `ELOOP`; a respelled path
    /// longer than `PATH_MAX`, `ENAMETOOLONG`. Empty when nothing leads below
    /// one.
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
                let Some(state) = dirfd_index.and_then(|index| {
                    Self::descriptor_directory_of(guest, argument(&args, index) as i32)
                }) else {
                    continue;
                };
                if !Self::bound_task(guest, &state.binding).await.1 {
                    return Err(Errno::ENOENT);
                }
                (below(&state.binding.lookup_base(caller), &path), true)
            };
            let (bytes, through_links) =
                Self::through_own_links(guest, spelled, caller, !relative).await?;
            if !relative && !through_links {
                continue;
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
    /// one of the caller's descriptor links to a descriptor directory and on
    /// below it ([`through_descriptor_link`]): the directory's absolute
    /// spelling joined with the rest, again for each such link the result
    /// leads through, and whether any did. `ENOENT` for a directory whose
    /// task is gone, `ELOOP` where the links Linux follows on the way (each
    /// descriptor link, and, where `guest_spelled` says the first one is the
    /// guest's own spelling, the selector links before it,
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
            let Some(binding) = Self::own_link_directory(guest, &link) else {
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
            if !Self::bound_task(guest, &binding).await.1 {
                return Err(Errno::ENOENT);
            }
            path = below(&binding.lookup_base(caller), &rest);
        }
        Ok((path, hops > 0))
    }

    /// The absolute guest path an open of `path` relative to `dirfd`, whose
    /// guest spelling made absolute is `observed` (`Detcore::observed_path`),
    /// reaches where it leads below a descriptor directory Detcore serves: a
    /// name relative to a descriptor directory's descriptor, or a path through
    /// the caller's descriptor links to one ([`Self::decided_path`]). `None`
    /// for any other path, which names what it spells.
    pub(super) fn path_below_descriptor_directory<G: Guest<Self>>(
        guest: &G,
        dirfd: RawFd,
        path: &Path,
        observed: &Path,
    ) -> Option<PathBuf> {
        Self::descriptor_directory_caller(guest)?;
        let relative_below = !path.is_absolute()
            && !path.as_os_str().is_empty()
            && Self::descriptor_directory_of(guest, dirfd).is_some();
        let (decided, _) = Self::decided_path(guest, observed).ok()?;
        (relative_below || decided.as_path() != observed).then_some(decided)
    }

    /// The absolute guest `path` as Detcore decides about it (which
    /// directory an open, a `stat` or a `readlink` names): where it leads
    /// through the caller's descriptor links to descriptor directories,
    /// below each directory's canonical spelling (`/proc/<pid>/fd/rest`), as
    /// Linux resolves it ([`Self::through_own_links`]); `path` itself
    /// otherwise; with the symbolic links Linux follows on the way (the
    /// selector links before the first descriptor link, [`selector_links`],
    /// and each descriptor link), or `ELOOP` where they pass `MAXSYMLINKS`.
    /// The canonical spellings it walks through name tasks by number, so
    /// only the guest's own spelling has selector links to count. Whether a
    /// binding is stale is left to the injection of the call, which respells
    /// the same path.
    fn decided_path<G: Guest<Self>>(guest: &G, path: &Path) -> Result<(PathBuf, usize), Errno> {
        let mut bytes = path.as_os_str().as_bytes().to_vec();
        let mut links = 0;
        let mut first = true;
        while let Some((link, rest)) = through_descriptor_link(&bytes) {
            let Some(binding) = Self::own_link_directory(guest, &link) else {
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
            bytes = below(&binding.canonical(), &rest);
        }
        Ok((PathBuf::from(std::ffi::OsString::from_vec(bytes)), links))
    }

    /// Answer a `readlink` of the absolute guest path `path` into the buffer
    /// `buf` of `buf_len` bytes if it is a descriptor link of the caller's to
    /// a descriptor directory ([`Self::own_link_directory`]): the directory's
    /// path (`/proc/<pid>/fd`, which `proc_fd_link` reads), truncated to the
    /// buffer and without a NUL, as `do_readlinkat` copies it.
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
}

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

    /// A directory descriptor on procfs's root is told from the descriptor
    /// alone, with nothing opened: its identity is an anchor's, or its
    /// filesystem is procfs at the root inode; one whose filesystem cannot be
    /// told is refused all the same (fail closed).
    #[test]
    fn a_procfs_root_is_told_without_opening_anything() {
        type Tool = Detcore<crate::record_or_replay::NoopTool>;
        let proc = reverie::syscalls::linux_abi::PROC_SUPER_MAGIC as i64;
        let none = BTreeSet::new();
        assert!(Tool::on_procfs_root((22, 1), Ok(proc), &none));
        assert!(!Tool::on_procfs_root((22, 2), Ok(proc), &none));
        assert!(!Tool::on_procfs_root((22, 1), Ok(0x9123683e), &none));
        assert!(Tool::on_procfs_root((22, 5), Err(Errno::EACCES), &none));
        assert!(Tool::on_procfs_root((22, 5), Err(Errno::ENOMEM), &none));
        let anchors = BTreeSet::from([(22, 5)]);
        assert!(Tool::on_procfs_root((22, 5), Ok(0x9123683e), &anchors));
    }

    #[test]
    fn every_redirected_call_has_its_operands() {
        assert_eq!(relative_operands(Sysno::fchmodat2), &[(0, 1)]);
        assert_eq!(relative_operands(Sysno::execveat), &[] as &[(usize, usize)]);
        assert_eq!(absolute_operands(Sysno::rename), &[0, 1]);
        assert_eq!(absolute_operands(Sysno::execve), &[] as &[usize]);
        assert_eq!(absolute_operands(Sysno::read), &[] as &[usize]);
    }
}
