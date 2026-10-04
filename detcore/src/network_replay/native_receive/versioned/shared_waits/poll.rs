//! Nonconsuming Poll output binds an immutable observation to the original
//! Call. A trace append, a native scan, and an actual user store are distinct.
use super::*;
use crate::network_runtime::shared_waits::ConfirmedSharedRecordPoll;
use crate::tool_global::SharedPollStoreAttempt;

#[derive(Debug, Clone)]
enum Provenance {
    Replay(super::super::raw_poll::SharedPollSnapshot),
    Record(Arc<SharedRecordPollPublication>),
}
impl Provenance {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Replay(a), Self::Replay(b)) => a == b,
            (Self::Record(a), Self::Record(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
    fn sample(&self) -> (LogicalTime, i16) {
        match self {
            Self::Replay(s) => (s.observed_at, s.revents),
            Self::Record(p) => (p.source().observed_at(), p.source().revents()),
        }
    }
}
#[derive(Debug, Clone)]
struct PollSelection {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    intent: Arc<OriginalPollIntent>,
    binding: crate::types::FdSlotBinding,
    ordinal: u64,
    epoch: u64,
    channel: NetworkChannelId,
    consumed: u64,
    control: u64,
    low_water: u32,
    at: LogicalTime,
    source: Provenance,
}
impl PollSelection {
    fn same(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.call == other.call
            && Arc::ptr_eq(&self.root, &other.root)
            && Arc::ptr_eq(&self.intent, &other.intent)
            && self.binding == other.binding
            && self.ordinal == other.ordinal
            && self.epoch == other.epoch
            && self.channel == other.channel
            && self.consumed == other.consumed
            && self.control == other.control
            && self.low_water == other.low_water
            && self.at == other.at
            && self.source.same(&other.source)
    }
    fn revents(&self) -> i16 {
        detcore_model::network_trace::tcp_poll_row_mask(
            self.source.sample().1,
            self.intent.rows[0].1,
        )
    }
}
#[derive(Debug)]
pub(crate) enum SharedPollDecision {
    Pending,
    Store(SharedPollPlan),
}
#[derive(Debug)]
pub(crate) struct SharedPollPlan {
    selected: PollSelection,
}
#[derive(Debug)]
pub(crate) struct SharedPollSource {
    selected: PollSelection,
}
impl SharedPollSource {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.selected.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.selected.call
    }
    pub(crate) fn open_file(&self) -> OpenFileId {
        self.selected.binding.open_file
    }
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        &self.selected.root
    }
    pub(crate) fn raw(&self) -> (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs) {
        self.selected.intent.raw
    }
    pub(crate) fn input(&self) -> (i32, i16, i32) {
        (
            self.selected.binding.slot.fd,
            self.selected.intent.rows[0].1,
            self.raw().1.arg2 as i32,
        )
    }
    pub(crate) fn revents(&self) -> i16 {
        self.selected.revents()
    }
    pub(crate) fn count(&self) -> i64 {
        i64::from(self.revents() != 0)
    }
    pub(crate) fn record_publication(&self) -> Option<&Arc<SharedRecordPollPublication>> {
        match &self.selected.source {
            Provenance::Record(p) => Some(p),
            _ => None,
        }
    }
}
#[derive(Debug, Clone)]
pub(super) struct SharedPollOutput {
    source: Arc<SharedPollSource>,
    attempts: Vec<Arc<SharedPollStoreAttempt>>,
    committed: bool,
}
/// An exclusive borrow of the original reservation. No engine operation can
/// remove/reassign its Call while the backend callback retains its outcome.
#[derive(Debug)]
pub(crate) struct SharedPollStoreRetainer<'a> {
    output: &'a mut SharedPollOutput,
}
impl SharedPollStoreRetainer<'_> {
    pub(crate) fn retain(
        &mut self,
        attempt: SharedPollStoreAttempt,
    ) -> Result<(), NetworkReplayError> {
        let same = Arc::ptr_eq(attempt.source(), &self.output.source);
        self.output.attempts.push(Arc::new(attempt));
        if !same || self.output.attempts.len() != 1 {
            return Err(invalid(
                "actual Poll output source/attempt changed; all effects retained",
            ));
        }
        Ok(())
    }
}

impl NetworkReplayEngine {
    fn poll_selection(
        &self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
        source: Provenance,
        reserved: bool,
    ) -> Result<PollSelection, NetworkReplayError> {
        self.shared_active(call, grant)?;
        let (state, wait) = self.shared_wait(grant.owner(), call)?;
        let SharedWaitIntent::Poll(intent) = &wait.intent else {
            return Err(invalid("Poll output changed original intent"));
        };
        let AttemptPhase::Active {
            ordinal,
            epoch,
            entry,
            completion: None,
        } = &wait.phase
        else {
            return Err(invalid("Poll output lacks its unused active attempt"));
        };
        let raw = intent.raw;
        let timeout = raw.1.arg2 as i32;
        let exact_deadline = if timeout < 0 {
            None
        } else {
            Some(LogicalTime::from_nanos(
                intent
                    .started
                    .as_nanos()
                    .checked_add((timeout as u64) * 1_000_000)
                    .ok_or(NetworkReplayError::Overflow)?,
            ))
        };
        if timeout < 0
            || raw.0 != reverie::syscalls::Sysno::poll
            || raw.1.arg0 == 0
            || raw.1.arg1 != 1
            || intent.rows.as_slice() != [(wait.binding, libc::POLLIN)]
            || intent.deadline != exact_deadline
            || now < intent.started
            || now < entry.began()
            || (!reserved && wait.poll_output.is_some())
            || wait.output.is_some()
        {
            return Err(invalid(
                "Poll output changed finite original pointer/row/deadline",
            ));
        }
        self.validate_stream_call_lifetime(grant.owner(), call, wait.binding.open_file)?;
        let channel = self.bound_channel(wait.binding.open_file)?;
        let queue = &self.channels[&channel];
        let socket = &self
            .shadow
            .as_ref()
            .ok_or(NetworkReplayError::WrongMode)?
            .sockets[&wait.binding.open_file];
        if queue.transport.is_datagram()
            || queue.local_read_shutdown
            || self.stream_role(channel)? != NetworkEndpointRoleV2::OutboundClient
        {
            return Err(invalid(
                "Poll output requires its connected outbound stream",
            ));
        }
        match &source {
            Provenance::Replay(s) => {
                self.shared_wait_non_output_debts_settled(call, state)?;
                if self.mode() != NetworkEngineMode::Replay
                    || state.physical_pin_required
                    || !self.socket_controls.is_empty()
                    || !matches!(&self.mode, EngineState::Native(n) if n.replay.as_ref().is_some_and(|r| r.connected.contains(&channel)))
                    || self.shared_poll_snapshot(wait.binding.open_file)? != Some(*s)
                {
                    return Err(invalid("Poll output changed exact Replay snapshot"));
                }
            }
            Provenance::Record(p) => {
                self.check_shared_record_poll_output_publication(p, grant, now)?;
                if !state.physical_pin_required || p.source().origin().call() != call {
                    return Err(invalid("Poll output changed original native scan owner"));
                }
            }
        }
        let (sampled, _) = source.sample();
        // Record owns a scan within this Call. Replay observes persistent
        // released state at this selected current cut, bound by `at`/`same`;
        // its immutable source time also remains the final-zero deadline proof.
        if sampled > now || (matches!(&source, Provenance::Record(_)) && sampled < intent.started) {
            return Err(invalid(
                "Poll observation is outside its original call interval",
            ));
        }
        Ok(PollSelection {
            owner: grant.owner(),
            call,
            root: wait.root.clone(),
            intent: intent.clone(),
            binding: wait.binding,
            ordinal: *ordinal,
            epoch: *epoch,
            channel,
            consumed: queue.inbound_consumed,
            control: queue.local_control_generation,
            low_water: socket.options.receive_low_water,
            at: now,
            source,
        })
    }
    fn poll_decision(selected: PollSelection) -> Result<SharedPollDecision, NetworkReplayError> {
        // A real nonzero mask takes precedence over timeout, including final
        // scans at or after the original absolute deadline.
        if selected.revents() != 0 {
            return Ok(SharedPollDecision::Store(SharedPollPlan { selected }));
        }
        match selected.intent.deadline {
            Some(deadline) if selected.at >= deadline => {
                if selected.source.sample().0 < deadline {
                    return Err(invalid("Poll timeout lacks an actual final zero scan"));
                }
                Ok(SharedPollDecision::Store(SharedPollPlan { selected }))
            }
            _ => Ok(SharedPollDecision::Pending),
        }
    }
    fn replay_poll_admission(
        &self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
    ) -> Result<(), NetworkReplayError> {
        self.shared_active(call, grant)?;
        let (state, wait) = self.shared_wait(grant.owner(), call)?;
        self.shared_wait_debts_settled(call, state)?;
        let SharedWaitIntent::Poll(intent) = &wait.intent else {
            return Err(invalid("Poll changed original intent"));
        };
        let raw = intent.raw;
        let timeout = raw.1.arg2 as i32;
        if timeout < 0
            || raw.0 != reverie::syscalls::Sysno::poll
            || raw.1.arg0 == 0
            || raw.1.arg1 != 1
            || intent.rows.as_slice() != [(wait.binding, libc::POLLIN)]
            || intent.deadline
                != Some(LogicalTime::from_nanos(
                    intent
                        .started
                        .as_nanos()
                        .checked_add(timeout as u64 * 1_000_000)
                        .ok_or(NetworkReplayError::Overflow)?,
                ))
        {
            return Err(invalid(
                "Poll admission changed finite original row/deadline",
            ));
        }
        if !matches!(wait.intent, SharedWaitIntent::Poll(_))
            || self.mode() != NetworkEngineMode::Replay
            || !admission.matches_selected(self, call)?
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !self.shared_census_matches_grant(Some(call), None, grant)?
        {
            return Err(invalid(
                "Poll output changed complete selected Replay admission",
            ));
        }
        self.validate_stream_call_lifetime(grant.owner(), call, wait.binding.open_file)
    }
    pub(crate) fn plan_shared_replay_poll(
        &mut self,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<SharedPollDecision, NetworkReplayError> {
        self.replay_poll_admission(call, grant, admission)?;
        self.release_native_eligible(now)?;
        self.replay_poll_admission(call, grant, admission)?;
        let (_, wait) = self.shared_wait(grant.owner(), call)?;
        let Some(sample) = self.shared_poll_snapshot(wait.binding.open_file)? else {
            if wait.intent.deadline().is_some_and(|d| now >= d) {
                return Err(invalid("Poll timeout has no final trace sample"));
            }
            return Ok(SharedPollDecision::Pending);
        };
        Self::poll_decision(self.poll_selection(
            call,
            grant,
            now,
            Provenance::Replay(sample),
            false,
        )?)
    }
    pub(crate) fn plan_shared_record_poll(
        &self,
        publication: &Arc<SharedRecordPollPublication>,
        grant: &SharedMmForegroundObservation<'_>,
        proof: &ConfirmedSharedRecordPoll<'_>,
        now: LogicalTime,
    ) -> Result<SharedPollDecision, NetworkReplayError> {
        if !Arc::ptr_eq(publication.source().origin(), proof.origin()) {
            return Err(invalid("Poll changed actual joined source borrower"));
        }
        Self::poll_decision(self.poll_selection(
            proof.origin().call(),
            grant,
            now,
            Provenance::Record(publication.clone()),
            false,
        )?)
    }
    fn reserve_poll(
        &mut self,
        plan: SharedPollPlan,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedPollSource>, NetworkReplayError> {
        let current = self.poll_selection(
            plan.selected.call,
            grant,
            now,
            plan.selected.source.clone(),
            false,
        )?;
        if !current.same(&plan.selected)
            || !matches!(Self::poll_decision(current)?, SharedPollDecision::Store(_))
        {
            return Err(invalid("Poll reservation changed selected observation"));
        }
        let source = Arc::new(SharedPollSource {
            selected: plan.selected,
        });
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        wait.poll_output = Some(SharedPollOutput {
            source: source.clone(),
            attempts: vec![],
            committed: false,
        });
        Ok(source)
    }
    pub(crate) fn reserve_shared_replay_poll(
        &mut self,
        plan: SharedPollPlan,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedPollSource>, NetworkReplayError> {
        if !matches!(plan.selected.source, Provenance::Replay(_)) {
            return Err(NetworkReplayError::WrongMode);
        }
        self.replay_poll_admission(plan.selected.call, grant, admission)?;
        self.reserve_poll(plan, grant, now)
    }
    pub(crate) fn reserve_shared_record_poll(
        &mut self,
        plan: SharedPollPlan,
        grant: &SharedMmForegroundObservation<'_>,
        proof: &ConfirmedSharedRecordPoll<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedPollSource>, NetworkReplayError> {
        let Provenance::Record(p) = &plan.selected.source else {
            return Err(NetworkReplayError::WrongMode);
        };
        if !Arc::ptr_eq(p.source().origin(), proof.origin()) {
            return Err(invalid("Poll reservation changed native borrower"));
        }
        self.reserve_poll(plan, grant, now)
    }
    pub(crate) fn check_shared_poll_source(
        &self,
        source: &Arc<SharedPollSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let p = &source.selected;
        let current = self.poll_selection(p.call, grant, now, p.source.clone(), true)?;
        let (_, wait) = self.shared_wait(p.owner, p.call)?;
        if !p.same(&current)
            || wait
                .poll_output
                .as_ref()
                .is_none_or(|o| !Arc::ptr_eq(&o.source, source) || o.committed)
        {
            return Err(invalid("Poll store lost exact reserved source/frontier"));
        }
        let peers = self.shared_poll_peer_census(source)?;
        if peers.rows.iter().any(|row| !grant.contains_root(&row.root)) {
            return Err(invalid("Poll store lost complete current peer census"));
        }
        Ok(())
    }
    pub(crate) fn shared_poll_peer_census(
        &self,
        source: &Arc<SharedPollSource>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        match &source.selected.source {
            Provenance::Replay(_) => self.shared_call_census_excluding(None, Some(source.call())),
            Provenance::Record(p) => self.shared_record_probe_peers(p.source().origin()),
        }
    }
    pub(crate) fn with_shared_poll_store_retention<T>(
        &mut self,
        source: &Arc<SharedPollSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
        effect: impl FnOnce(&mut SharedPollStoreRetainer<'_>) -> T,
    ) -> Result<T, NetworkReplayError> {
        self.check_shared_poll_source(source, grant, now)?;
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let output = wait.poll_output.as_mut().unwrap();
        if !output.attempts.is_empty() {
            return Err(invalid("Poll store already attempted; retry is forbidden"));
        }
        Ok(effect(&mut SharedPollStoreRetainer { output }))
    }
    fn full_poll_store(
        &self,
        source: &Arc<SharedPollSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.check_shared_poll_source(source, grant, now)?;
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let output = wait.poll_output.as_ref().unwrap();
        if output.attempts.len() != 1
            || !Arc::ptr_eq(output.attempts[0].source(), source)
            || !matches!(
                output.attempts[0].outcome(),
                reverie::syscalls::NativeUserStoreOutcome::Attempted {
                    raw: Ok(2),
                    postcheck: Ok(())
                }
            )
        {
            return Err(invalid(
                "Poll lacks actual exact two-byte store and successful postcheck",
            ));
        }
        Ok(())
    }
    pub(crate) fn complete_shared_replay_poll_store(
        &mut self,
        source: &Arc<SharedPollSource>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<i64, NetworkReplayError> {
        if !matches!(source.selected.source, Provenance::Replay(_)) {
            return Err(NetworkReplayError::WrongMode);
        }
        self.full_poll_store(source, grant, now)?;
        self.release_stream_call_lifetime(source.owner(), source.call(), source.open_file())
            .expect("exact Poll lifetime prevalidated under the same engine lock");
        self.stream_calls.remove(&source.call());
        self.complete_deferred_retirement(source.open_file());
        Ok(source.count())
    }
    pub(crate) fn complete_shared_record_poll_store(
        &mut self,
        source: &Arc<SharedPollSource>,
        grant: &SharedMmForegroundObservation<'_>,
        proof: &ConfirmedSharedRecordPoll<'_>,
        now: LogicalTime,
    ) -> Result<i64, NetworkReplayError> {
        let Some(publication) = source.record_publication() else {
            return Err(NetworkReplayError::WrongMode);
        };
        if !Arc::ptr_eq(publication.source().origin(), proof.origin()) {
            return Err(invalid("Poll completion changed actual native borrower"));
        }
        self.full_poll_store(source, grant, now)?;
        self.settle_shared_record_poll_output(publication, grant, now)?;
        let Some(SharedAttempt::Wait(wait)) = &mut self
            .stream_calls
            .get_mut(&source.call())
            .unwrap()
            .shared_attempt
        else {
            unreachable!()
        };
        let output = wait.poll_output.as_mut().unwrap();
        output.committed = true;
        // The synchronous borrower retains the same Arc through backend exit.
        // Successful physical pin release may start only after it is dropped.
        output.attempts[0].release_interval_after_commit();
        Ok(source.count())
    }
    pub(crate) fn shared_record_poll_output_committed(
        &self,
        source: &Arc<SharedPollSource>,
    ) -> bool {
        self.stream_calls.get(&source.call()).is_some_and(|s| matches!(&s.shared_attempt,
            Some(SharedAttempt::Wait(w)) if w.poll_output.as_ref().is_some_and(|o| o.committed && Arc::ptr_eq(&o.source, source))))
    }
    pub(in crate::network_replay) fn check_shared_poll_release(
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
            return Err(invalid("shared release is not a Poll output"));
        };
        let Some(output) = &wait.poll_output else {
            return Err(invalid("shared Poll has no committed output"));
        };
        if self.mode() != NetworkEngineMode::Record
            || state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state.original.is_some()
            || state.replay_connect.is_some()
            || !state.physical_pin_required
            || state.open_file != Some(output.source.open_file())
            || !wait.root.is_current(owner)
            || !output.committed
            || output.attempts.len() != 1
            || !Arc::ptr_eq(output.attempts[0].source(), &output.source)
            || !matches!(
                output.attempts[0].outcome(),
                reverie::syscalls::NativeUserStoreOutcome::Attempted {
                    raw: Ok(2),
                    postcheck: Ok(())
                }
            )
            || output
                .source
                .record_publication()
                .is_none_or(|p| !self.shared_record_poll_output_history_matches(wait, p))
        {
            return Err(invalid(
                "shared release lacks exact successful Record Poll output",
            ));
        }
        self.shared_wait_non_output_debts_settled(call, state)?;
        self.validate_stream_call_lifetime(owner, call, output.source.open_file())
    }
}
