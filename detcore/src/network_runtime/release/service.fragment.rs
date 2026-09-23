// Native-only Linux release service revision5. Not enrolled in Hermit.
// The controller starts the broker before any guest/host socket pins exist.
// Child paths use only fixed stack data and libc syscalls; no Rust allocator,
// async runtime, inherited locks, logging, formatting, panics, or destructors.
// The continuous physical-custody registry is included; scheduler/trace integration is separate.
use std::mem::size_of;
use std::mem::zeroed;
use std::ptr::null;
use std::ptr::null_mut;

const MAGIC: u64 = 0x48524d5452454c31;
const VERSION: u64 = 2;
const SUBMIT: u64 = 1;
const POSSESSED: u64 = 2;
const START_NORMAL: u64 = 3;
const START_EXIT: u64 = 4;
const CLOSED: u64 = 5;
const REAPED: u64 = 6;
const FAILED: u64 = 7;
const READY: u64 = 8;
const BROKER_DROPPED: u64 = 9;
const ABORT: u64 = 10;
const STOP: u64 = 11;
const STOPPED: u64 = 12;
const ESCROW_PAYLOAD: u64 = 13;
const RECEIPT_ACK: u64 = 14;
const STOPPED_ACK: u64 = 15;
const NO_CHILD_RECEIPT: u64 = 16;
const BROKER_DIAGNOSTICS: u64 = 17;
const FAULT_NONE: i64 = 0;
const FAULT_NO_READY: i64 = 1;
const FAULT_BEFORE_POSSESSED: i64 = 2;
const FAULT_NO_DROP_BARRIER: i64 = 3;
const FAULT_SMALL_SUPERVISOR_QUEUE: i64 = 4;
const PRIVATE_INTERRUPT: libc::c_int = libc::SIGUSR1;

#[repr(C)]
#[derive(Clone, Copy)]
struct Message {
    magic: u64,
    version: u64,
    kind: u64,
    job: u64,
    value: i64,
    error: i64,
    sequence: u64,
    incarnation: u64,
}
impl Message {
    const fn new(kind: u64, job: u64, value: i64, error: i64) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            kind,
            job,
            value,
            error,
            sequence: 0,
            incarnation: 0,
        }
    }
    fn receipt(mut self, sequence: u64, incarnation: u64) -> Self {
        self.sequence = sequence;
        self.incarnation = incarnation;
        self
    }
    fn valid(&self, kind: u64, job: u64) -> bool {
        self.magic == MAGIC && self.version == VERSION && self.kind == kind && self.job == job
    }
}

// Only sig_atomic_t operations would be safe in this handler. No flag is
// necessary: the controller retains the level-triggered interruption request.
extern "C" fn interrupt_handler(_: libc::c_int) {}
extern "C" fn child_handler(_: libc::c_int) {}

unsafe fn errno() -> i32 {
    unsafe { *libc::__errno_location() }
}
unsafe fn send_message(fd: i32, message: &Message) -> bool {
    loop {
        let n = unsafe {
            libc::send(
                fd,
                (message as *const Message).cast(),
                size_of::<Message>(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if n == size_of::<Message>() as isize {
            return true;
        }
        if n < 0 && unsafe { errno() } == libc::EINTR {
            continue;
        }
        return false;
    }
}
unsafe fn recv_message(fd: i32, message: &mut Message) -> bool {
    loop {
        let n = unsafe {
            libc::recv(
                fd,
                (message as *mut Message).cast(),
                size_of::<Message>(),
                libc::MSG_TRUNC,
            )
        };
        if n == size_of::<Message>() as isize {
            return true;
        }
        if n < 0 && unsafe { errno() } == libc::EINTR {
            continue;
        }
        return false;
    }
}
unsafe fn raw_close_range(first: u32, last: u32) -> bool {
    first > last || unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } == 0
}
unsafe fn close_except_two(a: i32, b: i32) -> bool {
    let low = a.min(b) as u32;
    let high = a.max(b) as u32;
    (low == 0 || unsafe { raw_close_range(0, low - 1) })
        && (low + 1 == high || unsafe { raw_close_range(low + 1, high - 1) })
        && unsafe { raw_close_range(high + 1, u32::MAX) }
}

// Diagnostic starttime only. Signals always use an owned pidfd, or a direct
// unreaped child in the sole-reaper bootstrap. No identity is reconstructed.
unsafe fn proc_start_ticks(pid: i32) -> u64 {
    let mut path = [0u8; 64];
    path[..6].copy_from_slice(b"/proc/");
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut value = pid as u32;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for i in 0..count {
        path[6 + i] = digits[count - 1 - i];
    }
    path[6 + count..11 + count].copy_from_slice(b"/stat");
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return 0;
    }
    let mut bytes = [0u8; 2048];
    let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    unsafe {
        libc::close(fd);
    }
    if n <= 0 {
        return 0;
    }
    let bytes = &bytes[..n as usize];
    let Some(end) = bytes.iter().rposition(|c| *c == b')') else {
        return 0;
    };
    let mut field = 3;
    let mut index = end + 2;
    while field < 22 && index < bytes.len() {
        while index < bytes.len() && bytes[index] != b' ' {
            index += 1;
        }
        index += 1;
        field += 1;
    }
    let mut ticks = 0u64;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        ticks = ticks * 10 + (bytes[index] - b'0') as u64;
        index += 1;
    }
    ticks
}

/// Child worker owns the escrow receiver and private job channel, then the socket.
/// Its parent retains the other job endpoint until waitpid confirms reaping.
unsafe fn worker(escrow: i32, channel: i32, job: u64, broker: i32, fault: i64) -> ! {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0
        || unsafe { libc::getppid() } != broker
        || !unsafe { close_except_two(escrow, channel) }
    {
        unsafe { libc::_exit(125) }
    }
    if fault == FAULT_BEFORE_POSSESSED {
        unsafe { libc::_exit(125) }
    }
    let mut action: libc::sigaction = unsafe { zeroed() };
    action.sa_sigaction = interrupt_handler as *const () as usize;
    action.sa_flags = 0; // specifically no SA_RESTART
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    let mut mask: libc::sigset_t = unsafe { zeroed() };
    unsafe {
        libc::sigfillset(&mut mask);
        libc::sigdelset(&mut mask, PRIVATE_INTERRUPT);
    }
    if unsafe { libc::sigaction(PRIVATE_INTERRUPT, &action, null_mut()) } != 0
        || unsafe { libc::sigprocmask(libc::SIG_SETMASK, &mask, null_mut()) } != 0
        || !unsafe {
            send_message(
                channel,
                &Message::new(
                    POSSESSED,
                    job,
                    libc::getpid() as i64,
                    proc_start_ticks(libc::getpid()) as i64,
                ),
            )
        }
    {
        unsafe { libc::_exit(125) }
    }
    let mut start = Message::new(0, 0, 0, 0);
    if !unsafe { recv_message(channel, &mut start) }
        || !(start.valid(START_NORMAL, job) || start.valid(START_EXIT, job))
    {
        unsafe { libc::_exit(125) }
    }
    // Only START authorizes dequeue, after controller closed its original pin.
    // Receiver remains installed in controller until queue-empty is proved.
    let mut payload = Message::new(0, 0, 0, 0);
    let mut rights = [-1; 2];
    let mut count = 0;
    if unsafe { recv_packet(escrow, &mut payload, &mut rights, &mut count) } != 1
        || count != 1
        || !payload.valid(ESCROW_PAYLOAD, job)
    {
        // Any installed rights are released by exit, not a controller close.
        unsafe { libc::_exit(125) }
    }
    let socket = rights[0];
    unsafe {
        libc::close(escrow);
    }
    if start.kind == START_EXIT {
        // Leave socket installed. Linux exit_files performs its last fput with
        // PF_EXITING, so SO_LINGER is not substituted by a normal worker close.
        unsafe { libc::_exit(0) }
    }
    let result = unsafe { libc::close(socket) };
    let error = if result < 0 { unsafe { errno() } } else { 0 };
    // This reports the actual close return, never infers it from child status.
    let delivered = unsafe {
        send_message(
            channel,
            &Message::new(CLOSED, job, result as i64, error as i64),
        )
    };
    unsafe { libc::_exit(if delivered { 0 } else { 125 }) }
}

/// Receive exactly a socket payload plus a separately declared IPC endpoint.
/// Authentication of the socket itself occurred at pidfd_getfd in the parent.
/// Unexpected rights are closed and refused; no fd is selected by guesswork.
unsafe fn recv_packet(
    control: i32,
    message: &mut Message,
    fds: &mut [i32; 2],
    count_out: &mut usize,
) -> i32 {
    let mut bytes = [0u64; 8]; // cmsghdr alignment, enough to detect extra rights
    let mut iov = libc::iovec {
        iov_base: (message as *mut Message).cast(),
        iov_len: size_of::<Message>(),
    };
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = bytes.as_mut_ptr().cast();
    msg.msg_controllen = size_of::<[u64; 8]>();
    let n = unsafe {
        libc::recvmsg(
            control,
            &mut msg,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC | libc::MSG_TRUNC,
        )
    };
    if n < 0 {
        return -unsafe { errno() };
    }
    if n == 0 {
        return 0;
    }
    let mut found = 0usize;
    let mut valid = n == size_of::<Message>() as isize && msg.msg_flags & libc::MSG_CTRUNC == 0;
    let mut header = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !header.is_null() {
        let h = unsafe { &*header };
        if h.cmsg_level != libc::SOL_SOCKET
            || h.cmsg_type != libc::SCM_RIGHTS
            || h.cmsg_len < unsafe { libc::CMSG_LEN(0) } as usize
        {
            valid = false;
            break;
        }
        let count = (h.cmsg_len - unsafe { libc::CMSG_LEN(0) } as usize) / size_of::<i32>();
        let rights = unsafe { libc::CMSG_DATA(header).cast::<i32>() };
        for i in 0..count {
            let fd = unsafe { *rights.add(i) };
            if found < 2 {
                fds[found] = fd;
            } else {
                unsafe {
                    libc::close(fd);
                }
                valid = false;
            }
            found += 1;
        }
        header = unsafe { libc::CMSG_NXTHDR(&msg, header) };
    }
    if !valid || found > 2 {
        for fd in fds.iter().copied().filter(|fd| *fd >= 0) {
            unsafe {
                libc::close(fd);
            }
        }
        *fds = [-1; 2];
        return -libc::EPROTO;
    }
    *count_out = found;
    1
}

/// Send the exact kernel pidfd acquired while the child is still unreaped.
/// No recipient opens a pidfd from a numeric PID in a message.
unsafe fn send_pidfd(channel: i32, message: &Message, pidfd: i32) -> bool {
    let mut iov = libc::iovec {
        iov_base: (message as *const Message).cast_mut().cast(),
        iov_len: size_of::<Message>(),
    };
    let mut bytes = [0u64; 8];
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = bytes.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<i32>() as u32) } as usize;
    unsafe {
        let h = libc::CMSG_FIRSTHDR(&msg);
        (*h).cmsg_level = libc::SOL_SOCKET;
        (*h).cmsg_type = libc::SCM_RIGHTS;
        (*h).cmsg_len = libc::CMSG_LEN(size_of::<i32>() as u32) as usize;
        *libc::CMSG_DATA(h).cast::<i32>() = pidfd;
    }
    loop {
        let n = unsafe { libc::sendmsg(channel, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
        if n == size_of::<Message>() as isize {
            return true;
        }
        if n < 0 && unsafe { errno() } == libc::EINTR {
            continue;
        }
        return false;
    }
}
unsafe fn signal_slot(slot: &BrokerSlot, signal: i32) -> bool {
    if slot.phase != SlotPhase::Active {
        return true;
    }
    if slot.pidfd >= 0 {
        return unsafe { signal_pidfd(slot.pidfd, signal) };
    }
    // Only the still-unreaped direct child in this sole reaper's live slot.
    (unsafe { libc::kill(slot.pid, signal) }) == 0 || unsafe { errno() } == libc::ESRCH
}
unsafe fn signal_pidfd(pidfd: i32, signal: i32) -> bool {
    (unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd,
            signal,
            null::<libc::siginfo_t>(),
            0u32,
        )
    }) == 0
        || unsafe { errno() } == libc::ESRCH
}

/// Persistent, single-threaded fork broker. It never performs normal final
/// socket close and never waits for one release before dispatching another.
/// It must start before guest descriptors/pins exist, and announce READY before
/// the controller creates them. Kernel close_range support is a prerequisite.
unsafe fn broker(
    control: i32,
    controller: i32,
    fault: i64,
    capacity: usize,
    incarnation: u64,
) -> ! {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0
        || unsafe { libc::getppid() } != controller
    {
        unsafe { libc::_exit(125) }
    }
    if control != 3 && unsafe { libc::dup3(control, 3, libc::O_CLOEXEC) } != 3 {
        unsafe { libc::_exit(125) }
    }
    if !unsafe { raw_close_range(0, 2) } || !unsafe { raw_close_range(4, u32::MAX) } {
        unsafe { libc::_exit(125) }
    }
    let control = 3;
    if fault == FAULT_SMALL_SUPERVISOR_QUEUE {
        let bytes = 4096i32;
        if unsafe {
            libc::setsockopt(
                control,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&bytes as *const i32).cast(),
                size_of::<i32>() as u32,
            )
        } != 0
        {
            unsafe { libc::_exit(125) }
        }
    }
    let mut action: libc::sigaction = unsafe { zeroed() };
    action.sa_sigaction = child_handler as *const () as usize;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
    }
    let mut blocked: libc::sigset_t = unsafe { zeroed() };
    unsafe {
        libc::sigfillset(&mut blocked);
    }
    if fault == FAULT_NO_READY {
        loop {
            unsafe {
                libc::pause();
            }
        }
    }
    if unsafe { libc::sigaction(libc::SIGCHLD, &action, null_mut()) } != 0
        || unsafe { libc::sigprocmask(libc::SIG_SETMASK, &blocked, null_mut()) } != 0
        || !unsafe {
            send_message(
                control,
                &Message::new(
                    READY,
                    0,
                    libc::getpid() as i64,
                    proc_start_ticks(libc::getpid()) as i64,
                )
                .receipt(0, incarnation),
            )
        }
    {
        unsafe { libc::_exit(125) }
    }
    let mut wait_mask = blocked;
    unsafe {
        libc::sigdelset(&mut wait_mask, libc::SIGCHLD);
    }
    let Ok(mut credits) = BrokerCredits::new(capacity) else {
        unsafe { libc::_exit(125) }
    };
    let mut stopping = false;
    let mut disconnected = false;
    let mut stopped_sent = false;
    let mut diagnostics_sent = false;
    let mut supervisor_eagain = 0u64;
    let mut first_eagain_after_sent = None;
    let mut last_sent = 0u64;
    let mut peak_slots = 0usize;
    loop {
        // Completion records replace active slots in place. No slot or exact
        // status is freed when output blocks, or before a committed ACK.
        for _ in 0..capacity {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            let Some(index) = credits.slots[..capacity]
                .iter()
                .position(|s| s.phase == SlotPhase::Active && s.pid == pid)
            else {
                unsafe { libc::_exit(125) }
            };
            if credits.terminal(index, status, false).is_err() {
                unsafe { libc::_exit(125) }
            }
            let slot = credits.slots[index];
            unsafe {
                send_message(
                    slot.channel,
                    &Message::new(REAPED, slot.id, status as i64, slot.start_ticks as i64),
                );
            }
        }
        if disconnected && !credits.active() {
            unsafe { libc::_exit(125) }
        }
        let mut output_blocked = false;
        for _ in 0..capacity {
            let Some(index) = credits.next_unsent() else {
                break;
            };
            let slot = credits.slots[index];
            let message = Message::new(
                if slot.no_child {
                    NO_CHILD_RECEIPT
                } else {
                    REAPED
                },
                slot.id,
                slot.status as i64,
                slot.start_ticks as i64,
            )
            .receipt(slot.sequence, incarnation);
            if unsafe { send_message(control, &message) } {
                if credits.sent(index).is_err() {
                    unsafe { libc::_exit(125) }
                }
                last_sent = slot.sequence;
            } else if unsafe { errno() } == libc::EAGAIN {
                let Some(count) = supervisor_eagain.checked_add(1) else {
                    unsafe { libc::_exit(125) }
                };
                supervisor_eagain = count;
                first_eagain_after_sent.get_or_insert(last_sent);
                output_blocked = true;
                break;
            } else {
                disconnected = true;
                stopping = true;
                for slot in &credits.slots[..capacity] {
                    unsafe {
                        signal_slot(slot, libc::SIGKILL);
                    }
                }
                break;
            }
        }
        if stopping && !disconnected && credits.occupied() == 0 && !diagnostics_sent {
            let diagnostics = Message::new(
                BROKER_DIAGNOSTICS,
                0,
                supervisor_eagain as i64,
                first_eagain_after_sent.unwrap_or(0) as i64,
            )
            .receipt(peak_slots as u64, incarnation);
            if unsafe { send_message(control, &diagnostics) } {
                diagnostics_sent = true;
            } else if unsafe { errno() } == libc::EAGAIN {
                output_blocked = true;
            } else {
                disconnected = true;
            }
        }
        if stopping && !disconnected && credits.occupied() == 0 && diagnostics_sent && !stopped_sent
        {
            let stopped =
                Message::new(STOPPED, 0, 0, 0).receipt(credits.last_completion, incarnation);
            if unsafe { send_message(control, &stopped) } {
                stopped_sent = true;
            } else if unsafe { errno() } == libc::EAGAIN {
                output_blocked = true;
            } else {
                disconnected = true;
            }
        }
        // Control reads remain enabled while stopping/full: ACK/ABORT/STOP can
        // never be gated behind receipt output or fresh-admission capacity.
        let mut consumed = false;
        for _ in 0..capacity + 2 {
            let mut message = Message::new(0, 0, 0, 0);
            let mut fds = [-1; 2];
            let mut count = 0;
            let read = if disconnected {
                -libc::EAGAIN
            } else {
                unsafe { recv_packet(control, &mut message, &mut fds, &mut count) }
            };
            if read == -libc::EAGAIN || read == -libc::EINTR {
                break;
            }
            consumed = true;
            if read == 0 {
                disconnected = true;
                stopping = true;
                for slot in &credits.slots[..capacity] {
                    unsafe {
                        signal_slot(slot, libc::SIGKILL);
                    }
                }
                break;
            }
            if read != 1
                || message.magic != MAGIC
                || message.version != VERSION
                || message.incarnation != incarnation
            {
                unsafe { libc::_exit(125) }
            }
            if count == 0 && message.valid(RECEIPT_ACK, 0) {
                let Ok(mask) = credits.acknowledge(message.sequence) else {
                    unsafe { libc::_exit(125) }
                };
                for index in 0..capacity {
                    if mask & (1 << index) != 0 {
                        let slot = credits.slots[index];
                        unsafe {
                            libc::close(slot.channel);
                            if slot.pidfd >= 0 {
                                libc::close(slot.pidfd);
                            }
                        }
                        if credits.release_acked(index).is_err() {
                            unsafe { libc::_exit(125) }
                        }
                    }
                }
                continue;
            }
            if count == 0 && message.valid(STOPPED_ACK, 0) {
                if stopping
                    && stopped_sent
                    && credits.occupied() == 0
                    && message.sequence == credits.last_completion
                {
                    unsafe { libc::_exit(0) }
                }
                unsafe { libc::_exit(125) }
            }
            if count == 0 && message.valid(STOP, 0) {
                stopping = true;
                for slot in &credits.slots[..capacity] {
                    if !unsafe { signal_slot(slot, libc::SIGKILL) } {
                        unsafe { libc::_exit(125) }
                    }
                }
                continue;
            }
            if count == 0 && message.valid(ABORT, message.job) {
                if let Some(slot) = credits.slots[..capacity]
                    .iter()
                    .find(|s| s.phase == SlotPhase::Active && s.id == message.job)
                {
                    if !unsafe { signal_slot(slot, libc::SIGKILL) } {
                        unsafe { libc::_exit(125) }
                    }
                }
                continue;
            }
            if stopping || count != 2 || !message.valid(SUBMIT, message.job) {
                unsafe { libc::_exit(125) }
            }
            let Ok(index) = credits.reserve(message.job) else {
                unsafe { libc::_exit(125) }
            };
            credits.slots[index].channel = fds[1];
            peak_slots = peak_slots.max(credits.occupied());
            let parent = unsafe { libc::getpid() };
            let pid = unsafe { libc::syscall(libc::SYS_fork) } as i32;
            if pid == 0 {
                unsafe { worker(fds[0], fds[1], message.job, parent, message.value) }
            }
            if pid < 0 {
                let error = unsafe { errno() };
                unsafe {
                    send_message(fds[1], &Message::new(FAILED, message.job, 0, error as i64));
                    libc::close(fds[0]);
                }
                if credits.terminal(index, error, true).is_err() {
                    unsafe { libc::_exit(125) }
                }
                continue;
            }
            // Only this sole reaper can waitpid; a child remains an owned zombie
            // until the next loop. Numeric PID reuse is impossible here.
            let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) } as i32;
            let pidfd_error = if pidfd < 0 { unsafe { errno() } } else { 0 };
            credits.slots[index].pid = pid;
            credits.slots[index].pidfd = pidfd;
            credits.slots[index].start_ticks = unsafe { proc_start_ticks(pid) };
            let dropped = unsafe { libc::close(fds[0]) };
            if dropped != 0 || pidfd_error != 0 {
                let error = if pidfd_error != 0 {
                    pidfd_error
                } else {
                    unsafe { errno() }
                };
                unsafe {
                    signal_slot(&credits.slots[index], libc::SIGKILL);
                    send_message(fds[1], &Message::new(FAILED, message.job, 1, error as i64));
                }
                continue;
            }
            if message.value == FAULT_NO_DROP_BARRIER {
                continue;
            }
            if !unsafe {
                send_pidfd(
                    fds[1],
                    &Message::new(
                        BROKER_DROPPED,
                        message.job,
                        pid as i64,
                        credits.slots[index].start_ticks as i64,
                    ),
                    pidfd,
                )
            } {
                unsafe {
                    signal_slot(&credits.slots[index], libc::SIGKILL);
                }
            }
        }
        if consumed {
            continue;
        }
        let mut p = libc::pollfd {
            fd: if disconnected { -1 } else { control },
            events: libc::POLLIN
                | if output_blocked || credits.next_unsent().is_some() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        };
        unsafe {
            libc::ppoll(&mut p, 1, null(), &wait_mask);
        }
    }
}
