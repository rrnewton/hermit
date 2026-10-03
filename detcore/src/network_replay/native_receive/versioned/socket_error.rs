//! Consuming SO_ERROR results have their own input stream and control lifetime.
use super::*;

impl NetworkReplayEngine {
    pub(crate) fn submit_socket_error_read(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.require_no_helper_copy_for_lease(owner, lease)?;
        let control = self.owned_socket_control(owner, lease)?;
        let file = control.open_file;
        if !control.physical.can_release_unchanged()
            || self.shadow_probes.contains_key(&lease)
            || !self.stream_calls.is_empty()
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        native.check_retirement()?;
        if native.replay.is_some()
            || grant.owner() != owner
            || !grant.admits_sole_initial_root(root)
            || !root.matches_metadata(&self.fd_metadata(owner, root.files())?)
            || native
                .policy_root
                .as_ref()
                .is_some_and(|held| !Arc::ptr_eq(held, root))
        {
            return Err(invalid("SO_ERROR lacks its actual foreground root"));
        }
        // Require the existing successful Connect producer. SO_ERROR == 0
        // itself never establishes a connection.
        let channel = self.bound_channel(file)?;
        let definition = native
            .trace
            .channels
            .iter()
            .find(|c| c.id == channel)
            .ok_or(NetworkReplayError::UnknownChannel(channel))?;
        if definition.transport != NetworkTransportV2::Tcp
            || definition.role != NetworkEndpointRoleV2::OutboundClient
            || !native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind,
                NetworkReleaseNodeKindV4::Progress { channel: c,
                    milestone: NetworkProgressV4::Established { .. } } if c == channel)
            })
        {
            return Err(invalid(
                "SO_ERROR requires an established outbound TCP channel",
            ));
        }
        if now < native.trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        let cut = NetworkReceiveEntryCutV4(
            u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?,
        );
        let release = NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: native
                .trace
                .entry_frontier(cut)
                .map_err(|e| invalid(&e.to_string()))?,
        };
        let read = SocketErrorRead {
            entry: Some((root.clone(), grant.epoch(), release)),
            consumed_prefix: self.channels[&channel].inbound_consumed,
            confirmed: false,
        };
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native.policy_root = Some(root.clone());
        self.socket_controls
            .get_mut(&file)
            .unwrap()
            .physical
            .socket_error = Some(read);
        Ok(())
    }

    pub(crate) fn confirm_socket_error_read(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        grant: &crate::scheduler::ordinary_fd::OrdinaryFdObservation<'_>,
        now: LogicalTime,
        errno: i32,
    ) -> Result<(), NetworkReplayError> {
        self.require_sole_initial_release_policy()?;
        let control = self.owned_socket_control(owner, lease)?;
        let file = control.open_file;
        let read = control
            .physical
            .socket_error
            .as_ref()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        let Some((held, epoch, release)) = &read.entry else {
            return Err(NetworkReplayError::WrongMode);
        };
        let channel = self.bound_channel(file)?;
        if read.confirmed
            || !(0..=4095).contains(&errno)
            || !Arc::ptr_eq(held, root)
            || grant.owner() != owner
            || grant.epoch() != *epoch
            || !grant.admits_sole_initial_root(root)
            || !root.matches_metadata(&self.fd_metadata(owner, root.files())?)
            || now < release.not_before_global_time
            || self.channels[&channel].inbound_consumed != read.consumed_prefix
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        native.check_retirement()?;
        if native.replay.is_some()
            || u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?
                != release.receive_entry_cut.0
            || native
                .trace
                .entry_frontier(release.receive_entry_cut)
                .map_err(|e| invalid(&e.to_string()))?
                != release.prerequisites
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let mut release = release.clone();
        release.not_before_global_time = now;
        let input = NetworkInputEventV4 {
            ordinal: u64::try_from(native.trace.inputs.len())
                .map_err(|_| NetworkReplayError::Overflow)?,
            channel,
            release: release.clone(),
            event: NetworkInputKindV2::SocketErrorRead {
                consumed_prefix: read.consumed_prefix,
                errno,
            },
        };
        let node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(
                u64::try_from(native.trace.release_model.nodes().len())
                    .map_err(|_| NetworkReplayError::Overflow)?,
            ),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: input.ordinal,
            },
            prerequisites: release.prerequisites,
        };
        let mut candidate = native.trace.clone();
        let progress = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(
                node.id
                    .0
                    .checked_add(1)
                    .ok_or(NetworkReplayError::Overflow)?,
            ),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::SocketErrorConsumed {
                    input_ordinal: input.ordinal,
                },
            },
            prerequisites: vec![node.id],
        };
        candidate.inputs.push(input.clone());
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut candidate.release_model else { return Err(invalid("legacy V4 publisher requires sole-initial-root policy")); };
        nodes.push(node.clone());
        nodes.push(progress.clone());
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
        // Publication precedes guest copyout: EFAULT does not restore SO_ERROR.
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        native.trace.inputs.push(input);
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut native.trace.release_model else { return Err(invalid("legacy V4 publisher requires sole-initial-root policy")); };
        nodes.push(node);
        nodes.push(progress);
        self.socket_controls
            .get_mut(&file)
            .unwrap()
            .physical
            .socket_error
            .as_mut()
            .unwrap()
            .confirmed = true;
        Ok(())
    }

    pub(crate) fn take_replay_socket_error(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        now: LogicalTime,
    ) -> Result<i32, NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let file = control.open_file;
        self.check_replay_connect_unclaimed(file)?;
        self.check_shared_attempt_unclaimed(file)?;
        if !control.physical.can_release_unchanged() || self.shadow_probes.contains_key(&lease) {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        if !self.native_receive_version() || self.mode() != NetworkEngineMode::Replay {
            return Err(NetworkReplayError::WrongMode);
        }
        let channel = self.bound_channel(file)?;
        while !self.release_native_eligible(now)?.is_empty() {}
        let EngineState::Native(native) = &self.mode else {
            unreachable!()
        };
        let replay = native.replay.as_ref().unwrap();
        let input = native
            .trace
            .inputs
            .iter()
            .find(|input| {
                input.channel == channel
                    && matches!(input.event, NetworkInputKindV2::SocketErrorRead { .. })
                    && !replay.consumed_socket_errors.contains(&input.ordinal)
            })
            .ok_or_else(|| invalid("SO_ERROR has no remaining recorded read"))?;
        let NetworkInputKindV2::SocketErrorRead {
            consumed_prefix,
            errno,
        } = input.event
        else {
            unreachable!()
        };
        if !replay.released[input.ordinal as usize]
            || self.channels[&channel].inbound_consumed != consumed_prefix
        {
            return Err(invalid("SO_ERROR changed its release or consumed-byte cut"));
        }
        let ordinal = input.ordinal;
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!()
        };
        assert!(
            native
                .replay
                .as_mut()
                .unwrap()
                .consumed_socket_errors
                .insert(ordinal)
        );
        self.socket_controls
            .get_mut(&file)
            .unwrap()
            .physical
            .socket_error = Some(SocketErrorRead {
            entry: None,
            consumed_prefix,
            confirmed: true,
        });
        Ok(errno)
    }
}
