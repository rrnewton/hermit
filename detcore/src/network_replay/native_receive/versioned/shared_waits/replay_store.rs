//! Shared receive selection is not a destination capability. The original Call
//! retains its exact Delivery and actual backend attempt until a full commit.
use super::*;
use crate::tool_global::SharedStoreAttempt;

#[derive(Debug, Clone)]
struct Selection {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    root: Arc<crate::network_runtime::ForegroundRoot>,
    policy: Arc<crate::tool_global::SavedReceivePolicy>,
    ordinal: u64,
    epoch: u64,
    raw: (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs),
    capacity: usize,
    target: usize,
    file: OpenFileId,
    channel: NetworkChannelId,
    before: u64,
    generation: Option<u64>,
    control: u64,
    consume_epoch: u64,
    cursor: Option<i32>,
    observed_at: LogicalTime,
}
impl Selection {
    fn same(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.call == other.call
            && Arc::ptr_eq(&self.root, &other.root)
            && Arc::ptr_eq(&self.policy, &other.policy)
            && self.ordinal == other.ordinal
            && self.epoch == other.epoch
            && self.raw == other.raw
            && self.capacity == other.capacity
            && self.target == other.target
            && self.file == other.file
            && self.channel == other.channel
            && self.before == other.before
            && self.generation == other.generation
            && self.control == other.control
            && self.consume_epoch == other.consume_epoch
            && self.cursor == other.cursor
            && self.observed_at == other.observed_at
    }
}
#[derive(Debug)]
pub(crate) enum SharedReplayReceivePlan {
    Bytes(SharedReplayBytesPlan),
    NoStore(SharedReplayNoStorePlan),
    Wait,
}
#[derive(Debug)]
pub(crate) struct SharedReplayBytesPlan {
    selected: Selection,
    bytes: Vec<u8>,
}
impl SharedReplayBytesPlan {
    #[cfg(test)]
    pub(crate) fn length(&self) -> usize {
        self.bytes.len()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyKind {
    Eof { ordinal: u64, repeated: bool },
    WouldBlock,
    TimedOut,
}
#[derive(Debug)]
pub(crate) struct SharedReplayNoStorePlan {
    selected: Selection,
    kind: EmptyKind,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SharedNoStoreResult {
    Eof,
    WouldBlock { timed_out: bool },
}
#[derive(Debug)]
pub(crate) struct SharedReplaySource {
    selected: Selection,
    lease: NetworkStreamLeaseId,
    bytes: Vec<u8>,
}
impl SharedReplaySource {
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.selected.owner
    }
    pub(crate) fn call(&self) -> NetworkStreamCallId {
        self.selected.call
    }
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        &self.selected.root
    }
    pub(crate) fn raw(&self) -> (reverie::syscalls::Sysno, reverie::syscalls::SyscallArgs) {
        self.selected.raw
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    #[cfg(test)]
    pub(crate) fn lease(&self) -> NetworkStreamLeaseId {
        self.lease
    }
}
#[derive(Debug, Clone)]
pub(super) struct SharedOutput {
    source: Arc<SharedReplaySource>,
    // The ordinary path has exactly one. Retaining a duplicate is evidence of
    // a protocol failure, never permission to overwrite or forget its effect.
    attempts: Vec<Arc<SharedStoreAttempt>>,
}

impl NetworkReplayEngine {
    fn shared_receive_selection(
        &self,
        call: NetworkStreamCallId,
        owner: NetworkStreamOwner,
        now: LogicalTime,
        reserved: bool,
    ) -> Result<Selection, NetworkReplayError> {
        let (state, wait) = self.shared_wait(owner, call)?;
        let SharedWaitIntent::Receive(policy) = &wait.intent else {
            return Err(invalid(
                "shared bytes require their original receive intent",
            ));
        };
        let AttemptPhase::Active {
            ordinal,
            epoch,
            entry,
            completion: None,
        } = &wait.phase
        else {
            return Err(invalid("shared bytes lack an unused active attempt"));
        };
        self.shared_wait_non_output_debts_settled(call, state)?;
        let raw = policy.raw();
        if self.mode() != NetworkEngineMode::Replay
            || state.physical_pin_required
            || now < entry.began()
            || now < policy.started()
            || !self.socket_controls.is_empty()
            || (!reserved && wait.output.is_some())
            || !matches!(
                raw.0,
                reverie::syscalls::Sysno::read | reverie::syscalls::Sysno::recvfrom
            )
            || (raw.0 == reverie::syscalls::Sysno::recvfrom
                && (raw.1.arg3 != 0 || raw.1.arg4 != 0 || raw.1.arg5 != 0))
            || raw.1.arg0 as i32 != wait.binding.slot.fd
            || raw.1.arg2 == 0
            || raw.1.arg2 > NETWORK_STREAM_CHUNK_LIMIT
            || policy.target() == 0
            || policy.target() > raw.1.arg2
        {
            return Err(invalid(
                "shared selection changed original scalar receive profile",
            ));
        }
        let file = wait.binding.open_file;
        self.validate_stream_call_lifetime(owner, call, file)?;
        let channel = self.bound_channel(file)?;
        let queue = &self.channels[&channel];
        let socket = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(file))?;
        if queue.transport.is_datagram()
            || queue.local_read_shutdown
            || self.stream_role(channel)? != NetworkEndpointRoleV2::OutboundClient
            || !matches!(&self.mode, EngineState::Native(native) if native.replay.as_ref().is_some_and(|r| r.connected.contains(&channel)))
        {
            return Err(invalid(
                "shared selection requires its open connected stream",
            ));
        }
        Ok(Selection {
            owner,
            call,
            root: wait.root.clone(),
            policy: policy.clone(),
            ordinal: *ordinal,
            epoch: *epoch,
            raw,
            capacity: raw.1.arg2,
            target: policy.target(),
            file,
            channel,
            before: queue.inbound_consumed,
            generation: queue.receive_input_generation,
            control: queue.local_control_generation,
            consume_epoch: socket.consume_epoch,
            cursor: socket.options.peek_offset,
            observed_at: now,
        })
    }
    fn shared_receive_plan_at(
        &self,
        selected: Selection,
    ) -> Result<SharedReplayReceivePlan, NetworkReplayError> {
        let queue = &self.channels[&selected.channel];
        let bytes = super::super::super::replay_store::plain_prefix(queue, selected.capacity)?;
        let expired = selected
            .policy
            .deadline()
            .is_some_and(|d| selected.observed_at >= d);
        if !bytes.is_empty() {
            let end = selected
                .before
                .checked_add(bytes.len() as u64)
                .ok_or(NetworkReplayError::Overflow)?;
            let eof = self
                .native_replay_eof(selected.channel, end)?
                .is_some_and(|(_, released, consumed)| released && !consumed);
            if bytes.len() < selected.target
                && let Some(terminal) = queue.inbound.iter().find(|item| {
                    !matches!(
                        item,
                        InboundOutcome::Stream {
                            ancillary: None,
                            message_flags: 0,
                            requires_message_io: false,
                            ..
                        }
                    )
                })
                && !(eof
                    && matches!(terminal, InboundOutcome::PeerShutdown {
                    stream_offset, direction: NetworkShutdownV2::Write
                } if *stream_offset == end))
            {
                return Err(invalid(
                    "shared short prefix has an unsupported error/control boundary",
                ));
            }
            if bytes.len() >= selected.target || selected.policy.nonblocking() || expired || eof {
                return Ok(SharedReplayReceivePlan::Bytes(SharedReplayBytesPlan {
                    selected,
                    bytes,
                }));
            }
            return Ok(SharedReplayReceivePlan::Wait);
        }
        let kind = match queue.inbound.front() {
            Some(InboundOutcome::PeerShutdown {
                stream_offset,
                direction: NetworkShutdownV2::Write,
            }) => {
                if queue.peer_write_closed || *stream_offset != selected.before {
                    return Err(invalid("shared EOF changed exact input byte frontier"));
                }
                let Some((ordinal, true, false)) =
                    self.native_replay_eof(selected.channel, selected.before)?
                else {
                    return Err(invalid(
                        "shared EOF lacks its released unconsumed trace identity",
                    ));
                };
                EmptyKind::Eof {
                    ordinal,
                    repeated: false,
                }
            }
            None if queue.peer_write_closed => {
                let Some((ordinal, true, true)) =
                    self.native_replay_eof(selected.channel, selected.before)?
                else {
                    return Err(invalid(
                        "shared repeated EOF lost its consumed trace identity",
                    ));
                };
                EmptyKind::Eof {
                    ordinal,
                    repeated: true,
                }
            }
            None if selected.policy.nonblocking() => EmptyKind::WouldBlock,
            None if expired => EmptyKind::TimedOut,
            None => return Ok(SharedReplayReceivePlan::Wait),
            _ => {
                return Err(invalid(
                    "shared no-store selected bytes/error/unsupported control",
                ));
            }
        };
        Ok(SharedReplayReceivePlan::NoStore(SharedReplayNoStorePlan {
            selected,
            kind,
        }))
    }

    pub(crate) fn plan_shared_replay_receive(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<SharedReplayReceivePlan, NetworkReplayError> {
        if owner != grant.owner() {
            return Err(invalid("shared plan changed current owner"));
        }
        self.shared_active(call, grant)?;
        self.shared_receive_selection(call, owner, now, false)?;
        self.release_native_eligible(now)?;
        self.shared_active(call, grant)?;
        let selected = self.shared_receive_selection(call, owner, now, false)?;
        self.shared_receive_plan_at(selected)
    }

    pub(crate) fn reserve_shared_replay_store(
        &mut self,
        plan: &SharedReplayBytesPlan,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
    ) -> Result<Arc<SharedReplaySource>, NetworkReplayError> {
        let p = &plan.selected;
        self.shared_active(p.call, grant)?;
        let current = self.shared_receive_selection(p.call, grant.owner(), p.observed_at, false)?;
        if !p.same(&current)
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !admission.matches_selected(self, p.call)?
            || !self.shared_census_matches_grant(Some(p.call), None, grant)?
        {
            return Err(invalid(
                "shared bytes reservation changed original attempt/census",
            ));
        }
        let SharedReplayReceivePlan::Bytes(current) = self.shared_receive_plan_at(current)? else {
            return Err(invalid(
                "shared bytes no longer select a full saved-target outcome",
            ));
        };
        if current.bytes != plan.bytes {
            return Err(invalid("shared bytes changed selected immutable prefix"));
        }
        p.before
            .checked_add(plan.bytes.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        p.consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let lease = self.allocate_stream_lease()?;
        let source = Arc::new(SharedReplaySource {
            selected: p.clone(),
            lease,
            bytes: plan.bytes.clone(),
        });
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner: p.owner,
                open_file: p.file,
                channel: p.channel,
                abandoned: false,
                kind: StreamOperationKind::Delivery {
                    at_offset: p.before,
                    peek_offset: 0,
                    selection_len: source.bytes.len(),
                    outcome: NetworkStreamChunkOutcome::Bytes(source.bytes.clone()),
                },
            },
        );
        self.stream_delivery.insert(p.file, lease);
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&p.call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.output = Some(SharedOutput {
            source: source.clone(),
            attempts: Vec::new(),
        });
        Ok(source)
    }

    fn check_shared_replay_source(
        &self,
        source: &Arc<SharedReplaySource>,
    ) -> Result<(), NetworkReplayError> {
        let p = &source.selected;
        let current = self.shared_receive_selection(p.call, p.owner, p.observed_at, true)?;
        let (_, wait) = self.shared_wait(p.owner, p.call)?;
        let operation = self.owned_stream_operation(p.owner, source.lease)?;
        if !p.same(&current)
            || wait
                .output
                .as_ref()
                .is_none_or(|o| !Arc::ptr_eq(&o.source, source))
            || operation.abandoned
            || operation.open_file != p.file
            || operation.channel != p.channel
            || self.stream_delivery.get(&p.file) != Some(&source.lease)
            || self.shadow_deliveries.contains_key(&source.lease)
            || !matches!(&operation.kind, StreamOperationKind::Delivery { at_offset, peek_offset: 0, selection_len, outcome: NetworkStreamChunkOutcome::Bytes(bytes) }
                if *at_offset == p.before && *selection_len == source.bytes.len() && bytes == &source.bytes)
            || super::super::super::replay_store::plain_prefix(
                &self.channels[&p.channel],
                source.bytes.len(),
            )? != source.bytes
        {
            return Err(invalid(
                "shared output lost exact retained Call/Delivery/frontier",
            ));
        }
        Ok(())
    }

    pub(crate) fn shared_output_peer_census(
        &self,
        source: &Arc<SharedReplaySource>,
    ) -> Result<SharedCallCensus, NetworkReplayError> {
        self.check_shared_replay_source(source)?;
        self.shared_call_census_except_delivery(None, Some(source.call()), Some(source.lease))
    }

    pub(in crate::network_replay) fn has_shared_output_lease(
        &self,
        lease: NetworkStreamLeaseId,
    ) -> bool {
        self.stream_calls.values().any(|state| matches!(&state.shared_attempt,
            Some(SharedAttempt::Wait(wait)) if wait.output.as_ref().is_some_and(|o| o.source.lease == lease)))
    }

    /// Synchronous actual-backend callback calls this before releasing its hold.
    /// Retain first, including failure/partial/postcheck effects; current-owner
    /// checks belong to the later consuming commit, not to forgetting evidence.
    pub(crate) fn retain_shared_replay_store_attempt(
        &mut self,
        attempt: SharedStoreAttempt,
    ) -> Result<(), NetworkReplayError> {
        let source = attempt.source().clone();
        let state = self
            .stream_calls
            .get_mut(&source.call())
            .ok_or(NetworkReplayError::UnknownStreamCall(source.call()))?;
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            return Err(invalid("actual shared store lost original Call"));
        };
        let output = wait
            .output
            .as_mut()
            .ok_or_else(|| invalid("actual shared store lost reservation"))?;
        if !Arc::ptr_eq(&output.source, &source) {
            return Err(invalid("actual shared store changed original source"));
        }
        output.attempts.push(Arc::new(attempt));
        if output.attempts.len() != 1 {
            return Err(invalid(
                "shared store attempted more than once; every effect remains retained",
            ));
        }
        Ok(())
    }

    pub(crate) fn complete_shared_replay_store(
        &mut self,
        source: &Arc<SharedReplaySource>,
        grant: &SharedMmForegroundObservation<'_>,
    ) -> Result<usize, NetworkReplayError> {
        self.shared_active(source.call(), grant)?;
        self.check_shared_replay_source(source)?;
        if self
            .shared_output_peer_census(source)?
            .rows
            .iter()
            .any(|row| !grant.contains_root(&row.root))
        {
            return Err(invalid(
                "shared output changed complete current physical census",
            ));
        }
        let (_, wait) = self.shared_wait(source.owner(), source.call())?;
        let output = wait.output.as_ref().unwrap();
        if output.attempts.len() != 1
            || !matches!(output.attempts[0].outcome(),
            reverie::syscalls::NativeUserStoreOutcome::Attempted { raw: Ok(count), postcheck: Ok(()) } if *count == source.bytes.len())
        {
            return Err(invalid(
                "shared receive lacks actual exact full store and successful postcheck",
            ));
        }
        let p = &source.selected;
        let after = p
            .before
            .checked_add(source.bytes.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let epoch = p
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let cursor = p.cursor.map(|value| {
            if value >= 0 {
                value.wrapping_sub(source.bytes.len() as i32).max(0)
            } else {
                value
            }
        });
        let queue = self.channels.get_mut(&p.channel).unwrap();
        let mut remaining = source.bytes.len();
        while remaining != 0 {
            let InboundOutcome::Stream { bytes, .. } = queue.inbound.front_mut().unwrap() else {
                unreachable!()
            };
            let count = remaining.min(bytes.len());
            bytes.drain(..count);
            remaining -= count;
            if bytes.is_empty() {
                queue.inbound.pop_front();
            }
        }
        queue.inbound_consumed = after;
        queue.refresh_readiness();
        let socket = self
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&p.file)
            .unwrap();
        socket.consume_epoch = epoch;
        socket.options.peek_offset = cursor;
        self.stream_delivery.remove(&p.file);
        self.stream_operations.remove(&source.lease);
        self.release_stream_call_lifetime(p.owner, p.call, p.file)
            .expect("shared store prevalidated the exact Call lease under this engine lock");
        self.stream_calls.remove(&p.call);
        self.complete_deferred_retirement(p.file);
        Ok(source.bytes.len())
    }

    pub(crate) fn complete_shared_replay_no_store(
        &mut self,
        plan: SharedReplayNoStorePlan,
        grant: &SharedMmForegroundObservation<'_>,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<SharedNoStoreResult, NetworkReplayError> {
        let p = &plan.selected;
        self.shared_active(p.call, grant)?;
        let current = self.shared_receive_selection(p.call, grant.owner(), now, false)?;
        if !p.same(&current)
            || !admission.matches_selected(self, p.call)?
            || !Arc::ptr_eq(admission.root(), grant.root())
            || !self.shared_census_matches_grant(Some(p.call), None, grant)?
        {
            return Err(invalid(
                "shared no-store changed exact attempt/grant/frontier",
            ));
        }
        let SharedReplayReceivePlan::NoStore(current) = self.shared_receive_plan_at(current)?
        else {
            return Err(invalid("shared no-store changed before commit"));
        };
        if current.kind != plan.kind {
            return Err(invalid("shared no-store changed trace identity"));
        }
        let epoch = if matches!(
            plan.kind,
            EmptyKind::Eof {
                repeated: false,
                ..
            }
        ) {
            p.consume_epoch
                .checked_add(1)
                .ok_or(NetworkReplayError::Overflow)?
        } else {
            p.consume_epoch
        };
        if let EmptyKind::Eof {
            ordinal,
            repeated: false,
        } = plan.kind
        {
            self.mark_native_replay_eof_consumed(ordinal);
            let queue = self.channels.get_mut(&p.channel).unwrap();
            queue.inbound.pop_front();
            queue.peer_write_closed = true;
            queue.refresh_readiness();
            self.shadow
                .as_mut()
                .unwrap()
                .sockets
                .get_mut(&p.file)
                .unwrap()
                .consume_epoch = epoch;
        }
        self.release_stream_call_lifetime(p.owner, p.call, p.file)
            .expect("shared no-store prevalidated the exact Call lease under this engine lock");
        self.stream_calls.remove(&p.call);
        self.complete_deferred_retirement(p.file);
        Ok(match plan.kind {
            EmptyKind::Eof { .. } => SharedNoStoreResult::Eof,
            EmptyKind::WouldBlock | EmptyKind::TimedOut => SharedNoStoreResult::WouldBlock {
                timed_out: plan.kind == EmptyKind::TimedOut,
            },
        })
    }
}
