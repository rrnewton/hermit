//! Post-clone ownership adapter for the shared Detcore Unix guard interface.
use std::fmt;
use std::io;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::thread::{self};
use std::time::Duration;
use std::time::Instant;

use detcore::network_runtime::guard::NetworkGuardBackendFailure;
use detcore::network_runtime::guard::NetworkGuardControl;
use detcore::network_runtime::guard::NetworkGuardControllerAbort;
use detcore::network_runtime::guard::NetworkGuardDenial;
use detcore::network_runtime::guard::NetworkGuardEvidence;
use detcore::network_runtime::guard::NetworkGuardOutcome;
use detcore::network_runtime::guard::NetworkGuardPublication;
use detcore::network_runtime::guard::NetworkGuardTerminal;
use detcore::network_runtime::guard::NetworkGuardTerminalDisposition;

use crate::unix_guard::ControllerGuard;
use crate::unix_guard::GuardEvidence;
use crate::unix_guard::GuardOutcome;

const UG_MONITOR_STATUS: u64 = 4;

fn evidence(value: GuardEvidence) -> NetworkGuardEvidence {
    NetworkGuardEvidence {
        secondary_monitor_failures: value.secondary_monitor_failures,
        secondary_guard_faults: value.secondary_guard_faults,
        denial: NetworkGuardDenial {
            phase: value.denial.phase,
            incarnation: value.denial.incarnation,
            task_start: value.denial.task_start,
            pid_tgid: value.denial.pid_tgid,
            source_generation: value.denial.source_generation,
            peer_generation: value.denial.peer_generation,
            injection: value.denial.injection,
            hook: value.denial.hook,
            reason: value.denial.reason,
        },
    }
}
fn observation(value: GuardOutcome) -> NetworkGuardOutcome {
    match value {
        GuardOutcome::Pending => NetworkGuardOutcome::Pending,
        GuardOutcome::Running => NetworkGuardOutcome::Running,
        GuardOutcome::Policy(value) => NetworkGuardOutcome::Policy(evidence(value)),
        GuardOutcome::Internal(value) => NetworkGuardOutcome::Internal(evidence(value)),
    }
}
fn internal_failure() -> NetworkGuardOutcome {
    NetworkGuardOutcome::Internal(NetworkGuardEvidence {
        secondary_monitor_failures: UG_MONITOR_STATUS,
        ..NetworkGuardEvidence::default()
    })
}
fn terminal(value: NetworkGuardOutcome) -> Option<NetworkGuardTerminal> {
    match value {
        NetworkGuardOutcome::Policy(value) => Some(NetworkGuardTerminal::Policy(value)),
        NetworkGuardOutcome::Internal(value) => Some(NetworkGuardTerminal::Internal(value)),
        NetworkGuardOutcome::Pending | NetworkGuardOutcome::Running => None,
    }
}

/// Keep the first typed primary; later failure bits are secondary evidence.
fn merge(previous: NetworkGuardOutcome, next: NetworkGuardOutcome) -> NetworkGuardOutcome {
    let extra = match next {
        NetworkGuardOutcome::Policy(value) | NetworkGuardOutcome::Internal(value) => value,
        _ => NetworkGuardEvidence::default(),
    };
    match previous {
        NetworkGuardOutcome::Policy(mut value) => {
            value.secondary_monitor_failures |= extra.secondary_monitor_failures;
            value.secondary_guard_faults |= extra.secondary_guard_faults;
            NetworkGuardOutcome::Policy(value)
        }
        NetworkGuardOutcome::Internal(mut value) => {
            value.secondary_monitor_failures |= extra.secondary_monitor_failures;
            value.secondary_guard_faults |= extra.secondary_guard_faults;
            NetworkGuardOutcome::Internal(value)
        }
        _ => next,
    }
}

/// A retained local monitor/start/stop error; it never replaces a typed primary.
#[derive(Debug, Clone)]
pub struct GuardControlFailure {
    /// Operation which failed.
    pub operation: &'static str,
    /// Original I/O error category.
    pub kind: io::ErrorKind,
    /// Original OS errno, if present.
    pub errno: Option<i32>,
    /// Original diagnostic text.
    pub message: String,
}
impl GuardControlFailure {
    fn capture(operation: &'static str, error: &io::Error) -> Self {
        Self {
            operation,
            kind: error.kind(),
            errno: error.raw_os_error(),
            message: error.to_string(),
        }
    }
}

// This seam substitutes only the native monitor in isolated controls. Production
// construction below always moves the actual post-clone ControllerGuard here.
trait GuardMonitor: Send {
    unsafe fn register(&mut self, pidfd: BorrowedFd<'_>, deadline: Instant) -> io::Result<()>;
    fn observe(&mut self) -> GuardOutcome;
    fn snapshot(&mut self) -> GuardOutcome;
}
impl GuardMonitor for ControllerGuard {
    unsafe fn register(&mut self, pidfd: BorrowedFd<'_>, deadline: Instant) -> io::Result<()> {
        unsafe { self.register_stopped_initial(pidfd, deadline) }
    }
    fn observe(&mut self) -> GuardOutcome {
        self.observe_guard()
    }
    fn snapshot(&mut self) -> GuardOutcome {
        self.snapshot_guard()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialState {
    Unregistered,
    Submitted,
    Registered,
    Failed,
}
struct Lifecycle {
    initial: InitialState,
    backend_completed: bool,
    observer_attempted: bool,
    spawn_in_flight: bool,
    observer_started: bool,
    abort: Option<NetworkGuardControllerAbort>,
    publication: Option<NetworkGuardPublication>,
    observer: Option<JoinHandle<()>>,
    failures: Vec<GuardControlFailure>,
}
struct Shared {
    startup_deadline: Instant,
    guard: Mutex<Box<dyn GuardMonitor>>,
    cached: Mutex<NetworkGuardOutcome>,
    lifecycle: Mutex<Lifecycle>,
    stop: AtomicBool,
}
impl Shared {
    fn cached(&self) -> NetworkGuardOutcome {
        match self.cached.lock() {
            Ok(cached) => *cached,
            Err(poisoned) => {
                let mut cached = poisoned.into_inner();
                *cached = merge(*cached, internal_failure());
                *cached
            }
        }
    }
    fn publish(&self, next: NetworkGuardOutcome) -> NetworkGuardOutcome {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *cached = merge(*cached, next);
        *cached
    }
    fn record(&self, operation: &'static str, error: &io::Error) {
        let mut state = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state
            .failures
            .iter()
            .any(|failure| failure.operation == operation)
        {
            state
                .failures
                .push(GuardControlFailure::capture(operation, error));
        }
    }
    fn read_guard(&self, wait: bool) -> NetworkGuardOutcome {
        match self.guard.lock() {
            Ok(mut guard) => observation(if wait {
                guard.observe()
            } else {
                guard.snapshot()
            }),
            Err(_) => internal_failure(),
        }
    }
    fn report_if_terminal(&self, outcome: NetworkGuardOutcome) -> bool {
        let Some(outcome) = terminal(outcome) else {
            return false;
        };
        // Release every adapter lock before selecting the shared first primary.
        // The token is non-Clone; a returned disposition means backend outcome
        // already owns primary, so this observer can finish with typed evidence.
        let abort = self
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .abort
            .take();
        if let Some(abort) = abort {
            match abort.report_terminal(outcome) {
                NetworkGuardTerminalDisposition::RetainedAfterPublication => return true,
            }
        }
        // A pre-observer registration failure has no token. Its typed snapshot
        // remains visible to the startup/owner caller, never a success receipt.
        true
    }
    fn observe_loop(&self) {
        loop {
            let stopping = self.stop.load(Ordering::Acquire);
            let outcome = self.publish(self.read_guard(!stopping));
            if self.report_if_terminal(outcome) || stopping {
                return;
            }
        }
    }
}
struct Control {
    shared: Arc<Shared>,
}
impl fmt::Debug for Control {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnixGuardControl")
            .field("observation", &self.shared.cached())
            .finish_non_exhaustive()
    }
}
impl NetworkGuardControl for Control {
    unsafe fn register_stopped_initial(
        &self,
        pidfd: BorrowedFd<'_>,
        deadline: Instant,
    ) -> io::Result<()> {
        {
            let mut state = self
                .shared
                .lifecycle
                .lock()
                .map_err(|_| io::Error::other("guard registration ownership lock poisoned"))?;
            if self.shared.stop.load(Ordering::Acquire)
                || state.backend_completed
                || state.initial != InitialState::Unregistered
            {
                return Err(io::Error::other(
                    "guard initial registration already submitted or closed",
                ));
            }
            state.initial = InitialState::Submitted;
        }
        let deadline = deadline.min(self.shared.startup_deadline);
        // The submitted state remains durable through the post-registration
        // snapshot: stop cannot pass this operation while it owns the monitor.
        let result = match self.shared.guard.lock() {
            Ok(mut guard) => unsafe { guard.register(pidfd, deadline) },
            Err(_) => Err(io::Error::other("guard registration owner poisoned")),
        };
        self.shared.publish(self.shared.read_guard(false));
        if let Err(error) = &result {
            self.shared.record("register_stopped_initial", error);
            self.shared.publish(internal_failure());
        }
        self.shared
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .initial = if result.is_ok() {
            InitialState::Registered
        } else {
            InitialState::Failed
        };
        result
    }
    fn observation(&self) -> NetworkGuardOutcome {
        self.shared.cached()
    }
    fn start_observer(&self, abort: NetworkGuardControllerAbort) -> io::Result<()> {
        {
            let mut state = self
                .shared
                .lifecycle
                .lock()
                .map_err(|_| io::Error::other("guard observer ownership lock poisoned"))?;
            if self.shared.stop.load(Ordering::Acquire)
                || state.backend_completed
                || state.initial != InitialState::Registered
                || state.observer_attempted
            {
                return Err(io::Error::other(
                    "guard observer requires one completed initial registration",
                ));
            }
            state.observer_attempted = true;
            state.spawn_in_flight = true;
            state.publication = Some(abort.publication());
            state.abort = Some(abort);
        }
        // No lifecycle lock crosses native spawn. stop_and_join must wait for
        // this committed in-flight state even before a JoinHandle is available.
        let shared = self.shared.clone();
        let spawned = thread::Builder::new()
            .name("hermit-unix-guard".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shared.observe_loop()
                }));
                if result.is_err() {
                    shared.record(
                        "observer_panic",
                        &io::Error::other("guard observer panicked"),
                    );
                    let outcome = shared.publish(internal_failure());
                    shared.report_if_terminal(outcome);
                }
            });
        let mut state = self
            .shared
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.spawn_in_flight = false;
        match spawned {
            Ok(thread) => {
                state.observer_started = true;
                state.observer = Some(thread);
                Ok(())
            }
            Err(error) => {
                state
                    .failures
                    .push(GuardControlFailure::capture("start_observer", &error));
                drop(state);
                self.shared.publish(self.shared.read_guard(false));
                self.shared.publish(internal_failure());
                Err(error)
            }
        }
    }
}

/// Actual post-clone guard/observer owner, retained outside the backend future.
/// Dropping it is neither an observer join nor a policy cleanup certificate.
#[must_use = "retain and stop/join the actual observer before publishing child success"]
pub struct GuardControllerOwner {
    shared: Arc<Shared>,
}
/// Backend outcome retained outside its future; success remains provisional.
#[must_use = "a successful backend return still requires guard and parent terminal proof"]
pub enum GuardedBackendResult<T, E> {
    /// No success publication has occurred; pending guard denial remains primary.
    SuccessPending(T),
    /// Actual backend failure and any guard primary already selected before it.
    Failure {
        /// Original backend error, retained even when a guard was first.
        error: E,
        /// Earlier typed guard result; later guard evidence remains on the owner.
        prior_guard: Option<NetworkGuardTerminal>,
    },
}
/// Joined observer and coherent final observation; not keeper/policy cleanup.
#[derive(Clone, Copy, Debug)]
pub struct GuardObserverStopped {
    /// First typed outcome plus secondary monitor flags.
    pub observation: NetworkGuardOutcome,
    /// Whether a native observer was actually spawned.
    pub observer_was_started: bool,
}
impl GuardControllerOwner {
    fn from_monitor(
        guard: Box<dyn GuardMonitor>,
        startup_deadline: Instant,
    ) -> (Self, Arc<dyn NetworkGuardControl>) {
        let shared = Arc::new(Shared {
            startup_deadline,
            guard: Mutex::new(guard),
            cached: Mutex::new(NetworkGuardOutcome::Pending),
            lifecycle: Mutex::new(Lifecycle {
                initial: InitialState::Unregistered,
                backend_completed: false,
                observer_attempted: false,
                spawn_in_flight: false,
                observer_started: false,
                abort: None,
                publication: None,
                observer: None,
                failures: Vec::new(),
            }),
            stop: AtomicBool::new(false),
        });
        (
            Self {
                shared: shared.clone(),
            },
            Arc::new(Control { shared }),
        )
    }
    /// # Safety
    /// Call only after the actual Container fork split. No Arc/Mutex/thread
    /// made here may be copied through another Container clone.
    pub unsafe fn from_controller(
        guard: ControllerGuard,
        startup_deadline: Instant,
    ) -> (Self, Arc<dyn NetworkGuardControl>) {
        Self::from_monitor(Box::new(guard), startup_deadline)
    }
    /// Original absolute Container startup deadline, never a renewed duration.
    pub fn startup_deadline(&self) -> Instant {
        self.shared.startup_deadline
    }
    /// Retain an actual error or nonzero backend status while its original
    /// value remains owned by the caller. The typed token cannot be made from
    /// a successful exit. This owner exposes no generic publication handle.
    pub fn retain_backend_failure(
        &mut self,
        failure: NetworkGuardBackendFailure,
    ) -> Result<(), NetworkGuardTerminal> {
        let publication = {
            let mut state = self
                .shared
                .lifecycle
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.backend_completed = true;
            state.publication.clone()
        };
        match publication {
            Some(publication) => publication.retain_backend_failure(failure),
            None => terminal(self.shared.cached()).map_or(Ok(()), Err),
        }
    }
    /// Consume the actual backend return. Only an error may retain primary
    /// before monitor shutdown. A successful return stays provisional: it does
    /// not demote a pending guard result, and requires parent terminal proof.
    pub fn settle_backend_result<T, E: std::error::Error + 'static>(
        &mut self,
        result: Result<T, E>,
    ) -> GuardedBackendResult<T, E> {
        self.shared
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .backend_completed = true;
        match result {
            Ok(value) => GuardedBackendResult::SuccessPending(value),
            Err(error) => {
                let prior_guard = self
                    .retain_backend_failure(NetworkGuardBackendFailure::from_error(&error))
                    .err();
                GuardedBackendResult::Failure { error, prior_guard }
            }
        }
    }
    /// Includes a late guard result after backend publication; never discard it
    /// as if monitor termination alone proved policy cleanup.
    pub fn retained_terminal(&self) -> Option<NetworkGuardTerminal> {
        let publication = self
            .shared
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .publication
            .clone();
        publication
            .and_then(|publication| publication.terminal())
            .or_else(|| terminal(self.shared.cached()))
    }
    /// Local failures retained independently of the first typed primary.
    pub fn retained_failures(&self) -> Vec<GuardControlFailure> {
        self.shared
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .failures
            .clone()
    }
    /// Bounded join; this owner remains available on every error. No scheduler
    /// lock may be held by the caller. A completed stop is not helper teardown.
    pub fn stop_and_join(&mut self, deadline: Instant) -> io::Result<GuardObserverStopped> {
        self.shared.stop.store(true, Ordering::Release);
        loop {
            let finished = {
                let state = self
                    .shared
                    .lifecycle
                    .lock()
                    .map_err(|_| io::Error::other("guard stop ownership lock poisoned"))?;
                state.initial != InitialState::Submitted
                    && !state.spawn_in_flight
                    && state.observer.as_ref().is_none_or(JoinHandle::is_finished)
            };
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                let error = io::Error::new(io::ErrorKind::TimedOut, "guard observer join deadline");
                self.shared.record("stop_and_join", &error);
                return Err(error);
            };
            if finished {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(5)));
        }
        let (observer, started) = {
            let mut state = self
                .shared
                .lifecycle
                .lock()
                .map_err(|_| io::Error::other("guard join ownership lock poisoned"))?;
            (state.observer.take(), state.observer_started)
        };
        if let Some(observer) = observer {
            if observer.join().is_err() {
                let error = io::Error::other("guard observer terminated with panic");
                self.shared.record("observer_join", &error);
                self.shared.publish(internal_failure());
                return Err(error);
            }
        }
        // Registration and observer are settled; do not turn an unexpected
        // monitor lock holder into an unbounded wait after the deadline.
        let next = match self.shared.guard.try_lock() {
            Ok(mut guard) => observation(guard.snapshot()),
            Err(_) => {
                let error = io::Error::other("guard final snapshot owner unavailable");
                self.shared.record("final_snapshot", &error);
                return Err(error);
            }
        };
        let final_outcome = self.shared.publish(next);
        // The join and the final native snapshot must both be observed within
        // the original deadline. A late typed denial is still retained/reported;
        // it cannot become a successful local stop receipt.
        let expired = Instant::now() >= deadline;
        if expired {
            let error = io::Error::new(io::ErrorKind::TimedOut, "guard final observation deadline");
            self.shared.record("final_snapshot_deadline", &error);
        }
        self.shared.report_if_terminal(final_outcome);
        if expired {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guard final observation deadline",
            ));
        }
        if matches!(final_outcome, NetworkGuardOutcome::Pending) {
            let error = io::Error::other("guard final observation still pending");
            self.shared.record("final_snapshot", &error);
            return Err(error);
        }
        Ok(GuardObserverStopped {
            observation: final_outcome,
            observer_was_started: started,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_policy_and_complete_denial_survive_secondary_failure() {
        let first = NetworkGuardEvidence {
            denial: NetworkGuardDenial {
                incarnation: 9,
                injection: 17,
                reason: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let outcome = merge(NetworkGuardOutcome::Policy(first), internal_failure());
        let NetworkGuardOutcome::Policy(value) = outcome else {
            panic!("policy primary changed")
        };
        assert_eq!(value.denial, first.denial);
        assert_eq!(value.secondary_monitor_failures, UG_MONITOR_STATUS);
    }
    #[test]
    fn first_internal_is_not_relabelled_by_later_policy() {
        assert!(matches!(
            merge(
                internal_failure(),
                NetworkGuardOutcome::Policy(NetworkGuardEvidence::default())
            ),
            NetworkGuardOutcome::Internal(_)
        ));
    }
    #[test]
    fn pending_and_running_are_never_terminal_authority() {
        assert_eq!(terminal(NetworkGuardOutcome::Pending), None);
        assert_eq!(terminal(NetworkGuardOutcome::Running), None);
    }
}
