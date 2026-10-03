//! Shared Record receive output preserves its actual Peek source through the
//! one guest store and one same-file Consume successor. Publication is separate.
use detcore_model::network_trace::NetworkNativeCopyDispositionV4;
use detcore_model::network_trace::NetworkNativeCopyFragmentV4;
use detcore_model::network_trace::NetworkNativeReceiveObservationV4;

use super::*;
use crate::network_replay::native_receive::no_store::NoStoreReturn;
use crate::network_replay::native_receive::no_store::RecordNoStore;
use crate::network_replay::native_receive::private_peek::PrivateSource;
use crate::network_runtime::HelperCopyBinding;
use crate::network_runtime::HelperCopyCompletion;
use crate::network_runtime::native_peer::Observation as NativeObservation;
use crate::network_runtime::shared_waits::ConfirmedSharedRecordReceive;
use crate::tool_global::SharedRecordStoreAttempt;

#[derive(Debug, Clone)]
pub(super) enum ReceiveSource {
    Bytes(PrivateSource),
    Empty(Arc<RecordNoStore>),
}
impl ReceiveSource {
    fn completion(&self) -> &HelperCopyCompletion {
        match self {
            Self::Bytes(s) => &s.completion,
            Self::Empty(s) => s.completion(),
        }
    }
}

/// Read-only snapshot issued by the exact current probe validator. Its private
/// source owns canonical bytes, not an arbitrary caller buffer or readiness bit.
#[derive(Debug, Clone)]
pub(super) struct RecordReceiveSnapshot {
    pub(super) origin: Arc<SharedRecordProbe>,
    pub(super) binding: crate::types::FdSlotBinding,
    pub(super) policy: Arc<crate::tool_global::SavedReceivePolicy>,
    pub(super) physical: crate::network_replay::native_receive::Cut,
    pub(super) consumed: u64,
    pub(super) ordinal: u64,
    pub(super) epoch: u64,
    pub(super) entry: NetworkReleaseV4,
    pub(super) control: u64,
    pub(super) low_water: u32,
    pub(super) record: record_probe::ProbeState,
    pub(super) probe: ShadowProbeState,
    pub(super) source: ReceiveSource,
    pub(super) at: LogicalTime,
}
impl RecordReceiveSnapshot {
    fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.origin, &other.origin)
            && self.binding == other.binding
            && Arc::ptr_eq(&self.policy, &other.policy)
            && self.physical == other.physical
            && self.consumed == other.consumed
            && self.ordinal == other.ordinal
            && self.epoch == other.epoch
            && self.entry == other.entry
            && self.control == other.control
            && self.low_water == other.low_water
            && self.at == other.at
            && self.source.completion() == other.source.completion()
            && self.record.receive_effects().len() == other.record.receive_effects().len()
            && self
                .record
                .receive_effects()
                .iter()
                .zip(other.record.receive_effects())
                .all(|(a, b)| Arc::ptr_eq(a, b))
    }
}
#[derive(Debug)]
pub(crate) enum SharedRecordReceivePlan {
    Bytes(SharedRecordBytesPlan),
    NoStore(SharedRecordNoStorePlan),
}
#[derive(Debug)]
pub(crate) struct SharedRecordBytesPlan {
    selected: RecordReceiveSnapshot,
    length: usize,
}
#[derive(Debug)]
pub(crate) struct SharedRecordNoStorePlan {
    selected: RecordReceiveSnapshot,
    outcome: SharedNoStoreResult,
}
impl SharedRecordNoStorePlan {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        &self.selected.origin
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.origin().owner()
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.origin().call()
    }
}
#[derive(Debug)]
pub(crate) struct SharedRecordReceiveSource {
    selected: RecordReceiveSnapshot,
    lease: NetworkStreamLeaseId,
    bytes: Vec<u8>,
    next_consume_epoch: u64,
}
impl SharedRecordReceiveSource {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        &self.selected.origin
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.origin().owner()
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.origin().call()
    }
    pub(crate) fn open_file(&self) -> OpenFileId {
        self.selected.binding.open_file
    }
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        self.origin().root()
    }
    pub(crate) fn raw(&self) -> (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs) {
        self.policy().raw()
    }
    pub(crate) fn policy(&self) -> &Arc<crate::tool_global::SavedReceivePolicy> {
        &self.selected.policy
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.selected.epoch
    }
    pub(crate) fn predecessor(&self) -> &HelperCopyCompletion {
        self.selected.source.completion()
    }
}
#[derive(Debug)]
pub(crate) struct SharedRecordStored {
    source: Arc<SharedRecordReceiveSource>,
}
impl SharedRecordStored {
    pub(crate) fn source(&self) -> &Arc<SharedRecordReceiveSource> {
        &self.source
    }
}
#[derive(Debug, Clone)]
pub(super) struct SharedRecordReceiveOutput {
    source: Arc<SharedRecordReceiveSource>,
    attempts: Vec<Arc<SharedRecordStoreAttempt>>,
    stored: bool,
    drain: Option<SharedRecordDrainState>,
    prepared: Option<Arc<PreparedSharedRecordReceivePublication>>,
    publication: Option<RecordReceivePublication>,
}
#[derive(Debug, Clone)]
struct SharedRecordDrainState {
    submission: Arc<SharedRecordDrainSubmission>,
    binding: Option<Arc<HelperCopyBinding>>,
    observed: Option<NativeObservation>,
    joined: Option<Arc<crate::network_runtime::shared_waits::JoinedSharedDrain>>,
    matched: bool,
}
#[derive(Debug)]
pub(crate) struct PreparedSharedRecordReceivePublication {
    source: Arc<SharedRecordReceiveSource>,
    joined: Arc<crate::network_runtime::shared_waits::JoinedSharedDrain>,
    at: LogicalTime,
}
impl PreparedSharedRecordReceivePublication {
    pub(crate) fn source(&self) -> &Arc<SharedRecordReceiveSource> {
        &self.source
    }
    pub(crate) fn joined(&self) -> &Arc<crate::network_runtime::shared_waits::JoinedSharedDrain> {
        &self.joined
    }
}

/// Holds the existing output exclusively across the synchronous native writer.
/// Append precedes every post-effect comparison; a mismatch remains evidence.
#[derive(Debug)]
pub(crate) struct SharedRecordStoreRetainer<'a> {
    output: &'a mut SharedRecordReceiveOutput,
}
impl SharedRecordStoreRetainer<'_> {
    pub(crate) fn retain(
        &mut self,
        attempt: SharedRecordStoreAttempt,
    ) -> Result<(), NetworkReplayError> {
        let same = Arc::ptr_eq(attempt.source(), &self.output.source);
        self.output.attempts.push(Arc::new(attempt));
        if !same || self.output.attempts.len() != 1 {
            return Err(invalid(
                "actual shared Record store changed source or repeated; effects retained",
            ));
        }
        Ok(())
    }
}

impl NetworkReplayEngine {
    pub(crate) fn plan_shared_record_receive(
        &self,
        origin: &Arc<SharedRecordProbe>,
        grant: &SharedMmForegroundObservation<'_>,
        proof: &ConfirmedSharedRecordReceive<'_>,
        now: LogicalTime,
    ) -> Result<SharedRecordReceivePlan, NetworkReplayError> {
        if !Arc::ptr_eq(origin, proof.origin()) {
            return Err(invalid("Record receive changed native source borrower"));
        }
        let s = self.shared_record_receive_snapshot(origin, grant, now)?;
        let raw = s.policy.raw();
        if !matches!(
            raw.0,
            reverie::syscalls::Sysno::read | reverie::syscalls::Sysno::recvfrom
        ) || raw.1.arg2 == 0
            || raw.1.arg2 > 512
            || s.policy.target() == 0
            || s.policy.target() > raw.1.arg2
            || (raw.0 == reverie::syscalls::Sysno::recvfrom
                && (raw.1.arg3 != 0 || raw.1.arg4 != 0 || raw.1.arg5 != 0))
            || !s
                .policy
                .matches_shared(origin.owner(), origin.call(), s.binding.open_file)
            || !Arc::ptr_eq(s.policy.root(), origin.root())
        {
            return Err(invalid(
                "shared Record receive changed original finite scalar policy",
            ));
        }
        match &s.source {
            ReceiveSource::Bytes(source) => {
                let length = raw.1.arg2.min(source.length);
                if length == 0
                    || (length < s.policy.target()
                        && !s.policy.nonblocking()
                        && !s.policy.expired(now))
                {
                    return Err(invalid(
                        "shared Record positive source has not met its original target/deadline",
                    ));
                }
                Ok(SharedRecordReceivePlan::Bytes(SharedRecordBytesPlan {
                    selected: s,
                    length,
                }))
            }
            ReceiveSource::Empty(source) => {
                let outcome = match source.kind() {
                    NoStoreReturn::Eof => SharedNoStoreResult::Eof,
                    NoStoreReturn::WouldBlock if s.policy.nonblocking() => {
                        SharedNoStoreResult::WouldBlock { timed_out: false }
                    }
                    NoStoreReturn::WouldBlock
                        if s.policy.expired(now)
                            && s.policy
                                .deadline()
                                .is_some_and(|end| s.entry.not_before_global_time >= end) =>
                    {
                        SharedNoStoreResult::WouldBlock { timed_out: true }
                    }
                    _ => {
                        return Err(invalid(
                            "shared Record empty timeout requires its fresh final attempt",
                        ));
                    }
                };
                Ok(SharedRecordReceivePlan::NoStore(SharedRecordNoStorePlan {
                    selected: s,
                    outcome,
                }))
            }
        }
    }

    pub(crate) fn reserve_shared_record_receive(
        &mut self,
        plan: SharedRecordBytesPlan,
        grant: &SharedMmForegroundObservation<'_>,
        proof: &ConfirmedSharedRecordReceive<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedRecordReceiveSource>, NetworkReplayError> {
        let SharedRecordReceivePlan::Bytes(current) =
            self.plan_shared_record_receive(&plan.selected.origin, grant, proof, now)?
        else {
            return Err(invalid("Record byte reservation changed source class"));
        };
        if !plan.selected.same(&current.selected) || plan.length != current.length {
            return Err(invalid(
                "Record byte reservation changed original source selection",
            ));
        }
        let p = &plan.selected;
        let file = p.binding.open_file;
        let socket = &self.shadow.as_ref().unwrap().sockets[&file];
        let queue = &self.channels[&p.probe.channel];
        self.validate_stream_call_lifetime(p.origin.owner(), p.origin.call(), file)?;
        if self.stream_delivery.contains_key(&file)
            || queue.inbound_consumed != p.physical.bytes
            || p.consumed != p.physical.bytes
            || !queue.inbound.is_empty()
            || queue.published_ingress.unwrap_or_default().stream_offset != p.physical.bytes
            || queue.published_ingress.unwrap_or_default().terminal
            || queue.peer_write_closed
        {
            return Err(invalid(
                "Record receive reservation lacks unpublished exact physical prefix",
            ));
        }
        let next_consume_epoch = socket
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        p.physical
            .bytes
            .checked_add(plan.length as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let bytes = p.source.completion().capture().committed[..plan.length].to_vec();
        let cursor_before = socket.options.peek_offset;
        let lease = self.allocate_stream_lease()?;
        let source = Arc::new(SharedRecordReceiveSource {
            selected: plan.selected,
            lease,
            bytes,
            next_consume_epoch,
        });
        let p = &source.selected;
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner: source.owner(),
                open_file: file,
                channel: p.probe.channel,
                abandoned: false,
                kind: StreamOperationKind::Delivery {
                    at_offset: p.physical.bytes,
                    peek_offset: 0,
                    selection_len: source.len(),
                    outcome: NetworkStreamChunkOutcome::Bytes(source.bytes.clone()),
                },
            },
        );
        self.stream_delivery.insert(file, lease);
        self.shadow_deliveries.insert(
            lease,
            ShadowDeliveryState {
                call: source.call(),
                private_offset: Some(0),
                selected_len: source.len(),
                cursor_before,
                next_consume_epoch,
                drain_started: false,
                drained: 0,
                peek_cursor_confirmed: false,
                pending: None,
            },
        );
        self.shadow_probes
            .remove(&source.origin().lease())
            .expect("exact original probe preflighted");
        self.socket_controls
            .remove(&file)
            .expect("exact unchanged control preflighted");
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        wait.record_probe
            .take()
            .expect("original source retained in snapshot");
        wait.record_receive = Some(SharedRecordReceiveOutput {
            source: source.clone(),
            attempts: Vec::new(),
            stored: false,
            drain: None,
            prepared: None,
            publication: None,
        });
        Ok(source)
    }

    pub(super) fn check_shared_record_receive_source(
        &self,
        source: &Arc<SharedRecordReceiveSource>,
    ) -> Result<(), NetworkReplayError> {
        let (state, wait) = self.shared_wait(source.owner(), source.call())?;
        let p = &source.selected;
        let output = wait
            .record_receive
            .as_ref()
            .filter(|o| Arc::ptr_eq(&o.source, source))
            .ok_or_else(|| invalid("Record output lost original Call source"))?;
        let AttemptPhase::Active {
            ordinal,
            epoch,
            entry: AttemptEntry::Record(entry),
            completion: None,
        } = &wait.phase
        else {
            return Err(invalid("Record output changed active attempt"));
        };
        let operation = self
            .stream_operations
            .get(&source.lease)
            .ok_or_else(|| invalid("Record output lost Delivery"))?;
        let delivery = self
            .shadow_deliveries
            .get(&source.lease)
            .ok_or_else(|| invalid("Record output lost physical delivery"))?;
        let channel = &self.channels[&p.probe.channel];
        let socket = &self
            .shadow
            .as_ref()
            .ok_or(NetworkReplayError::WrongMode)?
            .sockets[&source.open_file()];
        self.validate_stream_call_lifetime(source.owner(), source.call(), source.open_file())?;
        if self.mode() != NetworkEngineMode::Record
            || *ordinal != p.ordinal
            || *epoch != p.epoch
            || entry != &p.entry
            || !state.physical_pin_required
            || wait.record_probe.is_some()
            || wait.output.is_some()
            || wait.poll_output.is_some()
            || wait.binding != p.binding
            || !Arc::ptr_eq(&wait.root, source.root())
            || !matches!(&wait.intent,SharedWaitIntent::Receive(policy) if Arc::ptr_eq(policy,&p.policy))
            || state.capture_publication.is_some()
            || state.capture_control.is_some()
            || state.original.is_some()
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.no_store_completed
            || state.replay_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.replay_connect.is_some()
            || operation.owner != source.owner()
            || operation.open_file != source.open_file()
            || operation.channel != p.probe.channel
            || operation.abandoned
            || self.stream_delivery.get(&source.open_file()) != Some(&source.lease)
            || !matches!(&operation.kind,StreamOperationKind::Delivery{at_offset,peek_offset:0,selection_len,outcome:NetworkStreamChunkOutcome::Bytes(bytes)} if *at_offset==p.physical.bytes && *selection_len==source.len() && bytes==source.bytes())
            || delivery.call != source.call()
            || delivery.private_offset != Some(0)
            || delivery.selected_len != source.len()
            || delivery.next_consume_epoch != source.next_consume_epoch
            || delivery.peek_cursor_confirmed
            || channel.inbound_consumed != p.consumed
            || !channel.inbound.is_empty()
            || channel.local_control_generation != p.control
            || socket.options.receive_low_water != p.low_water
            || socket.consume_epoch.checked_add(1) != Some(source.next_consume_epoch)
            || socket
                .native
                .as_ref()
                .is_none_or(|n| n.identity != source.origin().identity())
            || output.publication.is_some()
        {
            return Err(invalid(
                "Record output changed original source/Call/Delivery/control/cut",
            ));
        }
        let expected = output
            .drain
            .as_ref()
            .and_then(|d| d.binding.as_ref())
            .unwrap_or_else(|| source.predecessor().binding());
        if state
            .helper_copy
            .as_ref()
            .is_none_or(|actual| !Arc::ptr_eq(actual, expected))
        {
            return Err(invalid("Record output lost its exact helper owner"));
        }
        // Before Drain, actual physical cut must remain the selected cut. Later
        // reconciliation validates the real successor; it never resets history.
        if output.drain.as_ref().is_none_or(|d| d.observed.is_none())
            && socket.native.as_ref().unwrap().physical_observed != p.physical
        {
            return Err(invalid("Record output physical source moved before Drain"));
        }
        Ok(())
    }
    pub(crate) fn shared_record_receive_peers(
        &self,
        source: &Arc<SharedRecordReceiveSource>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.check_shared_record_receive_source(source)?;
        self.shared_call_census_with_record_probe(
            None,
            Some(source.call()),
            Some(source.lease),
            None,
            Some(source),
        )
    }
    pub(crate) fn with_shared_record_store_retention<T>(
        &mut self,
        source: &Arc<SharedRecordReceiveSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
        effect: impl FnOnce(&mut SharedRecordStoreRetainer<'_>) -> T,
    ) -> Result<T, NetworkReplayError> {
        self.shared_active(source.call(), grant)?;
        self.check_shared_record_receive_source(source)?;
        if now < source.selected.at {
            return Err(invalid("Record store moved before source selection"));
        }
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let output = wait.record_receive.as_mut().unwrap();
        if output.stored || !output.attempts.is_empty() || output.drain.is_some() {
            return Err(invalid("Record source already attempted guest output"));
        }
        Ok(effect(&mut SharedRecordStoreRetainer { output }))
    }
    pub(crate) fn complete_shared_record_store(
        &mut self,
        source: &Arc<SharedRecordReceiveSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<SharedRecordStored, NetworkReplayError> {
        self.shared_active(source.call(), grant)?;
        self.check_shared_record_receive_source(source)?;
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let output = wait.record_receive.as_ref().unwrap();
        if now < source.selected.at
            || output.stored
            || output.drain.is_some()
            || output.attempts.len() != 1
            || !Arc::ptr_eq(output.attempts[0].source(), source)
            || !matches!(output.attempts[0].outcome(),reverie::syscalls::NativeUserStoreOutcome::Attempted{raw:Ok(n),postcheck:Ok(())} if *n==source.len())
        {
            return Err(invalid(
                "Record receive lacks exact actual full store and postcheck",
            ));
        }
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let output = wait.record_receive.as_mut().unwrap();
        output.stored = true;
        output.attempts[0].release_interval_after_store();
        Ok(SharedRecordStored {
            source: source.clone(),
        })
    }
}

/// One actual successor submission. The same Arc is retained on the Call
/// before a worker can start; dropping the returned token cannot undo it.
#[derive(Debug)]
pub(crate) struct SharedRecordDrainSubmission {
    source: Arc<SharedRecordReceiveSource>,
    claimed: std::sync::atomic::AtomicBool,
}
impl SharedRecordDrainSubmission {
    pub(crate) fn source(&self) -> &Arc<SharedRecordReceiveSource> {
        &self.source
    }
    pub(crate) fn claim(&self) -> std::io::Result<()> {
        self.claimed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map(|_| ())
            .map_err(|_| std::io::Error::other("shared Record Drain already submitted"))
    }
}
impl SharedRecordReceiveSource {
    pub(crate) fn effects(
        &self,
    ) -> &[Arc<crate::network_runtime::shared_waits::JoinedSharedEffect>] {
        self.selected.record.receive_effects()
    }
}
impl NetworkReplayEngine {
    pub(crate) fn prepare_shared_record_drain(
        &mut self,
        stored: SharedRecordStored,
    ) -> Result<Arc<SharedRecordDrainSubmission>, NetworkReplayError> {
        let source = stored.source;
        self.check_shared_record_receive_source(&source)?;
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let output = wait.record_receive.as_ref().unwrap();
        if !output.stored
            || output.drain.is_some()
            || output.attempts.len() != 1
            || !matches!(output.attempts[0].outcome(),reverie::syscalls::NativeUserStoreOutcome::Attempted{raw:Ok(n),postcheck:Ok(())} if *n==source.len())
        {
            return Err(invalid("Drain lacks original one-use full shared store"));
        }
        let submission = Arc::new(SharedRecordDrainSubmission {
            source: source.clone(),
            claimed: std::sync::atomic::AtomicBool::new(false),
        });
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        wait.record_receive.as_mut().unwrap().drain = Some(SharedRecordDrainState {
            submission: submission.clone(),
            binding: None,
            observed: None,
            joined: None,
            matched: false,
        });
        let delivery = self.shadow_deliveries.get_mut(&source.lease).unwrap();
        delivery.drain_started = true;
        delivery.pending = Some(NetworkStreamPhysicalEffect::Drain {
            maximum: source.len(),
        });
        Ok(submission)
    }
    pub(crate) fn validate_shared_record_drain(
        &self,
        submission: &Arc<SharedRecordDrainSubmission>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        let source = submission.source();
        self.check_shared_record_receive_source(source)?;
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let drain = wait
            .record_receive
            .as_ref()
            .unwrap()
            .drain
            .as_ref()
            .ok_or_else(|| invalid("Drain lost submission custody"))?;
        if !Arc::ptr_eq(&drain.submission, submission)
            || drain.observed.is_some()
            || drain.joined.is_some()
        {
            return Err(invalid(
                "Drain changed or completed its original submission",
            ));
        }
        self.shared_record_receive_peers(source)
    }
    pub(crate) fn bind_shared_record_drain_helper(
        &mut self,
        submission: &Arc<SharedRecordDrainSubmission>,
        binding: Arc<HelperCopyBinding>,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_record_drain(submission)?;
        let source = submission.source();
        let state = &self.stream_calls[&source.call()];
        if !submission
            .claimed
            .load(std::sync::atomic::Ordering::Acquire)
            || !binding.succeeds(source.predecessor())
            || binding.owner() != source.owner()
            || binding.call() != source.call()
            || binding.lease() != source.lease
            || binding.effect()
                != &(NetworkStreamPhysicalEffect::Drain {
                    maximum: source.len(),
                })
            || state
                .helper_copy
                .as_ref()
                .is_none_or(|b| !Arc::ptr_eq(b, source.predecessor().binding()))
        {
            return Err(invalid(
                "shared Drain changed exact original helper predecessor",
            ));
        }
        let state = self.stream_calls.get_mut(&source.call()).unwrap();
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            unreachable!()
        };
        let drain = wait
            .record_receive
            .as_mut()
            .unwrap()
            .drain
            .as_mut()
            .unwrap();
        if drain.binding.is_some() {
            return Err(invalid("shared Drain helper already bound"));
        }
        drain.binding = Some(binding.clone());
        state.helper_copy = Some(binding);
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RecordReceivePublication {
    input: Option<NetworkInputEventV4>,
    node: Option<NetworkReleaseNodeV4>,
    observation: Option<NetworkNativeReceiveObservationV4>,
    result: i64,
}
impl NetworkReplayEngine {
    /// Exact raw completion is attached before validating bytes or geometry.
    /// Failure preserves every actual effect on the original semantic owner.
    pub(crate) fn confirm_shared_record_drain(
        &mut self,
        proof: &crate::network_runtime::shared_waits::ConfirmedSharedDrain<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<Arc<PreparedSharedRecordReceivePublication>, NetworkReplayError> {
        let joined = proof.joined();
        let submission = joined.submission();
        let source = submission.source();
        self.shared_active(source.call(), grant)?;
        self.validate_shared_record_drain(submission)?;
        let observed = joined.observed();
        let completion = observed
            .helper_copy
            .as_ref()
            .ok_or_else(|| invalid("actual shared Drain omitted canonical helper owner"))?;
        {
            let state = &self.stream_calls[&source.call()];
            let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
                unreachable!()
            };
            let drain = wait
                .record_receive
                .as_ref()
                .unwrap()
                .drain
                .as_ref()
                .unwrap();
            if now < source.selected.at
                || drain
                    .binding
                    .as_ref()
                    .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
                || state
                    .helper_copy
                    .as_ref()
                    .is_none_or(|b| !Arc::ptr_eq(b, completion.binding()))
                || !completion.binding().succeeds(source.predecessor())
                || completion.binding().lease() != source.lease
                || self.shadow_deliveries[&source.lease].pending.as_ref()
                    != Some(completion.binding().effect())
            {
                return Err(invalid(
                    "actual shared Drain changed its exact original Pending",
                ));
            }
        }
        {
            let Some(SharedAttempt::Wait(wait)) = &mut self
                .stream_calls
                .get_mut(&source.call())
                .unwrap()
                .shared_attempt
            else {
                unreachable!()
            };
            let drain = wait
                .record_receive
                .as_mut()
                .unwrap()
                .drain
                .as_mut()
                .unwrap();
            drain.observed = Some(observed.clone());
            drain.joined = Some(joined.clone());
        }
        completion
            .joined_worker()
            .map_err(|e| invalid(&e.to_string()))?;
        let capture = completion.capture();
        if capture.manifest.present != 1
            || capture.manifest.summary.version != 5
            || completion.attempts().len() != capture.units.len()
            || observed.bytes != capture.committed
        {
            return Err(invalid(
                "shared Drain lacks exact complete copy5 result coverage",
            ));
        }
        let returned = if let Some(errno) = observed.errno {
            if errno <= 0
                || observed.raw_return != -1
                || capture.manifest.returned != -i64::from(errno)
                || observed.confirmation != NetworkStreamPhysicalResult::Errno(errno)
                || !capture.committed.is_empty()
            {
                return Err(invalid("shared Drain changed actual negative result"));
            }
            0
        } else {
            let n = usize::try_from(observed.raw_return)
                .map_err(|_| invalid("shared Drain has invalid raw return"))?;
            if capture.manifest.returned != observed.raw_return
                || n != capture.committed.len()
                || n > source.len()
                || observed.confirmation
                    != (NetworkStreamPhysicalResult::Drained {
                        bytes: capture.committed.clone(),
                    })
            {
                return Err(invalid("shared Drain changed actual successful result"));
            }
            n
        };
        let mut cursor = source.selected.physical;
        let mut copied = 0u64;
        for (index, (attempt, unit)) in completion.attempts().iter().zip(&capture.units).enumerate()
        {
            let geometry = unit
                .observation
                .ok_or_else(|| invalid("shared Drain lacks actual Consume geometry"))?;
            let native = &unit.native;
            if !completion.binding().owns_attempt(attempt)
                || attempt.operation() != 21
                || attempt.ordinal() != index
                || attempt.unit() != unit
                || native.disposition != crate::network_runtime::original_read_copy::CONSUME
                || !matches!(native.returned, 0 | -14)
                || native.offset != copied
                || geometry.begin.before != cursor.bytes
                || geometry.begin.start != cursor.bytes
                || geometry.begin.order != cursor.order
            {
                return Err(invalid(
                    "shared Drain changed contiguous actual Consume chain",
                ));
            }
            if native.returned == 0 {
                if native.copied == 0
                    || native.copied != native.requested
                    || cursor.bytes.checked_add(native.copied) != Some(geometry.after)
                    || cursor.order.checked_add(1) != Some(native.order)
                {
                    return Err(invalid(
                        "shared Drain unit changed actual physical progress",
                    ));
                }
                copied = copied
                    .checked_add(native.copied)
                    .ok_or(NetworkReplayError::Overflow)?;
                cursor = crate::network_replay::native_receive::Cut {
                    bytes: geometry.after,
                    order: native.order,
                };
            } else if native.copied >= native.requested
                || geometry.after != cursor.bytes
                || native.order != cursor.order
            {
                return Err(invalid(
                    "shared Drain failed unit changed actual nonconsuming cut",
                ));
            }
        }
        if usize::try_from(copied).ok() != Some(returned) {
            return Err(invalid(
                "shared Drain count differs from its actual Consume prefix",
            ));
        }
        // This existing API commits physical history only. A valid short or
        // mismatching actual prefix remains retained without semantic success.
        self.retain_native_receive_attempts(source.owner(), source.call(), completion.attempts())?;
        if observed.errno.is_some() || returned != source.len() || capture.committed != source.bytes
        {
            return Err(invalid(
                "actual shared Drain did not reconcile whole stored source",
            ));
        }
        let prepared = Arc::new(PreparedSharedRecordReceivePublication {
            source: source.clone(),
            joined: joined.clone(),
            at: now,
        });
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let output = wait.record_receive.as_mut().unwrap();
        output.drain.as_mut().unwrap().matched = true;
        output.prepared = Some(prepared.clone());
        let delivery = self.shadow_deliveries.get_mut(&source.lease).unwrap();
        delivery.drained = returned;
        delivery.pending = None;
        Ok(prepared)
    }

    fn shared_record_receive_candidate(
        &self,
        p: &RecordReceiveSnapshot,
        now: LogicalTime,
    ) -> Result<detcore_model::network_trace::NetworkTraceV4, NetworkReplayError> {
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some()||!matches!(native.trace.release_model,NetworkReleaseModelV4::SerializedSharedMmAttemptsV1{..})
            ||now<p.at||native.trace.release_model.nodes().len() as u64!=p.entry.receive_entry_cut.0
            ||native.trace.entry_frontier(p.entry.receive_entry_cut).map_err(|e|invalid(&e.to_string()))?!=p.entry.prerequisites
            ||native.trace.inputs.iter().rev().find(|i|i.channel==p.probe.channel).is_none_or(|i|i.release.not_before_global_time>now)
            ||!native.trace.release_model.nodes().iter().any(|n|matches!(n.kind,NetworkReleaseNodeKindV4::Progress{channel,milestone:detcore_model::network_trace::NetworkProgressV4::Established{..}} if channel==p.probe.channel)){
            return Err(invalid("shared receive changed original entry/established trace frontier"));
        }
        let mut candidate = native.trace.clone();
        let shadow = self.shadow.as_ref().ok_or(NetworkReplayError::WrongMode)?;
        candidate.fresh_stream_profiles = shadow.profiles.values().cloned().collect();
        candidate.channel_socket_classes = shadow
            .channel_classes
            .iter()
            .map(|(channel, key)| ChannelSocketClassV3 {
                channel: *channel,
                key: *key,
            })
            .collect();
        candidate.receive_environment = shadow.environment;
        candidate.fresh_send_timeouts = native
            .fresh_send
            .iter()
            .map(|(key, timeout)| FreshSendTimeoutV1 {
                key: *key,
                timeout: *timeout,
            })
            .collect();
        Ok(candidate)
    }
    pub(crate) fn publish_shared_record_receive(
        &mut self,
        prepared: &Arc<PreparedSharedRecordReceivePublication>,
        proof: &crate::network_runtime::shared_waits::ConfirmedSharedDrain<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<usize, NetworkReplayError> {
        let source = prepared.source();
        let p = &source.selected;
        self.shared_active(source.call(), grant)?;
        self.check_shared_record_receive_source(source)?;
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let output = wait.record_receive.as_ref().unwrap();
        let drain = output
            .drain
            .as_ref()
            .ok_or_else(|| invalid("shared publication lacks real Drain"))?;
        let completion = prepared
            .joined
            .observed()
            .helper_copy
            .as_ref()
            .ok_or_else(|| invalid("shared publication lost Drain receipt"))?;
        let native = self.shadow.as_ref().unwrap().sockets[&source.open_file()]
            .native
            .as_ref()
            .unwrap();
        let after = p
            .physical
            .bytes
            .checked_add(source.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let last = completion
            .attempts()
            .last()
            .ok_or_else(|| invalid("shared publication lacks Consume unit"))?;
        if now != prepared.at
            || !Arc::ptr_eq(prepared.joined(), proof.joined())
            || !output.stored
            || output
                .prepared
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, prepared))
            || !drain.matched
            || drain
                .joined
                .as_ref()
                .is_none_or(|actual| !Arc::ptr_eq(actual, prepared.joined()))
            || output.attempts.len() != 1
            || !matches!(output.attempts[0].outcome(),reverie::syscalls::NativeUserStoreOutcome::Attempted{raw:Ok(n),postcheck:Ok(())} if *n==source.len())
            || completion.capture().committed != source.bytes
            || native.physical_observed.bytes != after
            || native.physical_observed.order != last.unit().native.order
            || native.physical_observed.order <= p.physical.order
            || completion.attempts().iter().any(|receipt| {
                !self.stream_calls[&source.call()]
                    .native_receive
                    .iter()
                    .any(|a| a.joined && a.receipt.same(receipt))
            })
            || self.shadow_deliveries[&source.lease].drained != source.len()
            || self.shadow_deliveries[&source.lease].pending.is_some()
        {
            return Err(invalid(
                "shared publication changed exact full store/Drain/physical frontier",
            ));
        }
        let mut candidate = self.shared_record_receive_candidate(p, now)?;
        let payload_end = candidate
            .native_receive_observations
            .iter()
            .rev()
            .find(|o| o.channel == p.probe.channel)
            .map(|o| {
                o.stream_offset
                    .checked_add(o.length)
                    .ok_or(NetworkReplayError::Overflow)
            })
            .transpose()?
            .unwrap_or(0);
        let queue = &self.channels[&p.probe.channel];
        // Preserve actual legacy publisher's wholly unpublished suffix profile.
        // There is no availability-only producer to reinterpret earlier bytes.
        if payload_end != p.physical.bytes
            || queue.inbound_consumed != p.physical.bytes
            || !queue.inbound.is_empty()
            || queue.published_ingress.unwrap_or_default().stream_offset != p.physical.bytes
            || queue.published_ingress.unwrap_or_default().terminal
            || queue.peer_write_closed
            || queue.local_read_shutdown
        {
            return Err(invalid(
                "shared publication lacks its wholly unpublished contiguous payload",
            ));
        }
        let ordinal =
            u64::try_from(candidate.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let node_id = u64::try_from(candidate.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let observation_ordinal = u64::try_from(candidate.native_receive_observations.len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let mut release = p.entry.clone();
        release.not_before_global_time = now;
        let input = NetworkInputEventV4 {
            ordinal,
            channel: p.probe.channel,
            release: release.clone(),
            event: NetworkInputKindV2::StreamBytes {
                stream_offset: p.physical.bytes,
                bytes: source.bytes.clone(),
            },
        };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(node_id),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        let fragments = completion
            .attempts()
            .iter()
            .map(|attempt| {
                let unit = attempt.unit();
                let geometry = unit.observation.expect("checked complete copy5 geometry");
                let b = geometry.begin;
                NetworkNativeCopyFragmentV4 {
                    stream_offset: b.start,
                    requested: unit.native.requested,
                    copied: unit.native.copied,
                    available: b.available,
                    source_offset: b.source_offset,
                    storage_length: b.skb_length,
                    nonlinear_length: b.nonlinear,
                    disposition: NetworkNativeCopyDispositionV4::Consume,
                    physical_before: b.before,
                    physical_after: geometry.after,
                }
            })
            .collect();
        let observation = NetworkNativeReceiveObservationV4 {
            ordinal: observation_ordinal,
            channel: p.probe.channel,
            stream_offset: p.physical.bytes,
            length: source.len() as u64,
            fragments,
        };
        candidate.inputs.push(input.clone());
        candidate
            .native_receive_observations
            .push(observation.clone());
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut candidate.release_model
        else {
            unreachable!()
        };
        nodes.push(node.clone());
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        let history = p.record.receive_history(now);
        let receipt = RecordReceivePublication {
            input: Some(input.clone()),
            node: Some(node.clone()),
            observation: Some(observation.clone()),
            result: source.len() as i64,
        };
        // No fallible semantic operation follows the validated complete candidate.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native.trace.inputs.push(input);
        native.trace.native_receive_observations.push(observation);
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut native.trace.release_model
        else {
            unreachable!()
        };
        nodes.push(node);
        let channel = self.channels.get_mut(&p.probe.channel).unwrap();
        channel.inbound_consumed = after;
        channel.published_ingress = Some(PublishedIngress {
            stream_offset: after,
            last_release: None,
            terminal: false,
        });
        channel.receive_input_generation = Some(ordinal);
        channel.refresh_readiness();
        self.commit_shadow_delivery_finish(
            source.lease,
            source.open_file(),
            NetworkStreamChunkDisposition::Consumed,
            true,
        );
        self.stream_delivery.remove(&source.open_file());
        self.stream_operations.remove(&source.lease);
        let state = self.stream_calls.get_mut(&source.call()).unwrap();
        state.helper_copy = None;
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            unreachable!()
        };
        wait.record_history.push(history);
        wait.record_receive.as_mut().unwrap().publication = Some(receipt);
        Ok(source.len())
    }
}

impl NetworkReplayEngine {
    pub(crate) fn commit_shared_record_no_store(
        &mut self,
        plan: &SharedRecordNoStorePlan,
        proof: &ConfirmedSharedRecordReceive<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<SharedNoStoreResult, NetworkReplayError> {
        let SharedRecordReceivePlan::NoStore(current) =
            self.plan_shared_record_receive(plan.origin(), grant, proof, now)?
        else {
            return Err(invalid("NoStore changed actual source class"));
        };
        if !plan.selected.same(&current.selected) || plan.outcome != current.outcome {
            return Err(invalid("NoStore changed actual source/result/attempt"));
        }
        let p = &plan.selected;
        let file = p.binding.open_file;
        self.validate_stream_call_lifetime(plan.owner(), plan.call(), file)?;
        let queue = &self.channels[&p.probe.channel];
        let frontier = queue.published_ingress.unwrap_or_default();
        let socket = &self.shadow.as_ref().unwrap().sockets[&file];
        if p.probe.retained_prefix != 0
            || !queue.inbound.is_empty()
            || queue.local_read_shutdown
            || queue.inbound_consumed != p.physical.bytes
            || frontier.stream_offset != p.physical.bytes
            || socket
                .native
                .as_ref()
                .is_none_or(|n| n.physical_observed != p.physical)
            || frontier.terminal != queue.peer_write_closed
            || (matches!(plan.outcome, SharedNoStoreResult::WouldBlock { .. }) && frontier.terminal)
        {
            return Err(invalid(
                "NoStore changed actual zero-unit physical/semantic frontier",
            ));
        }
        let mut candidate = self.shared_record_receive_candidate(p, now)?;
        let payload_end = candidate
            .native_receive_observations
            .iter()
            .rev()
            .find(|o| o.channel == p.probe.channel)
            .map(|o| {
                o.stream_offset
                    .checked_add(o.length)
                    .ok_or(NetworkReplayError::Overflow)
            })
            .transpose()?
            .unwrap_or(0);
        let terminal: Vec<_> = candidate
            .inputs
            .iter()
            .filter(|i| {
                i.channel == p.probe.channel
                    && matches!(i.event, NetworkInputKindV2::PeerShutdown { .. })
            })
            .collect();
        if payload_end != p.physical.bytes
            || if frontier.terminal {
                terminal.len() != 1
                    || !matches!(terminal[0].event,NetworkInputKindV2::PeerShutdown{stream_offset,direction:NetworkShutdownV2::Write} if stream_offset==p.physical.bytes)
            } else {
                !terminal.is_empty()
            }
        {
            return Err(invalid(
                "NoStore changed immutable payload/terminal coverage",
            ));
        }
        let append = matches!(plan.outcome, SharedNoStoreResult::Eof) && !frontier.terminal;
        let mut release = p.entry.clone();
        release.not_before_global_time = now;
        let ordinal =
            u64::try_from(candidate.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let id = u64::try_from(candidate.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let input = NetworkInputEventV4 {
            ordinal,
            channel: p.probe.channel,
            release: release.clone(),
            event: NetworkInputKindV2::PeerShutdown {
                stream_offset: p.physical.bytes,
                direction: NetworkShutdownV2::Write,
            },
        };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(id),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        if append {
            candidate.inputs.push(input.clone());
            let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
                &mut candidate.release_model
            else {
                unreachable!()
            };
            nodes.push(node.clone());
        }
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        let result = match plan.outcome {
            SharedNoStoreResult::Eof => 0,
            SharedNoStoreResult::WouldBlock { .. } => -i64::from(libc::EAGAIN),
        };
        let source = Arc::new(SharedRecordReceiveSource {
            selected: p.clone(),
            lease: plan.origin().lease(),
            bytes: Vec::new(),
            next_consume_epoch: socket.consume_epoch,
        });
        let history = p.record.receive_history(now);
        let receipt = RecordReceivePublication {
            input: append.then_some(input.clone()),
            node: append.then_some(node.clone()),
            observation: None,
            result,
        };
        if append {
            let EngineState::Native(native) = &mut self.mode else {
                unreachable!()
            };
            native.trace.inputs.push(input);
            let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
                &mut native.trace.release_model
            else {
                unreachable!()
            };
            nodes.push(node);
            let channel = self.channels.get_mut(&p.probe.channel).unwrap();
            channel.published_ingress = Some(PublishedIngress {
                terminal: true,
                ..frontier
            });
            channel.peer_write_closed = true;
            channel.receive_input_generation = Some(ordinal);
            channel.refresh_readiness();
        }
        self.shadow_probes
            .remove(&plan.origin().lease())
            .expect("checked original no-store probe");
        self.finish_socket_control(
            plan.owner(),
            plan.origin().lease(),
            NetworkSocketControlFinish::Unchanged,
        )
        .expect("checked exact unchanged NoStore control");
        let state = self.stream_calls.get_mut(&plan.call()).unwrap();
        state.helper_copy = None;
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            unreachable!()
        };
        wait.record_probe = None;
        wait.record_history.push(history);
        wait.record_receive = Some(SharedRecordReceiveOutput {
            source,
            attempts: Vec::new(),
            stored: false,
            drain: None,
            prepared: None,
            publication: Some(receipt),
        });
        Ok(current.outcome)
    }
    pub(crate) fn shared_record_no_store_committed(&self, plan: &SharedRecordNoStorePlan) -> bool {
        self.stream_calls.get(&plan.call()).is_some_and(|state|matches!(&state.shared_attempt,Some(SharedAttempt::Wait(w)) if w.record_receive.as_ref().is_some_and(|o|Arc::ptr_eq(o.source.origin(),plan.origin())&&o.source.bytes.is_empty()&&o.publication.is_some())))
    }
    pub(in crate::network_replay) fn check_shared_record_receive_release(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            return Err(invalid("shared release lacks Receive output"));
        };
        let output = wait
            .record_receive
            .as_ref()
            .ok_or_else(|| invalid("shared Record Receive did not finish output"))?;
        let receipt = output
            .publication
            .as_ref()
            .ok_or_else(|| invalid("shared Record Receive did not publish exact source"))?;
        let source = &output.source;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some()
            || !self.uses_shared_mm_attempts()
            || state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || !state.physical_pin_required
            || state.open_file != Some(source.open_file())
            || !wait.root.is_current(owner)
            || wait.record_probe.is_some()
            || wait.output.is_some()
            || wait.poll_output.is_some()
            || state.capture_control.is_some()
            || state.capture_publication.is_some()
            || state.original.is_some()
            || state.helper_copy.is_some()
            || state.private_receive.is_some()
            || state.record_no_store.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || state.replay_receive.is_some()
            || state.native_entry.is_some()
            || state.native_entry_attempted.is_some()
            || state.replay_connect.is_some()
            || self.shadow_probes.values().any(|p| p.call == call)
            || self.shadow_deliveries.values().any(|d| d.call == call)
            || self
                .stream_operations
                .values()
                .any(|o| o.open_file == source.open_file())
            || self.socket_controls.contains_key(&source.open_file())
            || self.zero_stream_waits.values().any(|w| w.call == call)
            || receipt
                .input
                .as_ref()
                .is_some_and(|i| native.trace.inputs.get(i.ordinal as usize) != Some(i))
            || receipt
                .node
                .as_ref()
                .is_some_and(|n| native.trace.release_model.nodes().get(n.id.0 as usize) != Some(n))
            || receipt.observation.as_ref().is_some_and(|o| {
                native
                    .trace
                    .native_receive_observations
                    .get(o.ordinal as usize)
                    != Some(o)
            })
        {
            return Err(invalid(
                "shared Receive release lost exact published source/debt settlement",
            ));
        }
        match &source.selected.source {
            ReceiveSource::Bytes(_) => {
                if !output.stored
                    || output.attempts.len() != 1
                    || !Arc::ptr_eq(output.attempts[0].source(), source)
                    || !matches!(output.attempts[0].outcome(),reverie::syscalls::NativeUserStoreOutcome::Attempted{raw:Ok(n),postcheck:Ok(())} if *n==source.len())
                    || output
                        .drain
                        .as_ref()
                        .is_none_or(|d| !d.matched || d.joined.is_none())
                    || receipt.result != source.len() as i64
                    || receipt.input.is_none()
                    || receipt.node.is_none()
                    || receipt.observation.is_none()
                {
                    return Err(invalid(
                        "shared Receive release lost whole actual store/Drain",
                    ));
                }
            }
            ReceiveSource::Empty(empty) => {
                if output.stored
                    || !output.attempts.is_empty()
                    || output.drain.is_some()
                    || receipt.observation.is_some()
                    || receipt.result
                        != match empty.kind() {
                            NoStoreReturn::Eof => 0,
                            NoStoreReturn::WouldBlock => -i64::from(libc::EAGAIN),
                        }
                {
                    return Err(invalid("shared Receive release changed canonical NoStore"));
                }
            }
        }
        if state.native_receive.iter().any(|a| {
            !a.joined
                || (!source
                    .predecessor()
                    .attempts()
                    .iter()
                    .any(|r| r.same(&a.receipt))
                    && !output
                        .drain
                        .as_ref()
                        .and_then(|d| d.observed.as_ref())
                        .and_then(|o| o.helper_copy.as_ref())
                        .is_some_and(|c| c.attempts().iter().any(|r| r.same(&a.receipt)))
                    && !self.shared_record_history_receipt_covered(wait, &a.receipt))
        }) {
            return Err(invalid(
                "shared Receive release has uncovered physical history",
            ));
        }
        self.validate_stream_call_lifetime(owner, call, source.open_file())
    }
}

impl NetworkReplayEngine {
    pub(in crate::network_replay) fn has_shared_record_receive_output(
        &self,
        call: NetworkStreamCallId,
    ) -> bool {
        self.stream_calls.get(&call).is_some_and(|s|matches!(&s.shared_attempt,Some(SharedAttempt::Wait(w)) if w.record_receive.is_some()))
    }
}

impl NetworkReplayEngine {
    pub(crate) fn shared_record_store_ended(
        &self,
        source: &Arc<SharedRecordReceiveSource>,
    ) -> bool {
        self.stream_calls.get(&source.call()).is_some_and(|s|matches!(&s.shared_attempt,Some(SharedAttempt::Wait(w)) if w.record_receive.as_ref().is_some_and(|o|Arc::ptr_eq(&o.source,source)&&o.stored&&o.attempts.len()==1)))
    }
}
