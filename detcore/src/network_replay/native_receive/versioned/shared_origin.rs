//! Initial Connect is issued before the first birth; finalization requires
//! positive original parent and complete child facts, never an empty census.
use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

#[derive(Debug)]
pub(super) struct SharedInitialOrigin {
    root: Arc<ForegroundRoot>,
    projection: Arc<NativeTaskProjection>,
    operation: crate::resources::ExternalOpId,
}
impl NativeState {
    pub(super) fn check_shared_origin_history(&self) -> Result<(), NetworkReplayError> {
        if self.policy_failure
            || self.policy_root.is_some()
            || self.shared_origin.as_ref().is_some_and(|origin| {
                !origin.projection.matches_initial_root(&origin.root)
                    || !origin.root.has_shared_mm_history()
            })
        {
            return Err(invalid("shared trace lost original initial-root history"));
        }
        Ok(())
    }
    pub(super) fn check_shared_origin_finalization(&self) -> Result<(), NetworkReplayError> {
        self.check_shared_origin_history()?;
        let origin = self
            .shared_origin
            .as_ref()
            .ok_or_else(|| invalid("shared trace lacks actual initial origin"))?;
        if !origin.projection.completed_initial_final_wait(&origin.root) {
            return Err(invalid(
                "shared trace lacks complete original parent/child final waits",
            ));
        }
        Ok(())
    }
}
impl NetworkReplayEngine {
    fn require_shared_initial_policy(&self) -> Result<(), NetworkReplayError> {
        if !self.uses_shared_mm_attempts() {
            return Err(invalid(
                "shared origin requires its explicit release policy",
            ));
        }
        Ok(())
    }

    /// Historical origin only. The caller must still retain the actual current
    /// grant, complete physical census, exact Call and joined source interval.
    pub(crate) fn validate_shared_initial_origin(
        &self,
        selected: &ForegroundRoot,
    ) -> Result<(), NetworkReplayError> {
        self.require_shared_initial_policy()?;
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            unreachable!();
        };
        let origin = native
            .shared_origin
            .as_ref()
            .ok_or_else(|| invalid("shared attempt lacks original Connect origin"))?;
        if !std::ptr::eq(selected.initial_ancestor(), origin.root.as_ref())
            || !selected.same_shared_lineage(&origin.root)
            || !selected.has_shared_mm_history()
        {
            return Err(invalid("shared attempt changed retained initial lineage"));
        }
        Ok(())
    }

    pub(crate) fn shared_initial_origin(
        &self,
    ) -> Option<(Arc<ForegroundRoot>, Arc<NativeTaskProjection>)> {
        let EngineState::Native(native) = &self.mode else {
            return None;
        };
        native
            .shared_origin
            .as_ref()
            .map(|o| (o.root.clone(), o.projection.clone()))
    }

    pub(in crate::network_replay) fn check_shared_initial_finalization(
        &self,
    ) -> Result<(), NetworkReplayError> {
        if self.uses_shared_mm_attempts() {
            let EngineState::Native(native) = &self.mode else {
                unreachable!();
            };
            native.check_shared_origin_finalization()?;
        }
        Ok(())
    }

    pub(crate) fn bind_shared_initial_replay_origin(
        &mut self,
        root: Arc<ForegroundRoot>,
        projection: Arc<NativeTaskProjection>,
        grant: &crate::scheduler::fd_read::NativeCaptureEntryObservation<'_>,
    ) -> Result<(), NetworkReplayError> {
        self.require_shared_initial_policy()?;
        self.check_native_retirement()?;
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        if native.replay.is_none()
            || native.shared_origin.is_some()
            || !grant.admits_sole_initial_root(&root)
            || !projection.matches_initial_root(&root)
        {
            return Err(invalid(
                "Replay Connect changed its actual zero-birth initial origin",
            ));
        }
        native.shared_origin = Some(SharedInitialOrigin {
            root,
            projection,
            operation: grant.operation(),
        });
        Ok(())
    }

    pub(crate) fn begin_shared_initial_entry_stamp(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NativeEntryAttempt, NetworkReplayError> {
        self.require_shared_initial_policy()?;
        self.check_native_retirement()?;
        if !self.native_receive_version() || self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let state = self
            .stream_calls
            .get_mut(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner
            || state.native_entry_attempted.is_some()
            || state.abandoned
            || state.final_wait
        {
            return Err(invalid(
                "native receive entry attempt is one use on its actual Call",
            ));
        }
        let marker = Arc::new(NativeEntryMarker {
            spent: std::sync::atomic::AtomicBool::new(false),
            prefix: std::sync::OnceLock::new(),
        });
        state.native_entry_attempted = Some(marker.clone());
        Ok(NativeEntryAttempt {
            owner,
            call,
            marker,
        })
    }

    pub(crate) fn stamp_shared_initial_connect_entry(
        &mut self,
        attempt: NativeEntryAttempt,
        admission: &crate::network_runtime::ForegroundEntryAdmission<'_>,
        grant: &crate::scheduler::fd_read::NativeCaptureEntryObservation<'_>,
        projection: Arc<NativeTaskProjection>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.require_shared_initial_policy()?;
        let root = admission.root().clone();
        let owner = grant.owner();
        let kind = EntryKind::SharedInitialConnect {
            operation: grant.operation(),
        };
        if !grant.admits_sole_initial_root(&root) || !projection.matches_initial_root(&root) {
            return Err(invalid(
                "shared Connect lacks actual zero-birth initial grant/projection",
            ));
        }
        let call = attempt.call;
        self.check_native_retirement()?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.replay.is_some() || attempt.owner != owner || !root.is_sole_initial_root(owner) {
            return Err(invalid("V4 entry requires the actual live recorder root"));
        }
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner
            || state.abandoned
            || state.final_wait
            || state.terminal_evidence.is_some()
            || state
                .native_entry_attempted
                .as_ref()
                .is_none_or(|marker| !Arc::ptr_eq(marker, &attempt.marker))
            || state.native_entry.is_some()
            || state.helper_copy.is_some()
            || !state.native_receive.is_empty()
            || state.private_receive.is_some()
            || state.foreground_store.is_some()
            || state.private_drain.is_some()
            || self.stream_operations.values().any(|op| op.owner == owner)
            || self.stream_calls.keys().any(|id| *id != call)
        {
            return Err(invalid(
                "V4 entry is late, repeated, or belongs to another admitted Call",
            ));
        }
        if !state.original.as_ref().is_some_and(|original| {
            !original.is_native_send() && original.native_entry_unsubmitted(grant.operation())
        }) {
            return Err(invalid(
                "shared Connect entry follows original physical submission",
            ));
        }
        if now < native.trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        let cut = NetworkReceiveEntryCutV4(
            u64::try_from(native.trace.release_model.nodes().len())
                .map_err(|_| NetworkReplayError::Overflow)?,
        );
        let prerequisites = native
            .trace
            .entry_frontier(cut)
            .map_err(|e| invalid(&e.to_string()))?;
        if native.shared_origin.is_some() || native.policy_root.is_some() {
            return Err(invalid(
                "shared initial Connect origin is one use before births",
            ));
        }
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!("validated recorder");
        };
        native.shared_origin = Some(SharedInitialOrigin {
            root: root.clone(),
            projection,
            operation: grant.operation(),
        });
        self.stream_calls.get_mut(&call).unwrap().native_entry = Some(NativeEntry {
            root,
            kind,
            release: NetworkReleaseV4 {
                not_before_global_time: now,
                receive_entry_cut: cut,
                prerequisites,
            },
            used: false,
        });
        Ok(())
    }

    pub(super) fn retain_shared_retirement(
        &mut self,
        channel: NetworkChannelId,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Native(native) = &mut self.mode else {
            return Ok(());
        };
        native.check_retirement()?;
        if native.replay.is_some() {
            return Ok(());
        }
        let plan = (|| {
            if native
                .trace
                .channels
                .iter()
                .filter(|c| c.id == channel)
                .count()
                != 1
            {
                return Err(NetworkTraceValidationErrorV4::InvalidReference);
            }
            let cut = NetworkReceiveEntryCutV4(
                u64::try_from(native.trace.release_model.nodes().len())
                    .map_err(|_| NetworkTraceValidationErrorV4::Overflow)?,
            );
            let prerequisites = native.trace.entry_frontier(cut)?;
            if native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind, NetworkReleaseNodeKindV4::Progress {
                    channel: previous,
                    milestone: NetworkProgressV4::Retired,
                } if previous == channel)
            }) {
                return Err(NetworkTraceValidationErrorV4::EventAfterRetirement);
            }
            Ok(NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(cut.0),
                kind: NetworkReleaseNodeKindV4::Progress {
                    channel,
                    milestone: NetworkProgressV4::Retired,
                },
                prerequisites,
            })
        })();
        let node = match plan {
            Ok(node) => node,
            Err(error) => {
                native.retirement_failure = Some((channel, error.clone()));
                return Err(NetworkReplayError::NativeRetirement { channel, error });
            }
        };
        // Cleanup may outlive the final task, but cannot invent a policy root
        // from the final frontier or restore a premise lost to a sibling.
        if native
            .shared_origin
            .as_ref()
            .is_none_or(|origin| !origin.projection.matches_initial_root(&origin.root))
        {
            native.policy_failure = true;
            return Err(invalid("shared retirement lacks retained original history"));
        }
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut native.trace.release_model
        else {
            return Err(invalid("shared retirement changed release policy"));
        };
        nodes.push(node);
        Ok(())
    }

    pub(crate) fn publish_shared_initial_connected(
        &mut self,
        owner: NetworkStreamOwner,
        admission: &original_connect::Admission,
        root: &Arc<crate::network_runtime::ForegroundRoot>,
        completed: &crate::network_runtime::native_peer::CompletedNativeConnect<'_>,
        now: LogicalTime,
    ) -> Result<(), NetworkReplayError> {
        self.require_shared_initial_policy()?;
        let (open_file, returned) = self.original_native_connected(owner, admission)?;
        self.validate_stream_call_lifetime(owner, admission.call, open_file)?;
        let (peer, local) = completed
            .endpoints(owner, admission, returned)
            .map_err(|e| invalid(&e.to_string()))?;
        let channel = self.bound_channel(open_file)?;
        let definition = self
            .channel_definitions()
            .iter()
            .find(|d| d.id == channel)
            .unwrap();
        if definition.transport != NetworkTransportV2::Tcp
            || definition.role != NetworkEndpointRoleV2::OutboundClient
            || definition.peer_address.as_ref() != Some(&peer)
            || definition
                .local_address
                .as_ref()
                .is_some_and(|address| address != &local)
        {
            return Err(invalid("V4 Connect channel changed actual original peer"));
        }
        let release = self.native_entry_release_for(
            owner,
            admission.call,
            root,
            EntryKind::SharedInitialConnect {
                operation: admission.arguments.operation,
            },
            now,
        )?;
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if native.mode() != NetworkEngineMode::Record
            || native.trace.inputs.iter().any(|i| i.channel == channel)
            || native.trace.release_model.nodes().iter().any(|node| {
                matches!(node.kind,
                NetworkReleaseNodeKindV4::Progress { channel: c, .. } if c == channel)
            })
        {
            return Err(invalid(
                "V4 Connect is one successful establishment per channel",
            ));
        }
        let ordinal =
            u64::try_from(native.trace.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let first = u64::try_from(native.trace.release_model.nodes().len())
            .map_err(|_| NetworkReplayError::Overflow)?;
        let asynchronous = returned == -i64::from(libc::EINPROGRESS);
        let completion_ordinal = ordinal
            .checked_add(u64::from(asynchronous))
            .ok_or(NetworkReplayError::Overflow)?;
        let completion_node = first
            .checked_add(u64::from(asynchronous))
            .ok_or(NetworkReplayError::Overflow)?;
        let established = completion_node
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        established
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let input = NetworkInputEventV4 {
            ordinal,
            channel,
            release: release.clone(),
            event: NetworkInputKindV2::Connect(if asynchronous {
                NetworkConnectionResultV2::Error(libc::EINPROGRESS)
            } else {
                NetworkConnectionResultV2::Connected
            }),
        };
        let input_node = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(first),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: release.prerequisites.clone(),
        };
        let progress = NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(established),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel,
                milestone: NetworkProgressV4::Established {
                    source: NetworkEstablishmentV4::ConnectedInput {
                        input_ordinal: completion_ordinal,
                    },
                },
            },
            prerequisites: (first..=completion_node)
                .map(NetworkReleaseNodeIdV4)
                .collect(),
        };
        let mut candidate = native.trace.clone();
        // The actual same-pin endpoint observation is retained through close
        // and authenticated with the original effect above. Never synthesize
        // an ephemeral port from the Replay placeholder or requested target.
        candidate
            .channels
            .iter_mut()
            .find(|d| d.id == channel)
            .unwrap()
            .local_address = Some(local);
        candidate.inputs.push(input);
        if asynchronous {
            candidate.inputs.push(NetworkInputEventV4 {
                ordinal: completion_ordinal,
                channel,
                release: release.clone(),
                event: NetworkInputKindV2::ConnectEstablished,
            });
        }
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut candidate.release_model
        else {
            return Err(invalid("shared Connect publisher changed release policy"));
        };
        nodes.push(input_node);
        if asynchronous {
            nodes.push(NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(completion_node),
                kind: NetworkReleaseNodeKindV4::Input {
                    input_ordinal: completion_ordinal,
                },
                prerequisites: release.prerequisites,
            });
        }
        nodes.push(progress);
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
        candidate
            .validate()
            .map_err(|error| invalid(&error.to_string()))?;
        let origin = native
            .shared_origin
            .as_ref()
            .ok_or_else(|| invalid("missing shared Connect origin"))?;
        if !Arc::ptr_eq(&origin.root, root) || origin.operation != admission.arguments.operation {
            return Err(invalid(
                "shared Connect publication changed retained original operation",
            ));
        }
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        native.trace = candidate;
        self.consume_native_entry(admission.call);
        Ok(())
    }
}

#[cfg(test)]
#[path = "shared_origin/tests.rs"]
mod tests;

#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn controlled_shared_initial_finalization(&self) -> Result<(), NetworkReplayError> {
        self.check_shared_initial_finalization()
    }

    /// Terminal-fixture premise only: no Connect/native/provider completion is
    /// claimed. Entry issuance is exercised independently with actual Calls.
    pub(crate) fn controlled_shared_initial_origin(
        &mut self,
        root: Arc<ForegroundRoot>,
        projection: Arc<NativeTaskProjection>,
    ) {
        assert!(self.uses_shared_mm_attempts());
        assert!(root.is_sole_initial_root(root.owner()));
        assert!(projection.matches_initial_root(&root));
        let EngineState::Native(native) = &mut self.mode else {
            unreachable!();
        };
        assert!(native.shared_origin.is_none());
        native.shared_origin = Some(SharedInitialOrigin {
            operation: crate::resources::ExternalOpId::new(root.owner().thread, 1),
            root,
            projection,
        });
    }
}
