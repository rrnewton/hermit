//! One original blocking TX, retaining the selected Normal turn and entry cut.
//! Bytes come only from the positively completed op25 kernel capture.
use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::Kind;
use crate::network_runtime::shared_waits::JoinedSharedPrefix;
use crate::network_runtime::shared_waits::SharedAttemptAdmission;
use crate::scheduler::ordinary_fd::SharedMmForegroundObservation;

/// Original selected reader and independently captured syscall operands.
/// This value carries no grant, prefix or shared-attempt admission authority.
#[derive(Debug)]
pub(crate) struct SharedRecordSendEntry {
    pub(crate) read: NetworkFdReadAdmission,
    pub(crate) arguments: Arguments,
    pub(crate) raw: [usize; 6],
}

#[derive(Debug)]
pub(crate) struct SharedRecordSend {
    root: Arc<crate::network_runtime::ForegroundRoot>,
    epoch: u64,
    admission: Admission,
    raw: [usize; 6],
    prefix: JoinedSharedPrefix,
    channel: NetworkChannelId,
    output_offset: u64,
    option_generation: u64,
    timeout: u64,
    pub(crate) workers: crate::network_runtime::shared_send::SendWorkers,
}
impl SharedRecordSend {
    pub(crate) fn root(&self) -> &Arc<crate::network_runtime::ForegroundRoot> {
        &self.root
    }
    pub(crate) fn owner(&self) -> NetworkStreamOwner {
        self.root.owner()
    }
    pub(crate) fn admission(&self) -> &Admission {
        &self.admission
    }
    pub(crate) fn prefix(&self) -> &JoinedSharedPrefix {
        &self.prefix
    }
    pub(crate) fn timeout(&self) -> u64 {
        self.timeout
    }
    pub(crate) fn raw(&self) -> [usize; 6] {
        self.raw
    }
}
impl NetworkReplayEngine {
    pub(crate) fn shared_record_send_timeout(
        &self,
        file: OpenFileId,
    ) -> Result<u64, NetworkReplayError> {
        let socket = self
            .stream_socket_state(file)?
            .ok_or_else(|| invalid("shared send socket state absent"))?;
        match socket.send_timeout {
            Some(ReceiveTimeoutV3::FiniteTicks(ticks))
                if (1..=i64::MAX as u64 - 1).contains(&ticks) =>
            {
                Ok(ticks)
            }
            _ => Err(invalid(
                "shared blocking Sendto requires saved finite timeout",
            )),
        }
    }
    pub(crate) fn begin_shared_record_send(
        &mut self,
        entry: SharedRecordSendEntry,
        grant: &SharedMmForegroundObservation<'_>,
        prefix: &JoinedSharedPrefix,
        admission: &SharedAttemptAdmission<'_>,
        now: LogicalTime,
    ) -> Result<Arc<SharedRecordSend>, NetworkReplayError> {
        let SharedRecordSendEntry {
            read,
            arguments,
            raw,
        } = entry;
        self.validate_shared_initial_origin(grant.root())?;
        self.validate_fd_read(grant.owner(), &read)?;
        let Kind::BlockingSendto { timeout_ticks } = arguments.kind else {
            return Err(invalid("shared original send changed closed kind"));
        };
        let binding = read
            .binding
            .ok_or_else(|| invalid("shared send has no selected OFD"))?;
        if self.mode() != NetworkEngineMode::Record
            || !self.uses_shared_mm_attempts()
            || !arguments.kind.valid_operands(
                arguments.address,
                arguments.length,
                arguments.original_count,
            )
            || arguments.binding != Some(binding)
            || arguments.files != grant.root().files()
            || arguments.fd != read.fd
            || read.external_grant.is_some()
            || arguments.operation.tid != grant.owner().thread
            || raw
                != [
                    arguments.fd as usize,
                    arguments.address as usize,
                    arguments.original_count as usize,
                    libc::MSG_NOSIGNAL as usize,
                    0,
                    0,
                ]
            || !Arc::ptr_eq(grant.root(), prefix.root())
            || !admission.is_original_prefix(prefix)
            || !admission.matches_peers(self, None)?
            || !self.shared_census_matches_grant(None, None, grant)?
            || self.stream_calls.values().any(|c| c.owner == grant.owner())
            || !self.stream_operations.is_empty()
            || !self.shadow_probes.is_empty()
            || self.shared_record_send_timeout(binding.open_file)? != timeout_ticks
        {
            return Err(invalid(
                "shared send changed original owner/tuple/census/timeout",
            ));
        }
        let socket = self
            .stream_socket_state(binding.open_file)?
            .ok_or_else(|| invalid("shared send socket state absent"))?;
        let option_generation = socket.option_generation;
        if socket.key.domain != libc::AF_INET
            || socket.key.socket_type != libc::SOCK_STREAM
            || socket.key.protocol != libc::IPPROTO_TCP
        {
            return Err(invalid("shared send requires established IPv4 TCP"));
        }
        let channel = self.bound_channel(binding.open_file)?;
        let EngineState::Native(native) = &self.mode else {
            unreachable!()
        };
        if !native.trace.release_model.nodes().iter().any(|n| matches!(n.kind,
            NetworkReleaseNodeKindV4::Progress {channel:c,milestone:NetworkProgressV4::Established{..}} if c==channel))
            || now < native.trace.epoch_global_time()? {
            return Err(invalid("shared send lacks original established channel/time"));
        }
        let offset = native_stream_output_offset(&native.trace, channel)?;
        let cut = NetworkReceiveEntryCutV4(native.trace.release_model.nodes().len() as u64);
        let release = NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: native
                .trace
                .entry_frontier(cut)
                .map_err(|e| invalid(&e.to_string()))?,
        };
        // All semantic validation precedes this exact original read transfer.
        let selected = self.begin_shared_original_send_from_read(grant.owner(), arguments, read)?;
        let origin = Arc::new(SharedRecordSend {
            root: grant.root().clone(),
            epoch: grant.epoch(),
            admission: selected.clone(),
            raw,
            prefix: prefix.clone(),
            channel,
            output_offset: offset,
            option_generation,
            timeout: timeout_ticks,
            workers: Default::default(),
        });
        let marker = Arc::new(NativeEntryMarker {
            spent: std::sync::atomic::AtomicBool::new(false),
            prefix: std::sync::OnceLock::new(),
        });
        let attempt = NativeEntryAttempt {
            owner: grant.owner(),
            call: selected.call,
            marker: marker.clone(),
        };
        let state = self.stream_calls.get_mut(&selected.call).unwrap();
        state.native_entry_attempted = Some(marker);
        state.native_entry = Some(NativeEntry {
            root: grant.root().clone(),
            kind: EntryKind::SharedSend {
                epoch: grant.epoch(),
                operation: selected.arguments.operation,
            },
            release,
            used: false,
        });
        state.shared_attempt = Some(SharedAttempt::RecordTransmit(origin.clone()));
        drop(attempt); // original one-use marker is spent, never refreshed after refusal
        Ok(origin)
    }

    pub(crate) fn shared_record_send_origin(
        &self,
        admission: &Admission,
    ) -> Result<Arc<SharedRecordSend>, NetworkReplayError> {
        let state = self
            .stream_calls
            .get(&admission.call)
            .ok_or(NetworkReplayError::UnknownStreamCall(admission.call))?;
        let Some(SharedAttempt::RecordTransmit(origin)) = &state.shared_attempt else {
            return Err(invalid("original shared send lost retained origin"));
        };
        if origin.admission != *admission
            || state.owner != origin.owner()
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
        {
            return Err(invalid("shared send changed admitted original"));
        }
        Ok(origin.clone())
    }
    pub(crate) fn validate_shared_record_send(
        &self,
        origin: &Arc<SharedRecordSend>,
        grant: &SharedMmForegroundObservation<'_>,
        raw: [usize; 6],
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_initial_origin(grant.root())?;
        if !Arc::ptr_eq(&self.shared_record_send_origin(&origin.admission)?, origin)
            || !Arc::ptr_eq(grant.root(), &origin.root)
            || grant.epoch() != origin.epoch
            || raw != origin.raw
            || !origin
                .prefix
                .matches_retained_peers(self, Some(origin.admission.call))?
            || !self.shared_census_matches_grant(None, Some(origin.admission.call), grant)?
        {
            return Err(invalid(
                "shared send changed current grant/original peer census",
            ));
        }
        let state = &self.stream_calls[&origin.admission.call];
        let binding = origin.admission.arguments.binding.unwrap();
        self.validate_stream_call_lifetime(
            origin.owner(),
            origin.admission.call,
            binding.open_file,
        )?;
        let socket = self
            .stream_socket_state(binding.open_file)?
            .ok_or_else(|| invalid("shared send socket state absent"))?;
        let entry = state
            .native_entry
            .as_ref()
            .ok_or_else(|| invalid("shared send entry absent"))?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if state.open_file != Some(binding.open_file)
            || entry.used
            || !Arc::ptr_eq(&entry.root, &origin.root)
            || entry.kind
                != (EntryKind::SharedSend {
                    epoch: origin.epoch,
                    operation: origin.admission.arguments.operation,
                })
            || now < entry.release.not_before_global_time
            || socket.option_generation != origin.option_generation
            || socket.send_timeout != Some(ReceiveTimeoutV3::FiniteTicks(origin.timeout))
            || self.bound_channel(binding.open_file)? != origin.channel
            || native_stream_output_offset(&native.trace, origin.channel)? != origin.output_offset
            || native.trace.release_model.nodes().len() as u64 != entry.release.receive_entry_cut.0
            || native
                .trace
                .entry_frontier(entry.release.receive_entry_cut)
                .map_err(|e| invalid(&e.to_string()))?
                != entry.release.prerequisites
        {
            return Err(invalid("shared send changed original entry/options/offset"));
        }
        Ok(())
    }
    pub(crate) fn publish_shared_native_sent(
        &mut self,
        origin: &Arc<SharedRecordSend>,
        grant: &SharedMmForegroundObservation<'_>,
        completed: &crate::network_runtime::shared_send::CompletedSharedNativeSend<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shared_record_send(origin, grant, origin.raw, now)?;
        if !Arc::ptr_eq(origin, completed.origin()) {
            return Err(invalid("shared send worker owner replaced"));
        }
        let returned = self.original_shared_native_sent(origin.owner(), &origin.admission)?;
        let capture = completed.capture().map_err(|e| invalid(&e.to_string()))?;
        if returned <= 0 || capture.returned() != returned {
            return Err(invalid(
                "shared send requires genuine positive no-wait capture",
            ));
        }
        let state = &self.stream_calls[&origin.admission.call];
        let entry = state.native_entry.as_ref().unwrap();
        let EngineState::Native(native) = &self.mode else {
            unreachable!()
        };
        let (event, milestone) = original_send_output(
            origin.output_offset,
            native.trace.outputs.len() as u64,
            returned,
            capture.bytes(),
        )?;
        let output = NetworkOutputEventV2 {
            channel: origin.channel,
            event,
        };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(native.trace.release_model.nodes().len() as u64),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel: origin.channel,
                milestone,
            },
            prerequisites: entry.release.prerequisites.clone(),
        };
        let mut candidate = native.trace.clone();
        candidate.outputs.push(output.clone());
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut candidate.release_model
        else {
            return Err(NetworkReplayError::WrongMode);
        };
        nodes.push(node.clone());
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
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native.trace.outputs.push(output);
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut native.trace.release_model
        else {
            unreachable!()
        };
        nodes.push(node);
        self.consume_native_entry(origin.admission.call);
        Ok(())
    }
}

#[cfg(test)]
#[path = "shared_send/tests.rs"]
pub(crate) mod tests;
