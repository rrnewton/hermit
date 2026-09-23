use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering as AtomicOrdering;

#[derive(Clone)]
struct SharedFd(Arc<OwnedFd>);
impl SharedFd {
    fn new(fd: OwnedFd) -> Self {
        Self(Arc::new(fd))
    }
}
impl AsRawFd for SharedFd {
    fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}
#[derive(Clone, Debug)]
pub(crate) struct StoredError {
    kind: std::io::ErrorKind,
    message: String,
    raw_os_error: Option<i32>,
}
impl StoredError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            kind: std::io::ErrorKind::Other,
            message: message.into(),
            raw_os_error: None,
        }
    }
    fn from_io(error: std::io::Error) -> Self {
        Self {
            kind: error.kind(),
            raw_os_error: error.raw_os_error(),
            message: error.to_string(),
        }
    }
    pub(crate) fn io(&self) -> std::io::Error {
        match self.raw_os_error {
            Some(errno) => std::io::Error::from_raw_os_error(errno),
            None => std::io::Error::new(self.kind, self.message.clone()),
        }
    }
}
struct OwnedRelease {
    pin: Option<OwnedFd>,
    escrow: Option<OwnedFd>,
    channel: SharedFd,
    remote: Option<OwnedFd>,
    worker_pidfd: Option<SharedFd>,
    possessed_pid: Option<i32>,
    dropped_pid: Option<i32>,
    id: u64,
    fault: i64,
    submitted: bool,
    submission_uncertain: bool,
    rejected_without_child: bool,
    started: bool,
    started_as_exit: bool,
    start_pending: Option<bool>,
    original_removed: bool,
    interrupt_requested: bool,
    terminal_requested: bool,
    abort_pending: bool,
    abort_sent: bool,
    close_result: Option<Result<(), i32>>,
    reaped_status: Option<i32>,
    supervisor_reaped: Option<(i32, u64)>,
    error: Option<StoredError>,
    channel_broken: bool,
    version: u64,
    completion_waker: Option<(u64, std::task::Waker)>,
    waiters: usize,
    outcome_observed: bool,
    supervisor_deadline: Instant,
}
impl OwnedRelease {
    fn changed(&mut self) {
        match self.version.checked_add(1) {
            Some(version) => self.version = version,
            None => {
                self.error
                    .get_or_insert_with(|| StoredError::new("custody version exhausted"));
            }
        }
    }
}
fn can_retire(record: &OwnedRelease) -> bool {
    record.waiters == 0
        && record.outcome_observed
        && record.pin.is_none()
        && record.escrow.is_none()
        && (record.supervisor_reaped.is_some()
            || (!record.submitted && !record.submission_uncertain))
}
struct OwnerState {
    control: SharedFd,
    broker_pid: i32,
    broker_pidfd: SharedFd,
    incarnation: u64,
    capacity: usize,
    next_job: u64,
    jobs: BTreeMap<u64, OwnedRelease>,
    // Bounded recent committed diagnostics. Actual unresolved effects remain
    // in jobs and continue to occupy controller admission capacity.
    recent_receipts: VecDeque<(u64, i32, u64)>,
    received_prefix: u64,
    ack_sent_prefix: u64,
    stop_requested: bool,
    stop_sent: bool,
    stopped_watermark: Option<u64>,
    stopped_ack_sent: bool,
    broker_status: Option<i32>,
    error: Option<StoredError>,
    paused_control_reads: bool,
    diagnostics: Option<(u64, u64, usize)>,
}
struct ServiceOwner {
    state: Mutex<OwnerState>,
    changed: Condvar,
    wake: SharedFd,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}
static NEXT_SERVICE: AtomicU64 = AtomicU64::new(1);

fn bounded_wait<'a>(
    owner: &ServiceOwner,
    guard: std::sync::MutexGuard<'a, OwnerState>,
    deadline: Instant,
) -> std::io::Result<std::sync::MutexGuard<'a, OwnerState>> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "release supervisor bound")
        })?;
    let (guard, _) = owner.changed.wait_timeout(guard, remaining).unwrap();
    Ok(guard)
}
fn escrow_count(fd: i32) -> std::io::Result<i32> {
    let mut bytes = -1;
    if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut bytes) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(bytes)
}
fn release_empty(record: &mut OwnedRelease) -> std::io::Result<()> {
    if let Some(fd) = &record.escrow {
        if escrow_count(fd.as_raw_fd())? != 0 {
            return Err(std::io::Error::other(
                "unresolved data-bearing escrow remains owned",
            ));
        }
    }
    drop(record.escrow.take());
    Ok(())
}
fn prepared_record(record: &OwnedRelease) -> std::io::Result<bool> {
    if let Some(error) = &record.error {
        return Err(error.io());
    }
    if record.reaped_status.is_some() || record.supervisor_reaped.is_some() {
        return Err(std::io::Error::other("release worker exited before start"));
    }
    if record.possessed_pid.is_some() && record.dropped_pid.is_some() {
        if record.possessed_pid != record.dropped_pid || record.worker_pidfd.is_none() {
            return Err(std::io::Error::other("release handoff identities disagree"));
        }
        return Ok(true);
    }
    Ok(false)
}
fn signal_record(record: &OwnedRelease, signal: i32) -> std::io::Result<()> {
    let fd = record
        .worker_pidfd
        .as_ref()
        .ok_or_else(|| std::io::Error::other("missing authenticated worker pidfd"))?;
    if unsafe { signal_pidfd(fd.as_raw_fd(), signal) } {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
fn receive_owned(record: &mut OwnedRelease) -> std::io::Result<bool> {
    let mut message = Message::new(0, 0, 0, 0);
    let mut fds = [-1; 2];
    let mut count = 0;
    let read = unsafe {
        recv_packet(
            record.channel.as_raw_fd(),
            &mut message,
            &mut fds,
            &mut count,
        )
    };
    if read == -libc::EAGAIN || read == -libc::EINTR {
        return Ok(false);
    }
    if read != 1 {
        record.channel_broken = true;
        return Err(std::io::Error::other("release packet receive failed"));
    }
    let mut rights: Vec<SharedFd> = fds[..count]
        .iter()
        .map(|fd| SharedFd::new(unsafe { OwnedFd::from_raw_fd(*fd) }))
        .collect();
    if message.magic != MAGIC || message.version != VERSION || message.job != record.id {
        return Err(std::io::Error::other("release completion identity failure"));
    }
    eprintln!(
        "job={} receipt_kind={} value={} error_or_start_ticks={} rights={:?}",
        record.id,
        message.kind,
        message.value,
        message.error,
        &fds[..count]
    );
    match message.kind {
        POSSESSED
            if !record.started
                && record.possessed_pid.is_none()
                && count == 0
                && message.value > 0 =>
        {
            record.possessed_pid = Some(message.value as i32)
        }
        BROKER_DROPPED
            if !record.started
                && record.dropped_pid.is_none()
                && count == 1
                && message.value > 0 =>
        {
            record.dropped_pid = Some(message.value as i32);
            record.worker_pidfd = rights.pop();
        }
        CLOSED if record.started && record.close_result.is_none() && count == 0 => {
            record.close_result = Some(if message.value == 0 && message.error == 0 {
                Ok(())
            } else if message.value == -1 && message.error > 0 {
                Err(message.error as i32)
            } else {
                return Err(std::io::Error::other("invalid close result"));
            });
        }
        REAPED if record.reaped_status.is_none() && count == 0 => {
            if record
                .supervisor_reaped
                .is_some_and(|(status, _)| status != message.value as i32)
            {
                return Err(std::io::Error::other(
                    "private and supervisor reap status disagree",
                ));
            }
            record.reaped_status = Some(message.value as i32)
        }
        FAILED if count == 0 && (message.value == 0 || message.value == 1) => {
            record.rejected_without_child = message.value == 0;
            return Err(std::io::Error::other(format!(
                "release broker rejected job: errno {}",
                message.error
            )));
        }
        _ => {
            return Err(std::io::Error::other(
                "duplicate/unexpected release packet or rights",
            ));
        }
    }
    record.changed();
    Ok(true)
}
fn send_submission(control: i32, record: &OwnedRelease, incarnation: u64) -> std::io::Result<bool> {
    let remote = record
        .remote
        .as_ref()
        .ok_or_else(|| std::io::Error::other("missing submission channel"))?;
    let escrow = record
        .escrow
        .as_ref()
        .ok_or_else(|| std::io::Error::other("missing submission escrow"))?;
    let message = Message::new(SUBMIT, record.id, record.fault, 0).receipt(0, incarnation);
    let mut iov = libc::iovec {
        iov_base: (&message as *const Message).cast_mut().cast(),
        iov_len: size_of::<Message>(),
    };
    let mut bytes = [0u64; 8];
    let mut header: libc::msghdr = unsafe { zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = bytes.as_mut_ptr().cast();
    header.msg_controllen = unsafe { libc::CMSG_SPACE((2 * size_of::<i32>()) as u32) } as usize;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&header);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((2 * size_of::<i32>()) as u32) as usize;
        let rights = libc::CMSG_DATA(c).cast::<i32>();
        *rights = escrow.as_raw_fd();
        *rights.add(1) = remote.as_raw_fd();
    }
    let n = unsafe { libc::sendmsg(control, &header, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    if n == size_of::<Message>() as isize {
        return Ok(true);
    }
    if n < 0 && (unsafe { errno() } == libc::EAGAIN || unsafe { errno() } == libc::EINTR) {
        return Ok(false);
    }
    Err(std::io::Error::other(
        "release submission outcome unknown; registry retains all custody",
    ))
}
impl ServiceOwner {
    fn wake(&self) {
        let value = 1u64;
        loop {
            let n = unsafe { libc::write(self.wake.as_raw_fd(), (&value as *const u64).cast(), 8) };
            if n == 8 || (n < 0 && unsafe { errno() } == libc::EAGAIN) {
                break;
            }
            if n < 0 && unsafe { errno() } == libc::EINTR {
                continue;
            }
            let mut state = self.state.lock().unwrap();
            state
                .error
                .get_or_insert_with(|| StoredError::new("release service wake failed"));
            self.changed.notify_all();
            break;
        }
    }
    fn launch(state: OwnerState) -> Result<Arc<Self>, (std::io::Error, OwnerState)> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err((std::io::Error::last_os_error(), state));
        }
        let owner = Arc::new(Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            wake: SharedFd::new(unsafe { OwnedFd::from_raw_fd(fd) }),
            join: Mutex::new(None),
        });
        let driver = Arc::clone(&owner);
        let thread = match std::thread::Builder::new()
            .name("release-owner".into())
            .spawn(move || driver.run())
        {
            Ok(thread) => thread,
            Err(error) => {
                // spawn failure drops its closure/Arc; the original owner still
                // retains every bootstrap capability and no thread was started.
                let unique = match Arc::try_unwrap(owner) {
                    Ok(owner) => owner,
                    Err(_) => unreachable!("unstarted owner has no other handles"),
                };
                return Err((error, unique.state.into_inner().unwrap()));
            }
        };
        *owner.join.lock().unwrap() = Some(thread);
        Ok(owner)
    }
    fn run(self: Arc<Self>) {
        loop {
            let (mut pollfds, held, interrupt_pending, finished, wakers) = {
                let mut state = self.state.lock().unwrap();
                let mut wake_value = 0u64;
                unsafe {
                    libc::read(
                        self.wake.as_raw_fd(),
                        (&mut wake_value as *mut u64).cast(),
                        8,
                    );
                }
                let control = state.control.as_raw_fd();
                let incarnation = state.incarnation;
                let capacity = state.capacity;
                // Exactly this driver reads control, throughout the whole run.
                if !state.paused_control_reads && state.broker_status.is_none() {
                    for _ in 0..capacity + 2 {
                        let mut message = Message::new(0, 0, 0, 0);
                        let mut fds = [-1; 2];
                        let mut count = 0;
                        let read =
                            unsafe { recv_packet(control, &mut message, &mut fds, &mut count) };
                        if read == -libc::EAGAIN || read == -libc::EINTR {
                            break;
                        }
                        if count != 0 {
                            for fd in fds[..count].iter() {
                                unsafe {
                                    libc::close(*fd);
                                }
                            }
                        }
                        if read == 0 {
                            break;
                        } // exact broker pidfd below supplies termination proof
                        if read != 1
                            || count != 0
                            || message.magic != MAGIC
                            || message.version != VERSION
                            || message.incarnation != incarnation
                        {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("invalid supervisor receipt")
                            });
                            break;
                        }
                        if message.valid(BROKER_DIAGNOSTICS, 0) {
                            if state.diagnostics.is_some()
                                || !state.stop_sent
                                || message.sequence > capacity as u64
                                || message.error as u64 > state.received_prefix
                            {
                                state.error.get_or_insert_with(|| {
                                    StoredError::new("invalid broker diagnostic receipt")
                                });
                                break;
                            }
                            state.diagnostics = Some((
                                message.value as u64,
                                message.error as u64,
                                message.sequence as usize,
                            ));
                            continue;
                        }
                        if message.valid(STOPPED, 0) {
                            if !state.stop_sent
                                || state.diagnostics.is_none()
                                || state.stopped_watermark.is_some()
                                || message.sequence != state.received_prefix
                            {
                                state.error.get_or_insert_with(|| {
                                    StoredError::new("invalid STOPPED completion watermark")
                                });
                                break;
                            }
                            state.stopped_watermark = Some(message.sequence);
                            continue;
                        }
                        if !(message.kind == REAPED || message.kind == NO_CHILD_RECEIPT)
                            || state.received_prefix.checked_add(1) != Some(message.sequence)
                        {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("supervisor sequence gap/duplicate")
                            });
                            break;
                        }
                        let Some(record) = state.jobs.get_mut(&message.job) else {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("receipt has no exact durable job")
                            });
                            break;
                        };
                        if record.supervisor_reaped.is_some() || !record.submitted {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("duplicate or unsubmitted supervisor receipt")
                            });
                            break;
                        }
                        if record
                            .reaped_status
                            .is_some_and(|status| status != message.value as i32)
                        {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("supervisor and private reap status disagree")
                            });
                            break;
                        }
                        // Commit before ACK. Unknown CLOSED/read/drain/copy state
                        // is independent and is never cleared by this transition.
                        record.supervisor_reaped =
                            Some((message.value as i32, message.error as u64));
                        if message.kind == NO_CHILD_RECEIPT {
                            record.rejected_without_child = true;
                        }
                        record.changed();
                        state.received_prefix = message.sequence;
                        if state.recent_receipts.len() == capacity {
                            state.recent_receipts.pop_front();
                        }
                        state.recent_receipts.push_back((
                            message.job,
                            message.value as i32,
                            message.error as u64,
                        ));
                        eprintln!(
                            "SUPERVISOR_REAPED broker={} job={} wait_status={} start_ticks={} sequence={} incarnation={}",
                            state.broker_pid,
                            message.job,
                            message.value,
                            message.error,
                            message.sequence,
                            incarnation
                        );
                    }
                }
                let mut output_pending = false;
                if state.ack_sent_prefix < state.received_prefix {
                    let ack = Message::new(RECEIPT_ACK, 0, 0, 0)
                        .receipt(state.received_prefix, incarnation);
                    if unsafe { send_message(control, &ack) } {
                        state.ack_sent_prefix = state.received_prefix;
                    } else if unsafe { errno() } == libc::EAGAIN {
                        output_pending = true;
                    } else {
                        state.error.get_or_insert_with(|| {
                            StoredError::new(
                                "supervisor ACK send failed; receipts remain committed",
                            )
                        });
                    }
                }
                // No fresh submission overtakes an unsent ACK, so credits are
                // observed by the broker before their later ordered reuse.
                let mut admit = state.ack_sent_prefix == state.received_prefix
                    && !state.stop_requested
                    && state.error.is_none()
                    && !state.jobs.values().any(|r| r.submission_uncertain);
                for record in state.jobs.values_mut() {
                    if !record.submitted && !record.abort_pending && record.error.is_none() && admit
                    {
                        match send_submission(control, record, incarnation) {
                            Ok(true) => {
                                record.submitted = true;
                                drop(record.remote.take());
                                record.changed();
                            }
                            Ok(false) => {
                                output_pending = true;
                                admit = false;
                            }
                            Err(error) => {
                                record.submission_uncertain = true;
                                admit = false;
                                record
                                    .error
                                    .get_or_insert_with(|| StoredError::from_io(error));
                                record.changed();
                            }
                        }
                    }
                    if record.submitted && !record.channel_broken {
                        for _ in 0..4 {
                            match receive_owned(record) {
                                Ok(true) => (),
                                Ok(false) => break,
                                Err(error) => {
                                    // EOF after actual REAPED is expected; no
                                    // completion is inferred from EOF itself.
                                    if record.reaped_status.is_none()
                                        && !record.rejected_without_child
                                    {
                                        record
                                            .error
                                            .get_or_insert_with(|| StoredError::from_io(error));
                                    }
                                    record.changed();
                                    break;
                                }
                            }
                        }
                    }
                    if record.abort_pending && !record.abort_sent && record.submitted {
                        let abort = Message::new(ABORT, record.id, 0, 0).receipt(0, incarnation);
                        if unsafe { send_message(control, &abort) } {
                            record.abort_sent = true;
                        } else if unsafe { errno() } == libc::EAGAIN {
                            output_pending = true;
                        } else {
                            record.error.get_or_insert_with(|| {
                                StoredError::new("release abort send failed")
                            });
                        }
                    }
                    if let Some(terminal) = record.start_pending {
                        if record.error.is_none() {
                            if !record.original_removed {
                                let preflight = (|| {
                                    if !prepared_record(record)? {
                                        return Err(std::io::Error::other(
                                            "release job not uniquely prepared",
                                        ));
                                    }
                                    let mut p = libc::pollfd {
                                        fd: record.worker_pidfd.as_ref().unwrap().as_raw_fd(),
                                        events: libc::POLLIN,
                                        revents: 0,
                                    };
                                    if unsafe { libc::poll(&mut p, 1, 0) } != 0 {
                                        return Err(std::io::Error::other(
                                            "prepared worker died or identity poll failed",
                                        ));
                                    }
                                    let escrow = record.escrow.as_ref().ok_or_else(|| {
                                        std::io::Error::other("missing escrow custody")
                                    })?;
                                    if escrow_count(escrow.as_raw_fd())?
                                        != size_of::<Message>() as i32
                                    {
                                        return Err(std::io::Error::other(
                                            "escrow custody record missing",
                                        ));
                                    }
                                    let pin = record.pin.take().ok_or_else(|| {
                                        std::io::Error::other("owned original pin missing")
                                    })?;
                                    // Only this registry transition closes the original;
                                    // the queued escrow cannot be dequeued before START.
                                    record.original_removed = true;
                                    if unsafe { libc::close(pin.into_raw_fd()) } != 0 {
                                        return Err(std::io::Error::last_os_error());
                                    }
                                    Ok(())
                                })();
                                if let Err(error) = preflight {
                                    record
                                        .error
                                        .get_or_insert_with(|| StoredError::from_io(error));
                                    record.start_pending = None;
                                    record.changed();
                                }
                            }
                            if record.original_removed && record.error.is_none() {
                                let terminal = terminal || record.terminal_requested;
                                if unsafe {
                                    send_message(
                                        record.channel.as_raw_fd(),
                                        &Message::new(
                                            if terminal { START_EXIT } else { START_NORMAL },
                                            record.id,
                                            0,
                                            0,
                                        ),
                                    )
                                } {
                                    record.started = true;
                                    record.started_as_exit = terminal;
                                    record.terminal_requested = terminal;
                                    record.start_pending = None;
                                    record.changed();
                                    eprintln!(
                                        "job={} START terminal={} worker_pid={:?} worker_pidfd={:?}",
                                        record.id,
                                        terminal,
                                        record.possessed_pid,
                                        record.worker_pidfd.as_ref().map(AsRawFd::as_raw_fd)
                                    );
                                } else if unsafe { errno() } == libc::EAGAIN {
                                    // Register writable readiness on this job's
                                    // channel, not the unrelated control socket.
                                } else {
                                    record.error.get_or_insert_with(|| {
                                        StoredError::new("release start outcome unresolved")
                                    });
                                    record.changed();
                                }
                            }
                        }
                    }
                    if record.started
                        && record.reaped_status.is_none()
                        && record.supervisor_reaped.is_none()
                    {
                        let signal = if record.terminal_requested && !record.started_as_exit {
                            Some(libc::SIGKILL)
                        } else if record.interrupt_requested && record.close_result.is_none() {
                            Some(PRIVATE_INTERRUPT)
                        } else {
                            None
                        };
                        if let Some(signal) = signal {
                            if let Err(error) = signal_record(record, signal) {
                                record
                                    .error
                                    .get_or_insert_with(|| StoredError::from_io(error));
                            }
                        }
                    }
                }
                if state.stop_requested && !state.stop_sent {
                    if unsafe {
                        send_message(
                            control,
                            &Message::new(STOP, 0, 0, 0).receipt(0, incarnation),
                        )
                    } {
                        state.stop_sent = true;
                    } else if unsafe { errno() } == libc::EAGAIN {
                        output_pending = true;
                    } else {
                        state
                            .error
                            .get_or_insert_with(|| StoredError::new("release STOP send failed"));
                    }
                }
                if let Some(mark) = state.stopped_watermark {
                    if !state.stopped_ack_sent {
                        if unsafe {
                            send_message(
                                control,
                                &Message::new(STOPPED_ACK, 0, 0, 0).receipt(mark, incarnation),
                            )
                        } {
                            state.stopped_ack_sent = true;
                        } else if unsafe { errno() } == libc::EAGAIN {
                            output_pending = true;
                        } else {
                            state.error.get_or_insert_with(|| {
                                StoredError::new("release STOPPED acknowledgement failed")
                            });
                        }
                    }
                }
                if state.broker_status.is_none() {
                    let mut status = 0;
                    let result =
                        unsafe { libc::waitpid(state.broker_pid, &mut status, libc::WNOHANG) };
                    if result == state.broker_pid {
                        state.broker_status = Some(status);
                        if status != 0 || !state.stopped_ack_sent {
                            state.error.get_or_insert_with(|| {
                                StoredError::new(
                                    "broker exited without complete acknowledged shutdown",
                                )
                            });
                        }
                        eprintln!("broker REAPED pid={} status={}", state.broker_pid, status);
                    } else if result < 0 && unsafe { errno() } != libc::EINTR {
                        state
                            .error
                            .get_or_insert_with(|| StoredError::new("owned broker reaping failed"));
                    }
                }
                // Take observation callbacks under the lock, invoke/drop them
                // outside it. Even a user-defined RawWaker cannot reenter this
                // mutex while the driver still holds it.
                let service_terminal = state.error.is_some() || state.broker_status.is_some();
                let wakers: Vec<_> = state
                    .jobs
                    .values_mut()
                    .filter_map(|r| {
                        let ready = service_terminal
                            || can_retire(r)
                            || r.error.is_some()
                            || r.completion_waker
                                .as_ref()
                                .is_some_and(|(version, _)| *version != r.version);
                        if ready {
                            r.completion_waker.take().map(|(_, waker)| waker)
                        } else {
                            None
                        }
                    })
                    .collect();
                // Only fully known, observed outcomes with no waiter and no
                // target custody retire. Cancellation alone cannot meet this.
                state.jobs.retain(|_, r| !can_retire(r));
                self.changed.notify_all();
                let mut held = vec![
                    self.wake.clone(),
                    state.control.clone(),
                    state.broker_pidfd.clone(),
                ];
                let mut polls = vec![
                    libc::pollfd {
                        fd: self.wake.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: control,
                        events: if state.paused_control_reads {
                            0
                        } else {
                            libc::POLLIN
                        } | if output_pending { libc::POLLOUT } else { 0 },
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: state.broker_pidfd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                let mut interrupt_pending = false;
                for r in state.jobs.values() {
                    if !r.channel_broken {
                        held.push(r.channel.clone());
                        polls.push(libc::pollfd {
                            fd: r.channel.as_raw_fd(),
                            events: libc::POLLIN
                                | if r.start_pending.is_some()
                                    && r.original_removed
                                    && r.error.is_none()
                                {
                                    libc::POLLOUT
                                } else {
                                    0
                                },
                            revents: 0,
                        });
                    }
                    interrupt_pending |= r.started
                        && r.close_result.is_none()
                        && (r.interrupt_requested || r.terminal_requested);
                }
                (
                    polls,
                    held,
                    interrupt_pending,
                    state.broker_status.is_some(),
                    wakers,
                )
            };
            for waker in wakers {
                waker.wake();
            }
            if finished {
                break;
            }
            let interval = libc::timespec {
                tv_sec: 0,
                tv_nsec: 1_000_000,
            };
            unsafe {
                libc::ppoll(
                    pollfds.as_mut_ptr(),
                    pollfds.len() as libc::nfds_t,
                    if interrupt_pending { &interval } else { null() },
                    null(),
                );
            }
            drop(held);
        }
        self.changed.notify_all();
    }
}
