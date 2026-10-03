//! A pending Record attempt is positively retired native work, not an empty
//! operation map or a fabricated EAGAIN. Original Call and byte history survive.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;

#[path = "record_poll.rs"]
mod record_poll;
pub(crate) use record_poll::SharedRecordPollPublication;

use crate::network_replay::native_receive::no_store::NoStoreReturn;
use crate::network_replay::native_receive::no_store::RecordNoStore;
use crate::network_replay::native_receive::private_peek::PrivateSource;
use crate::network_runtime::original_installation::FileIdentity;
use crate::network_runtime::shared_waits::ConfirmedSharedEffect;
use crate::network_runtime::shared_waits::ConfirmedSharedRecordPending;
use crate::network_runtime::shared_waits::JoinedSharedEffect;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;

#[derive(Debug)]
pub(crate) struct SharedRecordProbe {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
    binding: crate::types::FdSlotBinding,
    identity: FileIdentity,
    physical: crate::network_replay::native_receive::Cut,
    consumed: u64,
    intent: SharedWaitIntent,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    ordinal: u64,
    epoch: u64,
    entry: NetworkReleaseV4,
    prefix: JoinedSharedPrefix,
    control: u64,
    low_water: u32,
}
impl SharedRecordProbe {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.call
    }
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        &self.root
    }
    pub(crate) fn prefix(&self) -> &JoinedSharedPrefix {
        &self.prefix
    }
}
/// Read-only source for the shared-only Poll publisher. This is not a
/// completion token: the real trace append must precede any Poll suspension.
#[derive(Debug)]
pub(crate) struct SharedRecordPollSource {
    origin: Arc<SharedRecordProbe>,
    scan: Arc<JoinedSharedEffect>,
    consumed: u64,
    channel: NetworkChannelId,
    observed_at: LogicalTime,
}
impl SharedRecordPollSource {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        &self.origin
    }
    pub(crate) fn revents(&self) -> i16 {
        let NetworkStreamPhysicalResult::PollState { revents } = &self.scan.observed().confirmation
        else {
            unreachable!("actual full scan was checked by the private issuer")
        };
        *revents
    }
    pub(crate) fn channel(&self) -> NetworkChannelId {
        self.channel
    }
    pub(crate) fn consumed(&self) -> u64 {
        self.consumed
    }
    pub(crate) fn control_generation(&self) -> u64 {
        self.origin.control
    }
    pub(crate) fn low_water(&self) -> u32 {
        self.origin.low_water
    }
    pub(crate) fn entry(&self) -> &NetworkReleaseV4 {
        &self.origin.entry
    }
    pub(crate) fn observed_at(&self) -> LogicalTime {
        self.observed_at
    }
}
#[derive(Debug)]
pub(crate) struct SharedEffectIdentity {
    origin: Arc<SharedRecordProbe>,
    number: u64,
    effect: NetworkStreamPhysicalEffect,
    submitted: AtomicBool,
}
impl SharedEffectIdentity {
    pub(crate) fn origin(&self) -> &Arc<SharedRecordProbe> {
        &self.origin
    }
    pub(crate) fn number(&self) -> u64 {
        self.number
    }
    pub(crate) fn effect(&self) -> &NetworkStreamPhysicalEffect {
        &self.effect
    }
    pub(crate) fn claim(&self) -> std::io::Result<()> {
        self.submitted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| std::io::Error::other("shared native effect submitted twice"))
    }
}
/// The original latch remains on the Call if this non-Clone token is dropped.
#[derive(Debug)]
pub(crate) struct PreparedSharedEffect(Arc<SharedEffectIdentity>);
impl PreparedSharedEffect {
    pub(crate) fn into_identity(self) -> Arc<SharedEffectIdentity> {
        self.0
    }
}
#[derive(Debug, Clone)]
enum Source {
    Bytes(PrivateSource),
    Empty(Arc<RecordNoStore>),
}
impl Source {
    fn completion(&self) -> &crate::network_runtime::HelperCopyCompletion {
        match self {
            Self::Bytes(s) => &s.completion,
            Self::Empty(s) => s.completion(),
        }
    }
}
#[derive(Debug, Clone)]
pub(super) struct ProbeState {
    origin: Arc<SharedRecordProbe>,
    pending: Option<Arc<SharedEffectIdentity>>,
    effects: Vec<Arc<JoinedSharedEffect>>,
    source: Option<Source>,
    confirmed_at: Option<LogicalTime>,
    published_poll: Option<Arc<SharedRecordPollPublication>>,
}
#[derive(Debug)]
pub(super) struct RecordHistory {
    origin: Arc<SharedRecordProbe>,
    effects: Vec<Arc<JoinedSharedEffect>>,
    source: Option<Source>,
    observed_at: LogicalTime,
    published_poll: Option<Arc<SharedRecordPollPublication>>,
}
impl PartialEq for RecordHistory {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for RecordHistory {}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedProbeProgress {
    Need(NetworkStreamPhysicalEffect),
    PendingCandidate,
    EligibleSource,
    Refused(String),
}

impl NetworkReplayEngine {
    pub(super) fn shared_record_history_covers(&self, state: &StreamCallState) -> bool {
        if state.native_receive.is_empty() {
            return true;
        }
        let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
            return false;
        };
        state.native_receive.iter().all(|attempt| {
            attempt.joined
                && wait.record_history.iter().any(|history| {
                    history.source.as_ref().is_some_and(|source| {
                        source
                            .completion()
                            .attempts()
                            .iter()
                            .any(|actual| actual.same(&attempt.receipt))
                    })
                })
        })
    }

    pub(crate) fn begin_shared_record_probe(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedRecordProbe>, NetworkReplayError> {
        let wait = self.shared_active(call, grant)?;
        let AttemptPhase::Active {
            ordinal,
            epoch,
            entry: AttemptEntry::Record(entry),
            completion: None,
        } = &wait.phase
        else {
            return Err(invalid(
                "shared Record probe requires its original active Record entry",
            ));
        };
        let state = &self.stream_calls[&call];
        self.shared_wait_debts_settled(call, state)?;
        self.validate_stream_call_lifetime(grant.owner(), call, wait.binding.open_file)?;
        if self.mode() != NetworkEngineMode::Record
            || !state.physical_pin_required
            || state.phase != StreamCallPhase::Active
            || !admission.matches_selected(self, call)?
            || !self.shared_census_matches_grant(Some(call), None, grant)?
            || !Arc::ptr_eq(admission.root(), grant.root())
            || now < entry.not_before_global_time
        {
            return Err(invalid("shared Record probe lost actual pin/grant/census"));
        }
        let binding = wait.binding;
        let socket = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&binding.open_file))
            .ok_or_else(|| invalid("shared Record probe lost socket profile"))?;
        let identity = socket
            .native
            .as_ref()
            .ok_or_else(|| invalid("shared Record probe lost native origin"))?
            .identity;
        let channel = self.bound_channel(binding.open_file)?;
        if self.stream_role(channel)? != NetworkEndpointRoleV2::OutboundClient
            || self.channels[&channel].transport != NetworkTransportV2::Tcp
        {
            return Err(invalid(
                "shared Record probe requires enrolled outbound TCP",
            ));
        }
        let queue = &self.channels[&channel];
        let frontier = queue.published_ingress.unwrap_or_default();
        let retained = usize::try_from(
            frontier
                .stream_offset
                .checked_sub(queue.inbound_consumed)
                .ok_or(NetworkReplayError::Overflow)?,
        )
        .map_err(|_| NetworkReplayError::Overflow)?;
        retained
            .checked_add(1024)
            .ok_or(NetworkReplayError::Overflow)?;
        let intent = wait.intent.clone();
        let physical = socket
            .native
            .as_ref()
            .expect("checked original pin")
            .physical_observed;
        let consumed = queue.inbound_consumed;
        let (ordinal, epoch, entry, cursor, low_water, control) = (
            *ordinal,
            *epoch,
            entry.clone(),
            socket.options.peek_offset,
            socket.options.receive_low_water,
            queue.local_control_generation,
        );
        let lease = self.begin_socket_controls_inner(
            grant.owner(),
            vec![binding.open_file],
            Some(binding.open_file),
            false,
        )?[0]
            .1;
        let origin = Arc::new(SharedRecordProbe {
            owner: grant.owner(),
            call,
            lease,
            binding,
            identity,
            physical,
            consumed,
            intent,
            root: grant.root().clone(),
            ordinal,
            epoch,
            entry,
            prefix: admission.retained_capture_prefix(),
            control,
            low_water,
        });
        self.shadow_probes.insert(
            lease,
            ShadowProbeState {
                call,
                channel,
                began: now,
                retained_prefix: retained,
                captured_through: frontier.stream_offset,
                cursor_observed: false,
                original_cursor: cursor,
                current_cursor: cursor,
                peek: None,
                poll: None,
                poll_wait: None,
                queued: None,
                pending: None,
            },
        );
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.record_probe = Some(ProbeState {
            origin: origin.clone(),
            pending: None,
            effects: Vec::new(),
            source: None,
            confirmed_at: None,
            published_poll: None,
        });
        Ok(origin)
    }

    fn record_probe(
        &self,
        origin: &Arc<SharedRecordProbe>,
    ) -> Result<(&SharedWait, &ProbeState, &ShadowProbeState), NetworkReplayError> {
        let (state, wait) = self.shared_wait(origin.owner, origin.call)?;
        let record = wait
            .record_probe
            .as_ref()
            .ok_or_else(|| invalid("shared Record origin is absent"))?;
        let probe = self
            .shadow_probes
            .get(&origin.lease)
            .ok_or_else(|| invalid("shared Record lease is absent"))?;
        let control = self.owned_socket_control(origin.owner, origin.lease)?;
        let same_intent = match (&wait.intent, &origin.intent) {
            (SharedWaitIntent::Receive(actual), SharedWaitIntent::Receive(original)) => {
                Arc::ptr_eq(actual, original)
            }
            (SharedWaitIntent::Poll(actual), SharedWaitIntent::Poll(original)) => {
                Arc::ptr_eq(actual, original)
            }
            _ => false,
        };
        if !same_intent
            || !Arc::ptr_eq(&record.origin, origin)
            || !Arc::ptr_eq(&wait.root, &origin.root)
            || wait.binding != origin.binding
            || probe.call != origin.call
            || control.open_file != origin.binding.open_file
            || !control.physical.can_release_unchanged()
            || !state.physical_pin_required
            || state.phase != StreamCallPhase::Active
            || !origin.root.is_current(origin.owner)
            || !matches!(&wait.phase, AttemptPhase::Active { ordinal, epoch, entry: AttemptEntry::Record(entry), completion: None }
                if *ordinal == origin.ordinal && *epoch == origin.epoch && *entry == origin.entry)
            || self.mode() != NetworkEngineMode::Record
        {
            return Err(invalid(
                "shared Record probe replaced its original Call/attempt/control",
            ));
        }
        self.validate_stream_call_lifetime(origin.owner, origin.call, origin.binding.open_file)?;
        let queue = &self.channels[&probe.channel];
        let socket = &self.shadow.as_ref().unwrap().sockets[&origin.binding.open_file];
        if queue.inbound_consumed != origin.consumed
            || socket
                .native
                .as_ref()
                .is_none_or(|n| n.physical_observed != origin.physical)
            || queue.local_control_generation != origin.control
            || socket.options.receive_low_water != origin.low_water
            || socket
                .native
                .as_ref()
                .is_none_or(|n| n.identity != origin.identity)
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
            || wait.output.is_some()
        {
            return Err(invalid(
                "shared Record probe changed controls or owns unrelated effect debt",
            ));
        }
        if state.helper_copy.as_ref().is_some_and(|binding| {
            record.source.as_ref().map_or_else(
                || !matches!(&record.pending, Some(p) if matches!(p.effect, NetworkStreamPhysicalEffect::Peek { .. })
                    && binding.owner() == origin.owner && binding.call() == origin.call
                    && binding.lease() == origin.lease && binding.effect() == &p.effect),
                |source| !Arc::ptr_eq(binding, source.completion().binding()),
            )
        }) {
            return Err(invalid("shared Record probe contains an unrelated helper binding"));
        }
        Ok((wait, record, probe))
    }

    pub(crate) fn shared_record_probe_peers(
        &self,
        origin: &Arc<SharedRecordProbe>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.record_probe(origin)?;
        if self.shadow_probes.len() != 1 || self.socket_controls.len() != 1 {
            return Err(invalid("shared Record probe has another control owner"));
        }
        self.shared_call_census_with_record_probe(None, Some(origin.call), None, Some(origin), None)
    }

    pub(crate) fn shared_record_probe_progress(
        &self,
        origin: &Arc<SharedRecordProbe>,
        now: LogicalTime,
    ) -> Result<SharedProbeProgress, NetworkReplayError> {
        let (wait, record, probe) = self.record_probe(origin)?;
        if now < origin.entry.not_before_global_time
            || record.confirmed_at.is_some_and(|earlier| now < earlier)
        {
            return Err(invalid(
                "shared probe observation regressed its original logical time",
            ));
        }
        if record.pending.is_some() || probe.pending.is_some() {
            return Err(invalid("shared native effect remains submitted"));
        }
        if matches!(wait.intent, SharedWaitIntent::Receive(_)) {
            if !probe.cursor_observed {
                return Ok(SharedProbeProgress::Need(
                    NetworkStreamPhysicalEffect::ReadPeekOffset,
                ));
            }
            if probe.peek.is_none() {
                if probe.current_cursor.is_some_and(|v| v >= 0) {
                    return Ok(SharedProbeProgress::Need(
                        NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 },
                    ));
                }
                return Ok(SharedProbeProgress::Need(
                    NetworkStreamPhysicalEffect::Peek {
                        maximum: probe.retained_prefix + 1024,
                    },
                ));
            }
            if !probe.cursor_restored() {
                let value = probe
                    .original_cursor
                    .ok_or_else(|| invalid("shared probe cannot restore unknown cursor"))?;
                return Ok(SharedProbeProgress::Need(
                    NetworkStreamPhysicalEffect::SetPeekOffset { value },
                ));
            }
        }
        if probe.poll.is_none() {
            return Ok(SharedProbeProgress::Need(
                NetworkStreamPhysicalEffect::PollState,
            ));
        }
        let raw = probe.poll.unwrap();
        match &wait.intent {
            SharedWaitIntent::Receive(policy) => {
                let source = record
                    .source
                    .as_ref()
                    .ok_or_else(|| invalid("shared receive lacks actual Peek source"))?;
                match source {
                    Source::Empty(s) if s.kind() == NoStoreReturn::Eof => {
                        return Ok(SharedProbeProgress::EligibleSource);
                    }
                    Source::Bytes(s) if s.length >= policy.target() => {
                        return Ok(SharedProbeProgress::EligibleSource);
                    }
                    _ => {}
                }
                if policy.nonblocking() || wait.intent.deadline().is_some_and(|d| now >= d) {
                    return Ok(SharedProbeProgress::EligibleSource);
                }
                let cannot_wait = libc::POLLIN
                    | libc::POLLRDNORM
                    | libc::POLLRDBAND
                    | libc::POLLPRI
                    | libc::POLLERR
                    | libc::POLLHUP
                    | libc::POLLRDHUP;
                if raw & cannot_wait != 0 {
                    return Err(invalid(
                        "short Peek has a later readable/terminal scan; fresh result proof required",
                    ));
                }
            }
            SharedWaitIntent::Poll(policy) => {
                if policy.ready(raw) || wait.intent.deadline().is_some_and(|d| now >= d) {
                    return Ok(SharedProbeProgress::EligibleSource);
                }
            }
        }
        Ok(SharedProbeProgress::PendingCandidate)
    }

    pub(crate) fn prepare_shared_record_effect(
        &mut self,
        origin: &Arc<SharedRecordProbe>,
        grant: &SharedMmForegroundObservation<'_>,
        effect: NetworkStreamPhysicalEffect,
        now: LogicalTime,
    ) -> Result<PreparedSharedEffect, NetworkReplayError> {

        if matches!(&self.stream_calls.get(&origin.call).and_then(|s| s.shared_attempt.as_ref()), Some(SharedAttempt::Wait(w)) if w.poll_output.is_some()) {
            return Err(invalid("shared native transition retains Poll output custody"));
        }
        self.shared_active(origin.call, grant)?;
        if self.shared_record_probe_progress(origin, now)?
            != SharedProbeProgress::Need(effect.clone())
        {
            return Err(invalid(
                "shared Record effect is not the next original probe step",
            ));
        }
        let (_, record, _) = self.record_probe(origin)?;
        let number = u64::try_from(record.effects.len())
            .map_err(|_| NetworkReplayError::Overflow)?
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let step = Arc::new(SharedEffectIdentity {
            origin: origin.clone(),
            number,
            effect: effect.clone(),
            submitted: AtomicBool::new(false),
        });
        self.shadow_probes.get_mut(&origin.lease).unwrap().pending = Some(effect);
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&origin.call)
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        wait.record_probe.as_mut().unwrap().pending = Some(step.clone());
        Ok(PreparedSharedEffect(step))
    }

    pub(crate) fn validate_shared_record_effect(
        &self,
        step: &Arc<SharedEffectIdentity>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        let (_, record, probe) = self.record_probe(&step.origin)?;
        if record
            .pending
            .as_ref()
            .is_none_or(|p| !Arc::ptr_eq(p, step))
            || probe.pending.as_ref() != Some(&step.effect)
            || step.number != record.effects.len() as u64 + 1
        {
            return Err(invalid("shared native worker changed exact submitted step"));
        }
        self.shared_record_probe_peers(&step.origin)
    }

    pub(crate) fn confirm_shared_record_effect(
        &mut self,
        proof: &ConfirmedSharedEffect<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<SharedProbeProgress, NetworkReplayError> {
        let joined = proof.joined();
        let step = joined.step();
        let origin = step.origin();
        self.shared_active(origin.call, grant)?;
        self.validate_shared_record_effect(step)?;
        if !step.submitted.load(Ordering::Acquire) || now < origin.entry.not_before_global_time {
            return Err(invalid(
                "shared native result lacks original submitted time",
            ));
        }
        let observed = joined.observed();
        let (_, record, probe) = self.record_probe(origin)?;
        if record.confirmed_at.is_some_and(|earlier| now < earlier) {
            return Err(invalid(
                "shared native confirmation moved before an earlier actual step",
            ));
        }
        let mut next = probe.clone();
        let mut source = None;
        match (&step.effect, &observed.confirmation) {
            (
                NetworkStreamPhysicalEffect::ReadPeekOffset,
                NetworkStreamPhysicalResult::PeekOffset(value),
            ) if observed.raw_return == 0
                && observed.errno.is_none()
                && observed.bytes.is_empty()
                && next.original_cursor == Some(*value) =>
            {
                next.cursor_observed = true;
                next.current_cursor = Some(*value);
            }
            (
                NetworkStreamPhysicalEffect::ReadPeekOffset,
                NetworkStreamPhysicalResult::Errno(errno),
            ) if next.original_cursor.is_none()
                && matches!(*errno, libc::ENOPROTOOPT | libc::EOPNOTSUPP)
                && observed.raw_return == -1
                && observed.errno == Some(*errno)
                && observed.bytes.is_empty() =>
            {
                next.cursor_observed = true;
                next.current_cursor = None;
            }
            (
                NetworkStreamPhysicalEffect::SetPeekOffset { value },
                NetworkStreamPhysicalResult::Unit,
            ) if observed.raw_return == 0
                && observed.errno.is_none()
                && observed.bytes.is_empty() =>
            {
                next.current_cursor = Some(*value)
            }
            (NetworkStreamPhysicalEffect::Peek { maximum }, _) => {
                let completion = observed
                    .helper_copy
                    .as_ref()
                    .ok_or_else(|| invalid("shared Peek lost canonical helper"))?;
                let binding = completion.binding();
                if binding.owner() != origin.owner
                    || binding.call() != origin.call
                    || binding.lease() != origin.lease
                    || binding.effect() != &step.effect
                    || self.stream_calls[&origin.call]
                        .helper_copy
                        .as_ref()
                        .is_none_or(|b| !Arc::ptr_eq(b, binding))
                    || record.source.is_some()
                {
                    return Err(invalid("shared Peek replaced exact raw binding"));
                }
                if completion.capture().manifest.returned > 0 {
                    let bytes = PrivateSource::checked(completion)?;
                    if bytes.length > *maximum
                        || bytes.length < next.retained_prefix
                        || observed.errno.is_some()
                        || observed.raw_return != bytes.length as i64
                        || observed.confirmation
                            != (NetworkStreamPhysicalResult::Peeked {
                                count: bytes.length,
                            })
                        || observed.bytes != completion.capture().committed[next.retained_prefix..]
                    {
                        return Err(invalid("shared Peek changed canonical result/suffix"));
                    }
                    self.check_private_receive_cut_for_file(origin.binding.open_file, &bytes)?;
                    self.check_private_published_prefix(&next, &bytes)?;
                    for state in self.stream_calls.values() {
                        if state.open_file == Some(origin.binding.open_file)
                            && let Some(SharedAttempt::Wait(w)) = &state.shared_attempt
                        {
                            for history in &w.record_history {
                                if let Some(Source::Bytes(old)) = &history.source {
                                    bytes.check_overlap(old)?;
                                }
                            }
                        }
                    }
                    next.peek = Some(Ok(bytes.length));
                    source = Some(Source::Bytes(bytes));
                } else {
                    let empty = Arc::new(RecordNoStore::checked(observed)?);
                    if next.retained_prefix != 0 {
                        return Err(invalid("empty Peek contradicts retained prefix"));
                    }
                    next.peek = Some(match empty.kind() {
                        NoStoreReturn::Eof => Ok(0),
                        NoStoreReturn::WouldBlock => Err(libc::EAGAIN),
                    });
                    source = Some(Source::Empty(empty));
                }
            }
            (
                NetworkStreamPhysicalEffect::PollState,
                NetworkStreamPhysicalResult::PollState { revents },
            ) if observed.raw_return == i64::from(*revents != 0)
                && observed.errno.is_none()
                && observed.bytes.is_empty()
                && observed.helper_copy.is_none() =>
            {
                next.poll = Some(*revents)
            }
            _ => {
                return Err(invalid(
                    "shared probe did not complete its exact native effect",
                ));
            }
        }
        if !matches!(step.effect, NetworkStreamPhysicalEffect::Peek { .. })
            && observed.helper_copy.is_some()
        {
            return Err(invalid("shared control carries unrelated helper proof"));
        }
        if let Some(source) = &source {
            self.retain_native_receive_attempts(
                origin.owner,
                origin.call,
                source.completion().attempts(),
            )?;
        }
        next.pending = None;
        self.shadow_probes.insert(origin.lease, next);
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&origin.call)
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let record = wait.record_probe.as_mut().unwrap();
        if source.is_some() {
            record.source = source;
        }
        record.pending = None;
        record.effects.push(joined.clone());
        record.confirmed_at = Some(now);
        // Classification may retain an explicit unsupported terminal/readiness
        // result. The actual effect remains confirmed and owned, never retried.
        Ok(self
            .shared_record_probe_progress(origin, now)
            .unwrap_or_else(|error| SharedProbeProgress::Refused(error.to_string())))
    }

    pub(crate) fn complete_shared_record_pending(
        &mut self,
        proof: &ConfirmedSharedRecordPending<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<CompletedSharedAttempt, NetworkReplayError> {
        let origin = proof.origin();
        if matches!(&self.stream_calls.get(&origin.call).and_then(|s| s.shared_attempt.as_ref()), Some(SharedAttempt::Wait(w)) if w.poll_output.is_some()) {
            return Err(invalid("shared native transition retains Poll output custody"));
        }

        self.shared_active(origin.call, grant)?;
        if self.shared_record_probe_progress(origin, now)? != SharedProbeProgress::PendingCandidate
        {
            return Err(invalid(
                "shared pending completion requires exact nonready native result",
            ));
        }
        let (_, record, probe) = self.record_probe(origin)?;
        if probe.pending.is_some()
            || record.pending.is_some()
            || record.effects.is_empty()
            || !matches!(
                record.effects.last().unwrap().step().effect(),
                NetworkStreamPhysicalEffect::PollState
            )
        {
            return Err(invalid("shared completion lacks final actual PollState"));
        }
        let (wait, _, _) = self.record_probe(origin)?;
        // A raw scan alone is insufficient for a guest Poll: retain the exact
        // successful typed trace append before retiring this attempt. Receive's
        // private post-Peek scan remains unlogged and owns no such receipt.
        self.check_shared_record_poll_publication(origin, grant, now)?;
        let covered = |attempt: &crate::network_replay::native_receive::RetainedAttempt| {
            let current = record.source.as_ref().is_some_and(|source| {
                source
                    .completion()
                    .attempts()
                    .iter()
                    .any(|actual| actual.same(&attempt.receipt))
            });
            let historical = wait.record_history.iter().any(|history| {
                history.source.as_ref().is_some_and(|source| {
                    source
                        .completion()
                        .attempts()
                        .iter()
                        .any(|actual| actual.same(&attempt.receipt))
                })
            });
            attempt.joined && (current || historical)
        };
        if !self.stream_calls[&origin.call]
            .native_receive
            .iter()
            .all(covered)
        {
            return Err(invalid(
                "shared completion has uncovered physical receive history",
            ));
        }
        let history = Arc::new(RecordHistory {
            origin: origin.clone(),
            effects: record.effects.clone(),
            source: record.source.clone(),
            observed_at: now,
            published_poll: record.published_poll.clone(),
        });
        let completion = Arc::new(Completion {
            owner: origin.owner,
            call: origin.call,
            ordinal: origin.ordinal,
            epoch: origin.epoch,
            file: origin.binding.open_file,
            observed_at: now,
            observation: Observation::Record(history.clone()),
        });
        // Identity, lifetime, physical completion and unchanged-control checks
        // all precede these infallible removals. Keep every source/attempt owner.
        self.shadow_probes
            .remove(&origin.lease)
            .expect("checked exact probe");
        self.finish_socket_control(
            origin.owner,
            origin.lease,
            NetworkSocketControlFinish::Unchanged,
        )
        .expect("preflighted shared unchanged control under same engine lock");
        let state = self.stream_calls.get_mut(&origin.call).unwrap();
        state.helper_copy = None;
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            unreachable!()
        };
        wait.record_probe = None;
        wait.record_history.push(history);
        let AttemptPhase::Active {
            completion: actual, ..
        } = &mut wait.phase
        else {
            unreachable!()
        };
        *actual = Some(completion.clone());
        Ok(CompletedSharedAttempt(completion))
    }

    pub(super) fn shared_completed_observation_matches(
        &self,
        call: NetworkStreamCallId,
        wait: &SharedWait,
        completion: &Completion,
    ) -> Result<bool, NetworkReplayError> {
        match &completion.observation {
            Observation::Record(history) => Ok(self.mode() == NetworkEngineMode::Record
                && history.origin.call == call
                && history.origin.epoch == completion.epoch
                && history.origin.ordinal == completion.ordinal
                && history.observed_at == completion.observed_at
                && wait.record_probe.is_none()
                && wait.record_history.iter().any(|h| Arc::ptr_eq(h, history))
                && self.shared_record_history_covers(&self.stream_calls[&call])
                && self.shared_record_poll_history_matches(history)),
            _ => Ok(
                self.replay_shared_wait_observation(call, wait, completion.observed_at)?
                    == completion.observation,
            ),
        }
    }

    pub(crate) fn shared_record_poll_source(
        &self,
        origin: &Arc<SharedRecordProbe>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<SharedRecordPollSource, NetworkReplayError> {
        self.shared_active(origin.call, grant)?;
        let (_, record, probe) = self.record_probe(origin)?;
        if record.pending.is_some()
            || probe.pending.is_some()
            || now < origin.entry.not_before_global_time
            || record.confirmed_at != Some(now)
        {
            return Err(invalid(
                "shared Poll source retains an unfinished native effect",
            ));
        }
        let scan = record
            .effects
            .last()
            .filter(|last| matches!(last.step().effect(), NetworkStreamPhysicalEffect::PollState))
            .ok_or_else(|| invalid("shared Poll source lacks its last joined full scan"))?;
        if probe.poll.is_none() {
            return Err(invalid("shared Poll source lacks confirmed mask"));
        }
        Ok(SharedRecordPollSource {
            origin: origin.clone(),
            scan: scan.clone(),
            consumed: self.channels[&probe.channel].inbound_consumed,
            channel: probe.channel,
            observed_at: now,
        })
    }

    /// Revalidate the same actual source inside the publisher's engine
    /// transaction; neither this view nor numeric control metadata is a grant.
    pub(crate) fn validate_shared_record_poll_source(
        &self,
        source: &SharedRecordPollSource,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.shared_active(source.origin.call, grant)?;
        let (_, record, probe) = self.record_probe(&source.origin)?;
        if now != source.observed_at
            || record.confirmed_at != Some(source.observed_at)
            || record.pending.is_some()
            || probe.pending.is_some()
            || source.channel != probe.channel
            || self.channels[&probe.channel].inbound_consumed != source.consumed
            || record
                .effects
                .last()
                .is_none_or(|actual| !Arc::ptr_eq(actual, &source.scan))
            || probe.poll != Some(source.revents())
        {
            return Err(invalid(
                "shared Poll publication changed original actual scan/frontier",
            ));
        }
        Ok(())
    }

    pub(crate) fn shared_record_probe_effects(
        &self,
        origin: &Arc<SharedRecordProbe>,
    ) -> Result<&[Arc<JoinedSharedEffect>], NetworkReplayError> {
        Ok(&self.record_probe(origin)?.1.effects)
    }
}
impl ProbeState {
    pub(super) fn receive_effects(&self) -> &[Arc<JoinedSharedEffect>] {
        &self.effects
    }
    pub(super) fn receive_history(&self, now: LogicalTime) -> Arc<RecordHistory> {
        Arc::new(RecordHistory {
            origin: self.origin.clone(),
            effects: self.effects.clone(),
            source: self.source.clone(),
            observed_at: now,
            published_poll: self.published_poll.clone(),
        })
    }
}
impl NetworkReplayEngine {
    /// Separate source view for the output consumer. The existing probe and
    /// Pending issuer keep all their original predicates.
    pub(super) fn shared_record_receive_snapshot(
        &self,
        origin: &Arc<SharedRecordProbe>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<super::record_receive::RecordReceiveSnapshot, NetworkReplayError> {
        self.shared_active(origin.call, grant)?;
        if self.shared_record_probe_progress(origin, now)? != SharedProbeProgress::EligibleSource {
            return Err(invalid(
                "Record receive output requires its actual eligible source",
            ));
        }
        let (wait, record, probe) = self.record_probe(origin)?;
        let SharedWaitIntent::Receive(policy) = &wait.intent else {
            return Err(invalid("Record byte source is not original Receive"));
        };
        if wait.record_receive.is_some()
            || wait.poll_output.is_some()
            || record.pending.is_some()
            || record.published_poll.is_some()
            || probe.pending.is_some()
            || !probe.cursor_restored()
            || record
                .effects
                .last()
                .is_none_or(|e| e.step().effect() != &NetworkStreamPhysicalEffect::PollState)
            || record.confirmed_at.is_none_or(|at| at > now)
        {
            return Err(invalid(
                "Record receive source retains unfinished cursor/worker/output history",
            ));
        }
        let source = match record
            .source
            .as_ref()
            .ok_or_else(|| invalid("Record receive has no canonical helper source"))?
        {
            Source::Bytes(s) => super::record_receive::ReceiveSource::Bytes(s.clone()),
            Source::Empty(s) => super::record_receive::ReceiveSource::Empty(s.clone()),
        };
        Ok(super::record_receive::RecordReceiveSnapshot {
            origin: origin.clone(),
            binding: origin.binding,
            policy: policy.clone(),
            physical: origin.physical,
            consumed: origin.consumed,
            ordinal: origin.ordinal,
            epoch: origin.epoch,
            entry: origin.entry.clone(),
            control: origin.control,
            low_water: origin.low_water,
            record: record.clone(),
            probe: probe.clone(),
            source,
            at: now,
        })
    }
}

impl NetworkReplayEngine {
    pub(super) fn shared_record_history_receipt_covered(
        &self,
        wait: &SharedWait,
        receipt: &crate::network_runtime::original_read_copy::NativeAttempt,
    ) -> bool {
        wait.record_history.iter().any(|h| {
            h.source
                .as_ref()
                .is_some_and(|s| s.completion().attempts().iter().any(|r| r.same(receipt)))
        })
    }
}
