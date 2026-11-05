//! Bounded Linux process hosting for the synchronous framed plugin adapter.
//!
//! The domain traits remain synchronous and process-agnostic. This module owns the child, pipes,
//! protocol worker, control-phase deadlines, independent cancellation handle, process group, and
//! leader reaping. `next_item` is deliberately allowed to block until an event. Cancellation first
//! serializes semantic Close beside that read and requires post-Close EOF plus exact status zero;
//! only a bounded failure falls back to process-group termination.
//!
//! Plugins are trusted same-user code. A private process group is a cleanup and supervision
//! boundary, not a security containment boundary: a plugin can deliberately escape it with
//! `setsid`. Sandboxing or cgroup containment belongs to the launcher when untrusted code is in
//! scope.

use std::fmt;
use std::fs::File;
use std::io::{self, Write as _};
use std::marker::PhantomData;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chat_subscription::{
    BackendCapabilities, BackendFailure, CancellationError, ChatSubscriptionBackend,
    ChatSubscriptionCancellation, ChatSubscriptionDriver, DeliveryId, SubscribeRequest,
    SubscriptionItem,
};

use crate::{ClientFrame, PluginBackend, PluginError};

const KILL_REAP_TIMEOUT: Duration = Duration::from_secs(2);
const EXEC_READY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CLEANUP_ADMISSIONS: usize = 32;
const SUPERVISOR_READY: u8 = 1;
const SUPERVISOR_GROUP_FAILED: u8 = 2;
const SUPERVISOR_FD_CLOSE_FAILED: u8 = 3;
const SUPERVISOR_PARENT_DEATH_FAILED: u8 = 4;
const SUPERVISOR_RELEASE_FAILED: u8 = 5;
const SUPERVISOR_RELEASE: u8 = 1;
#[cfg(test)]
const RESTORE_FAILURE_PID_MARKER_ENV: &str = "CHAT_SUBSCRIPTION_RESTORE_FAILURE_PID_MARKER";

type CleanupTask = Box<dyn FnOnce() + Send + 'static>;

struct TrackedCleanup {
    handle: JoinHandle<()>,
    permit: Option<CleanupPermit>,
}

struct RetainedCleanup {
    _task: CleanupTask,
    _permit: CleanupPermit,
    retryable: bool,
}

struct CleanupRegistryState {
    active: usize,
    tracked: Vec<TrackedCleanup>,
    retained: Vec<RetainedCleanup>,
}

struct CleanupRegistry {
    limit: usize,
    state: Mutex<CleanupRegistryState>,
    #[cfg(test)]
    force_spawn_failure: AtomicBool,
}

impl CleanupRegistry {
    fn new(limit: usize) -> Arc<Self> {
        debug_assert!(limit > 0, "cleanup admission limit must be positive");
        Arc::new(Self {
            limit: limit.max(1),
            state: Mutex::new(CleanupRegistryState {
                active: 0,
                tracked: Vec::with_capacity(limit),
                retained: Vec::new(),
            }),
            #[cfg(test)]
            force_spawn_failure: AtomicBool::new(false),
        })
    }

    fn lock(&self) -> MutexGuard<'_, CleanupRegistryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reap_finished(&self) {
        let finished = {
            let mut state = self.lock();
            let mut finished = Vec::new();
            let mut index = 0;
            while index < state.tracked.len() {
                if state.tracked[index].handle.is_finished() {
                    finished.push(state.tracked.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };
        for cleanup in finished {
            let _ = cleanup.handle.join();
            // Drop the admission only after releasing the registry mutex: permit release locks it.
            drop(cleanup.permit);
        }
    }

    fn try_acquire(self: &Arc<Self>) -> io::Result<CleanupPermit> {
        // A launch attempt is also an explicit event for any known-capable tracked reaper that
        // retained ownership after an unexpected pidfd-wait failure. No timer polling is needed.
        reap_retry_signal().notify_all();
        self.reap_finished();
        self.retry_retained();
        self.reap_finished();
        let mut state = self.lock();
        if state.active >= self.limit {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "plugin cleanup admission is saturated at {} live or retained processes",
                    self.limit
                ),
            ));
        }
        state.active += 1;
        Ok(CleanupPermit {
            registry: Arc::clone(self),
            released: false,
        })
    }

    fn submit(self: &Arc<Self>, task: CleanupTask, permit: CleanupPermit) -> io::Result<()> {
        debug_assert!(Arc::ptr_eq(self, &permit.registry));
        #[cfg(test)]
        if self.force_spawn_failure.load(Ordering::SeqCst) {
            self.lock().retained.push(RetainedCleanup {
                _task: task,
                _permit: permit,
                retryable: true,
            });
            return Err(io::Error::other(
                "injected plugin cleanup thread spawn failure",
            ));
        }

        let task_slot = Arc::new(Mutex::new(Some(task)));
        let task_for_thread = Arc::clone(&task_slot);
        // The one-shot start message is buffered: cleanup submission, including Drop, must never
        // wait for the newly-created OS thread to be scheduled. The worker cannot run the task
        // until its JoinHandle and admission have been recorded below.
        let (start_sender, start_receiver) = mpsc::channel();
        let handle = match thread::Builder::new()
            .name("chat-plugin-cleanup".to_owned())
            .spawn(move || {
                if start_receiver.recv().is_err() {
                    return;
                }
                let task = task_for_thread
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(task) = task {
                    task();
                }
            }) {
            Ok(handle) => handle,
            Err(error) => {
                if let Some(task) = task_slot
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    self.lock().retained.push(RetainedCleanup {
                        _task: task,
                        _permit: permit,
                        retryable: true,
                    });
                }
                return Err(error);
            }
        };

        let mut state = self.lock();
        state.tracked.push(TrackedCleanup {
            handle,
            permit: Some(permit),
        });
        if start_sender.send(()).is_err() {
            let task = task_slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            let permit = state
                .tracked
                .last_mut()
                .and_then(|cleanup| cleanup.permit.take());
            if let (Some(task), Some(permit)) = (task, permit) {
                state.retained.push(RetainedCleanup {
                    _task: task,
                    _permit: permit,
                    retryable: true,
                });
            }
            return Err(io::Error::other(
                "plugin cleanup thread stopped before accepting ownership",
            ));
        }
        Ok(())
    }

    fn retry_retained(self: &Arc<Self>) {
        let retained = {
            let mut state = self.lock();
            let mut retryable = Vec::new();
            let mut index = 0;
            while index < state.retained.len() {
                if state.retained[index].retryable {
                    retryable.push(state.retained.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            retryable
        };
        for RetainedCleanup {
            _task: task,
            _permit: permit,
            retryable: _,
        } in retained
        {
            // A repeated OS refusal simply puts the same bounded task and admission back in the
            // retained set. No resource is detached and no new admission is created.
            let _ = self.submit(task, permit);
        }
    }

    fn retain_permanently(self: &Arc<Self>, task: CleanupTask, permit: CleanupPermit) {
        debug_assert!(Arc::ptr_eq(self, &permit.registry));
        self.lock().retained.push(RetainedCleanup {
            _task: task,
            _permit: permit,
            retryable: false,
        });
    }

    fn release(&self) {
        let mut state = self.lock();
        state.active = state.active.saturating_sub(1);
    }

    #[cfg(test)]
    fn counts(&self) -> (usize, usize, usize) {
        self.reap_finished();
        let state = self.lock();
        (state.active, state.tracked.len(), state.retained.len())
    }
}

struct CleanupPermit {
    registry: Arc<CleanupRegistry>,
    released: bool,
}

impl CleanupPermit {
    fn registry(&self) -> Arc<CleanupRegistry> {
        Arc::clone(&self.registry)
    }
}

impl fmt::Debug for CleanupPermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupPermit")
            .field("limit", &self.registry.limit)
            .field("released", &self.released)
            .finish()
    }
}

impl Drop for CleanupPermit {
    fn drop(&mut self) {
        if !self.released {
            self.registry.release();
            self.released = true;
        }
    }
}

fn cleanup_registry() -> Arc<CleanupRegistry> {
    static REGISTRY: OnceLock<Arc<CleanupRegistry>> = OnceLock::new();
    Arc::clone(REGISTRY.get_or_init(|| CleanupRegistry::new(MAX_CLEANUP_ADMISSIONS)))
}

struct ReapRetrySignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

impl ReapRetrySignal {
    fn new() -> Self {
        Self {
            generation: Mutex::new(0),
            changed: Condvar::new(),
        }
    }

    fn snapshot(&self) -> u64 {
        *self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn notify_all(&self) {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn wait_after(&self, observed: u64) -> u64 {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *generation == observed {
            generation = self
                .changed
                .wait(generation)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *generation
    }
}

fn reap_retry_signal() -> &'static ReapRetrySignal {
    static SIGNAL: OnceLock<ReapRetrySignal> = OnceLock::new();
    SIGNAL.get_or_init(ReapRetrySignal::new)
}

#[cfg(test)]
type PostSpawnHook = Box<dyn FnOnce(u32, u32) + Send>;
#[cfg(test)]
static POST_SPAWN_HOOK: Mutex<Option<PostSpawnHook>> = Mutex::new(None);

#[cfg(test)]
fn run_post_spawn_hook(supervisor_id: u32, plugin_id: u32) {
    let hook = POST_SPAWN_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(hook) = hook {
        hook(supervisor_id, plugin_id);
    }
}

/// Explicit deadlines for process-protocol control phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessPhaseTimeouts {
    hello: Duration,
    start: Duration,
    commit: Duration,
    close: Duration,
    shutdown_grace: Duration,
}

impl ProcessPhaseTimeouts {
    /// Validate phase deadlines and the cooperative process-exit grace period.
    ///
    /// Hello, Start, Commit, and Close deadlines must be positive. A zero shutdown grace requests
    /// immediate process-group termination after the cooperative protocol operation.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::ErrorKind::InvalidInput`] for zero or unrepresentable deadlines.
    pub fn new(
        hello: Duration,
        start: Duration,
        commit: Duration,
        close: Duration,
        shutdown_grace: Duration,
    ) -> io::Result<Self> {
        let now = Instant::now();
        for (name, value) in [
            ("hello", hello),
            ("start", start),
            ("commit", commit),
            ("close", close),
        ] {
            if value.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("plugin {name} timeout must be positive"),
                ));
            }
            if now.checked_add(value).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("plugin {name} timeout is too large"),
                ));
            }
        }
        if now.checked_add(shutdown_grace).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin shutdown grace is too large",
            ));
        }
        Ok(Self {
            hello,
            start,
            commit,
            close,
            shutdown_grace,
        })
    }

    /// Return the Hello response deadline.
    #[must_use]
    pub fn hello(self) -> Duration {
        self.hello
    }

    /// Return the Start response deadline.
    #[must_use]
    pub fn start(self) -> Duration {
        self.start
    }

    /// Return the Commit confirmation deadline.
    #[must_use]
    pub fn commit(self) -> Duration {
        self.commit
    }

    /// Return the Close write deadline.
    #[must_use]
    pub fn close(self) -> Duration {
        self.close
    }

    /// Return the grace period before forced process-group termination.
    #[must_use]
    pub fn shutdown_grace(self) -> Duration {
        self.shutdown_grace
    }
}

/// A bounded process-launch, handshake, or supervision failure.
#[derive(Debug)]
pub struct ProcessPluginError {
    code: String,
    detail: String,
}

impl ProcessPluginError {
    fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: bounded_detail(detail.into()),
        }
    }

    fn io(error: &io::Error) -> Self {
        Self::new("plugin_process_io", error.to_string())
    }

    fn protocol(error: &PluginError) -> Self {
        Self::new(error.code(), error.detail())
    }

    fn timeout(phase: &'static str, limit: Duration) -> Self {
        Self::new(
            format!("plugin_{phase}_timeout"),
            format!("plugin {phase} phase exceeded {limit:?}"),
        )
    }

    fn with_cleanup(mut self, cleanup: io::Result<ExitStatus>) -> Self {
        if let Err(error) = cleanup {
            self.detail = bounded_detail(format!(
                "{}; process cleanup also failed: {error}",
                self.detail
            ));
        }
        self
    }

    /// Return the stable machine-readable classification.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Return the bounded diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ProcessPluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProcessPluginError {}

impl From<io::Error> for ProcessPluginError {
    fn from(error: io::Error) -> Self {
        Self::io(&error)
    }
}

fn bounded_detail(mut detail: String) -> String {
    if detail.len() > 2_000 {
        let mut boundary = 2_000;
        while !detail.is_char_boundary(boundary) {
            boundary -= 1;
        }
        detail.truncate(boundary);
    }
    detail
}

fn backend_failure(
    code: impl Into<String>,
    detail: impl Into<String>,
    retryable: bool,
) -> BackendFailure {
    BackendFailure::new(code, bounded_detail(detail.into()), retryable)
        .expect("process-host failure constants and diagnostics are bounded")
}

fn require_reapable_sigchld() -> io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    // SAFETY: a null new-action pointer queries SIGCHLD without changing it; action points to
    // writable sigaction storage for the duration of the call.
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful sigaction initialized the output storage.
    let action = unsafe { action.assume_init() };
    let auto_reap =
        action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0;
    let external_handler = action.sa_sigaction != libc::SIG_DFL;
    if auto_reap || external_handler {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "plugin launch requires the default SIGCHLD disposition without SA_NOCLDWAIT; \
             process-wide auto-reapers and SIGCHLD handlers are unsupported",
        ));
    }
    Ok(())
}

struct SignalMaskGuard {
    previous: u64,
    active: bool,
    // Signal masks are per-thread; restoration authority must never migrate to another thread.
    _thread_bound: PhantomData<std::rc::Rc<()>>,
}

impl SignalMaskGuard {
    fn block_all() -> io::Result<Self> {
        let all = u64::MAX;
        let mut previous = 0_u64;
        // glibc intentionally withholds its two NPTL signals from pthread_sigmask. The raw Linux
        // ABI operates on the complete 64-signal kernel set; the kernel itself removes only the
        // unblockable SIGKILL and SIGSTOP bits.
        // SAFETY: all and previous are correctly sized/aligned kernel sigset storage for Linux.
        let result = unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &raw const all,
                &raw mut previous,
                std::mem::size_of::<u64>(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            previous,
            active: true,
            _thread_bound: PhantomData,
        })
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        // SAFETY: previous is the exact kernel mask returned for this same thread. A null output
        // pointer requests no additional snapshot.
        let result = unsafe {
            libc::syscall(
                libc::SYS_rt_sigprocmask,
                libc::SIG_SETMASK,
                &raw const self.previous,
                std::ptr::null_mut::<u64>(),
                std::mem::size_of::<u64>(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.active = false;
        Ok(())
    }

    fn restore_or_abort(&mut self, cloned_child: Option<(i32, u32)>, force_failure: bool) {
        if self.restore().is_err() || force_failure {
            // Returning to arbitrary host code with a silently altered per-thread mask is not a
            // recoverable launch failure. The arguments are an exact kernel-provided snapshot, so
            // only an external policy such as seccomp can make restoration fail. The raw child
            // remains armed with PR_SET_PDEATHSIG until successful restoration releases it, so it
            // cannot survive the host abort even if the best-effort exact pidfd kill is denied.
            #[cfg(test)]
            if force_failure {
                if let (Some((_, process_id)), Some(marker)) = (
                    cloned_child,
                    std::env::var_os(RESTORE_FAILURE_PID_MARKER_ENV),
                ) {
                    // This isolated test seam records the just-returned clone identity after
                    // restoring the real mask, then exits without attempting pidfd_send_signal.
                    // It exercises parent-death cleanup rather than the exact-kill fast path.
                    let _ = std::fs::write(marker, process_id.to_string());
                }
                // SAFETY: the isolated helper intentionally simulates fatal host termination.
                unsafe { libc::_exit(134) };
            }
            if let Some((pidfd, _)) = cloned_child {
                loop {
                    // SAFETY: successful CLONE_PIDFD installed this descriptor atomically and the
                    // parent has not yet exposed or closed it. SIGKILL requires no userspace data.
                    let result = unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            pidfd,
                            libc::SIGKILL,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        )
                    };
                    if result == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                    {
                        break;
                    }
                }
            }
            std::process::abort();
        }
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        // Every ordinary parent path restores explicitly. This is the fail-closed last resort for
        // unwinding before that point. The raw child never drops the guard.
        self.restore_or_abort(None, false);
    }
}

fn unsupported_pidfd_wait() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "plugin launch requires waitid(P_PIDFD) in addition to clone3(CLONE_PIDFD)",
    )
}

fn classify_waitid_pidfd_support(result: i32, error: io::Error) -> io::Result<()> {
    if result == 0 {
        return Err(io::Error::other(
            "waitid(P_PIDFD) unexpectedly accepted a non-pidfd",
        ));
    }
    match error.raw_os_error() {
        Some(libc::EBADF) => Ok(()),
        Some(libc::EINVAL) | Some(libc::ENOSYS) => Err(unsupported_pidfd_wait()),
        _ => Err(error),
    }
}

fn probe_waitid_pidfd_support(non_pidfd: i32) -> io::Result<()> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: information is writable. The known non-pidfd must produce EBADF on a kernel that
    // recognizes P_PIDFD; kernels predating that id type produce EINVAL instead.
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            libc::id_t::try_from(non_pidfd).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "probe descriptor does not fit id_t",
                )
            })?,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    classify_waitid_pidfd_support(result, io::Error::last_os_error())
}

fn probe_live_pidfd(pidfd: &OwnedFd) -> io::Result<()> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: pidfd names the live atomic supervisor and information is writable. WNOWAIT keeps
    // any unexpectedly observed status available to the owning reap path.
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            libc::id_t::try_from(pidfd.as_raw_fd()).expect("pidfd fits id_t"),
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::EINVAL) | Some(libc::ENOSYS) => Err(unsupported_pidfd_wait()),
            _ => Err(error),
        };
    }
    // SAFETY: successful waitid initialized the output. The supervisor cannot exit voluntarily.
    if unsafe { information.assume_init().si_pid() } != 0 {
        return Err(io::Error::other(
            "atomic plugin supervisor exited during pidfd capability preflight",
        ));
    }
    Ok(())
}

fn parse_proc_fd_name(name: &[u8]) -> Option<i32> {
    let mut descriptor = 0_i32;
    let mut digits = 0_usize;
    for byte in name.iter().copied() {
        if byte == 0 {
            break;
        }
        if !byte.is_ascii_digit() {
            return None;
        }
        descriptor = descriptor
            .checked_mul(10)?
            .checked_add(i32::from(byte - b'0'))?;
        digits += 1;
    }
    (digits > 0).then_some(descriptor)
}

fn close_inherited_fds_from_proc(keep_a: i32, keep_b: i32) -> bool {
    let path = b"/proc/self/fd\0";
    // SAFETY: path is NUL-terminated static storage. Raw syscalls avoid allocator and libc state in
    // the post-clone single-threaded child.
    let directory = unsafe {
        libc::syscall(
            libc::SYS_openat,
            libc::AT_FDCWD,
            path.as_ptr().cast::<libc::c_char>(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )
    };
    if directory < 0 {
        return false;
    }
    let Ok(directory) = i32::try_from(directory) else {
        return false;
    };
    let mut buffer = [0_u8; 4096];
    loop {
        // SAFETY: directory is an open procfs directory and buffer is writable for its full size.
        let bytes_read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if bytes_read < 0 {
            // SAFETY: directory remains owned by this child.
            unsafe { libc::close(directory) };
            return false;
        }
        if bytes_read == 0 {
            break;
        }
        let Ok(bytes_read) = usize::try_from(bytes_read) else {
            unsafe { libc::close(directory) };
            return false;
        };
        let mut offset = 0_usize;
        while offset < bytes_read {
            if bytes_read - offset < 19 {
                unsafe { libc::close(directory) };
                return false;
            }
            // linux_dirent64 stores d_reclen at byte offset 16 and its NUL-terminated name at 19.
            // SAFETY: the length check above provides the two bytes read unaligned here.
            let record_len = usize::from(unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(offset + 16).cast::<u16>())
            });
            if record_len < 20 || record_len > bytes_read - offset {
                unsafe { libc::close(directory) };
                return false;
            }
            if let Some(descriptor) = parse_proc_fd_name(&buffer[offset + 19..offset + record_len])
            {
                if descriptor != keep_a && descriptor != keep_b && descriptor != directory {
                    // SAFETY: each numeric procfs entry is a descriptor inherited by this child.
                    // Linux releases the slot even when close reports EINTR, and EBADF means it was
                    // already removed by a successful partial close_range.
                    unsafe { libc::close(descriptor) };
                }
            }
            offset += record_len;
        }
    }
    // SAFETY: directory was excluded from the loop above and remains owned here.
    unsafe { libc::close(directory) };
    true
}

fn close_supervisor_inherited_fds(keep_a: i32, keep_b: i32, allow_close_range: bool) -> bool {
    if allow_close_range {
        let lower_keep = keep_a.min(keep_b);
        let upper_keep = keep_a.max(keep_b);
        let lower = if lower_keep == 0 {
            0
        } else {
            // SAFETY: the range excludes both preserved descriptors and has no pointer arguments.
            unsafe { libc::syscall(libc::SYS_close_range, 0_u32, (lower_keep - 1) as u32, 0_u32) }
        };
        let middle = if upper_keep <= lower_keep.saturating_add(1) {
            0
        } else {
            // SAFETY: this range lies strictly between the two preserved descriptors.
            unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    (lower_keep + 1) as u32,
                    (upper_keep - 1) as u32,
                    0_u32,
                )
            }
        };
        // SAFETY: the range starts immediately above the greater preserved descriptor.
        let upper = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                (upper_keep as u32).saturating_add(1),
                u32::MAX,
                0_u32,
            )
        };
        if lower == 0 && middle == 0 && upper == 0 {
            return true;
        }
    }
    // Kernels before close_range remain supported when procfs is available. Failure is reported to
    // the parent before any plugin executable is spawned; the short-lived supervisor then exits.
    close_inherited_fds_from_proc(keep_a, keep_b)
}

fn child_report_status(descriptor: i32, status: u8) {
    let status = [status];
    // SAFETY: descriptor is the sole preserved pipe writer and status is one initialized byte.
    unsafe {
        libc::write(descriptor, status.as_ptr().cast(), status.len());
        libc::close(descriptor);
    }
}

fn supervisor_wait_forever() -> ! {
    loop {
        // SAFETY: pause has no pointer arguments. Signals only cause the loop to retry; SIGKILL
        // terminates the supervisor without returning.
        unsafe {
            libc::pause();
        }
    }
}

struct AtomicChild {
    process_id: u32,
    pidfd: OwnedFd,
    plugin: Child,
    plugin_pidfd: OwnedFd,
    reader: File,
    writer: File,
    cleanup_permit: CleanupPermit,
}

struct PendingSupervisor {
    process_id: u32,
    pidfd: Option<OwnedFd>,
    cleanup_permit: Option<CleanupPermit>,
}

impl PendingSupervisor {
    fn new(process_id: u32, pidfd: OwnedFd, cleanup_permit: CleanupPermit) -> Self {
        Self {
            process_id,
            pidfd: Some(pidfd),
            cleanup_permit: Some(cleanup_permit),
        }
    }

    fn pidfd(&self) -> &OwnedFd {
        self.pidfd
            .as_ref()
            .expect("pending supervisor owns its pidfd")
    }

    fn take(mut self) -> (u32, OwnedFd, CleanupPermit) {
        (
            self.process_id,
            self.pidfd
                .take()
                .expect("pending supervisor owns its pidfd"),
            self.cleanup_permit
                .take()
                .expect("pending supervisor owns its cleanup admission"),
        )
    }
}

impl Drop for PendingSupervisor {
    fn drop(&mut self) {
        if let (Some(pidfd), Some(permit)) = (self.pidfd.take(), self.cleanup_permit.take()) {
            dispose_supervisor(self.process_id, pidfd, permit);
        }
    }
}

fn spawn_atomic_supervisor(
    command: Command,
    cleanup_permit: CleanupPermit,
) -> io::Result<AtomicChild> {
    spawn_atomic_supervisor_with_probes(
        command,
        cleanup_permit,
        probe_waitid_pidfd_support,
        probe_live_pidfd,
        SupervisorSpawnOptions::STANDARD,
    )
}

#[derive(Clone, Copy)]
struct SupervisorSpawnOptions {
    allow_close_range: bool,
    child_test_signal: Option<i32>,
    force_pidfd_transfer_failure: bool,
    force_mask_restore_failure: bool,
    force_receive_eintr_once: bool,
}

impl SupervisorSpawnOptions {
    const STANDARD: Self = Self {
        allow_close_range: true,
        child_test_signal: None,
        force_pidfd_transfer_failure: false,
        force_mask_restore_failure: false,
        force_receive_eintr_once: false,
    };
}

fn spawn_atomic_supervisor_with_probes<Preflight, LiveProbe>(
    mut command: Command,
    cleanup_permit: CleanupPermit,
    preflight: Preflight,
    live_probe: LiveProbe,
    options: SupervisorSpawnOptions,
) -> io::Result<AtomicChild>
where
    Preflight: FnOnce(i32) -> io::Result<()>,
    LiveProbe: FnOnce(&OwnedFd) -> io::Result<()>,
{
    let (ready_reader, ready_writer) = pipe_cloexec()?;
    let (release_parent, release_child) = socket_pair_cloexec()?;
    preflight(ready_reader.as_raw_fd())?;
    require_reapable_sigchld()?;

    // SAFETY: getpid has no pointer arguments or failure mode and is captured before clone so the
    // raw child can detect parent death that races PR_SET_PDEATHSIG setup.
    let parent_process_id = unsafe { libc::getpid() };
    let mut signal_mask = SignalMaskGuard::block_all()?;
    let mut pidfd = -1_i32;
    // SAFETY: clone_args is a plain kernel ABI structure. CLONE_PIDFD makes pidfd initialization
    // and child creation one indivisible operation, so no numeric PID lookup race exists.
    let mut arguments = unsafe { std::mem::zeroed::<libc::clone_args>() };
    arguments.flags = u64::try_from(libc::CLONE_PIDFD).expect("CLONE_PIDFD is positive");
    arguments.pidfd = u64::try_from((&raw mut pidfd).addr()).expect("pointer fits u64");
    arguments.exit_signal = u64::try_from(libc::SIGCHLD).expect("SIGCHLD is positive");
    // SAFETY: the arguments pointer and declared size describe the initialized clone_args above.
    // The child branch uses only inherited storage and async-signal-safe syscalls.
    let result = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &raw const arguments,
            std::mem::size_of::<libc::clone_args>(),
        )
    };
    let clone_error = (result < 0).then(io::Error::last_os_error);
    if result != 0 {
        let cloned_child = if pidfd >= 0 {
            u32::try_from(result)
                .ok()
                .map(|process_id| (pidfd, process_id))
        } else {
            None
        };
        signal_mask.restore_or_abort(cloned_child, options.force_mask_restore_failure);
    }
    if result < 0 {
        return Err(clone_error.expect("failed clone3 captured its errno before mask restoration"));
    }
    if result == 0 {
        let ready_writer = ready_writer.as_raw_fd();
        let release_child = release_child.as_raw_fd();
        // PR_SET_PDEATHSIG closes the only gap in which parent mask restoration can fail before a
        // PendingSupervisor owns cleanup. The parent releases this protection only after its exact
        // mask is restored and the atomic pidfd is wrapped. getppid closes the standard race where
        // the parent dies immediately before prctl.
        // SAFETY: these raw syscalls have no userspace pointer arguments in these forms.
        if unsafe {
            libc::syscall(
                libc::SYS_prctl,
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL,
                0,
                0,
                0,
            )
        } < 0
            || unsafe { libc::syscall(libc::SYS_getppid) } != libc::c_long::from(parent_process_id)
        {
            child_report_status(ready_writer, SUPERVISOR_PARENT_DEATH_FAILED);
            unsafe { libc::_exit(124) };
        }
        // Establish a durable group leader before creating the executable child. This supervisor
        // never exits voluntarily: the host therefore kills a still-owned PGID before any reaper
        // can release and recycle that numeric identity.
        // SAFETY: setpgid with zero arguments targets this freshly cloned process.
        if unsafe { libc::setpgid(0, 0) } < 0 {
            child_report_status(ready_writer, SUPERVISOR_GROUP_FAILED);
            unsafe { libc::_exit(127) };
        }
        if let Some(signal) = options.child_test_signal {
            // SAFETY: the test-only signal targets this exact raw child while every blockable
            // signal remains masked. It deterministically exercises the pre-FD-cleanup window.
            unsafe { libc::kill(libc::getpid(), signal) };
        }
        if !close_supervisor_inherited_fds(ready_writer, release_child, options.allow_close_range) {
            child_report_status(ready_writer, SUPERVISOR_FD_CLOSE_FAILED);
            unsafe { libc::_exit(126) };
        }
        let mut release = [0_u8; 1];
        loop {
            // SAFETY: release_child is the sole other descriptor preserved through inherited-FD
            // cleanup and release is writable for one byte.
            let read =
                unsafe { libc::read(release_child, release.as_mut_ptr().cast(), release.len()) };
            if read == 1 && release == [SUPERVISOR_RELEASE] {
                break;
            }
            // SAFETY: libc exposes the calling thread's errno slot; no allocation or shared state
            // is touched in this post-clone child.
            if read < 0 && unsafe { *libc::__errno_location() } == libc::EINTR {
                continue;
            }
            child_report_status(ready_writer, SUPERVISOR_RELEASE_FAILED);
            unsafe { libc::_exit(125) };
        }
        // SAFETY: the parent has now demonstrated successful mask restoration and durable pidfd
        // ownership. Clearing parent-death coupling makes the supervisor independent of the
        // launching thread for its normal lifetime.
        if unsafe { libc::syscall(libc::SYS_prctl, libc::PR_SET_PDEATHSIG, 0, 0, 0, 0) } < 0 {
            child_report_status(ready_writer, SUPERVISOR_PARENT_DEATH_FAILED);
            unsafe { libc::_exit(124) };
        }
        // SAFETY: release_child was preserved solely for this one-byte handshake.
        unsafe { libc::close(release_child) };
        child_report_status(ready_writer, SUPERVISOR_READY);
        supervisor_wait_forever();
    }

    let process_id = u32::try_from(result).unwrap_or_else(|_| {
        // Linux clone3 returns a positive pid_t. Without that ABI guarantee there is no exact
        // identity with which this library can safely clean up, so fail the host closed instead of
        // performing a numeric lookup or releasing the admission.
        std::process::abort();
    });
    if pidfd < 0 {
        // Successful CLONE_PIDFD atomically installs this descriptor. Continuing without it would
        // violate the adapter's identity guarantee, and numeric recovery is intentionally absent.
        std::process::abort();
    }
    // SAFETY: successful CLONE_PIDFD returned a fresh descriptor owned by this process.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
    let supervisor = PendingSupervisor::new(process_id, pidfd, cleanup_permit);
    drop(release_child);
    let release = [SUPERVISOR_RELEASE];
    // MSG_NOSIGNAL prevents a setup failure in the armed child from terminating the host while it
    // releases parent-death coupling.
    // SAFETY: release_parent is live and release is readable for its one-byte length.
    loop {
        let sent = unsafe {
            libc::send(
                release_parent.as_raw_fd(),
                release.as_ptr().cast(),
                release.len(),
                libc::MSG_NOSIGNAL,
            )
        };
        if sent == 1 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    drop(release_parent);
    drop(ready_writer);
    let ready_available = wait_fd_until(
        ready_reader.as_raw_fd(),
        libc::POLLIN,
        Instant::now() + EXEC_READY_TIMEOUT,
    )?;
    if !ready_available {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "atomic plugin supervisor did not establish its process group",
        ));
    }
    let mut ready = [0_u8; 1];
    let read = loop {
        // SAFETY: ready_reader is live and ready is writable.
        let read = unsafe {
            libc::read(
                ready_reader.as_raw_fd(),
                ready.as_mut_ptr().cast(),
                ready.len(),
            )
        };
        if read >= 0 {
            break read;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    if read != 1 || ready != [SUPERVISOR_READY] {
        let detail = match ready[0] {
            SUPERVISOR_FD_CLOSE_FAILED => {
                "atomic plugin supervisor could not close inherited descriptors; close_range or /proc/self/fd is required"
            }
            SUPERVISOR_PARENT_DEATH_FAILED => {
                "atomic plugin supervisor could not establish its parent-death handshake"
            }
            SUPERVISOR_RELEASE_FAILED => {
                "atomic plugin supervisor did not receive its parent release handshake"
            }
            _ => "atomic plugin supervisor failed before establishing its process group",
        };
        let kind = if ready[0] == SUPERVISOR_FD_CLOSE_FAILED {
            io::ErrorKind::Unsupported
        } else {
            io::ErrorKind::Other
        };
        return Err(io::Error::new(kind, detail));
    }
    live_probe(supervisor.pidfd())?;

    let (stdin_reader, stdin_writer) = pipe_cloexec()?;
    let (stdout_reader, stdout_writer) = pipe_cloexec()?;
    let (pidfd_receiver, pidfd_sender) = socket_pair_cloexec()?;
    let pidfd_receiver_raw = pidfd_receiver.as_raw_fd();
    let pidfd_sender_raw = pidfd_sender.as_raw_fd();
    command
        .stdin(Stdio::from(File::from(stdin_reader)))
        .stdout(Stdio::from(File::from(stdout_writer)))
        .stderr(Stdio::null())
        .process_group(libc::pid_t::try_from(process_id).expect("supervisor pid fits pid_t"));
    // SAFETY: the callback performs only raw close/pidfd_open/sendmsg syscalls and returns before
    // the standard launcher execs the plugin. It creates identity authority for this exact child,
    // so the parent never looks up a potentially reused numeric PID.
    unsafe {
        command.pre_exec(move || send_own_pidfd(pidfd_sender_raw, pidfd_receiver_raw));
    }
    let plugin = command.spawn()?;
    drop(command);
    drop(pidfd_sender);
    #[cfg(test)]
    run_post_spawn_hook(process_id, plugin.id());
    let plugin_pidfd_result = if options.force_pidfd_transfer_failure {
        Err(io::Error::other("injected plugin pidfd transfer failure"))
    } else {
        receive_pidfd(
            &pidfd_receiver,
            Instant::now() + EXEC_READY_TIMEOUT,
            options.force_receive_eintr_once,
        )
    };
    let plugin_pidfd = match plugin_pidfd_result {
        Ok(plugin_pidfd) => plugin_pidfd,
        Err(error) => {
            // The still-live atomic supervisor pins the process-group identity, so failure cannot
            // redirect group termination to a reused PGID. Without the transferred plugin pidfd,
            // however, neither Child::kill nor Child::wait is identity-safe against an external
            // process-wide reaper. Kill the pinned group, then retain every remaining ownership
            // object and the bounded admission permanently instead of making a numeric PID call.
            let (process_id, pidfd, cleanup_permit) = supervisor.take();
            terminate_supervisor_group(process_id, &pidfd);
            let registry = cleanup_permit.registry();
            registry.retain_permanently(
                Box::new(move || {
                    drop(plugin);
                    drop(pidfd);
                }),
                cleanup_permit,
            );
            return Err(error);
        }
    };
    let (process_id, pidfd, cleanup_permit) = supervisor.take();
    let reader = File::from(stdout_reader);
    let writer = File::from(stdin_writer);
    Ok(AtomicChild {
        process_id,
        pidfd,
        plugin,
        plugin_pidfd,
        reader,
        writer,
        cleanup_permit,
    })
}

fn signal_pidfd(pidfd: &OwnedFd) -> io::Result<()> {
    // SAFETY: pidfd_send_signal addresses the descriptor's stable process identity.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn terminate_supervisor_group(process_id: u32, pidfd: &OwnedFd) {
    let process_group = libc::pid_t::try_from(process_id).expect("supervisor pid fits pid_t");
    // SAFETY: the atomic supervisor remains alive and pins this private process-group identity.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    let _ = signal_pidfd(pidfd);
}

fn cleanup_supervisor(process_id: u32, pidfd: &OwnedFd) -> io::Result<()> {
    terminate_supervisor_group(process_id, pidfd);
    if !wait_pidfd_until(pidfd, Instant::now() + KILL_REAP_TIMEOUT)? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "atomic plugin supervisor did not exit after bounded termination",
        ));
    }
    reap_supervisor_owned(pidfd).map(|_| ())
}

fn dispose_supervisor(process_id: u32, pidfd: OwnedFd, permit: CleanupPermit) {
    if cleanup_supervisor(process_id, &pidfd).is_ok() {
        return;
    }
    let registry = permit.registry();
    let _ = registry.submit(Box::new(move || eventual_reap_supervisor(pidfd)), permit);
}

fn wait_fd_until(descriptor: i32, events: i16, deadline: Instant) -> io::Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let seconds = libc::time_t::try_from(remaining.as_secs()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "pidfd wait seconds do not fit time_t",
            )
        })?;
        let nanoseconds = libc::c_long::from(remaining.subsec_nanos());
        let timeout = libc::timespec {
            tv_sec: seconds,
            tv_nsec: nanoseconds,
        };
        let mut descriptor = libc::pollfd {
            fd: descriptor,
            events,
            revents: 0,
        };
        // SAFETY: descriptor and timeout point to initialized local values for the duration of the
        // call. The one-element pollfd array is described by the matching nfds value.
        let result = unsafe {
            libc::ppoll(
                &mut descriptor,
                1,
                &timeout,
                std::ptr::null::<libc::sigset_t>(),
            )
        };
        if result > 0 {
            return Ok(true);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn wait_pidfd_until(pidfd: &OwnedFd, deadline: Instant) -> io::Result<bool> {
    wait_fd_until(pidfd.as_raw_fd(), libc::POLLIN, deadline)
}

fn wait_pidfd_until_or_cancelled(
    pidfd: &OwnedFd,
    cancellation_fd: RawFd,
    deadline: Instant,
) -> io::Result<bool> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let seconds = libc::time_t::try_from(remaining.as_secs()).unwrap_or(libc::time_t::MAX);
        let nanoseconds = libc::c_long::from(remaining.subsec_nanos());
        let timeout = libc::timespec {
            tv_sec: seconds,
            tv_nsec: nanoseconds,
        };
        let mut descriptors = [
            libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: cancellation_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: descriptors and timeout point to initialized local values for the duration of
        // the call. Both descriptors remain borrowed by the caller for the whole wait.
        let result = unsafe {
            libc::ppoll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                &timeout,
                std::ptr::null::<libc::sigset_t>(),
            )
        };
        if result > 0 {
            if descriptors[1].revents != 0 {
                return Ok(false);
            }
            if descriptors[0].revents != 0 {
                return Ok(true);
            }
            continue;
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn pidfd_proves_status_consumed(pidfd: &OwnedFd) -> io::Result<bool> {
    loop {
        // SAFETY: signal zero performs no delivery and addresses the stable pidfd identity. Linux
        // keeps a zombie's task attached to its pidfd until wait consumes the status, so success
        // proves the child still needs an exact reap and ESRCH proves that status was consumed.
        // Unlike POLLHUP, this distinction is available on every kernel that supports clone3.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                0,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => return Ok(true),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn waitid_reap_pidfd_once(pidfd: &OwnedFd) -> io::Result<ExitStatus> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: pidfd names the direct supervisor child and information is writable. Successful
    // WEXITED both obtains and consumes its status.
    let result = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            libc::id_t::try_from(pidfd.as_raw_fd()).expect("pidfd fits id_t"),
            information.as_mut_ptr(),
            libc::WEXITED,
        )
    };
    if result == 0 {
        // SAFETY: successful waitid initialized a SIGCHLD status record.
        let information = unsafe { information.assume_init() };
        return validated_exit_status_from_siginfo(&information);
    }
    Err(io::Error::last_os_error())
}

fn waitid_reap_pidfd_with<Waitid>(mut waitid: Waitid) -> io::Result<ExitStatus>
where
    Waitid: FnMut() -> io::Result<ExitStatus>,
{
    loop {
        match waitid() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

fn waitid_reap_pidfd(pidfd: &OwnedFd) -> io::Result<ExitStatus> {
    waitid_reap_pidfd_with(|| waitid_reap_pidfd_once(pidfd))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PidfdReapOutcome {
    Status(ExitStatus),
    StatusConsumed,
}

fn reap_supervisor_owned(pidfd: &OwnedFd) -> io::Result<PidfdReapOutcome> {
    reap_pidfd_owned_with(pidfd, waitid_reap_pidfd)
}

fn reap_pidfd_owned_with<Waitid>(pidfd: &OwnedFd, waitid: Waitid) -> io::Result<PidfdReapOutcome>
where
    Waitid: FnOnce(&OwnedFd) -> io::Result<ExitStatus>,
{
    let error = match waitid(pidfd) {
        Ok(status) => return Ok(PidfdReapOutcome::Status(status)),
        Err(error) => error,
    };
    // No wait error, including ECHILD, proves that a child was reaped: seccomp can synthesize any
    // errno. Stable pidfd identity evidence distinguishes an externally consumed status from a
    // live or exited-but-unreaped task without ever consulting its reusable numeric PID.
    if pidfd_proves_status_consumed(pidfd)? {
        return Ok(PidfdReapOutcome::StatusConsumed);
    }
    Err(error)
}

fn reap_pidfd_tracked(pidfd: &OwnedFd) -> PidfdReapOutcome {
    reap_pidfd_tracked_with(pidfd, reap_retry_signal(), waitid_reap_pidfd)
}

fn reap_pidfd_tracked_with<Waitid>(
    pidfd: &OwnedFd,
    retry: &ReapRetrySignal,
    waitid: Waitid,
) -> PidfdReapOutcome
where
    Waitid: FnMut(&OwnedFd) -> io::Result<ExitStatus>,
{
    reap_pidfd_tracked_with_hook(pidfd, retry, waitid, || {})
}

fn reap_pidfd_tracked_with_hook<Waitid, BeforeWait>(
    pidfd: &OwnedFd,
    retry: &ReapRetrySignal,
    mut waitid: Waitid,
    mut before_wait: BeforeWait,
) -> PidfdReapOutcome
where
    Waitid: FnMut(&OwnedFd) -> io::Result<ExitStatus>,
    BeforeWait: FnMut(),
{
    loop {
        let observed = retry.snapshot();
        match reap_pidfd_owned_with(pidfd, &mut waitid) {
            Ok(status) => return status,
            Err(_) => {
                // This thread, its JoinHandle, the pidfd, and the launch admission all remain
                // owned while waiting for an explicit retry event. Repeated failures cannot turn a
                // zombie into an "exact" success or release capacity for another plugin.
                before_wait();
                retry.wait_after(observed);
            }
        }
    }
}

fn eventual_reap_supervisor(pidfd: OwnedFd) {
    let _status = reap_pidfd_tracked(&pidfd);
}

fn validated_exit_status_from_siginfo(information: &libc::siginfo_t) -> io::Result<ExitStatus> {
    if information.si_signo != libc::SIGCHLD
        || unsafe { information.si_pid() } <= 0
        || !matches!(
            information.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        )
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "waitid(P_PIDFD) returned success without a valid terminal SIGCHLD record",
        ));
    }
    let raw = match information.si_code {
        libc::CLD_EXITED => (unsafe { information.si_status() }) << 8,
        libc::CLD_DUMPED => (unsafe { information.si_status() }) | 0x80,
        _ => unsafe { information.si_status() },
    };
    Ok(ExitStatus::from_raw(raw))
}

fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1_i32; 2];
    // SAFETY: descriptors points to writable storage for exactly two file descriptors.
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 returned two fresh descriptors owned by this function.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn socket_pair_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1_i32; 2];
    // SAFETY: descriptors points to writable storage for exactly two socket descriptors.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful socketpair returned two fresh descriptors owned by this function.
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn send_own_pidfd(socket: i32, peer: i32) -> io::Result<()> {
    // SAFETY: this runs in the standard launcher's pre-exec child. Closing the unused inherited
    // peer and opening a pidfd for getpid use only async-signal-safe syscalls.
    unsafe {
        libc::close(peer);
    }
    // SAFETY: pidfd_open receives this exact child identity and zero flags.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    if pidfd < 0 {
        return Err(io::Error::last_os_error());
    }
    let pidfd = pidfd as i32;
    let mut payload = [1_u8];
    let mut vector = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0 as libc::c_long; 8];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: the one-descriptor payload size is a compile-time u32-sized constant.
    message.msg_controllen =
        unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
    // SAFETY: message owns a sufficiently aligned and sized ancillary buffer.
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null() {
        // SAFETY: pidfd is still owned locally on this error path.
        unsafe {
            libc::close(pidfd);
        }
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    // SAFETY: header addresses the buffer above and CMSG_DATA has room for exactly one descriptor.
    unsafe {
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
        std::ptr::write(libc::CMSG_DATA(header).cast::<i32>(), pidfd);
    }
    let send_error = loop {
        // SAFETY: the msghdr, iovec, payload, and ancillary descriptor remain live for sendmsg.
        let sent = unsafe { libc::sendmsg(socket, &message, libc::MSG_NOSIGNAL) };
        if sent == 1 {
            break None;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            break Some(error);
        }
    };
    // SAFETY: SCM_RIGHTS duplicated the descriptor into the queued message on success; this child
    // closes both local descriptors before exec.
    unsafe {
        libc::close(pidfd);
        libc::close(socket);
    }
    if let Some(error) = send_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn receive_pidfd(
    socket: &OwnedFd,
    deadline: Instant,
    mut force_eintr_once: bool,
) -> io::Result<OwnedFd> {
    if !wait_fd_until(socket.as_raw_fd(), libc::POLLIN, deadline)? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "plugin child did not transfer its self-opened pidfd",
        ));
    }
    let mut payload = [0_u8; 1];
    let mut vector = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = [0 as libc::c_long; 8];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control);
    let received = loop {
        let result = if force_eintr_once {
            force_eintr_once = false;
            Err(io::Error::from_raw_os_error(libc::EINTR))
        } else {
            message.msg_flags = 0;
            message.msg_controllen = std::mem::size_of_val(&control);
            // SAFETY: message points to live payload and ancillary buffers for recvmsg.
            let received =
                unsafe { libc::recvmsg(socket.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
            if received < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(received)
            }
        };
        match result {
            Ok(received) => break received,
            // SOCK_SEQPACKET leaves the queued packet untouched on EINTR, so retry cannot
            // duplicate the transferred descriptor.
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    };
    if received != 1 || payload != [1] || message.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin child returned an invalid pidfd transfer message",
        ));
    }
    // SAFETY: successful recvmsg initialized the control buffer described by message.
    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    if header.is_null()
        || unsafe { (*header).cmsg_level } != libc::SOL_SOCKET
        || unsafe { (*header).cmsg_type } != libc::SCM_RIGHTS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin child pidfd transfer omitted SCM_RIGHTS",
        ));
    }
    // SAFETY: the validated SCM_RIGHTS record contains one descriptor written by send_own_pidfd.
    let descriptor = unsafe { std::ptr::read(libc::CMSG_DATA(header).cast::<i32>()) };
    if descriptor < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "plugin child transferred an invalid pidfd",
        ));
    }
    // SAFETY: recvmsg created a fresh descriptor owned by this process.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

fn set_nonblocking(descriptor: i32) -> io::Result<()> {
    // SAFETY: fcntl receives a live descriptor and F_GETFL has no third argument.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL accepts the retrieved flag word plus O_NONBLOCK for the same descriptor.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug)]
struct CancellationSignal {
    descriptor: OwnedFd,
    cancelled: AtomicBool,
}

impl CancellationSignal {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd takes an initial counter and flags and returns a new owned descriptor.
        let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful eventfd returns a fresh descriptor owned by this function.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        Ok(Self {
            descriptor,
            cancelled: AtomicBool::new(false),
        })
    }

    fn cancel(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        let value = 1_u64.to_ne_bytes();
        loop {
            // SAFETY: descriptor is a live eventfd and value points to exactly one initialized u64.
            let result = unsafe {
                libc::write(
                    self.descriptor.as_raw_fd(),
                    value.as_ptr().cast(),
                    value.len(),
                )
            };
            if result >= 0 {
                return;
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            debug_assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            return;
        }
    }

    fn wait_io(&self, descriptor: i32, events: i16) -> io::Result<()> {
        self.wait_io_until(descriptor, events, None)
    }

    fn wait_io_until(
        &self,
        descriptor: i32,
        events: i16,
        deadline: Option<Instant>,
    ) -> io::Result<()> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "plugin process I/O was cancelled",
            ));
        }
        let mut descriptors = [
            libc::pollfd {
                fd: descriptor,
                events,
                revents: 0,
            },
            libc::pollfd {
                fd: self.descriptor.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let timeout = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "plugin process I/O exceeded its deadline",
                        ));
                    }
                    i32::try_from(remaining.as_millis().max(1).min(i32::MAX as u128))
                        .expect("bounded poll timeout fits i32")
                }
                None => -1,
            };
            // SAFETY: descriptors is an initialized two-element pollfd array that remains live for
            // the call. A timeout of -1 blocks until pipe progress or explicit cancellation.
            let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, timeout) };
            if result > 0 {
                if descriptors[1].revents != 0 || self.cancelled.load(Ordering::SeqCst) {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "plugin process I/O was cancelled",
                    ));
                }
                if descriptors[0].revents != 0 {
                    return Ok(());
                }
                continue;
            }
            if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "plugin process I/O exceeded its deadline",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

struct CancellableReader {
    reader: File,
    cancellation: Arc<CancellationSignal>,
    activity: Arc<ProtocolActivity>,
}

impl io::Read for CancellableReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.reader.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => self
                    .cancellation
                    .wait_io(self.reader.as_raw_fd(), libc::POLLIN)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => {
                    if matches!(result, Ok(0)) {
                        self.activity.transport_eof();
                    }
                    return result;
                }
            }
        }
    }
}

struct CancellableWriter {
    writer: Arc<Mutex<File>>,
    cancellation: Arc<CancellationSignal>,
    deadline: Option<Instant>,
}

impl CancellableWriter {
    fn lock_writer(&self) -> io::Result<MutexGuard<'_, File>> {
        match self.deadline {
            None => Ok(self
                .writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)),
            Some(deadline) => loop {
                match self.writer.try_lock() {
                    Ok(writer) => return Ok(writer),
                    Err(std::sync::TryLockError::Poisoned(error)) => {
                        return Ok(error.into_inner());
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        if Instant::now() >= deadline {
                            self.cancellation.cancel();
                            return Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "plugin Close could not acquire the frame writer before its deadline",
                            ));
                        }
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            },
        }
    }

    fn write_locked(&self, writer: &mut File, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0_usize;
        while written < bytes.len() {
            match writer.write(&bytes[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "plugin process pipe accepted no frame bytes",
                    ));
                }
                Ok(count) => written = written.saturating_add(count),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => self
                    .cancellation
                    .wait_io_until(writer.as_raw_fd(), libc::POLLOUT, self.deadline)?,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Write and flush one complete frame while retaining the same deadline-aware reservation.
    fn write_frame(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.lock_writer()?;
        self.write_locked(&mut writer, bytes)?;
        writer.flush()
    }
}

impl io::Write for CancellableWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut writer = self.lock_writer()?;
        self.write_locked(&mut writer, bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtocolWorkerPhase {
    Control,
    Idle,
    Next,
    NextResultReady,
    CloseWriting,
    CloseWritingEof,
    CloseSent,
    CloseFailed,
    CloseEof,
    CloseOther,
    Stopped,
}

struct ProtocolActivity {
    phase: Mutex<ProtocolWorkerPhase>,
    changed: Condvar,
}

impl ProtocolActivity {
    fn new() -> Self {
        Self {
            phase: Mutex::new(ProtocolWorkerPhase::Control),
            changed: Condvar::new(),
        }
    }

    fn set(&self, phase: ProtocolWorkerPhase) {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = phase;
        self.changed.notify_all();
    }

    fn get(&self) -> ProtocolWorkerPhase {
        *self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn claim_next_for_close(&self) -> bool {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *phase != ProtocolWorkerPhase::Next {
            return false;
        }
        *phase = ProtocolWorkerPhase::CloseWriting;
        self.changed.notify_all();
        true
    }

    fn transport_eof(&self) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *phase = match *phase {
            ProtocolWorkerPhase::Next => ProtocolWorkerPhase::NextResultReady,
            ProtocolWorkerPhase::CloseWriting => ProtocolWorkerPhase::CloseWritingEof,
            ProtocolWorkerPhase::CloseSent => ProtocolWorkerPhase::CloseEof,
            other => other,
        };
        self.changed.notify_all();
    }

    fn finish_close_write(&self, success: bool) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *phase = match (*phase, success) {
            (ProtocolWorkerPhase::CloseWriting, true) => ProtocolWorkerPhase::CloseSent,
            (ProtocolWorkerPhase::CloseWritingEof, true) => ProtocolWorkerPhase::CloseEof,
            (ProtocolWorkerPhase::CloseWriting, false)
            | (ProtocolWorkerPhase::CloseWritingEof, false) => ProtocolWorkerPhase::CloseFailed,
            (other, _) => other,
        };
        self.changed.notify_all();
    }

    fn finish_next(&self, observed_eof: bool) -> bool {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while matches!(
            *phase,
            ProtocolWorkerPhase::CloseWriting | ProtocolWorkerPhase::CloseWritingEof
        ) {
            phase = self
                .changed
                .wait(phase)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        match *phase {
            ProtocolWorkerPhase::CloseSent => {
                *phase = if observed_eof {
                    ProtocolWorkerPhase::CloseEof
                } else {
                    ProtocolWorkerPhase::CloseOther
                };
                self.changed.notify_all();
                true
            }
            ProtocolWorkerPhase::CloseEof => {
                if !observed_eof {
                    *phase = ProtocolWorkerPhase::CloseOther;
                    self.changed.notify_all();
                }
                true
            }
            ProtocolWorkerPhase::CloseFailed => false,
            ProtocolWorkerPhase::Next | ProtocolWorkerPhase::NextResultReady => {
                *phase = ProtocolWorkerPhase::NextResultReady;
                self.changed.notify_all();
                false
            }
            _ => false,
        }
    }

    fn wait_until_changed(&self, observed: ProtocolWorkerPhase, deadline: Instant) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *phase == observed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let waited = self.changed.wait_timeout(phase, remaining);
            let (next, timed) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
            phase = next;
            if timed.timed_out() {
                return;
            }
        }
    }

    fn stop(&self) {
        let mut phase = self
            .phase
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(
            *phase,
            ProtocolWorkerPhase::CloseEof | ProtocolWorkerPhase::CloseOther
        ) {
            *phase = ProtocolWorkerPhase::Stopped;
        }
        self.changed.notify_all();
    }
}

struct ClosePort {
    writer: Mutex<Option<CancellableWriter>>,
}

impl ClosePort {
    fn new(writer: CancellableWriter) -> Self {
        Self {
            writer: Mutex::new(Some(writer)),
        }
    }

    fn send(&self, deadline: Instant) -> io::Result<()> {
        let mut slot = loop {
            match self.writer.try_lock() {
                Ok(writer) => break writer,
                Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "plugin Close port remained busy through its deadline",
                        ));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
            }
        };
        let mut writer = slot
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Close port is stopped"))?;
        drop(slot);
        writer.deadline = Some(deadline);
        let payload = serde_json::to_vec(&ClientFrame::Close)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let length = u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Close frame length exceeds u32")
        })?;
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&payload);
        writer.write_frame(&frame)
    }

    fn stop(&self) {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }
}

/// A spawned plugin leader, private cleanup process group, and event-driven reap handle.
#[derive(Debug)]
pub struct ProcessPluginChild {
    process_id: u32,
    process_group: libc::pid_t,
    pidfd: Option<OwnedFd>,
    plugin: Option<Child>,
    plugin_pidfd: Option<OwnedFd>,
    reader: Option<File>,
    writer: Option<File>,
    plugin_status: Option<ExitStatus>,
    process_group_terminated: bool,
    shutdown: Option<StoredShutdown>,
    cleanup_permit: Option<CleanupPermit>,
    #[cfg(test)]
    cleanup_barrier: Option<CleanupBarrier>,
}

#[cfg(test)]
#[derive(Debug)]
struct CleanupBarrier {
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

impl ProcessPluginChild {
    /// Spawn a command with piped protocol streams, null stderr, and a private process group.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the child or its event-driven supervision handle cannot be
    /// created. The command is not started when atomic clone3 pidfd supervision is unavailable or
    /// the global bounded cleanup registry has no admission available.
    pub fn spawn(command: Command) -> io::Result<Self> {
        Self::spawn_with_registry(command, cleanup_registry())
    }

    fn spawn_with_registry(command: Command, registry: Arc<CleanupRegistry>) -> io::Result<Self> {
        let permit = registry.try_acquire()?;
        spawn_atomic_supervisor(command, permit).map(Self::from_atomic)
    }

    fn from_atomic(child: AtomicChild) -> Self {
        let process_group = libc::pid_t::try_from(child.process_id)
            .expect("clone3 returned a positive pid_t-compatible process id");
        Self {
            process_id: child.process_id,
            process_group,
            pidfd: Some(child.pidfd),
            plugin: Some(child.plugin),
            plugin_pidfd: Some(child.plugin_pidfd),
            reader: Some(child.reader),
            writer: Some(child.writer),
            plugin_status: None,
            process_group_terminated: false,
            shutdown: None,
            cleanup_permit: Some(child.cleanup_permit),
            #[cfg(test)]
            cleanup_barrier: None,
        }
    }

    /// Return the process-group leader PID.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.process_id
    }

    /// Return the executable plugin PID inside the supervisor's private process group.
    #[must_use]
    pub fn executable_id(&self) -> u32 {
        self.plugin
            .as_ref()
            .expect("owned plugin child is present before cleanup")
            .id()
    }

    /// Observe an exited executable's status without reaping it or releasing its identity.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the kernel cannot inspect the atomically transferred pidfd.
    pub fn executable_status(&self) -> io::Result<Option<ExitStatus>> {
        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: plugin_pidfd is live and information is writable. WNOWAIT preserves the status
        // for the owned Child waiter, while WNOHANG makes a live executable return immediately.
        let result = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                libc::id_t::try_from(self.plugin_pidfd().as_raw_fd()).expect("pidfd fits id_t"),
                information.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful waitid initialized the supplied siginfo storage. A zero si_pid means
        // WNOHANG found no exited process.
        let information = unsafe { information.assume_init() };
        if unsafe { information.si_pid() } == 0 {
            Ok(None)
        } else {
            validated_exit_status_from_siginfo(&information).map(Some)
        }
    }

    fn supervisor_pidfd(&self) -> &OwnedFd {
        self.pidfd
            .as_ref()
            .expect("owned supervisor pidfd is present before cleanup")
    }

    fn plugin_pidfd(&self) -> &OwnedFd {
        self.plugin_pidfd
            .as_ref()
            .expect("owned plugin pidfd is present before cleanup")
    }

    /// Move the protocol streams out of the child before constructing a framed backend.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if either stream was already taken.
    pub fn take_transport(&mut self) -> io::Result<(File, File)> {
        let reader = self.reader.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "plugin stdout was already taken")
        })?;
        let writer = self.writer.take().ok_or_else(|| {
            io::Error::new(io::ErrorKind::BrokenPipe, "plugin stdin was already taken")
        })?;
        Ok((reader, writer))
    }

    /// Connect the framed backend on an owned worker with a bounded Hello phase.
    ///
    /// The returned cancellation handle remains usable while `next_item` blocks on another thread.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessPluginError`] after terminating the process group, joining the worker, and
    /// reaping the leader when transport setup, negotiation, or supervision fails.
    pub fn connect(
        mut self,
        timeouts: ProcessPhaseTimeouts,
    ) -> Result<(ProcessPluginBackend, ProcessPluginCancellation), ProcessPluginError> {
        let process_id = self.id();
        let (reader, writer) = self
            .take_transport()
            .map_err(|error| ProcessPluginError::io(&error))?;
        set_nonblocking(reader.as_raw_fd()).map_err(|error| ProcessPluginError::io(&error))?;
        set_nonblocking(writer.as_raw_fd()).map_err(|error| ProcessPluginError::io(&error))?;
        let cancellation =
            Arc::new(CancellationSignal::new().map_err(|error| ProcessPluginError::io(&error))?);
        let activity = Arc::new(ProtocolActivity::new());
        let reader = CancellableReader {
            reader,
            cancellation: Arc::clone(&cancellation),
            activity: Arc::clone(&activity),
        };
        let shared_writer = Arc::new(Mutex::new(writer));
        let writer = CancellableWriter {
            writer: Arc::clone(&shared_writer),
            cancellation: Arc::clone(&cancellation),
            deadline: None,
        };
        let close_port = ClosePort::new(CancellableWriter {
            writer: shared_writer,
            cancellation: Arc::clone(&cancellation),
            deadline: None,
        });
        let worker_activity = Arc::clone(&activity);
        let (commands, command_receiver) = mpsc::channel();
        let (hello_sender, hello_receiver) = mpsc::channel();
        let (worker_done_sender, worker_done_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name(format!("chat-plugin-{process_id}"))
            .spawn(move || {
                protocol_worker(
                    reader,
                    writer,
                    command_receiver,
                    hello_sender,
                    worker_activity,
                );
                let _ = worker_done_sender.send(());
            })
            .map_err(|error| ProcessPluginError::io(&error))?;
        let core = Arc::new(ProcessCore::new(
            self,
            worker,
            worker_done_receiver,
            commands,
            cancellation,
            close_port,
            activity,
        ));
        let capabilities = match hello_receiver.recv_timeout(timeouts.hello()) {
            Ok(Ok(capabilities)) => capabilities,
            Ok(Err(error)) => {
                return Err(
                    ProcessPluginError::protocol(&error).with_cleanup(core.finish(Duration::ZERO))
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(ProcessPluginError::timeout("hello", timeouts.hello())
                    .with_cleanup(core.finish(Duration::ZERO)));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ProcessPluginError::new(
                    "plugin_worker_stopped",
                    "plugin protocol worker stopped during Hello",
                )
                .with_cleanup(core.finish(Duration::ZERO)));
            }
        };
        let runtime = ProcessRuntime {
            core: Arc::clone(&core),
        };
        let cancellation = ProcessPluginCancellation { core, timeouts };
        Ok((
            ProcessPluginBackend {
                capabilities,
                runtime: Some(runtime),
                cancellation: cancellation.clone(),
                timeouts,
            },
            cancellation,
        ))
    }

    /// Wait cooperatively, terminate the complete private process group, and reap its leader.
    ///
    /// The leader is observed through pidfd without reaping. The process group is always terminated
    /// before `wait`, so an exited leader cannot release its PID/PGID for reuse before descendants
    /// are handled.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] for process-control failures or if the leader cannot be observed
    /// within the fixed post-kill bound.
    pub fn shutdown(&mut self, grace: Duration) -> io::Result<ExitStatus> {
        self.shutdown_with_reap_timeout(grace, KILL_REAP_TIMEOUT)
    }

    /// Wait cooperatively until either the executable exits, the grace expires, or an external
    /// event descriptor becomes readable, then terminate and reap the complete process group.
    ///
    /// `cancellation_fd` is borrowed for this call and must be a pollable descriptor whose
    /// readability means cancellation. This keeps cancellation inside the pidfd/process-group
    /// supervisor instead of making callers signal a numeric PID or process group themselves.
    pub fn shutdown_cancellable(
        &mut self,
        grace: Duration,
        cancellation_fd: RawFd,
    ) -> io::Result<ExitStatus> {
        let started = Instant::now();
        let cooperative_deadline = started.checked_add(grace).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin cooperative shutdown deadline is too large",
            )
        })?;
        let final_deadline = cooperative_deadline
            .checked_add(KILL_REAP_TIMEOUT)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "plugin post-kill reap deadline is too large",
                )
            })?;
        if let Some(result) = &self.shutdown {
            return result.result();
        }
        let result =
            self.shutdown_once_cancellable(cooperative_deadline, final_deadline, cancellation_fd);
        self.shutdown = Some(StoredShutdown::from_result(&result));
        result
    }

    fn shutdown_with_reap_timeout(
        &mut self,
        grace: Duration,
        reap_timeout: Duration,
    ) -> io::Result<ExitStatus> {
        let started = Instant::now();
        let cooperative_deadline = started.checked_add(grace).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin cooperative shutdown deadline is too large",
            )
        })?;
        let final_deadline = cooperative_deadline
            .checked_add(reap_timeout)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "plugin post-kill reap deadline is too large",
                )
            })?;
        self.shutdown_until(cooperative_deadline, final_deadline)
    }

    fn shutdown_until(
        &mut self,
        cooperative_deadline: Instant,
        final_deadline: Instant,
    ) -> io::Result<ExitStatus> {
        if let Some(result) = &self.shutdown {
            return result.result();
        }
        let result = self.shutdown_once(cooperative_deadline, final_deadline);
        self.shutdown = Some(StoredShutdown::from_result(&result));
        result
    }

    fn shutdown_once(
        &mut self,
        cooperative_deadline: Instant,
        final_deadline: Instant,
    ) -> io::Result<ExitStatus> {
        let cooperative = self.wait_plugin_until(cooperative_deadline);
        let mut supervision_error = cooperative.as_ref().err().map(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot observe plugin exit during cooperative wait: {error}"),
            )
        });
        let mut plugin_status = cooperative.unwrap_or(None);
        let termination = self.terminate_processes();
        if plugin_status.is_none() && supervision_error.is_none() {
            match self.wait_plugin_until(final_deadline) {
                Ok(Some(status)) => plugin_status = Some(status),
                Ok(None) => {
                    supervision_error = Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "plugin did not exit after bounded process-group termination",
                    ));
                }
                Err(error) => supervision_error = Some(error),
            }
        }
        let supervisor_result = match wait_pidfd_until(self.supervisor_pidfd(), final_deadline) {
            Ok(true) => self.reap_supervisor(),
            Ok(false) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "plugin process-group supervisor did not exit after bounded kill",
            )),
            Err(error) => {
                if supervision_error.is_none() {
                    supervision_error = Some(io::Error::new(
                        error.kind(),
                        format!("cannot observe plugin supervisor after kill: {error}"),
                    ));
                }
                Err(error)
            }
        };
        if let Err(error) = termination {
            if supervision_error.is_none() {
                supervision_error = Some(error);
            }
        }
        if let Err(error) = supervisor_result {
            if supervision_error.is_none() {
                supervision_error = Some(error);
            }
        }
        if let Some(error) = supervision_error {
            Err(error)
        } else {
            plugin_status.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "plugin exit status was unavailable after bounded cleanup",
                )
            })
        }
    }

    fn shutdown_once_cancellable(
        &mut self,
        cooperative_deadline: Instant,
        final_deadline: Instant,
        cancellation_fd: RawFd,
    ) -> io::Result<ExitStatus> {
        let cooperative =
            self.wait_plugin_until_or_cancelled(cooperative_deadline, cancellation_fd);
        let mut supervision_error = cooperative.as_ref().err().map(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot observe plugin exit during cancellable cooperative wait: {error}"),
            )
        });
        let mut plugin_status = cooperative.unwrap_or(None);
        let termination = self.terminate_processes();
        if plugin_status.is_none() && supervision_error.is_none() {
            match self.wait_plugin_until(final_deadline) {
                Ok(Some(status)) => plugin_status = Some(status),
                Ok(None) => {
                    supervision_error = Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "plugin did not exit after bounded process-group termination",
                    ));
                }
                Err(error) => supervision_error = Some(error),
            }
        }
        let supervisor_result = match wait_pidfd_until(self.supervisor_pidfd(), final_deadline) {
            Ok(true) => self.reap_supervisor(),
            Ok(false) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "plugin process-group supervisor did not exit after bounded kill",
            )),
            Err(error) => {
                if supervision_error.is_none() {
                    supervision_error = Some(io::Error::new(
                        error.kind(),
                        format!("cannot observe plugin supervisor after kill: {error}"),
                    ));
                }
                Err(error)
            }
        };
        if let Err(error) = termination {
            if supervision_error.is_none() {
                supervision_error = Some(error);
            }
        }
        if let Err(error) = supervisor_result {
            if supervision_error.is_none() {
                supervision_error = Some(error);
            }
        }
        if let Some(error) = supervision_error {
            Err(error)
        } else {
            plugin_status.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "plugin exit status was unavailable after bounded cleanup",
                )
            })
        }
    }

    fn shutdown_eventual(&mut self, cooperative_deadline: Instant) -> io::Result<ExitStatus> {
        if let Some(result) = &self.shutdown {
            return result.result();
        }
        let cooperative = self.wait_plugin_until(cooperative_deadline);
        let mut supervision_error = cooperative.as_ref().err().map(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot observe plugin exit during cooperative wait: {error}"),
            )
        });
        let mut plugin_status = cooperative.unwrap_or(None);
        if let Err(error) = self.terminate_processes() {
            if supervision_error.is_none() {
                supervision_error = Some(error);
            }
        }
        #[cfg(test)]
        if let Some(barrier) = self.cleanup_barrier.take() {
            let _ = barrier.entered.send(());
            let _ = barrier.release.recv();
        }
        if plugin_status.is_none() {
            match self.wait_plugin_eventual() {
                Ok(status) => plugin_status = Some(status),
                Err(error) => {
                    if supervision_error.is_none() {
                        supervision_error = Some(error);
                    }
                }
            }
        }
        let _supervisor_status = self.reap_supervisor_tracked();
        let result = if let Some(error) = supervision_error {
            Err(error)
        } else {
            plugin_status.ok_or_else(|| {
                io::Error::other("plugin exit status unavailable after eventual cleanup")
            })
        };
        self.shutdown = Some(StoredShutdown::from_result(&result));
        result
    }

    fn wait_plugin_until(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.plugin_status {
            return Ok(Some(status));
        }
        if !wait_pidfd_until(self.plugin_pidfd(), deadline)? {
            return Ok(None);
        }
        let status = waitid_reap_pidfd(self.plugin_pidfd())?;
        self.plugin_status = Some(status);
        Ok(Some(status))
    }

    fn wait_plugin_until_or_cancelled(
        &mut self,
        deadline: Instant,
        cancellation_fd: RawFd,
    ) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.plugin_status {
            return Ok(Some(status));
        }
        if !wait_pidfd_until_or_cancelled(self.plugin_pidfd(), cancellation_fd, deadline)? {
            return Ok(None);
        }
        let status = waitid_reap_pidfd(self.plugin_pidfd())?;
        self.plugin_status = Some(status);
        Ok(Some(status))
    }

    fn wait_plugin_eventual(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.plugin_status {
            return Ok(status);
        }
        match reap_pidfd_tracked(self.plugin_pidfd()) {
            PidfdReapOutcome::Status(status) => {
                self.plugin_status = Some(status);
                Ok(status)
            }
            PidfdReapOutcome::StatusConsumed => Err(io::Error::from_raw_os_error(libc::ECHILD)),
        }
    }

    fn reap_supervisor(&mut self) -> io::Result<PidfdReapOutcome> {
        reap_supervisor_owned(self.supervisor_pidfd())
    }

    fn reap_supervisor_tracked(&mut self) -> PidfdReapOutcome {
        reap_pidfd_tracked(self.supervisor_pidfd())
    }

    fn terminate_processes(&mut self) -> io::Result<()> {
        let group_result = self.terminate_process_group();
        if group_result.is_ok() {
            return Ok(());
        }
        // Preserve the group-delivery diagnostic, but still address the leader by its stable pidfd
        // so a partial cleanup failure never turns into a live orphaned supervisor.
        let _ = signal_pidfd(self.supervisor_pidfd());
        group_result
    }

    fn terminate_process_group(&mut self) -> io::Result<()> {
        if self.process_group_terminated {
            return Ok(());
        }
        // SAFETY: process_group is the positive PID captured immediately after spawning the child
        // with process_group(0); negating it targets only that private process group.
        let result = unsafe { libc::kill(-self.process_group, libc::SIGKILL) };
        if result == 0 {
            self.process_group_terminated = true;
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            self.process_group_terminated = true;
            Ok(())
        } else {
            Err(error)
        }
    }
}

impl Drop for ProcessPluginChild {
    fn drop(&mut self) {
        if self.shutdown.is_none() {
            let _ = self.shutdown(Duration::ZERO);
        }
        if matches!(self.shutdown, Some(StoredShutdown::Failure(_, _))) {
            let _ = self.terminate_processes();
            let Some(cleanup_permit) = self.cleanup_permit.take() else {
                return;
            };
            let Some(supervisor_pidfd) = self.pidfd.take() else {
                return;
            };
            let Some(plugin) = self.plugin.take() else {
                return;
            };
            let Some(plugin_pidfd) = self.plugin_pidfd.take() else {
                return;
            };
            let registry = cleanup_permit.registry();
            let _ = registry.submit(
                Box::new(move || {
                    eventual_reap(plugin, plugin_pidfd, supervisor_pidfd);
                }),
                cleanup_permit,
            );
        }
    }
}

fn eventual_reap(plugin: Child, plugin_pidfd: OwnedFd, supervisor_pidfd: OwnedFd) {
    let _plugin = plugin;
    let _plugin_status = reap_pidfd_tracked(&plugin_pidfd);
    eventual_reap_supervisor(supervisor_pidfd);
}

enum WorkerCommand {
    Subscribe(
        Box<SubscribeRequest>,
        mpsc::Sender<Result<(), BackendFailure>>,
    ),
    Next(mpsc::Sender<Result<Option<SubscriptionItem>, BackendFailure>>),
    Acknowledge(DeliveryId, mpsc::Sender<Result<(), BackendFailure>>),
    CloseDriver(mpsc::Sender<Result<(), BackendFailure>>),
    CloseBackend(mpsc::Sender<Result<(), PluginError>>),
}

fn protocol_worker(
    reader: CancellableReader,
    writer: CancellableWriter,
    commands: mpsc::Receiver<WorkerCommand>,
    hello: mpsc::Sender<Result<BackendCapabilities, PluginError>>,
    activity: Arc<ProtocolActivity>,
) {
    struct StopActivity(Arc<ProtocolActivity>);

    impl Drop for StopActivity {
        fn drop(&mut self) {
            self.0.stop();
        }
    }

    let _stop_activity = StopActivity(Arc::clone(&activity));
    let mut backend = match PluginBackend::connect(reader, writer) {
        Ok(backend) => backend,
        Err(error) => {
            let _ = hello.send(Err(error));
            return;
        }
    };
    if hello.send(Ok(backend.capabilities())).is_err() {
        return;
    }
    activity.set(ProtocolWorkerPhase::Idle);
    let mut driver = match commands.recv() {
        Ok(WorkerCommand::Subscribe(request, response)) => {
            activity.set(ProtocolWorkerPhase::Control);
            match backend.subscribe(&request) {
                Ok(driver) => {
                    if response.send(Ok(())).is_err() {
                        return;
                    }
                    activity.set(ProtocolWorkerPhase::Idle);
                    driver
                }
                Err(error) => {
                    let _ = response.send(Err(error));
                    return;
                }
            }
        }
        Ok(WorkerCommand::CloseBackend(response)) => {
            activity.set(ProtocolWorkerPhase::Control);
            let _ = response.send(backend.close());
            return;
        }
        Ok(WorkerCommand::CloseDriver(response)) => {
            activity.set(ProtocolWorkerPhase::Control);
            let result = backend
                .close()
                .map_err(|error| backend_failure(error.code(), error.detail().to_owned(), false));
            let _ = response.send(result);
            return;
        }
        Ok(_) | Err(_) => return,
    };
    loop {
        match commands.recv() {
            Ok(WorkerCommand::Next(response)) => {
                activity.set(ProtocolWorkerPhase::Next);
                let mut result = driver.next_item();
                let observed_eof = matches!(
                    &result,
                    Err(error) if error.code() == "unexpected_eof"
                );
                let close_in_flight = activity.finish_next(observed_eof);
                if close_in_flight && observed_eof {
                    result = Err(backend_failure(
                        "plugin_receive_cancelled",
                        "plugin stdout reached EOF after the host wrote Close",
                        true,
                    ));
                }
                let terminal = close_in_flight || !matches!(result, Ok(Some(_)));
                let _ = response.send(result);
                if terminal {
                    return;
                }
                activity.set(ProtocolWorkerPhase::Idle);
            }
            Ok(WorkerCommand::Acknowledge(delivery_id, response)) => {
                activity.set(ProtocolWorkerPhase::Control);
                let result = driver.acknowledge(&delivery_id);
                let terminal = result.is_err();
                let _ = response.send(result);
                if terminal {
                    return;
                }
                activity.set(ProtocolWorkerPhase::Idle);
            }
            Ok(WorkerCommand::CloseDriver(response)) => {
                activity.set(ProtocolWorkerPhase::Control);
                let _ = response.send(driver.close());
                return;
            }
            Ok(_) => return,
            Err(_) => return,
        }
    }
}

struct ShutdownResources {
    child: ProcessPluginChild,
    worker: JoinHandle<()>,
    worker_done: mpsc::Receiver<()>,
}

#[derive(Clone, Debug)]
enum StoredShutdown {
    Success(ExitStatus),
    Failure(io::ErrorKind, String),
}

impl StoredShutdown {
    fn from_result(result: &io::Result<ExitStatus>) -> Self {
        match result {
            Ok(status) => Self::Success(*status),
            Err(error) => Self::Failure(error.kind(), error.to_string()),
        }
    }

    fn result(&self) -> io::Result<ExitStatus> {
        match self {
            Self::Success(status) => Ok(*status),
            Self::Failure(kind, detail) => Err(io::Error::new(*kind, detail.clone())),
        }
    }
}

enum ShutdownState {
    Running(ShutdownResources),
    Graceful {
        resources: ShutdownResources,
        cooperative_deadline: Instant,
        final_deadline: Instant,
    },
    Stopping {
        deadline: Instant,
        timed_out: bool,
        graceful: bool,
    },
    Done {
        result: StoredShutdown,
        graceful: bool,
    },
}

struct ShutdownCoordinator {
    state: Mutex<ShutdownState>,
    complete: Condvar,
}

struct CommandPort {
    sender: Mutex<Option<mpsc::Sender<WorkerCommand>>>,
}

impl CommandPort {
    fn new(sender: mpsc::Sender<WorkerCommand>) -> Self {
        Self {
            sender: Mutex::new(Some(sender)),
        }
    }

    fn dispatch(&self, command: WorkerCommand) -> Result<(), WorkerCommand> {
        self.dispatch_with_hook(command, || {})
    }

    fn dispatch_with_hook(
        &self,
        command: WorkerCommand,
        before_send: impl FnOnce(),
    ) -> Result<(), WorkerCommand> {
        let sender = ProcessCore::lock(&self.sender);
        let Some(sender) = sender.as_ref() else {
            return Err(command);
        };
        before_send();
        sender.send(command).map_err(|error| error.0)
    }

    fn dispatch_terminal_with_hook(
        &self,
        command: WorkerCommand,
        before_send: impl FnOnce(),
    ) -> Result<(), WorkerCommand> {
        let mut sender = ProcessCore::lock(&self.sender);
        let Some(active) = sender.as_ref() else {
            return Err(command);
        };
        before_send();
        let result = active.send(command).map_err(|error| error.0);
        sender.take();
        result
    }

    fn stop(&self) {
        self.stop_with_hook(|| {});
    }

    fn stop_with_hook(&self, before_lock: impl FnOnce()) {
        before_lock();
        ProcessCore::lock(&self.sender).take();
    }
}

struct ProcessCore {
    process_id: u32,
    commands: CommandPort,
    cancellation: Arc<CancellationSignal>,
    close_port: ClosePort,
    activity: Arc<ProtocolActivity>,
    shutdown: Arc<ShutdownCoordinator>,
}

impl ProcessCore {
    fn new(
        child: ProcessPluginChild,
        worker: JoinHandle<()>,
        worker_done: mpsc::Receiver<()>,
        commands: mpsc::Sender<WorkerCommand>,
        cancellation: Arc<CancellationSignal>,
        close_port: ClosePort,
        activity: Arc<ProtocolActivity>,
    ) -> Self {
        Self {
            process_id: child.id(),
            commands: CommandPort::new(commands),
            cancellation,
            close_port,
            activity,
            shutdown: Arc::new(ShutdownCoordinator {
                state: Mutex::new(ShutdownState::Running(ShutdownResources {
                    child,
                    worker,
                    worker_done,
                })),
                complete: Condvar::new(),
            }),
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn dispatch(&self, command: WorkerCommand) -> Result<(), WorkerCommand> {
        self.commands.dispatch(command)
    }

    fn graceful_finish(
        &self,
        close_timeout: Duration,
        shutdown_grace: Duration,
    ) -> io::Result<ExitStatus> {
        self.graceful_finish_with_hook(close_timeout, shutdown_grace, || {})
    }

    fn graceful_finish_with_hook(
        &self,
        close_timeout: Duration,
        shutdown_grace: Duration,
        after_admit: impl FnOnce(),
    ) -> io::Result<ExitStatus> {
        let started = Instant::now();
        let close_deadline = started.checked_add(close_timeout).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin Close deadline is too large",
            )
        })?;
        let cooperative_deadline = close_deadline.checked_add(shutdown_grace).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin graceful shutdown deadline is too large",
            )
        })?;
        let final_deadline = cooperative_deadline
            .checked_add(KILL_REAP_TIMEOUT)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "plugin final shutdown deadline is too large",
                )
            })?;

        let owner = {
            let mut state = Self::lock(&self.shutdown.state);
            match &*state {
                ShutdownState::Running(_) => {
                    let ShutdownState::Running(resources) = std::mem::replace(
                        &mut *state,
                        ShutdownState::Stopping {
                            deadline: final_deadline,
                            timed_out: false,
                            graceful: true,
                        },
                    ) else {
                        unreachable!("running shutdown state was just matched")
                    };
                    *state = ShutdownState::Graceful {
                        resources,
                        cooperative_deadline,
                        final_deadline,
                    };
                    self.shutdown.complete.notify_all();
                    true
                }
                ShutdownState::Graceful { .. } | ShutdownState::Stopping { .. } => false,
                ShutdownState::Done { result, .. } => return result.result(),
            }
        };
        if !owner {
            return self.finish(Duration::ZERO);
        }
        after_admit();

        let (response, receiver) = mpsc::channel();
        if self
            .commands
            .dispatch_terminal_with_hook(WorkerCommand::CloseDriver(response), || {})
            .is_err()
        {
            return self.finish_graceful_transaction(CleanupRequirement::Failure(
                io::ErrorKind::BrokenPipe,
                "plugin worker stopped before graceful Close admission".to_owned(),
            ));
        }

        loop {
            let observed_phase = self.activity.get();
            if self.activity.claim_next_for_close() {
                let close_result = self.close_port.send(close_deadline);
                self.activity.finish_close_write(close_result.is_ok());
                if let Err(error) = close_result {
                    return self.finish_graceful_transaction(CleanupRequirement::Failure(
                        error.kind(),
                        format!("cannot write Close during blocked receive: {error}"),
                    ));
                }
                return self.finish_graceful_transaction(CleanupRequirement::GracefulDirect(
                    Arc::clone(&self.activity),
                ));
            }

            match receiver.try_recv() {
                Ok(Ok(())) => {
                    return self.finish_graceful_transaction(CleanupRequirement::GracefulQueued);
                }
                Ok(Err(error)) => {
                    return self.finish_graceful_transaction(CleanupRequirement::Failure(
                        io::ErrorKind::BrokenPipe,
                        format!("plugin Close failed: {error}"),
                    ));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    return self.finish_graceful_transaction(CleanupRequirement::Failure(
                        io::ErrorKind::BrokenPipe,
                        "plugin worker stopped without confirming graceful Close".to_owned(),
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            let remaining = close_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return self.finish_graceful_transaction(CleanupRequirement::Failure(
                    io::ErrorKind::TimedOut,
                    format!("plugin Close exceeded {close_timeout:?}"),
                ));
            }
            self.activity
                .wait_until_changed(observed_phase, close_deadline);
        }
    }

    fn finish(&self, grace: Duration) -> io::Result<ExitStatus> {
        self.finish_with_reap_timeout_and_hook(grace, KILL_REAP_TIMEOUT, || {})
    }

    fn graceful_finish_admitted(&self) -> bool {
        matches!(
            &*Self::lock(&self.shutdown.state),
            ShutdownState::Graceful { .. }
                | ShutdownState::Stopping { graceful: true, .. }
                | ShutdownState::Done { graceful: true, .. }
        )
    }

    fn finish_with_reap_timeout_and_hook(
        &self,
        grace: Duration,
        reap_timeout: Duration,
        after_start: impl FnOnce(),
    ) -> io::Result<ExitStatus> {
        self.finish_with_mode(grace, reap_timeout, true, after_start)
    }

    fn finish_with_mode(
        &self,
        grace: Duration,
        reap_timeout: Duration,
        interrupt_protocol_io: bool,
        after_start: impl FnOnce(),
    ) -> io::Result<ExitStatus> {
        let started = Instant::now();
        let cooperative_deadline = started.checked_add(grace).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "plugin cooperative shutdown deadline is too large",
            )
        })?;
        let requested_deadline =
            cooperative_deadline
                .checked_add(reap_timeout)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "plugin shutdown deadline is too large",
                    )
                })?;
        let (resources, cooperative_deadline, requirement, interrupt) = {
            let mut state = Self::lock(&self.shutdown.state);
            loop {
                match &mut *state {
                    ShutdownState::Done { result, .. } => return result.result(),
                    ShutdownState::Stopping {
                        deadline,
                        timed_out,
                        ..
                    } => {
                        if *timed_out {
                            return shutdown_deadline_error();
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            *timed_out = true;
                            return shutdown_deadline_error();
                        }
                        let waited = self.shutdown.complete.wait_timeout(state, remaining);
                        let (next_state, wait_result) =
                            waited.unwrap_or_else(std::sync::PoisonError::into_inner);
                        state = next_state;
                        if wait_result.timed_out() {
                            if let ShutdownState::Stopping { timed_out, .. } = &mut *state {
                                *timed_out = true;
                                return shutdown_deadline_error();
                            }
                        }
                    }
                    ShutdownState::Graceful {
                        cooperative_deadline,
                        final_deadline,
                        ..
                    } => {
                        let fallback_at = *cooperative_deadline;
                        let final_at = *final_deadline;
                        let remaining = fallback_at.saturating_duration_since(Instant::now());
                        if !remaining.is_zero() {
                            let waited = self.shutdown.complete.wait_timeout(state, remaining);
                            state = waited.unwrap_or_else(std::sync::PoisonError::into_inner).0;
                            continue;
                        }
                        let ShutdownState::Graceful { resources, .. } = std::mem::replace(
                            &mut *state,
                            ShutdownState::Stopping {
                                deadline: final_at,
                                timed_out: false,
                                graceful: true,
                            },
                        ) else {
                            unreachable!("graceful shutdown state was just matched")
                        };
                        break (
                            resources,
                            Instant::now(),
                            CleanupRequirement::Failure(
                                io::ErrorKind::TimedOut,
                                "plugin graceful Close exceeded its shared absolute deadline"
                                    .to_owned(),
                            ),
                            true,
                        );
                    }
                    ShutdownState::Running(_) => {
                        let ShutdownState::Running(resources) = std::mem::replace(
                            &mut *state,
                            ShutdownState::Stopping {
                                deadline: requested_deadline,
                                timed_out: false,
                                graceful: false,
                            },
                        ) else {
                            unreachable!("running state was just matched")
                        };
                        break (
                            resources,
                            cooperative_deadline,
                            CleanupRequirement::Raw,
                            interrupt_protocol_io,
                        );
                    }
                }
            }
        };
        self.finish_owned(
            resources,
            cooperative_deadline,
            requirement,
            interrupt,
            after_start,
        )
    }

    fn finish_graceful_transaction(
        &self,
        requirement: CleanupRequirement,
    ) -> io::Result<ExitStatus> {
        let (resources, cooperative_deadline) = {
            let mut state = Self::lock(&self.shutdown.state);
            loop {
                match &mut *state {
                    ShutdownState::Done { result, .. } => return result.result(),
                    ShutdownState::Stopping {
                        deadline,
                        timed_out,
                        ..
                    } => {
                        if *timed_out {
                            return shutdown_deadline_error();
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            *timed_out = true;
                            return shutdown_deadline_error();
                        }
                        let waited = self.shutdown.complete.wait_timeout(state, remaining);
                        state = waited.unwrap_or_else(std::sync::PoisonError::into_inner).0;
                    }
                    ShutdownState::Graceful {
                        cooperative_deadline,
                        final_deadline,
                        ..
                    } => {
                        let cooperative = if matches!(requirement, CleanupRequirement::Failure(..))
                        {
                            Instant::now()
                        } else {
                            *cooperative_deadline
                        };
                        let final_at = *final_deadline;
                        let ShutdownState::Graceful { resources, .. } = std::mem::replace(
                            &mut *state,
                            ShutdownState::Stopping {
                                deadline: final_at,
                                timed_out: false,
                                graceful: true,
                            },
                        ) else {
                            unreachable!("graceful shutdown state was just matched")
                        };
                        break (resources, cooperative);
                    }
                    ShutdownState::Running(_) => {
                        unreachable!("graceful completion requires an admitted transaction")
                    }
                }
            }
        };
        self.finish_owned(resources, cooperative_deadline, requirement, false, || {})
    }

    fn finish_owned(
        &self,
        mut resources: ShutdownResources,
        cooperative_deadline: Instant,
        requirement: CleanupRequirement,
        interrupt_protocol_io: bool,
        after_start: impl FnOnce(),
    ) -> io::Result<ExitStatus> {
        self.commands.stop();
        self.close_port.stop();
        if interrupt_protocol_io {
            self.cancellation.cancel();
        }
        after_start();
        let shutdown = Arc::clone(&self.shutdown);
        let cleanup_permit = resources
            .child
            .cleanup_permit
            .take()
            .expect("a running plugin child owns one cleanup admission");
        let registry = cleanup_permit.registry();
        let cleanup_task = Box::new(move || {
            let result = cleanup_resources(resources, cooperative_deadline, requirement);
            let mut state = ProcessCore::lock(&shutdown.state);
            if matches!(&*state, ShutdownState::Done { .. }) {
                shutdown.complete.notify_all();
                return;
            }
            let graceful = matches!(&*state, ShutdownState::Stopping { graceful: true, .. });
            let result = if matches!(
                &*state,
                ShutdownState::Stopping {
                    timed_out: true,
                    ..
                }
            ) {
                StoredShutdown::Failure(
                    io::ErrorKind::TimedOut,
                    "plugin shutdown exceeded its shared absolute deadline".to_owned(),
                )
            } else {
                result
            };
            *state = ShutdownState::Done { result, graceful };
            shutdown.complete.notify_all();
        });
        if let Err(error) = registry.submit(cleanup_task, cleanup_permit) {
            let result = StoredShutdown::Failure(
                error.kind(),
                format!(
                    "tracked plugin cleanup could not start; bounded ownership was retained: {error}"
                ),
            );
            let returned = result.result();
            let mut state = Self::lock(&self.shutdown.state);
            let graceful = matches!(&*state, ShutdownState::Stopping { graceful: true, .. });
            *state = ShutdownState::Done { result, graceful };
            self.shutdown.complete.notify_all();
            return returned;
        }

        let mut state = Self::lock(&self.shutdown.state);
        loop {
            match &mut *state {
                ShutdownState::Done { result, .. } => return result.result(),
                ShutdownState::Stopping {
                    deadline,
                    timed_out,
                    ..
                } => {
                    if *timed_out {
                        return shutdown_deadline_error();
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        *timed_out = true;
                        return shutdown_deadline_error();
                    }
                    let waited = self.shutdown.complete.wait_timeout(state, remaining);
                    let (next_state, wait_result) =
                        waited.unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = next_state;
                    if wait_result.timed_out() {
                        if let ShutdownState::Stopping { timed_out, .. } = &mut *state {
                            *timed_out = true;
                            return shutdown_deadline_error();
                        }
                    }
                }
                ShutdownState::Running(_) => {
                    unreachable!("cleanup resources were transferred before waiting")
                }
                ShutdownState::Graceful { .. } => {
                    unreachable!("graceful resources were transferred before waiting")
                }
            }
        }
    }
}

fn shutdown_deadline_error() -> io::Result<ExitStatus> {
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "plugin shutdown exceeded its shared absolute deadline",
    ))
}

enum CleanupRequirement {
    Raw,
    GracefulQueued,
    GracefulDirect(Arc<ProtocolActivity>),
    Failure(io::ErrorKind, String),
}

fn cleanup_resources(
    resources: ShutdownResources,
    cooperative_deadline: Instant,
    requirement: CleanupRequirement,
) -> StoredShutdown {
    let ShutdownResources {
        mut child,
        worker,
        worker_done,
    } = resources;
    let child_result = child.shutdown_eventual(cooperative_deadline);
    let _ = worker_done.recv();
    let worker_result = worker
        .join()
        .map_err(|_| io::Error::other("plugin protocol worker panicked"));
    let cleanup = match (child_result, worker_result) {
        (Err(error), _) => StoredShutdown::Failure(error.kind(), error.to_string()),
        (Ok(_), Err(error)) => StoredShutdown::Failure(error.kind(), error.to_string()),
        (Ok(status), Ok(())) => StoredShutdown::Success(status),
    };
    match (cleanup, requirement) {
        (StoredShutdown::Failure(kind, cleanup), CleanupRequirement::Failure(_, protocol)) => {
            StoredShutdown::Failure(
                kind,
                format!("{protocol}; process cleanup also failed: {cleanup}"),
            )
        }
        (failure @ StoredShutdown::Failure(..), _) => failure,
        (StoredShutdown::Success(status), CleanupRequirement::Raw) => {
            StoredShutdown::Success(status)
        }
        (StoredShutdown::Success(status), CleanupRequirement::Failure(kind, detail)) => {
            let detail = if status.success() {
                detail
            } else {
                format!("{detail}; forced cleanup exited with {status}")
            };
            StoredShutdown::Failure(kind, detail)
        }
        (StoredShutdown::Success(status), CleanupRequirement::GracefulQueued) => {
            if status.success() {
                StoredShutdown::Success(status)
            } else {
                StoredShutdown::Failure(
                    io::ErrorKind::Other,
                    format!("plugin required forced shutdown after Close and exited with {status}"),
                )
            }
        }
        (StoredShutdown::Success(status), CleanupRequirement::GracefulDirect(activity)) => {
            if !status.success() {
                StoredShutdown::Failure(
                    io::ErrorKind::Other,
                    format!("plugin required forced shutdown after Close and exited with {status}"),
                )
            } else if activity.get() != ProtocolWorkerPhase::CloseEof {
                StoredShutdown::Failure(
                    io::ErrorKind::InvalidData,
                    "plugin exited without the blocked receive observing post-Close EOF".to_owned(),
                )
            } else {
                StoredShutdown::Success(status)
            }
        }
    }
}

#[derive(Clone)]
struct ProcessRuntime {
    core: Arc<ProcessCore>,
}

impl ProcessRuntime {
    fn subscribe(
        &self,
        request: SubscribeRequest,
        timeout: Duration,
    ) -> Result<(), BackendFailure> {
        let (response, receiver) = mpsc::channel();
        if self
            .core
            .dispatch(WorkerCommand::Subscribe(Box::new(request), response))
            .is_err()
        {
            let cleanup = self.core.finish(Duration::ZERO);
            let graceful = self.core.graceful_finish_admitted();
            return Err(backend_failure(
                if graceful {
                    "plugin_start_cancelled"
                } else {
                    "plugin_worker_stopped"
                },
                cleanup_detail(
                    if graceful {
                        "plugin graceful Close retired the generation before Start admission"
                            .to_owned()
                    } else {
                        "plugin worker stopped before Start".to_owned()
                    },
                    cleanup,
                ),
                true,
            ));
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                if result.is_err() {
                    let _ = self.core.finish(Duration::ZERO);
                }
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let cleanup = self.core.finish(Duration::ZERO);
                Err(backend_failure(
                    "plugin_start_timeout",
                    cleanup_detail(format!("plugin Start exceeded {timeout:?}"), cleanup),
                    true,
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let cleanup = self.core.finish(Duration::ZERO);
                Err(backend_failure(
                    "plugin_worker_stopped",
                    cleanup_detail("plugin worker stopped during Start".to_owned(), cleanup),
                    true,
                ))
            }
        }
    }

    fn next_item(
        &self,
        shutdown_grace: Duration,
    ) -> Result<Option<SubscriptionItem>, BackendFailure> {
        let (response, receiver) = mpsc::channel();
        if self.core.dispatch(WorkerCommand::Next(response)).is_err() {
            let cleanup = self.core.finish(Duration::ZERO);
            let graceful = self.core.graceful_finish_admitted();
            return Err(backend_failure(
                if graceful {
                    "plugin_receive_cancelled"
                } else {
                    "plugin_worker_stopped"
                },
                cleanup_detail(
                    if graceful {
                        "plugin graceful Close retired the receive before it started".to_owned()
                    } else {
                        "plugin worker stopped before receiving an item".to_owned()
                    },
                    cleanup,
                ),
                true,
            ));
        }
        let result = receiver.recv().map_err(|_| {
            let cleanup = self.core.finish(Duration::ZERO);
            backend_failure(
                "plugin_worker_stopped",
                cleanup_detail(
                    "plugin worker stopped while receiving an item".to_owned(),
                    cleanup,
                ),
                true,
            )
        })?;
        match result {
            Ok(Some(item)) => Ok(Some(item)),
            Ok(None) => {
                let status = self.core.finish(shutdown_grace).map_err(|error| {
                    backend_failure(
                        "plugin_shutdown_failed",
                        format!("plugin shutdown after clean end failed: {error}"),
                        true,
                    )
                })?;
                if status.success() {
                    Ok(None)
                } else {
                    Err(backend_failure(
                        "plugin_unclean_end",
                        format!("plugin sent End but exited with {status}"),
                        true,
                    ))
                }
            }
            Err(error) => {
                if error.code() == "plugin_receive_cancelled" {
                    return Err(error);
                }
                let cleanup = self.core.finish(Duration::ZERO);
                if let Err(cleanup) = cleanup {
                    Err(backend_failure(
                        error.code(),
                        format!("{}; process cleanup also failed: {cleanup}", error.detail()),
                        error.retryable(),
                    ))
                } else {
                    Err(error)
                }
            }
        }
    }

    fn acknowledge(
        &self,
        delivery_id: DeliveryId,
        timeout: Duration,
    ) -> Result<(), BackendFailure> {
        let (response, receiver) = mpsc::channel();
        if self
            .core
            .dispatch(WorkerCommand::Acknowledge(delivery_id, response))
            .is_err()
        {
            let cleanup = self.core.finish(Duration::ZERO);
            return Err(backend_failure(
                "commit_outcome_unknown",
                cleanup_detail(
                    "plugin worker stopped before Commit confirmation".to_owned(),
                    cleanup,
                ),
                true,
            ));
        }
        match receiver.recv_timeout(timeout) {
            Ok(result) => {
                if result.is_err() {
                    let _ = self.core.finish(Duration::ZERO);
                }
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let cleanup = self.core.finish(Duration::ZERO);
                Err(backend_failure(
                    "commit_outcome_unknown",
                    cleanup_detail(format!("plugin Commit exceeded {timeout:?}"), cleanup),
                    true,
                ))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let cleanup = self.core.finish(Duration::ZERO);
                Err(backend_failure(
                    "commit_outcome_unknown",
                    cleanup_detail("plugin worker stopped during Commit".to_owned(), cleanup),
                    true,
                ))
            }
        }
    }

    fn close_driver(
        &self,
        timeout: Duration,
        shutdown_grace: Duration,
    ) -> Result<(), BackendFailure> {
        let (response, receiver) = mpsc::channel();
        if self
            .core
            .dispatch(WorkerCommand::CloseDriver(response))
            .is_err()
        {
            let cleanup = self.core.finish(Duration::ZERO);
            return Err(backend_failure(
                "plugin_close_failed",
                cleanup_detail("plugin worker stopped before Close".to_owned(), cleanup),
                false,
            ));
        }
        let protocol_result = match receiver.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(backend_failure(
                "plugin_close_timeout",
                format!("plugin Close exceeded {timeout:?}"),
                false,
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(backend_failure(
                "plugin_close_failed",
                "plugin worker stopped during Close",
                false,
            )),
        };
        let grace = if protocol_result.is_ok() {
            shutdown_grace
        } else {
            Duration::ZERO
        };
        let shutdown = self.core.finish(grace);
        protocol_result?;
        let status = shutdown.map_err(|error| {
            backend_failure(
                "plugin_shutdown_failed",
                format!("plugin shutdown failed: {error}"),
                false,
            )
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(backend_failure(
                "plugin_forced_shutdown",
                format!("plugin ignored Close and exited with {status}"),
                false,
            ))
        }
    }

    fn close_backend(
        &self,
        timeout: Duration,
        shutdown_grace: Duration,
    ) -> Result<ExitStatus, ProcessPluginError> {
        let (response, receiver) = mpsc::channel();
        if self
            .core
            .dispatch(WorkerCommand::CloseBackend(response))
            .is_err()
        {
            let cleanup = self.core.finish(Duration::ZERO);
            return Err(ProcessPluginError::new(
                "plugin_worker_stopped",
                cleanup_detail("plugin worker stopped before Close".to_owned(), cleanup),
            ));
        }
        let protocol = match receiver.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(ProcessPluginError::protocol(&error)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                Err(ProcessPluginError::timeout("close", timeout))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProcessPluginError::new(
                "plugin_worker_stopped",
                "plugin worker stopped during Close",
            )),
        };
        let grace = if protocol.is_ok() {
            shutdown_grace
        } else {
            Duration::ZERO
        };
        let shutdown = self.core.finish(grace);
        protocol?;
        shutdown.map_err(|error| ProcessPluginError::io(&error))
    }
}

impl Drop for ProcessRuntime {
    fn drop(&mut self) {
        let _ = self.core.finish(Duration::ZERO);
    }
}

fn cleanup_detail(prefix: String, cleanup: io::Result<ExitStatus>) -> String {
    match cleanup {
        Ok(_) => prefix,
        Err(error) => format!("{prefix}; process cleanup also failed: {error}"),
    }
}

/// A connected, single-use process plugin implementing the ordinary backend trait.
pub struct ProcessPluginBackend {
    capabilities: BackendCapabilities,
    runtime: Option<ProcessRuntime>,
    cancellation: ProcessPluginCancellation,
    timeouts: ProcessPhaseTimeouts,
}

impl ProcessPluginBackend {
    /// Cooperatively close a negotiated process before Start, then supervise its exit.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessPluginError`] for a bounded Close or supervision failure.
    pub fn close(&mut self) -> Result<(), ProcessPluginError> {
        let runtime = self.runtime.take().ok_or_else(|| {
            ProcessPluginError::new(
                "plugin_process_stopped",
                "plugin process is already stopped",
            )
        })?;
        let status =
            runtime.close_backend(self.timeouts.close(), self.timeouts.shutdown_grace())?;
        if status.success() {
            Ok(())
        } else {
            Err(ProcessPluginError::new(
                "plugin_forced_shutdown",
                format!("plugin ignored Close and exited with {status}"),
            ))
        }
    }
}

impl ChatSubscriptionBackend for ProcessPluginBackend {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(self.cancellation.clone())
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }

    fn subscribe(
        &mut self,
        request: &SubscribeRequest,
    ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
        let runtime = self.runtime.take().ok_or_else(|| {
            backend_failure(
                "plugin_already_subscribed",
                "one plugin process can open only one subscription",
                false,
            )
        })?;
        runtime.subscribe(request.clone(), self.timeouts.start())?;
        Ok(Box::new(ProcessPluginDriver {
            runtime: Some(runtime),
            cancellation: self.cancellation.clone(),
            timeouts: self.timeouts,
            clean_end: false,
        }))
    }
}

impl Drop for ProcessPluginBackend {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.core.finish(Duration::ZERO);
        }
    }
}

struct ProcessPluginDriver {
    runtime: Option<ProcessRuntime>,
    cancellation: ProcessPluginCancellation,
    timeouts: ProcessPhaseTimeouts,
    clean_end: bool,
}

impl ChatSubscriptionDriver for ProcessPluginDriver {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(self.cancellation.clone())
    }

    fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
        let result = self
            .runtime
            .as_ref()
            .ok_or_else(|| {
                backend_failure(
                    "plugin_process_stopped",
                    "plugin process is already stopped",
                    true,
                )
            })?
            .next_item(self.timeouts.shutdown_grace());
        if matches!(result, Ok(None)) {
            self.clean_end = true;
        }
        result
    }

    fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
        self.runtime
            .as_ref()
            .ok_or_else(|| {
                backend_failure(
                    "commit_outcome_unknown",
                    "plugin process stopped before Commit",
                    true,
                )
            })?
            .acknowledge(delivery_id.clone(), self.timeouts.commit())
    }

    fn close(&mut self) -> Result<(), BackendFailure> {
        let runtime = self.runtime.take().ok_or_else(|| {
            backend_failure(
                "plugin_process_stopped",
                "plugin process is already stopped",
                false,
            )
        })?;
        if self.clean_end {
            return Ok(());
        }
        runtime.close_driver(self.timeouts.close(), self.timeouts.shutdown_grace())
    }
}

impl Drop for ProcessPluginDriver {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            let _ = runtime.core.finish(self.timeouts.shutdown_grace());
        }
    }
}

/// Independent cancellation authority for a connected plugin process.
#[derive(Clone)]
pub struct ProcessPluginCancellation {
    core: Arc<ProcessCore>,
    timeouts: ProcessPhaseTimeouts,
}

impl ProcessPluginCancellation {
    /// Return the supervised process-group leader PID.
    #[must_use]
    pub fn process_id(&self) -> u32 {
        self.core.process_id
    }

    /// Report whether the framed worker is currently waiting for the next provider frame.
    ///
    /// This is an observation for diagnostics and deterministic lifecycle tests, not admission
    /// authority: the phase may change immediately after this method returns.
    #[must_use]
    pub fn receive_in_progress(&self) -> bool {
        self.core.activity.get() == ProtocolWorkerPhase::Next
    }

    /// Report whether a graceful shutdown transaction owns this process generation.
    ///
    /// This is an observation for diagnostics and deterministic lifecycle tests, not admission
    /// authority: the transaction may complete immediately after this method returns.
    #[must_use]
    pub fn graceful_shutdown_in_progress(&self) -> bool {
        self.core.graceful_finish_admitted()
    }

    /// Interrupt any blocked `next_item` through semantic Close, join the protocol worker, and
    /// reap the leader. A bounded semantic failure falls back to process-group termination.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if process termination or bounded reaping fails.
    pub fn cancel(&self) -> io::Result<ExitStatus> {
        self.core
            .graceful_finish(self.timeouts.close(), self.timeouts.shutdown_grace())
    }
}

impl ChatSubscriptionCancellation for ProcessPluginCancellation {
    fn cancel(&self) -> Result<(), CancellationError> {
        ProcessPluginCancellation::cancel(self)
            .map(|_| ())
            .map_err(|error| {
                CancellationError::CleanupUncertain(backend_failure(
                    "plugin_cleanup_uncertain",
                    error.to_string(),
                    true,
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::io::Read as _;
    use std::os::fd::IntoRawFd;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::sync::Barrier;

    const AUTOREAP_HELPER_ENV: &str = "CHAT_SUBSCRIPTION_AUTOREAP_HELPER";
    const AUTOREAP_MARKER_ENV: &str = "CHAT_SUBSCRIPTION_AUTOREAP_MARKER";
    const SPAWN_RACE_HELPER_ENV: &str = "CHAT_SUBSCRIPTION_SPAWN_RACE_HELPER";
    const SIGNAL_MASK_HELPER_ENV: &str = "CHAT_SUBSCRIPTION_SIGNAL_MASK_HELPER";
    const TRANSFER_FAILURE_HELPER_ENV: &str = "CHAT_SUBSCRIPTION_TRANSFER_FAILURE_HELPER";

    extern "C" fn inherited_signal_handler(_signal: libc::c_int) {
        // SAFETY: _exit is async-signal-safe. Reaching this handler in the sentinel is the failure
        // the isolated regression is designed to make immediately observable.
        unsafe { libc::_exit(91) }
    }

    fn current_signal_mask() -> u64 {
        let mut mask = 0_u64;
        // SAFETY: a null set pointer queries the calling thread and mask is writable kernel sigset
        // storage of the exact size required by rt_sigprocmask.
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_rt_sigprocmask,
                    libc::SIG_SETMASK,
                    std::ptr::null::<u64>(),
                    &raw mut mask,
                    std::mem::size_of::<u64>(),
                )
            },
            0
        );
        mask
    }

    fn signal_bit(signal: i32) -> u64 {
        1_u64 << u32::try_from(signal - 1).expect("positive Linux signal number")
    }

    fn inert_protocol_controls(
        cancellation: &Arc<CancellationSignal>,
    ) -> (ClosePort, Arc<ProtocolActivity>) {
        let writer = File::options()
            .write(true)
            .open("/dev/null")
            .expect("open inert Close sink");
        (
            ClosePort::new(CancellableWriter {
                writer: Arc::new(Mutex::new(writer)),
                cancellation: Arc::clone(cancellation),
                deadline: None,
            }),
            Arc::new(ProtocolActivity::new()),
        )
    }

    fn sleeping_core(
        worker: impl FnOnce(mpsc::Receiver<WorkerCommand>) + Send + 'static,
    ) -> (Arc<ProcessCore>, u32) {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        test_core(command, move |commands, _activity| worker(commands))
    }

    fn test_core(
        command: Command,
        worker: impl FnOnce(mpsc::Receiver<WorkerCommand>, Arc<ProtocolActivity>) + Send + 'static,
    ) -> (Arc<ProcessCore>, u32) {
        let child = ProcessPluginChild::spawn(command).expect("spawn supervised fixture");
        let process_id = child.id();
        let (sender, receiver) = mpsc::channel();
        let (worker_done_sender, worker_done_receiver) = mpsc::sync_channel(1);
        let cancellation = Arc::new(CancellationSignal::new().expect("create cancellation event"));
        let (close_port, activity) = inert_protocol_controls(&cancellation);
        let worker_activity = Arc::clone(&activity);
        let worker = thread::spawn(move || {
            worker(receiver, Arc::clone(&worker_activity));
            // Production protocol workers publish Stopped from their scope guard after the
            // terminal response. Keep this synthetic worker faithful: graceful waiters use that
            // transition to avoid missing the response-channel send between activity checks.
            worker_activity.stop();
            let _ = worker_done_sender.send(());
        });
        (
            Arc::new(ProcessCore::new(
                child,
                worker,
                worker_done_receiver,
                sender,
                cancellation,
                close_port,
                activity,
            )),
            process_id,
        )
    }

    fn responsive_close_core() -> (Arc<ProcessCore>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, entered_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::channel();
        let (core, _process_id) =
            test_core(Command::new("/bin/true"), move |commands, activity| {
                activity.set(ProtocolWorkerPhase::Idle);
                let WorkerCommand::CloseDriver(response) = commands
                    .recv()
                    .expect("graceful transaction sends one terminal Close")
                else {
                    panic!("graceful transaction sent a non-Close command")
                };
                entered.send(()).expect("announce Close admission");
                release_receiver.recv().expect("release semantic Close");
                activity.set(ProtocolWorkerPhase::Control);
                response.send(Ok(())).expect("confirm semantic Close");
                assert!(
                    commands.recv().is_err(),
                    "terminal admission closes commands"
                );
            });
        (core, entered_receiver, release)
    }

    #[test]
    fn phase_deadlines_are_explicit_and_positive() {
        let positive = Duration::from_millis(1);
        let error =
            ProcessPhaseTimeouts::new(Duration::ZERO, positive, positive, positive, Duration::ZERO)
                .expect_err("zero Hello deadline must be refused");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("hello"));
    }

    #[test]
    fn pre_close_eof_is_not_relabelled_as_post_close_cancellation() {
        let activity = ProtocolActivity::new();
        activity.set(ProtocolWorkerPhase::Next);
        activity.transport_eof();

        assert!(!activity.claim_next_for_close());
        assert!(!activity.finish_next(true));
        assert_eq!(activity.get(), ProtocolWorkerPhase::NextResultReady);
    }

    #[test]
    fn eof_during_close_write_waits_for_the_exact_write_outcome() {
        let activity = Arc::new(ProtocolActivity::new());
        activity.set(ProtocolWorkerPhase::Next);
        assert!(activity.claim_next_for_close());
        activity.transport_eof();
        assert_eq!(activity.get(), ProtocolWorkerPhase::CloseWritingEof);

        let worker_activity = Arc::clone(&activity);
        let (finished, finished_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            finished
                .send(worker_activity.finish_next(true))
                .expect("publish receive classification");
        });
        assert!(matches!(
            finished_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        activity.finish_close_write(true);
        assert!(finished_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("write outcome releases receive classification"));
        worker.join().expect("classification worker joins");
        assert_eq!(activity.get(), ProtocolWorkerPhase::CloseEof);
    }

    #[test]
    fn failed_close_write_cannot_turn_eof_into_success() {
        let activity = ProtocolActivity::new();
        activity.set(ProtocolWorkerPhase::Next);
        assert!(activity.claim_next_for_close());
        activity.transport_eof();
        activity.finish_close_write(false);

        assert!(!activity.finish_next(true));
        assert_eq!(activity.get(), ProtocolWorkerPhase::CloseFailed);
    }

    #[test]
    fn close_port_serializes_exactly_one_complete_frame() {
        let (writer, mut reader) = UnixStream::pair().expect("create Close transport");
        let writer = unsafe { File::from_raw_fd(writer.into_raw_fd()) };
        let cancellation = Arc::new(CancellationSignal::new().expect("create cancellation event"));
        let port = ClosePort::new(CancellableWriter {
            writer: Arc::new(Mutex::new(writer)),
            cancellation,
            deadline: None,
        });
        let deadline = Instant::now() + Duration::from_secs(1);

        port.send(deadline).expect("first Close frame is complete");
        assert_eq!(
            port.send(deadline)
                .expect_err("terminal Close port is single-use")
                .kind(),
            io::ErrorKind::BrokenPipe
        );

        let mut length = [0_u8; 4];
        reader.read_exact(&mut length).expect("read Close length");
        let mut payload = vec![0_u8; u32::from_be_bytes(length) as usize];
        reader.read_exact(&mut payload).expect("read Close payload");
        assert!(matches!(
            serde_json::from_slice::<ClientFrame>(&payload).expect("decode Close frame"),
            ClientFrame::Close
        ));
        let mut trailing = Vec::new();
        reader
            .read_to_end(&mut trailing)
            .expect("read terminal Close EOF");
        assert!(trailing.is_empty(), "no second frame was serialized");
    }

    #[test]
    fn concurrent_graceful_callers_share_the_first_transaction() {
        let (core, close_entered, close_release) = responsive_close_core();
        let (admitted, admitted_receiver) = mpsc::sync_channel(1);
        let (admit_release, admit_release_receiver) = mpsc::channel();
        let owner_core = Arc::clone(&core);
        let owner = thread::spawn(move || {
            owner_core.graceful_finish_with_hook(Duration::from_secs(1), Duration::ZERO, || {
                admitted.send(()).expect("announce graceful owner");
                admit_release_receiver
                    .recv()
                    .expect("release graceful owner");
            })
        });
        admitted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first graceful transaction is published");

        let waiter_core = Arc::clone(&core);
        let (waiter_result, waiter_result_receiver) = mpsc::channel();
        let waiter = thread::spawn(move || {
            waiter_result
                .send(waiter_core.graceful_finish(Duration::from_millis(1), Duration::ZERO))
                .expect("publish shared graceful result");
        });
        assert!(matches!(
            waiter_result_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        admit_release.send(()).expect("release graceful admission");
        close_entered
            .recv_timeout(Duration::from_secs(1))
            .expect("worker receives one Close");
        assert!(matches!(
            waiter_result_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        close_release.send(()).expect("complete semantic Close");

        assert!(owner
            .join()
            .expect("graceful owner joins")
            .unwrap()
            .success());
        assert!(waiter_result_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter receives shared result")
            .unwrap()
            .success());
        waiter.join().expect("graceful waiter joins");
    }

    #[test]
    fn retired_next_keeps_graceful_provenance_after_cleanup_completes() {
        let (core, close_entered, close_release) = responsive_close_core();
        let owner_core = Arc::clone(&core);
        let owner = thread::spawn(move || {
            owner_core.graceful_finish(Duration::from_secs(1), Duration::ZERO)
        });
        close_entered
            .recv_timeout(Duration::from_secs(1))
            .expect("worker receives terminal Close");
        close_release.send(()).expect("complete semantic Close");
        assert!(owner
            .join()
            .expect("graceful owner joins")
            .unwrap()
            .success());
        assert!(core.graceful_finish_admitted());

        let runtime = ProcessRuntime { core };
        let error = runtime
            .next_item(Duration::ZERO)
            .expect_err("closed command admission retires the pending receive");
        assert_eq!(error.code(), "plugin_receive_cancelled");
        assert!(error.detail().contains("graceful Close"), "{error}");
    }

    #[test]
    fn drop_waits_for_an_already_admitted_graceful_transaction() {
        let (core, close_entered, close_release) = responsive_close_core();
        let runtime = ProcessRuntime {
            core: Arc::clone(&core),
        };
        let (admitted, admitted_receiver) = mpsc::sync_channel(1);
        let (admit_release, admit_release_receiver) = mpsc::channel();
        let owner_core = Arc::clone(&core);
        let owner = thread::spawn(move || {
            owner_core.graceful_finish_with_hook(Duration::from_secs(1), Duration::ZERO, || {
                admitted.send(()).expect("announce graceful owner");
                admit_release_receiver
                    .recv()
                    .expect("release graceful owner");
            })
        });
        admitted_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("graceful transaction is published before Drop");

        let (dropped, dropped_receiver) = mpsc::channel();
        let dropper = thread::spawn(move || {
            drop(runtime);
            dropped.send(()).expect("announce completed Drop");
        });
        assert!(matches!(
            dropped_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        admit_release.send(()).expect("release graceful admission");
        close_entered
            .recv_timeout(Duration::from_secs(1))
            .expect("worker receives one Close");
        assert!(matches!(
            dropped_receiver.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        close_release.send(()).expect("complete semantic Close");

        assert!(owner
            .join()
            .expect("graceful owner joins")
            .unwrap()
            .success());
        dropped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("Drop observes shared completion");
        dropper.join().expect("Drop thread joins");
    }

    #[test]
    fn sentinel_masks_inherited_handlers_and_group_signals_while_parent_restores_exactly() {
        if std::env::var_os(SIGNAL_MASK_HELPER_ENV).is_some() {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = inherited_signal_handler as *const () as usize;
            // SAFETY: action has a valid empty handler mask and function pointer.
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(
                    libc::sigaction(libc::SIGUSR1, &action, std::ptr::null_mut()),
                    0
                );
                assert_eq!(
                    libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()),
                    0
                );
            }

            let mut expected_mask = current_signal_mask();
            // Leave the group-test signals deliverable to the executable while preserving a
            // distinctive pre-existing blocked bit that exact parent restoration must retain.
            expected_mask &= !signal_bit(libc::SIGUSR1);
            expected_mask &= !signal_bit(libc::SIGTERM);
            expected_mask |= signal_bit(libc::SIGUSR2);
            // SAFETY: expected_mask is complete kernel sigset storage for the calling thread.
            assert_eq!(
                unsafe {
                    libc::syscall(
                        libc::SYS_rt_sigprocmask,
                        libc::SIG_SETMASK,
                        &raw const expected_mask,
                        std::ptr::null_mut::<u64>(),
                        std::mem::size_of::<u64>(),
                    )
                },
                0
            );

            let registry = CleanupRegistry::new(1);
            let permit = registry.try_acquire().expect("reserve cleanup admission");
            let mut command = Command::new("/bin/sh");
            command.arg("-c").arg("kill -TERM 0; sleep 60");
            let atomic = spawn_atomic_supervisor_with_probes(
                command,
                permit,
                probe_waitid_pidfd_support,
                probe_live_pidfd,
                SupervisorSpawnOptions {
                    child_test_signal: Some(libc::SIGUSR1),
                    ..SupervisorSpawnOptions::STANDARD
                },
            )
            .expect("blocked inherited handler cannot interrupt raw sentinel setup");
            assert_eq!(current_signal_mask(), expected_mask);
            let mut child = ProcessPluginChild::from_atomic(atomic);
            let process_id = child.id();
            let status = fs::read_to_string(format!("/proc/{process_id}/status"))
                .expect("read sentinel signal mask");
            let blocked = status
                .lines()
                .find_map(|line| line.strip_prefix("SigBlk:\t"))
                .expect("sentinel status reports SigBlk");
            let blocked = u64::from_str_radix(blocked, 16).expect("SigBlk is hexadecimal");
            let all_blockable = !signal_bit(libc::SIGKILL) & !signal_bit(libc::SIGSTOP);
            assert_eq!(
                blocked, all_blockable,
                "raw sentinel must also block NPTL-reserved signals 32 and 33"
            );

            let executable_deadline = Instant::now() + Duration::from_secs(1);
            let executable_status = loop {
                if let Some(status) = child
                    .executable_status()
                    .expect("inspect group-signalling executable")
                {
                    break status;
                }
                assert!(
                    Instant::now() < executable_deadline,
                    "parent mask leaked into executable, suppressing kill(0, SIGTERM)"
                );
                thread::yield_now();
            };
            assert!(!executable_status.success());
            assert!(
                !wait_pidfd_until(
                    child.supervisor_pidfd(),
                    Instant::now() + Duration::from_millis(50),
                )
                .expect("inspect sentinel after executable group signal"),
                "kill(0, SIGTERM) reached an inherited sentinel disposition"
            );

            // SAFETY: the live sentinel pins this private process group. Both signals are blockable
            // and therefore remain pending without invoking the inherited handlers.
            assert_eq!(
                unsafe { libc::kill(-libc::pid_t::try_from(process_id).unwrap(), libc::SIGTERM) },
                0
            );
            assert_eq!(
                unsafe { libc::kill(-libc::pid_t::try_from(process_id).unwrap(), libc::SIGUSR1) },
                0
            );
            assert!(
                !wait_pidfd_until(
                    child.supervisor_pidfd(),
                    Instant::now() + Duration::from_millis(50),
                )
                .expect("inspect sentinel after direct group signals"),
                "blockable group signal terminated the sentinel"
            );
            let status = child
                .shutdown(Duration::ZERO)
                .expect("SIGKILL terminates and reaps masked sentinel");
            assert!(!status.success());
            drop(child);
            assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
            assert_eq!(registry.counts(), (0, 0, 0));
            return;
        }

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "process::tests::sentinel_masks_inherited_handlers_and_group_signals_while_parent_restores_exactly",
                "--nocapture",
            ])
            .env(SIGNAL_MASK_HELPER_ENV, "1")
            .output()
            .expect("run isolated signal-mask helper");
        assert!(
            output.status.success(),
            "signal-mask helper failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn mask_restore_failure_cannot_leave_the_raw_sentinel_running() {
        if std::env::var_os(RESTORE_FAILURE_PID_MARKER_ENV).is_some() {
            let registry = CleanupRegistry::new(1);
            let permit = registry.try_acquire().expect("reserve cleanup admission");
            let result = spawn_atomic_supervisor_with_probes(
                Command::new("/bin/true"),
                permit,
                probe_waitid_pidfd_support,
                probe_live_pidfd,
                SupervisorSpawnOptions {
                    force_mask_restore_failure: true,
                    ..SupervisorSpawnOptions::STANDARD
                },
            );
            panic!(
                "injected fatal mask-restoration failure unexpectedly returned: {}",
                result
                    .err()
                    .map_or_else(|| "success".to_owned(), |error| format!("error {error}"))
            );
        }

        let marker = std::env::temp_dir().join(format!(
            "chat-subscription-mask-restore-failure-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "process::tests::mask_restore_failure_cannot_leave_the_raw_sentinel_running",
                "--nocapture",
            ])
            .env(RESTORE_FAILURE_PID_MARKER_ENV, &marker)
            .output()
            .expect("run isolated mask-restoration failure helper");
        assert!(
            !output.status.success(),
            "fatal restoration failure unexpectedly returned success"
        );
        let process_id: u32 = fs::read_to_string(&marker)
            .expect("fatal helper records its atomic child identity")
            .parse()
            .expect("recorded sentinel identity is numeric");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let status = fs::read_to_string(format!("/proc/{process_id}/status"));
            if status
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                || status
                    .as_ref()
                    .is_ok_and(|status| status.lines().any(|line| line.starts_with("State:\tZ")))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "raw sentinel {process_id} survived fatal parent restoration failure: {status:?}"
            );
            thread::yield_now();
        }
        fs::remove_file(marker).expect("remove restoration-failure marker");
    }

    fn marker_command(marker: &std::path::Path) -> Command {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf ran > \"$1\"")
            .arg("sh")
            .arg(marker);
        command
    }

    fn pidfd_process_id(pidfd: &OwnedFd) -> u32 {
        let detail = fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
            .expect("read pidfd metadata");
        detail
            .lines()
            .find_map(|line| line.strip_prefix("Pid:\t"))
            .expect("pidfd metadata names a process")
            .parse()
            .expect("pidfd process id is numeric")
    }

    #[test]
    fn supervisor_closes_host_descriptors_without_exec_even_on_proc_fallback() {
        let registry = CleanupRegistry::new(1);
        let permit = registry.try_acquire().expect("reserve cleanup admission");
        let (pipe_reader, pipe_writer) = pipe_cloexec().expect("create unrelated host pipe");
        let (socket_left, socket_right) =
            socket_pair_cloexec().expect("create unrelated host socket");
        let unrelated_file = File::open("/dev/null").expect("open unrelated host file");
        let unrelated_descriptors = [
            pipe_reader.as_raw_fd(),
            pipe_writer.as_raw_fd(),
            socket_left.as_raw_fd(),
            socket_right.as_raw_fd(),
            unrelated_file.as_raw_fd(),
        ];

        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let atomic = spawn_atomic_supervisor_with_probes(
            command,
            permit,
            probe_waitid_pidfd_support,
            probe_live_pidfd,
            SupervisorSpawnOptions {
                allow_close_range: false,
                ..SupervisorSpawnOptions::STANDARD
            },
        )
        .unwrap_or_else(|error| panic!("spawn through procfs descriptor fallback: {error}"));
        let mut child = ProcessPluginChild::from_atomic(atomic);
        let supervisor_id = child.id();
        let supervisor_fds = fs::read_dir(format!("/proc/{supervisor_id}/fd"))
            .expect("inspect live supervisor descriptors")
            .collect::<Result<Vec<_>, _>>()
            .expect("enumerate live supervisor descriptors");
        assert!(
            supervisor_fds.is_empty(),
            "non-execing supervisor retained descriptors: {supervisor_fds:?}"
        );
        for descriptor in unrelated_descriptors {
            assert!(
                !PathBuf::from(format!("/proc/{supervisor_id}/fd/{descriptor}")).exists(),
                "supervisor retained unrelated host descriptor {descriptor}"
            );
        }

        drop(pipe_writer);
        let mut pipe_reader = File::from(pipe_reader);
        set_nonblocking(pipe_reader.as_raw_fd()).expect("make EOF check nonblocking");
        let mut byte = [0_u8; 1];
        let eof_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match pipe_reader.read(&mut byte) {
                Ok(0) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < eof_deadline,
                        "a supervisor copy of the writer suppressed EOF"
                    );
                    thread::yield_now();
                }
                result => panic!("unexpected unrelated-pipe read result: {result:?}"),
            }
        }
        drop(socket_right);
        let mut socket_left = File::from(socket_left);
        set_nonblocking(socket_left.as_raw_fd()).expect("make socket EOF check nonblocking");
        let socket_eof_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match socket_left.read(&mut byte) {
                Ok(0) => break,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < socket_eof_deadline,
                        "a supervisor copy of the peer socket suppressed EOF"
                    );
                    thread::yield_now();
                }
                result => panic!("unexpected unrelated-socket read result: {result:?}"),
            }
        }
        child
            .shutdown(Duration::ZERO)
            .expect("shut down descriptor regression fixture");
        drop(child);
        assert_eq!(registry.counts(), (0, 0, 0));
    }

    #[test]
    fn waitid_pidfd_is_preflighted_before_clone_or_plugin_exec() {
        let registry = CleanupRegistry::new(1);
        let marker = std::env::temp_dir().join(format!(
            "chat-subscription-waitid-preflight-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let permit = registry.try_acquire().expect("reserve cleanup admission");
        let result = spawn_atomic_supervisor_with_probes(
            marker_command(&marker),
            permit,
            |_| classify_waitid_pidfd_support(-1, io::Error::from_raw_os_error(libc::EINVAL)),
            |_| Ok(()),
            SupervisorSpawnOptions::STANDARD,
        );
        let error = result
            .err()
            .expect("missing waitid primitive refuses launch");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(error.to_string().contains("waitid(P_PIDFD)"));
        assert!(
            !marker.exists(),
            "refused plugin executable unexpectedly ran"
        );
        assert_eq!(registry.counts(), (0, 0, 0));
    }

    #[test]
    fn failed_live_waitid_probe_reaps_sentinel_before_plugin_exec() {
        let registry = CleanupRegistry::new(1);
        let marker = std::env::temp_dir().join(format!(
            "chat-subscription-waitid-live-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        let supervisor_id = Arc::new(AtomicU32::new(0));
        let captured_id = Arc::clone(&supervisor_id);
        let permit = registry.try_acquire().expect("reserve cleanup admission");
        let result = spawn_atomic_supervisor_with_probes(
            marker_command(&marker),
            permit,
            probe_waitid_pidfd_support,
            move |pidfd| {
                captured_id.store(pidfd_process_id(pidfd), Ordering::SeqCst);
                Err(unsupported_pidfd_wait())
            },
            SupervisorSpawnOptions::STANDARD,
        );
        let error = result
            .err()
            .expect("failed live waitid probe refuses plugin executable");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        let supervisor_id = supervisor_id.load(Ordering::SeqCst);
        assert_ne!(supervisor_id, 0);
        assert!(
            !PathBuf::from(format!("/proc/{supervisor_id}")).exists(),
            "failed primitive probe left the sentinel alive or zombie"
        );
        assert!(!marker.exists(), "plugin executable ran after failed probe");
        assert_eq!(registry.counts(), (0, 0, 0));
    }

    #[test]
    fn pidfd_wait_retries_eintr_before_returning_a_status() {
        let mut calls = 0_u8;
        let expected = ExitStatus::from_raw(libc::SIGKILL);
        let status = waitid_reap_pidfd_with(|| {
            calls += 1;
            if calls == 1 {
                Err(io::Error::from_raw_os_error(libc::EINTR))
            } else {
                Ok(expected)
            }
        })
        .expect("EINTR is retried instead of escaping the exact pidfd reap");
        assert_eq!(calls, 2);
        assert_eq!(status, expected);
    }

    #[test]
    fn pidfd_wait_refuses_a_false_success_without_terminal_siginfo() {
        // A seccomp filter can return errno zero without running waitid. Zero-initialized output
        // must not be interpreted as a successful exit or release tracked ownership.
        let information = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let error = validated_exit_status_from_siginfo(&information)
            .expect_err("missing terminal SIGCHLD record must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("terminal SIGCHLD"));
    }

    #[test]
    fn failed_pidfd_transfer_retains_identity_resources_without_numeric_child_calls() {
        if std::env::var_os(TRANSFER_FAILURE_HELPER_ENV).is_some() {
            let registry = CleanupRegistry::new(1);
            let supervisor_id = Arc::new(AtomicU32::new(0));
            let plugin_id = Arc::new(AtomicU32::new(0));
            let supervisor_for_hook = Arc::clone(&supervisor_id);
            let plugin_for_hook = Arc::clone(&plugin_id);
            *POST_SPAWN_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Box::new(move |supervisor, plugin| {
                    supervisor_for_hook.store(supervisor, Ordering::SeqCst);
                    plugin_for_hook.store(plugin, Ordering::SeqCst);
                }));

            let permit = registry.try_acquire().expect("reserve cleanup admission");
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            let error = spawn_atomic_supervisor_with_probes(
                command,
                permit,
                probe_waitid_pidfd_support,
                probe_live_pidfd,
                SupervisorSpawnOptions {
                    force_pidfd_transfer_failure: true,
                    ..SupervisorSpawnOptions::STANDARD
                },
            )
            .err()
            .expect("injected transfer failure refuses the plugin");
            assert!(error.to_string().contains("pidfd transfer failure"));
            let supervisor_id = supervisor_id.load(Ordering::SeqCst);
            let plugin_id = plugin_id.load(Ordering::SeqCst);
            assert_ne!(supervisor_id, 0);
            assert_ne!(plugin_id, 0);
            assert!(
                PathBuf::from(format!("/proc/{supervisor_id}")).exists(),
                "unreaped supervisor identity must stay retained"
            );
            assert!(
                PathBuf::from(format!("/proc/{plugin_id}")).exists(),
                "plugin Child ownership must stay retained without a numeric wait"
            );
            assert_eq!(
                registry.counts(),
                (1, 0, 1),
                "unrecoverable transfer retains one bounded admission without a blocked thread"
            );
            assert_eq!(
                registry
                    .try_acquire()
                    .expect_err("permanent identity retention must saturate admission")
                    .kind(),
                io::ErrorKind::WouldBlock
            );
            return;
        }

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "process::tests::failed_pidfd_transfer_retains_identity_resources_without_numeric_child_calls",
                "--nocapture",
            ])
            .env(TRANSFER_FAILURE_HELPER_ENV, "1")
            .output()
            .expect("run isolated transfer-failure helper");
        assert!(
            output.status.success(),
            "transfer-failure helper failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pidfd_transfer_retries_eintr_without_consuming_cleanup_capacity() {
        let registry = CleanupRegistry::new(1);
        let permit = registry.try_acquire().expect("reserve cleanup admission");
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let atomic = spawn_atomic_supervisor_with_probes(
            command,
            permit,
            probe_waitid_pidfd_support,
            probe_live_pidfd,
            SupervisorSpawnOptions {
                force_receive_eintr_once: true,
                ..SupervisorSpawnOptions::STANDARD
            },
        )
        .expect("queued SCM_RIGHTS transfer survives interrupted receive");
        let mut child = ProcessPluginChild::from_atomic(atomic);
        child
            .shutdown(Duration::ZERO)
            .expect("shut down interrupted-transfer fixture");
        drop(child);
        assert_eq!(registry.counts(), (0, 0, 0));
    }

    #[test]
    fn tracked_reap_waits_for_events_without_losing_a_pre_wait_notification() {
        let retry = Arc::new(ReapRetrySignal::new());
        // SAFETY: eventfd returns a fresh descriptor. It is deliberately not a pidfd so the
        // injected wait failures cannot accidentally be classified as an externally reaped task.
        let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(descriptor >= 0);
        // SAFETY: the successful eventfd result is newly owned by this test.
        let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let (attempted, attempted_receiver) = mpsc::channel();
        let (before_wait, before_wait_receiver) = mpsc::channel();
        let (release_first_wait, release_first_wait_receiver) = mpsc::channel();
        let retry_for_thread = Arc::clone(&retry);
        let reaper = thread::spawn(move || {
            let mut attempts = 0_u8;
            let mut waits = 0_u8;
            reap_pidfd_tracked_with_hook(
                &descriptor,
                &retry_for_thread,
                |_| {
                    attempts += 1;
                    attempted.send(attempts).expect("publish reap attempt");
                    if attempts < 3 {
                        Err(io::Error::from_raw_os_error(libc::EINVAL))
                    } else {
                        Ok(ExitStatus::from_raw(libc::SIGKILL))
                    }
                },
                || {
                    waits += 1;
                    before_wait.send(waits).expect("publish retry wait");
                    if waits == 1 {
                        release_first_wait_receiver
                            .recv()
                            .expect("release first retry wait");
                    }
                },
            )
        });

        assert_eq!(
            attempted_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("observe first failed reap"),
            1
        );
        assert_eq!(
            before_wait_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("observe first pre-wait window"),
            1
        );
        retry.notify_all();
        release_first_wait
            .send(())
            .expect("release pre-notified wait");
        assert_eq!(
            attempted_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("notification between snapshot and wait is not lost"),
            2
        );
        assert_eq!(
            before_wait_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("second failure reaches event wait"),
            2
        );
        assert!(
            attempted_receiver
                .recv_timeout(Duration::from_millis(30))
                .is_err(),
            "tracked reaper retried without an explicit event"
        );
        retry.notify_all();
        assert_eq!(
            attempted_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("explicit event triggers third attempt"),
            3
        );
        assert!(matches!(
            reaper.join().expect("tracked reaper joins"),
            PidfdReapOutcome::Status(status) if !status.success()
        ));
    }

    #[test]
    fn unexpected_waitid_failure_retains_tracked_reaper_and_admission_until_retry() {
        let registry = CleanupRegistry::new(1);
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut child = ProcessPluginChild::spawn_with_registry(command, Arc::clone(&registry))
            .expect("spawn retained waitid fixture");
        let process_id = child.id();
        child
            .terminate_processes()
            .expect("terminate retained waitid fixture process group");
        let plugin_status = child
            .wait_plugin_eventual()
            .expect("reap retained waitid fixture executable");
        let supervisor_pidfd = child.pidfd.take().expect("supervisor pidfd exists");
        let cleanup_permit = child
            .cleanup_permit
            .take()
            .expect("fixture owns its cleanup admission");
        child.shutdown = Some(StoredShutdown::Success(plugin_status));
        drop(child);

        let (failed, failed_receiver) = mpsc::sync_channel(1);
        let (echild, echild_receiver) = mpsc::sync_channel(1);
        let (retried, retried_receiver) = mpsc::sync_channel(1);
        let (allow_success, allow_success_receiver) = mpsc::channel();
        let (completed, completed_receiver) = mpsc::sync_channel(1);
        let mut attempts = 0_u8;
        registry
            .submit(
                Box::new(move || {
                    let status = reap_pidfd_tracked_with(
                        &supervisor_pidfd,
                        reap_retry_signal(),
                        move |pidfd| {
                            attempts += 1;
                            match attempts {
                                1 => {
                                    failed.send(()).expect("announce injected waitid EINVAL");
                                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                                }
                                2 => {
                                    echild
                                        .send(())
                                        .expect("announce injected live-zombie ECHILD");
                                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                                }
                                _ => {}
                            }
                            retried.send(()).expect("announce tracked waitid retry");
                            allow_success_receiver
                                .recv()
                                .expect("release successful pidfd reap");
                            waitid_reap_pidfd(pidfd)
                        },
                    );
                    completed
                        .send(status)
                        .expect("publish retained reap status");
                }),
                cleanup_permit,
            )
            .expect("start tracked retained reaper");
        failed_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("tracked reaper observes injected EINVAL");
        assert_eq!(registry.counts(), (1, 1, 0));
        assert!(
            PathBuf::from(format!("/proc/{process_id}")).exists(),
            "unreaped sentinel identity must remain owned during retry"
        );
        assert_eq!(
            registry
                .try_acquire()
                .expect_err("retained reap must consume the only admission")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        echild_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("first retry injects ECHILD for an unreaped sentinel");
        assert_eq!(registry.counts(), (1, 1, 0));
        assert!(PathBuf::from(format!("/proc/{process_id}")).exists());
        assert_eq!(
            registry
                .try_acquire()
                .expect_err("ECHILD while pidfd signal-zero succeeds must retain admission")
                .kind(),
            io::ErrorKind::WouldBlock
        );
        retried_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("launch attempt wakes known-capable tracked reaper");
        assert_eq!(registry.counts(), (1, 1, 0));
        allow_success.send(()).expect("allow exact pidfd reap");
        let outcome = completed_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("tracked pidfd reap completes");
        assert!(matches!(
            outcome,
            PidfdReapOutcome::Status(status) if !status.success()
        ));
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if registry.counts() == (0, 0, 0) {
                break;
            }
            assert!(Instant::now() < deadline, "tracked reaper did not join");
            thread::yield_now();
        }
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn external_supervisor_reaper_cannot_redirect_cleanup_to_an_unrelated_pid() {
        let registry = CleanupRegistry::new(1);
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut child = ProcessPluginChild::spawn_with_registry(command, Arc::clone(&registry))
            .expect("spawn external-reaper fixture");
        let process_id = child.id();
        child
            .terminate_processes()
            .expect("terminate external-reaper fixture group");
        let plugin_status = child
            .wait_plugin_eventual()
            .expect("reap external-reaper fixture executable");
        let supervisor_pidfd = child.pidfd.take().expect("supervisor pidfd exists");

        let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: this test deliberately acts as a hostile process-wide numeric reaper for the
        // exact direct supervisor child. Production cleanup never uses this numeric identity.
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    libc::id_t::try_from(process_id).expect("supervisor pid fits id_t"),
                    information.as_mut_ptr(),
                    libc::WEXITED,
                )
            },
            0
        );
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());

        let mut unrelated = Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .expect("spawn potential PID-reuse target");
        let unrelated_pid = unrelated.id();
        let outcome = reap_supervisor_owned(&supervisor_pidfd)
            .expect("pidfd ECHILD proves the original sentinel was externally reaped");
        assert_eq!(outcome, PidfdReapOutcome::StatusConsumed);
        assert!(
            unrelated
                .try_wait()
                .expect("inspect unrelated potential reuse target")
                .is_none(),
            "pidfd cleanup must never wait on or signal an unrelated numeric PID"
        );
        unrelated.kill().expect("kill unrelated fixture");
        unrelated.wait().expect("reap unrelated fixture");
        assert!(!PathBuf::from(format!("/proc/{unrelated_pid}")).exists());

        child.shutdown = Some(StoredShutdown::Success(plugin_status));
        drop(child);
        assert_eq!(registry.counts(), (0, 0, 0));
    }

    #[test]
    fn stuck_cleanup_saturates_admission_without_growing_threads_or_processes() {
        let registry = CleanupRegistry::new(1);
        let permit = registry
            .try_acquire()
            .expect("reserve only cleanup admission");
        let (entered, entered_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::channel();
        registry
            .submit(
                Box::new(move || {
                    entered.send(()).expect("announce stuck cleanup");
                    let _ = release_receiver.recv();
                }),
                permit,
            )
            .expect("start tracked cleanup");
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup reaches deterministic stall");
        assert_eq!(registry.counts(), (1, 1, 0));

        let marker = std::env::temp_dir().join(format!(
            "chat-subscription-cleanup-saturation-{}",
            std::process::id()
        ));
        let _ = fs::remove_file(&marker);
        for _ in 0..64 {
            let error = ProcessPluginChild::spawn_with_registry(
                marker_command(&marker),
                Arc::clone(&registry),
            )
            .expect_err("saturated cleanup admission must refuse before launch");
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        }
        assert_eq!(registry.counts(), (1, 1, 0));
        assert!(!marker.exists(), "saturated launches unexpectedly executed");

        release.send(()).expect("release tracked cleanup");
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if registry.counts() == (0, 0, 0) {
                break;
            }
            assert!(Instant::now() < deadline, "tracked cleanup did not join");
            thread::yield_now();
        }
    }

    #[test]
    fn cleanup_thread_spawn_failure_is_nonpanicking_bounded_and_recoverable() {
        let registry = CleanupRegistry::new(1);
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut child = ProcessPluginChild::spawn_with_registry(command, Arc::clone(&registry))
            .expect("spawn cleanup failure fixture");
        let process_id = child.id();
        // SAFETY: eventfd returns a fresh inert descriptor used to force the bounded reap timeout.
        let inert_descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(inert_descriptor >= 0);
        // SAFETY: the successful eventfd result is newly owned by this test.
        let inert_pidfd = unsafe { OwnedFd::from_raw_fd(inert_descriptor) };
        let real_pidfd = child
            .pidfd
            .replace(inert_pidfd)
            .expect("real supervisor pidfd exists");
        let error = child
            .shutdown_with_reap_timeout(Duration::ZERO, Duration::from_millis(20))
            .expect_err("inert descriptor forces a terminal cleanup timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        child.pidfd = Some(real_pidfd);
        registry.force_spawn_failure.store(true, Ordering::SeqCst);
        drop(child);
        assert_eq!(
            registry.counts(),
            (1, 0, 1),
            "failed thread launch must retain exactly one task and admission"
        );
        assert!(
            ProcessPluginChild::spawn_with_registry(
                Command::new("/bin/true"),
                Arc::clone(&registry)
            )
            .expect_err("retained cleanup saturates the one-slot registry")
            .kind()
                == io::ErrorKind::WouldBlock
        );
        registry.force_spawn_failure.store(false, Ordering::SeqCst);
        registry.retry_retained();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if registry.counts() == (0, 0, 0) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "retried cleanup was not reaped and joined"
            );
            thread::yield_now();
        }
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn auto_reap_dispositions_are_refused_before_supported_pidfd_spawn() {
        if let Some(mode) = std::env::var_os(AUTOREAP_HELPER_ENV) {
            let mut probe = ProcessPluginChild::spawn(Command::new("/bin/true"))
                .expect("isolated regression requires real supported atomic pidfd spawn");
            probe
                .shutdown(Duration::from_secs(1))
                .expect("supported atomic pidfd probe is reaped");
            match mode.to_str().expect("ASCII helper mode") {
                "ignore" => {
                    // SAFETY: this runs in a dedicated subprocess and SIG_IGN is a valid SIGCHLD
                    // disposition. The helper exits immediately after the assertion.
                    let previous = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
                    assert_ne!(previous, libc::SIG_ERR);
                }
                "no-cldwait" => {
                    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
                    action.sa_sigaction = libc::SIG_DFL;
                    action.sa_flags = libc::SA_NOCLDWAIT;
                    // SAFETY: action contains a valid empty mask, default handler, and supported
                    // flag; the isolated helper intentionally changes only its own disposition.
                    unsafe {
                        libc::sigemptyset(&mut action.sa_mask);
                        assert_eq!(
                            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
                            0
                        );
                    }
                }
                other => panic!("unknown auto-reap helper mode {other}"),
            }
            let marker = PathBuf::from(
                std::env::var_os(AUTOREAP_MARKER_ENV).expect("helper marker is supplied"),
            );
            let marker_path = CString::new(marker.to_string_lossy().as_bytes())
                .expect("temporary marker path has no NUL");
            let mut command = Command::new("/bin/sleep");
            command.arg("60");
            // SAFETY: the closure calls only async-signal-safe libc open/close operations. It
            // creates a marker before exec, making any accidental spawn observable even if the
            // returned supervisor is dropped immediately.
            unsafe {
                command.pre_exec(move || {
                    let descriptor = libc::open(
                        marker_path.as_ptr(),
                        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                        0o600,
                    );
                    if descriptor < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::close(descriptor) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let error = ProcessPluginChild::spawn(command)
                .expect_err("auto-reap disposition must refuse before command spawn");
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("default SIGCHLD disposition"));
            thread::sleep(Duration::from_millis(20));
            assert!(!marker.exists(), "refused command unexpectedly ran");
            return;
        }

        for mode in ["ignore", "no-cldwait"] {
            let marker = std::env::temp_dir().join(format!(
                "chat-subscription-auto-reap-{mode}-{}",
                std::process::id()
            ));
            let _ = fs::remove_file(&marker);
            let output = Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "--exact",
                    "process::tests::auto_reap_dispositions_are_refused_before_supported_pidfd_spawn",
                    "--nocapture",
                ])
                .env(AUTOREAP_HELPER_ENV, mode)
                .env(AUTOREAP_MARKER_ENV, &marker)
                .output()
                .expect("run isolated auto-reap helper");
            assert!(
                output.status.success(),
                "auto-reap helper {mode} failed:\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!marker.exists());
        }
    }

    #[test]
    fn atomic_supervisor_survives_concurrent_disposition_and_reaper_races() {
        if let Some(mode) = std::env::var_os(SPAWN_RACE_HELPER_ENV) {
            let mode = mode.to_str().expect("ASCII helper mode").to_owned();
            let mut unrelated = Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .expect("spawn unrelated process");
            let unrelated_pid = unrelated.id();
            let supervisor_id = Arc::new(AtomicU32::new(0));
            let plugin_id = Arc::new(AtomicU32::new(0));
            let supervisor_for_hook = Arc::clone(&supervisor_id);
            let plugin_for_hook = Arc::clone(&plugin_id);
            let (race_start, race_receiver) = mpsc::sync_channel(1);
            let (race_done, race_done_receiver) = mpsc::sync_channel(1);
            let race_mode = mode.clone();
            let racer = thread::spawn(move || {
                let plugin_pid = race_receiver.recv().expect("receive plugin pid");
                match race_mode.as_str() {
                    "disposition" => {
                        // SAFETY: the helper is an isolated process. The main thread restores the
                        // default disposition before terminating its unrelated live child.
                        let previous = unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
                        assert_ne!(previous, libc::SIG_ERR);
                        let deadline = Instant::now() + Duration::from_secs(1);
                        loop {
                            // SAFETY: after SIG_IGN, waitpid cannot reap this child itself; ECHILD
                            // proves the kernel's auto-reap policy consumed the later exit.
                            let result = unsafe {
                                libc::waitpid(
                                    plugin_pid as libc::pid_t,
                                    std::ptr::null_mut(),
                                    libc::WNOHANG,
                                )
                            };
                            if result < 0
                                && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
                            {
                                break;
                            }
                            assert!(Instant::now() < deadline, "auto-reap race did not complete");
                            thread::yield_now();
                        }
                    }
                    "reaper" => {
                        let mut status = 0;
                        // SAFETY: the exact plugin pid is this process's direct child and status is
                        // writable. This deliberately races the adapter's pidfd acquisition.
                        assert_eq!(
                            unsafe { libc::waitpid(plugin_pid as libc::pid_t, &mut status, 0) },
                            plugin_pid as libc::pid_t
                        );
                    }
                    other => panic!("unknown spawn-race helper mode {other}"),
                }
                race_done.send(()).expect("announce completed race");
            });
            *POST_SPAWN_HOOK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(Box::new(move |supervisor, plugin| {
                    supervisor_for_hook.store(supervisor, Ordering::SeqCst);
                    plugin_for_hook.store(plugin, Ordering::SeqCst);
                    race_start.send(plugin).expect("start adversarial race");
                    race_done_receiver
                        .recv_timeout(Duration::from_secs(1))
                        .expect("adversarial race completes before parent receives pidfd");
                }));

            let plugin_command = if mode == "disposition" {
                let mut command = Command::new("/bin/sleep");
                command.arg("0.05");
                command
            } else {
                Command::new("/bin/true")
            };
            let mut child = ProcessPluginChild::spawn(plugin_command)
                .expect("self-opened pidfd survives the adversarial post-spawn reap");
            assert_eq!(child.id(), supervisor_id.load(Ordering::SeqCst));
            assert_eq!(child.executable_id(), plugin_id.load(Ordering::SeqCst));
            let error = child
                .shutdown(Duration::ZERO)
                .expect_err("stolen exit status is reported instead of guessed");
            assert!(error.to_string().contains("No child processes"), "{error}");
            // SAFETY: restore the helper's original default disposition before unrelated cleanup.
            assert_ne!(
                unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) },
                libc::SIG_ERR
            );
            racer.join().expect("race thread joins");
            let supervisor_id = supervisor_id.load(Ordering::SeqCst);
            let plugin_id = plugin_id.load(Ordering::SeqCst);
            assert_ne!(supervisor_id, 0);
            assert_ne!(plugin_id, 0);
            drop(child);
            assert!(!PathBuf::from(format!("/proc/{supervisor_id}")).exists());
            assert!(!PathBuf::from(format!("/proc/{plugin_id}")).exists());
            assert!(
                unrelated
                    .try_wait()
                    .expect("inspect unrelated child")
                    .is_none(),
                "failed spawn cleanup must not signal an unrelated process"
            );
            unrelated.kill().expect("kill unrelated fixture");
            unrelated.wait().expect("reap unrelated fixture");
            assert!(!PathBuf::from(format!("/proc/{unrelated_pid}")).exists());
            return;
        }

        for mode in ["disposition", "reaper"] {
            let output = Command::new(std::env::current_exe().expect("current test executable"))
                .args([
                    "--exact",
                    "process::tests::atomic_supervisor_survives_concurrent_disposition_and_reaper_races",
                    "--nocapture",
                ])
                .env(SPAWN_RACE_HELPER_ENV, mode)
                .output()
                .expect("run isolated spawn-race helper");
            assert!(
                output.status.success(),
                "spawn-race helper {mode} failed:\nstdout={}\nstderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[test]
    fn shutdown_failure_uses_one_terminal_reap_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut child = ProcessPluginChild::spawn(command).expect("spawn deadline fixture");
        let process_id = child.id();
        // SAFETY: eventfd returns a new descriptor or a negative error indicator.
        let inert_descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        assert!(inert_descriptor >= 0, "create inert poll descriptor");
        // SAFETY: the successful eventfd result is a fresh descriptor owned by this test.
        let inert_pidfd = unsafe { OwnedFd::from_raw_fd(inert_descriptor) };
        let real_pidfd = child
            .pidfd
            .replace(inert_pidfd)
            .expect("real supervisor pidfd is present");
        let reap_timeout = Duration::from_millis(30);
        let started = Instant::now();
        let first = child
            .shutdown_with_reap_timeout(Duration::ZERO, reap_timeout)
            .expect_err("inert pidfd reaches the one reap deadline");
        assert_eq!(first.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        let second = child
            .shutdown_with_reap_timeout(Duration::ZERO, reap_timeout)
            .expect_err("terminal shutdown result is reused without a second wait");
        assert_eq!(second.kind(), io::ErrorKind::TimedOut);
        assert_eq!(second.to_string(), first.to_string());
        child.pidfd = Some(real_pidfd);
        let drop_started = Instant::now();
        drop(child);
        assert!(
            drop_started.elapsed() < Duration::from_millis(500),
            "Drop must transfer timed-out ownership without a second wait"
        );
        let reap_deadline = Instant::now() + Duration::from_secs(1);
        while PathBuf::from(format!("/proc/{process_id}")).exists()
            && Instant::now() < reap_deadline
        {
            thread::yield_now();
        }
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn shared_deadline_bounds_worker_join_and_concurrent_finish() {
        let (release_worker, worker_release) = mpsc::channel();
        let (worker_exited, worker_exit_receiver) = mpsc::channel();
        let (core, process_id) = sleeping_core(move |_commands| {
            let _ = worker_release.recv();
            worker_exited.send(()).expect("announce worker exit");
        });
        let owner_entered = Arc::new(Barrier::new(2));
        let owner_release = Arc::new(Barrier::new(2));
        let owner_core = Arc::clone(&core);
        let owner_entered_thread = Arc::clone(&owner_entered);
        let owner_release_thread = Arc::clone(&owner_release);
        let (owner_result, owner_result_receiver) = mpsc::channel();
        let owner = thread::spawn(move || {
            let result = owner_core.finish_with_reap_timeout_and_hook(
                Duration::ZERO,
                Duration::from_millis(40),
                || {
                    owner_entered_thread.wait();
                    owner_release_thread.wait();
                },
            );
            owner_result.send(result).expect("publish owner result");
        });
        owner_entered.wait();
        let waiter_core = Arc::clone(&core);
        let (waiter_started, waiter_started_receiver) = mpsc::channel();
        let (waiter_result, waiter_result_receiver) = mpsc::channel();
        let waiter = thread::spawn(move || {
            waiter_started.send(()).expect("announce concurrent finish");
            waiter_result
                .send(waiter_core.finish_with_reap_timeout_and_hook(
                    Duration::from_secs(60),
                    Duration::from_secs(60),
                    || {},
                ))
                .expect("publish waiter result");
        });
        waiter_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("concurrent finisher starts");
        owner_release.wait();

        for result in [
            owner_result_receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("owner obeys shared deadline"),
            waiter_result_receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("concurrent waiter obeys owner's deadline"),
        ] {
            assert_eq!(
                result
                    .expect_err("blocked worker forces terminal timeout")
                    .kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert_eq!(
            core.finish(Duration::ZERO)
                .expect_err("terminal timeout is cached")
                .kind(),
            io::ErrorKind::TimedOut
        );
        release_worker
            .send(())
            .expect("release cleanup-owned worker");
        worker_exit_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cleanup-owned worker exits after release");
        owner.join().expect("owner joins");
        waiter.join().expect("waiter joins");
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn timed_out_cleanup_retains_child_and_worker_until_eventual_reap_and_join() {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut child = ProcessPluginChild::spawn(command).expect("spawn delayed cleanup fixture");
        let process_id = child.id();
        let (cleanup_entered, cleanup_entered_receiver) = mpsc::sync_channel(1);
        let (cleanup_release, cleanup_release_receiver) = mpsc::channel();
        child.cleanup_barrier = Some(CleanupBarrier {
            entered: cleanup_entered,
            release: cleanup_release_receiver,
        });
        let (commands, command_receiver) = mpsc::channel();
        let (worker_done_sender, worker_done_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            assert!(command_receiver.recv().is_err());
            let _ = worker_done_sender.send(());
        });
        let cancellation = Arc::new(CancellationSignal::new().expect("create cancellation event"));
        let (close_port, activity) = inert_protocol_controls(&cancellation);
        let core = Arc::new(ProcessCore::new(
            child,
            worker,
            worker_done_receiver,
            commands,
            cancellation,
            close_port,
            activity,
        ));

        let error = core
            .finish_with_reap_timeout_and_hook(Duration::ZERO, Duration::from_millis(30), || {})
            .expect_err("delayed cleanup exceeds caller deadline");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        cleanup_entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("owned cleanup reaches deterministic reap barrier");
        assert!(
            PathBuf::from(format!("/proc/{process_id}")).exists(),
            "supervisor remains owned until the cleanup worker is released"
        );
        cleanup_release
            .send(())
            .expect("release eventual child reap and worker join");

        let mut state = ProcessCore::lock(&core.shutdown.state);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !matches!(&*state, ShutdownState::Done { .. }) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "eventual cleanup did not finish");
            let waited = core.shutdown.complete.wait_timeout(state, remaining);
            state = waited.unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
        drop(state);
        assert_eq!(
            core.finish(Duration::ZERO)
                .expect_err("original terminal timeout remains cached")
                .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn dispatch_and_shutdown_are_atomic_before_send() {
        let (sender, receiver) = mpsc::channel();
        let port = Arc::new(CommandPort::new(sender));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let dispatch_port = Arc::clone(&port);
        let dispatch_entered = Arc::clone(&entered);
        let dispatch_release = Arc::clone(&release);
        let (response, _response_receiver) = mpsc::channel();
        let dispatch = thread::spawn(move || {
            dispatch_port.dispatch_with_hook(WorkerCommand::CloseBackend(response), || {
                dispatch_entered.wait();
                dispatch_release.wait();
            })
        });
        entered.wait();
        let stop_port = Arc::clone(&port);
        let (stop_started, stop_started_receiver) = mpsc::channel();
        let (stopped, stopped_receiver) = mpsc::channel();
        let stop = thread::spawn(move || {
            stop_port.stop_with_hook(|| {
                stop_started.send(()).expect("announce stop attempt");
            });
            stopped.send(()).expect("announce completed stop");
        });
        stop_started_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("stop thread starts");
        release.wait();
        assert!(dispatch.join().expect("dispatch thread joins").is_ok());
        stopped_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("shutdown completes after dispatch");
        stop.join().expect("stop thread joins");
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            Ok(WorkerCommand::CloseBackend(_))
        ));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn cancellation_after_response_does_not_wait_for_caller_scope() {
        let (core, process_id) = sleeping_core(|commands| {
            let WorkerCommand::Next(response) = commands.recv().expect("receive Next") else {
                panic!("expected Next command");
            };
            response
                .send(Ok(Some(SubscriptionItem::Heartbeat(
                    chat_subscription::Heartbeat::new(
                        chat_subscription::EventSequence::new(1).expect("nonzero sequence"),
                    ),
                ))))
                .expect("send item response");
            assert!(
                commands.recv().is_err(),
                "shutdown disconnects command port"
            );
        });
        let runtime = ProcessRuntime {
            core: Arc::clone(&core),
        };
        let caller_release = Arc::new(Barrier::new(2));
        let caller_barrier = Arc::clone(&caller_release);
        let (answered, answer_receiver) = mpsc::channel();
        let caller = thread::spawn(move || {
            let result = runtime.next_item(Duration::ZERO);
            answered.send(result).expect("publish Next result");
            caller_barrier.wait();
        });
        assert!(matches!(
            answer_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("Next response arrives"),
            Ok(Some(SubscriptionItem::Heartbeat(_)))
        ));
        let cancel_core = Arc::clone(&core);
        let (cancelled, cancelled_receiver) = mpsc::channel();
        let cancel = thread::spawn(move || {
            cancelled
                .send(cancel_core.finish(Duration::ZERO))
                .expect("publish cancellation result");
        });
        let status = cancelled_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("cancellation must not wait for paused caller")
            .expect("cancellation succeeds");
        assert!(!status.success());
        caller_release.wait();
        caller.join().expect("caller joins");
        cancel.join().expect("cancel thread joins");
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn command_send_failure_finishes_and_reaps_process() {
        let (disconnected, disconnected_receiver) = mpsc::channel();
        let (core, process_id) = sleeping_core(move |commands| {
            drop(commands);
            disconnected
                .send(())
                .expect("announce disconnected command receiver");
        });
        disconnected_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("command receiver is disconnected before dispatch");
        let runtime = ProcessRuntime {
            core: Arc::clone(&core),
        };
        let error = runtime
            .next_item(Duration::ZERO)
            .expect_err("disconnected command channel fails");
        assert_eq!(error.code(), "plugin_worker_stopped");
        assert!(!core
            .finish(Duration::ZERO)
            .expect("stored cleanup")
            .success());
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }

    #[test]
    fn response_disconnect_finishes_and_reaps_process() {
        let (core, process_id) = sleeping_core(|commands| {
            let WorkerCommand::Next(response) = commands.recv().expect("receive Next") else {
                panic!("expected Next command");
            };
            drop(response);
        });
        let runtime = ProcessRuntime {
            core: Arc::clone(&core),
        };
        let error = runtime
            .next_item(Duration::ZERO)
            .expect_err("missing worker response fails");
        assert_eq!(error.code(), "plugin_worker_stopped");
        assert!(!core
            .finish(Duration::ZERO)
            .expect("stored cleanup")
            .success());
        assert!(!PathBuf::from(format!("/proc/{process_id}")).exists());
    }
}
