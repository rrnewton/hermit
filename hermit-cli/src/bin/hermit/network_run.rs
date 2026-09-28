// Copyright (c) Meta Platforms, Inc. and affiliates.
// This source code is licensed under the BSD-style license found in LICENSE.

//! The CLI owns the encoded child result and external guard until both retire.
//! No successful value or verification result crosses this boundary early.

use std::any::Any;
use std::cell::RefCell;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use detcore::network_runtime::NetworkRuntimeResources;
use detcore::network_runtime::capability_unit::CapabilityServiceKind;
use detcore::network_runtime::capability_unit::CapabilityServiceLifetime;
use detcore::network_runtime::capability_unit::CapabilityUnitLaunch;
use detcore::network_runtime::guard::NetworkGuardBackendFailure;
use hermit::Error;
use hermit::SerializableError;
use hermit::unix_guard::GuardOutcome;
use hermit::unix_guard::prepare_guard;
use hermit::unix_guard_package::GuardDeploymentRoots;
use hermit::unix_guard_package::PackagedUnixGuard;
use hermit::unix_guard_terminal::GuardDrainEvidence;
use hermit::unix_guard_terminal::GuardParentFinalizer;
use reverie::process::ChildCleanupObservation;
use reverie::process::Container;
use reverie::process::ExitStatus;
use reverie::process::Output;
use reverie::process::OwnedFinalization;
use reverie::process::OwnedFinalize;
use reverie::process::OwnedReapedResult;
use reverie::process::OwnedRunFailure;
use reverie::process::RunError;
use reverie::process::StartupError;
use reverie::process::StartupOwnedFailure;

use super::container::PolicyRefusal;
use super::container::arm_container_init_guards;
use super::container::catch_child_panic_at;
use super::container::classify_container_result;

const STARTUP: Duration = Duration::from_secs(30);
const TERMINAL: Duration = Duration::from_secs(30);
const OBSERVER_STOP: Duration = Duration::from_secs(1);

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(super) enum RunValue {
    Run(ExitStatus, Option<Output>),
    Verify(Output, u64),
}
impl RunValue {
    pub(super) fn into_status(self) -> Result<ExitStatus, Error> {
        match self {
            Self::Run(status, None) => Ok(status),
            _ => Err(Error::msg(
                "wrong authenticated full-record/replay status result kind",
            )),
        }
    }
    pub(super) fn into_output(self) -> Result<Output, Error> {
        match self {
            Self::Run(status, Some(output)) if status == output.status => Ok(output),
            _ => Err(Error::msg(
                "wrong authenticated full-record/replay output result kind",
            )),
        }
    }
    fn status(&self) -> ExitStatus {
        match self {
            Self::Run(status, _) => *status,
            Self::Verify(output, _) => output.status,
        }
    }
}
type Wire = Result<RunValue, SerializableError>;

/// Failure-only return: preserve the actual guest status while withholding all
/// output/verification data after refused publication. It cannot denote success.
#[derive(Debug)]
pub(super) struct UnpublishedGuestFailure {
    status: ExitStatus,
    phase: &'static str,
    detail: String,
}
impl UnpublishedGuestFailure {
    pub(super) fn status(&self) -> ExitStatus {
        self.status
    }
    pub(super) fn phase(&self) -> &'static str {
        self.phase
    }
}
impl std::fmt::Display for UnpublishedGuestFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "guest failed with {:?}; result publication withheld by Unix {}: {}",
            self.status, self.phase, self.detail
        )
    }
}
impl std::error::Error for UnpublishedGuestFailure {}

struct FailedOwners {
    _child: Option<OwnedFinalization<Wire>>,
    _guard: Option<GuardParentFinalizer>,
    _provider: Option<AcceptedStartup>,
    _backing: Option<Box<dyn Any>>,
}
thread_local! {
    // This is one failed CLI invocation's real custody, not a reconstructed
    // registry. Main exits after the error. Drop must not release uncertain
    // aliases or introduce an unbounded implicit Container wait.
    static FAILED: RefCell<Option<ManuallyDrop<FailedOwners>>> = const { RefCell::new(None) };
}
fn retain(
    child: Option<OwnedFinalization<Wire>>,
    guard: Option<GuardParentFinalizer>,
    provider: Option<AcceptedStartup>,
    backing: Option<Box<dyn Any>>,
) {
    FAILED.with(|slot| {
        let mut slot = slot.borrow_mut();
        assert!(
            slot.is_none(),
            "a failed network invocation cannot start another run"
        );
        *slot = Some(ManuallyDrop::new(FailedOwners {
            _child: child,
            _guard: guard,
            _provider: provider,
            _backing: backing,
        }));
    });
}
fn refusal(message: impl std::fmt::Display) -> Error {
    Error::new(PolicyRefusal).context(format!(
        "network disabled: Unix policy startup unavailable: {message}"
    ))
}
fn receipt_file(path: &Path, name: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path.join(name))
}
fn emit(file: &mut File, value: serde_json::Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()
}
fn proof_json(
    proof: &GuardDrainEvidence,
    child_pid: Option<i32>,
    child_status: Option<ExitStatus>,
) -> serde_json::Value {
    let birth = proof.birth.map(|b| serde_json::json!({"incarnation":b.incarnation,"sequence":b.sequence,"object":b.object,"generation":b.generation,"cookie":b.cookie}));
    let (outcome, evidence) = match proof.provisional.outcome {
        GuardOutcome::Pending => ("pending", None),
        GuardOutcome::Running => ("running", None),
        GuardOutcome::Policy(e) => ("policy", Some(e)),
        GuardOutcome::Internal(e) => ("internal", Some(e)),
    };
    let denial = evidence.map(|e| serde_json::json!({"phase":e.denial.phase,"incarnation":e.denial.incarnation,"task_start":e.denial.task_start,"pid_tgid":e.denial.pid_tgid,"source_generation":e.denial.source_generation,"peer_generation":e.denial.peer_generation,"injection":e.denial.injection,"hook":e.denial.hook,"reason":e.denial.reason,"secondary_monitor_failures":e.secondary_monitor_failures,"secondary_guard_faults":e.secondary_guard_faults}));
    serde_json::json!({"schema":1,"stage":"terminal","guard":{"incarnation":proof.provisional.incarnation,"birth":birth,"terminal":{"proof_sequence":proof.provisional.proof_sequence,"reply_sequence":proof.provisional.reply_sequence,"record_ordinal":proof.provisional.record_ordinal,"initial_tasks":proof.provisional.initial_tasks,"removed_links":proof.provisional.removed_links,"removed_map_pins":proof.provisional.removed_map_pins,"outcome":outcome},"denial":denial,"inventory":{"counts":proof.counts,"ids":proof.original_ids},"readback":{"original_ids":proof.readback.original_ids(),"closed_ns":proof.readback.closed_ns(),"deadline_ns":proof.readback.deadline_ns(),"observed_ns":proof.readback.observed_ns(),"complete_passes":proof.readback.complete_passes()}},"child":{"pid":child_pid,"wait_status":child_status.map(ExitStatus::into_raw),"exit_code":child_status.and_then(|s|s.code()),"signal":child_status.and_then(|s|s.signal())},"loader":proof.loader,"query":proof.query,"loader_wait":proof.loader_wait,"query_wait":proof.query_wait,"terminal_observed_ns":proof.terminal_observed_ns})
}

/// The private certificate binds non-vacuous enrollment and actual child wait
/// to the completed external drain. Its constructor is not a config/JSON API.
struct GuestCompletion<'a> {
    value: RunValue,
    guard: &'a GuardDrainEvidence,
}
impl<'a> GuestCompletion<'a> {
    fn certify(value: RunValue, guard: &'a GuardDrainEvidence) -> Result<Self, Error> {
        if guard.birth.is_none()
            || guard.provisional.initial_tasks == 0
            || guard.counts != [10, 31, 31]
            || guard.original_ids.len() != 72
            || guard.readback.original_ids() != 72
            || guard.readback.complete_passes() != 2
            || guard.loader_wait != 0
            || guard.query_wait != 0
        {
            return Err(Error::msg(
                "Unix guest terminal lacks full enrollment, inventory or actor proof",
            ));
        }
        Ok(Self { value, guard })
    }
    fn publish(self) -> Result<RunValue, Error> {
        match self.guard.provisional.outcome {
            GuardOutcome::Running => Ok(self.value),
            GuardOutcome::Policy(evidence) => Err(Error::new(PolicyRefusal)
                .context(format!("network disabled by Unix policy: {evidence:?}"))),
            outcome => Err(Error::msg(format!(
                "Unix guard final state refuses successful publication: {outcome:?}"
            ))),
        }
    }
}

/// A missing wire result is not policy authority. This candidate only records
/// the exact failure and owned reaped status that a later guard certificate may
/// explain. It is never created for an earlier startup/backend failure.
#[derive(Clone, Copy)]
struct MissingResultAbort;
impl MissingResultAbort {
    fn observe(
        cause: OwnedRunFailure,
        child: ChildCleanupObservation,
        earlier_failure: bool,
    ) -> Option<Self> {
        (!earlier_failure
            && cause == OwnedRunFailure::Startup(StartupError::MissingResult)
            && matches!(child, ChildCleanupObservation::Reaped(ExitStatus::Exited(code))
                if code == detcore_model::HERMIT_POLICY_REFUSAL_EXIT))
        .then_some(Self)
    }
    fn certify(self, expected: u64, guard: &GuardDrainEvidence) -> Result<Error, Error> {
        // The actual private readback certificate and actor drain enter only
        // here. The value projection below supports predicate tests; it cannot
        // construct GuardDrainEvidence or authorize physical cleanup.
        let facts = PolicyAbortFacts::from_guard(guard);
        facts.validate(expected)?;
        Ok(Error::new(PolicyRefusal).context(format!(
            "network disabled by Unix policy: {:?}",
            guard.provisional.outcome
        )))
    }
}

/// Read-only facts from the actual finalizer, not an admission/cleanup token.
#[derive(Clone)]
struct PolicyAbortFacts {
    birth: Option<hermit::unix_guard::GuardBirth>,
    terminal: hermit::unix_guard::GuardProvisionalReceipt,
    counts: [u32; 3],
    ids: Vec<(u32, u32)>,
    readback: [u64; 8],
    actor_waits: [i32; 2],
    terminal_observed_ns: u64,
}
impl PolicyAbortFacts {
    fn from_guard(guard: &GuardDrainEvidence) -> Self {
        let readback = &guard.readback;
        Self {
            birth: guard.birth,
            terminal: guard.provisional,
            counts: guard.counts,
            ids: guard.original_ids.clone(),
            readback: [
                readback.incarnation(),
                readback.proof_sequence(),
                readback.record_ordinal(),
                readback.closed_ns(),
                readback.deadline_ns(),
                readback.original_ids(),
                readback.observed_ns(),
                readback.complete_passes(),
            ],
            actor_waits: [guard.loader_wait, guard.query_wait],
            terminal_observed_ns: guard.terminal_observed_ns,
        }
    }
    fn validate(&self, expected: u64) -> Result<(), Error> {
        let birth = self
            .birth
            .ok_or_else(|| Error::msg("policy abort lacks namespace birth"))?;
        let terminal = self.terminal;
        let evidence = match terminal.outcome {
            GuardOutcome::Policy(evidence) => evidence,
            _ => return Err(Error::msg("missing result has no committed policy outcome")),
        };
        let denial = evidence.denial;
        let unique = self
            .ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let mut counts = [0u32; 3];
        for &(kind, id) in &unique {
            if kind > 2 || id == 0 {
                return Err(Error::msg(
                    "policy abort has invalid original object identity",
                ));
            }
            counts[kind as usize] += 1;
        }
        // Phase/reason/hook values are the maintained ug_denial ABI. Internal
        // reason4 is never reclassified as a policy refusal.
        if expected == 0
            || birth.incarnation != expected
            || birth.sequence == 0
            || birth.object == 0
            || birth.generation == 0
            || birth.cookie == 0
            || terminal.incarnation != expected
            || terminal.initial_tasks == 0
            || terminal.proof_sequence <= birth.sequence
            || terminal.reply_sequence < terminal.proof_sequence
            || terminal.record_ordinal == 0
            || terminal.removed_links != 31
            || terminal.removed_map_pins != 10
            || denial.phase != 2
            || denial.incarnation != expected
            || denial.task_start == 0
            || denial.pid_tgid as u32 == 0
            || denial.pid_tgid >> 32 == 0
            || !(1..=19).contains(&denial.hook)
            || !matches!(denial.reason, 1 | 2 | 3 | 5)
            || self.counts != [10, 31, 31]
            || counts != self.counts
            || self.ids.len() != 72
            || unique.len() != 72
            || self.readback[0] != expected
            || self.readback[1] != terminal.proof_sequence
            || self.readback[2] < terminal.record_ordinal
            || self.readback[3] == 0
            || self.readback[4] <= self.readback[3]
            || self.readback[4] - self.readback[3] > 1_000_000_000
            || self.readback[5] != 72
            || self.readback[7] != 2
            || self.readback[6] < self.readback[3]
            || self.terminal_observed_ns < self.readback[6]
            || self.terminal_observed_ns >= self.readback[4]
            || self.actor_waits != [0, 0]
        {
            return Err(Error::msg(
                "missing result lacks complete same-run policy abort certificate",
            ));
        }
        Ok(())
    }
}

/// Private provisional data after the actual child has been reaped. Decode can
/// reveal a panic/error even when the transport child exited zero. Retain that
/// primary before external drain; successful data cannot leave this wrapper
/// until drain and the positive-only guard certificate both succeed.
struct UnpublishedChildResult(Result<RunValue, Error>);
impl UnpublishedChildResult {
    fn decode(child: OwnedReapedResult<Wire>) -> Self {
        Self(
            child
                .decode()
                .map_err(|failure| Error::msg(format!("owned network child decode: {failure:?}")))
                .and_then(|wire| classify_container_result(Ok(wire))),
        )
    }
    fn failure_description(&self) -> Option<String> {
        match &self.0 {
            Err(error) => Some(format!("{error:#}")),
            Ok(value) if !value.status().success() => {
                Some(format!("guest status: {:?}", value.status()))
            }
            _ => None,
        }
    }
    fn classify_missing_abort(
        self,
        candidate: Option<MissingResultAbort>,
        certify: impl FnOnce(MissingResultAbort) -> Result<Error, Error>,
    ) -> Self {
        let Some(candidate) = candidate else {
            return self;
        };
        let Err(primary) = self.0 else {
            return self;
        };
        Self(Err(match certify(candidate) {
            Ok(policy) => policy,
            Err(error) => {
                eprintln!("hermit: secondary policy abort certification: {error:#}");
                primary
            }
        }))
    }
    fn publish(
        self,
        drain: Result<(), Error>,
        certify_value: impl FnOnce(RunValue) -> Result<RunValue, Error>,
    ) -> Result<RunValue, Error> {
        let value = match self.0 {
            Err(primary) => {
                if let Err(error) = drain {
                    eprintln!("hermit: secondary Unix aggregate cleanup: {error:#}");
                }
                return Err(primary);
            }
            Ok(value) => value,
        };
        let status = value.status();
        let preserve_failure = |phase, error: Error| {
            if status.success() {
                error
            } else {
                Error::new(UnpublishedGuestFailure {
                    status,
                    phase,
                    detail: format!("{error:#}"),
                })
            }
        };
        drain.map_err(|error| preserve_failure("cleanup", error))?;
        // Every data-bearing Ok, including an allowed nonzero verification
        // result, requires full enrollment and final guard authorization.
        certify_value(value).map_err(|error| preserve_failure("certificate", error))
    }
}

/// Existing authenticated deployment roots shared by full record and replay.
#[derive(Debug, Default, Clone, clap::Args)]
pub(super) struct DeploymentOpts {
    /// Existing private bpffs directory for network isolation during replay.
    #[arg(long, value_name = "DIRECTORY", requires = "network_guard_recovery")]
    network_guard_bpffs: Option<std::path::PathBuf>,
    /// Existing private directory for bounded network-provider recovery logs.
    #[arg(long, value_name = "DIRECTORY", requires = "network_guard_bpffs")]
    network_guard_recovery: Option<std::path::PathBuf>,
    /// Separate existing private accepted-provider receipt directory.
    #[arg(long, value_name = "DIRECTORY")]
    network_accepted_recovery: Option<std::path::PathBuf>,
}
impl DeploymentOpts {
    pub(super) fn accepted_root(&self) -> Option<&Path> {
        self.network_accepted_recovery.as_deref()
    }
    pub(super) fn roots(&self) -> Result<Option<(&Path, &Path)>, Error> {
        deployment_roots(
            self.network_guard_bpffs.as_deref(),
            self.network_guard_recovery.as_deref(),
        )
    }
}
pub(super) fn deployment_roots<'a>(
    bpffs: Option<&'a Path>,
    recovery: Option<&'a Path>,
) -> Result<Option<(&'a Path, &'a Path)>, Error> {
    match (bpffs, recovery) {
        (None, None) => Ok(None),
        (Some(bpffs), Some(recovery)) => Ok(Some((bpffs, recovery))),
        _ => Err(refusal("both Unix guard deployment roots are required")),
    }
}
fn accepted_recovery_root(
    roots: Option<(&Path, &Path)>,
) -> std::io::Result<hermit::unix_guard_package::RecoveryDeploymentRoot> {
    match roots {
        Some((_, recovery)) => {
            hermit::unix_guard_package::RecoveryDeploymentRoot::open_at(recovery)
        }
        None => hermit::unix_guard_package::RecoveryDeploymentRoot::open_accepted(),
    }
}

fn check_recovery_routing(
    roots: Option<(&Path, &Path)>,
    accepted: Option<&Path>,
    use_guard: bool,
    use_accepted: bool,
) -> Result<(), Error> {
    if accepted.is_some() && !use_accepted {
        return Err(refusal(
            "accepted recovery root requires Record or Replay custody",
        ));
    }
    if use_guard && use_accepted && roots.is_some() && accepted.is_none() {
        return Err(refusal(
            "combined Replay with explicit guard roots requires --network-accepted-recovery",
        ));
    }
    Ok(())
}
fn accepted_root_for_mode(
    roots: Option<(&Path, &Path)>,
    explicit: Option<&Path>,
    use_guard: bool,
) -> std::io::Result<hermit::unix_guard_package::RecoveryDeploymentRoot> {
    match explicit {
        Some(path) => hermit::unix_guard_package::RecoveryDeploymentRoot::open_at(path),
        None if use_guard => hermit::unix_guard_package::RecoveryDeploymentRoot::open_accepted(),
        None => accepted_recovery_root(roots),
    }
}

/// Default policy and replay use the same Unix guard startup and terminal proof.
pub(super) fn run(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    mut execute: impl FnMut(Option<NetworkRuntimeResources>) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    run_owned(
        container,
        roots,
        accepted_root,
        true,
        false,
        "with_container",
        None,
        None,
        None,
        |resource, _| execute(resource),
    )
}
/// Record retains live networking and authenticates original syscall observation.
pub(super) fn run_record(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    mut execute: impl FnMut(Option<NetworkRuntimeResources>) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    run_owned(
        container,
        roots,
        accepted_root,
        false,
        true,
        "with_container",
        None,
        None,
        None,
        |resource, _| execute(resource),
    )
}
/// Replay needs original file-table authority and fail-closed network policy.
/// The caller must also install the offline network namespace before clone.
pub(super) fn run_replay(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    mut execute: impl FnMut(Option<NetworkRuntimeResources>) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    run_owned(
        container,
        roots,
        accepted_root,
        true,
        true,
        "with_container",
        None,
        None,
        None,
        |resource, _| execute(resource),
    )
}
pub(super) fn run_record_at(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    site: &'static str,
    mut execute: impl FnMut(Option<NetworkRuntimeResources>) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    run_owned(
        container,
        roots,
        accepted_root,
        false,
        true,
        site,
        None,
        None,
        None,
        |resource, _| execute(resource),
    )
}
pub(super) fn run_replay_at(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    site: &'static str,
    listener: Option<std::net::TcpListener>,
    execute: impl FnMut(
        Option<NetworkRuntimeResources>,
        Option<std::net::TcpListener>,
    ) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    run_owned(
        container,
        roots,
        accepted_root,
        true,
        true,
        site,
        listener,
        None,
        None,
        execute,
    )
}

fn run_owned_with_guards<G, F>(
    container: &mut Container,
    guards: G,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    use_guard: bool,
    use_accepted: bool,
    site: &'static str,
    listener: Option<std::net::TcpListener>,
    timeout: Option<Duration>,
    work: F,
) -> Result<(RunValue, G), Error>
where
    G: 'static,
    F: FnMut(
            &mut G,
            Option<NetworkRuntimeResources>,
            Option<std::net::TcpListener>,
        ) -> Result<RunValue, Error>
        + 'static,
{
    let state = Rc::new(RefCell::new((guards, work)));
    let child_state = Rc::clone(&state);
    let backing: Option<Box<dyn Any>> = Some(Box::new(Rc::clone(&state)));
    let value = run_owned(
        container,
        roots,
        accepted_root,
        use_guard,
        use_accepted,
        site,
        listener,
        timeout,
        backing,
        move |resource, listener| {
            let mut state = child_state.borrow_mut();
            let (guards, work) = &mut *state;
            work(guards, resource, listener)
        },
    )?;
    let (guards, _) = Rc::try_unwrap(state)
        .map_err(|_| Error::msg("network run retained guards after confirmed completion"))?
        .into_inner();
    Ok((value, guards))
}

pub(super) fn run_record_at_owned<G, F>(
    container: &mut Container,
    guards: G,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    site: &'static str,
    timeout: Option<Duration>,
    mut work: F,
) -> Result<(RunValue, G), Error>
where
    G: 'static,
    F: FnMut(&mut G, Option<NetworkRuntimeResources>) -> Result<RunValue, Error> + 'static,
{
    run_owned_with_guards(
        container,
        guards,
        roots,
        accepted_root,
        false,
        true,
        site,
        None,
        timeout,
        move |guards, resource, _| work(guards, resource),
    )
}

pub(super) fn run_replay_at_owned<G, F>(
    container: &mut Container,
    guards: G,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    site: &'static str,
    listener: Option<std::net::TcpListener>,
    timeout: Option<Duration>,
    work: F,
) -> Result<(RunValue, G), Error>
where
    G: 'static,
    F: FnMut(
            &mut G,
            Option<NetworkRuntimeResources>,
            Option<std::net::TcpListener>,
        ) -> Result<RunValue, Error>
        + 'static,
{
    run_owned_with_guards(
        container,
        guards,
        roots,
        accepted_root,
        true,
        true,
        site,
        listener,
        timeout,
        work,
    )
}

pub(super) fn run_at_owned<G, F>(
    container: &mut Container,
    guards: G,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    site: &'static str,
    timeout: Option<Duration>,
    mut work: F,
) -> Result<(RunValue, G), Error>
where
    G: 'static,
    F: FnMut(&mut G, Option<NetworkRuntimeResources>) -> Result<RunValue, Error> + 'static,
{
    run_owned_with_guards(
        container,
        guards,
        roots,
        accepted_root,
        true,
        false,
        site,
        None,
        timeout,
        move |guards, resource, _| work(guards, resource),
    )
}

/// The clone creates exactly two descriptor aliases. Drop the parent's alias
/// in the actual after-clone callback; the child alone supplies the listener to
/// GDB. CLOEXEC separately closes guest aliases at exec. Holding the parent
/// alias until container return would fool GdbClientWatch after a healthy accept.
struct ControllerDebugger(RefCell<Option<std::net::TcpListener>>);
impl ControllerDebugger {
    fn release_parent(&self) {
        drop(self.0.borrow_mut().take());
    }
    fn take_child(&self) -> Option<std::net::TcpListener> {
        self.0.borrow_mut().take()
    }
}

fn requires_grouped_parent(topology: &detcore::network_runtime::ProviderTopology) -> bool {
    matches!(
        topology,
        detcore::network_runtime::ProviderTopology::GroupedV1 { .. }
    )
}

struct AcceptedStartup {
    launch: Option<detcore::network_runtime::AcceptedProviderLaunch>,
    owner: Option<hermit::accepted_terminal::AcceptedParentFinalizer>,
    receipts: hermit::accepted_terminal::AcceptedRecovery,
}
impl AcceptedStartup {
    fn retain(recovery: hermit::unix_guard_package::RecoveryDeploymentRoot) -> Self {
        Self {
            launch: None,
            owner: None,
            receipts: hermit::accepted_terminal::AcceptedRecovery::retain(
                recovery,
                uuid::Uuid::new_v4().simple().to_string(),
            ),
        }
    }
    fn prepare(&mut self) -> Result<(), Error> {
        let executable = std::env::current_exe()?;
        let package =
            hermit::network_provider_package::PackagedAcceptedProvider::discover(&executable)?;
        let mut namespace_setup = package.open_namespace_setup()?;
        let namespace_setup_sha256 = package.namespace_setup_sha256;
        self.receipts.initialize(package.expected.clone())?;
        let (stdout, stderr) = self.receipts.service_logs()?;
        let launch = detcore::network_runtime::AcceptedProviderLaunch {
            helper: executable.clone(),
            object: package.object,
            library: package.library,
            expected: package.expected,
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            stdout: stdout.try_clone()?.into(),
            stderr: stderr.try_clone()?.into(),
        };
        self.launch = Some(launch);
        self.owner = Some(
            hermit::accepted_terminal::AcceptedParentFinalizer::before_startup(
                stdout, stderr, executable,
            ),
        );
        let launch = self.launch.as_ref().unwrap();
        if requires_grouped_parent(&launch.expected.topology) {
            // Sealing reads bytes only. No loader, thread, child or active
            // native session crosses Container clone; after_spawn starts them.
            let artifacts = hermit::network_provider_package::SealedAcceptedArtifacts::snapshot(
                &launch.object,
                &launch.library,
            )?;
            let bridge = artifacts.library_fd().try_clone_to_owned()?;
            let root = self.receipts.create_grouped_sibling()?;
            let mut grouped = Some(detcore::network_runtime::GroupedParentOwner::retain(
                root,
                launch.helper.clone(),
                bridge,
                launch.expected.library_sha256,
                namespace_setup
                    .take()
                    .ok_or_else(|| {
                        std::io::Error::other("grouped trusted namespace setup member absent")
                    })?
                    .into(),
                namespace_setup_sha256.ok_or_else(|| {
                    std::io::Error::other("grouped namespace setup fingerprint absent")
                })?,
            ));
            self.owner.as_mut().unwrap().install_grouped(&mut grouped)?;
        }
        Ok(())
    }
    fn observe(
        &mut self,
        owner: &hermit::network_container::NetworkParentOwnership,
        deadline: Instant,
    ) -> std::io::Result<()> {
        let finalizer = self
            .owner
            .as_mut()
            .expect("startup retains original accepted owner");
        finalizer.observe_startup(owner, deadline)?;
        self.receipts.started(finalizer.startup_receipt(owner)?)
    }
    fn attach(
        &mut self,
        parent: Option<hermit::network_container::NetworkParentOwnership>,
    ) -> Option<Error> {
        use hermit::network_container::NetworkParentOwnership;
        let (service, error) = match parent {
            Some(NetworkParentOwnership::Running(service)) => (Some(service), None),
            Some(NetworkParentOwnership::StartupFailed(failure)) => (
                Some(failure.owner),
                Some(Error::new(failure.error).context("accepted provider startup")),
            ),
            None => (None, None),
        };
        if let Some(service) = service {
            self.owner = Some(
                self.owner
                    .take()
                    .expect("original accepted owner")
                    .with_service(service),
            );
        }
        error
    }
}
struct GuardFinalization {
    owner: GuardParentFinalizer,
    evidence: File,
    incarnation: u64,
}

fn run_owned(
    container: &mut Container,
    roots: Option<(&Path, &Path)>,
    accepted_root: Option<&Path>,
    use_guard: bool,
    use_accepted: bool,
    site: &'static str,
    listener: Option<std::net::TcpListener>,
    timeout: Option<Duration>,
    mut failed_backing: Option<Box<dyn Any>>,
    mut execute: impl FnMut(
        Option<NetworkRuntimeResources>,
        Option<std::net::TcpListener>,
    ) -> Result<RunValue, Error>,
) -> Result<RunValue, Error> {
    let debugger = ControllerDebugger(RefCell::new(listener));
    if FAILED.with(|slot| slot.borrow().is_some()) {
        return Err(refusal("earlier invocation retains unresolved ownership"));
    }
    check_recovery_routing(roots, accepted_root, use_guard, use_accepted)?;
    hermit::unix_guard_terminal::prepare_command_parent()?;
    // All fallible package/log preparation precedes launching either service.
    // Accepted-only Record authenticates the recovery root without requiring bpffs.
    let guard_roots = if use_guard {
        Some(
            match roots {
                Some((bpffs, recovery)) => GuardDeploymentRoots::open_at(bpffs, recovery),
                None => GuardDeploymentRoots::open(),
            }
            .map_err(refusal)?,
        )
    } else {
        None
    };
    let mut accepted = if use_accepted {
        let root = accepted_root_for_mode(roots, accepted_root, use_guard)?;
        if let Some(guard) = &guard_roots {
            root.require_disjoint(guard.recovery.as_fd(), &guard.writable_paths[1])?;
            root.require_disjoint(guard.bpffs.as_fd(), &guard.writable_paths[0])?;
        }
        Some(AcceptedStartup::retain(root))
    } else {
        None
    };
    if let Some(startup) = accepted.as_mut() {
        if let Err(error) = startup.prepare() {
            let _ = startup.receipts.failure(&error.to_string());
            retain(None, None, accepted.take(), failed_backing.take());
            return Err(error);
        }
    }
    let guard_preparation = (|| -> Result<_, Error> {
        let mut guard_context = None;
        let prepared = if let Some(roots) = guard_roots {
            roots.admit_launch().map_err(refusal)?;
            let package =
                PackagedUnixGuard::discover(&std::env::current_exe()?).map_err(refusal)?;
            let identity = uuid::Uuid::new_v4().simple().to_string();
            let incarnation = u64::from_str_radix(&identity[..16], 16).map_err(refusal)?;
            if incarnation == 0 {
                return Err(refusal("zero random incarnation"));
            }
            let unit = format!("hermit-unix-{identity}.service");
            let mut evidence = receipt_file(
                &roots.writable_paths[1],
                &format!("{identity}.terminal.jsonl"),
            )?;
            let stdout = receipt_file(&roots.writable_paths[1], &format!("{identity}.stdout.log"))?;
            let stderr = receipt_file(&roots.writable_paths[1], &format!("{identity}.stderr.log"))?;
            emit(
                &mut evidence,
                serde_json::json!({"schema":1,"stage":"before_launch","incarnation":incarnation,"loader_unit":unit}),
            )?;
            let launch = CapabilityUnitLaunch {
                kind: CapabilityServiceKind::UnixGuard,
                unit: &unit,
                executable: &package.helper,
                arguments: &[],
                lifetime: CapabilityServiceLifetime::ControllerOwned,
                writable_directories: &roots.writable_paths,
            };
            let prepared = match unsafe {
                prepare_guard(
                    &launch,
                    stdout.as_fd(),
                    stderr.as_fd(),
                    package.object.try_clone()?,
                    roots.bpffs,
                    roots.recovery,
                    incarnation,
                    Instant::now() + STARTUP,
                )
            } {
                Ok(prepared) => prepared,
                Err(failure) => {
                    let primary = refusal(&failure.error);
                    if let Some(guard) = failure.owner {
                        let mut owner = GuardParentFinalizer::new(
                            guard,
                            &package,
                            stdout.into(),
                            stderr.into(),
                        );
                        let cleanup = owner.drain(Instant::now() + TERMINAL);
                        let _ = emit(
                            &mut evidence,
                            serde_json::json!({"schema":1,"stage":"startup_failed","error":failure.error.to_string(),"cleanup_error":cleanup.err().map(|e|e.to_string())}),
                        );
                        if owner.failure().is_some() {
                            retain(None, Some(owner), accepted.take(), failed_backing.take());
                        }
                    }
                    return Err(primary);
                }
            };
            guard_context = Some((package, stdout, stderr, evidence, incarnation));
            Some(prepared)
        } else {
            None
        };
        Ok((guard_context, prepared))
    })();
    let (guard_context, prepared) = match guard_preparation {
        Ok(value) => value,
        Err(error) => {
            if let Some(startup) = accepted.as_mut() {
                let _ = startup
                    .receipts
                    .failure(&format!("guard preparation: {error}"));
            }
            if accepted.is_some() {
                retain(None, None, accepted.take(), failed_backing.take());
            }
            return Err(error);
        }
    };
    let accepted_launch = accepted.as_mut().and_then(|startup| startup.launch.take());
    let mut grouped_hook = accepted
        .as_ref()
        .and_then(|startup| startup.owner.as_ref())
        .map(|owner| owner.grouped_hook())
        .unwrap_or_default();
    let mut observation_error = None;
    let network = unsafe {
        hermit::network_container::run_with_network_startup_hooked(
            container,
            STARTUP,
            accepted_launch,
            prepared,
            &mut grouped_hook,
            &mut |owner, deadline| {
                debugger.release_parent();
                let observed = accepted
                    .as_mut()
                    .ok_or_else(|| std::io::Error::other("unexpected accepted startup owner"))
                    .and_then(|startup| startup.observe(owner, deadline));
                observed.map_err(|error| {
                    observation_error = Some(
                        Error::new(error)
                            .context("accepted provider owner capture before STARTUP_READY"),
                    );
                    StartupError::Protocol
                })
            },
            &mut |mut resource, control, mut guard_owner| {
                let mut runtime_owner = None;
                let mut alarm = None;
                let result = arm_container_init_guards()
                    .and_then(|()| {
                        if let Some(limit) = timeout {
                            let after = limit
                                .checked_add(super::run_timeout::RUN_TIMEOUT_UNWIND_GRACE)
                                .ok_or_else(|| anyhow::anyhow!("--timeout grace overflow"))?;
                            alarm = Some(super::run_timeout::RunTimeoutFallback::arm(after)?);
                        }
                        Ok(())
                    })
                    .and_then(|()| {
                        catch_child_panic_at(site, &mut || {
                            if use_accepted && resource.is_none() {
                                return Err(Error::msg("missing authenticated accepted startup"));
                            }
                            if !use_guard {
                                if control.is_some() || guard_owner.is_some() {
                                    return Err(Error::msg(
                                        "accepted-only Record received foreign guard custody",
                                    ));
                                }
                                return execute(resource.take(), debugger.take_child());
                            }
                            let owner = guard_owner
                                .as_deref_mut()
                                .ok_or_else(|| Error::msg("missing actual guard recovery owner"))?;
                            let control = control
                                .as_ref()
                                .ok_or_else(|| Error::msg("missing authenticated guard resource"))?
                                .clone();
                            let resource = match resource.take() {
                                Some(resource) => {
                                    resource.attach_authenticated_guard(
                                        control,
                                        owner.startup_deadline(),
                                    )?;
                                    resource
                                }
                                None => {
                                    let (retained, resource) =
                                        NetworkRuntimeResources::from_authenticated_guard(
                                            control,
                                            owner.startup_deadline(),
                                        );
                                    runtime_owner = Some(retained);
                                    resource
                                }
                            };
                            let result = execute(Some(resource), debugger.take_child());
                            let prior = match &result {
                                Err(error) => owner
                                    .retain_backend_failure(NetworkGuardBackendFailure::from_error(
                                        error.as_ref(),
                                    ))
                                    .err(),
                                Ok(value) => {
                                    NetworkGuardBackendFailure::from_exit_status(value.status())
                                        .and_then(|failure| {
                                            owner.retain_backend_failure(failure).err()
                                        })
                                }
                            };
                            let stopped = owner.stop_and_join(Instant::now() + OBSERVER_STOP);
                            if let Some(prior) = prior {
                                return Err(Error::msg(format!(
                                    "guard selected primary before backend return: {prior:?}"
                                )));
                            }
                            if result.is_err()
                                || result.as_ref().is_ok_and(|value| !value.status().success())
                            {
                                if let Err(error) = stopped {
                                    eprintln!("hermit: secondary Unix observer cleanup: {error}");
                                }
                                return result;
                            }
                            stopped?;
                            result
                        })
                    });
                let unresolved = result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.cleanup_stage().is_some());
                if result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.kind() == hermit::FailureKind::RunTimeout)
                {
                    super::run_timeout::stall_the_unwind_if_asked();
                }
                let exit = super::owned_container::PublishedFailureExit::new(unresolved, alarm);
                let result: Wire = result.map_err(SerializableError::from);
                (result, (runtime_owner, exit))
            },
        )
    };
    // The observer may not run on a startup failure. Close the parent alias
    // before any outside finalization or debugger-watch result is selected.
    debugger.release_parent();
    let (container_result, guard, parent, startup_error) = match network {
        Ok(network) => (Some(network.container), network.guard, network.parent, None),
        Err(failure) => (
            None,
            failure.guard,
            None,
            Some(Error::msg(format!(
                "network before-clone startup failed: {:?}: {:?}",
                failure.cause, failure.guard_error
            ))),
        ),
    };
    let mut primary = startup_error.or(observation_error);
    if let Some(startup) = accepted.as_mut() {
        // Provider startup precedes owner-observer failure; retain that original cause.
        if let Some(error) = startup.attach(parent) {
            primary = Some(error);
        }
    } else {
        assert!(
            parent.is_none(),
            "guard-only startup cannot launch an accepted provider"
        );
    }
    let guard = match (guard_context, guard) {
        (Some((package, stdout, stderr, evidence, incarnation)), Some(owner)) => {
            Some(GuardFinalization {
                owner: GuardParentFinalizer::new(owner, &package, stdout.into(), stderr.into()),
                evidence,
                incarnation,
            })
        }
        (Some(_), None) => {
            primary.get_or_insert_with(|| refusal("startup lost its guard owner"));
            None
        }
        (None, None) => None,
        (None, Some(_)) => unreachable!("accepted-only startup cannot launch a guard"),
    };
    finalize_owned(
        container_result,
        guard,
        accepted,
        primary,
        failed_backing,
    )
}

/// The exact child terminal precedes both service drains: the provider lifetime
/// is the held controller PIDFD. A successful value remains private until every
/// required outside owner has complete physical drain evidence.
fn finalize_owned(
    container_result: Option<
        Result<reverie::process::OwnedDeferredContainerRun<Wire>, StartupOwnedFailure<Wire>>,
    >,
    mut guard: Option<GuardFinalization>,
    mut accepted: Option<AcceptedStartup>,
    mut primary: Option<Error>,
    mut failed_backing: Option<Box<dyn Any>>,
) -> Result<RunValue, Error> {
    let deadline = Instant::now() + TERMINAL;
    let mut child_cleanup = None;
    let mut child_pid = None;
    let mut child_status = None;
    let mut encoded = None;
    let mut missing_abort = None;
    let mut classify_actual_missing_terminal = false;
    let mut child_terminal = container_result.is_none();
    if let Some(result) = container_result {
        let finalized = match result {
            Ok(child) => {
                child_pid = Some(child.cleanup().child_pid().as_raw());
                Some(child.finalize_until(deadline))
            }
            Err(StartupOwnedFailure::BeforeClone { cause }) => {
                primary
                    .get_or_insert_with(|| Error::msg(format!("network clone failed: {cause:?}")));
                child_terminal = true;
                None
            }
            Err(StartupOwnedFailure::AfterClone { cause, run }) => {
                child_pid = Some(run.cleanup().child_pid().as_raw());
                missing_abort = MissingResultAbort::observe(
                    cause,
                    run.cleanup().observation(),
                    primary.is_some(),
                );
                classify_actual_missing_terminal = primary.is_none()
                    && cause == OwnedRunFailure::Startup(StartupError::MissingResult);
                primary.get_or_insert_with(|| owned_failure(cause));
                Some(run.retry_until(deadline))
            }
        };
        match finalized {
            Some(OwnedFinalize::Complete(child)) => {
                child_status = Some(child.status());
                child_terminal = true;
                encoded = Some(child);
            }
            Some(OwnedFinalize::Failed { cause, cleanup }) => {
                if primary.is_none() {
                    missing_abort =
                        MissingResultAbort::observe(cause, cleanup.cleanup().observation(), false);
                    classify_actual_missing_terminal =
                        cause == OwnedRunFailure::Startup(StartupError::MissingResult);
                }
                if let ChildCleanupObservation::Reaped(status) = cleanup.cleanup().observation() {
                    child_status = Some(status);
                    child_terminal = true;
                }
                primary.get_or_insert_with(|| owned_failure(cause));
                child_cleanup = Some(cleanup);
            }
            Some(OwnedFinalize::Pending(cleanup)) => {
                primary.get_or_insert_with(|| {
                    Error::msg("actual network child terminal remains pending")
                });
                child_cleanup = Some(cleanup);
            }
            None => {}
        }
    }
    if !child_terminal {
        if let Some(guard) = guard.as_mut() {
            let _ = emit(
                &mut guard.evidence,
                serde_json::json!({"schema":1,"stage":"child_terminal_unknown","child_pid":child_pid}),
            );
        }
        retain(
            child_cleanup,
            guard.map(|guard| guard.owner),
            accepted,
            failed_backing.take(),
        );
        return Err(primary.unwrap_or_else(|| Error::msg("network child terminal unknown")));
    }
    if classify_actual_missing_terminal {
        if let Some(status) = child_status {
            if let Some(actual) = classify_missing_terminal(status) {
                primary = Some(match primary.take() {
                    Some(error) => actual.context(error.to_string()),
                    None => actual,
                });
            }
        }
    }
    let mut outcome = match primary {
        Some(error) => UnpublishedChildResult(Err(error)),
        None => match encoded {
            Some(child) => UnpublishedChildResult::decode(child),
            None => UnpublishedChildResult(Err(Error::msg("missing reaped network child result"))),
        },
    };
    // Drain both even if one fails. Never let an error skip the other original
    // owner's cleanup, or publish on the strength of just one certificate.
    let accepted_result = accepted.as_mut().map_or(Ok(()), |startup| {
        let proof = match startup
            .owner
            .as_mut()
            .expect("original accepted owner")
            .drain(deadline)
        {
            Ok(proof) => proof,
            Err(error) => {
                let _ = startup.receipts.failure(&format!(
                    "accepted terminal: {error}; child_wait={:?}",
                    child_status.map(ExitStatus::into_raw)
                ));
                return Err(Error::new(error).context("accepted exact terminal cleanup"));
            }
        };
        startup.receipts.finish(proof).map_err(Error::from)
    });
    let drain = match guard.as_mut() {
        None => accepted_result,
        Some(context) => match context.owner.drain(deadline) {
            Ok(proof) => {
                let emitted = emit(
                    &mut context.evidence,
                    proof_json(proof, child_pid, child_status),
                )
                .map_err(Error::from);
                if emitted.is_ok() {
                    outcome = outcome.classify_missing_abort(missing_abort, |candidate| {
                        candidate.certify(context.incarnation, proof)
                    });
                }
                let drain = combine_terminal_results(accepted_result, emitted);
                if drain.is_ok() {
                    // Keep the borrowed certificate inside its successful drain
                    // branch. Failed cleanup must read the owner's current
                    // recovery units, after drain has attempted every terminal.
                    return outcome.publish(Ok(()), |value| {
                        GuestCompletion::certify(value, proof)?.publish()
                    });
                }
                drain
            }
            Err(error) => {
                let _ = emit(
                    &mut context.evidence,
                    serde_json::json!({"schema":1,"stage":"terminal_failed","error":error.to_string(),"backend_failure":outcome.failure_description(),"child_pid":child_pid,"child_wait":child_status.map(|s|format!("{s:?}")),"ids":context.owner.guard().original_ids(),"units":context.owner.recovery_units()}),
                );
                combine_terminal_results(
                    accepted_result,
                    Err(Error::msg(format!(
                        "Unix aggregate terminal failed: {error}"
                    ))),
                )
            }
        },
    };
    if drain.is_err() {
        // Successful proof references cannot authorize this failed aggregate.
        retain(
            child_cleanup,
            guard.map(|guard| guard.owner),
            accepted,
            failed_backing.take(),
        );
        return outcome.publish(drain, |_| unreachable!("failed drain cannot publish"));
    }
    outcome.publish(Ok(()), Ok)
}

/// No wire result can certify policy. Preserve actual deadline/signal/crash
/// classes, while reserved policy status 122 still needs the full guard proof.
fn classify_missing_terminal(status: ExitStatus) -> Option<Error> {
    if status.success() || status.code() == Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT) {
        return None;
    }
    Some(
        classify_container_result::<RunValue>(Err(RunError::ExitStatus(status)))
            .expect_err("non-success physical child status cannot publish a result"),
    )
}

fn combine_terminal_results(
    accepted: Result<(), Error>,
    guard: Result<(), Error>,
) -> Result<(), Error> {
    match (accepted, guard) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(secondary)) => {
            Err(primary.context(format!("Unix aggregate cleanup: {secondary:#}")))
        }
    }
}

fn owned_failure(cause: OwnedRunFailure) -> Error {
    match cause {
        OwnedRunFailure::ChildStatus(status) => {
            classify_container_result::<RunValue>(Err(RunError::ExitStatus(status))).unwrap_err()
        }
        cause => Error::msg(format!("owned network child failed: {cause:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::container::catch_child_panic;
    use super::*;

    #[test]
    fn ftrace_provider_never_constructs_tracefs_grouped_parent() {
        use detcore::network_runtime::ProviderTopology;
        assert!(!requires_grouped_parent(&ProviderTopology::ClassicV40));
        assert!(!requires_grouped_parent(&ProviderTopology::FtraceV1 {
            contract_sha256: [8; 32],
        }));
        assert!(requires_grouped_parent(&ProviderTopology::GroupedV1 {
            contract_sha256: [9; 32],
        }));
    }

    #[test]
    fn combined_terminal_failure_withholds_success_and_retains_both_errors() {
        for (accepted, guard, expected) in [
            (
                Err(Error::msg("provider unresolved")),
                Ok(()),
                "provider unresolved",
            ),
            (
                Ok(()),
                Err(Error::msg("guard unresolved")),
                "guard unresolved",
            ),
            (
                Err(Error::msg("provider unresolved")),
                Err(Error::msg("guard unresolved")),
                "provider unresolved",
            ),
        ] {
            let drain = combine_terminal_results(accepted, guard);
            let error = UnpublishedChildResult(Ok(RunValue::Run(ExitStatus::Exited(0), None)))
                .publish(drain, |_| {
                    panic!("partial service drain cannot certify data")
                })
                .unwrap_err();
            assert_eq!(error.root_cause().to_string(), expected);
        }
        let both = combine_terminal_results(
            Err(Error::msg("provider unresolved")),
            Err(Error::msg("guard unresolved")),
        )
        .unwrap_err();
        assert!(format!("{both:#}").contains("guard unresolved"));
        assert!(combine_terminal_results(Ok(()), Ok(())).is_ok());
    }

    #[test]
    fn actual_owned_deadline_keeps_timeout_class_without_certifying_policy() {
        let started = Container::new().run_with_startup_owned(
            Duration::from_secs(2),
            &mut |_| Ok(()),
            &mut |_| Ok(()),
            &mut |_| -> (Wire, ()) { unsafe { libc::_exit(124) } },
        );
        let error = finalize_owned(Some(started), None, None, None, None).unwrap_err();
        assert!(
            error
                .downcast_ref::<super::super::container::RunTimeoutMarker>()
                .is_some(),
            "{error:#}"
        );
        assert!(error.downcast_ref::<PolicyRefusal>().is_none());
        assert!(classify_missing_terminal(ExitStatus::Exited(122)).is_none());
        assert!(classify_missing_terminal(ExitStatus::Exited(0)).is_none());
    }

    #[test]
    fn controller_debugger_handoff_retires_parent_alias_and_keeps_child_listener() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let debugger = ControllerDebugger(RefCell::new(Some(listener)));
        // Queue a real client before clone; accepting never depends on a test thread.
        let client =
            std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1)).unwrap();
        let started = Container::new()
            .run_with_startup_owned(
                Duration::from_secs(2),
                &mut |_| {
                    debugger.release_parent();
                    Ok(())
                },
                &mut |_| Ok(()),
                &mut |_| {
                    let listener = debugger
                        .take_child()
                        .expect("child owns its actual fork alias");
                    assert_eq!(listener.local_addr().unwrap(), address);
                    listener.set_nonblocking(true).unwrap();
                    let accepted = listener.accept().is_ok();
                    drop(listener);
                    (accepted, ())
                },
            )
            .expect("actual owned clone startup");
        assert!(
            debugger.take_child().is_none(),
            "parent retained its listener alias"
        );
        let OwnedFinalize::Complete(child) =
            started.finalize_until(Instant::now() + Duration::from_secs(2))
        else {
            panic!("actual debugger handoff child did not terminate");
        };
        assert!(
            child.decode().unwrap(),
            "child could not accept the prequeued debugger client"
        );
        drop(client);
    }

    fn actual_missing_child(
        code: Option<i32>,
        earlier_failure: bool,
    ) -> (UnpublishedChildResult, Option<MissingResultAbort>) {
        let started = Container::new().run_with_startup_owned(
            Duration::from_secs(2),
            &mut |_| Ok(()),
            &mut |_| Ok(()),
            &mut |_| -> (Wire, ()) {
                unsafe {
                    if let Some(code) = code {
                        libc::_exit(code);
                    }
                    libc::raise(libc::SIGKILL);
                    libc::_exit(125);
                }
            },
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let finalized = match started {
            Ok(run) => run.finalize_until(deadline),
            Err(StartupOwnedFailure::AfterClone { cause, run }) => {
                assert_eq!(cause, OwnedRunFailure::Startup(StartupError::MissingResult));
                run.retry_until(deadline)
            }
            Err(error) => panic!("actual child must reach result acquisition: {error:?}"),
        };
        let OwnedFinalize::Failed { cause, cleanup } = finalized else {
            panic!("actual missing wire must fail after physical child settlement");
        };
        assert_eq!(cause, OwnedRunFailure::Startup(StartupError::MissingResult));
        let observed = cleanup.cleanup().observation();
        assert!(matches!(observed, ChildCleanupObservation::Reaped(_)));
        let candidate = MissingResultAbort::observe(cause, observed, earlier_failure);
        (UnpublishedChildResult(Err(owned_failure(cause))), candidate)
    }
    // Value predicates only: no GuardReadbackCertificate or physical drain
    // authority is constructed. The armed gate must still qualify that path.
    fn policy_facts() -> PolicyAbortFacts {
        use hermit::unix_guard::GuardBirth;
        use hermit::unix_guard::GuardDenial;
        use hermit::unix_guard::GuardEvidence;
        use hermit::unix_guard::GuardProvisionalReceipt;
        let mut evidence = GuardEvidence::default();
        evidence.denial = GuardDenial {
            incarnation: 17,
            phase: 2,
            task_start: 1,
            pid_tgid: (12 << 32) | 12,
            hook: 1,
            reason: 3,
            ..Default::default()
        };
        PolicyAbortFacts {
            birth: Some(GuardBirth {
                incarnation: 17,
                sequence: 4,
                object: 9,
                generation: 1,
                cookie: 2,
            }),
            terminal: GuardProvisionalReceipt {
                incarnation: 17,
                proof_sequence: 7,
                reply_sequence: 7,
                record_ordinal: 42,
                removed_links: 31,
                removed_map_pins: 10,
                initial_tasks: 1,
                outcome: GuardOutcome::Policy(evidence),
            },
            counts: [10, 31, 31],
            ids: [(0, 10), (1, 31), (2, 31)]
                .into_iter()
                .flat_map(|(kind, count)| (1..=count).map(move |id| (kind, id)))
                .collect(),
            readback: [17, 7, 45, 100, 1100, 72, 300, 2],
            actor_waits: [0, 0],
            terminal_observed_ns: 400,
        }
    }
    fn controlled_policy(facts: &PolicyAbortFacts, expected: u64) -> Result<Error, Error> {
        facts.validate(expected)?;
        Ok(Error::new(PolicyRefusal).context("controlled committed network-disabled policy"))
    }
    #[test]
    fn actual_missing_122_requires_policy_certificate_before_classification() {
        let (outcome, candidate) = actual_missing_child(Some(122), false);
        assert!(candidate.is_some());
        let error = outcome
            .classify_missing_abort(candidate, |_| controlled_policy(&policy_facts(), 17))
            .publish(Ok(()), |_| panic!("missing wire cannot publish data"))
            .unwrap_err();
        assert!(error.downcast_ref::<PolicyRefusal>().is_some());
    }
    #[test]
    fn actual_missing_125_zero_crash_and_prior_failure_cannot_claim_policy() {
        for (code, prior) in [
            (Some(125), false),
            (Some(0), false),
            (None, false),
            (Some(122), true),
        ] {
            let (outcome, candidate) = actual_missing_child(code, prior);
            assert!(candidate.is_none());
            let error = outcome
                .classify_missing_abort(candidate, |_| {
                    panic!("unrelated exit cannot certify policy")
                })
                .publish(Ok(()), |_| panic!("missing result cannot publish data"))
                .unwrap_err();
            assert!(error.to_string().contains("MissingResult"));
            assert!(error.downcast_ref::<PolicyRefusal>().is_none());
        }
        assert!(
            MissingResultAbort::observe(
                OwnedRunFailure::Cancelled,
                ChildCleanupObservation::Reaped(ExitStatus::Exited(122)),
                false
            )
            .is_none()
        );
    }
    #[test]
    fn actual_missing_122_keeps_primary_when_cleanup_or_policy_proof_fails() {
        let (outcome, _candidate) = actual_missing_child(Some(122), false);
        let error = outcome
            .publish(Err(Error::msg("original drain failed")), |_| {
                panic!("failed drain cannot certify")
            })
            .unwrap_err();
        assert!(error.to_string().contains("MissingResult"));
        let (outcome, candidate) = actual_missing_child(Some(122), false);
        let error = outcome
            .classify_missing_abort(candidate, |_| controlled_policy(&policy_facts(), 18))
            .publish(Ok(()), |_| panic!("missing result cannot publish data"))
            .unwrap_err();
        assert!(error.to_string().contains("MissingResult"));
        assert!(error.downcast_ref::<PolicyRefusal>().is_none());
    }
    #[test]
    fn policy_abort_predicates_reject_unproven_or_mismatched_terminal_facts() {
        let baseline = policy_facts();
        baseline.validate(17).unwrap();
        let controls: &[fn(&mut PolicyAbortFacts)] = &[
            |f| f.birth = None,
            |f| f.birth.as_mut().unwrap().incarnation = 18,
            |f| f.terminal.incarnation = 18,
            |f| f.terminal.initial_tasks = 0,
            |f| f.terminal.outcome = GuardOutcome::Pending,
            |f| f.terminal.outcome = GuardOutcome::Running,
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.phase = 1;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.incarnation = 18;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.task_start = 0;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.pid_tgid = 12;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.reason = 4;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.reason = 0;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.hook = 0;
                }
            },
            |f| {
                if let GuardOutcome::Policy(e) = &mut f.terminal.outcome {
                    e.denial.hook = 20;
                }
            },
            |f| f.counts = [0, 0, 0],
            |f| f.ids[1] = f.ids[0],
            |f| {
                f.ids.pop();
            },
            |f| f.readback[0] = 18,
            |f| f.readback[1] = 6,
            |f| f.readback[2] = 41,
            |f| f.readback[4] = 1_000_000_101,
            |f| f.readback[5] = 71,
            |f| f.readback[7] = 1,
            |f| f.terminal_observed_ns = f.readback[4],
            |f| f.actor_waits[0] = 1,
            |f| f.actor_waits[1] = 1,
        ];
        for (index, control) in controls.iter().enumerate() {
            let mut facts = baseline.clone();
            control(&mut facts);
            assert!(
                facts.validate(17).is_err(),
                "unproved predicate control{index}"
            );
        }
        assert!(baseline.validate(0).is_err());
        assert!(baseline.validate(18).is_err());
    }

    fn actual_completed_child(mut body: impl FnMut() -> Wire) -> OwnedReapedResult<Wire> {
        let child = Container::new()
            .run_with_startup_owned(
                Duration::from_secs(2),
                &mut |_| Ok(()),
                &mut |_| Ok(()),
                &mut |_| (body(), ()),
            )
            .unwrap();
        let OwnedFinalize::Complete(child) =
            child.finalize_until(Instant::now() + Duration::from_secs(2))
        else {
            panic!("actual child must settle before decoding its result");
        };
        child
    }

    #[test]
    fn actual_serialized_child_panic_keeps_primary_without_positive_enrollment() {
        let child = actual_completed_child(|| {
            catch_child_panic(&mut || panic!("original controller startup panic"))
        });
        let error = UnpublishedChildResult::decode(child)
            .publish(Ok(()), |_| {
                panic!("failed child cannot require positive enrollment")
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("original controller startup panic"));
    }

    #[test]
    fn actual_nonzero_guest_result_requires_and_keeps_positive_certificate() {
        let child = actual_completed_child(|| Ok(RunValue::Run(ExitStatus::Exited(17), None)));
        let mut certified = false;
        let result = UnpublishedChildResult::decode(child)
            .publish(Ok(()), |value| {
                certified = true;
                Ok(value)
            })
            .unwrap();
        assert!(certified);
        assert_eq!(result.status(), ExitStatus::Exited(17));
    }

    #[test]
    fn actual_failed_child_remains_primary_when_external_drain_fails() {
        for panic in [true, false] {
            let child = actual_completed_child(|| {
                if panic {
                    catch_child_panic(&mut || panic!("original failure before external drain"))
                } else {
                    Ok(RunValue::Run(ExitStatus::Exited(17), None))
                }
            });
            let retained = UnpublishedChildResult::decode(child);
            assert!(
                retained.failure_description().is_some(),
                "classify before external drain"
            );
            let result = retained.publish(Err(Error::msg("later external drain failure")), |_| {
                panic!("failed drain cannot mint a success certificate")
            });
            if panic {
                let error = result.unwrap_err();
                assert!(format!("{error:#}").contains("original failure before external drain"));
                assert!(!format!("{error:#}").contains("later external drain failure"));
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    error
                        .downcast_ref::<UnpublishedGuestFailure>()
                        .unwrap()
                        .status(),
                    ExitStatus::Exited(17)
                );
                assert_eq!(super::super::failure_exit_code(&error), 17);
                assert!(
                    super::super::classify_failure(&error)
                        .contains("guard=cleanup result=withheld")
                );
            }
        }
    }

    #[test]
    fn actual_successful_child_cannot_publish_when_external_drain_fails() {
        let child = actual_completed_child(|| Ok(RunValue::Run(ExitStatus::SUCCESS, None)));
        let retained = UnpublishedChildResult::decode(child);
        let error = retained
            .publish(Err(Error::msg("external drain unresolved")), |_| {
                panic!("failed drain cannot enter positive certification")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "external drain unresolved");
    }

    #[test]
    fn actual_nonzero_verify_output_is_withheld_when_external_drain_fails() {
        let child = actual_completed_child(|| {
            Ok(RunValue::Verify(
                Output {
                    status: ExitStatus::Exited(17),
                    stdout: b"must not reach comparator".to_vec(),
                    stderr: vec![],
                },
                0,
            ))
        });
        let error = UnpublishedChildResult::decode(child)
            .publish(Err(Error::msg("external drain unresolved")), |_| {
                panic!("unresolved cleanup cannot certify verification data")
            })
            .unwrap_err();
        assert_eq!(super::super::failure_exit_code(&error), 17);
        assert!(error.downcast_ref::<UnpublishedGuestFailure>().is_some());
    }

    #[test]
    fn actual_nonzero_verify_output_is_withheld_when_certificate_is_refused() {
        let child = actual_completed_child(|| {
            Ok(RunValue::Verify(
                Output {
                    status: ExitStatus::Exited(17),
                    stdout: b"withheld".to_vec(),
                    stderr: vec![],
                },
                0,
            ))
        });
        let mut certified = false;
        let error = UnpublishedChildResult::decode(child)
            .publish(Ok(()), |_| {
                certified = true;
                Err(Error::new(PolicyRefusal).context("network disabled by final guard policy"))
            })
            .unwrap_err();
        assert!(certified);
        assert_eq!(super::super::failure_exit_code(&error), 17);
        assert!(
            super::super::classify_failure(&error).contains("guard=certificate result=withheld")
        );
        assert!(
            error
                .to_string()
                .contains("network disabled by final guard policy")
        );
        assert!(!error.to_string().contains("cleanup is unresolved"));
    }

    #[test]
    fn actual_nonzero_verify_output_requires_successful_certificate() {
        let child = actual_completed_child(|| {
            Ok(RunValue::Verify(
                Output {
                    status: ExitStatus::Exited(17),
                    stdout: b"certified".to_vec(),
                    stderr: vec![],
                },
                0,
            ))
        });
        let mut certified = false;
        let result = UnpublishedChildResult::decode(child)
            .publish(Ok(()), |value| {
                certified = true;
                Ok(value)
            })
            .unwrap();
        assert!(certified);
        let RunValue::Verify(output, 0) = result else {
            panic!("expected certified verification output")
        };
        assert_eq!(output.status, ExitStatus::Exited(17));
        assert_eq!(output.stdout, b"certified");
    }

    #[test]
    fn actual_successful_child_cannot_bypass_positive_certificate() {
        let child = actual_completed_child(|| Ok(RunValue::Run(ExitStatus::SUCCESS, None)));
        let mut checked = false;
        let error = UnpublishedChildResult::decode(child)
            .publish(Ok(()), |_| {
                checked = true;
                Err(Error::msg("missing positive guard certificate"))
            })
            .unwrap_err();
        assert!(checked);
        assert_eq!(error.to_string(), "missing positive guard certificate");
    }
}

#[cfg(test)]
mod deployment_tests {
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Options {
        #[command(flatten)]
        deployment: DeploymentOpts,
    }
    #[test]
    fn deployment_roots_require_a_pair_and_preserve_exact_paths() {
        assert!(Options::try_parse_from(["test", "--network-guard-bpffs", "/pins"]).is_err());
        assert!(
            Options::try_parse_from(["test", "--network-guard-recovery", "/recovery"]).is_err()
        );
        let parsed = Options::try_parse_from([
            "test",
            "--network-guard-bpffs",
            "/private/pins",
            "--network-guard-recovery",
            "/private/recovery",
        ])
        .unwrap();
        assert_eq!(
            parsed.deployment.roots().unwrap(),
            Some((Path::new("/private/pins"), Path::new("/private/recovery")))
        );
        assert_eq!(
            Options::try_parse_from(["test"])
                .unwrap()
                .deployment
                .roots()
                .unwrap(),
            None
        );
        assert!(deployment_roots(Some(Path::new("/pins")), None).is_err());
        assert!(deployment_roots(None, Some(Path::new("/recovery"))).is_err());
    }
    #[test]
    fn all_record_replay_front_doors_parse_the_same_deployment_pair() {
        for (front, guest) in [
            (vec!["hermit", "record"], true),
            (vec!["hermit", "record", "start"], true),
            (vec!["hermit", "replay", "--autopilot"], false),
            (vec!["hermit", "run", "--record-networking", "/trace"], true),
            (vec!["hermit", "run", "--replay-networking", "/trace"], true),
        ] {
            for pair in [true, false] {
                let mut args = front.clone();
                args.extend(["--network-guard-bpffs", "/private/pins"]);
                if pair {
                    args.extend(["--network-guard-recovery", "/private/recovery"]);
                }
                if guest {
                    args.extend(["--", "/bin/true"]);
                }
                let parsed = crate::Args::try_parse_from(&args);
                if pair {
                    assert!(parsed.is_ok(), "{args:?}: {parsed:?}");
                } else {
                    assert_eq!(
                        parsed.unwrap_err().kind(),
                        clap::error::ErrorKind::MissingRequiredArgument,
                        "{args:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn explicit_record_recovery_authenticates_only_its_owned_directory() {
        let root = tempfile::tempdir().unwrap();
        let missing_bpffs = root.path().join("absent-pins");
        let recovery = root.path().join("recovery");
        std::fs::create_dir(&recovery).unwrap();
        std::fs::set_permissions(&recovery, std::fs::Permissions::from_mode(0o700)).unwrap();
        let held = accepted_recovery_root(Some((&missing_bpffs, &recovery))).unwrap();
        assert_eq!(held.writable_path, recovery);
        assert!(!missing_bpffs.exists());
        // Replay still needs the real, separately authenticated bpffs root.
        assert!(GuardDeploymentRoots::open_at(&missing_bpffs, &recovery).is_err());
        std::fs::set_permissions(&recovery, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(accepted_recovery_root(Some((&missing_bpffs, &recovery))).is_err());
    }
}

#[cfg(test)]
mod accepted_root_routing_tests {
    use clap::Parser;

    use super::*;
    #[derive(Debug, Parser)]
    struct Options {
        #[command(flatten)]
        deployment: DeploymentOpts,
    }
    #[test]
    fn accepted_root_propagates_separately_and_explicit_replay_refuses_missing_root() {
        let parsed = Options::try_parse_from([
            "test",
            "--network-guard-bpffs",
            "/pins",
            "--network-guard-recovery",
            "/guard",
            "--network-accepted-recovery",
            "/accepted",
        ])
        .unwrap();
        assert_eq!(
            parsed.deployment.roots().unwrap(),
            Some((Path::new("/pins"), Path::new("/guard")))
        );
        assert_eq!(
            parsed.deployment.accepted_root(),
            Some(Path::new("/accepted"))
        );
        let roots = parsed.deployment.roots().unwrap();
        assert!(check_recovery_routing(roots, None, true, true).is_err());
        assert!(
            check_recovery_routing(roots, parsed.deployment.accepted_root(), true, true).is_ok()
        );
        assert!(check_recovery_routing(roots, None, false, true).is_ok());
        assert!(check_recovery_routing(None, None, true, true).is_ok());
        assert!(
            check_recovery_routing(roots, parsed.deployment.accepted_root(), true, false).is_err()
        );
    }
    #[test]
    fn accepted_and_guard_options_parse_at_each_actual_record_replay_front_door() {
        for (front, guest) in [
            (vec!["hermit", "record"], true),
            (vec!["hermit", "record", "start"], true),
            (vec!["hermit", "replay", "--autopilot"], false),
            (vec!["hermit", "run", "--record-networking", "/trace"], true),
            (vec!["hermit", "run", "--replay-networking", "/trace"], true),
        ] {
            let mut args = front;
            args.extend([
                "--network-guard-bpffs",
                "/pins",
                "--network-guard-recovery",
                "/guard",
                "--network-accepted-recovery",
                "/accepted",
            ]);
            if guest {
                args.extend(["--", "/bin/true"]);
            }
            assert!(crate::Args::try_parse_from(&args).is_ok(), "{args:?}");
        }
    }
}
