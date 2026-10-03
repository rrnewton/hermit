//! A shared wait retains the original Call and deadline while one positively
//! completed attempt gives up its current grant. Native pin custody is separate.
use super::*;
use crate::network_runtime::shared_waits::SharedAttemptAdmission;
use crate::resources::NetworkWaitKind;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

#[path = "shared_waits/replay_store.rs"]
mod replay_store;
pub(crate) use replay_store::SharedNoStoreResult;
pub(crate) use replay_store::SharedReplayBytesPlan;
pub(crate) use replay_store::SharedReplayNoStorePlan;
pub(crate) use replay_store::SharedReplayReceivePlan;
pub(crate) use replay_store::SharedReplaySource;

#[path = "shared_waits/poll.rs"]
mod poll;
pub(crate) use poll::{SharedPollDecision, SharedPollPlan, SharedPollSource, SharedPollStoreRetainer};

#[path = "shared_waits/record_probe.rs"]
mod record_probe;
pub(crate) use record_probe::PreparedSharedEffect;
pub(crate) use record_probe::SharedEffectIdentity;
pub(crate) use record_probe::SharedProbeProgress;
pub(crate) use record_probe::SharedRecordPollPublication;
pub(crate) use record_probe::SharedRecordPollSource;
pub(crate) use record_probe::SharedRecordProbe;

#[path = "shared_waits/capture.rs"]
mod capture;
pub(crate) use capture::SharedCaptureOrigin;
pub(crate) use capture::SharedCaptureSubmission;

#[derive(Debug, Clone)]
pub(crate) struct OriginalPollIntent {
    raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
    rows: Vec<(crate::types::FdSlotBinding, i16)>,
    started: LogicalTime,
    deadline: Option<LogicalTime>,
}
impl OriginalPollIntent {
    /// These are immutable operands, not memory/entry authority. The actual
    /// Global issuer still authenticates the original array and native tuple.
    pub(crate) fn new(
        raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
        rows: Vec<(crate::types::FdSlotBinding, i16)>,
        started: LogicalTime,
        deadline: Option<LogicalTime>,
    ) -> Result<Self, NetworkReplayError> {
        if !matches!(
            raw.0,
            reverie::syscalls::Sysno::poll | reverie::syscalls::Sysno::ppoll
        ) || rows.is_empty()
            || rows.len() > 1024
            || raw.1.arg1 != rows.len()
            || deadline.is_some_and(|end| end < started)
            || rows
                .iter()
                .any(|(_, events)| !detcore_model::network_trace::valid_tcp_poll_mask(*events))
            || rows
                .iter()
                .any(|(binding, _)| binding.open_file != rows[0].0.open_file)
        {
            return Err(invalid(
                "shared Poll changed its finite original row/timeout profile",
            ));
        }
        Ok(Self {
            raw,
            rows,
            started,
            deadline,
        })
    }
    pub(crate) fn raw(&self) -> (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs) {
        self.raw
    }
    fn accepts(&self, kind: NetworkWaitKind) -> bool {
        self.rows.iter().any(|(_, events)| match kind {
            NetworkWaitKind::PollReadable => events & (libc::POLLIN | libc::POLLRDNORM) != 0,
            NetworkWaitKind::ReceiveHalfClosed => events & libc::POLLRDHUP != 0,
            NetworkWaitKind::Writable => events & (libc::POLLOUT | libc::POLLWRNORM) != 0,
            NetworkWaitKind::Terminal => {
                events
                    & (libc::POLLIN
                        | libc::POLLRDNORM
                        | libc::POLLOUT
                        | libc::POLLWRNORM
                        | libc::POLLRDHUP)
                    == 0
            }
            _ => false,
        })
    }
    fn ready(&self, raw: i16) -> bool {
        self.rows
            .iter()
            .any(|(_, events)| detcore_model::network_trace::tcp_poll_row_mask(raw, *events) != 0)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SharedWaitIntent {
    Receive(Arc<crate::tool_global::SavedReceivePolicy>),
    Poll(Arc<OriginalPollIntent>),
}
impl SharedWaitIntent {
    fn deadline(&self) -> Option<LogicalTime> {
        match self {
            Self::Receive(p) => p.deadline(),
            Self::Poll(p) => p.deadline,
        }
    }
    fn started(&self) -> LogicalTime {
        match self {
            Self::Receive(p) => p.started(),
            Self::Poll(p) => p.started,
        }
    }
    fn interests(&self) -> Vec<NetworkWaitKind> {
        match self {
            Self::Receive(p) => vec![NetworkWaitKind::ReadableAtLeast(p.target())],
            Self::Poll(_) => [
                NetworkWaitKind::PollReadable,
                NetworkWaitKind::ReceiveHalfClosed,
                NetworkWaitKind::Writable,
                NetworkWaitKind::Terminal,
            ]
            .into_iter()
            .filter(|kind| self.accepts(*kind))
            .collect(),
        }
    }
    fn accepts(&self, kind: NetworkWaitKind) -> bool {
        match self {
            Self::Receive(p) => {
                !p.nonblocking() && kind == NetworkWaitKind::ReadableAtLeast(p.target())
            }
            Self::Poll(p) => p.accepts(kind),
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::network_replay) struct SharedWait {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    binding: crate::types::FdSlotBinding,
    intent: SharedWaitIntent,
    phase: AttemptPhase,
    output: Option<replay_store::SharedOutput>,
    poll_output: Option<poll::SharedPollOutput>,
    capture: Option<Arc<SharedCaptureOrigin>>,
    record_probe: Option<record_probe::ProbeState>,
    record_history: Vec<Arc<record_probe::RecordHistory>>,
}
#[derive(Debug, Clone)]
enum AttemptPhase {
    Active {
        ordinal: u64,
        epoch: u64,
        entry: AttemptEntry,
        completion: Option<Arc<Completion>>,
    },
    Suspended {
        completed: Arc<Completion>,
    },
}
#[derive(Debug, Clone)]
enum AttemptEntry {
    Record(NetworkReleaseV4),
    Replay { began: LogicalTime },
}
impl AttemptEntry {
    fn began(&self) -> LogicalTime {
        match self {
            Self::Record(release) => release.not_before_global_time,
            Self::Replay { began } => *began,
        }
    }
}
#[derive(Debug)]
struct Completion {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    ordinal: u64,
    epoch: u64,
    file: OpenFileId,
    observed_at: LogicalTime,
    observation: Observation,
}
#[derive(Debug, PartialEq, Eq)]
enum Observation {
    Record(Arc<record_probe::RecordHistory>),
    ReplayReceive {
        consumed: u64,
        generation: Option<u64>,
        control: u64,
        available: usize,
    },
    ReplayPoll {
        consumed: u64,
        sample: Option<(LogicalTime, i16)>,
    },
}
/// Private issuer, non-Clone: only an exact completed observation may suspend.
#[derive(Debug)]
pub(crate) struct CompletedSharedAttempt(Arc<Completion>);

#[derive(Debug, Clone)]
pub(crate) struct SharedCallIdentity {
    pub(crate) call: NetworkStreamCallId,
    pub(crate) owner: NetworkStreamOwner,
    pub(crate) file: OpenFileId,
    pub(crate) root: Arc<crate::network_runtime::ForegroundRoot>,
    pub(crate) native: Option<crate::network_runtime::original_installation::FileIdentity>,
    ordinal: u64,
    epoch: u64,
    suspended: bool,
}
impl SharedCallIdentity {
    fn same(&self, other: &Self) -> bool {
        self.call == other.call
            && self.owner == other.owner
            && self.file == other.file
            && Arc::ptr_eq(&self.root, &other.root)
            && self.native == other.native
            && self.ordinal == other.ordinal
            && self.epoch == other.epoch
            && self.suspended == other.suspended
    }
}
#[derive(Debug, Clone)]
pub(crate) struct SharedCallCensus {
    pub(crate) rows: Vec<SharedCallIdentity>,
    controls: Vec<(NetworkStreamOwner, NetworkStreamLeaseId)>,
}
impl SharedCallCensus {
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.same_rows(other) && self.controls == other.controls
    }
    pub(crate) fn same_rows(&self, other: &Self) -> bool {
        self.rows.len() == other.rows.len()
            && self.rows.iter().zip(&other.rows).all(|(a, b)| a.same(b))
    }
}

pub(crate) struct CallWaitBinding {
    pub(crate) open_file: OpenFileId,
    /// Poll uses an actual eligible full-mask trace observation, not writable
    /// readiness or a fabricated zero. Receive uses existing threshold logic.
    pub(crate) observed_ready: Option<bool>,
}

impl NetworkReplayEngine {
    fn shared_wait_capture_pending_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<&StreamCallState, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        };
        if !self.uses_shared_mm_attempts()
            || self.mode() != NetworkEngineMode::Record
            || state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || !state.physical_pin_required
            || state.phase != StreamCallPhase::PinAcquireSubmitted
            || state.open_file != Some(wait.binding.open_file)
            || state.capture_publication.is_none()
            || state.capture_control.is_none()
            || state.original.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.replay_receive.is_some()
            || state.record_no_store.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.replay_connect.is_some()
            || !wait.root.is_current(owner)
            || !wait.root.has_shared_mm_history()
            || !matches!(
                wait.phase,
                AttemptPhase::Active {
                    ordinal: 0,
                    completion: None,
                    ..
                }
            )
        {
            return Err(invalid(
                "shared capture confirmation changed untouched original pin acquisition",
            ));
        }
        Ok(state)
    }

    fn shared_wait(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(&StreamCallState, &SharedWait), NetworkReplayError> {
        self.check_stream_owner(owner)?;
        self.check_native_retirement()?;
        if !self.uses_shared_mm_attempts() || !self.fd_table_capability() {
            return Err(invalid("shared wait lost declared policy/table custody"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        };
        if state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.phase != StreamCallPhase::Active
            || state.open_file != Some(wait.binding.open_file)
            || !wait.root.is_current(owner)
            || !wait.root.has_shared_mm_history()
        {
            return Err(invalid("shared wait changed original live Call/root"));
        }
        if let SharedWaitIntent::Receive(policy) = &wait.intent
            && (!policy.matches_shared(owner, call, wait.binding.open_file)
                || !Arc::ptr_eq(policy.root(), &wait.root)
                || state
                    .receive_policy
                    .as_ref()
                    .is_none_or(|actual| !Arc::ptr_eq(actual, policy)))
        {
            return Err(invalid("shared receive changed its saved policy owner"));
        }
        Ok((state, wait))
    }

    fn shared_wait_debts_settled(
        &self,
        call: NetworkStreamCallId,
        state: &StreamCallState,
    ) -> Result<(), NetworkReplayError> {
        self.shared_wait_non_output_debts_settled(call, state)?;
        if matches!(&state.shared_attempt, Some(SharedAttempt::Wait(wait)) if wait.output.is_some() || wait.poll_output.is_some())
        {
            return Err(invalid("shared wait retains its exact output attempt"));
        }
        Ok(())
    }

    fn shared_wait_non_output_debts_settled(
        &self,
        call: NetworkStreamCallId,
        state: &StreamCallState,
    ) -> Result<(), NetworkReplayError> {
        if matches!(&state.shared_attempt, Some(SharedAttempt::Wait(w)) if w.record_probe.is_some())
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || state.original.is_some()
            || state.helper_copy.is_some()
            || !self.shared_record_history_covers(state)
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.no_store_completed
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.replay_connect.is_some()
            || self.shadow_probes.values().any(|p| p.call == call)
            || self.zero_stream_waits.values().any(|w| w.call == call)
        {
            return Err(invalid(
                "shared wait still owns unresolved capture/source/store/cursor debt",
            ));
        }
        Ok(())
    }

    /// Complete existing Call census, never supplied by numeric caller IDs.
    /// One selected Active attempt may be inspected; every peer must already
    /// own a positive Suspended predecessor. A native lifetime pin may remain.
    pub(crate) fn shared_call_census(
        &self,
        selected: Option<NetworkStreamCallId>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.shared_call_census_excluding(selected, None)
    }
    pub(crate) fn shared_call_census_excluding(
        &self,
        selected: Option<NetworkStreamCallId>,
        exclude: Option<NetworkStreamCallId>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.shared_call_census_except_delivery(selected, exclude, None)
    }
    fn shared_call_census_except_delivery(
        &self,
        selected: Option<NetworkStreamCallId>,
        exclude: Option<NetworkStreamCallId>,
        delivery: Option<NetworkStreamLeaseId>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.shared_call_census_with_record_probe(selected, exclude, delivery, None)
    }
    fn shared_call_census_with_record_probe(
        &self,
        selected: Option<NetworkStreamCallId>,
        exclude: Option<NetworkStreamCallId>,
        delivery: Option<NetworkStreamLeaseId>,
        record: Option<&SharedRecordProbe>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        if !self.uses_shared_mm_attempts() || !self.fd_table_capability() {
            return Err(invalid("shared census lacks declared policy/table"));
        }
        self.check_native_retirement()?;
        if self
            .stream_operations
            .keys()
            .any(|lease| Some(*lease) != delivery)
            || self.shadow_probes.iter().any(|(lease, probe)| {
                record.is_none_or(|r| *lease != r.lease() || probe.call != r.call())
            })
            || self.stream_delivery.iter().any(|(file, lease)| {
                Some(*lease) != delivery
                    || self
                        .stream_operations
                        .get(lease)
                        .is_none_or(|operation| operation.open_file != *file)
            })
            || !self.shadow_deliveries.is_empty()
        {
            return Err(invalid(
                "shared census retains operation/control/delivery debt",
            ));
        }
        let mut rows = Vec::new();
        for (&call, state) in &self.stream_calls {
            if Some(call) == exclude {
                continue;
            }
            let (_, wait) = self.shared_wait(state.owner, call)?;
            self.shared_wait_debts_settled(call, state)?;
            let (ordinal, epoch, suspended) = match &wait.phase {
                AttemptPhase::Active { ordinal, epoch, .. } if selected == Some(call) => {
                    (*ordinal, *epoch, false)
                }
                AttemptPhase::Suspended { completed } => (completed.ordinal, completed.epoch, true),
                _ => return Err(invalid("shared census contains another active attempt")),
            };
            let native = if state.physical_pin_required {
                Some(
                    self.shadow
                        .as_ref()
                        .and_then(|s| s.sockets.get(&wait.binding.open_file))
                        .and_then(|s| s.native.as_ref())
                        .ok_or_else(|| {
                            invalid("shared native Call lost installed file provenance")
                        })?
                        .identity,
                )
            } else {
                None
            };
            rows.push(SharedCallIdentity {
                call,
                owner: state.owner,
                file: wait.binding.open_file,
                root: wait.root.clone(),
                native,
                ordinal,
                epoch,
                suspended,
            });
        }
        if selected.is_some_and(|id| !rows.iter().any(|r| r.call == id)) {
            return Err(invalid("shared census selected an absent Call"));
        }
        Ok(SharedCallCensus {
            rows,
            controls: self
                .socket_controls
                .values()
                .map(|c| (c.owner, c.lease))
                .collect(),
        })
    }

    pub(super) fn shared_census_matches_grant(
        &self,
        selected: Option<NetworkStreamCallId>,
        exclude: Option<NetworkStreamCallId>,
        grant: &SharedMmForegroundObservation<'_>,
    ) -> Result<bool, NetworkReplayError> {
        Ok(self
            .shared_call_census_excluding(selected, exclude)?
            .rows
            .iter()
            .all(|row| grant.contains_root(&row.root)))
    }

    pub(crate) fn preflight_shared_wait_begin(
        &self,
        read: &NetworkFdReadAdmission,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
    ) -> Result<(), NetworkReplayError> {
        self.validate_fd_read(grant.owner(), read)?;
        let binding = read
            .binding
            .ok_or_else(|| invalid("shared wait lacks original selected FD"))?;
        if read.external_grant.is_some()
            || binding.slot.files != grant.root().files()
            || !admission.matches_peers(self, None)?
            || !self.shared_census_matches_grant(None, None, grant)?
            || self
                .stream_calls
                .values()
                .any(|state| state.owner == grant.owner())
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !grant.root().has_shared_mm_history()
            || self.socket_controls.len() != 1
            || self
                .socket_controls
                .get(&binding.open_file)
                .is_none_or(|c| c.owner != grant.owner() || Some(c.lease) != read.control)
        {
            return Err(invalid("shared wait changed selected read/grant/runtime"));
        }
        Ok(())
    }

    /// Global creates policy from the actual transferred Call inside the same
    /// engine transaction. Any refusal here leaves that Call owned by Global.
    pub(crate) fn attach_shared_wait_call(
        &mut self,
        call: NetworkStreamCallId,
        binding: crate::types::FdSlotBinding,
        intent: SharedWaitIntent,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let owner = grant.owner();
        self.check_stream_owner(owner)?;
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if !self.uses_shared_mm_attempts()
            || state.owner != owner
            || state.open_file != Some(binding.open_file)
            || state.shared_attempt.is_some()
            || state.abandoned
            || state.final_wait
            || state.original.is_some()
            || state.replay_connect.is_some()
            || !admission.matches_peers(self, Some(call))?
            || !self.shared_census_matches_grant(None, Some(call), grant)?
            || state.terminal_evidence.is_some()
            || state.receive_policy.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.no_store_completed
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || match self.mode() {
                NetworkEngineMode::Replay => {
                    state.phase != StreamCallPhase::Active
                        || state.physical_pin_required
                        || state.capture_publication.is_some()
                        || state.capture_control.is_some()
                }
                NetworkEngineMode::Record => {
                    state.phase != StreamCallPhase::PinAcquireSubmitted
                        || !state.physical_pin_required
                        || state.capture_publication.is_none()
                        || state.capture_control.is_none()
                }
            }
            || !Arc::ptr_eq(admission.root(), grant.root())
            || binding.slot.files != grant.root().files()
            || now < intent.started()
            || !grant.root().has_shared_mm_history()
        {
            return Err(invalid(
                "shared wait attachment changed transferred Call/intent",
            ));
        }
        match &intent {
            SharedWaitIntent::Receive(p)
                if p.matches_shared(owner, call, binding.open_file)
                    && Arc::ptr_eq(p.root(), grant.root())
                    && p.target() != 0
                    && p.raw().1.arg0 as i32 == binding.slot.fd => {}
            SharedWaitIntent::Poll(p)
                if p.rows.iter().all(|(b, _)| {
                    b.open_file == binding.open_file && b.slot.files == binding.slot.files
                }) => {}
            _ => {
                return Err(invalid(
                    "shared wait does not match original syscall policy",
                ));
            }
        }
        let entry = self.shared_wait_entry(now)?;
        let state = self.stream_calls.get_mut(&call).unwrap();
        if let SharedWaitIntent::Receive(p) = &intent {
            state.receive_policy = Some(p.clone());
        }
        state.shared_attempt = Some(SharedAttempt::Wait(SharedWait {
            root: grant.root().clone(),
            binding,
            intent,
            output: None,
            poll_output: None,
            capture: None,
            record_probe: None,
            record_history: Vec::new(),
            phase: AttemptPhase::Active {
                ordinal: 0,
                epoch: grant.epoch(),
                entry,
                completion: None,
            },
        }));
        Ok(())
    }

    fn shared_wait_entry(&self, now: LogicalTime) -> Result<AttemptEntry, NetworkReplayError> {
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if now < native.trace.epoch_global_time()? {
            return Err(invalid("shared attempt precedes original epoch"));
        }
        if native.replay.is_some() {
            return Ok(AttemptEntry::Replay { began: now });
        }
        let cut = NetworkReceiveEntryCutV4(
            u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?,
        );
        Ok(AttemptEntry::Record(NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: native
                .trace
                .entry_frontier(cut)
                .map_err(|e| invalid(&e.to_string()))?,
        }))
    }

    fn shared_active(
        &self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
    ) -> Result<&SharedWait, NetworkReplayError> {
        let (_, wait) = self.shared_wait(grant.owner(), call)?;
        if !Arc::ptr_eq(&wait.root, grant.root())
            || !grant.contains_root(&wait.root)
            || !matches!(wait.phase, AttemptPhase::Active { epoch, .. } if epoch == grant.epoch())
        {
            return Err(invalid(
                "shared attempt changed original current Normal grant",
            ));
        }
        Ok(wait)
    }

    fn replay_shared_wait_observation(
        &self,
        call: NetworkStreamCallId,
        wait: &SharedWait,
        now: LogicalTime,
    ) -> Result<Observation, NetworkReplayError> {
        if self.mode() != NetworkEngineMode::Replay {
            return Err(NetworkReplayError::WrongMode);
        }
        let file = wait.binding.open_file;
        let channel = self.bound_channel(file)?;
        let queue = &self.channels[&channel];
        if wait.intent.deadline().is_some_and(|d| now >= d) {
            return Err(invalid(
                "deadline requires a fresh final result, not suspension",
            ));
        }
        match &wait.intent {
            SharedWaitIntent::Receive(p) => {
                if p.nonblocking()
                    || queue.local_read_shutdown
                    || queue.peer_write_closed
                    || queue.inbound.iter().any(|i| {
                        !matches!(
                            i,
                            InboundOutcome::Stream {
                                ancillary: None,
                                message_flags: 0,
                                requires_message_io: false,
                                ..
                            }
                        )
                    })
                {
                    return Err(invalid(
                        "shared receive wait lacks a nonterminal ordinary prefix",
                    ));
                }
                let available = super::super::replay_store::plain_prefix(
                    queue,
                    p.raw().1.arg2.min(NETWORK_STREAM_CHUNK_LIMIT),
                )?
                .len();
                if available >= p.target() {
                    return Err(invalid("shared receive already satisfies its saved target"));
                }
                Ok(Observation::ReplayReceive {
                    consumed: queue.inbound_consumed,
                    generation: queue.receive_input_generation,
                    control: queue.local_control_generation,
                    available,
                })
            }
            SharedWaitIntent::Poll(p) => {
                let sample = self.shared_poll_sample(file)?;
                if sample.is_some_and(|(_, raw)| p.ready(raw)) {
                    return Err(invalid("shared Poll has a ready raw observation"));
                }
                let _ = call;
                Ok(Observation::ReplayPoll {
                    consumed: queue.inbound_consumed,
                    sample,
                })
            }
        }
    }

    /// A pure Replay observation does not consume bytes or certify native IO.
    /// Record must obtain a distinct issuer from its actual retired helper.
    pub(crate) fn complete_shared_replay_wait_observation(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<CompletedSharedAttempt, NetworkReplayError> {
        {
            let wait = self.shared_active(call, grant)?;
            let AttemptPhase::Active {
                entry,
                completion: None,
                ..
            } = &wait.phase
            else {
                return Err(invalid("shared observation already completed"));
            };
            if self.mode() != NetworkEngineMode::Replay {
                return Err(NetworkReplayError::WrongMode);
            }
            if now < entry.began() {
                return Err(invalid("shared observation moved before attempt"));
            }
            self.shared_wait_debts_settled(call, &self.stream_calls[&call])?;
        }
        // Pending is a checked Replay frontier, never an invented native zero.
        // Establish due input availability here rather than relying on a
        // caller's earlier maintenance pass, then inspect the refreshed state.
        self.release_native_eligible(now)?;
        let wait = self.shared_active(call, grant)?;
        let AttemptPhase::Active {
            ordinal,
            epoch,
            completion: None,
            ..
        } = &wait.phase
        else {
            return Err(invalid("shared observation changed during trace release"));
        };
        let completion = Arc::new(Completion {
            owner: grant.owner(),
            call,
            ordinal: *ordinal,
            epoch: *epoch,
            file: wait.binding.open_file,
            observed_at: now,
            observation: self.replay_shared_wait_observation(call, wait, now)?,
        });
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        let AttemptPhase::Active {
            completion: slot, ..
        } = &mut wait.phase
        else {
            unreachable!()
        };
        *slot = Some(completion.clone());
        Ok(CompletedSharedAttempt(completion))
    }

    pub(crate) fn suspend_shared_wait(
        &mut self,
        completed: CompletedSharedAttempt,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
    ) -> Result<(), NetworkReplayError> {
        let c = &completed.0;
        let wait = self.shared_active(c.call, grant)?;
        if c.owner != grant.owner()
            || c.file != wait.binding.open_file
            || c.epoch != grant.epoch()
            || !matches!(&wait.phase, AttemptPhase::Active { ordinal, completion: Some(actual), .. } if *ordinal == c.ordinal && Arc::ptr_eq(actual,c))
            || !self.socket_controls.is_empty()
            || !admission.matches_selected(self, c.call)?
            || !self.shared_census_matches_grant(Some(c.call), None, grant)?
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !self.shared_completed_observation_matches(c.call, wait, c)?
        {
            return Err(invalid(
                "shared suspension changed exact completed attempt/census",
            ));
        }
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&c.call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.phase = AttemptPhase::Suspended {
            completed: completed.0,
        };
        Ok(())
    }

    pub(crate) fn resume_shared_wait(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<u64, NetworkReplayError> {
        let (_, wait) = self.shared_wait(grant.owner(), call)?;
        let AttemptPhase::Suspended { completed } = &wait.phase else {
            return Err(invalid(
                "shared resume lacks positively completed predecessor",
            ));
        };
        if !Arc::ptr_eq(&wait.root, grant.root())
            || !grant.contains_root(&wait.root)
            || grant.epoch() <= completed.epoch
            || now < completed.observed_at
            || !self.socket_controls.is_empty()
            || !admission.matches_selected(self, call)?
            || !self.shared_census_matches_grant(Some(call), None, grant)?
            || !Arc::ptr_eq(admission.root(), grant.root())
        {
            return Err(invalid(
                "shared resume changed original Call/root or reused old grant",
            ));
        }
        let ordinal = completed
            .ordinal
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let entry = self.shared_wait_entry(now)?;
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.phase = AttemptPhase::Active {
            ordinal,
            epoch: grant.epoch(),
            entry,
            completion: None,
        };
        Ok(ordinal)
    }

    /// Both wait admission and maintenance check the whole original set.
    /// Checking each allowed member alone would silently accept a missing mask.
    pub(crate) fn validate_call_wait_set(
        &self,
        owner: NetworkStreamOwner,
        interests: &[(NetworkStreamCallId, NetworkWaitKind)],
        deadline: Option<LogicalTime>,
    ) -> Result<(), NetworkReplayError> {
        let mut checked = BTreeSet::new();
        for &(call, _) in interests {
            if !checked.insert(call)
                || self
                    .stream_calls
                    .get(&call)
                    .is_none_or(|s| s.shared_attempt.is_none())
            {
                continue;
            }
            let (_, wait) = self.shared_wait(owner, call)?;
            if !matches!(wait.phase, AttemptPhase::Suspended { .. })
                || wait.intent.deadline() != deadline
            {
                return Err(invalid(
                    "shared wait set changed suspended original deadline",
                ));
            }
            let expected = wait.intent.interests();
            let actual: Vec<_> = interests
                .iter()
                .filter_map(|(id, kind)| (*id == call).then_some(*kind))
                .collect();
            if actual.is_empty()
                || actual.iter().any(|kind| !expected.contains(kind))
                || expected.iter().any(|kind| !actual.contains(kind))
            {
                return Err(invalid(
                    "shared wait set omitted or added original interests",
                ));
            }
        }
        if self.stream_calls.iter().any(|(id, state)| {
            state.owner == owner && state.shared_attempt.is_some() && !checked.contains(id)
        }) {
            return Err(invalid("shared wait set omitted its original Call"));
        }
        Ok(())
    }

    pub(crate) fn shared_wait_interests(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<Vec<NetworkWaitKind>, NetworkReplayError> {
        let (_, wait) = self.shared_wait(owner, call)?;
        if !matches!(wait.phase, AttemptPhase::Suspended { .. }) {
            return Err(invalid("active shared attempt cannot park"));
        }
        Ok(wait.intent.interests())
    }

    pub(crate) fn call_wait_binding(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        kind: NetworkWaitKind,
        deadline: Option<LogicalTime>,
    ) -> Result<CallWaitBinding, NetworkReplayError> {
        if self
            .stream_calls
            .get(&call)
            .is_none_or(|s| s.shared_attempt.is_none())
        {
            return self
                .stream_call_open_file(owner, call)
                .map(|open_file| CallWaitBinding {
                    open_file,
                    observed_ready: None,
                });
        }
        let (_, wait) = self.shared_wait(owner, call)?;
        if !matches!(wait.phase, AttemptPhase::Suspended { .. })
            || wait.intent.deadline() != deadline
            || !wait.intent.accepts(kind)
        {
            return Err(invalid(
                "shared wait changed original suspended interest/deadline",
            ));
        }
        let observed_ready = match &wait.intent {
            SharedWaitIntent::Receive(_) => None,
            SharedWaitIntent::Poll(_) if self.mode() == NetworkEngineMode::Record => {
                let AttemptPhase::Suspended { completed } = &wait.phase else {
                    unreachable!("suspended phase checked above")
                };
                self.shared_wait_debts_settled(call, &self.stream_calls[&call])?;
                self.validate_stream_call_lifetime(owner, call, wait.binding.open_file)?;
                if completed.owner != owner
                    || completed.call != call
                    || completed.file != wait.binding.open_file
                    || !matches!(completed.observation, Observation::Record(_))
                    || !self.shared_completed_observation_matches(call, wait, completed)?
                {
                    return Err(invalid(
                        "shared Record Poll lost its completed pending publication",
                    ));
                }
                // This is the original positively pending attempt, not a fresh
                // readiness observation. Generic modeled bytes or a historical
                // mask cannot provide a new schedule-changing wake source.
                // Fresh Record retry/wake requires its own replayed evidence.
                Some(false)
            }
            SharedWaitIntent::Poll(p) => Some(
                self.shared_poll_sample(wait.binding.open_file)?
                    .is_some_and(|(_, raw)| p.ready(raw)),
            ),
        };
        Ok(CallWaitBinding {
            open_file: wait.binding.open_file,
            observed_ready,
        })
    }

    pub(super) fn shared_poll_sample(
        &self,
        file: OpenFileId,
    ) -> Result<Option<(LogicalTime, i16)>, NetworkReplayError> {
        let channel = self.bound_channel(file)?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let replay = native
            .replay
            .as_ref()
            .ok_or(NetworkReplayError::WrongMode)?;
        let queue = &self.channels[&channel];
        let low_water = self
            .shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(file))?
            .options
            .receive_low_water;
        replay
            .poll
            .shared_observation_at(
                channel,
                queue.inbound_consumed,
                queue.local_control_generation,
                low_water,
            )
            .map(|sample| sample.map(|s| (s.observed_at, s.revents)))
    }
}

#[cfg(test)]
#[path = "shared_waits/tests.rs"]
mod tests;
