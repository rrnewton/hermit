//! Accepted TCP custody and inheritance. Provider identities are run-local;
//! portable child creation is an input of the listener, never of an accepter.
use detcore_model::network_trace::AcceptedStreamModelV1;
use detcore_model::network_trace::ChildCreatedV1;
use detcore_model::network_trace::ChildCreationIdV1;
use detcore_model::network_trace::ChildDispositionV1;
use detcore_model::network_trace::ChildInheritanceV1;
use detcore_model::network_trace::FreshSendTimeoutV1;
use detcore_model::network_trace::ReceiveTimeoutV3;

use super::*;

/// Durable accept operation identity, unrelated to a trace child ordinal.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize
)]
pub struct NetworkAcceptLeaseId(pub u64);

/// A reserved child and its immutable creation-time semantic state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAcceptedChild {
    /// Durable accept receipt.
    pub lease: NetworkAcceptLeaseId,
    /// Shared external creation occurrence.
    pub child: ChildCreationIdV1,
    /// Exact accepted class.
    pub key: StreamSocketKeyV3,
    /// Actual bound local endpoint, not the requested wildcard bind.
    pub local: NetworkAddressV2,
    /// Actual connecting peer endpoint.
    pub peer: NetworkAddressV2,
}

/// Active listener syscall admitted before a possibly blocking kernel accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAcceptReservation {
    /// Exact durable receipt, present even before a child exists in Record.
    pub lease: NetworkAcceptLeaseId,
    /// Replay's reserved shared child; Record learns it from the provider.
    pub child: Option<NetworkAcceptedChild>,
}

/// Facts returned only after an accepted installation is reconciled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkAcceptedCompletion {
    /// Exact newly installed descriptor.
    pub fd: i32,
    /// Exact lifetime-authorized open file.
    pub open_file: OpenFileId,
    /// Trace channel selected by the reserved child.
    pub channel: NetworkChannelId,
}

/// Private capability: neither a guest RPC nor socket metadata can create it.
/// Production construction requires BOTH the creation observer and descriptor
/// mutation service. Neither is qualified yet; ordinary backends have None.
#[derive(Debug)]
pub(crate) struct AcceptedBackendCapability(());
impl AcceptedBackendCapability {
    #[cfg(test)]
    pub(crate) fn controlled_fixture() -> Self {
        Self(())
    }
}

/// Run-local creation/lifetime association issued by the kernel observer.
/// Before accept, Linux owns queue custody; this identity is not a recorder FD.
/// Installed additionally requires a held-FD match to this exact generation.
/// A scalar cookie or reusable raw kernel pointer alone is not that proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AcceptedPhysicalIdentity {
    pub provider: u64,
    pub object: u64,
    pub namespace: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ChildCreationCertificate {
    pub sequence: u64,
    pub listener: AcceptedPhysicalIdentity,
    pub child: AcceptedPhysicalIdentity,
    pub listener_generation: u64,
    pub inherited: NetworkStreamSocketState,
    pub local: NetworkAddressV2,
    pub peer: NetworkAddressV2,
}

/// Result authority is supplied by the fd mutation service, not by the adapter's
/// fd number or by add_fd. In particular EFAULT can dequeue a connection without
/// creating a guest slot. It does not prove a final physical close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcceptedInstallationFact {
    NoConnection {
        errno: i32,
    },
    DequeuedNoInstallation {
        child: ChildCreationIdV1,
        errno: i32,
    },
    Installed {
        child: ChildCreationIdV1,
        fd: i32,
        open_file: OpenFileId,
        slot_generation: u64,
        physical: AcceptedPhysicalIdentity,
    },
}

#[derive(Debug, Clone)]
struct ChildState {
    id: ChildCreationIdV1,
    listener: NetworkChannelId,
    key: StreamSocketKeyV3,
    local: NetworkAddressV2,
    peer: NetworkAddressV2,
    release: NetworkReleaseV2,
    history_prefix: u64,
    planned: Option<ChildDispositionV1>,
    completed: Option<ChildDispositionV1>,
    inherited: Option<NetworkStreamSocketState>,
    physical: Option<AcceptedPhysicalIdentity>,
    reservation: Option<NetworkAcceptLeaseId>,
}
impl ChildState {
    fn from_trace(child: ChildCreatedV1) -> Self {
        Self {
            id: child.id,
            listener: child.listener,
            key: child.key,
            local: child.local,
            peer: child.peer,
            release: child.release,
            history_prefix: child.history_prefix,
            planned: Some(child.disposition),
            completed: None,
            inherited: None,
            physical: None,
            reservation: None,
        }
    }
    fn into_trace(self) -> Result<ChildCreatedV1, NetworkReplayError> {
        Ok(ChildCreatedV1 {
            id: self.id,
            listener: self.listener,
            key: self.key,
            local: self.local,
            peer: self.peer,
            release: self.release,
            history_prefix: self.history_prefix,
            inheritance: ChildInheritanceV1::LinuxTcpListenerV1,
            disposition: self
                .completed
                .ok_or(NetworkReplayError::UnresolvedAcceptedChild(self.id))?,
        })
    }
}
#[derive(Debug, Clone)]
struct AcceptOperation {
    owner: NetworkStreamOwner,
    listener_call: NetworkStreamCallId,
    listener: NetworkChannelId,
    child: Option<ChildCreationIdV1>,
    submitted: bool,
    abandoned: bool,
    confirmed: Option<AcceptedInstallationFact>,
}

#[derive(Debug, Default)]
pub(super) struct AcceptedRuntime {
    pub(super) fresh_send: BTreeMap<StreamSocketKeyV3, ReceiveTimeoutV3>,
    children: Vec<ChildState>,
    physical_listeners: BTreeMap<OpenFileId, AcceptedPhysicalIdentity>,
    last_observation: BTreeMap<u64, u64>,
    provider_cookies: BTreeMap<ChildCreationIdV1, u64>,
    next_accept: u64,
    operations: BTreeMap<NetworkAcceptLeaseId, AcceptOperation>,
    completions: BTreeMap<
        NetworkAcceptLeaseId,
        (
            NetworkStreamOwner,
            AcceptedInstallationFact,
            Option<NetworkAcceptedCompletion>,
        ),
    >,
}
impl AcceptedRuntime {
    pub(super) fn replay(model: AcceptedStreamModelV1) -> Self {
        Self {
            fresh_send: model
                .fresh_send_timeouts
                .into_iter()
                .map(|p| (p.key, p.timeout))
                .collect(),
            children: model
                .children
                .into_iter()
                .map(ChildState::from_trace)
                .collect(),
            next_accept: 1,
            ..Self::default()
        }
    }
    pub(super) fn record() -> Self {
        Self {
            next_accept: 1,
            ..Self::default()
        }
    }
    pub(super) fn into_model(self) -> Result<AcceptedStreamModelV1, NetworkReplayError> {
        Ok(AcceptedStreamModelV1 {
            fresh_send_timeouts: self
                .fresh_send
                .into_iter()
                .map(|(key, timeout)| FreshSendTimeoutV1 { key, timeout })
                .collect(),
            children: self
                .children
                .into_iter()
                .map(ChildState::into_trace)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl NetworkReplayEngine {
    /// Explicit enrollment; ordinary V3 Record construction remains unchanged.
    pub(crate) fn record_shadow_accepted(
        epoch: DateTime<Utc>,
        _cap: AcceptedBackendCapability,
    ) -> Self {
        let mut engine = Self::record_shadow(epoch);
        engine.shadow.as_mut().unwrap().accepted = Some(AcceptedRuntime::record());
        engine
    }
    /// Reader dispatch follows the declared variant, never socket defaults.
    pub fn accepted_mode(&self) -> bool {
        self.shadow.as_ref().is_some_and(|s| s.accepted.is_some())
    }
    fn accepted(&self) -> Result<&AcceptedRuntime, NetworkReplayError> {
        self.shadow
            .as_ref()
            .and_then(|s| s.accepted.as_ref())
            .ok_or(NetworkReplayError::WrongMode)
    }
    fn accepted_mut(&mut self) -> Result<&mut AcceptedRuntime, NetworkReplayError> {
        self.shadow
            .as_mut()
            .and_then(|s| s.accepted.as_mut())
            .ok_or(NetworkReplayError::WrongMode)
    }
    /// Explicit fresh observation, before mutation. Replay requires the recorded
    /// entry and never supplies a placeholder getter or a synthesized default.
    pub fn register_accepted_fresh_send(
        &mut self,
        key: StreamSocketKeyV3,
        observed: Option<ReceiveTimeoutV3>,
    ) -> Result<(), NetworkReplayError> {
        match (&self.mode, observed) {
            (EngineState::Record(_), Some(ReceiveTimeoutV3::Infinite)) => {
                self.accepted_mut()?
                    .fresh_send
                    .insert(key, ReceiveTimeoutV3::Infinite);
                Ok(())
            }
            (EngineState::Replay(_), None) if self.accepted()?.fresh_send.contains_key(&key) => {
                Ok(())
            }
            _ => Err(NetworkReplayError::StreamProfileMismatch(key)),
        }
    }
    pub(crate) fn accepted_listener_enrolled(&self, open_file: OpenFileId) -> bool {
        self.accepted()
            .is_ok_and(|runtime| runtime.physical_listeners.contains_key(&open_file))
    }
    pub(crate) fn accepted_listener_enrollment_target(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(OpenFileId, NetworkStreamSocketState), NetworkReplayError> {
        self.accepted()?;
        let open_file = self.stream_call_open_file(owner, call)?;
        let channel = self.bound_channel(open_file)?;
        if !self.channel_definitions().iter().any(|definition| {
            definition.id == channel && definition.role == NetworkEndpointRoleV2::Listener
        }) {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        Ok((open_file, self.stream_call_socket_state(owner, call)?))
    }

    pub(crate) fn confirm_provider_listener_enrollment(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        evidence: crate::network_runtime::accepted_listener::Enrollment,
    ) -> Result<(), NetworkReplayError> {
        let (current, current_state) = self.accepted_listener_enrollment_target(owner, call)?;
        let (open_file, physical, observed_state) = evidence.parts();
        if current != open_file || &current_state != observed_state {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        self.bind_accepted_listener(open_file, physical)
    }

    /// Matched provider enrollment, distinct from the portable listener channel.
    pub(crate) fn enroll_accepted_listener(
        &mut self,
        open_file: OpenFileId,
        physical: AcceptedPhysicalIdentity,
        _cap: &AcceptedBackendCapability,
    ) -> Result<(), NetworkReplayError> {
        self.bind_accepted_listener(open_file, physical)
    }
    fn bind_accepted_listener(
        &mut self,
        open_file: OpenFileId,
        physical: AcceptedPhysicalIdentity,
    ) -> Result<(), NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        if !self
            .channel_definitions()
            .iter()
            .any(|c| c.id == channel && c.role == NetworkEndpointRoleV2::Listener)
            || physical.provider == 0
            || physical.object == 0
            || physical.namespace == 0
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        let runtime = self.accepted_mut()?;
        if runtime
            .physical_listeners
            .get(&open_file)
            .is_some_and(|p| *p != physical)
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        runtime.physical_listeners.insert(open_file, physical);
        Ok(())
    }
    /// Provider calls at an authenticated, paid observation cut, before a later
    /// listener option generation can commit. No RPC caller can submit a cert.
    pub(crate) fn observe_child_creation(
        &mut self,
        listener: OpenFileId,
        certificate: ChildCreationCertificate,
        now: LogicalTime,
        _cap: &AcceptedBackendCapability,
    ) -> Result<ChildCreationIdV1, NetworkReplayError> {
        self.publish_child_creation(listener, certificate, now)
    }
    /// Only the authenticated provider response can create this opaque proof.
    pub(crate) fn confirm_provider_child_creation(
        &mut self,
        evidence: &crate::network_runtime::accepted_creation::ObservedCreation,
        now: LogicalTime,
    ) -> Result<ChildCreationIdV1, NetworkReplayError> {
        let listener = self
            .accepted()?
            .physical_listeners
            .iter()
            .find_map(|(open_file, physical)| {
                (*physical == evidence.listener()).then_some(*open_file)
            })
            .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
        let state = self
            .stream_socket_state(listener)?
            .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
        let certificate = evidence
            .certificate(&state)
            .map_err(|_| NetworkReplayError::InvalidAcceptedReceipt)?;
        let child = self.publish_child_creation(listener, certificate, now)?;
        self.accepted_mut()?
            .provider_cookies
            .insert(child, evidence.cookie());
        Ok(child)
    }
    pub(crate) fn provider_child_published(
        &self,
        matched: crate::network_runtime::accepted::Resolved,
    ) -> Result<bool, NetworkReplayError> {
        let runtime = self.accepted()?;
        if let Some(child) = runtime
            .children
            .iter()
            .find(|child| child.physical == Some(matched.physical))
        {
            if child.id.0 != matched.creation
                || runtime.provider_cookies.get(&child.id) != Some(&matched.cookie)
            {
                return Err(NetworkReplayError::InvalidAcceptedReceipt);
            }
            return Ok(true);
        }
        if runtime
            .last_observation
            .get(&matched.physical.provider)
            .copied()
            .unwrap_or(0)
            >= matched.creation
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        Ok(false)
    }
    fn publish_child_creation(
        &mut self,
        listener: OpenFileId,
        certificate: ChildCreationCertificate,
        now: LogicalTime,
    ) -> Result<ChildCreationIdV1, NetworkReplayError> {
        let EngineState::Record(trace) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let channel = self.bound_channel(listener)?;
        let state = self
            .stream_socket_state(listener)?
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(listener))?;
        let runtime = self.accepted()?;
        if runtime.physical_listeners.get(&listener) != Some(&certificate.listener)
            || certificate.listener.provider != certificate.child.provider
            || certificate.listener.namespace != certificate.child.namespace
            || certificate.listener == certificate.child
            || certificate.child.object == 0
            || certificate.sequence
                != runtime
                    .last_observation
                    .get(&certificate.child.provider)
                    .copied()
                    .unwrap_or(0)
                    + 1
            || certificate.listener_generation != state.option_generation
            || certificate.inherited != state
            || runtime
                .children
                .iter()
                .any(|c| c.physical == Some(certificate.child))
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        let id = ChildCreationIdV1(
            u64::try_from(runtime.children.len()).map_err(|_| NetworkReplayError::Overflow)? + 1,
        );
        let child = ChildState {
            id,
            listener: channel,
            key: state.key,
            local: certificate.local,
            peer: certificate.peer,
            release: NetworkReleaseV2 {
                not_before_global_time: now,
                after_transmitted_offset: 0,
            },
            history_prefix: trace.inputs.len() as u64,
            planned: None,
            completed: None,
            inherited: Some(certificate.inherited),
            physical: Some(certificate.child),
            reservation: None,
        };
        // The same address/class check is used before any installed-channel mutation.
        validate_child_addresses(&child)?;
        let runtime = self.accepted_mut()?;
        runtime
            .last_observation
            .insert(certificate.child.provider, certificate.sequence);
        runtime.children.push(child);
        Ok(id)
    }
    /// Derive inheritance at shared external input eligibility, before choosing
    /// an accepter. A later guest setter never rewrites this captured state.
    pub(super) fn release_accepted_children(
        &mut self,
        now: LogicalTime,
    ) -> Result<BTreeSet<NetworkChannelId>, NetworkReplayError> {
        if !self.accepted_mode() {
            return Ok(BTreeSet::new());
        }
        let EngineState::Replay(replay) = &self.mode else {
            return Ok(BTreeSet::new());
        };
        let mut pending = Vec::new();
        let mut blocked = BTreeSet::new();
        for child in &self.accepted()?.children {
            if child.inherited.is_some() {
                continue;
            }
            if blocked.contains(&child.listener) {
                continue;
            }
            let prior_input_pending =
                replay
                    .trace
                    .inputs
                    .iter()
                    .zip(&replay.released)
                    .any(|(input, released)| {
                        input.channel == child.listener
                            && input.ordinal < child.history_prefix
                            && !released
                    });
            let ofd = self.reverse_bindings.get(&child.listener).copied();
            if prior_input_pending
                || !child
                    .release
                    .is_eligible(now, self.channels[&child.listener].transmitted)
                || ofd.is_none()
            {
                blocked.insert(child.listener);
                continue;
            }
            let state = self
                .stream_socket_state(ofd.unwrap())?
                .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
            if state.key != child.key || state.send_timeout.is_none() {
                return Err(NetworkReplayError::InvalidAcceptedReceipt);
            }
            pending.push((child.id, state));
        }
        let mut ready = BTreeSet::new();
        for (id, state) in pending {
            let child = self
                .accepted_mut()?
                .children
                .iter_mut()
                .find(|c| c.id == id)
                .unwrap();
            child.inherited = Some(state);
            ready.insert(child.listener);
        }
        Ok(ready)
    }
    pub(super) fn accepted_input_ready(
        &self,
        channel: NetworkChannelId,
        event: &NetworkInputKindV2,
    ) -> bool {
        let NetworkInputKindV2::Accept { accepted, .. } = event else {
            return true;
        };
        let Ok(runtime) = self.accepted() else {
            return true;
        };
        runtime.children.iter().any(|c|c.listener==channel && c.inherited.is_some()
            && matches!(c.planned,Some(ChildDispositionV1::Accepted {channel,..}) if channel==*accepted))
    }
    pub(super) fn next_child_release(&self) -> Option<LogicalTime> {
        let EngineState::Replay(replay) = &self.mode else {
            return None;
        };
        self.accepted()
            .ok()?
            .children
            .iter()
            .filter(|c| {
                c.inherited.is_none()
                    && self.reverse_bindings.contains_key(&c.listener)
                    && !replay
                        .trace
                        .inputs
                        .iter()
                        .zip(&replay.released)
                        .any(|(i, r)| i.channel == c.listener && i.ordinal < c.history_prefix && !r)
            })
            .map(|c| c.release.not_before_global_time)
            .min()
    }
    /// Reserve without consuming the listener's Accept input. This call retains
    /// the already-admitted listener syscall reference, not an OFD mutex.
    pub fn begin_accepted_socket(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<Option<NetworkAcceptReservation>, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        let listener = self.bound_channel(open_file)?;
        if self.gone_stream_owners.contains(&owner) {
            return Err(NetworkReplayError::StreamOwnerGone(owner));
        }
        if self
            .accepted()?
            .operations
            .values()
            .any(|op| op.listener_call == call)
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        let selected = if self.mode() == NetworkEngineMode::Record {
            None
        } else {
            // Accept queue order, not child-creation order, chooses the next
            // connection. An earlier call's reservation does not hide later
            // queued connections from another scheduled accepter.
            let mut selected = None;
            for input in &self.channels[&listener].inbound {
                let InboundOutcome::Control(ConnectionOutcome::Accept { accepted, .. }) = input
                else {
                    break;
                };
                let child = self
                    .accepted()?
                    .children
                    .iter()
                    .find(|child| {
                        child.listener == listener
                            && matches!(child.planned,
                        Some(ChildDispositionV1::Accepted { channel, .. }) if channel == *accepted)
                    })
                    .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
                if child.reservation.is_some() {
                    continue;
                }
                if child.inherited.is_none() || child.completed.is_some() {
                    return Err(NetworkReplayError::InvalidAcceptedReceipt);
                }
                selected = Some(child.clone());
                break;
            }
            selected
        };
        if selected.is_none() && self.mode() == NetworkEngineMode::Replay {
            return Ok(None);
        }
        let runtime = self.accepted_mut()?;
        let lease = NetworkAcceptLeaseId(runtime.next_accept);
        runtime.next_accept = runtime
            .next_accept
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        if let Some(child) = &selected {
            runtime
                .children
                .iter_mut()
                .find(|c| c.id == child.id)
                .unwrap()
                .reservation = Some(lease);
        }
        runtime.operations.insert(
            lease,
            AcceptOperation {
                owner,
                listener_call: call,
                listener,
                child: selected.as_ref().map(|c| c.id),
                submitted: false,
                abandoned: false,
                confirmed: None,
            },
        );
        Ok(Some(NetworkAcceptReservation {
            lease,
            child: selected.map(|child| NetworkAcceptedChild {
                lease,
                child: child.id,
                key: child.key,
                local: child.local,
                peer: child.peer,
            }),
        }))
    }
    fn accept_operation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<&AcceptOperation, NetworkReplayError> {
        let operation = self
            .accepted()?
            .operations
            .get(&lease)
            .ok_or(NetworkReplayError::UnknownAcceptLease(lease))?;
        if operation.owner != owner
            || operation.abandoned
            || self.gone_stream_owners.contains(&owner)
        {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        self.stream_call_open_file(owner, operation.listener_call)?;
        Ok(operation)
    }
    /// Latch possible allocation before any kernel entry.
    pub fn submit_accepted_socket(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<(), NetworkReplayError> {
        if self.accept_operation(owner, lease)?.submitted {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        self.accepted_mut()?
            .operations
            .get_mut(&lease)
            .unwrap()
            .submitted = true;
        Ok(())
    }
    /// Release a reservation only before any possible physical effect.
    pub fn cancel_accepted_socket(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<(), NetworkReplayError> {
        if self.accept_operation(owner, lease)?.submitted {
            return Err(NetworkReplayError::UnresolvedAccept(lease));
        }
        self.remove_accept_reservation(lease);
        Ok(())
    }
    pub(crate) fn accepted_capture_call(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<NetworkStreamCallId, NetworkReplayError> {
        let operation = self.accept_operation(owner, lease)?;
        if !operation.submitted || operation.confirmed.is_some() {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        Ok(operation.listener_call)
    }
    /// Recovery authenticates the original submitted receipt even after owner
    /// retirement. This grants neither a new physical pin nor channel mutation.
    pub(crate) fn accepted_capture_recovery_call(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> Result<NetworkStreamCallId, NetworkReplayError> {
        let op = self
            .accepted()?
            .operations
            .get(&lease)
            .filter(|op| op.owner == owner && op.submitted)
            .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
        Ok(op.listener_call)
    }
    fn remove_accept_reservation(&mut self, lease: NetworkAcceptLeaseId) {
        let runtime = self.accepted_mut().unwrap();
        runtime.operations.remove(&lease);
        for child in &mut runtime.children {
            if child.reservation == Some(lease) {
                child.reservation = None;
            }
        }
    }
    /// Only the fd service may confirm this fact. The public RPC supplies the
    /// kernel return for comparison, never its own purported slot certificate.
    pub(crate) fn confirm_accepted_installation(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        fact: AcceptedInstallationFact,
        _cap: &AcceptedBackendCapability,
    ) -> Result<(), NetworkReplayError> {
        let operation = self.accept_operation(owner, lease)?.clone();
        if !operation.submitted || operation.confirmed.is_some() {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        match &fact {
            AcceptedInstallationFact::NoConnection { errno } if !(1..=4095).contains(errno) => {
                return Err(NetworkReplayError::InvalidAcceptedReceipt);
            }
            AcceptedInstallationFact::DequeuedNoInstallation { child, errno }
                if operation.child.is_some_and(|id| id != *child)
                    || !(1..=4095).contains(errno) =>
            {
                return Err(NetworkReplayError::InvalidAcceptedReceipt);
            }
            AcceptedInstallationFact::Installed {
                child,
                fd,
                open_file,
                slot_generation,
                physical,
            } => {
                let selected = self
                    .accepted()?
                    .children
                    .iter()
                    .find(|c| c.id == *child)
                    .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
                if selected.listener != operation.listener
                    || selected.completed.is_some()
                    || selected.reservation.is_some_and(|r| r != lease)
                    || operation.child.is_some_and(|id| id != *child)
                    || *fd < 0
                    || *slot_generation == 0
                    || !open_file.is_socket()
                    || selected.physical.is_some_and(|p| p != *physical)
                    || physical.provider == 0
                    || physical.object == 0
                {
                    return Err(NetworkReplayError::InvalidAcceptedReceipt);
                }
            }
            _ => {}
        }
        let child = match &fact {
            AcceptedInstallationFact::Installed { child, .. }
            | AcceptedInstallationFact::DequeuedNoInstallation { child, .. } => Some(*child),
            _ => None,
        };
        if let Some(id) = child {
            let selected = self
                .accepted_mut()?
                .children
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
            if selected.listener != operation.listener
                || selected.completed.is_some()
                || selected.reservation.is_some_and(|r| r != lease)
            {
                return Err(NetworkReplayError::InvalidAcceptedReceipt);
            }
            selected.reservation = Some(lease);
        }
        let operation = self.accepted_mut()?.operations.get_mut(&lease).unwrap();
        operation.child = child;
        operation.confirmed = Some(fact);
        Ok(())
    }
    /// Resolve exact known outcome. Unknown acquisition/allocation/copyout
    /// remains latched; a numeric errno alone never means no connection effect.
    pub fn complete_accepted_socket(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        kernel_result: Result<i32, i32>,
        installed_open_file: Option<OpenFileId>,
        now: LogicalTime,
    ) -> Result<Option<NetworkAcceptedCompletion>, NetworkReplayError> {
        if let Some((prior_owner, fact, result)) = self.accepted()?.completions.get(&lease) {
            if *prior_owner == owner
                && fact_result(fact) == kernel_result
                && fact_open_file(fact) == installed_open_file
            {
                return Ok(result.clone());
            }
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        let operation = self.accept_operation(owner, lease)?.clone();
        let fact = operation
            .confirmed
            .clone()
            .ok_or(NetworkReplayError::UnresolvedAccept(lease))?;
        if fact_result(&fact) != kernel_result || fact_open_file(&fact) != installed_open_file {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
        let completion = match fact.clone() {
            AcceptedInstallationFact::NoConnection { .. } => None,
            AcceptedInstallationFact::DequeuedNoInstallation { .. } => {
                // A portable dequeue-without-install disposition still needs its
                // model variant and physical close custody; do not pretend this
                // is Accepted or UnacceptedListenerClose.
                return Err(NetworkReplayError::UnresolvedAccept(lease));
            }
            AcceptedInstallationFact::Installed {
                child,
                fd,
                open_file,
                ..
            } => {
                let selected = self
                    .accepted()?
                    .children
                    .iter()
                    .find(|c| c.id == child)
                    .unwrap()
                    .clone();
                if self.retired_open_files.contains(&open_file)
                    || self.bindings.contains_key(&open_file)
                    || self
                        .shadow
                        .as_ref()
                        .unwrap()
                        .sockets
                        .contains_key(&open_file)
                {
                    return Err(NetworkReplayError::InvalidAcceptedReceipt);
                }
                let inherited = selected
                    .inherited
                    .clone()
                    .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?;
                let selected_channel = match selected.planned {
                    Some(ChildDispositionV1::Accepted { channel, .. }) => Some(channel),
                    None => None,
                    _ => return Err(NetworkReplayError::InvalidAcceptedReceipt),
                };
                let request = NetworkChannelBinding {
                    transport: selected.key.transport,
                    role: NetworkEndpointRoleV2::Accepted,
                    peer_address: Some(selected.peer.clone()),
                    requested_local_constraint: None,
                    observed_local_address: if self.mode() == NetworkEngineMode::Record {
                        Some(selected.local.clone())
                    } else {
                        None
                    },
                    accepted_from: Some(operation.listener),
                    selected_channel,
                };
                if let EngineState::Record(trace) = &self.mode {
                    if self.channels[&operation.listener]
                        .published_ingress
                        .is_some()
                        || now < selected.release.not_before_global_time
                        || (trace.inputs.len() as u64) < selected.history_prefix
                    {
                        return Err(NetworkReplayError::InvalidAcceptedReceipt);
                    }
                }
                // Locate the exact reserved queue entry BEFORE enrollment.
                // Completion order may differ from FIFO selection order.
                let queue_index = if let Some(selected_channel) = selected_channel {
                    Some(self.channels[&operation.listener].inbound.iter().position(|input|
                        matches!(input, InboundOutcome::Control(ConnectionOutcome::Accept { accepted, .. })
                            if *accepted == selected_channel))
                        .ok_or(NetworkReplayError::InvalidAcceptedReceipt)?)
                } else {
                    None
                };
                self.shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .insert(open_file, inherited);
                let channel = match self.ensure_channel(open_file, request) {
                    Ok(c) => c,
                    Err(error) => {
                        self.shadow.as_mut().unwrap().sockets.remove(&open_file);
                        return Err(error);
                    }
                };
                let disposition = match &mut self.mode {
                    EngineState::Record(trace) => {
                        let ordinal = trace.inputs.len() as u64;
                        trace.inputs.push(NetworkInputEventV2 {
                            ordinal,
                            channel: operation.listener,
                            release: NetworkReleaseV2 {
                                not_before_global_time: now,
                                after_transmitted_offset: 0,
                            },
                            event: NetworkInputKindV2::Accept {
                                accepted: channel,
                                peer: Some(selected.peer),
                                ancillary: None,
                            },
                        });
                        ChildDispositionV1::Accepted {
                            channel,
                            input_ordinal: ordinal,
                        }
                    }
                    EngineState::Replay(_) => {
                        self.channels
                            .get_mut(&operation.listener)
                            .unwrap()
                            .inbound
                            .remove(queue_index.expect("Replay selection validated"));
                        self.channels
                            .get_mut(&operation.listener)
                            .unwrap()
                            .refresh_readiness();
                        selected.planned.unwrap()
                    }
                };
                self.accepted_mut()?
                    .children
                    .iter_mut()
                    .find(|c| c.id == child)
                    .unwrap()
                    .completed = Some(disposition);
                Some(NetworkAcceptedCompletion {
                    fd,
                    open_file,
                    channel,
                })
            }
        };
        self.remove_accept_reservation(lease);
        self.accepted_mut()?
            .completions
            .insert(lease, (owner, fact, completion.clone()));
        Ok(completion)
    }
    /// Bound endpoint getters never consult the unconnected Replay placeholder.
    pub fn accepted_endpoint(
        &self,
        open_file: OpenFileId,
        peer: bool,
    ) -> Result<Option<NetworkAddressV2>, NetworkReplayError> {
        if !self.accepted_mode() {
            return Ok(None);
        }
        let Some(channel) = self.channel_for(open_file) else {
            return Ok(None);
        };
        let definition = self
            .channel_definitions()
            .iter()
            .find(|c| c.id == channel)
            .unwrap();
        if definition.role != NetworkEndpointRoleV2::Accepted {
            return Ok(None);
        }
        Ok(if peer {
            definition.peer_address.clone()
        } else {
            definition.local_address.clone()
        })
    }
    pub(super) fn check_accept_call_release(
        &self,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        if let Ok(runtime) = self.accepted() {
            if let Some((lease, _)) = runtime
                .operations
                .iter()
                .find(|(_, op)| op.listener_call == call)
            {
                return Err(NetworkReplayError::UnresolvedAccept(*lease));
            }
        }
        Ok(())
    }
    pub(super) fn accepted_owner_gone(&mut self, owner: NetworkStreamOwner) {
        if let Ok(runtime) = self.accepted_mut() {
            for operation in runtime
                .operations
                .values_mut()
                .filter(|op| op.owner == owner)
            {
                operation.abandoned = true;
            }
        }
    }
    pub(super) fn check_accepted_finished(&self) -> Result<(), NetworkReplayError> {
        let Ok(runtime) = self.accepted() else {
            return Ok(());
        };
        if let Some(lease) = runtime.operations.keys().next() {
            return Err(NetworkReplayError::UnresolvedAccept(*lease));
        }
        if let Some(child) = runtime.children.iter().find(|c| c.completed.is_none()) {
            return Err(NetworkReplayError::UnresolvedAcceptedChild(child.id));
        }
        Ok(())
    }
}
fn fact_result(fact: &AcceptedInstallationFact) -> Result<i32, i32> {
    match fact {
        AcceptedInstallationFact::Installed { fd, .. } => Ok(*fd),
        AcceptedInstallationFact::NoConnection { errno }
        | AcceptedInstallationFact::DequeuedNoInstallation { errno, .. } => Err(*errno),
    }
}
fn validate_child_addresses(child: &ChildState) -> Result<(), NetworkReplayError> {
    for address in [&child.local, &child.peer] {
        if !matches!(
            (child.key.domain, address),
            (libc::AF_INET, NetworkAddressV2::Inet4 { .. })
                | (libc::AF_INET6, NetworkAddressV2::Inet6 { .. })
        ) {
            return Err(NetworkReplayError::InvalidAcceptedReceipt);
        }
    }
    Ok(())
}

fn fact_open_file(fact: &AcceptedInstallationFact) -> Option<OpenFileId> {
    match fact {
        AcceptedInstallationFact::Installed { open_file, .. } => Some(*open_file),
        _ => None,
    }
}
