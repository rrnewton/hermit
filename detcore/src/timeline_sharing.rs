/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): what a multiverse copy shares with its
//! template and the other copies, and the refusals by name for it (owner,
//! Slack 6804).
//!
//! A copy gets its own instance of its memory, its threads, its pipes and
//! socket pairs, its regular files' positions and its outputs
//! (`crate::timelines`). It still shares the file system and the network
//! with its template and every other copy, and two kinds of use of those
//! make one copy's run visible in another's. Each is refused in a copy
//! before the call has any effect; the run ends with
//! `HERMIT_POLICY_REFUSAL_EXIT` and a message naming the file or socket.
//!
//! - **Rule F, a shared regular file.**
//!   - A call that changes the contents or size of a regular file through a
//!     descriptor: `write`, `writev`, `pwrite64`, `pwritev`, `pwritev2`,
//!     `ftruncate`, `fallocate`, and the output descriptor of
//!     `copy_file_range`, `sendfile` and `splice`.
//!   - `mmap` with `MAP_SHARED` or `MAP_SHARED_VALIDATE` of a regular file
//!     whose descriptor is open for writing: Linux lets that mapping be
//!     written, at once or after `mprotect`, with no further syscall.
//!   - A change to an inode's metadata through a descriptor of a regular
//!     file or a directory: `fchmod`, `fchown`, `fsetxattr`, `fremovexattr`,
//!     and the descriptor forms (a null path, or an empty one with
//!     `AT_EMPTY_PATH`) of `utimensat`, `futimesat`, `fchownat`,
//!     `fchmodat2`, `setxattrat`, `removexattrat` and `file_setattr`.
//!   - An open that creates or truncates a regular file: `creat`, and
//!     `open`, `openat` or `openat2` with `O_CREAT` where the path names
//!     nothing in an existing directory, or with `O_TRUNC` (without
//!     `O_EXCL`) where it names a shared regular file; `truncate` of a
//!     shared regular file. A relative path is resolved from the directory
//!     the backend names for the call's directory descriptor, or for the
//!     caller's working directory; where it names none, such an open is
//!     refused whenever it may create or truncate. `O_TMPFILE` creates an
//!     unnamed file only this copy reaches, and is not refused.
//!   - A change to a name, or to an inode by path: `unlink`, `unlinkat`,
//!     `rename`, `renameat`, `renameat2`, `mkdir`, `mkdirat`, `rmdir`,
//!     `link`, `linkat`, `symlink`, `symlinkat`, `mknod`, `mknodat`,
//!     `chmod`, `fchmodat`, `chown`, `lchown`, `utime`, `utimes`,
//!     `setxattr`, `lsetxattr`, `removexattr`, `lremovexattr`, and the path
//!     forms of the descriptor-or-path calls above.
//!
//!   A regular file is shared unless only this copy reaches it: one of the
//!   copy's own outputs (its standard output, standard error and registered
//!   outputs, after their redirection), a file on procfs (per-process kernel
//!   state), or an unnamed file (`st_nlink == 0`: a `memfd`, an `O_TMPFILE`
//!   file) that the template did not hold at the cut. Reading a shared file
//!   is not refused: no copy can change it.
//! - **Rule I, an inet socket.** A call on an `AF_INET` or `AF_INET6` socket
//!   that moves data or changes the socket's state: `read`, `readv`,
//!   `preadv`, `preadv2`, `recvfrom`, `recvmsg`, `recvmmsg`, `write`,
//!   `writev`, `sendto`, `sendmsg`, `sendmmsg`, `connect`, `accept`,
//!   `accept4`, `bind`, `listen`, `shutdown`, `setsockopt`, `getsockopt`
//!   (reading `SO_ERROR` clears it), the socket end of `sendfile` and
//!   `splice`, `fcntl` with `F_SETFL`, `F_SETOWN`, `F_SETOWN_EX` or
//!   `F_SETSIG`, and `ioctl` with `FIONBIO`, `FIOASYNC`, `FIOSETOWN` or
//!   `SIOCSPGRP`. This covers an inet socket whether its peer is a guest
//!   process or not: a copy shares the sockets it inherited, and the network
//!   namespace's ports and loopback peers, with the template and every other
//!   copy. Not refused: creating or closing one, `getsockname`,
//!   `getpeername`, `fstat`, other `ioctl` requests, and waiting for
//!   readiness (`poll`, `select`, `epoll`), since no copy can move data
//!   through one.
//!
//! The gate names each descriptor a call reaches as the backend names it
//! (`Guest::link_path`): the text `readlink(2)` of `/proc/<pid>/fd/<fd>`
//! gives. It finds the object behind that text through a [`HostView`]. On
//! the KVM backend the executor runs each guest call on a host descriptor of
//! its own, whose link is the text the fact gives, so the object is one of
//! this process's ([`OwnProcess`]): a socket by its `socket:[<inode>]` text,
//! a named file by its path, and an unlinked one by the descriptors whose
//! link reads the same. Where several objects read the same text, the call
//! is judged as if it reached the one the policy refuses, and a shared
//! mapping is refused where any descriptor of that text is open for writing
//! (fail closed). A descriptor the backend does not name (on the KVM
//! backend: a `/proc` file the executor makes itself and, with captured
//! output, the run's standard output and error) is refused for every use but
//! reading.
//!
//! A guest that maps a regular file shared before the cut could write it
//! with no syscall at all, so `crate::timelines` refuses that capture
//! instead ([`shared_file_mapping`]).
//!
//! A call in flight at the cut, in any thread, resumes in the copy past the
//! point where calls are checked, so the gate keeps each thread's call in
//! flight from the run's start, with the names of what it reached as it
//! entered, and judges those calls as the copy starts.
//!
//! The structure is the one the lazy design (owner, Slack 6803) extends: the
//! gate ([`TimelineGate`]) finds, before each call, the resources the copy
//! shares that the call is about to use, and puts each use to one
//! per-resource policy ([`SharingPolicy`]). [`RefuseShared`] refuses every
//! such use; a duplicating policy will give the copy its own instance first
//! instead.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::ffi::OsStr;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use reverie::Errno;
use reverie::FactError;
use reverie::PathLink;
use reverie::syscalls::Sysno;

use crate::call_gate::CallGate;
use crate::call_gate::GateCall;

/// How a call uses a descriptor it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Use {
    /// Reads from it.
    Read,
    /// Writes to it, or changes a file's size.
    Write,
    /// Maps it with `MAP_SHARED`.
    MapShared,
    /// Changes the metadata of the inode behind it.
    Metadata,
    /// Moves data through a socket or changes its connection or options.
    Traffic,
    /// Changes what every descriptor that shares its open file description
    /// or socket sees (`F_SETFL`, `FIONBIO`, ...).
    Control,
}

/// A guest descriptor or working directory, as the backend names it
/// (`Guest::link_path`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    /// The text `readlink(2)` of its `/proc` link gives.
    Text(Vec<u8>),
    /// The descriptor is not open (`EBADF`).
    Closed,
    /// The backend does not name it.
    Unknown,
}

impl Link {
    /// The caller's link `link`, as the backend names it.
    fn of(call: &GateCall<'_>, link: PathLink) -> Link {
        match call.link(link) {
            Ok(text) => Link::Text(text),
            Err(FactError::Errno(errno)) if errno == Errno::EBADF => Link::Closed,
            Err(_) => Link::Unknown,
        }
    }

    /// How a message names descriptor `fd`, named by this link.
    fn name(&self, fd: RawFd) -> String {
        match self {
            Link::Text(text) => String::from_utf8_lossy(text).into_owned(),
            Link::Closed | Link::Unknown => format!("descriptor {fd}"),
        }
    }
}

/// What Linux appends to the link text of an unlinked file (`d_path`).
const DELETED: &[u8] = b" (deleted)";

/// The file an open that may create or truncate names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// This absolute path: the call's own, or its relative path joined to
    /// the directory the backend names.
    Path(PathBuf),
    /// A relative path from a directory the backend does not name.
    Unresolved(PathBuf),
    /// No file the call can open: a null or unreadable path (`EFAULT`), or a
    /// relative one from a descriptor that is not open (`EBADF`) or not a
    /// directory (`ENOTDIR`).
    Failing,
}

/// One thing a call reaches, before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The descriptor `fd`, used as `how`, named `link` as the call entered.
    Descriptor { fd: RawFd, how: Use, link: Link },
    /// A change, by path, to a name or to an inode's metadata.
    PathChange { path: Option<PathBuf> },
    /// An open that creates `target` where it names nothing, or truncates
    /// it where it names a regular file.
    CreatingOpen {
        target: Target,
        create: bool,
        truncate: bool,
    },
}

/// `O_TMPFILE` without the `O_DIRECTORY` bit it includes.
const O_TMPFILE_BIT: i32 = libc::O_TMPFILE & !libc::O_DIRECTORY;

/// `fcntl` commands that change what every descriptor of an open file
/// description, or of a socket, sees.
fn shared_fcntl(command: i32) -> bool {
    // From Linux's include/uapi/asm-generic/fcntl.h; the libc crate does
    // not name these two.
    const F_SETSIG: i32 = 10;
    const F_SETOWN_EX: i32 = 15;
    matches!(
        command,
        libc::F_SETFL | libc::F_SETOWN | F_SETOWN_EX | F_SETSIG
    )
}

/// `ioctl` requests that change a socket's state, or its open file
/// description's (Linux takes the request as an `unsigned int`).
fn shared_ioctl(request: u32) -> bool {
    const FIOSETOWN: u32 = 0x8901;
    const SIOCSPGRP: u32 = 0x8902;
    [
        libc::FIONBIO as u32,
        libc::FIOASYNC as u32,
        FIOSETOWN,
        SIOCSPGRP,
    ]
    .contains(&request)
}

/// The file an open of the path at `address` in the caller's memory names,
/// relative to `dirfd`, or to the caller's working directory for
/// `AT_FDCWD`.
fn target(call: &GateCall<'_>, dirfd: RawFd, address: usize) -> Target {
    let Some(path) = call.path(address) else {
        return Target::Failing;
    };
    if path.is_absolute() {
        return Target::Path(path);
    }
    let base = if dirfd == libc::AT_FDCWD {
        PathLink::WorkingDirectory
    } else {
        PathLink::Descriptor(dirfd)
    };
    match Link::of(call, base) {
        // A directory's text is its path; an unlinked one's carries
        // " (deleted)", which a directory's own name may end in too.
        Link::Text(dir) if dir.first() == Some(&b'/') && !dir.ends_with(DELETED) => {
            Target::Path(Path::new(OsStr::from_bytes(&dir)).join(path))
        }
        // An object with no path is no directory.
        Link::Text(dir) if dir.first() != Some(&b'/') => Target::Failing,
        Link::Closed => Target::Failing,
        Link::Text(_) | Link::Unknown => Target::Unresolved(path),
    }
}

/// What `call` reaches that a copy may share, from its number and arguments
/// and the backend's names for its descriptors (the module documentation
/// lists the calls). Any other call reaches nothing.
pub(crate) fn reaches(call: &GateCall<'_>) -> Vec<Reach> {
    let a = &call.args;
    let descriptor = |raw: usize, how: Use| {
        let fd = raw as RawFd;
        Reach::Descriptor {
            fd,
            how,
            link: Link::of(call, PathLink::Descriptor(fd)),
        }
    };
    let change = |address: usize| Reach::PathChange {
        path: call.path(address),
    };
    // Calls that act on the descriptor itself for a null path, or for an
    // empty one with AT_EMPTY_PATH among `flags`.
    let path_or_descriptor =
        |dirfd: usize, address: usize, flags: Option<usize>| match call.path(address) {
            None => descriptor(dirfd, Use::Metadata),
            Some(path)
                if path.as_os_str().is_empty()
                    && flags.is_some_and(|f| f as i32 & libc::AT_EMPTY_PATH != 0) =>
            {
                descriptor(dirfd, Use::Metadata)
            }
            Some(path) => Reach::PathChange { path: Some(path) },
        };
    // An open reaches its file only where it may create or truncate it.
    let open = |dirfd: RawFd, address: usize, flags: i32| {
        let create = flags & libc::O_CREAT != 0;
        let truncate = flags & libc::O_TRUNC != 0 && flags & libc::O_EXCL == 0;
        if flags & O_TMPFILE_BIT != 0 || !(create || truncate) {
            return Vec::new();
        }
        vec![Reach::CreatingOpen {
            target: target(call, dirfd, address),
            create,
            truncate,
        }]
    };
    match call.number {
        Sysno::read | Sysno::readv | Sysno::preadv | Sysno::preadv2 => {
            vec![descriptor(a.arg0, Use::Read)]
        }
        Sysno::write
        | Sysno::writev
        | Sysno::pwrite64
        | Sysno::pwritev
        | Sysno::pwritev2
        | Sysno::ftruncate
        | Sysno::fallocate => vec![descriptor(a.arg0, Use::Write)],
        Sysno::copy_file_range | Sysno::splice => vec![
            descriptor(a.arg0, Use::Read),
            descriptor(a.arg2, Use::Write),
        ],
        // sendfile(out_fd, in_fd, offset, count).
        Sysno::sendfile => vec![
            descriptor(a.arg1, Use::Read),
            descriptor(a.arg0, Use::Write),
        ],
        Sysno::mmap => {
            let flags = a.arg3 as i32;
            let sharing = flags & 0x0f;
            if flags & libc::MAP_ANONYMOUS == 0
                && (sharing == libc::MAP_SHARED || sharing == libc::MAP_SHARED_VALIDATE)
            {
                vec![descriptor(a.arg4, Use::MapShared)]
            } else {
                Vec::new()
            }
        }
        Sysno::fchmod | Sysno::fchown | Sysno::fsetxattr | Sysno::fremovexattr => {
            vec![descriptor(a.arg0, Use::Metadata)]
        }
        Sysno::recvfrom
        | Sysno::recvmsg
        | Sysno::recvmmsg
        | Sysno::sendto
        | Sysno::sendmsg
        | Sysno::sendmmsg
        | Sysno::connect
        | Sysno::accept
        | Sysno::accept4
        | Sysno::bind
        | Sysno::listen
        | Sysno::shutdown
        | Sysno::setsockopt
        | Sysno::getsockopt => vec![descriptor(a.arg0, Use::Traffic)],
        Sysno::fcntl if shared_fcntl(a.arg1 as i32) => vec![descriptor(a.arg0, Use::Control)],
        Sysno::ioctl if shared_ioctl(a.arg1 as u32) => vec![descriptor(a.arg0, Use::Control)],
        Sysno::open => open(libc::AT_FDCWD, a.arg0, a.arg1 as i32),
        Sysno::openat => open(a.arg0 as RawFd, a.arg1, a.arg2 as i32),
        // openat2(dirfd, path, how, size): the flags are how's first field.
        // An unreadable how fails the call with EFAULT.
        Sysno::openat2 => match call.u64_at(a.arg2) {
            Some(flags) => open(a.arg0 as RawFd, a.arg1, flags as i32),
            None => Vec::new(),
        },
        Sysno::creat => open(
            libc::AT_FDCWD,
            a.arg0,
            libc::O_CREAT | libc::O_TRUNC | libc::O_WRONLY,
        ),
        Sysno::truncate => open(libc::AT_FDCWD, a.arg0, libc::O_TRUNC),
        // The name each call changes or creates: link and symlink create
        // their second path, linkat its fourth, symlinkat its third.
        Sysno::unlink
        | Sysno::rename
        | Sysno::mkdir
        | Sysno::rmdir
        | Sysno::mknod
        | Sysno::chmod
        | Sysno::chown
        | Sysno::lchown
        | Sysno::utime
        | Sysno::utimes
        | Sysno::setxattr
        | Sysno::lsetxattr
        | Sysno::removexattr
        | Sysno::lremovexattr => vec![change(a.arg0)],
        Sysno::link | Sysno::symlink => vec![change(a.arg1)],
        Sysno::unlinkat
        | Sysno::renameat
        | Sysno::renameat2
        | Sysno::mkdirat
        | Sysno::mknodat
        | Sysno::fchmodat => vec![change(a.arg1)],
        Sysno::linkat => vec![change(a.arg3)],
        Sysno::symlinkat => vec![change(a.arg2)],
        Sysno::fchownat => vec![path_or_descriptor(a.arg0, a.arg1, Some(a.arg4))],
        Sysno::fchmodat2 | Sysno::utimensat => {
            vec![path_or_descriptor(a.arg0, a.arg1, Some(a.arg3))]
        }
        Sysno::setxattrat | Sysno::removexattrat => {
            vec![path_or_descriptor(a.arg0, a.arg1, Some(a.arg2))]
        }
        Sysno::file_setattr => vec![path_or_descriptor(a.arg0, a.arg1, Some(a.arg4))],
        Sysno::futimesat => vec![path_or_descriptor(a.arg0, a.arg1, None)],
        _ => Vec::new(),
    }
}

/// What a descriptor names, as far as sharing goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A regular file the template or another copy can reach.
    SharedFile,
    /// A regular file only this copy reaches.
    PrivateFile,
    /// A directory.
    Directory,
    /// An `AF_INET` or `AF_INET6` socket.
    InetSocket,
    /// Anything else: pipes and Unix sockets (the copy's own since the
    /// fork), devices, event and timer descriptors.
    Other,
    /// No such descriptor: the call fails as Linux fails it.
    Closed,
    /// A descriptor the backend does not name, or whose object this process
    /// cannot find.
    Unknown,
}

/// One object behind a host descriptor.
#[derive(Clone, Debug)]
pub(crate) struct Object {
    /// Its `fstat`.
    pub stat: libc::stat,
    /// The descriptor's open file description is open for writing.
    pub writable: bool,
    /// It is on procfs.
    pub procfs: bool,
    /// An inet socket's addresses, for a message; `None` for any other
    /// object.
    pub inet: Option<String>,
}

/// What a path resolves to.
#[derive(Debug)]
pub(crate) enum Resolved {
    /// It names this; `procfs` for a file on procfs.
    Found { stat: libc::stat, procfs: bool },
    /// Nothing is there (`ENOENT`).
    Missing,
    /// The lookup fails with another error, as a call's will.
    Failed,
}

/// What a copy's process can read about the objects behind the backend's
/// link texts, and about paths, without asking the guest.
pub(crate) trait HostView: Send + Sync {
    /// What absolute `path` names, following a final symbolic link, as a
    /// guest call resolves it.
    fn resolve(&self, path: &Path) -> Resolved;
    /// The objects of the host descriptors whose link reads `text`.
    fn named(&self, text: &[u8]) -> Vec<Object>;
    /// The socket whose link reads `text` (`socket:[<inode>]`), through
    /// whichever of its descriptors is found first.
    fn socket(&self, text: &[u8]) -> Option<Object> {
        self.named(text).into_iter().next()
    }
}

/// `PROC_SUPER_MAGIC`, from Linux's `include/uapi/linux/magic.h`.
const PROC_SUPER_MAGIC: i64 = 0x9fa0;

/// The host view of a backend whose guest descriptors are host descriptors
/// of this process, and whose guest paths resolve as this process's do: the
/// KVM backend's executor runs each guest call on a host descriptor of its
/// own, and names a guest descriptor by that one's link.
#[derive(Default)]
pub(crate) struct OwnProcess {
    /// The host descriptor last found for each socket's link text: a
    /// descriptor whose link still reads that text is that socket.
    sockets: Mutex<HashMap<Vec<u8>, RawFd>>,
}

/// `fstat` of this process's descriptor `fd`.
fn fstat(fd: RawFd) -> Option<libc::stat> {
    // SAFETY: fstat into a zeroed buffer.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    (unsafe { libc::fstat(fd, &mut stat) } == 0).then_some(stat)
}

/// The link text of this process's descriptor `fd`.
fn read_link(fd: RawFd) -> Option<Vec<u8>> {
    std::fs::read_link(format!("/proc/self/fd/{fd}"))
        .ok()
        .map(|target| target.into_os_string().into_vec())
}

/// This process's descriptors whose link reads `text`.
fn descriptors_named(text: &[u8]) -> Vec<RawFd> {
    // Raw getdents64 (util::find_in_directory), never glibc's opendir.
    let mut fds = Vec::new();
    let _ = crate::util::find_in_directory(Path::new("/proc/self/fd"), |name| {
        if let Some(fd) = name.to_str().and_then(|name| name.parse::<RawFd>().ok())
            && read_link(fd).as_deref() == Some(text)
        {
            fds.push(fd);
        }
        None::<()>
    });
    fds
}

/// A socket's domain (`SO_DOMAIN`).
fn socket_domain(fd: RawFd) -> Option<i32> {
    let mut domain: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the kernel writes one int.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_DOMAIN,
            (&mut domain as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    (rc == 0).then_some(domain)
}

/// An inet socket's local or peer address, as `ip:port`.
fn inet_address(fd: RawFd, peer: bool) -> Option<String> {
    // SAFETY: a zeroed sockaddr_storage.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let address = (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr>();
    // SAFETY: the kernel writes at most `length` bytes into `storage`.
    let rc = unsafe {
        if peer {
            libc::getpeername(fd, address, &mut length)
        } else {
            libc::getsockname(fd, address, &mut length)
        }
    };
    if rc != 0 {
        return None;
    }
    match storage.ss_family as i32 {
        libc::AF_INET => {
            // SAFETY: the family says the storage holds a sockaddr_in.
            let inet = unsafe {
                &*(&storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in>()
            };
            let ip = std::net::Ipv4Addr::from(u32::from_be(inet.sin_addr.s_addr));
            Some(format!("{ip}:{}", u16::from_be(inet.sin_port)))
        }
        libc::AF_INET6 => {
            // SAFETY: the family says the storage holds a sockaddr_in6.
            let inet = unsafe {
                &*(&storage as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()
            };
            let ip = std::net::Ipv6Addr::from(inet.sin6_addr.s6_addr);
            Some(format!("[{ip}]:{}", u16::from_be(inet.sin6_port)))
        }
        _ => None,
    }
}

/// The object behind this process's descriptor `fd`.
fn host_object(fd: RawFd) -> Option<Object> {
    let stat = fstat(fd)?;
    // SAFETY: fcntl on a descriptor number; no memory.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    // SAFETY: fstatfs into a zeroed buffer.
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    let procfs = unsafe { libc::fstatfs(fd, &mut fs) } == 0 && fs.f_type as i64 == PROC_SUPER_MAGIC;
    let inet = (stat.st_mode & libc::S_IFMT == libc::S_IFSOCK
        && matches!(socket_domain(fd), Some(libc::AF_INET | libc::AF_INET6)))
    .then(|| {
        let local = inet_address(fd, false).unwrap_or_else(|| "an unread address".to_string());
        match inet_address(fd, true) {
            Some(peer) => format!("{local} -> {peer}"),
            None => format!("{local} (no peer)"),
        }
    });
    Some(Object {
        stat,
        writable: flags >= 0 && flags & libc::O_ACCMODE != libc::O_RDONLY,
        procfs,
        inet,
    })
}

impl HostView for OwnProcess {
    fn resolve(&self, path: &Path) -> Resolved {
        let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
            return Resolved::Failed;
        };
        // SAFETY: stat into a zeroed buffer, with a NUL-terminated path.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::stat(name.as_ptr(), &mut stat) } != 0 {
            return match std::io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT) => Resolved::Missing,
                _ => Resolved::Failed,
            };
        }
        let procfs = stat.st_mode & libc::S_IFMT == libc::S_IFREG && {
            // SAFETY: statfs into a zeroed buffer, with the same path.
            let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            let found = unsafe { libc::statfs(name.as_ptr(), &mut fs) } == 0;
            found && fs.f_type as i64 == PROC_SUPER_MAGIC
        };
        Resolved::Found { stat, procfs }
    }

    fn named(&self, text: &[u8]) -> Vec<Object> {
        descriptors_named(text)
            .into_iter()
            .filter_map(host_object)
            .collect()
    }

    fn socket(&self, text: &[u8]) -> Option<Object> {
        let mut sockets = self
            .sockets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(&fd) = sockets.get(text)
            && read_link(fd).as_deref() == Some(text)
        {
            return host_object(fd);
        }
        let fd = descriptors_named(text).into_iter().next()?;
        sockets.insert(text.to_vec(), fd);
        host_object(fd)
    }
}

/// A resource a copy shares, as a call is about to use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Shared {
    /// A regular file or directory, by what names it.
    File(String),
    /// A path whose name or inode the call changes.
    Name(Option<PathBuf>),
    /// An inet socket, by its addresses.
    InetSocket(String),
    /// A descriptor the backend does not name, by its number.
    Unnamed(String),
}

/// What the call would do to a [`Shared`] resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Write the file or change its size.
    Write,
    /// Map the file shared and writable.
    MapShared,
    /// Change the inode's metadata.
    Metadata,
    /// Create a file.
    Create,
    /// Truncate a file.
    Truncate,
    /// Change a name, or an inode by path.
    ChangeName,
    /// Use the socket.
    UseSocket,
    /// Use a descriptor in a way that changes what may be shared.
    Use,
}

/// One use of a shared resource, about to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SharedUse {
    /// The call.
    pub call: Sysno,
    /// What it would do.
    pub effect: Effect,
    /// To what.
    pub resource: Shared,
}

/// The one decision point for each use of a resource a copy shares.
pub(crate) trait SharingPolicy: Send + Sync {
    /// The message that refuses `what` before the call runs, or `None` to
    /// let it run. A duplicating policy (owner, Slack 6803) will answer a
    /// third way: give the copy its own instance, then let the call run.
    fn at_use(&self, what: &SharedUse) -> Option<String>;
}

/// The phrase every refusal of this module carries.
pub const REFUSAL_PHRASE: &str = "not supported yet in multiverse copies";

/// Refuses every use of a shared resource (owner, Slack 6804).
pub(crate) struct RefuseShared;

impl SharingPolicy for RefuseShared {
    fn at_use(&self, what: &SharedUse) -> Option<String> {
        let name = |path: &Option<PathBuf>| match path {
            Some(path) => path.display().to_string(),
            None => "a path it could not read".to_string(),
        };
        let action = match (&what.resource, what.effect) {
            (Shared::File(file), Effect::MapShared) => {
                format!("would map {file} shared and writable")
            }
            (Shared::File(file), Effect::Metadata) => {
                format!("would change the metadata of {file}")
            }
            (Shared::File(file), _) => format!("would write {file}"),
            (Shared::Name(path), Effect::Create) => format!("would create {}", name(path)),
            (Shared::Name(path), Effect::Truncate) => format!("would truncate {}", name(path)),
            (Shared::Name(path), _) => format!("would change {}", name(path)),
            (Shared::InetSocket(socket), _) => format!("would use the inet socket {socket}"),
            (Shared::Unnamed(descriptor), _) => {
                format!("would use {descriptor}, which the backend does not name")
            }
        };
        let what_kind = match what.resource {
            Shared::InetSocket(_) => "inet sockets",
            Shared::Unnamed(_) => "descriptors a copy cannot name",
            Shared::File(_) | Shared::Name(_) => "files the template and the other copies share",
        };
        Some(format!(
            "{}(2) {action}; {what_kind} are {REFUSAL_PHRASE}",
            what.call
        ))
    }
}

/// What a process knows once it is copy `index`.
pub(crate) struct CopyContext {
    /// Its timeline number.
    pub index: usize,
    /// Its own outputs, as (device, inode), after their redirection.
    pub outputs: HashSet<(u64, u64)>,
    /// The unnamed regular files the template held at the cut, as (device,
    /// inode): the template and every copy reach them.
    pub inherited_unnamed: HashSet<(u64, u64)>,
}

/// The gate of a run that may be copied. In the template it keeps each
/// thread's call in flight, and refuses nothing. Once the process is a copy
/// ([`TimelineGate::become_copy`]), it puts each use of a shared resource
/// that a call is about to make to the policy, beginning with the calls in
/// flight at the cut.
pub(crate) struct TimelineGate<V: HostView, P: SharingPolicy> {
    view: V,
    policy: P,
    copy: OnceLock<CopyContext>,
    /// By thread, its call in flight that reaches something a copy may
    /// share: the call's number and what it reaches, named as it entered.
    in_flight: Mutex<HashMap<i32, (Sysno, Vec<Reach>)>>,
}

fn identity(stat: &libc::stat) -> (u64, u64) {
    (stat.st_dev, stat.st_ino)
}

fn is_regular(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFREG
}

impl<V: HostView, P: SharingPolicy> TimelineGate<V, P> {
    /// A gate for a run that has not been copied yet.
    pub(crate) fn new(view: V, policy: P) -> Self {
        TimelineGate {
            view,
            policy,
            copy: OnceLock::new(),
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    /// Makes this process copy `context.index`. Returns the refusal of a
    /// call in flight at the cut, in any thread, that the copy would refuse:
    /// such a call resumes in the copy past the point where calls are
    /// checked. A call stops being in flight once Detcore has finished it
    /// (`CallGate::completed`); one waiting at a scheduler turn inside its
    /// handler after its effect is refused too, because the gate cannot tell
    /// it apart (fail closed).
    pub(crate) fn become_copy(&self, context: CopyContext) -> Option<String> {
        let context = self.copy.get_or_init(|| context);
        // A thread parked at the cut can hold this lock; it is re-created
        // only after the copy starts, so do not wait for it.
        let mut in_flight = match self.in_flight.try_lock() {
            Ok(calls) => calls
                .iter()
                .map(|(thread, (number, reaches))| (*thread, *number, reaches.clone()))
                .collect::<Vec<_>>(),
            Err(_) => {
                return Some(format!(
                    "multiverse copy {}: a thread was recording its call at the cut, so the \
                     calls in flight there are unknown; copies taken there are {REFUSAL_PHRASE}",
                    context.index
                ));
            }
        };
        in_flight.sort_by_key(|(thread, ..)| *thread);
        in_flight.into_iter().find_map(|(thread, number, reaches)| {
            self.judge(context, number, reaches)
                .map(|message| format!("{message} (in flight in thread {thread} at the cut)"))
        })
    }

    /// The refusal, in copy `context`, of a call `number` that reaches
    /// `reaches`, if the policy refuses one of its uses of a shared resource.
    fn judge(&self, context: &CopyContext, number: Sysno, reaches: Vec<Reach>) -> Option<String> {
        reaches.into_iter().find_map(|reach| {
            let what = self.shared_use(context, number, reach)?;
            let message = self.policy.at_use(&what)?;
            Some(format!("multiverse copy {}: {message}", context.index))
        })
    }

    /// What a regular file with `stat` is in copy `context`.
    fn file_kind(&self, context: &CopyContext, stat: &libc::stat, procfs: bool) -> Kind {
        let id = identity(stat);
        let unnamed_and_new = stat.st_nlink == 0 && !context.inherited_unnamed.contains(&id);
        if context.outputs.contains(&id) || unnamed_and_new || procfs {
            Kind::PrivateFile
        } else {
            Kind::SharedFile
        }
    }

    /// What a named object with `stat` is in copy `context`.
    fn stat_kind(&self, context: &CopyContext, stat: &libc::stat, procfs: bool) -> Kind {
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => self.file_kind(context, stat, procfs),
            libc::S_IFDIR => Kind::Directory,
            _ => Kind::Other,
        }
    }

    /// What descriptor `fd`, named `link`, is in copy `context`, and how a
    /// message names it.
    pub(crate) fn classify(&self, context: &CopyContext, fd: RawFd, link: &Link) -> (Kind, String) {
        let name = link.name(fd);
        let text = match link {
            Link::Text(text) => text,
            Link::Closed => return (Kind::Closed, name),
            Link::Unknown => return (Kind::Unknown, name),
        };
        if text.first() != Some(&b'/') {
            // An object with no path; of those, only a socket may be inet.
            if !text.starts_with(b"socket:") {
                return (Kind::Other, name);
            }
            return match self.view.socket(text) {
                Some(Object {
                    inet: Some(addresses),
                    ..
                }) => (Kind::InetSocket, addresses),
                Some(_) => (Kind::Other, name),
                None => (Kind::Unknown, name),
            };
        }
        // A named file is what its path names, since Linux appends
        // " (deleted)" to the text once it is unlinked. An unlinked one, or
        // one whose path names nothing now, is found by the descriptors
        // whose link reads the same text.
        let resolved = (!text.ends_with(DELETED))
            .then(|| self.view.resolve(Path::new(OsStr::from_bytes(text))));
        let kinds: Vec<Kind> = match resolved {
            Some(Resolved::Found { stat, procfs }) => vec![self.stat_kind(context, &stat, procfs)],
            _ => self
                .view
                .named(text)
                .iter()
                .map(|object| self.stat_kind(context, &object.stat, object.procfs))
                .collect(),
        };
        // Several objects of one text: judged as the one the policy refuses.
        let kind = [Kind::SharedFile, Kind::Directory]
            .into_iter()
            .find(|kind| kinds.contains(kind))
            .or_else(|| kinds.first().copied())
            .unwrap_or(Kind::Unknown);
        (kind, name)
    }

    /// Whether a descriptor named `link` may be open for writing: where any
    /// host descriptor whose link reads the same text is, or none is found
    /// (fail closed).
    fn writable(&self, link: &Link) -> bool {
        let Link::Text(text) = link else {
            return true;
        };
        let objects = self.view.named(text);
        objects.is_empty() || objects.iter().any(|object| object.writable)
    }

    /// Whether the directory `path` would be created in exists.
    fn directory_exists(&self, path: &Path) -> bool {
        path.parent().is_some_and(|parent| {
            matches!(
                self.view.resolve(parent),
                Resolved::Found { stat, .. } if stat.st_mode & libc::S_IFMT == libc::S_IFDIR
            )
        })
    }

    /// The use of a shared resource that `reach` would make, if any.
    fn shared_use(&self, context: &CopyContext, call: Sysno, reach: Reach) -> Option<SharedUse> {
        let found = |effect: Effect, resource: Shared| {
            Some(SharedUse {
                call,
                effect,
                resource,
            })
        };
        match reach {
            Reach::Descriptor { fd, how, link } => {
                let (kind, name) = self.classify(context, fd, &link);
                match (kind, how) {
                    (Kind::SharedFile, Use::Write) => found(Effect::Write, Shared::File(name)),
                    (Kind::SharedFile, Use::MapShared) if self.writable(&link) => {
                        found(Effect::MapShared, Shared::File(name))
                    }
                    (Kind::SharedFile | Kind::Directory, Use::Metadata) => {
                        found(Effect::Metadata, Shared::File(name))
                    }
                    (Kind::InetSocket, Use::Read | Use::Write | Use::Traffic | Use::Control) => {
                        found(Effect::UseSocket, Shared::InetSocket(name))
                    }
                    (
                        Kind::Unknown,
                        Use::Write | Use::MapShared | Use::Metadata | Use::Traffic | Use::Control,
                    ) => found(Effect::Use, Shared::Unnamed(name)),
                    _ => None,
                }
            }
            Reach::PathChange { path } => found(Effect::ChangeName, Shared::Name(path)),
            Reach::CreatingOpen {
                target,
                create,
                truncate,
            } => match target {
                Target::Failing => None,
                Target::Unresolved(path) => found(
                    if create {
                        Effect::Create
                    } else {
                        Effect::Truncate
                    },
                    Shared::Name(Some(path)),
                ),
                Target::Path(path) => match self.view.resolve(&path) {
                    // Opening an existing file creates nothing; only
                    // truncating a shared one changes it.
                    Resolved::Found { stat, procfs }
                        if truncate
                            && is_regular(&stat)
                            && self.file_kind(context, &stat, procfs) == Kind::SharedFile =>
                    {
                        found(Effect::Truncate, Shared::Name(Some(path)))
                    }
                    Resolved::Found { .. } | Resolved::Failed => None,
                    // Linux fails a create in a directory that is not there.
                    Resolved::Missing if create && self.directory_exists(&path) => {
                        found(Effect::Create, Shared::Name(Some(path)))
                    }
                    Resolved::Missing => None,
                },
            },
        }
    }
}

impl<V: HostView, P: SharingPolicy> CallGate for TimelineGate<V, P> {
    fn refusal(&self, call: &GateCall<'_>) -> Option<String> {
        let reaches = reaches(call);
        if reaches.is_empty() {
            return None;
        }
        if let Some(context) = self.copy.get()
            && let Some(message) = self.judge(context, call.number, reaches.clone())
        {
            return Some(message);
        }
        if let Ok(mut calls) = self.in_flight.lock() {
            calls.insert(call.thread, (call.number, reaches));
        }
        None
    }

    fn completed(&self, thread: i32) {
        if let Ok(mut calls) = self.in_flight.lock() {
            calls.remove(&thread);
        }
    }
}

static GATE: OnceLock<TimelineGate<OwnProcess, RefuseShared>> = OnceLock::new();

/// Installs the gate of a run that may be copied: before its guest starts,
/// so that it knows every call in flight at a cut. Returns false when this
/// process already has a call gate.
pub(crate) fn install() -> bool {
    crate::call_gate::install(
        GATE.get_or_init(|| TimelineGate::new(OwnProcess::default(), RefuseShared)),
    )
}

/// Makes this process copy `context.index` ([`TimelineGate::become_copy`]).
pub(crate) fn become_copy(context: CopyContext) -> Result<Option<String>, String> {
    GATE.get()
        .map(|gate| gate.become_copy(context))
        .ok_or_else(|| "no sharing gate was installed before the guest started".to_string())
}

/// The (device, inode) of every unnamed regular file (`st_nlink == 0`)
/// among this process's descriptors `fds`: read in the template at the cut,
/// for [`CopyContext::inherited_unnamed`].
pub(crate) fn unnamed_files(fds: &[RawFd]) -> HashSet<(u64, u64)> {
    fds.iter()
        .filter_map(|&fd| fstat(fd))
        .filter(|stat| is_regular(stat) && stat.st_nlink == 0)
        .map(|stat| identity(&stat))
        .collect()
}

/// The (device, inode) of each of this process's descriptors `fds`.
pub(crate) fn identities(fds: &[RawFd]) -> HashSet<(u64, u64)> {
    fds.iter()
        .filter_map(|&fd| fstat(fd))
        .map(|stat| identity(&stat))
        .collect()
}

/// The name of the first mapping in `maps` (the text of `/proc/self/maps`)
/// that the template's copies would share: a mapping marked shared whose
/// name is a path, other than a `memfd` (each copy gets its own copy of
/// those, `reverie_kvm::host_fork`) and `/dev/zero` (shared anonymous
/// memory). On the KVM backend, the executor maps a guest's shared file
/// mappings into this process.
pub(crate) fn shared_file_mapping(maps: &str) -> Option<String> {
    maps.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let shared = fields.get(1)?.as_bytes().get(3) == Some(&b's');
        let name = fields.get(5..)?.join(" ");
        (shared
            && name.starts_with('/')
            && !name.starts_with("/memfd:")
            && !name.starts_with("/dev/zero"))
        .then_some(name)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::os::fd::AsRawFd;

    use reverie::syscalls::SyscallArgs;

    use super::*;

    /// A host view made of fixed answers.
    #[derive(Default)]
    struct Fixed {
        /// The objects behind each link text.
        objects: HashMap<Vec<u8>, Vec<Object>>,
        /// What each path names, and whether it is on procfs.
        paths: HashMap<PathBuf, (libc::stat, bool)>,
    }

    impl HostView for Fixed {
        fn resolve(&self, path: &Path) -> Resolved {
            match self.paths.get(path) {
                Some(&(stat, procfs)) => Resolved::Found { stat, procfs },
                None => Resolved::Missing,
            }
        }
        fn named(&self, text: &[u8]) -> Vec<Object> {
            self.objects.get(text).cloned().unwrap_or_default()
        }
    }

    fn stat_of(mode: u32, ino: u64, nlink: u64) -> libc::stat {
        // SAFETY: a plain C struct; zero is valid for every field.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        stat.st_mode = mode;
        stat.st_dev = 7;
        stat.st_ino = ino;
        stat.st_nlink = nlink;
        stat
    }

    fn file(ino: u64, nlink: u64, writable: bool) -> Object {
        Object {
            stat: stat_of(libc::S_IFREG, ino, nlink),
            writable,
            procfs: false,
            inet: None,
        }
    }

    fn socket(ino: u64, inet: Option<&str>) -> Object {
        Object {
            stat: stat_of(libc::S_IFSOCK, ino, 1),
            writable: true,
            procfs: false,
            inet: inet.map(str::to_string),
        }
    }

    // The guest's descriptors, named by `link`.
    const OUTPUT: RawFd = 1;
    const SHARED: RawFd = 3;
    const PROC: RawFd = 4;
    const NEW_MEMFD: RawFd = 5;
    const OLD_MEMFD: RawFd = 6;
    const INET: RawFd = 7;
    const UNIX: RawFd = 8;
    const DIR: RawFd = 9;
    const READ_ONLY: RawFd = 10;
    /// A read-only descriptor of the file `SHARED` holds for writing.
    const ALSO_SHARED: RawFd = 11;
    const PIPE: RawFd = 12;
    /// A descriptor the backend does not name.
    const UNNAMED: RawFd = 13;
    /// An unlinked file whose text two objects read: one new, one
    /// inherited.
    const AMBIGUOUS: RawFd = 14;
    /// A file whose path names nothing now (a directory mounted over).
    const SHADOWED: RawFd = 15;

    /// The guest's links, as the backend names them.
    fn link(link: PathLink) -> Result<Vec<u8>, FactError> {
        let text = match link {
            PathLink::WorkingDirectory => "/work",
            PathLink::Descriptor(fd) => match fd {
                OUTPUT => "/out/stdout",
                SHARED | ALSO_SHARED => "/data/shared",
                PROC => "/proc/7/status",
                NEW_MEMFD => "/memfd:new (deleted)",
                OLD_MEMFD => "/memfd:old (deleted)",
                INET => "socket:[105]",
                UNIX => "socket:[106]",
                DIR => "/data",
                READ_ONLY => "/data/read-only",
                PIPE => "pipe:[110]",
                AMBIGUOUS => "/memfd:same (deleted)",
                SHADOWED => "/data/shadowed",
                UNNAMED => return Err(FactError::NotProvided),
                _ => return Err(FactError::Errno(Errno::EBADF)),
            },
            _ => return Err(FactError::NotProvided),
        };
        Ok(text.as_bytes().to_vec())
    }

    fn context() -> CopyContext {
        CopyContext {
            index: 2,
            outputs: [(7, 101)].into(),
            inherited_unnamed: [(7, 104), (7, 112)].into(),
        }
    }

    fn view() -> Fixed {
        let mut view = Fixed::default();
        let mut path = |path: &str, mode: u32, ino: u64, procfs: bool| {
            view.paths
                .insert(path.into(), (stat_of(mode, ino, 1), procfs));
        };
        path("/data/shared", libc::S_IFREG, 100, false);
        path("/out/stdout", libc::S_IFREG, 101, false);
        path("/proc/7/status", libc::S_IFREG, 102, true);
        path("/data", libc::S_IFDIR, 107, false);
        path("/dev/null", libc::S_IFCHR, 108, false);
        path("/data/read-only", libc::S_IFREG, 109, false);
        path("/work", libc::S_IFDIR, 113, false);
        path("/work/existing", libc::S_IFREG, 114, false);
        let mut named = |text: &str, objects: Vec<Object>| {
            view.objects.insert(text.as_bytes().to_vec(), objects);
        };
        named(
            "/data/shared",
            vec![file(100, 1, true), file(100, 1, false)],
        );
        named("/data/read-only", vec![file(109, 1, false)]);
        named("/memfd:new (deleted)", vec![file(103, 0, true)]);
        named("/memfd:old (deleted)", vec![file(104, 0, true)]);
        named(
            "/memfd:same (deleted)",
            vec![file(111, 0, true), file(112, 0, true)],
        );
        named("/data/shadowed", vec![file(115, 1, true)]);
        named(
            "socket:[105]",
            vec![socket(105, Some("127.0.0.1:5000 -> 127.0.0.1:41234"))],
        );
        named("socket:[106]", vec![socket(106, None)]);
        view
    }

    fn gate() -> TimelineGate<Fixed, RefuseShared> {
        let gate = TimelineGate::new(view(), RefuseShared);
        assert_eq!(gate.become_copy(context()), None);
        gate
    }

    fn call<'a>(
        number: Sysno,
        args: [usize; 6],
        thread: i32,
        read_path: &'a dyn Fn(usize) -> Option<PathBuf>,
        read_memory: crate::call_gate::ReadMemory<'a>,
    ) -> GateCall<'a> {
        let args = SyscallArgs::new(args[0], args[1], args[2], args[3], args[4], args[5]);
        GateCall::new(number, args, thread, read_path, read_memory, &link)
    }

    /// The refusal `gate()` gives a call, with `paths` the guest's strings
    /// by address.
    fn refusal(number: Sysno, args: [usize; 6], paths: &[(usize, &str)]) -> Option<String> {
        let paths: HashMap<usize, PathBuf> = paths
            .iter()
            .map(|(at, p)| (*at, PathBuf::from(p)))
            .collect();
        let read_path = |address: usize| paths.get(&address).cloned();
        let read_memory = |_: usize, _: &mut [u8]| false;
        gate().refusal(&call(number, args, 1, &read_path, &read_memory))
    }

    #[test]
    fn a_call_in_flight_at_the_cut_is_refused_as_the_copy_starts() {
        let no_path = |_: usize| None;
        let no_memory = |_: usize, _: &mut [u8]| false;
        let shared_write = [SHARED as usize, 0, 1, 0, 0, 0];
        // In the template, nothing is refused; each call in flight is kept.
        let template = TimelineGate::new(view(), RefuseShared);
        let write = call(Sysno::write, shared_write, 11, &no_path, &no_memory);
        assert_eq!(template.refusal(&write), None);
        let recv = call(
            Sysno::recvfrom,
            [INET as usize, 0, 0, 0, 0, 0],
            12,
            &no_path,
            &no_memory,
        );
        assert_eq!(template.refusal(&recv), None);
        // Thread 12's receive returned before the cut; thread 11's write did not.
        template.completed(12);
        let message = template.become_copy(context()).unwrap();
        assert_eq!(
            message,
            "multiverse copy 2: write(2) would write /data/shared; files the template and \
             the other copies share are not supported yet in multiverse copies (in flight in \
             thread 11 at the cut)"
        );
        // With no call in flight, the copy starts; its own calls are checked.
        let template = TimelineGate::new(view(), RefuseShared);
        assert_eq!(template.refusal(&write), None);
        template.completed(11);
        assert_eq!(template.become_copy(context()), None);
        assert!(template.refusal(&write).is_some());
        assert!(template.refusal(&recv).is_some());
    }

    #[test]
    fn a_write_to_a_shared_file_is_refused_by_name() {
        let message = refusal(Sysno::write, [SHARED as usize, 0, 1, 0, 0, 0], &[]).unwrap();
        assert_eq!(
            message,
            "multiverse copy 2: write(2) would write /data/shared; files the template and \
             the other copies share are not supported yet in multiverse copies"
        );
        for number in [
            Sysno::writev,
            Sysno::pwrite64,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::ftruncate,
            Sysno::fallocate,
        ] {
            assert!(
                refusal(number, [SHARED as usize, 0, 0, 0, 0, 0], &[]).is_some(),
                "{number}"
            );
        }
        // The output end of the copying calls.
        assert!(refusal(Sysno::sendfile, [SHARED as usize, 0, 0, 0, 0, 0], &[]).is_some());
        assert!(
            refusal(
                Sysno::copy_file_range,
                [UNIX as usize, 0, SHARED as usize, 0, 0, 0],
                &[]
            )
            .is_some()
        );
        assert!(
            refusal(
                Sysno::splice,
                [PIPE as usize, 0, SHARED as usize, 0, 0, 0],
                &[]
            )
            .is_some()
        );
        // A file whose path names nothing now is found by its descriptors.
        let message = refusal(Sysno::write, [SHADOWED as usize, 0, 1, 0, 0, 0], &[]).unwrap();
        assert!(message.contains("would write /data/shadowed"), "{message}");
        // Two unlinked files of one text, one inherited: judged as shared.
        let message = refusal(Sysno::write, [AMBIGUOUS as usize, 0, 1, 0, 0, 0], &[]).unwrap();
        assert!(
            message.contains("would write /memfd:same (deleted)"),
            "{message}"
        );
    }

    #[test]
    fn writes_the_copy_alone_reaches_are_not_refused() {
        for fd in [OUTPUT, PROC, NEW_MEMFD, UNIX, PIPE, 99] {
            assert_eq!(
                refusal(Sysno::write, [fd as usize, 0, 1, 0, 0, 0], &[]),
                None,
                "{fd}"
            );
        }
        // A memfd the template held at the cut is shared.
        assert!(refusal(Sysno::write, [OLD_MEMFD as usize, 0, 1, 0, 0, 0], &[]).is_some());
        // Reading a shared file changes nothing.
        assert_eq!(
            refusal(Sysno::read, [SHARED as usize, 0, 1, 0, 0, 0], &[]),
            None
        );
        assert_eq!(
            refusal(
                Sysno::copy_file_range,
                [SHARED as usize, 0, OUTPUT as usize, 0, 0, 0],
                &[]
            ),
            None
        );
    }

    #[test]
    fn a_descriptor_the_backend_does_not_name_is_refused_for_every_use_but_reading() {
        let message = refusal(Sysno::write, [UNNAMED as usize, 0, 1, 0, 0, 0], &[]).unwrap();
        assert_eq!(
            message,
            "multiverse copy 2: write(2) would use descriptor 13, which the backend does not \
             name; descriptors a copy cannot name are not supported yet in multiverse copies"
        );
        let setfl = libc::F_SETFL as usize;
        for (number, args) in [
            (Sysno::ftruncate, [UNNAMED as usize, 0, 0, 0, 0, 0]),
            (Sysno::fchmod, [UNNAMED as usize, 0o600, 0, 0, 0, 0]),
            (Sysno::sendto, [UNNAMED as usize, 0, 0, 0, 0, 0]),
            (Sysno::fcntl, [UNNAMED as usize, setfl, 0, 0, 0, 0]),
        ] {
            assert!(refusal(number, args, &[]).is_some(), "{number}");
        }
        assert_eq!(
            refusal(Sysno::read, [UNNAMED as usize, 0, 1, 0, 0, 0], &[]),
            None
        );
    }

    #[test]
    fn a_shared_writable_file_mapping_is_refused() {
        let shared = libc::MAP_SHARED as usize;
        let message = refusal(Sysno::mmap, [0, 4096, 1, shared, SHARED as usize, 0], &[]).unwrap();
        assert!(
            message.contains("mmap(2) would map /data/shared shared and writable"),
            "{message}"
        );
        // A file no descriptor holds for writing: Linux refuses PROT_WRITE
        // on that mapping.
        assert_eq!(
            refusal(
                Sysno::mmap,
                [0, 4096, 1, shared, READ_ONLY as usize, 0],
                &[]
            ),
            None
        );
        // A read-only descriptor of a file another descriptor holds for
        // writing: the gate cannot tell them apart (fail closed).
        assert!(
            refusal(
                Sysno::mmap,
                [0, 4096, 1, shared, ALSO_SHARED as usize, 0],
                &[]
            )
            .is_some()
        );
        let private = libc::MAP_PRIVATE as usize;
        assert_eq!(
            refusal(Sysno::mmap, [0, 4096, 3, private, SHARED as usize, 0], &[]),
            None
        );
        let anonymous = (libc::MAP_SHARED | libc::MAP_ANONYMOUS) as usize;
        assert_eq!(
            refusal(
                Sysno::mmap,
                [0, 4096, 3, anonymous, SHARED as usize, 0],
                &[]
            ),
            None
        );
    }

    #[test]
    fn a_change_to_a_name_is_refused_by_path() {
        let message = refusal(Sysno::unlink, [0x10, 0, 0, 0, 0, 0], &[(0x10, "/data/x")]).unwrap();
        assert!(
            message.contains("unlink(2) would change /data/x"),
            "{message}"
        );
        // link and symlink create their second path, linkat its fourth.
        let message = refusal(
            Sysno::link,
            [0x10, 0x20, 0, 0, 0, 0],
            &[(0x10, "/a"), (0x20, "/b")],
        )
        .unwrap();
        assert!(message.contains("would change /b"), "{message}");
        let message = refusal(
            Sysno::linkat,
            [3, 0x10, 3, 0x20, 0, 0],
            &[(0x10, "a"), (0x20, "b")],
        )
        .unwrap();
        assert!(message.contains("would change b"), "{message}");
        // utimensat with a null path acts on the descriptor: a directory's
        // metadata is shared, a pipe's is not.
        assert!(refusal(Sysno::utimensat, [DIR as usize, 0, 0, 0, 0, 0], &[]).is_some());
        assert_eq!(
            refusal(Sysno::utimensat, [UNIX as usize, 0, 0, 0, 0, 0], &[]),
            None
        );
        assert!(refusal(Sysno::fchmod, [SHARED as usize, 0o600, 0, 0, 0, 0], &[]).is_some());
    }

    #[test]
    fn an_open_is_refused_only_where_it_creates_or_truncates_a_shared_file() {
        let creat = (libc::O_WRONLY | libc::O_CREAT) as usize;
        let trunc = (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) as usize;
        let at = libc::AT_FDCWD as usize;
        // Creates a file that does not exist, in a directory that does.
        let message = refusal(
            Sysno::openat,
            [at, 0x10, creat, 0, 0, 0],
            &[(0x10, "/data/new")],
        )
        .unwrap();
        assert!(
            message.contains("openat(2) would create /data/new"),
            "{message}"
        );
        // Linux fails a create in a directory that is not there.
        assert_eq!(
            refusal(
                Sysno::openat,
                [at, 0x10, creat, 0, 0, 0],
                &[(0x10, "/nowhere/new")]
            ),
            None
        );
        // Truncates an existing shared file.
        let message = refusal(
            Sysno::openat,
            [at, 0x10, trunc, 0, 0, 0],
            &[(0x10, "/data/shared")],
        )
        .unwrap();
        assert!(message.contains("would truncate /data/shared"), "{message}");
        // Opens an existing shared file without truncating it.
        assert_eq!(
            refusal(
                Sysno::openat,
                [at, 0x10, creat, 0, 0, 0],
                &[(0x10, "/data/shared")]
            ),
            None
        );
        // A device, and the copy's own output, may be truncated.
        for path in ["/dev/null", "/out/stdout"] {
            assert_eq!(
                refusal(Sysno::open, [0x10, trunc, 0, 0, 0, 0], &[(0x10, path)]),
                None,
                "{path}"
            );
        }
        // An unnamed O_TMPFILE file is the copy's own.
        let tmpfile = (libc::O_TMPFILE | libc::O_RDWR) as usize;
        assert_eq!(
            refusal(
                Sysno::openat,
                [at, 0x10, tmpfile, 0, 0, 0],
                &[(0x10, "/data")]
            ),
            None
        );
        // Relative to the working directory, which the backend names.
        let message =
            refusal(Sysno::open, [0x10, creat, 0, 0, 0, 0], &[(0x10, "out.txt")]).unwrap();
        assert!(message.contains("would create /work/out.txt"), "{message}");
        let message = refusal(
            Sysno::open,
            [0x10, trunc, 0, 0, 0, 0],
            &[(0x10, "existing")],
        )
        .unwrap();
        assert!(
            message.contains("would truncate /work/existing"),
            "{message}"
        );
        // Relative to a directory descriptor.
        let message = refusal(
            Sysno::openat,
            [DIR as usize, 0x10, trunc, 0, 0, 0],
            &[(0x10, "shared")],
        )
        .unwrap();
        assert!(message.contains("would truncate /data/shared"), "{message}");
        // From a directory the backend does not name: refused whenever it
        // may create.
        let message = refusal(
            Sysno::openat,
            [UNNAMED as usize, 0x10, creat, 0, 0, 0],
            &[(0x10, "out.txt")],
        )
        .unwrap();
        assert!(message.contains("would create out.txt"), "{message}");
        // From a descriptor that is no directory, or not open: the call fails.
        for dirfd in [PIPE, 99] {
            assert_eq!(
                refusal(
                    Sysno::openat,
                    [dirfd as usize, 0x10, creat, 0, 0, 0],
                    &[(0x10, "out.txt")]
                ),
                None,
                "{dirfd}"
            );
        }
        // Read-only opens are not refused.
        assert_eq!(
            refusal(Sysno::open, [0x10, 0, 0, 0, 0, 0], &[(0x10, "out.txt")]),
            None
        );
        assert!(
            refusal(
                Sysno::creat,
                [0x10, 0o644, 0, 0, 0, 0],
                &[(0x10, "/data/new")]
            )
            .is_some()
        );
        assert!(
            refusal(
                Sysno::truncate,
                [0x10, 0, 0, 0, 0, 0],
                &[(0x10, "/data/shared")]
            )
            .is_some()
        );
    }

    #[test]
    fn a_use_of_an_inet_socket_is_refused_by_name() {
        let message = refusal(Sysno::connect, [INET as usize, 0, 16, 0, 0, 0], &[]).unwrap();
        assert_eq!(
            message,
            "multiverse copy 2: connect(2) would use the inet socket 127.0.0.1:5000 -> \
             127.0.0.1:41234; inet sockets are not supported yet in multiverse copies"
        );
        for number in [
            Sysno::read,
            Sysno::write,
            Sysno::recvfrom,
            Sysno::recvmsg,
            Sysno::sendto,
            Sysno::sendmsg,
            Sysno::accept,
            Sysno::accept4,
            Sysno::bind,
            Sysno::listen,
            Sysno::shutdown,
            Sysno::setsockopt,
            Sysno::getsockopt,
        ] {
            assert!(
                refusal(number, [INET as usize, 0, 0, 0, 0, 0], &[]).is_some(),
                "{number}"
            );
        }
        let setfl = libc::F_SETFL as usize;
        assert!(refusal(Sysno::fcntl, [INET as usize, setfl, 0, 0, 0, 0], &[]).is_some());
        let fionbio = libc::FIONBIO as usize;
        assert!(refusal(Sysno::ioctl, [INET as usize, fionbio, 0, 0, 0, 0], &[]).is_some());
    }

    #[test]
    fn inspecting_an_inet_socket_or_using_a_unix_one_is_not_refused() {
        for number in [
            Sysno::getsockname,
            Sysno::getpeername,
            Sysno::fstat,
            Sysno::close,
        ] {
            assert_eq!(
                refusal(number, [INET as usize, 0, 0, 0, 0, 0], &[]),
                None,
                "{number}"
            );
        }
        let getfl = libc::F_GETFL as usize;
        assert_eq!(
            refusal(Sysno::fcntl, [INET as usize, getfl, 0, 0, 0, 0], &[]),
            None
        );
        let fionread = libc::FIONREAD as usize;
        assert_eq!(
            refusal(Sysno::ioctl, [INET as usize, fionread, 0, 0, 0, 0], &[]),
            None
        );
        for number in [Sysno::connect, Sysno::sendmsg, Sysno::recvmsg, Sysno::write] {
            assert_eq!(
                refusal(number, [UNIX as usize, 0, 0, 0, 0, 0], &[]),
                None,
                "{number}"
            );
        }
    }

    #[test]
    fn the_own_process_view_finds_the_objects_behind_this_process_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let named = std::fs::File::create(dir.path().join("named")).unwrap();
        // SAFETY: a NUL-terminated name; a new descriptor this test owns.
        let memfd = unsafe { libc::memfd_create(c"gate-own-view".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(memfd >= 0);
        let held = std::fs::File::create(dir.path().join("held")).unwrap();
        std::fs::remove_file(dir.path().join("held")).unwrap();
        let inet = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (unix, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let proc = std::fs::File::open("/proc/self/status").unwrap();
        // The template held `held`, an unnamed file, at the cut.
        let inherited = unnamed_files(&[held.as_raw_fd()]);
        assert_eq!(inherited.len(), 1);
        let gate = TimelineGate::new(OwnProcess::default(), RefuseShared);
        let copy = CopyContext {
            index: 0,
            outputs: HashSet::new(),
            inherited_unnamed: inherited,
        };
        // Each descriptor named as the KVM backend names it: its own link.
        let own = |fd: RawFd| read_link(fd).map_or(Link::Closed, Link::Text);
        let kind = |fd: RawFd| gate.classify(&copy, fd, &own(fd)).0;
        assert_eq!(kind(named.as_raw_fd()), Kind::SharedFile);
        assert_eq!(kind(held.as_raw_fd()), Kind::SharedFile);
        assert_eq!(kind(memfd), Kind::PrivateFile);
        assert_eq!(kind(proc.as_raw_fd()), Kind::PrivateFile);
        assert_eq!(kind(unix.as_raw_fd()), Kind::Other);
        // A number far above anything this test opened.
        assert_eq!(kind(1 << 20), Kind::Closed);
        // The socket is found once, then through the descriptor kept for it.
        let local = inet.local_addr().unwrap();
        for _ in 0..2 {
            assert_eq!(
                gate.classify(&copy, inet.as_raw_fd(), &own(inet.as_raw_fd())),
                (Kind::InetSocket, format!("{local} (no peer)"))
            );
        }
        // Once the socket is closed, no descriptor reads its text.
        let text = own(inet.as_raw_fd());
        drop(inet);
        assert_eq!(gate.classify(&copy, 99, &text).0, Kind::Unknown);
        assert_eq!(gate.classify(&copy, 99, &Link::Unknown).0, Kind::Unknown);
        // Paths resolve as this process's calls resolve them.
        assert!(matches!(
            gate.view.resolve(&dir.path().join("named")),
            Resolved::Found { procfs: false, .. }
        ));
        assert!(matches!(
            gate.view.resolve(&dir.path().join("missing")),
            Resolved::Missing
        ));
        assert!(matches!(
            gate.view.resolve(Path::new("/proc/self/status")),
            Resolved::Found { procfs: true, .. }
        ));
        // SAFETY: closes the memfd this test opened.
        unsafe { libc::close(memfd) };
    }

    #[test]
    fn a_shared_file_mapping_is_found_in_a_memory_map() {
        let maps = "\
7f0000000000-7f0000001000 rw-p 00000000 00:00 0
7f0000001000-7f0000002000 rw-s 00000000 00:01 12 /memfd:arena (deleted)
7f0000002000-7f0000003000 rw-s 00000000 00:01 13 /dev/zero (deleted)
7f0000003000-7f0000004000 r--p 00000000 08:01 14 /usr/lib/libc.so.6
7f0000004000-7f0000005000 rw-s 00000000 00:00 15 anon_inode:kvm-vcpu:0
";
        assert_eq!(shared_file_mapping(maps), None);
        let with_file =
            format!("{maps}7f0000005000-7f0000006000 r--s 00000000 08:01 16 /data/my file.db\n");
        assert_eq!(
            shared_file_mapping(&with_file).as_deref(),
            Some("/data/my file.db")
        );
    }
}
