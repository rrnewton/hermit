//! Actual post-clone guard control, never serialized or reconstructed from Config.
use std::fmt::Debug;
use std::io;
use std::io::Write;
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Exact fields of the authenticated first native Unix-policy denial.
pub struct NetworkGuardDenial {
    /// Native guard phase at the first denial.
    pub phase: u64,
    /// Authenticated same-run provider incarnation.
    pub incarnation: u64,
    /// Actual kernel task lifetime discriminator.
    pub task_start: u64,
    /// Observed native task and process IDs.
    pub pid_tgid: u64,
    /// Exact source socket lifetime generation.
    pub source_generation: u64,
    /// Exact peer socket lifetime generation.
    pub peer_generation: u64,
    /// Associated native injection identity, if any.
    pub injection: u64,
    /// Qualified native hook number.
    pub hook: u32,
    /// Typed native denial reason.
    pub reason: u32,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Typed native primary evidence and accumulated secondary faults.
pub struct NetworkGuardEvidence {
    /// Accumulated monitor failure bits after the first primary.
    pub secondary_monitor_failures: u64,
    /// Accumulated native guard fault bits after the first primary.
    pub secondary_guard_faults: u64,
    /// Complete first native denial receipt.
    pub denial: NetworkGuardDenial,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Cached authenticated monitor state; native exit codes never classify it.
pub enum NetworkGuardOutcome {
    /// Initial native admission is pending.
    Pending,
    /// No terminal condition was observed.
    Running,
    /// Authenticated native policy denial.
    Policy(NetworkGuardEvidence),
    /// Authenticated native observer or guard failure.
    Internal(NetworkGuardEvidence),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A terminal typed monitor result; Pending and Running cannot abort.
pub enum NetworkGuardTerminal {
    /// Authenticated native policy denial.
    Policy(NetworkGuardEvidence),
    /// Authenticated native observer or guard failure.
    Internal(NetworkGuardEvidence),
}

#[derive(Debug, Default)]
struct PublicationState {
    backend_failure_retained: bool,
    terminal: Option<NetworkGuardTerminal>,
    guard_selected: bool,
}

/// An actual failed backend outcome. Successful exit cannot construct this.
#[derive(Debug)]
pub struct NetworkGuardBackendFailure {
    _private: (),
}
impl NetworkGuardBackendFailure {
    /// The caller retains this exact backend exit status alongside the guard
    /// owner. A successful guest exit is deliberately rejected.
    pub fn from_exit_status(status: reverie::process::ExitStatus) -> Option<Self> {
        (!status.success()).then_some(Self { _private: () })
    }
    /// The caller retains the actual returned backend error as the primary;
    /// this value controls ordering only and does not replace that error.
    pub fn from_error(_error: &(dyn std::error::Error + 'static)) -> Self {
        Self { _private: () }
    }
}

/// Result priority only; this cannot terminate a controller or enroll a task.
/// Keep the actual backend result outside its future while publishing it here.
#[derive(Clone, Debug)]
pub struct NetworkGuardPublication {
    state: Arc<Mutex<PublicationState>>,
}
impl NetworkGuardPublication {
    /// Retain an actual failed backend result before later guard evidence.
    /// On Err the guard had already selected the primary. This API has no
    /// success transition: successful guest bytes remain provisional until the
    /// parent proves the complete keeper, task, namespace and policy drain.
    pub fn retain_backend_failure(
        &self,
        _failure: NetworkGuardBackendFailure,
    ) -> Result<(), NetworkGuardTerminal> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.guard_selected {
            return Err(state.terminal.expect("selected guard terminal"));
        }
        state.backend_failure_retained = true;
        Ok(())
    }
    /// A late guard result is retained as secondary evidence and must be read
    /// after observer join; it is never silently converted into cleanup success.
    pub fn terminal(&self) -> Option<NetworkGuardTerminal> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Disposition when an actual backend result already owns the primary.
pub enum NetworkGuardTerminalDisposition {
    /// Preserve the published backend primary and attach guard evidence.
    RetainedAfterPublication,
}

/// Minted only by unsafe runtime adoption at the actual owned PID-namespace
/// controller boundary. Not Clone/Serialize and no public constructor.
#[derive(Debug)]
/// Post-clone guard capability retained by a separate recovery owner.
pub struct NetworkGuardControllerAbort {
    state: Arc<Mutex<PublicationState>>,
}
impl NetworkGuardControllerAbort {
    pub(crate) fn owned_controller() -> Self {
        Self {
            state: Arc::default(),
        }
    }
    /// The independent observer owner uses this handle to preserve the actual
    /// backend result before stopping; it conveys no process-exit authority.
    pub fn publication(&self) -> NetworkGuardPublication {
        NetworkGuardPublication {
            state: self.state.clone(),
        }
    }
    fn select(&self, terminal: NetworkGuardTerminal) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.terminal.is_none() {
            state.terminal = Some(terminal);
        }
        if state.backend_failure_retained {
            return false;
        }
        state.guard_selected = true;
        true
    }
    /// Abort only if this guard observation owns the first primary decision.
    /// After backend publication, retain typed secondary evidence and return.
    /// No cleanup completion is inferred in either case.
    pub fn report_terminal(
        &self,
        terminal: NetworkGuardTerminal,
    ) -> NetworkGuardTerminalDisposition {
        if !self.select(terminal) {
            return NetworkGuardTerminalDisposition::RetainedAfterPublication;
        }
        // The first terminal receipt, not a later competing observation, owns
        // both the diagnostic and status. This native observer never needs a
        // guest turn or a scheduler lock to stop the owned PID namespace.
        let first = self.publication().terminal().expect("selected terminal");
        let status = match first {
            NetworkGuardTerminal::Policy(_) => detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
            NetworkGuardTerminal::Internal(_) => 125,
        };
        let _ = writeln!(
            crate::util::RetryingStderr,
            "hermit: Unix network guard terminal: {first:?}; unfinished state is not published"
        );
        crate::tool_global::exit_owned_controller(status)
    }
}

/// Post-clone guard capability retained by a separate recovery owner.
pub trait NetworkGuardControl: Debug + Send + Sync {
    /// # Safety
    /// pidfd is the actual backend-authenticated stopped initial guest task,
    /// supplied before any guest instruction/FD operation. No numeric reopen.
    unsafe fn register_stopped_initial(
        &self,
        pidfd: BorrowedFd<'_>,
        deadline: Instant,
    ) -> io::Result<()>;

    /// Short-lock cached result only: never invoke the native 50ms poll here.
    fn observation(&self) -> NetworkGuardOutcome;

    /// Exactly one post-clone native observer. The outside-backend owner retains
    /// the guard, thread, startup error and incomplete stop/join state. It pumps
    /// the authenticated native monitor independently of guest progress, and
    /// calls report_terminal only for an actual typed terminal observation.
    fn start_observer(&self, abort: NetworkGuardControllerAbort) -> io::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> NetworkGuardTerminal {
        NetworkGuardTerminal::Policy(NetworkGuardEvidence::default())
    }
    fn internal() -> NetworkGuardTerminal {
        NetworkGuardTerminal::Internal(NetworkGuardEvidence::default())
    }
    #[test]
    fn selected_guard_precedes_a_later_backend_failure() {
        let abort = NetworkGuardControllerAbort::owned_controller();
        assert!(abort.select(policy()));
        assert_eq!(
            abort.publication().retain_backend_failure(
                NetworkGuardBackendFailure::from_exit_status(reverie::process::ExitStatus::Exited(
                    1
                ))
                .unwrap()
            ),
            Err(policy())
        );
    }
    #[test]
    fn known_backend_failure_retains_late_guard_as_secondary() {
        let abort = NetworkGuardControllerAbort::owned_controller();
        let publication = abort.publication();
        publication
            .retain_backend_failure(
                NetworkGuardBackendFailure::from_exit_status(reverie::process::ExitStatus::Exited(
                    1,
                ))
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            abort.report_terminal(policy()),
            NetworkGuardTerminalDisposition::RetainedAfterPublication
        );
        assert_eq!(publication.terminal(), Some(policy()));
        publication
            .retain_backend_failure(
                NetworkGuardBackendFailure::from_exit_status(reverie::process::ExitStatus::Exited(
                    1,
                ))
                .unwrap(),
            )
            .unwrap();
    }
    #[test]
    fn success_cannot_retain_primary_before_guard_or_parent_drain() {
        assert!(
            NetworkGuardBackendFailure::from_exit_status(reverie::process::ExitStatus::SUCCESS)
                .is_none()
        );
        let abort = NetworkGuardControllerAbort::owned_controller();
        assert!(abort.select(policy()));
        assert!(abort.publication().state.lock().unwrap().guard_selected);
    }
    #[test]
    fn competing_guard_terminal_does_not_replace_the_first_receipt() {
        let abort = NetworkGuardControllerAbort::owned_controller();
        assert!(abort.select(internal()));
        assert!(abort.select(policy()));
        assert_eq!(abort.publication().terminal(), Some(internal()));
    }
}
