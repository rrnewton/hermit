// Controller API over one durable physical-custody owner. Waiters never own
// the only original pin, escrow, worker identity, job channel or receipt.
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::time::Duration;
use std::time::Instant;

pub(crate) struct ReleaseService {
    owner: Arc<ServiceOwner>,
    broker_pid: i32,
    broker_pidfd: SharedFd,
    stopped: bool,
    reap_receipts: BTreeMap<u64, (i32, u64)>,
}
struct PinView<'pin> {
    holder: &'pin mut Option<OwnedFd>,
    owner: Option<Arc<ServiceOwner>>,
    id: u64,
}
impl PinView<'_> {
    fn is_some(&self) -> bool {
        self.holder.is_some()
            || self.owner.as_ref().is_some_and(|o| {
                o.state
                    .lock()
                    .unwrap()
                    .jobs
                    .get(&self.id)
                    .is_some_and(|r| r.pin.is_some())
            })
    }
    fn is_none(&self) -> bool {
        !self.is_some()
    }
}
struct ReleaseJob<'pin> {
    pin: PinView<'pin>,
    owner: Option<Arc<ServiceOwner>>,
    broker_control: SharedFd,
    channel: SharedFd,
    worker_pidfd: Option<SharedFd>,
    possessed_pid: Option<i32>,
    dropped_pid: Option<i32>,
    escrow: Option<()>,
    id: u64,
    interrupt_requested: bool,
    terminal_requested: bool,
    close_result: Option<Result<(), i32>>,
    reaped_status: Option<i32>,
    submitted: bool,
    rejected_without_child: bool,
    started: bool,
    started_as_exit: bool,
}
#[must_use]
struct ReleasePrepareError<'pin> {
    error: std::io::Error,
    job: Option<ReleaseJob<'pin>>,
}

#[must_use]
pub(crate) struct ReleaseBootstrapError {
    pub(crate) error: std::io::Error,
    pub(crate) cleanup_error: Option<std::io::Error>,
    // Exact direct-child capability survives every bounded-out cleanup.
    pub(crate) cleanup: Option<BrokerCleanup>,
}
impl std::fmt::Debug for ReleaseBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseBootstrapError")
            .field("error", &self.error)
            .field("cleanup_error", &self.cleanup_error)
            .field("owned_broker", &self.cleanup.as_ref().map(|c| c.pid))
            .finish()
    }
}
impl From<std::io::Error> for ReleaseBootstrapError {
    fn from(error: std::io::Error) -> Self {
        Self {
            error,
            cleanup_error: None,
            cleanup: None,
        }
    }
}
#[must_use]
pub(crate) struct BrokerCleanup {
    control: SharedFd,
    pid: i32,
    pidfd: Option<SharedFd>,
    reaped_status: Option<i32>,
}
impl BrokerCleanup {
    pub(crate) fn terminate_and_reap(&mut self, deadline: Instant) -> std::io::Result<()> {
        if self.reaped_status.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "owned bootstrap cleanup bound exhausted",
            ));
        }
        let signaled = if let Some(pidfd) = &self.pidfd {
            unsafe { signal_pidfd(pidfd.as_raw_fd(), libc::SIGKILL) }
        } else {
            // Only our unreaped direct child, never a PID from a message.
            unsafe { libc::kill(self.pid, libc::SIGKILL) == 0 || errno() == libc::ESRCH }
        };
        if !signaled {
            return Err(std::io::Error::last_os_error());
        }
        let status = if let Some(pidfd) = &self.pidfd {
            reap_owned(self.pid, pidfd.as_raw_fd(), deadline)?
        } else {
            reap_bootstrap_without_pidfd(self.pid, deadline)?
        };
        self.reaped_status = Some(status);
        Ok(())
    }
}

fn wait_readable(fd: i32, deadline: Instant) -> std::io::Result<()> {
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "release supervisor bound")
            })?;
        let timeout = libc::timespec {
            tv_sec: remaining.as_secs() as libc::time_t,
            tv_nsec: remaining.subsec_nanos() as libc::c_long,
        };
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::ppoll(&mut pollfd, 1, &timeout, null()) };
        if result > 0 {
            return Ok(());
        }
        if result < 0 && unsafe { errno() } != libc::EINTR {
            return Err(std::io::Error::last_os_error());
        }
    }
}
fn reap_bootstrap_without_pidfd(pid: i32, deadline: Instant) -> std::io::Result<i32> {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return Ok(status);
        }
        if result < 0 && unsafe { errno() } != libc::EINTR {
            return Err(std::io::Error::last_os_error());
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "owned bootstrap cleanup incomplete",
                )
            })?;
        std::thread::sleep(remaining.min(Duration::from_millis(1)));
    }
}
fn reap_owned(pid: i32, pidfd: i32, deadline: Instant) -> std::io::Result<i32> {
    loop {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            return Ok(status);
        }
        if result < 0 && unsafe { errno() } != libc::EINTR {
            return Err(std::io::Error::last_os_error());
        }
        wait_readable(pidfd, deadline)?;
    }
}

impl ReleaseService {
    pub(crate) unsafe fn start_before_guests(
        deadline: Instant,
    ) -> Result<Self, ReleaseBootstrapError> {
        unsafe { Self::start_with_fault(deadline, FAULT_NONE) }
    }
    unsafe fn start_with_fault(
        deadline: Instant,
        fault: i64,
    ) -> Result<Self, ReleaseBootstrapError> {
        Self::start_with_capacity(deadline, fault, DEFAULT_RELEASE_SLOTS)
    }
    unsafe fn start_with_capacity(
        deadline: Instant,
        fault: i64,
        capacity: usize,
    ) -> Result<Self, ReleaseBootstrapError> {
        if capacity == 0 || capacity > MAX_RELEASE_SLOTS {
            return Err(std::io::Error::other("invalid release slot capacity").into());
        }
        let incarnation = NEXT_SERVICE
            .try_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |n| {
                n.checked_add(1)
            })
            .map_err(|_| {
                ReleaseBootstrapError::from(std::io::Error::other("service incarnation exhausted"))
            })?;
        let mut action: libc::sigaction = unsafe { zeroed() };
        if unsafe { libc::sigaction(libc::SIGCHLD, null(), &mut action) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            return Err(
                std::io::Error::other("release startup requires owned, waitable children").into(),
            );
        }
        let mut pair = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let controller = unsafe { libc::getpid() };
        let pid = unsafe { libc::syscall(libc::SYS_fork) } as i32;
        if pid == 0 {
            unsafe { broker(pair[1], controller, fault, capacity, incarnation) }
        }
        if pid < 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(pair[0]);
                libc::close(pair[1]);
            }
            return Err(error.into());
        }
        unsafe {
            libc::close(pair[1]);
        }
        let control = SharedFd::new(unsafe { OwnedFd::from_raw_fd(pair[0]) });
        // Sole direct-child reaper, no auto-reap: PID cannot be reused here.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
        let pidfd_error = if pidfd < 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        let mut cleanup = BrokerCleanup {
            control,
            pid,
            pidfd: if pidfd >= 0 {
                Some(SharedFd::new(unsafe { OwnedFd::from_raw_fd(pidfd) }))
            } else {
                None
            },
            reaped_status: None,
        };
        let startup = (|| {
            if let Some(error) = pidfd_error {
                return Err(error);
            }
            wait_readable(cleanup.control.as_raw_fd(), deadline)?;
            let mut ready = Message::new(0, 0, 0, 0);
            if !unsafe { recv_message(cleanup.control.as_raw_fd(), &mut ready) }
                || !ready.valid(READY, 0)
                || ready.value != pid as i64
                || ready.incarnation != incarnation
            {
                return Err(std::io::Error::other(
                    "release broker did not acknowledge isolated startup",
                ));
            }
            eprintln!(
                "broker READY pid={} start_ticks={} pidfd={} fdinfo={:?}",
                pid,
                ready.error,
                cleanup.pidfd.as_ref().unwrap().as_raw_fd(),
                std::fs::read_to_string(format!(
                    "/proc/self/fdinfo/{}",
                    cleanup.pidfd.as_ref().unwrap().as_raw_fd()
                ))
            );
            Ok(())
        })();
        if let Err(error) = startup {
            // An expired supervisor bound is not permission to discard ownership.
            // Return all handles even if the bounded best-effort cleanup fails.
            let cleanup_error = cleanup.terminate_and_reap(deadline).err();
            return Err(ReleaseBootstrapError {
                error,
                cleanup_error,
                cleanup: Some(cleanup),
            });
        }
        let state = OwnerState {
            control: cleanup.control,
            broker_pid: pid,
            broker_pidfd: cleanup.pidfd.unwrap(),
            incarnation,
            capacity,
            next_job: 0,
            jobs: BTreeMap::new(),
            recent_receipts: VecDeque::new(),
            received_prefix: 0,
            ack_sent_prefix: 0,
            stop_requested: false,
            stop_sent: false,
            stopped_watermark: None,
            stopped_ack_sent: false,
            broker_status: None,
            error: None,
            paused_control_reads: false,
            diagnostics: None,
        };
        let owner = match ServiceOwner::launch(state) {
            Ok(owner) => owner,
            Err((error, state)) => {
                return Err(ReleaseBootstrapError {
                    error,
                    cleanup_error: None,
                    cleanup: Some(BrokerCleanup {
                        control: state.control,
                        pid: state.broker_pid,
                        pidfd: Some(state.broker_pidfd),
                        reaped_status: state.broker_status,
                    }),
                });
            }
        };
        let broker_pidfd = owner.state.lock().unwrap().broker_pidfd.clone();
        Ok(Self {
            owner,
            broker_pid: pid,
            broker_pidfd,
            stopped: false,
            reap_receipts: BTreeMap::new(),
        })
    }

    fn prepare_transfer<'pin>(
        &mut self,
        pin: &'pin mut Option<OwnedFd>,
        deadline: Instant,
    ) -> Result<ReleaseJob<'pin>, ReleasePrepareError<'pin>> {
        self.prepare_with_fault(pin, deadline, FAULT_NONE)
    }
    fn prepare_with_fault<'pin>(
        &mut self,
        pin: &'pin mut Option<OwnedFd>,
        deadline: Instant,
        fault: i64,
    ) -> Result<ReleaseJob<'pin>, ReleasePrepareError<'pin>> {
        let custody = self
            .register_with_fault(pin, deadline, fault, true)
            .map_err(|error| ReleasePrepareError { error, job: None })?;
        let mut job = custody
            .waiter(pin)
            .expect("synchronously admitted record is retained by custody capability");
        drop(custody);
        if let Err(error) = job.await_prepared(deadline) {
            return Err(ReleasePrepareError {
                error,
                job: Some(job),
            });
        }
        Ok(job)
    }
    /// Synchronous custody admission. No condition-variable wait or guest
    /// suspension occurs here. On Err the caller retains its exact original;
    /// on Ok the registry owns it and this opaque capability names that job.
    pub(crate) fn try_register(
        &self,
        pin: &mut Option<OwnedFd>,
        deadline: Instant,
    ) -> std::io::Result<ReleaseCustody> {
        self.register_with_fault(pin, deadline, FAULT_NONE, false)
    }
    fn register_with_fault(
        &self,
        pin: &mut Option<OwnedFd>,
        deadline: Instant,
        fault: i64,
        wait_for_capacity: bool,
    ) -> std::io::Result<ReleaseCustody> {
        let allocation = (|| {
            let mut state = self.owner.state.lock().unwrap();
            while state.jobs.len() >= state.capacity
                && !state.stop_requested
                && state.error.is_none()
            {
                if !wait_for_capacity {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "release custody capacity full",
                    ));
                }
                state = bounded_wait(&self.owner, state, deadline)?;
            }
            if state.stop_requested || pin.is_none() {
                return Err(std::io::Error::other("release service/pin unavailable"));
            }
            if state.jobs.values().any(|r| r.submission_uncertain) {
                return Err(std::io::Error::other(
                    "release submission stream unresolved",
                ));
            }
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            let id = state
                .next_job
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("release job identity exhausted"))?;
            let make_pair = || -> std::io::Result<(OwnedFd, OwnedFd)> {
                let mut pair = [-1; 2];
                if unsafe {
                    libc::socketpair(
                        libc::AF_UNIX,
                        libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                        0,
                        pair.as_mut_ptr(),
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) })
            };
            let (channel, remote) = make_pair()?;
            let (receiver, sender) = make_pair()?;
            if !unsafe {
                send_pidfd(
                    sender.as_raw_fd(),
                    &Message::new(ESCROW_PAYLOAD, id, 0, 0),
                    pin.as_ref().unwrap().as_raw_fd(),
                )
            } {
                return Err(std::io::Error::last_os_error());
            }
            drop(sender);
            let channel = SharedFd::new(channel);
            // Exact synchronous transfer into the persistent registry: after
            // this point a canceled waiter owns no target descriptor to Drop.
            let record = OwnedRelease {
                pin: pin.take(),
                escrow: Some(receiver),
                channel: channel.clone(),
                remote: Some(remote),
                worker_pidfd: None,
                possessed_pid: None,
                dropped_pid: None,
                id,
                fault,
                submitted: false,
                submission_uncertain: false,
                rejected_without_child: false,
                started: false,
                started_as_exit: false,
                start_pending: None,
                original_removed: false,
                interrupt_requested: false,
                terminal_requested: false,
                abort_pending: false,
                abort_sent: false,
                close_result: None,
                reaped_status: None,
                supervisor_reaped: None,
                error: None,
                channel_broken: false,
                version: 0,
                completion_waker: None,
                waiters: 1,
                outcome_observed: false,
                supervisor_deadline: deadline,
            };
            state.next_job = id;
            state.jobs.insert(id, record);
            Ok(id)
        })();
        let id = allocation?;
        let incarnation = self.owner.state.lock().unwrap().incarnation;
        self.owner.wake();
        Ok(ReleaseCustody {
            owner: self.owner.clone(),
            token: ReleaseToken {
                incarnation,
                job_id: id,
            },
        })
    }
    fn begin_shutdown(&self) {
        {
            self.owner.state.lock().unwrap().stop_requested = true;
        }
        self.owner.wake();
    }
    #[cfg(test)]
    fn pause_control_reads(&self, pause: bool) {
        {
            self.owner.state.lock().unwrap().paused_control_reads = pause;
        }
        self.owner.wake();
    }
    fn shutdown(&mut self, deadline: Instant) -> std::io::Result<()> {
        if self.stopped {
            return Err(std::io::Error::other("release service already stopped"));
        }
        self.begin_shutdown();
        let mut state = self.owner.state.lock().unwrap();
        while state.broker_status.is_none() {
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            state = bounded_wait(&self.owner, state, deadline)?;
        }
        self.reap_receipts = state
            .recent_receipts
            .iter()
            .map(|(id, status, ticks)| (*id, (*status, *ticks)))
            .collect();
        if state.broker_status != Some(0)
            || !state.stopped_ack_sent
            || state.stopped_watermark != Some(state.received_prefix)
        {
            return Err(std::io::Error::other(
                "release shutdown not fully acknowledged",
            ));
        }
        // This is broker cleanup only. Any unknown effect/custody still resides
        // in the same owner and must be settled by its explicit recovery owner.
        self.stopped = true;
        drop(state);
        if let Some(thread) = self.owner.join.lock().unwrap().take() {
            thread
                .join()
                .map_err(|_| std::io::Error::other("release owner thread failed"))?;
        }
        Ok(())
    }
    #[cfg(test)]
    fn attach<'pin>(
        &self,
        id: u64,
        holder: &'pin mut Option<OwnedFd>,
    ) -> std::io::Result<ReleaseJob<'pin>> {
        attach_owned(&self.owner, id, holder)
    }
}
impl Drop for ReleaseJob<'_> {
    fn drop(&mut self) {
        if let Some(owner) = &self.owner {
            {
                let mut state = owner.state.lock().unwrap();
                if let Some(record) = state.jobs.get_mut(&self.id) {
                    record.waiters -= 1;
                }
            }
            // Metadata lease release only. The owner retains all physical FDs,
            // in-flight effects and unobserved results independently.
            owner.wake();
        }
    }
}
impl<'pin> ReleaseJob<'pin> {
    #[cfg(test)]
    fn mock(pin: &'pin mut Option<OwnedFd>, control: OwnedFd, channel: OwnedFd, id: u64) -> Self {
        Self {
            pin: PinView {
                holder: pin,
                owner: None,
                id,
            },
            owner: None,
            broker_control: SharedFd::new(control),
            channel: SharedFd::new(channel),
            worker_pidfd: None,
            possessed_pid: None,
            dropped_pid: None,
            escrow: None,
            id,
            interrupt_requested: false,
            terminal_requested: false,
            close_result: None,
            reaped_status: None,
            submitted: true,
            rejected_without_child: false,
            started: false,
            started_as_exit: false,
        }
    }
    fn copy_record(&mut self, r: &OwnedRelease) {
        self.worker_pidfd = r.worker_pidfd.clone();
        self.possessed_pid = r.possessed_pid;
        self.dropped_pid = r.dropped_pid;
        self.escrow = r.escrow.as_ref().map(|_| ());
        self.interrupt_requested = r.interrupt_requested;
        self.terminal_requested = r.terminal_requested;
        self.close_result = r.close_result;
        self.reaped_status = r.reaped_status;
        self.submitted = r.submitted;
        self.rejected_without_child = r.rejected_without_child;
        self.started = r.started;
        self.started_as_exit = r.started_as_exit;
    }
    fn escrow_bytes(&self) -> std::io::Result<i32> {
        let owner = self
            .owner
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing escrow custody"))?;
        let state = owner.state.lock().unwrap();
        let record = state
            .jobs
            .get(&self.id)
            .ok_or_else(|| std::io::Error::other("missing exact job"))?;
        let fd = record
            .escrow
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing escrow custody"))?;
        escrow_count(fd.as_raw_fd())
    }
    fn release_empty_escrow(&mut self) -> std::io::Result<()> {
        let Some(owner) = self.owner.clone() else {
            return Ok(());
        };
        let mut state = owner.state.lock().unwrap();
        let record = state.jobs.get_mut(&self.id).unwrap();
        release_empty(record)?;
        self.copy_record(record);
        Ok(())
    }
    fn restore_unstarted(&mut self) -> std::io::Result<()> {
        let Some(owner) = self.owner.clone() else {
            return Ok(());
        };
        let mut state = owner.state.lock().unwrap();
        let record = state.jobs.get_mut(&self.id).unwrap();
        if record.started
            || record.submission_uncertain
            || record.original_removed
            || (record.submitted
                && record.supervisor_reaped.is_none()
                && record.reaped_status.is_none())
        {
            return Err(std::io::Error::other(
                "cannot transfer unresolved original custody",
            ));
        }
        if self.pin.holder.is_some() {
            return Ok(());
        }
        if record.pin.is_none() {
            return Err(std::io::Error::other("original pin already transferred"));
        }
        // The target original still exists while the redundant queued right is
        // discarded. Return it only in this explicit synchronous operation.
        drop(record.escrow.take());
        *self.pin.holder = record.pin.take();
        record.outcome_observed = true;
        self.copy_record(record);
        Ok(())
    }
    fn discard_unstarted_escrow(&mut self) -> std::io::Result<()> {
        self.restore_unstarted()
    }
    fn await_prepared(&mut self, deadline: Instant) -> std::io::Result<()> {
        if let Some(owner) = self.owner.clone() {
            let mut state = owner.state.lock().unwrap();
            loop {
                let record = state.jobs.get(&self.id).unwrap();
                self.copy_record(record);
                if prepared_record(record)? {
                    return Ok(());
                }
                if let Some(error) = &state.error {
                    return Err(error.io());
                }
                state = bounded_wait(&owner, state, deadline)?;
            }
        }
        loop {
            if self.reaped_status.is_some() {
                return Err(std::io::Error::other("release worker exited before start"));
            }
            if self.possessed_pid.is_some() && self.dropped_pid.is_some() {
                if self.possessed_pid != self.dropped_pid || self.worker_pidfd.is_none() {
                    return Err(std::io::Error::other("release handoff identities disagree"));
                }
                return Ok(());
            }
            self.receive(deadline)?;
        }
    }
    fn start(&mut self, terminal: bool) -> std::io::Result<()> {
        let owner = self
            .owner
            .clone()
            .ok_or_else(|| std::io::Error::other("mock release is not prepared"))?;
        {
            let mut state = owner.state.lock().unwrap();
            let record = state.jobs.get_mut(&self.id).unwrap();
            if record.started || record.start_pending.is_some() || !prepared_record(record)? {
                return Err(std::io::Error::other("release job not uniquely prepared"));
            }
            record.start_pending = Some(terminal || record.terminal_requested);
        }
        owner.wake();
        // Reuse this operation's existing bound; do not create a fresh timeout
        // simply because preparation consumed part of the original horizon.
        let deadline = owner
            .state
            .lock()
            .unwrap()
            .jobs
            .get(&self.id)
            .unwrap()
            .supervisor_deadline;
        let mut state = owner.state.lock().unwrap();
        loop {
            let record = state.jobs.get(&self.id).unwrap();
            self.copy_record(record);
            if let Some(error) = &record.error {
                return Err(error.io());
            }
            if record.started {
                return Ok(());
            }
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            state = bounded_wait(&owner, state, deadline)?;
        }
    }
    fn abort_and_reap(&mut self, deadline: Instant) -> std::io::Result<()> {
        let Some(owner) = self.owner.clone() else {
            return Ok(());
        };
        {
            let mut state = owner.state.lock().unwrap();
            state.jobs.get_mut(&self.id).unwrap().abort_pending = true;
        }
        owner.wake();
        let mut state = owner.state.lock().unwrap();
        loop {
            let record = state.jobs.get(&self.id).unwrap();
            self.copy_record(record);
            if record.supervisor_reaped.is_some()
                || record.reaped_status.is_some()
                || !record.submitted
            {
                break;
            }
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            state = bounded_wait(&owner, state, deadline)?;
        }
        drop(state);
        if !self.started {
            self.restore_unstarted()?;
        }
        Ok(())
    }
    fn request_interrupt(&mut self) {
        self.interrupt_requested = true;
        if let Some(owner) = &self.owner {
            {
                owner
                    .state
                    .lock()
                    .unwrap()
                    .jobs
                    .get_mut(&self.id)
                    .unwrap()
                    .interrupt_requested = true;
            }
            owner.wake();
        }
    }
    fn request_terminal(&mut self) {
        self.terminal_requested = true;
        if let Some(owner) = &self.owner {
            {
                owner
                    .state
                    .lock()
                    .unwrap()
                    .jobs
                    .get_mut(&self.id)
                    .unwrap()
                    .terminal_requested = true;
            }
            owner.wake();
        }
    }
    fn private_signal(&self, signal: i32) -> std::io::Result<()> {
        if let Some(owner) = &self.owner {
            return signal_record(
                owner.state.lock().unwrap().jobs.get(&self.id).unwrap(),
                signal,
            );
        }
        let fd = self
            .worker_pidfd
            .as_ref()
            .ok_or_else(|| std::io::Error::other("missing authenticated worker pidfd"))?;
        if unsafe { signal_pidfd(fd.as_raw_fd(), signal) } {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    fn completed_outcome(&self) -> std::io::Result<bool> {
        release_outcome_ready(
            self.started_as_exit,
            self.terminal_requested,
            self.reaped_status,
            self.close_result,
        )
    }
    fn drive_to_bound(&mut self, deadline: Instant) -> std::io::Result<()> {
        if !self.started {
            return Err(std::io::Error::other("release job not started"));
        }
        let Some(owner) = self.owner.clone() else {
            return self.completed_outcome().and_then(|complete| {
                if complete {
                    Ok(())
                } else {
                    Err(std::io::Error::other("incomplete mock"))
                }
            });
        };
        owner.wake();
        let mut state = owner.state.lock().unwrap();
        loop {
            if let Some(error) = &state.error {
                return Err(error.io());
            }
            let record = state.jobs.get_mut(&self.id).unwrap();
            self.copy_record(record);
            if let Some(error) = &record.error {
                return Err(error.io());
            }
            if self.completed_outcome()? {
                release_empty(record)?;
                record.outcome_observed = true;
                self.copy_record(record);
                return Ok(());
            }
            state = bounded_wait(&owner, state, deadline)?;
        }
    }
    fn receive(&mut self, deadline: Instant) -> std::io::Result<()> {
        if let Some(owner) = self.owner.clone() {
            let mut state = owner.state.lock().unwrap();
            let version = state.jobs.get(&self.id).unwrap().version;
            loop {
                let record = state.jobs.get(&self.id).unwrap();
                self.copy_record(record);
                if let Some(error) = &record.error {
                    return Err(error.io());
                }
                if record.version != version {
                    return Ok(());
                }
                state = bounded_wait(&owner, state, deadline)?;
            }
        }
        wait_readable(self.channel.as_raw_fd(), deadline)?;
        // Mock fixtures use the exact production packet parser. Only their
        // state storage is local because they have no broker/service owner.
        let mut r = OwnedRelease {
            pin: None,
            escrow: None,
            channel: self.channel.clone(),
            remote: None,
            worker_pidfd: self.worker_pidfd.clone(),
            possessed_pid: self.possessed_pid,
            dropped_pid: self.dropped_pid,
            id: self.id,
            fault: 0,
            submitted: self.submitted,
            submission_uncertain: false,
            rejected_without_child: self.rejected_without_child,
            started: self.started,
            started_as_exit: self.started_as_exit,
            start_pending: None,
            original_removed: false,
            interrupt_requested: self.interrupt_requested,
            terminal_requested: self.terminal_requested,
            abort_pending: false,
            abort_sent: false,
            close_result: self.close_result,
            reaped_status: self.reaped_status,
            supervisor_reaped: None,
            error: None,
            channel_broken: false,
            version: 0,
            completion_waker: None,
            waiters: 1,
            outcome_observed: false,
            supervisor_deadline: deadline,
        };
        let result = receive_owned(&mut r);
        self.copy_record(&r);
        result.map(|_| ())
    }
}

fn attach_owned<'pin>(
    owner: &Arc<ServiceOwner>,
    id: u64,
    holder: &'pin mut Option<OwnedFd>,
) -> std::io::Result<ReleaseJob<'pin>> {
    if holder.is_some() {
        return Err(std::io::Error::other("reattachment holder must be empty"));
    }
    let mut state = owner.state.lock().unwrap();
    let control = state.control.clone();
    let record = state
        .jobs
        .get_mut(&id)
        .ok_or_else(|| std::io::Error::other("unknown exact release job"))?;
    record.waiters = record
        .waiters
        .checked_add(1)
        .ok_or_else(|| std::io::Error::other("waiter count exhausted"))?;
    let mut job = ReleaseJob {
        pin: PinView {
            holder,
            owner: Some(owner.clone()),
            id,
        },
        owner: Some(owner.clone()),
        broker_control: control,
        channel: record.channel.clone(),
        worker_pidfd: None,
        possessed_pid: None,
        dropped_pid: None,
        escrow: None,
        id,
        interrupt_requested: false,
        terminal_requested: false,
        close_result: None,
        reaped_status: None,
        submitted: false,
        rejected_without_child: false,
        started: false,
        started_as_exit: false,
    };
    job.copy_record(record);
    Ok(job)
}

fn release_outcome_ready(
    started_as_exit: bool,
    terminal_requested: bool,
    reaped_status: Option<i32>,
    close_result: Option<Result<(), i32>>,
) -> std::io::Result<bool> {
    let Some(status) = reaped_status else {
        return Ok(false);
    };
    if terminal_requested {
        if (started_as_exit && status == 0)
            || (!started_as_exit
                && (status == libc::SIGKILL || (status == 0 && close_result.is_some())))
        {
            return Ok(true);
        }
        return Err(std::io::Error::other(
            "terminal release worker failed expected exit path",
        ));
    }
    if status == 0 && close_result.is_some() {
        return Ok(true);
    }
    Err(std::io::Error::other(
        "release worker exited without complete close result",
    ))
}
