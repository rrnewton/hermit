/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::num::NonZeroUsize;

use async_trait::async_trait;
use reverie::Errno;
use reverie::Error;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::MemoryAccess;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Timespec;
use reverie::syscalls::WaitPidFlag;

use crate::record_or_replay::RecordOrReplay;
use crate::resources::ExternalOpId;
use crate::resources::Permission;
use crate::resources::ResourceID;
use crate::resources::Resources;
use crate::tool_global::resource_request;
use crate::tool_global::thread_observe_time;
use crate::tool_global::trace_schedevent;
use crate::tool_local::Detcore;
use crate::types::LogicalTime;
use crate::types::SchedEvent;
use crate::types::SyscallPhase;

impl<T: RecordOrReplay> Detcore<T> {
    /// Record or replay a BLOCKING syscall without stalling the current thread (and thus
    /// deadlocking).  This uses a protocol of an extra resource request before/after the
    /// syscall to inform the scheduler that the thread is leaving/rejoining the runnable
    /// threads pool.
    ///
    /// This is only valid to use (1) in hermit record/replay modes, or (2)
    /// when we're in "hermit run", but we're NOT sequentializing threads, because in
    /// that case it's ok to use the blocking versions of system calls.
    pub async fn record_or_replay_blocking<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        let dettid = guest.thread_state().dettid;
        let op_id = ExternalOpId::new(dettid, guest.thread_state().stats.syscall_count);
        {
            let mut rsrcs = Resources::new(dettid);
            // TODO: check if the file descriptors include anything EXTERNAL before
            // asking for this resource:
            rsrcs.insert(ResourceID::BlockingExternalIO(op_id), Permission::RW);
            rsrcs.fyi(call.name());
            resource_request(guest, rsrcs).await;
        }
        tracing::trace!(
            "Guest proceeding to execute potentially blocking call {}...",
            call.name()
        );
        let res = self.record_or_replay(guest, call).await;
        // N.B. BlockingExternalIO is a "oneshot" resource, so no need to release
        // explicitly here:
        {
            let mut rsrcs = Resources::new(dettid);
            rsrcs.insert(ResourceID::BlockedExternalContinue(op_id), Permission::RW);
            rsrcs.fyi(call.name());
            resource_request(guest, rsrcs).await;
        }
        Ok(res?)
    }

    /// Executes a nonblockable syscall according to the following strategy:
    /// - Record mode: Execute possibly blocking syscall
    /// - Run mode: Transform the syscall to nonblocking if required before executing
    ///
    /// These are fd-oriented syscalls in the sense that whether they block or not depends
    /// on whether NONBLOCK was set on the corresponding file descriptor.
    pub async fn execute_nonblockable_fd_syscall<
        G: Guest<Self>,
        C: SyscallInfo + NonblockableSyscall + Into<Syscall>,
    >(
        &self,
        guest: &mut G,
        call: C,
    ) -> Result<i64, Error> {
        let wrapped: Syscall = call.into();

        let action = ioaction_based_on_fd_status(guest, call);

        if !self.cfg.sequentialize_threads
            || self.cfg.recordreplay_modes
            || action == IOAction::Blocking
        {
            tracing::trace!(
                "NonblockableSyscall: executing in blocking mode after all: {}",
                call.name()
            );
            // We let these have nondeterminstic timing in record mode:
            Ok(self.record_or_replay_blocking(guest, wrapped).await?)
            // If in the future we want to record EXTERNAL network traffic only, we have a
            // challenge to overcome.  We don't know if we need to record until after the
            // accept completes, so we need an API for *post-facto* recording.
        } else if action == IOAction::NonblockizeRetry {
            tracing::trace!(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): {}",
                call.name()
            );
            let rsrc = internal_io_polling_resources(guest, call.name());
            Ok(retry_nonblocking_syscall(guest, call, rsrc).await?)
        } else {
            assert!(action == IOAction::PassThru);
            tracing::trace!(
                "NonblockableSyscall: just passing it through: {}",
                call.name()
            );
            // Otherwise, the socket was already nonblocking, so we can safely execute it just once.
            Ok(self.record_or_replay(guest, wrapped).await?)
        }
    }

    /// Override physically_nonblocking to true for the file descriptor, if appropriate.
    pub fn maybe_set_nonblocking_fd<G: Guest<Self>>(&self, guest: &G, fd: i32) {
        if self.cfg.sequentialize_threads && !self.cfg.debug_externalize_sockets {
            guest
                .thread_state()
                .with_detfd(fd, |detfd| {
                    detfd.set_physically_nonblocking();
                })
                .unwrap();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsedTimeout {
    Immediate,
    Infinite,
    Deadline(LogicalTime),
}

impl ParsedTimeout {
    fn deadline(self) -> Option<LogicalTime> {
        match self {
            Self::Immediate | Self::Infinite => None,
            Self::Deadline(deadline) => Some(deadline),
        }
    }
}

pub fn checked_timespec_to_nanos(timeout: Timespec) -> Result<u128, Errno> {
    if timeout.tv_sec < 0 || timeout.tv_nsec < 0 || timeout.tv_nsec >= 1_000_000_000 {
        return Err(Errno::EINVAL);
    }

    Ok(timeout.tv_sec as u128 * 1_000_000_000 + timeout.tv_nsec as u128)
}

pub fn checked_timeval_to_nanos(timeout: libc::timeval) -> Result<u128, Errno> {
    if timeout.tv_sec < 0 || timeout.tv_usec < 0 || timeout.tv_usec >= 1_000_000 {
        return Err(Errno::EINVAL);
    }

    Ok(timeout.tv_sec as u128 * 1_000_000_000 + timeout.tv_usec as u128 * 1_000)
}

fn saturating_logical_time(nanos: u128) -> LogicalTime {
    LogicalTime::from_nanos(nanos.try_into().unwrap_or(u64::MAX))
}

fn saturating_deadline(base: LogicalTime, delta_nanos: u128) -> LogicalTime {
    saturating_logical_time(base.as_nanos() as u128 + delta_nanos)
}

pub async fn relative_timespec_timeout<T, G>(
    guest: &mut G,
    timeout: Option<Timespec>,
) -> Result<ParsedTimeout, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(timeout) = timeout else {
        return Ok(ParsedTimeout::Infinite);
    };
    let timeout_nanos = checked_timespec_to_nanos(timeout)?;
    Ok(relative_nanos_timeout(guest, timeout_nanos).await)
}

pub async fn relative_timeval_timeout<T, G>(
    guest: &mut G,
    timeout: Option<libc::timeval>,
) -> Result<ParsedTimeout, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let Some(timeout) = timeout else {
        return Ok(ParsedTimeout::Infinite);
    };
    let timeout_nanos = checked_timeval_to_nanos(timeout)?;
    Ok(relative_nanos_timeout(guest, timeout_nanos).await)
}

pub fn absolute_timespec_timeout(timeout: Option<Timespec>) -> Result<ParsedTimeout, Errno> {
    let Some(timeout) = timeout else {
        return Ok(ParsedTimeout::Infinite);
    };
    Ok(ParsedTimeout::Deadline(saturating_logical_time(
        checked_timespec_to_nanos(timeout)?,
    )))
}

pub async fn millis_timeout<T, G>(guest: &mut G, timeout_millis: i32) -> ParsedTimeout
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    match timeout_millis {
        0 => ParsedTimeout::Immediate,
        timeout if timeout < 0 => ParsedTimeout::Infinite,
        timeout => relative_nanos_timeout(guest, timeout as u128 * 1_000_000).await,
    }
}

async fn relative_nanos_timeout<T, G>(guest: &mut G, timeout_nanos: u128) -> ParsedTimeout
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    if timeout_nanos == 0 {
        ParsedTimeout::Immediate
    } else {
        let base = thread_observe_time(guest).await;
        ParsedTimeout::Deadline(saturating_deadline(base, timeout_nanos))
    }
}

fn internal_io_polling_resources<T, G>(guest: &G, name: &str) -> Resources
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let mut resources = Resources::new(guest.thread_state().dettid);
    resources.insert(ResourceID::InternalIOPolling, Permission::W);
    resources.fyi(name);
    resources
}

pub async fn execute_internal_io_polling<T, G, C>(
    guest: &mut G,
    call: C,
    timeout: ParsedTimeout,
) -> Result<i64, Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
    C: NonblockableSyscall + TimeoutableSyscall,
{
    if timeout == ParsedTimeout::Immediate {
        return Ok(guest.inject(call).await?);
    }

    let resources = internal_io_polling_resources(guest, call.name());
    retry_nonblocking_syscall_with_timeout(guest, call, resources, timeout.deadline()).await
}

/// A blocking syscall that involves a fail descriptor may be handled in these three ways:
#[derive(PartialEq, Eq, Debug)]
pub enum IOAction {
    /// It may physically block and we can't change that.  Treat it as ExternalBlockingIO.
    Blocking,
    /// We can nonblockize and retry the call.
    NonblockizeRetry,
    /// The call is nonblocking already, and safe to execute.
    PassThru,
}

/// Returns strategy based on FD-based call may actually block when executed.
pub fn ioaction_based_on_fd_status<
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
    C: SyscallInfo + Into<Syscall>,
>(
    guest: &mut G,
    call: C,
) -> IOAction {
    let wrapped: Syscall = call.into();
    let fd = get_fd(wrapped).unwrap_or_else(|| panic!("Failed to get fd for {}", call.name()));
    let (phys, virt) = guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            (detfd.physically_nonblocking(), detfd.is_nonblocking())
        })
        .unwrap();
    tracing::trace!(
        "Checking FD {} for nonblocking: physical {} / virtual {}",
        fd,
        phys,
        virt
    );
    if virt && !phys {
        // TF: simulate nonblocking on top of physically blocking? How?
        panic!(
            "Invariant violation, fd {}: we cannot simulate nonblocking behavior when set to blocking mode in the kernel.",
            fd
        );
    } else if !virt && !phys {
        // FF: logically blocking, physically blocking, this could only work with BlockingExternalIO.
        IOAction::Blocking
    } else if virt && phys {
        // TT: both nonblocking, so firing once is sufficient
        IOAction::PassThru
    } else {
        // FT: Need to simulate blocking on top of nonblocking.
        IOAction::NonblockizeRetry
    }
}

/// A large subset of system calls have a single, unique file descriptor argument.  This
/// is a convenience function for grabbing that argument.
///
/// It does not cover system calls with multiple fd arguments, with pointers to heap
/// structures that contain fds.
fn get_fd(s: Syscall) -> Option<i32> {
    match s {
        Syscall::Recvfrom(s) => Some(s.fd()),
        Syscall::Recvmsg(s) => Some(s.sockfd()),
        Syscall::Recvmmsg(s) => Some(s.fd()),
        Syscall::Sendto(s) => Some(s.fd()),
        Syscall::Sendmsg(s) => Some(s.fd()),
        Syscall::Sendmmsg(s) => Some(s.sockfd()),
        Syscall::Accept(s) => Some(s.sockfd()),
        Syscall::Accept4(s) => Some(s.sockfd()),
        Syscall::Connect(s) => Some(s.fd()),
        Syscall::Bind(s) => Some(s.fd()),
        Syscall::Listen(s) => Some(s.fd()),
        Syscall::Getsockname(s) => Some(s.fd()),
        Syscall::Getpeername(s) => Some(s.fd()),
        Syscall::Setsockopt(s) => Some(s.fd()),
        Syscall::Getsockopt(s) => Some(s.fd()),

        Syscall::Read(s) => Some(s.fd()),
        Syscall::Write(s) => Some(s.fd()),
        Syscall::Close(s) => Some(s.fd()),
        Syscall::Fstat(s) => Some(s.fd()),
        Syscall::Lseek(s) => Some(s.fd()),
        Syscall::Mmap(s) => Some(s.fd()),
        Syscall::Ioctl(s) => Some(s.fd()),
        Syscall::Pread64(s) => Some(s.fd()),
        Syscall::Pwrite64(s) => Some(s.fd()),
        Syscall::Readv(s) => Some(s.fd()),
        Syscall::Writev(s) => Some(s.fd()),

        Syscall::Shutdown(s) => Some(s.fd()),
        Syscall::Fcntl(s) => Some(s.fd()),
        Syscall::Flock(s) => Some(s.fd()),
        Syscall::Fsync(s) => Some(s.fd()),
        Syscall::Fdatasync(s) => Some(s.fd()),
        Syscall::Ftruncate(s) => Some(s.fd()),
        Syscall::Fchdir(s) => Some(s.fd()),
        Syscall::Fchmod(s) => Some(s.fd()),
        Syscall::Fchown(s) => Some(s.fd()),
        Syscall::Fstatfs(s) => Some(s.fd()),
        Syscall::Readahead(s) => Some(s.fd()),
        Syscall::Fsetxattr(s) => Some(s.fd()),
        Syscall::Fgetxattr(s) => Some(s.fd()),
        Syscall::Flistxattr(s) => Some(s.fd()),
        Syscall::Fremovexattr(s) => Some(s.fd()),
        Syscall::Fadvise64(s) => Some(s.fd()),
        Syscall::InotifyAddWatch(s) => Some(s.fd()),
        Syscall::InotifyRmWatch(s) => Some(s.fd()),
        Syscall::SyncFileRange(s) => Some(s.fd()),
        Syscall::Vmsplice(s) => Some(s.fd()),
        Syscall::Utimensat(s) => Some(s.dirfd()),
        Syscall::Signalfd(s) => Some(s.fd()),
        Syscall::Fallocate(s) => Some(s.fd()),
        Syscall::TimerfdSettime(s) => Some(s.fd()),
        Syscall::TimerfdGettime(s) => Some(s.fd()),
        Syscall::Signalfd4(s) => Some(s.fd()),
        Syscall::Preadv(s) => Some(s.fd()),
        Syscall::Pwritev(s) => Some(s.fd()),
        Syscall::Syncfs(s) => Some(s.fd()),
        Syscall::Setns(s) => Some(s.fd()),
        Syscall::FinitModule(s) => Some(s.fd()),
        Syscall::Preadv2(s) => Some(s.fd()),
        Syscall::Pwritev2(s) => Some(s.fd()),

        Syscall::Openat(s) => Some(s.dirfd()),
        Syscall::Mkdirat(s) => Some(s.dirfd()),
        Syscall::Mknodat(s) => Some(s.dirfd()),
        Syscall::Fchownat(s) => Some(s.dirfd()),
        Syscall::Futimesat(s) => Some(s.dirfd()),
        Syscall::Newfstatat(s) => Some(s.dirfd()),
        Syscall::Unlinkat(s) => Some(s.dirfd()),
        Syscall::Readlinkat(s) => Some(s.dirfd()),
        Syscall::Fchmodat(s) => Some(s.dirfd()),
        Syscall::Faccessat(s) => Some(s.dirfd()),
        Syscall::NameToHandleAt(s) => Some(s.dirfd()),
        Syscall::Execveat(s) => Some(s.dirfd()),
        Syscall::Statx(s) => Some(s.dirfd()),
        Syscall::Symlinkat(s) => Some(s.newdirfd()),
        Syscall::PerfEventOpen(s) => Some(s.group_fd()),
        Syscall::OpenByHandleAt(s) => Some(s.mount_fd()),

        Syscall::EpollCtl(s) => Some(s.epfd()),
        Syscall::EpollWait(s) => Some(s.epfd()),
        Syscall::EpollPwait(s) => Some(s.epfd()),

        // Ambiguous, 2 fds, no answer:
        Syscall::Dup2(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Sendfile(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Renameat(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Linkat(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::FanotifyMark(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Renameat2(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::Dup3(_) => None,
        // Ambiguous, 2 fds, no answer:
        Syscall::KexecLoad(_) => None,

        // Takes a pointer to fd, not directly accessible:
        Syscall::Poll(_) => None,
        // Takes a pointer to fd, not directly accessible:
        Syscall::Ppoll(_) => None,

        _ => None,
    }
}

/// A system call which may or may not block, but which can be MADE nonblocking.
#[async_trait]
pub trait NonblockableSyscall: SyscallInfo {
    /// Convert the system call to a nonblocking version of itself.  Sometimes this means
    /// setting a zero timeout, and sometimes it means something else.
    ///
    /// This may need to stack allocate, so it returns a StackGuard.
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>);

    /// Restore inputs that the kernel overwrites during a nonblocking probe.
    fn prepare_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        _guest: &mut G,
        _original: Self,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Publish probe outputs once the syscall has completed or timed out.
    fn finish_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        _guest: &mut G,
        _original: Self,
    ) -> Result<(), Error> {
        Ok(())
    }

    /// Check if the result (in nonblocking mode) is analogous to blocking in blocking mode.
    /// I.e. the result means "try again".
    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Ok(0)
    }

    /// Convert a physical nonblocking completion into the result expected by the guest.
    /// `retried` is true after a prior result was classified as blocked.
    fn normalize_nonblocking_result(
        &self,
        res: Result<i64, Errno>,
        _retried: bool,
    ) -> Result<i64, Errno> {
        res
    }
}

/// A system call which can logically timeout and then would return a given value
/// indicating that timeout.
pub trait TimeoutableSyscall: SyscallInfo {
    /// What would the syscall return IF it timed out?
    fn timeout_return_val(&self) -> Result<i64, Errno>;
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Poll {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        (self.with_timeout(0), None)
    }
}

impl TimeoutableSyscall for reverie::syscalls::Poll {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::EpollWait {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        (self.with_timeout(0), None)
    }
}

impl TimeoutableSyscall for reverie::syscalls::EpollWait {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

async fn zero_timespec<'stack, T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
) -> (Addr<'stack, Timespec>, <G::Stack as Stack>::StackGuard) {
    let mut stack = guest.stack().await;
    let tp_val = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let tp = stack.push(tp_val);
    let guard = stack.commit().expect("stack.commit to succeed");
    (tp, guard)
}

async fn zero_timeval<'stack, T: RecordOrReplay, G: Guest<Detcore<T>>>(
    guest: &mut G,
) -> (Addr<'stack, libc::timeval>, <G::Stack as Stack>::StackGuard) {
    let mut stack = guest.stack().await;
    let timeout = stack.push(libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    });
    let guard = stack.commit().expect("stack.commit to succeed");
    (timeout, guard)
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Ppoll {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let (timeout, guard) = zero_timeval(guest).await;
        (self.with_timeout(Some(timeout)), Some(guard))
    }
}

impl TimeoutableSyscall for reverie::syscalls::Ppoll {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

fn select_fd_set_len(nfds: i32) -> Result<usize, Errno> {
    let nfds = usize::try_from(nfds).map_err(|_| Errno::EINVAL)?;
    let bits_per_word = std::mem::size_of::<libc::c_ulong>() * 8;
    Ok(nfds.div_ceil(bits_per_word) * std::mem::size_of::<libc::c_ulong>())
}

fn copy_select_fd_sets<T, G>(
    guest: &mut G,
    nfds: i32,
    sources: [Option<AddrMut<'_, libc::fd_set>>; 3],
    destinations: [Option<AddrMut<'_, libc::fd_set>>; 3],
) -> Result<(), Error>
where
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let len = select_fd_set_len(nfds)?;
    if len == 0 {
        return Ok(());
    }

    let mut memory = guest.memory();
    for (source, destination) in sources.into_iter().zip(destinations) {
        if let (Some(source), Some(destination)) = (source, destination) {
            let mut bytes = vec![0; len];
            memory.read_exact(source.cast::<u8>(), &mut bytes)?;
            memory.write_exact(destination.cast::<u8>(), &bytes)?;
        }
    }
    Ok(())
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Select {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let mut stack = guest.stack().await;
        let readfds = self.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = self.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = self.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        let timeout = stack.reserve::<libc::timeval>();
        let guard = stack.commit().expect("stack.commit to succeed");
        (
            self.with_readfds(readfds)
                .with_writefds(writefds)
                .with_exceptfds(exceptfds)
                .with_timeout(Some(timeout)),
            Some(guard),
        )
    }

    fn prepare_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        guest: &mut G,
        original: Self,
    ) -> Result<(), Error> {
        copy_select_fd_sets(
            guest,
            self.nfds(),
            [
                original.readfds(),
                original.writefds(),
                original.exceptfds(),
            ],
            [self.readfds(), self.writefds(), self.exceptfds()],
        )
    }

    fn finish_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        guest: &mut G,
        original: Self,
    ) -> Result<(), Error> {
        copy_select_fd_sets(
            guest,
            self.nfds(),
            [self.readfds(), self.writefds(), self.exceptfds()],
            [
                original.readfds(),
                original.writefds(),
                original.exceptfds(),
            ],
        )
    }
}

impl TimeoutableSyscall for reverie::syscalls::Select {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Pselect6 {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let mut stack = guest.stack().await;
        let readfds = self.readfds().map(|_| stack.reserve::<libc::fd_set>());
        let writefds = self.writefds().map(|_| stack.reserve::<libc::fd_set>());
        let exceptfds = self.exceptfds().map(|_| stack.reserve::<libc::fd_set>());
        let timeout = stack.reserve::<libc::timeval>();
        let guard = stack.commit().expect("stack.commit to succeed");
        (
            self.with_readfds(readfds)
                .with_writefds(writefds)
                .with_exceptfds(exceptfds)
                .with_timeout(Some(timeout.into())),
            Some(guard),
        )
    }

    fn prepare_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        guest: &mut G,
        original: Self,
    ) -> Result<(), Error> {
        copy_select_fd_sets(
            guest,
            self.nfds(),
            [
                original.readfds(),
                original.writefds(),
                original.exceptfds(),
            ],
            [self.readfds(), self.writefds(), self.exceptfds()],
        )
    }

    fn finish_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        &self,
        guest: &mut G,
        original: Self,
    ) -> Result<(), Error> {
        copy_select_fd_sets(
            guest,
            self.nfds(),
            [self.readfds(), self.writefds(), self.exceptfds()],
            [
                original.readfds(),
                original.writefds(),
                original.exceptfds(),
            ],
        )
    }
}

impl TimeoutableSyscall for reverie::syscalls::Pselect6 {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Wait4 {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        _guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let call2 = self.with_options(self.options() | WaitPidFlag::WNOHANG);
        (call2, None)
    }

    // Child has not changed state yet, so we go to the scheduler and wait to poll again.
    // In scenarios with lots of outstanding waits, this polling strategy can change the asymptotic
    // complexity of the program. Ideally, we would model the blocking `wait4` (and process state
    // transitions) directly in the scheduler, and execute it only when we know it will complete.
    //
    // The polling backoff strategy mitigates this problem however.
    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Ok(0)
    }
}

#[async_trait]
/// Used only for FUTEX_WAIT
impl NonblockableSyscall for reverie::syscalls::Futex {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        let (tp, guard) = zero_timespec(guest).await;
        (self.with_timeout(Some(tp)), Some(guard))
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // EAGAIN can mean the futex wait's compare-and-block failed and we should return that to
        // the guest.  With timeout=0, the timeout is what shows that it would have blocked.
        res == Err(Errno::ETIMEDOUT)
    }
}

impl TimeoutableSyscall for reverie::syscalls::Futex {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Err(Errno::ETIMEDOUT)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::RtSigtimedwait {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        // This is a bit more complicated because we need a new timespec to point to in
        // the guest memory.
        let (tp, guard) = zero_timespec(guest).await;
        (self.with_timeout(Some(tp)), Some(guard))
    }
}

impl TimeoutableSyscall for reverie::syscalls::RtSigtimedwait {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

/// While the read syscall is quite general, this nonblocking capacity is used
/// ONLY for sockets and pipes.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Read {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // A return value of Ok(0) indicates end of file.
        // Note that we've ruled out 0-count reads before this point.
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

/// While the read syscall is quite general, this nonblocking capacity is used
/// ONLY for sockets and pipes.
#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Write {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        // A return value of Ok(0) indicates end of file.
        // Note that we've ruled out 0-count reads before this point.
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Readv {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Writev {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

/// A common helper shared among several network syscalls.
/// We can't actually CONVERT these syscalls into nonblocking, but we can assert that they are by
/// checking the status of their file descriptor.
fn network_comm_syscall<T: RecordOrReplay, G: Guest<Detcore<T>>, C: SyscallInfo + Into<Syscall>>(
    call: C,
    guest: &mut G,
) -> (C, Option<<G::Stack as Stack>::StackGuard>) {
    // Already nonblocking because we've assured the socket is.
    let fd = get_fd(call.into()).unwrap_or_else(|| {
        panic!(
            "network_comm_syscall called on invalid syscall / unknown fd: {}",
            call.name()
        );
    });
    guest
        .thread_state()
        .with_detfd(fd, |detfd| {
            assert!(
                detfd.physically_nonblocking(),
                "expecting sockets/pipes to be physically nonblocking"
            );
        })
        .unwrap();
    (call, None)
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Accept4 {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

impl TimeoutableSyscall for reverie::syscalls::Accept4 {
    fn timeout_return_val(&self) -> Result<i64, Errno> {
        Ok(0)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvfrom {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Recvmmsg {
    // This system call has a timeout argument, but we ignore it because the underlying
    // socket is nonblocking anyway (in runs where we call this).
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendto {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendmmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Sendmsg {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN) || res == Err(Errno::EWOULDBLOCK)
    }
}

#[async_trait]
impl NonblockableSyscall for reverie::syscalls::Connect {
    async fn into_nonblocking<T: RecordOrReplay, G: Guest<Detcore<T>>>(
        self,
        guest: &mut G,
    ) -> (Self, Option<<G::Stack as Stack>::StackGuard>) {
        network_comm_syscall(self, guest)
    }

    fn syscall_would_have_blocked(&self, res: Result<i64, Errno>) -> bool {
        res == Err(Errno::EAGAIN)
            || res == Err(Errno::EWOULDBLOCK)
            || res == Err(Errno::EINPROGRESS)
            || res == Err(Errno::EALREADY)
    }

    fn normalize_nonblocking_result(
        &self,
        res: Result<i64, Errno>,
        retried: bool,
    ) -> Result<i64, Errno> {
        match (retried, res) {
            (true, Err(Errno::EISCONN)) => Ok(0),
            (_, res) => res,
        }
    }
}

/// Transform a syscall to nonblocking, then retry it until it returns a successful result.
pub async fn retry_nonblocking_syscall<T, G, C>(
    guest: &mut G,
    call: C,
    rsrc: Resources,
) -> Result<i64, Error>
where
    C: NonblockableSyscall,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    // Bogus 99 return value is dead code below:
    retry_nonblocking_syscall_helper(guest, call, rsrc, None).await
}

/// Retry a non-blocking syscall until it succeeds. Set the timeout to zero for the actual
/// syscalls (retries), while monitoring the clock to see if/when the logical timeout
/// should trigger.  Timeout is passed as an ABSOLUTE TIME (not duration).
pub async fn retry_nonblocking_syscall_with_timeout<T, G, C>(
    guest: &mut G,
    call: C,
    rsrc: Resources,
    // Logical timeout:
    maybe_timeout: Option<LogicalTime>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall + TimeoutableSyscall,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let maybe_tup = maybe_timeout.map(|t| (t, call.timeout_return_val()));
    retry_nonblocking_syscall_helper(guest, call, rsrc, maybe_tup).await
}

// Private helper.
async fn retry_nonblocking_syscall_helper<T, G, C>(
    guest: &mut G,
    call0: C,
    rsrc: Resources,
    maybe_timeout: Option<(LogicalTime, Result<i64, Errno>)>,
) -> Result<i64, Error>
where
    C: NonblockableSyscall,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    // The stack-allocated memory here needs to live across the loop, which means
    // surviving multiple syscall injections:
    let (call, _maybe_stackguard) = call0.into_nonblocking(guest).await;
    let mut rsrc = rsrc.clone();

    loop {
        call.prepare_nonblocking(guest, call0)?;
        resource_request(guest, rsrc.clone()).await;
        let res = guest.inject_with_retry(call).await;
        if call.syscall_would_have_blocked(res) {
            rsrc.poll_attempt += 1;
            if let Some((timeout, timeout_result)) = maybe_timeout {
                let new_time = thread_observe_time(guest).await;
                if new_time >= timeout {
                    call.finish_nonblocking(guest, call0)?;
                    tracing::trace!(
                        "Timing out syscall after #{} retries: {}",
                        rsrc.poll_attempt - 1,
                        call.display(&guest.memory())
                    );
                    return timeout_result.map_err(|e| e.into());
                } else {
                    tracing::trace!(
                        "Retry #{} for syscall due to result {:?}, {} from timeout: {}",
                        rsrc.poll_attempt,
                        res,
                        timeout - new_time,
                        call.display(&guest.memory())
                    );
                    record_retry_event(guest, call).await;
                }
            } else {
                tracing::trace!(
                    "Retry #{} for syscall due to result {:?}: {}",
                    rsrc.poll_attempt,
                    res,
                    call.display(&guest.memory())
                );
                record_retry_event(guest, call).await;
            }
        } else {
            call.finish_nonblocking(guest, call0)?;
            let res = call
                .normalize_nonblocking_result(res, rsrc.poll_attempt > 0)
                .map_err(|e| e.into());
            tracing::trace!(
                "retry_nonblocking_syscall: syscall completed after {} retries: {} = {:?}",
                rsrc.poll_attempt,
                call.display(&guest.memory()),
                res
            );
            return res;
        }
    }
}

async fn record_retry_event<G, C, T>(guest: &mut G, call: C)
where
    C: NonblockableSyscall,
    T: RecordOrReplay,
    G: Guest<Detcore<T>>,
{
    let dettid = guest.thread_state().dettid;
    let cfg = &guest.config();
    if cfg.sequentialize_threads && cfg.should_trace_schedevent() {
        trace_schedevent(
            guest,
            with_guest_time(
                guest,
                SchedEvent::syscall(dettid, call.number(), SyscallPhase::Polling),
            ),
            true,
        )
        .await;
    }
}

// A helper function for enriching the schedevent with local information.
pub fn with_guest_time<G, T>(guest: &G, event: SchedEvent) -> SchedEvent
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    let dettime = &guest.thread_state().thread_logical_time;
    event.with_dettime(dettime)
}

// Enrich the event with the RIP register from the current guest state, but only if it is unset.
pub async fn with_guest_rip<G, T>(guest: &mut G, mut event: SchedEvent) -> SchedEvent
where
    G: Guest<Detcore<T>>,
    T: RecordOrReplay,
{
    assert!(event.end_rip.is_none());

    let regs = guest.regs().await;
    let end_rip = NonZeroUsize::new(regs.rip.try_into().unwrap()).unwrap();
    event.end_rip = Some(end_rip);
    event
}

#[cfg(test)]
mod nonblocking_tests {
    use super::*;

    #[test]
    fn timespec_validation_and_saturation() {
        assert_eq!(
            checked_timespec_to_nanos(Timespec {
                tv_sec: 1,
                tv_nsec: 2,
            }),
            Ok(1_000_000_002)
        );
        assert_eq!(
            checked_timespec_to_nanos(Timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            }),
            Err(Errno::EINVAL)
        );
        assert_eq!(
            saturating_deadline(LogicalTime::from_nanos(7), u64::MAX as u128 + 1),
            LogicalTime::MAX
        );
        assert_eq!(
            absolute_timespec_timeout(Some(Timespec {
                tv_sec: i64::MAX,
                tv_nsec: 999_999_999,
            })),
            Ok(ParsedTimeout::Deadline(LogicalTime::MAX))
        );
        assert_eq!(select_fd_set_len(-1), Err(Errno::EINVAL));
        assert_eq!(select_fd_set_len(0), Ok(0));
        assert_eq!(select_fd_set_len(1), Ok(8));
        assert_eq!(select_fd_set_len(65), Ok(16));
        assert_eq!(
            checked_timeval_to_nanos(libc::timeval {
                tv_sec: 1,
                tv_usec: 2,
            }),
            Ok(1_000_002_000)
        );
        assert_eq!(
            checked_timeval_to_nanos(libc::timeval {
                tv_sec: 0,
                tv_usec: 1_000_000,
            }),
            Err(Errno::EINVAL)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_nonblocking_results() {
        let call = reverie::syscalls::Connect::new();
        assert!(call.syscall_would_have_blocked(Err(Errno::EINPROGRESS)));
        assert!(call.syscall_would_have_blocked(Err(Errno::EALREADY)));
        assert_eq!(
            call.normalize_nonblocking_result(Err(Errno::EISCONN), true),
            Ok(0)
        );
        assert_eq!(
            call.normalize_nonblocking_result(Err(Errno::EISCONN), false),
            Err(Errno::EISCONN)
        );
    }
}
