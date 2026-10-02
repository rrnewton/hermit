//! Pure Replay binding/establishment checks for the actual local source issuer.
//! This is a child of versioned so completed producer facts stay private.
use std::sync::Mutex;

use super::*;

impl NetworkReplayEngine {
    pub(crate) fn validate_replay_transmit_read(
        &self,
        owner: NetworkStreamOwner,
        read: &NetworkFdReadAdmission,
        actual: &Arc<Mutex<crate::tool_local::FileMetadata>>,
        metadata: &mut crate::tool_local::FileMetadata,
    ) -> Result<OpenFileId, NetworkReplayError> {
        self.check_native_retirement()?;
        if !self.native_receive_version() || self.mode() != NetworkEngineMode::Replay {
            return Err(NetworkReplayError::WrongMode);
        }
        self.validate_fd_metadata(owner, read.publication.permit.files, actual, metadata)?;
        self.validate_fd_read_grant(owner, read)?;
        let observed = metadata
            .observe_fd_read(read)
            .map_err(|e| invalid(&e.to_string()))?;
        let binding = read
            .binding
            .ok_or_else(|| invalid("Replay transmit lost FD binding"))?;
        if read.external_grant.is_some()
            || observed.binding != Some(binding)
            || observed.socket != Some(binding.open_file)
            || observed.nonblocking.is_none()
        {
            return Err(invalid(
                "Replay transmit changed original FD reader/binding",
            ));
        }
        let channel = self.bound_channel(binding.open_file)?;
        let definition = self
            .channel_definitions()
            .iter()
            .find(|definition| definition.id == channel)
            .ok_or(NetworkReplayError::UnknownChannel(channel))?;
        if definition.transport != NetworkTransportV2::Tcp
            || definition.role != NetworkEndpointRoleV2::OutboundClient
        {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let EngineState::Native(native) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let completed = self.native_completed()?;
        if !native.trace.release_model.nodes().iter().any(|node| {
            matches!(node.kind, NetworkReleaseNodeKindV4::Progress {
                channel: established,
                milestone: NetworkProgressV4::Established { .. },
            } if established == channel)
                && completed.contains(&node.id)
        }) {
            return Err(invalid(
                "Replay transmit lacks completed channel establishment",
            ));
        }
        Ok(binding.open_file)
    }
}
