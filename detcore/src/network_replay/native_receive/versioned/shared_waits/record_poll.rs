//! One actual shared Poll scan, one immutable trace append. The receipt does
//! not certify a guest store or return, and never retires the original pin.
use super::*;
use crate::network_runtime::shared_waits::ConfirmedSharedRecordPoll;

#[derive(Debug)]
pub(crate) struct SharedRecordPollPublication {
    source: Arc<SharedRecordPollSource>,
    input: NetworkInputEventV4,
    node: NetworkReleaseNodeV4,
}
impl SharedRecordPollPublication {
    pub(crate) fn source(&self) -> &Arc<SharedRecordPollSource> {
        &self.source
    }
    pub(crate) fn input_ordinal(&self) -> u64 {
        self.input.ordinal
    }
}

impl NetworkReplayEngine {
    /// Consume the privately issued source once while the current runtime
    /// census/actual worker joins remain borrowed. No caller supplies a mask,
    /// ordinal, timestamp or completion boolean.
    pub(crate) fn publish_shared_record_poll(
        &mut self,
        source: SharedRecordPollSource,
        proof: &ConfirmedSharedRecordPoll<'_>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedRecordPollPublication>, NetworkReplayError> {
        self.validate_shared_record_poll_source(&source, grant, now)?;
        let origin = &source.origin;
        let (wait, record, _) = self.record_probe(origin)?;
        if !Arc::ptr_eq(origin, proof.origin())
            || !matches!(wait.intent, SharedWaitIntent::Poll(_))
            || record.published_poll.is_some()
        {
            return Err(invalid(
                "shared Poll publication changed source or was repeated",
            ));
        }
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some()
            || !matches!(
                native.trace.release_model,
                NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. }
            )
            || native.trace.release_model.nodes().len() as u64 != source.entry().receive_entry_cut.0
            || native
                .trace
                .entry_frontier(source.entry().receive_entry_cut)
                .map_err(|error| invalid(&error.to_string()))?
                != source.entry().prerequisites
        {
            return Err(invalid(
                "shared Poll publication changed original pre-effect entry",
            ));
        }
        let mut candidate = native.trace.clone();
        let ordinal =
            u64::try_from(candidate.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let node_id = u64::try_from(candidate.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let mut release = source.entry().clone();
        release.not_before_global_time = source.observed_at();
        let input = NetworkInputEventV4 {
            ordinal,
            channel: source.channel(),
            release: release.clone(),
            event: NetworkInputKindV2::SharedRawTcpPollState {
                consumed_prefix: source.consumed(),
                revents: source.revents(),
                control_generation: source.control_generation(),
                receive_low_water: source.low_water(),
            },
        };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(node_id),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites,
        };
        candidate.inputs.push(input.clone());
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut candidate.release_model
        else {
            unreachable!("policy checked above");
        };
        nodes.push(node.clone());
        // The recorder retains current profiles separately until finalization,
        // exactly as the legacy publisher does. Validate a complete candidate;
        // only the checked input/node pair is committed below.
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
        candidate.validate().map_err(|e| invalid(&e.to_string()))?;
        let receipt = Arc::new(SharedRecordPollPublication {
            source: Arc::new(source),
            input,
            node,
        });
        let call = receipt.source.origin.call;
        // All fallible checks precede the first trace mutation. The same
        // engine transaction installs its only receipt on the original Call.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut native.trace.release_model
        else {
            unreachable!()
        };
        native.trace.inputs.push(receipt.input.clone());
        nodes.push(receipt.node.clone());
        let Some(SharedAttempt::Wait(wait)) =
            &mut self.stream_calls.get_mut(&call).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.record_probe.as_mut().unwrap().published_poll = Some(receipt.clone());
        Ok(receipt)
    }

    fn shared_record_poll_row_matches(&self, receipt: &SharedRecordPollPublication) -> bool {
        let EngineState::Native(native) = &self.mode else {
            return false;
        };
        native.replay.is_none()
            && matches!(
                native.trace.release_model,
                NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. }
            )
            && native.trace.inputs.get(receipt.input.ordinal as usize) == Some(&receipt.input)
            && native
                .trace
                .release_model
                .nodes()
                .get(receipt.node.id.0 as usize)
                == Some(&receipt.node)
    }

    pub(super) fn check_shared_record_poll_publication(
        &self,
        origin: &Arc<SharedRecordProbe>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        let (wait, record, _) = self.record_probe(origin)?;
        match (&wait.intent, &record.published_poll) {
            (SharedWaitIntent::Receive(_), None) => Ok(()),
            (SharedWaitIntent::Poll(_), Some(receipt))
                if Arc::ptr_eq(receipt.source.origin(), origin)
                    && self.shared_record_poll_row_matches(receipt) =>
            {
                self.validate_shared_record_poll_source(&receipt.source, grant, now)
            }
            _ => Err(invalid(
                "shared Record Poll lacks its exact successful publication",
            )),
        }
    }

    pub(super) fn shared_record_poll_history_matches(&self, history: &RecordHistory) -> bool {
        match (&history.origin.intent, &history.published_poll) {
            (SharedWaitIntent::Receive(_), None) => true,
            (SharedWaitIntent::Poll(_), Some(receipt)) => {
                Arc::ptr_eq(receipt.source.origin(), &history.origin)
                    && receipt.source.observed_at == history.observed_at
                    && history
                        .effects
                        .last()
                        .is_some_and(|scan| Arc::ptr_eq(scan, &receipt.source.scan))
                    && self.shared_record_poll_row_matches(receipt)
            }
            _ => false,
        }
    }
}

impl NetworkReplayEngine {
    pub(in crate::network_replay) fn check_shared_record_poll_output_publication(
        &self,
        receipt: &Arc<SharedRecordPollPublication>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_record_poll_source(receipt.source(), grant, now)?;
        let (wait, record, probe) = self.record_probe(receipt.source().origin())?;
        if !matches!(wait.intent, SharedWaitIntent::Poll(_))
            || record
                .published_poll
                .as_ref()
                .is_none_or(|p| !Arc::ptr_eq(p, receipt))
            || !self.shared_record_poll_row_matches(receipt)
            || record.pending.is_some()
            || probe.pending.is_some()
            || record.source.is_some()
            || !self
                .shared_record_history_covers(&self.stream_calls[&receipt.source().origin().call()])
        {
            return Err(invalid(
                "Poll output lacks its exact published actual full scan",
            ));
        }
        Ok(())
    }

    /// This is output settlement, not Pending. Runtime independently retains
    /// and preflights the exact confirmed native lease through this transaction.
    pub(in crate::network_replay) fn settle_shared_record_poll_output(
        &mut self,
        receipt: &Arc<SharedRecordPollPublication>,
        grant: &SharedMmForegroundObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.check_shared_record_poll_output_publication(receipt, grant, now)?;
        let origin = receipt.source().origin();
        let (_, record, _) = self.record_probe(origin)?;
        let history = Arc::new(RecordHistory {
            origin: origin.clone(),
            effects: record.effects.clone(),
            source: None,
            observed_at: now,
            published_poll: Some(receipt.clone()),
        });
        // All source, lifetime, history and unchanged-control checks precede
        // mutation. No byte delivery, cursor movement or time advance occurs.
        self.shadow_probes
            .remove(&origin.lease)
            .expect("checked actual Poll probe");
        self.finish_socket_control(
            origin.owner,
            origin.lease,
            NetworkSocketControlFinish::Unchanged,
        )
        .expect("exact unchanged Poll control preflighted under same engine lock");
        let state = self.stream_calls.get_mut(&origin.call).unwrap();
        let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
            unreachable!()
        };
        wait.record_probe = None;
        wait.record_history.push(history);
        Ok(())
    }
    pub(in crate::network_replay) fn shared_record_poll_output_history_matches(
        &self,
        wait: &SharedWait,
        receipt: &Arc<SharedRecordPollPublication>,
    ) -> bool {
        wait.record_probe.is_none()
            && wait.record_history.last().is_some_and(|history| {
                history
                    .published_poll
                    .as_ref()
                    .is_some_and(|p| Arc::ptr_eq(p, receipt))
                    && self.shared_record_poll_history_matches(history)
            })
    }
}
