/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod fs;
mod mmap;
mod network;
mod random;
mod time;

use std::collections::HashSet;
use std::fs as stdfs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use reverie::Errno;
use reverie::Error;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Pid;
use reverie::Rdtsc;
use reverie::RdtscResult;
use reverie::Subscription;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::Close;
use reverie::syscalls::Fcntl;
use reverie::syscalls::FcntlCmd;
use reverie::syscalls::Lseek;
use reverie::syscalls::OFlag;
use reverie::syscalls::Openat;
use reverie::syscalls::ReadAddr;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use reverie::syscalls::Whence;
use serde::Deserialize;
use serde::Serialize;

use crate::event::Event;
use crate::event::ExecDependency;
use crate::event::ExecDescriptor;
use crate::event::ExecDescriptorAlias;
use crate::event::ExecEvent;
use crate::event::ExecImage;
use crate::event::ExecMaterialization;
use crate::event::ExecMaterializationBase;
use crate::event::ExecRequest;
use crate::event::ExecTarget;
use crate::event::MkdirEvent;
use crate::event::OpenEvent;
use crate::event::OpenMaterialization;
use crate::event::ReplayFdKind;
use crate::event::SyscallEvent;
use crate::event_stream::ChildEventStreamIds;
use crate::event_stream::DebugEvent;
use crate::event_stream::EventStreamId;
use crate::event_stream::EventWriter;

const MAX_EXEC_DEPENDENCY_DEPTH: usize = 5;

#[derive(Default, Serialize, Deserialize)]
pub struct RecorderThreadState {
    events: EventWriter,
    stream_id: EventStreamId,
    child_stream_ids: ChildEventStreamIds,
    pending_exec: Option<PreparedExec>,
    bootstrapped: bool,
}

impl RecorderThreadState {
    fn push_event(&mut self, event: Event) -> Result<(), bincode::error::EncodeError> {
        self.events.push_event(event)
    }

    fn push_debug_event(&mut self, event: DebugEvent) -> Result<(), bincode::error::EncodeError> {
        self.events.push_debug_event(event)
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum PreparedExec {
    Ready(ExecEvent),
    Unrepresentable(String),
}

struct ExecDependencyContext<'a> {
    pid: Pid,
    root: &'a OwnedFd,
    visited: &'a mut HashSet<(libc::dev_t, libc::ino_t)>,
    dependencies: &'a mut Vec<ExecDependency>,
}

fn exec_request_uses_live_magic_path(path: &[u8]) -> bool {
    [
        b"/proc/self/".as_slice(),
        b"/proc/thread-self/".as_slice(),
        b"/dev/fd/".as_slice(),
    ]
    .into_iter()
    .any(|prefix| path.starts_with(prefix))
}

fn parse_decimal_fd(bytes: &[u8]) -> Option<libc::c_int> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0_i32, |value, byte| {
        byte.is_ascii_digit()
            .then_some(())
            .and_then(|()| value.checked_mul(10))
            .and_then(|value| value.checked_add(i32::from(*byte - b'0')))
    })
}

fn exec_request_descriptor_fd(request: &ExecRequest) -> Option<libc::c_int> {
    if request.path.is_empty() && request.flags & libc::AT_EMPTY_PATH != 0 {
        return Some(request.dirfd);
    }
    [
        b"/proc/self/fd/".as_slice(),
        b"/proc/thread-self/fd/".as_slice(),
        b"/dev/fd/".as_slice(),
    ]
    .into_iter()
    .find_map(|prefix| request.path.strip_prefix(prefix).and_then(parse_decimal_fd))
}

fn split_fd_alias(bytes: &[u8]) -> Option<(libc::c_int, Option<&[u8]>)> {
    match bytes.iter().position(|byte| *byte == b'/') {
        Some(index) => Some((
            parse_decimal_fd(&bytes[..index])?,
            Some(&bytes[index + 1..]),
        )),
        None => Some((parse_decimal_fd(bytes)?, None)),
    }
}

fn exec_magic_materialization(request: &ExecRequest) -> Option<(ExecMaterializationBase, Vec<u8>)> {
    for prefix in [
        b"/proc/self/root/".as_slice(),
        b"/proc/thread-self/root/".as_slice(),
    ] {
        if let Some(path) = request.path.strip_prefix(prefix) {
            return Some((ExecMaterializationBase::Root, path.to_vec()));
        }
    }
    for prefix in [
        b"/proc/self/cwd/".as_slice(),
        b"/proc/thread-self/cwd/".as_slice(),
    ] {
        if let Some(path) = request.path.strip_prefix(prefix) {
            return Some((ExecMaterializationBase::Cwd, path.to_vec()));
        }
    }
    for prefix in [
        b"/proc/self/fd/".as_slice(),
        b"/proc/thread-self/fd/".as_slice(),
        b"/dev/fd/".as_slice(),
    ] {
        let Some(rest) = request.path.strip_prefix(prefix) else {
            continue;
        };
        let (fd, suffix) = split_fd_alias(rest)?;
        if let Some(path) = suffix {
            return Some((ExecMaterializationBase::DirectoryFd(fd), path.to_vec()));
        }
    }
    None
}

fn exec_request_is_live_executable(request: &ExecRequest) -> bool {
    matches!(
        request.path.as_slice(),
        b"/proc/self/exe" | b"/proc/thread-self/exe"
    )
}

fn same_guest_open_file_description(
    pid: Pid,
    left: libc::c_int,
    right: libc::c_int,
) -> io::Result<bool> {
    const KCMP_FILE: libc::c_int = 0;
    let comparison = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid.as_raw(),
            pid.as_raw(),
            KCMP_FILE,
            left,
            right,
        )
    };
    if comparison == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(comparison == 0)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Serialize,
    Deserialize,
    Eq,
    Ord,
    PartialEq,
    PartialOrd
)]
struct OutputIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Eq, PartialEq)]
enum MkdirProbeDisposition {
    Directory(i64),
    NotDirectory,
}

fn classify_mkdir_probe(result: Result<i64, Errno>) -> Result<MkdirProbeDisposition, Errno> {
    match result {
        Ok(fd) => Ok(MkdirProbeDisposition::Directory(fd)),
        Err(Errno::ENOENT | Errno::ENOTDIR | Errno::ELOOP) => {
            Ok(MkdirProbeDisposition::NotDirectory)
        }
        Err(error) => Err(error),
    }
}

fn require_mkdir_probe_closed(result: Result<i64, Errno>) -> Result<(), Errno> {
    match result {
        Ok(0) => Ok(()),
        Ok(_) => Err(Errno::EIO),
        Err(error) => Err(error),
    }
}

impl OutputIdentity {
    fn for_fd(pid: Pid, fd: i32) -> Option<Self> {
        let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
        Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn matches(&self, metadata: &std::fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

fn duplicate_regular_output(pid: Pid, fd: libc::c_int) -> Option<std::os::fd::OwnedFd> {
    let metadata = std::fs::metadata(format!("/proc/{}/fd/{fd}", pid.as_raw())).ok()?;
    metadata
        .file_type()
        .is_file()
        .then(|| crate::fd::duplicate_guest_fd(pid, fd).ok())
        .flatten()
}
fn guest_has_open_file_description(pid: Pid, target: &std::os::fd::OwnedFd) -> bool {
    let entries = match std::fs::read_dir(format!("/proc/{}/fd", pid.as_raw())) {
        Ok(entries) => entries,
        Err(_) => return true,
    };
    let mut compared = false;
    let mut saw_any = false;
    for entry in entries.flatten() {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::c_int>().ok())
        else {
            continue;
        };
        saw_any = true;
        let Ok(candidate) = crate::fd::duplicate_guest_fd(pid, fd) else {
            continue;
        };
        match crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd()) {
            Ok(true) => return true,
            Ok(false) => compared = true,
            Err(error) => tracing::debug!(
                %error,
                fd,
                "could not compare guest fd while releasing captured output"
            ),
        }
    }
    saw_any && !compared
}

/// Syscalls which Detcore's shared network engine owns for new recordings.
pub(crate) fn shared_network_syscall(syscall: &Syscall) -> bool {
    matches!(
        syscall,
        Syscall::Socket(_)
            | Syscall::Connect(_)
            | Syscall::Sendto(_)
            | Syscall::Sendmsg(_)
            | Syscall::Recvfrom(_)
            | Syscall::Recvmsg(_)
    )
}

/// A Reverie tool that records syscalls. Note that only syscalls that cannot be
/// made deterministic are forwarded to this tool.
#[derive(Serialize, Deserialize)]
pub struct Recorder {
    // TODO: We'll need to keep track of file descriptors here in order to
    // determine if a file descriptor should be fully recorded or simply cached
    // with a reflink. We can use `fstatfs` to figure out if the target file
    // system supports reflinks or not. All other file systems will need their
    // file interactions to be recorded on the syscall level.

    // Keep track of the data directory. Each thread uses this path to open its
    // event stream.
    data: PathBuf,
    /// Physical output endpoints inherited by the root guest.
    stdout: Option<OutputIdentity>,
    stderr: Option<OutputIdentity>,
    /// Stable regular-file OFDs used for offset aliasing checks.
    #[serde(skip)]
    stdout_ofd: Mutex<Option<std::os::fd::OwnedFd>>,
    #[serde(skip)]
    stderr_ofd: Mutex<Option<std::os::fd::OwnedFd>>,
    /// New-format network syscalls must be consumed by Detcore, not this
    /// per-thread legacy event stream.
    network_trace_owned_by_detcore: bool,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            data: PathBuf::new(),
            stdout: None,
            stderr: None,
            stdout_ofd: Mutex::new(None),
            stderr_ofd: Mutex::new(None),
            network_trace_owned_by_detcore: false,
        }
    }
}

impl detcore::RecordOrReplay for Recorder {
    async fn invoke_original_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Read,
    ) -> Result<reverie::InjectedReadResult, Error> {
        let debug = DebugEvent::new(call.into(), &guest.memory());
        let outcome = guest.inject_original_read(call).await;
        match &outcome {
            reverie::InjectedReadResult::Complete(result) => {
                guest.thread_state_mut().push_debug_event(debug).unwrap();
                self.record_read_result(guest, call, *result);
            }
            reverie::InjectedReadResult::Interrupted(ticket) => {
                let signal = ticket.signal().ok_or_else(|| {
                    Error::Tool(anyhow::anyhow!(
                        "interrupted Read ticket has no actual signal cause"
                    ))
                })?;
                // Keep the attempted Read before subsequent handler events.
                // No ReadV2 bytes or native errno exist for this attempt.
                guest.thread_state_mut().push_debug_event(debug).unwrap();
                self.record_event(
                    guest,
                    Ok(SyscallEvent::ReadInterrupted {
                        signal: signal as i32,
                    }),
                );
            }
            reverie::InjectedReadResult::RecordedInterruption(_) => {
                return Err(Error::Tool(anyhow::anyhow!(
                    "native recorder received a replay control"
                )));
            }
        }
        Ok(outcome)
    }
}

#[reverie::tool]
impl Tool for Recorder {
    type GlobalState = detcore::GlobalState;
    type ThreadState = RecorderThreadState;

    fn new(pid: Pid, cfg: &<Self::GlobalState as GlobalTool>::Config) -> Self {
        Self {
            data: cfg.replay_data.as_ref().unwrap().clone(),
            stdout: OutputIdentity::for_fd(pid, libc::STDOUT_FILENO),
            stderr: OutputIdentity::for_fd(pid, libc::STDERR_FILENO),
            stdout_ofd: Mutex::new(duplicate_regular_output(pid, libc::STDOUT_FILENO)),
            stderr_ofd: Mutex::new(duplicate_regular_output(pid, libc::STDERR_FILENO)),
            network_trace_owned_by_detcore: cfg.network_trace.uses_trace(),
        }
    }

    fn init_thread_state(
        &self,
        child: Tid,
        parent: Option<(Tid, &Self::ThreadState)>,
    ) -> Self::ThreadState {
        let stream_id = match parent {
            None => EventStreamId::root(),
            Some((_, state)) => state.child_stream_ids.next(&state.stream_id),
        };
        // We have to unwrap because there is no way to handle errors here.
        RecorderThreadState {
            events: EventWriter::create(&self.data, &stream_id).unwrap_or_else(|err| {
                panic!(
                    "Failed to create {:?} for recording thread {} (physical {}): {}",
                    self.data, stream_id, child, err
                )
            }),
            stream_id,
            child_stream_ids: ChildEventStreamIds::default(),
            pending_exec: None,
            bootstrapped: parent.is_some_and(|(_, state)| state.bootstrapped),
        }
    }

    fn subscriptions(_config: &<Self::GlobalState as GlobalTool>::Config) -> Subscription {
        let mut subscription = Subscription::none();
        subscription.rdtsc().cpuid().syscalls([
            Sysno::execve,
            Sysno::execveat,
            //Sysno::brk,
            Sysno::mprotect,
            //Sysno::arch_prctl,
            Sysno::read,
            Sysno::pread64,
            Sysno::readv,
            Sysno::preadv,
            Sysno::preadv2,
            Sysno::recvfrom,
            Sysno::recvmsg,
            Sysno::write,
            Sysno::pwrite64,
            Sysno::writev,
            Sysno::pwritev,
            Sysno::pwritev2,
            Sysno::access,
            Sysno::lseek,
            Sysno::stat,
            Sysno::fstat,
            Sysno::lstat,
            Sysno::newfstatat,
            Sysno::statfs,
            Sysno::fstatfs,
            Sysno::statx,
            Sysno::getdents,
            Sysno::getdents64,
            Sysno::mmap,
            //Sysno::munmap,
            Sysno::open,
            Sysno::openat,
            Sysno::close,
            Sysno::openat2,
            Sysno::mkdirat,
            Sysno::mknodat,
            Sysno::fchownat,
            Sysno::linkat,
            Sysno::renameat,
            Sysno::renameat2,
            Sysno::symlinkat,
            Sysno::fchmodat,
            Sysno::utimensat,
            Sysno::fchdir,
            Sysno::close_range,
            Sysno::fadvise64,
            Sysno::flock,
            Sysno::ftruncate,
            Sysno::dup,
            Sysno::dup2,
            Sysno::dup3,
            Sysno::ioctl,
            Sysno::socket,
            Sysno::pidfd_getfd,
            Sysno::clock_gettime,
            Sysno::gettimeofday,
            Sysno::settimeofday,
            Sysno::time,
            Sysno::setsockopt,
            Sysno::fcntl,
            Sysno::connect,
            Sysno::sendto,
            Sysno::sendmsg,
            Sysno::poll,
            Sysno::ppoll,
            Sysno::epoll_wait,
            Sysno::getsockopt,
            Sysno::getpeername,
            Sysno::getsockname,
            Sysno::getrandom,
            Sysno::readlink,
            Sysno::mkdir,
            Sysno::unlink,
            Sysno::unlinkat,
        ]);

        subscription
    }

    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        if self.network_trace_owned_by_detcore && shared_network_syscall(&syscall) {
            return Err(Error::Tool(anyhow::anyhow!(
                "shared network engine forwarded a network syscall to the legacy Recorder event stream"
            )));
        }
        self.record_raw_syscall(guest, syscall);

        Ok(match syscall {
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Execve(_) | Syscall::Execveat(_) => self.handle_exec(guest, syscall).await,
            Syscall::Brk(_) => self.let_through(guest, syscall).await,
            Syscall::Mprotect(_) => self.let_through(guest, syscall).await,
            Syscall::ArchPrctl(_) => {
                // To properly handle arch_prctl, we should prevent calls from
                // using ARCH_SET_CPUID since we already do that for the
                // tracees. However, it is rare for programs to use
                // ARCH_SET_CPUID. For all other arch_prctl subfunctions, we
                // should let it through.
                self.let_through(guest, syscall).await
            }
            Syscall::Read(syscall) => self.handle_read(guest, syscall).await,
            Syscall::Pread64(syscall) => self.handle_pread64(guest, syscall).await,
            Syscall::Readv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len(),
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Preadv2(syscall) => {
                self.handle_readv_family(
                    guest,
                    syscall.iov().map(|a| a.as_raw()),
                    syscall.iov_len() as usize,
                    syscall.fd(),
                    syscall.into(),
                )
                .await
            }
            Syscall::Recvfrom(syscall) => self.handle_recvfrom(guest, syscall).await,
            Syscall::Recvmsg(syscall) => self.handle_recvmsg(guest, syscall).await,
            Syscall::Write(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwrite64(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Writev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Pwritev2(syscall) => self.handle_write_family(guest, syscall.into()).await,
            Syscall::Access(_) => self.handle_simple(guest, syscall).await,
            Syscall::Lseek(_) => self.handle_simple(guest, syscall).await,
            Syscall::Stat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Fstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Lstat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Newfstatat(syscall) => self.handle_stat_family(guest, syscall.into()).await,
            Syscall::Statfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Fstatfs(syscall) => {
                self.handle_statfs(guest, syscall.into(), syscall.buf())
                    .await
            }
            Syscall::Statx(syscall) => self.handle_statx(guest, syscall).await,
            Syscall::Getdents(syscall) => self.handle_getdents(guest, syscall).await,
            Syscall::Getdents64(syscall) => self.handle_getdents64(guest, syscall).await,
            Syscall::Mmap(syscall) => self.handle_mmap(guest, syscall).await,
            Syscall::Munmap(_) => self.let_through(guest, syscall).await,
            Syscall::Open(_) | Syscall::Openat(_) => self.handle_open(guest, syscall).await,
            Syscall::Close(_) => self.handle_fd_table_mutation(guest, syscall).await,
            Syscall::Openat2(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdirat(_) => self.handle_mkdir(guest, syscall).await,
            Syscall::Mknodat(_)
            | Syscall::Fchownat(_)
            | Syscall::Linkat(_)
            | Syscall::Renameat(_)
            | Syscall::Renameat2(_)
            | Syscall::Symlinkat(_)
            | Syscall::Fchmodat(_)
            | Syscall::Utimensat(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fchdir(_) => self.handle_simple(guest, syscall).await,
            Syscall::Fadvise64(_) => self.handle_simple(guest, syscall).await,
            Syscall::Flock(_) => self.handle_simple(guest, syscall).await,
            Syscall::Ftruncate(syscall) => self.handle_ftruncate(guest, syscall).await,
            Syscall::Dup(_) => self.handle_simple(guest, syscall).await,
            Syscall::Dup2(_) | Syscall::Dup3(_) => {
                self.handle_fd_table_mutation(guest, syscall).await
            }
            Syscall::Ioctl(syscall) => self.handle_ioctl(guest, syscall).await,
            Syscall::Socket(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-979): pidfd_open is an input-only,
            // fd-returning syscall (like socket): record its return value so
            // the pidfd allocation is captured and can be recreated/validated
            // on replay. Without this arm it fell through to live injection and
            // the fd side effect was neither recorded nor replayed.
            Syscall::PidfdOpen(_) => self.handle_simple(guest, syscall).await,
            Syscall::ClockGettime(syscall) => self.handle_clock_gettime(guest, syscall).await,
            Syscall::Gettimeofday(syscall) => self.handle_gettimeofday(guest, syscall).await,
            Syscall::Settimeofday(_) => self.handle_simple(guest, syscall).await,
            Syscall::Time(syscall) => self.handle_time(guest, syscall).await,
            Syscall::Setsockopt(_) => self.handle_simple(guest, syscall).await,
            // FIXME: Not all fcntl cases are simple.
            Syscall::Fcntl(_) => self.handle_simple(guest, syscall).await,
            Syscall::Connect(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendto(_) => self.handle_simple(guest, syscall).await,
            Syscall::Sendmsg(_) => self.handle_simple(guest, syscall).await,
            Syscall::Poll(syscall) => self.handle_poll(guest, syscall).await,
            Syscall::Ppoll(syscall) => self.handle_ppoll(guest, syscall).await,
            Syscall::EpollWait(syscall) => self.handle_epoll_wait(guest, syscall).await,
            Syscall::Getsockopt(syscall) => self.handle_sockopt_family(guest, syscall.into()).await,
            Syscall::Getpeername(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getsockname(syscall) => {
                self.handle_sockopt_family(guest, syscall.into()).await
            }
            Syscall::Getrandom(syscall) => self.handle_getrandom(guest, syscall).await,
            Syscall::Readlink(syscall) => self.handle_readlink(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Mkdir(_) => self.handle_mkdir(guest, syscall).await,
            Syscall::Unlink(_) => self.handle_simple(guest, syscall).await,
            Syscall::Unlinkat(_) => self.handle_simple(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            Syscall::Other(Sysno::close_range, _) => self.handle_close_range(guest, syscall).await,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(#2407): Preserve pidfd_getfd's returned
            // descriptor and errno in the record stream.
            Syscall::Other(Sysno::pidfd_getfd, _) => {
                self.handle_fd_table_mutation(guest, syscall).await
            }
            unsupported => return Ok(guest.inject(unsupported).await?),
        }?)
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_post_exec<G: Guest<Self>>(&self, guest: &mut G) -> Result<(), Errno> {
        let result = match guest.thread_state_mut().pending_exec.take() {
            Some(PreparedExec::Ready(event)) => {
                self.record_event(guest, Ok(SyscallEvent::Exec(event)));
                Ok(())
            }
            Some(PreparedExec::Unrepresentable(reason)) => {
                tracing::error!(
                    thread = %guest.tid(),
                    %reason,
                    "recording cannot represent successful exec"
                );
                Err(Errno::EIO)
            }
            None => Ok(()),
        };
        guest.thread_state_mut().bootstrapped = true;
        self.release_unreferenced_outputs(guest.pid());
        result
    }

    async fn handle_rdtsc_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: Rdtsc,
    ) -> Result<RdtscResult, Errno> {
        let result = RdtscResult::new(request);
        self.record_event(guest, Ok(SyscallEvent::Rdtsc(result)));
        Ok(result)
    }
}

impl Recorder {
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit recorded physical-open classification.
    async fn handle_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let materialize = result
            .ok()
            .and_then(|fd| {
                std::fs::metadata(format!("/proc/{}/fd/{fd}", guest.pid().as_raw())).ok()
            })
            .map_or(OpenMaterialization::None, |metadata| {
                if metadata.file_type().is_dir() {
                    OpenMaterialization::Directory
                } else if metadata.file_type().is_file() {
                    OpenMaterialization::RegularFile
                } else {
                    OpenMaterialization::None
                }
            });
        self.record_event(
            guest,
            Ok(SyscallEvent::Open(OpenEvent {
                result,
                materialize,
            })),
        );
        result
    }

    // TODO-HUMAN-REVIEW(#2370)
    /// Records enough information to distinguish the two guest-visible
    /// `EEXIST` cases: an already-existing directory needs to be reconstructed
    /// in the fresh replay root, while a file or symlink must remain an error
    /// without directory materialization.
    async fn handle_mkdir<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        let existing_directory = if result == Err(Errno::EEXIST) {
            self.mkdir_target_is_directory(guest, syscall)
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "could not classify recorded mkdir EEXIST target without changing its meaning: {error}"
                    )
                })
        } else {
            false
        };
        self.record_event(
            guest,
            Ok(SyscallEvent::Mkdir(MkdirEvent {
                result,
                existing_directory,
            })),
        );
        result
    }

    /// Probes the final pathname component without following a symlink. `O_PATH`
    /// avoids adding read/search-permission requirements that the original
    /// `mkdir` did not have. The temporary descriptor is closed before the
    /// guest resumes, so the probe has no persistent fd-table side effect.
    async fn mkdir_target_is_directory<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<bool, Errno> {
        let result = match syscall {
            Syscall::Mkdir(call) => {
                let path = call.path().ok_or(Errno::EFAULT)?;
                guest
                    .inject_with_retry(
                        Openat::new()
                            .with_dirfd(libc::AT_FDCWD)
                            .with_path(Some(path))
                            .with_flags(
                                OFlag::O_PATH
                                    | OFlag::O_DIRECTORY
                                    | OFlag::O_NOFOLLOW
                                    | OFlag::O_CLOEXEC,
                            ),
                    )
                    .await
            }
            Syscall::Mkdirat(call) => {
                let path = call.path().ok_or(Errno::EFAULT)?;
                guest
                    .inject_with_retry(
                        Openat::new()
                            .with_dirfd(call.dirfd())
                            .with_path(Some(path))
                            .with_flags(
                                OFlag::O_PATH
                                    | OFlag::O_DIRECTORY
                                    | OFlag::O_NOFOLLOW
                                    | OFlag::O_CLOEXEC,
                            ),
                    )
                    .await
            }
            _ => unreachable!("mkdir directory probe called for {syscall:?}"),
        };
        match classify_mkdir_probe(result)? {
            MkdirProbeDisposition::Directory(fd) => {
                let closed = guest
                    .inject_with_retry(Close::new().with_fd(fd as libc::c_int))
                    .await;
                require_mkdir_probe_closed(closed)?;
                Ok(true)
            }
            MkdirProbeDisposition::NotDirectory => Ok(false),
        }
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#662): Audit guest FD replay classification.
    fn fd_replay_kind(&self, pid: Pid, fd: libc::c_int) -> ReplayFdKind {
        let path = format!("/proc/{}/fd/{fd}", pid.as_raw());
        if std::fs::read_link(&path)
            .ok()
            .is_some_and(|target| target == std::path::Path::new("anon_inode:[eventfd]"))
        {
            return ReplayFdKind::Eventfd;
        }

        if std::fs::metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
        {
            ReplayFdKind::RegularFile
        } else {
            ReplayFdKind::None
        }
    }

    fn epoll_requires_replay_kernel_side_effect(&self, pid: Pid, fd: libc::c_int) -> bool {
        let Ok(fdinfo) = std::fs::read_to_string(format!("/proc/{}/fdinfo/{fd}", pid.as_raw()))
        else {
            return false;
        };
        let targets = fdinfo.lines().filter_map(|line| {
            line.strip_prefix("tfd:")?
                .split_whitespace()
                .next()?
                .parse::<libc::c_int>()
                .ok()
        });
        let mut saw_target = false;
        for target in targets {
            saw_target = true;
            if self.fd_replay_kind(pid, target) == ReplayFdKind::None {
                return false;
            }
        }
        saw_target
    }

    pub(super) fn output_ofd_matches(
        &self,
        output_fd: libc::c_int,
        candidate: &std::os::fd::OwnedFd,
    ) -> bool {
        let output = match output_fd {
            libc::STDOUT_FILENO => &self.stdout_ofd,
            libc::STDERR_FILENO => &self.stderr_ofd,
            _ => return false,
        };
        let output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        output.as_ref().is_some_and(|target| {
            crate::fd::same_open_file_description(candidate.as_raw_fd(), target.as_raw_fd())
                .unwrap_or(false)
        })
    }

    fn release_unreferenced_output(output: &Mutex<Option<std::os::fd::OwnedFd>>, pid: Pid) {
        let mut output = output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if output
            .as_ref()
            .is_some_and(|target| !guest_has_open_file_description(pid, target))
        {
            output.take();
        }
    }

    fn release_unreferenced_outputs(&self, pid: Pid) {
        Self::release_unreferenced_output(&self.stdout_ofd, pid);
        Self::release_unreferenced_output(&self.stderr_ofd, pid);
    }

    async fn handle_fd_table_mutation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;
        self.release_unreferenced_outputs(guest.pid());
        self.record_event(guest, result.map(SyscallEvent::Return));
        result
    }

    // TODO-HUMAN-REVIEW(#557): Audit close_range fd-table replay semantics.
    async fn handle_close_range<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let Syscall::Other(Sysno::close_range, args) = syscall else {
            unreachable!("handle_close_range called for {syscall:?}");
        };

        if args.arg2 & libc::CLOSE_RANGE_UNSHARE as usize != 0 {
            let result = Err(Errno::ENOSYS);
            self.record_event(guest, result.map(SyscallEvent::Return));
            return result;
        }

        self.handle_fd_table_mutation(guest, syscall).await
    }

    fn record_raw_syscall<G: Guest<Self>>(&self, guest: &mut G, syscall: Syscall) {
        let debug_event = DebugEvent::new(syscall, &guest.memory());
        guest
            .thread_state_mut()
            .push_debug_event(debug_event)
            .unwrap();
    }

    // TODO-HUMAN-REVIEW(#2370)
    async fn handle_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let prepared = match self.prepare_exec(guest, syscall).await {
            Ok(event) => PreparedExec::Ready(event),
            Err(error) => PreparedExec::Unrepresentable(error.to_string()),
        };
        assert!(
            guest
                .thread_state_mut()
                .pending_exec
                .replace(prepared)
                .is_none(),
            "nested pending exec on thread {}",
            guest.tid()
        );

        // A successful exec never returns here. Its pending event is committed
        // by handle_post_exec after Linux has replaced the image.
        let initial_root_exec = guest.is_root_thread() && !guest.thread_state().bootstrapped;
        let result = guest.inject(syscall).await;
        let pending = guest.thread_state_mut().pending_exec.take();
        assert!(pending.is_some(), "failed exec lost its pending event");
        let error = result.expect_err("successful exec unexpectedly returned to syscall handler");
        if initial_root_exec && error == Errno::ENOEXEC {
            panic!(
                "record/replay does not support execvpe shell fallback for an initial executable without a recognized format; add an explicit shebang"
            );
        }
        self.record_event(guest, Err(error));
        Err(error)
    }

    async fn prepare_exec<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> io::Result<ExecEvent> {
        let call = match syscall {
            Syscall::Execve(call) => call.into(),
            Syscall::Execveat(call) => call,
            _ => unreachable!("exec preparation called for {syscall:?}"),
        };
        let path_ptr = call
            .path()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
        let path = path_ptr
            .read(&guest.memory())
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let request = ExecRequest {
            dirfd: call.dirfd(),
            path: path.as_os_str().as_bytes().to_vec(),
            flags: call.flags(),
        };
        let allowed_flags = libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW;
        if request.flags & !allowed_flags != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported execveat flags {:#x}", request.flags),
            ));
        }

        let pinned = if path.as_os_str().is_empty() {
            if request.flags & libc::AT_EMPTY_PATH == 0 {
                return Err(io::Error::from_raw_os_error(libc::ENOENT));
            }
            crate::record_replay_path::open_process_fd(guest.pid(), request.dirfd)?
        } else {
            self.pin_exec_path(guest, request.dirfd, path_ptr, request.flags)
                .await?
        };
        let pinned_identity = crate::record_replay_path::file_identity(pinned.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(pinned_identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "successful exec target would not be a regular file",
            ));
        }

        let root = crate::record_replay_path::open_process_root(guest.pid())?;
        let target = if let Some(fd) = exec_request_descriptor_fd(&request) {
            ExecTarget::RestoreDescriptor(
                self.capture_exec_descriptor(guest, fd, pinned_identity)
                    .await?,
            )
        } else if exec_request_is_live_executable(&request) {
            ExecTarget::VerifyLive
        } else {
            let (base, materialization_path) =
                if let Some(materialization) = exec_magic_materialization(&request) {
                    materialization
                } else if exec_request_uses_live_magic_path(&request.path) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "unsupported process-relative executable path {:?}",
                            path.as_os_str()
                        ),
                    ));
                } else if path.is_absolute() {
                    (
                        ExecMaterializationBase::Root,
                        path.as_os_str().as_bytes().to_vec(),
                    )
                } else if request.dirfd == libc::AT_FDCWD {
                    (
                        ExecMaterializationBase::Cwd,
                        path.as_os_str().as_bytes().to_vec(),
                    )
                } else {
                    (
                        ExecMaterializationBase::DirectoryFd(request.dirfd),
                        path.as_os_str().as_bytes().to_vec(),
                    )
                };
            let start = match &base {
                ExecMaterializationBase::Root => {
                    crate::record_replay_path::open_process_root(guest.pid())?
                }
                ExecMaterializationBase::Cwd => {
                    crate::record_replay_path::open_process_cwd(guest.pid())?
                }
                ExecMaterializationBase::DirectoryFd(fd) => {
                    crate::record_replay_path::open_process_directory_fd(guest.pid(), *fd)?
                }
            };
            let materialization_path =
                PathBuf::from(std::ffi::OsString::from_vec(materialization_path));
            let resolved = crate::record_replay_path::resolve_existing_path(
                &root,
                &start,
                &materialization_path,
                request.flags & libc::AT_SYMLINK_NOFOLLOW != 0,
            )?;
            let resolved_identity =
                crate::record_replay_path::file_identity(resolved.object.as_raw_fd())?;
            if resolved_identity.device != pinned_identity.device
                || resolved_identity.inode != pinned_identity.inode
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "descriptor path walk selected a different exec object",
                ));
            }
            ExecTarget::Materialize(ExecMaterialization {
                base,
                path: materialization_path.as_os_str().as_bytes().to_vec(),
                symlinks: resolved.symlinks,
            })
        };

        let (executable, snapshot) = self.snapshot_exec_object(&pinned)?;
        let mut dependencies = Vec::new();
        let mut visited = HashSet::new();
        let mut dependency_context = ExecDependencyContext {
            pid: guest.pid(),
            root: &root,
            visited: &mut visited,
            dependencies: &mut dependencies,
        };
        self.record_exec_dependencies(&snapshot, 0, &mut dependency_context)?;

        Ok(ExecEvent {
            request,
            executable,
            target,
            dependencies,
        })
    }

    async fn capture_exec_descriptor<G: Guest<Self>>(
        &self,
        guest: &mut G,
        target_fd: libc::c_int,
        target_identity: crate::record_replay_path::FileIdentity,
    ) -> io::Result<ExecDescriptor> {
        let status_flags = guest
            .inject_with_retry(Fcntl::new().with_fd(target_fd).with_cmd(FcntlCmd::F_GETFL))
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
            as libc::c_int;
        if status_flags & libc::O_PATH == 0 && status_flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "successful descriptor exec used unsupported access flags {status_flags:#x}"
                ),
            ));
        }

        let offset = match guest
            .inject_with_retry(
                Lseek::new()
                    .with_fd(target_fd)
                    .with_offset(0)
                    .with_whence(Whence::SEEK_CUR),
            )
            .await
        {
            Ok(offset) => Some(offset as libc::off_t),
            Err(Errno::EBADF | Errno::ESPIPE) => None,
            Err(error) => return Err(io::Error::from_raw_os_error(error.into_raw())),
        };

        let mut candidate_fds = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/{}/fd", guest.pid().as_raw()))? {
            let entry = entry?;
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            else {
                continue;
            };
            let metadata = match std::fs::metadata(entry.path()) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.dev() == target_identity.device && metadata.ino() == target_identity.inode {
                candidate_fds.push(fd);
            }
        }
        candidate_fds.sort_unstable();

        let mut aliases = Vec::new();
        for fd in candidate_fds {
            if fd != target_fd && !same_guest_open_file_description(guest.pid(), target_fd, fd)? {
                continue;
            }
            let descriptor_flags = guest
                .inject_with_retry(Fcntl::new().with_fd(fd).with_cmd(FcntlCmd::F_GETFD))
                .await
                .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?
                as libc::c_int;
            aliases.push(ExecDescriptorAlias {
                fd,
                descriptor_flags,
            });
        }
        if !aliases.iter().any(|alias| alias.fd == target_fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec descriptor disappeared while its state was captured",
            ));
        }

        Ok(ExecDescriptor {
            target_fd,
            status_flags,
            offset,
            aliases,
        })
    }

    async fn pin_exec_path<G: Guest<Self>>(
        &self,
        guest: &mut G,
        dirfd: libc::c_int,
        path: reverie::syscalls::PathPtr<'_>,
        flags: libc::c_int,
    ) -> io::Result<OwnedFd> {
        let mut open_flags = OFlag::O_PATH | OFlag::O_CLOEXEC;
        if flags & libc::AT_SYMLINK_NOFOLLOW != 0 {
            open_flags |= OFlag::O_NOFOLLOW;
        }
        let temporary = guest
            .inject_with_retry(
                Openat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_flags(open_flags),
            )
            .await
            .map_err(|error| io::Error::from_raw_os_error(error.into_raw()))?;
        let pinned =
            crate::record_replay_path::open_process_fd(guest.pid(), temporary as libc::c_int);
        let close = guest
            .inject_with_retry(Close::new().with_fd(temporary as libc::c_int))
            .await;
        if close != Ok(0) {
            return Err(io::Error::other(format!(
                "could not close temporary exec descriptor: {close:?}"
            )));
        }
        pinned
    }

    fn snapshot_exec_object(&self, object: &OwnedFd) -> io::Result<(ExecImage, PathBuf)> {
        let before = crate::record_replay_path::file_identity(object.as_raw_fd())?;
        if !crate::record_replay_path::is_regular_file(before) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec snapshot source is not a regular file",
            ));
        }
        let mut input = crate::record_replay_path::open_readable_fd(object.as_raw_fd())?;
        let snapshots = self.data.join(crate::consts::EXEC_FILES_NAME);
        stdfs::create_dir_all(&snapshots)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&snapshots)?;
        io::copy(&mut input, &mut temporary)?;
        temporary.flush()?;
        let after = crate::record_replay_path::file_identity(object.as_raw_fd())?;
        if before != after {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec source changed while it was being snapshotted",
            ));
        }

        let digest = detcore::Digest::digest_path(temporary.path())?;
        let snapshot = snapshots.join(digest.to_string());
        match temporary.persist_noclobber(&snapshot) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                if detcore::Digest::digest_path(&snapshot)? != digest {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("recorded exec snapshot collision at {snapshot:?}"),
                    ));
                }
            }
            Err(error) => return Err(error.error),
        }

        Ok((
            ExecImage {
                digest,
                mode: before.mode & 0o7777,
            },
            snapshot,
        ))
    }

    fn record_exec_dependencies(
        &self,
        snapshot: &Path,
        depth: usize,
        context: &mut ExecDependencyContext<'_>,
    ) -> io::Result<()> {
        if depth > MAX_EXEC_DEPENDENCY_DEPTH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "too many executable interpreter levels",
            ));
        }
        let mut header = Vec::new();
        stdfs::File::open(snapshot)?
            .take(256)
            .read_to_end(&mut header)?;
        let Some(dependency) = (if let Some(shebang) = crate::Shebang::from_buf(&header) {
            Some(shebang.interpreter().to_path_buf())
        } else {
            crate::interp::elf_get_interp(snapshot)
        }) else {
            return Ok(());
        };
        let request = ExecRequest {
            dirfd: libc::AT_FDCWD,
            path: dependency.as_os_str().as_bytes().to_vec(),
            flags: 0,
        };
        if exec_request_descriptor_fd(&request).is_some()
            || exec_request_is_live_executable(&request)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "descriptor-relative executable interpreter is unsupported: {dependency:?}"
                ),
            ));
        }
        let (base, path) = if let Some(materialization) = exec_magic_materialization(&request) {
            materialization
        } else if exec_request_uses_live_magic_path(&request.path) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported process-relative executable interpreter: {dependency:?}"),
            ));
        } else if dependency.is_absolute() {
            (ExecMaterializationBase::Root, request.path)
        } else {
            (ExecMaterializationBase::Cwd, request.path)
        };
        let start = match &base {
            ExecMaterializationBase::Root => {
                crate::record_replay_path::open_process_root(context.pid)?
            }
            ExecMaterializationBase::Cwd => {
                crate::record_replay_path::open_process_cwd(context.pid)?
            }
            ExecMaterializationBase::DirectoryFd(fd) => {
                crate::record_replay_path::open_process_directory_fd(context.pid, *fd)?
            }
        };
        self.record_exec_dependency_from_start(&start, base, path, depth, context)
    }

    fn record_exec_dependency_from_start(
        &self,
        start: &OwnedFd,
        base: ExecMaterializationBase,
        path: Vec<u8>,
        depth: usize,
        context: &mut ExecDependencyContext<'_>,
    ) -> io::Result<()> {
        let dependency = PathBuf::from(std::ffi::OsString::from_vec(path.clone()));
        let resolved = crate::record_replay_path::resolve_existing_path(
            context.root,
            start,
            &dependency,
            false,
        )?;
        let identity = crate::record_replay_path::file_identity(resolved.object.as_raw_fd())?;
        if !context.visited.insert((identity.device, identity.inode)) {
            return Ok(());
        }
        let (image, dependency_snapshot) = self.snapshot_exec_object(&resolved.object)?;
        self.record_exec_dependencies(&dependency_snapshot, depth + 1, context)?;
        context.dependencies.push(ExecDependency {
            base,
            path,
            image,
            symlinks: resolved.symlinks,
        });
        Ok(())
    }

    fn record_event<G: Guest<Self>>(&self, guest: &mut G, event: Result<SyscallEvent, Errno>) {
        // Record the event.
        guest
            .thread_state_mut()
            .push_event(Event { event })
            // TODO: Log errors instead of panicking.
            .unwrap();
    }

    /// Called for syscalls to explicitly let through. This should only be called
    /// for syscalls that cannot be recorded and are necessary for the program to
    /// function correctly. Examples of syscalls that fall into this category are
    /// ones that help with memory management (e.g., `brk`, `mprotect`, `mmap`,
    /// or `munmap`) or process management (e.g., `fork`, `vfork`, `clone`).
    ///
    /// For these syscalls, we don't really need to record anything, but we
    /// record their arguments to detect any desynchronization.
    async fn let_through<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        guest.inject(syscall).await
    }

    /// Handles a syscall whose only value we care about is the return value
    /// (i.e., simple syscalls).
    ///
    /// For recording, this means we only record the return value of the syscall.
    /// For replay, this means we substitute the return value in lieu of actually
    /// performing the injection.
    ///
    /// The syscall must have two properties satisfied for this to be called:
    ///  1. The syscall must only have "input" arguments. That is, all arguments
    ///     must either be values or const pointers.
    ///  2. The execution of the program must not depend on anything else other
    ///     than the return value of the syscall. For example, `mmap` would violate
    ///     this rule since it affects later memory access.
    ///
    /// There are many syscalls who satisfy these two requirements.
    async fn handle_simple<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Errno> {
        let result = guest.inject(syscall).await;

        self.record_event(guest, result.map(SyscallEvent::Return));

        result
    }
}

#[cfg(test)]
mod exec_path_tests {
    use super::*;

    #[test]
    fn process_relative_magic_paths_are_classified_without_controller_lookup() {
        for path in [
            b"/proc/self/exe".as_slice(),
            b"/proc/thread-self/exe".as_slice(),
            b"/proc/self/fd/7".as_slice(),
            b"/dev/fd/7".as_slice(),
        ] {
            assert!(exec_request_uses_live_magic_path(path));
        }
        for path in [
            b"/proc/123/exe".as_slice(),
            b"/tmp/proc/self/exe".as_slice(),
            b"relative/proc/self/exe".as_slice(),
        ] {
            assert!(!exec_request_uses_live_magic_path(path));
        }
    }
}

#[cfg(test)]
mod mkdir_probe_tests {
    use super::*;

    #[test]
    fn resource_and_permission_failures_are_not_classified_as_non_directories() {
        for error in [Errno::EMFILE, Errno::ENFILE, Errno::EACCES, Errno::EPERM] {
            assert_eq!(classify_mkdir_probe(Err(error)), Err(error));
        }
    }

    #[test]
    fn file_and_symlink_probe_errors_are_not_directories() {
        for error in [Errno::ENOENT, Errno::ENOTDIR, Errno::ELOOP] {
            assert_eq!(
                classify_mkdir_probe(Err(error)),
                Ok(MkdirProbeDisposition::NotDirectory)
            );
        }
    }

    #[test]
    fn probe_close_must_succeed_exactly() {
        assert_eq!(require_mkdir_probe_closed(Ok(0)), Ok(()));
        assert_eq!(require_mkdir_probe_closed(Ok(1)), Err(Errno::EIO));
        assert_eq!(
            require_mkdir_probe_closed(Err(Errno::EINTR)),
            Err(Errno::EINTR)
        );
    }
}
#[cfg(test)]
mod original_file_delegate_tests {
    use std::os::fd::AsRawFd;

    use detcore::OriginalFileExecution;
    use detcore::RecordOrReplay;
    use reverie::GlobalRPC;
    use reverie::Never;
    use reverie::Stack;
    use reverie::TimerSchedule;
    use reverie::syscalls::Addr;
    use reverie::syscalls::AddrMut;
    use reverie::syscalls::LocalMemory;
    use reverie::syscalls::Read as SysRead;
    use reverie::syscalls::SyscallInfo;

    use super::*;

    // This Guest executes real test-owned F_GETFL and armed Read syscalls. It does not
    // synthesize Prepared/Returned or provider selection. These are delegate
    // stream/provenance controls, not ptrace/provider qualification.
    struct DelegateGuest<T: RecordOrReplay> {
        config: detcore::Config,
        thread: T::ThreadState,
        injections: usize,
        permit_injection: bool,
        read_operands: Option<(i32, usize, usize)>,
        interrupt_next_read: bool,
        recorded_interruption_waits: usize,
    }
    struct NoStack;
    struct NoStackGuard;
    impl Drop for NoStackGuard {
        fn drop(&mut self) {}
    }
    impl Stack for NoStack {
        type StackGuard = NoStackGuard;
        fn size(&self) -> usize {
            panic!("no stack")
        }
        fn capacity(&self) -> usize {
            panic!("no stack")
        }
        fn push<'s, V>(&mut self, _: V) -> Addr<'s, V> {
            panic!("no stack")
        }
        fn reserve<'s, V>(&mut self) -> AddrMut<'s, V> {
            panic!("no stack")
        }
        fn commit(self) -> Result<Self::StackGuard, Errno> {
            panic!("no stack")
        }
    }
    #[reverie::tool]
    impl<T: RecordOrReplay> GlobalRPC<detcore::GlobalState> for DelegateGuest<T> {
        async fn send_rpc(
            &self,
            _: <detcore::GlobalState as GlobalTool>::Request,
        ) -> <detcore::GlobalState as GlobalTool>::Response {
            panic!("delegate sent unexpected RPC")
        }
        fn config(&self) -> &detcore::Config {
            &self.config
        }
    }
    #[reverie::tool]
    impl<T: RecordOrReplay> Guest<T> for DelegateGuest<T> {
        type Memory = LocalMemory;
        type Stack = NoStack;
        fn tid(&self) -> Tid {
            Tid::from_raw(std::process::id() as i32)
        }
        fn pid(&self) -> Pid {
            Pid::from_raw(std::process::id() as i32)
        }
        fn ppid(&self) -> Option<Pid> {
            None
        }
        fn memory(&self) -> LocalMemory {
            LocalMemory::default()
        }
        fn thread_state(&self) -> &T::ThreadState {
            &self.thread
        }
        fn thread_state_mut(&mut self) -> &mut T::ThreadState {
            &mut self.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("no registers")
        }
        async fn stack(&mut self) -> NoStack {
            panic!("no stack")
        }
        async fn daemonize(&mut self) {
            panic!("no daemon")
        }
        async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
            assert!(self.permit_injection, "replay attempted a physical syscall");
            let (nr, args) = call.into_parts();
            if nr == Sysno::read {
                assert_eq!(
                    self.read_operands.take().expect("unarmed test read"),
                    (args.arg0 as i32, args.arg1, args.arg2)
                );
                self.injections += 1;
                // SAFETY: the armed tuple names the test-owned mapping, and
                // the sole producer queued at most three bytes (or fd is -1).
                let result = unsafe {
                    libc::read(args.arg0 as i32, args.arg1 as *mut libc::c_void, args.arg2)
                };
                return if result < 0 {
                    Err(Errno::new(
                        io::Error::last_os_error().raw_os_error().unwrap(),
                    ))
                } else {
                    Ok(result as i64)
                };
            }
            assert_eq!(nr, Sysno::fcntl);
            assert_eq!(args.arg1, libc::F_GETFL as usize);
            self.injections += 1;
            let result = unsafe { libc::fcntl(args.arg0 as i32, libc::F_GETFL) };
            if result < 0 {
                Err(Errno::new(
                    io::Error::last_os_error().raw_os_error().unwrap(),
                ))
            } else {
                Ok(i64::from(result))
            }
        }
        async fn inject_original_read(
            &mut self,
            call: reverie::syscalls::Read,
        ) -> reverie::InjectedReadResult {
            if std::mem::take(&mut self.interrupt_next_read) {
                // Explicit delegate input only. This fixture has no native
                // observation, Task or provider cancellation authority.
                reverie::InjectedReadResult::Interrupted(reverie::InterruptedSyscall::with_signal(
                    reverie::Signal::SIGUSR1,
                ))
            } else {
                reverie::InjectedReadResult::Complete(self.inject(call).await)
            }
        }
        async fn await_recorded_read_interruption(
            &mut self,
            _: reverie::syscalls::Read,
            signal: reverie::Signal,
        ) -> Result<reverie::InterruptedSyscall, Error> {
            // Delegate-only control. Real stopped-task custody is exercised
            // separately by interrupted_read_tests through actual ptrace.
            assert!(!self.permit_injection);
            self.recorded_interruption_waits += 1;
            assert_eq!(signal, reverie::Signal::SIGUSR1);
            Ok(reverie::InterruptedSyscall::with_signal(signal))
        }
        async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
            panic!("no tail injection")
        }
        fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("no timer")
        }
        fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
            panic!("no timer")
        }
        fn read_clock(&mut self) -> Result<u64, Error> {
            panic!("no clock")
        }
    }
    fn guest<T: RecordOrReplay>(
        tool: &T,
        config: &detcore::Config,
        permit_injection: bool,
    ) -> DelegateGuest<T> {
        DelegateGuest {
            config: config.clone(),
            thread: tool.init_thread_state(Tid::from_raw(std::process::id() as i32), None),
            injections: 0,
            permit_injection,
            read_operands: None,
            interrupt_next_read: false,
            recorded_interruption_waits: 0,
        }
    }
    async fn recorded_fixture() -> (
        tempfile::TempDir,
        detcore::Config,
        [Syscall; 2],
        [Result<i64, Errno>; 2],
    ) {
        let data = tempfile::tempdir().unwrap();
        let config = detcore::Config {
            replay_data: Some(data.path().to_path_buf()),
            ..Default::default()
        };
        let tool = Recorder::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut recording = guest(&tool, &config, true);
        let file = std::fs::File::open("/dev/null").unwrap();
        let calls = [
            Fcntl::new()
                .with_fd(file.as_raw_fd())
                .with_cmd(FcntlCmd::F_GETFL)
                .into(),
            Fcntl::new().with_fd(-1).with_cmd(FcntlCmd::F_GETFL).into(),
        ];
        assert_eq!(
            tool.original_file_execution(calls[0]),
            OriginalFileExecution::Native
        );
        let first = tool
            .handle_syscall_event(&mut recording, calls[0])
            .await
            .map_err(|e| e.into_errno().unwrap());
        let second = tool
            .handle_syscall_event(&mut recording, calls[1])
            .await
            .map_err(|e| e.into_errno().unwrap());
        assert!(first.is_ok());
        assert_eq!(second, Err(Errno::EBADF));
        assert_eq!(recording.injections, 2);
        drop(recording); // flush the actual Recorder event and debug streams
        drop(file); // replay must support the original descriptor being virtual
        (data, config, calls, [first, second])
    }
    #[tokio::test]
    async fn recorded_original_file_consumes_real_recorder_stream_without_injection() {
        let (_data, config, calls, expected) = recorded_fixture().await;
        let tool =
            crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut replay = guest(&tool, &config, false);
        for (index, call) in calls.into_iter().enumerate() {
            assert_eq!(
                tool.original_file_execution(call),
                OriginalFileExecution::Recorded
            );
            let value = tool
                .consume_recorded_original_file(&mut replay, call)
                .await
                .map_err(|e| e.into_errno().unwrap());
            assert_eq!(value, expected[index]);
            assert_eq!(replay.thread.count, index as u64 + 1);
            assert_eq!(replay.injections, 0);
        }
    }
    #[tokio::test]
    async fn recorded_original_file_drop_before_poll_consumes_no_event_or_physical_call() {
        let (_data, config, calls, expected) = recorded_fixture().await;
        let tool =
            crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut replay = guest(&tool, &config, false);
        drop(tool.consume_recorded_original_file(&mut replay, calls[0]));
        assert_eq!(replay.thread.count, 0);
        assert_eq!(replay.injections, 0);
        let actual = tool
            .consume_recorded_original_file(&mut replay, calls[0])
            .await
            .map_err(|e| e.into_errno().unwrap());
        assert_eq!(actual, expected[0]);
        assert_eq!(replay.thread.count, 1);
        assert_eq!(replay.injections, 0);
    }
    #[tokio::test]
    async fn recorded_original_file_wrong_command_does_not_consume_return() {
        let (_data, config, calls, expected) = recorded_fixture().await;
        let tool =
            crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut replay = guest(&tool, &config, false);
        let wrong = Fcntl::new().with_fd(-1).with_cmd(FcntlCmd::F_GETFD).into();
        assert!(matches!(
            tool.consume_recorded_original_file(&mut replay, wrong)
                .await,
            Err(Error::Tool(_))
        ));
        assert_eq!(replay.thread.count, 0);
        assert_eq!(replay.injections, 0);
        let actual = tool
            .consume_recorded_original_file(&mut replay, calls[0])
            .await
            .map_err(|e| e.into_errno().unwrap());
        assert_eq!(actual, expected[0]);
        assert_eq!(replay.thread.count, 1);
    }
    #[tokio::test]
    async fn recorded_original_file_wrong_debug_identity_is_not_a_return_receipt() {
        use futures_util::FutureExt;
        let (_data, config, calls, _) = recorded_fixture().await;
        let tool =
            crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut replay = guest(&tool, &config, false);
        let wrong = Fcntl::new().with_fd(-2).with_cmd(FcntlCmd::F_GETFL).into();
        assert_ne!(wrong, calls[0]);
        assert!(
            std::panic::AssertUnwindSafe(tool.consume_recorded_original_file(&mut replay, wrong))
                .catch_unwind()
                .await
                .is_err()
        );
        assert_eq!(replay.injections, 0);
    }

    const READ_COUNTS: [usize; 6] = [0, 1, 32, 1usize << 32, 0x10000000020, 1];
    const READ_BYTES: [&[u8]; 6] = [b"", b"a", b"bcd", b"efg", b"hij", b""];
    const READ_CANARY: u8 = 0xa5;

    // Keep the original full-width access_ok range below the x86_64 user limit,
    // independently of ASLR. Only the explicitly queued (at most three) bytes
    // can be copied by the real nonblocking socket read; no helper reads them.
    struct ReadBuffer(usize);
    impl ReadBuffer {
        fn new() -> Self {
            // SAFETY: a fresh anonymous mapping, released by this owner's Drop.
            let address = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_32BIT,
                    -1,
                    0,
                )
            };
            assert_ne!(address, libc::MAP_FAILED);
            let buffer = Self(address as usize);
            assert!(buffer.0 < 1usize << 31);
            assert!(buffer.0 + READ_COUNTS[4] < 1usize << 47);
            buffer
        }
        fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: this uniquely owned mapping contains at least 32 bytes.
            unsafe { std::slice::from_raw_parts_mut(self.0 as *mut u8, 32) }
        }
        fn require(&mut self, expected: &[u8]) {
            assert_eq!(&self.bytes()[..expected.len()], expected);
            assert!(
                self.bytes()[expected.len()..]
                    .iter()
                    .all(|b| *b == READ_CANARY)
            );
        }
    }
    impl Drop for ReadBuffer {
        fn drop(&mut self) {
            // SAFETY: this is the exact still-owned mapping and original length.
            assert_eq!(
                unsafe { libc::munmap(self.0 as *mut libc::c_void, 4096) },
                0
            );
        }
    }

    struct RecordedReadFixture {
        _data: tempfile::TempDir,
        config: detcore::Config,
        calls: Vec<Syscall>,
        results: Vec<Result<i64, Errno>>,
        buffer: ReadBuffer,
        slot: OwnedFd,
    }

    async fn recorded_read_fixture() -> RecordedReadFixture {
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::os::unix::net::UnixStream;

        let data = tempfile::tempdir().unwrap();
        let config = detcore::Config {
            replay_data: Some(data.path().to_path_buf()),
            ..Default::default()
        };
        let tool = Recorder::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut recording = guest(&tool, &config, true);
        let (socket, mut producer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let slot: OwnedFd = socket.into();
        assert!(matches!(
            tool.fd_replay_kind(recording.pid(), slot.as_raw_fd()),
            ReplayFdKind::None
        ));
        let mut buffer = ReadBuffer::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for (index, (&count, bytes)) in READ_COUNTS.iter().zip(READ_BYTES).enumerate() {
            buffer.bytes().fill(READ_CANARY);
            producer.write_all(bytes).unwrap();
            let fd = if index == 5 { -1 } else { slot.as_raw_fd() };
            let call = SysRead::new()
                .with_fd(fd)
                .with_buf(AddrMut::from_raw(buffer.0))
                .with_len(count)
                .into();
            recording.read_operands = Some((fd, buffer.0, count));
            assert_eq!(
                tool.original_file_execution(call),
                OriginalFileExecution::Native
            );
            let result = tool
                .handle_syscall_event(&mut recording, call)
                .await
                .map_err(|e| e.into_errno().unwrap());
            let expected = if index == 5 {
                Err(Errno::EBADF)
            } else {
                Ok(bytes.len() as i64)
            };
            assert_eq!(result, expected);
            assert_eq!(recording.injections, index + 1);
            buffer.require(bytes);
            calls.push(call);
            results.push(result);
        }
        // A different event variant after all ReadV2/Errno entries detects an
        // extra or missing payload consumption through the real dispatcher.
        let sentinel = Fcntl::new()
            .with_fd(slot.as_raw_fd())
            .with_cmd(FcntlCmd::F_GETFL)
            .into();
        let flags = tool
            .handle_syscall_event(&mut recording, sentinel)
            .await
            .map_err(|e| e.into_errno().unwrap());
        assert!(flags.is_ok());
        assert_eq!(recording.injections, 7);
        calls.push(sentinel);
        results.push(flags);
        drop(recording); // flush both actual streams before opening Replayer

        let mut replacement = tempfile::tempfile().unwrap();
        replacement.write_all(b"replacement").unwrap();
        replacement.seek(SeekFrom::Start(0)).unwrap();
        assert_ne!(replacement.as_raw_fd(), slot.as_raw_fd());
        // SAFETY: both descriptors are owned. Atomically close the original
        // socket mapping and replace it without an unowned numeric-FD gap.
        assert_eq!(
            unsafe { libc::dup3(replacement.as_raw_fd(), slot.as_raw_fd(), libc::O_CLOEXEC) },
            slot.as_raw_fd()
        );
        assert_eq!(
            producer.write_all(b"!").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        drop(producer);
        drop(replacement);
        buffer.bytes().fill(READ_CANARY);
        RecordedReadFixture {
            _data: data,
            config,
            calls,
            results,
            buffer,
            slot,
        }
    }

    async fn replay_read_entry(
        tool: &crate::replayer::Replayer,
        replay: &mut DelegateGuest<crate::replayer::Replayer>,
        fixture: &mut RecordedReadFixture,
        index: usize,
    ) {
        fixture.buffer.bytes().fill(READ_CANARY);
        let call = fixture.calls[index];
        assert_eq!(
            tool.original_file_execution(call),
            OriginalFileExecution::Recorded
        );
        let result = tool
            .consume_recorded_original_file(replay, call)
            .await
            .map_err(|e| e.into_errno().unwrap());
        assert_eq!(result, fixture.results[index]);
        fixture
            .buffer
            .require(READ_BYTES.get(index).copied().unwrap_or(b""));
        // count belongs to the debug stream; the trailing Return and EOF below
        // independently establish the exact payload stream boundary.
        assert_eq!(replay.thread.count, index as u64 + 1);
        assert_eq!(replay.injections, 0);
        // SAFETY: the test still owns this regular-file replacement. Any
        // accidental read through the reused slot would advance this offset.
        assert_eq!(
            unsafe { libc::lseek(fixture.slot.as_raw_fd(), 0, libc::SEEK_CUR) },
            0
        );
    }

    #[tokio::test]
    async fn recorded_original_read_real_stream_survives_native_slot_replacement() {
        let mut fixture = recorded_read_fixture().await;
        let tool = crate::replayer::Replayer::new(
            Pid::from_raw(std::process::id() as i32),
            &fixture.config,
        );
        let mut replay = guest(&tool, &fixture.config, false);
        drop(tool.consume_recorded_original_file(&mut replay, fixture.calls[0]));
        assert_eq!(replay.thread.count, 0);
        assert_eq!(replay.injections, 0);
        fixture.buffer.require(b"");
        for index in 0..fixture.calls.len() {
            replay_read_entry(&tool, &mut replay, &mut fixture, index).await;
        }
        assert!(matches!(
            replay.thread.next_event(),
            Err(bincode::error::DecodeError::Io { inner, .. })
                if inner.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            replay.thread.next_debug_event(),
            Err(bincode::error::DecodeError::Io { inner, .. })
                if inner.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert_eq!(replay.thread.count, 7);
        let mut actual = [0_u8; 11];
        // SAFETY: this bounded postcondition reads only the owned replacement,
        // after every recorded call has completed; it is not a replay helper.
        assert_eq!(
            unsafe {
                libc::read(
                    fixture.slot.as_raw_fd(),
                    actual.as_mut_ptr().cast(),
                    actual.len(),
                )
            },
            11
        );
        assert_eq!(&actual, b"replacement");
    }

    #[tokio::test]
    async fn recorded_original_read_rejects_full_operand_mismatch_before_payload() {
        use futures_util::FutureExt;

        for (index, mismatch) in [(3, 0), (4, 0), (1, 1), (1, 2)] {
            let mut fixture = recorded_read_fixture().await;
            let tool = crate::replayer::Replayer::new(
                Pid::from_raw(std::process::id() as i32),
                &fixture.config,
            );
            let mut replay = guest(&tool, &fixture.config, false);
            for prior in 0..index {
                replay_read_entry(&tool, &mut replay, &mut fixture, prior).await;
            }
            let Syscall::Read(call) = fixture.calls[index] else {
                panic!("expected Read")
            };
            let wrong = match mismatch {
                0 => call.with_len(call.len() as u32 as usize),
                1 => call.with_buf(AddrMut::from_raw(fixture.buffer.0 + 1)),
                2 => call.with_fd(-1),
                _ => unreachable!(),
            };
            assert_ne!(Syscall::from(wrong), fixture.calls[index]);
            fixture.buffer.bytes().fill(READ_CANARY);
            let panic = std::panic::AssertUnwindSafe(
                tool.consume_recorded_original_file(&mut replay, wrong.into()),
            )
            .catch_unwind()
            .await
            .expect_err("operand mismatch must refuse");
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap();
            assert!(message.contains("Replay diverged from recording"));
            // expect_syscall consumes the debug identity before refusing; it
            // must not consume the corresponding recorded ReadV2 payload.
            assert_eq!(replay.thread.count, index as u64 + 1);
            assert_eq!(replay.injections, 0);
            fixture.buffer.require(b"");
            let event = replay.thread.next_event().unwrap().event.unwrap();
            let SyscallEvent::ReadV2(read) = event else {
                panic!("ReadV2 payload was consumed")
            };
            assert_eq!(read.bytes.as_slice(), READ_BYTES[index]);
            assert_eq!(read.consumed_sigpipe_count, 0);
            assert!(matches!(read.replay_fd_kind, ReplayFdKind::None));
            // SAFETY: exact owned replacement, no numeric slot relookup.
            assert_eq!(
                unsafe { libc::lseek(fixture.slot.as_raw_fd(), 0, libc::SEEK_CUR) },
                0
            );
        }
    }
    #[tokio::test]
    async fn malformed_read_control_is_not_an_authentic_recorded_einval() {
        use std::os::fd::FromRawFd;
        for invalid in [0, i32::MIN, i32::MAX] {
            let data = tempfile::tempdir().unwrap();
            let config = detcore::Config {
                replay_data: Some(data.path().to_path_buf()),
                ..Default::default()
            };
            let recorder = Recorder::new(Pid::from_raw(std::process::id() as i32), &config);
            let mut recording = guest(&recorder, &config, true);
            let raw = unsafe {
                libc::timerfd_create(
                    libc::CLOCK_MONOTONIC,
                    libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
                )
            };
            assert!(raw >= 0, "timerfd fixture: {}", io::Error::last_os_error());
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let mut byte = [0x5a];
            let read = reverie::syscalls::Read::new()
                .with_fd(fd.as_raw_fd())
                .with_buf(AddrMut::from_raw(byte.as_mut_ptr() as usize))
                .with_len(1);
            // Explicit malformed trace input, followed by an actual Linux
            // timerfd Read whose one-byte count produces the genuine EINVAL.
            let debug = DebugEvent::new(read.into(), &recording.memory());
            recording.thread.push_debug_event(debug).unwrap();
            recorder.record_event(
                &mut recording,
                Ok(SyscallEvent::ReadInterrupted { signal: invalid }),
            );
            recording.read_operands = Some((read.fd(), read.buf().unwrap().as_raw(), read.len()));
            assert!(matches!(
                recorder
                    .invoke_original_read(&mut recording, read)
                    .await
                    .unwrap(),
                reverie::InjectedReadResult::Complete(Err(Errno::EINVAL))
            ));
            assert_eq!(recording.injections, 1);
            assert_eq!(byte, [0x5a]);
            drop(recording);
            let replayer =
                crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
            let mut replay = guest(&replayer, &config, false);
            let error = replayer
                .invoke_original_read(&mut replay, read)
                .await
                .unwrap_err();
            let Error::Tool(error) = error else {
                panic!("malformed control became a guest errno")
            };
            assert_eq!(
                error.to_string(),
                format!("recorded Read interruption has invalid signal number {invalid}")
            );
            assert_eq!(replay.thread.count, 1);
            assert_eq!(replay.recorded_interruption_waits, 0);
            assert_eq!(replay.injections, 0);
            assert_eq!(byte, [0x5a]);
            assert!(matches!(
                replayer
                    .invoke_original_read(&mut replay, read)
                    .await
                    .unwrap(),
                reverie::InjectedReadResult::Complete(Err(Errno::EINVAL))
            ));
            assert_eq!(replay.thread.count, 2);
            assert_eq!(replay.recorded_interruption_waits, 0);
            assert_eq!(replay.injections, 0);
            assert_eq!(byte, [0x5a]);
            assert!(matches!(replay.thread.next_event(),
                Err(bincode::error::DecodeError::Io { inner, .. }) if inner.kind() == io::ErrorKind::UnexpectedEof));
            assert!(matches!(replay.thread.next_debug_event(),
                Err(bincode::error::DecodeError::Io { inner, .. }) if inner.kind() == io::ErrorKind::UnexpectedEof));
        }
    }

    #[tokio::test]
    async fn original_read_interruption_preserves_control_handler_and_retry_order() {
        let data = tempfile::tempdir().unwrap();
        let config = detcore::Config {
            replay_data: Some(data.path().to_path_buf()),
            ..Default::default()
        };
        let recorder = Recorder::new(Pid::from_raw(std::process::id() as i32), &config);
        let mut recording = guest(&recorder, &config, true);
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut bytes = [0x5a; 4];
        let read = reverie::syscalls::Read::new()
            .with_fd(file.as_raw_fd())
            .with_buf(AddrMut::from_raw(bytes.as_mut_ptr() as usize))
            .with_len(bytes.len());
        recording.interrupt_next_read = true;
        let result = recorder
            .invoke_original_read(&mut recording, read)
            .await
            .unwrap();
        assert!(matches!(
            result,
            reverie::InjectedReadResult::Interrupted(_)
        ));
        assert_eq!(bytes, [0x5a; 4]);
        assert_eq!(recording.injections, 0);
        let sentinel = Fcntl::new()
            .with_fd(file.as_raw_fd())
            .with_cmd(FcntlCmd::F_GETFL);
        let flags = recorder
            .handle_syscall_event(&mut recording, sentinel.into())
            .await
            .unwrap();
        assert_eq!(recording.injections, 1);
        recording.read_operands = Some((read.fd(), read.buf().unwrap().as_raw(), read.len()));
        assert!(matches!(
            recorder
                .invoke_original_read(&mut recording, read)
                .await
                .unwrap(),
            reverie::InjectedReadResult::Complete(Ok(0))
        ));
        assert_eq!(recording.injections, 2);
        assert_eq!(bytes, [0x5a; 4]);
        drop(recording);
        let replayer =
            crate::replayer::Replayer::new(Pid::from_raw(std::process::id() as i32), &config);
        // The canceled attempt has an explicit control, never ReadV2 or errno.
        // The old test incorrectly omitted the original replay Read entirely.
        let mut inspect = guest(&replayer, &config, false);
        assert!(matches!(
            inspect.thread.next_event().unwrap().event,
            Ok(SyscallEvent::ReadInterrupted {
                signal: libc::SIGUSR1
            })
        ));
        drop(inspect);
        let mut replay = guest(&replayer, &config, false);
        assert!(
            matches!(replayer.invoke_original_read(&mut replay, read).await.unwrap(),
            reverie::InjectedReadResult::RecordedInterruption(ticket)
                if ticket.signal() == Some(reverie::Signal::SIGUSR1))
        );
        assert_eq!(replay.thread.count, 1);
        assert_eq!(bytes, [0x5a; 4]);
        assert_eq!(replay.injections, 0);
        assert_eq!(
            replayer
                .consume_recorded_original_file(&mut replay, sentinel.into())
                .await
                .unwrap(),
            flags
        );
        assert_eq!(replay.thread.count, 2);
        assert!(matches!(
            replayer
                .invoke_original_read(&mut replay, read)
                .await
                .unwrap(),
            reverie::InjectedReadResult::Complete(Ok(0))
        ));
        assert_eq!(replay.thread.count, 3);
        assert_eq!(replay.injections, 0);
        assert_eq!(bytes, [0x5a; 4]);
        assert!(matches!(replay.thread.next_event(),
            Err(bincode::error::DecodeError::Io { inner, .. }) if inner.kind() == io::ErrorKind::UnexpectedEof));
        assert!(matches!(replay.thread.next_debug_event(),
            Err(bincode::error::DecodeError::Io { inner, .. }) if inner.kind() == io::ErrorKind::UnexpectedEof));
    }
}

#[cfg(test)]
mod interrupted_read_tests;
