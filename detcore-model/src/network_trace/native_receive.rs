//! V4 separates immutable payload coordinates, descriptive native observations,
//! and typed progress frozen at receive entry. This module authenticates no
//! native effect or issue timing: only the actual shared engine may issue those
//! facts. A serializable node, cut or successful validation is never a permit.
use std::collections::VecDeque;

use super::*;

/// Stable producer-ledger identity, unrelated to a thread, FD or syscall count.
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
pub struct NetworkReleaseNodeIdV4(pub u64);

/// Number of producer nodes already issued at the actual receive's entry.
/// This serialized claim cannot reconstruct the runtime's opaque entry permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkReceiveEntryCutV4(pub u64);

/// Declared creation semantics. Accepted children need an explicit successor
/// with typed creation/disposition gates; V3's accepted model stays unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkCreationModelV4 {
    /// Outbound TCP and connectionless datagrams; no listeners/accepted children.
    OutboundAndDatagramV1,
}

/// Actual operation that established the channel, not inferred endpoint data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkEstablishmentV4 {
    /// Exact successful completion on this outbound channel.
    ConnectedInput { input_ordinal: u64 },
    /// Actual connectionless endpoint setup. Runtime provenance is mandatory.
    DatagramSetup,
}

/// Typed committed progress. Expected output bytes alone issue none of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkProgressV4 {
    /// Positive immutable committed stream prefix, including partial writes.
    StreamPrefix {
        exclusive_offset: u64,
    },
    /// Whole datagrams: completing sequence zero advances this count to one,
    /// including when that datagram has an empty payload.
    DatagramPrefix {
        completed: u64,
    },
    Established {
        source: NetworkEstablishmentV4,
    },
    /// Exact output row; shutdown is not byte progress.
    LocalShutdown {
        output_ordinal: u64,
    },
    /// Exact unsuccessful output row and positive errno; no invented bytes.
    OutputError {
        output_ordinal: u64,
    },
    /// Final local incarnation retirement. Prior progress remains addressable.
    Retired,
}

/// One represented producer. Extra declared dependencies express known program
/// order, not inferred peer causality or arbitrary application dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkReleaseNodeKindV4 {
    /// Actual guest consumption/delivery, or control completion. Queue release
    /// alone never fulfills this producer. Per-channel Input graph edges order
    /// these completions, not the availability of following payload rows.
    Input { input_ordinal: u64 },
    Progress {
        channel: NetworkChannelId,
        milestone: NetworkProgressV4,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReleaseNodeV4 {
    pub id: NetworkReleaseNodeIdV4,
    pub kind: NetworkReleaseNodeKindV4,
    /// Canonical, strictly increasing node IDs; no absent or duplicate edges.
    pub prerequisites: Vec<NetworkReleaseNodeIdV4>,
}

/// Explicitly bounded observed program-order policy. Runtime admission must
/// prove sole initial root and freeze its committed frontier before private
/// Peek/effects. A late snapshot or a sibling's output cannot issue this policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkReleaseModelV4 {
    SoleInitialRootProgramOrderV1 { nodes: Vec<NetworkReleaseNodeV4> },
}
impl NetworkReleaseModelV4 {
    pub fn nodes(&self) -> &[NetworkReleaseNodeV4] {
        match self {
            Self::SoleInitialRootProgramOrderV1 { nodes } => nodes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReleaseV4 {
    pub not_before_global_time: LogicalTime,
    pub receive_entry_cut: NetworkReceiveEntryCutV4,
    /// Complete typed frontier at entry, including retired-channel progress.
    pub prerequisites: Vec<NetworkReleaseNodeIdV4>,
}
impl NetworkReleaseV4 {
    /// Structural predicate only. The shared engine must authenticate every
    /// completed producer and retain progress across local channel retirement.
    pub fn is_eligible(
        &self,
        now: LogicalTime,
        completed: &BTreeSet<NetworkReleaseNodeIdV4>,
    ) -> bool {
        now >= self.not_before_global_time
            && self.prerequisites.iter().all(|n| completed.contains(n))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInputEventV4 {
    pub ordinal: u64,
    pub channel: NetworkChannelId,
    pub release: NetworkReleaseV4,
    pub event: NetworkInputKindV2,
}

/// How the physical current-attempt copy treated the original receive queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkNativeCopyDispositionV4 {
    Observe,
    Consume,
}

/// Descriptive geometry of one complete successful physical copy, emitted once.
/// `storage_length` and `nonlinear_length` are the true whole-SKB `len` and
/// `data_len`; a copy may begin in the linear head and continue into page frags.
/// Those scalar facts grant no storage ownership or copy5 admission authority.
/// Available storage can exceed copied bytes; it never grants unseen payload
/// or a replay unit. The producer must emit each actual copy only once. The
/// validator rejects repeated consumption through the channel cursor; identical
/// Observe geometry may describe distinct legitimate peeks, so its physical
/// identity and nonrepetition require producer authority, not these scalars.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkNativeCopyFragmentV4 {
    pub stream_offset: u64,
    pub requested: u64,
    pub copied: u64,
    pub available: u64,
    pub source_offset: u64,
    pub storage_length: u64,
    pub nonlinear_length: u64,
    pub disposition: NetworkNativeCopyDispositionV4,
    pub physical_before: u64,
    pub physical_after: u64,
}

/// Canonical payload coverage is independent of input fragmentation and actual
/// native copy/storage boundaries. Overlapping or missing publication coverage
/// is invalid. A complete physical copy may surround this newly published range,
/// but the producer must emit it exactly once in the channel's physical history
/// and every consumed byte must belong to the complete canonical payload.
/// Current recording emits actual confirmed Drain Consume chains; larger
/// private Peek descriptions do not substitute for those effects. This finite
/// coverage model alone does not
/// represent a later consume of previously published PEEK-only bytes when no
/// new payload is published; that requires an explicit successor representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkNativeReceiveObservationV4 {
    pub ordinal: u64,
    pub channel: NetworkChannelId,
    pub stream_offset: u64,
    pub length: u64,
    pub fragments: Vec<NetworkNativeCopyFragmentV4>,
}

/// Native receive history with the shared typed and umbrella V4 framing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceV4 {
    pub epoch: DateTime<Utc>,
    pub channels: Vec<NetworkChannelV2>,
    pub inputs: Vec<NetworkInputEventV4>,
    pub outputs: Vec<NetworkOutputEventV2>,
    pub creation_model: NetworkCreationModelV4,
    pub release_model: NetworkReleaseModelV4,
    pub native_receive_observations: Vec<NetworkNativeReceiveObservationV4>,
    /// Exact original profiles, including kernel HZ/normalization; no defaults.
    pub fresh_stream_profiles: Vec<FreshStreamSocketProfileV3>,
    pub receive_environment: ReceiveEnvironmentV3,
    pub channel_socket_classes: Vec<ChannelSocketClassV3>,
    pub fresh_send_timeouts: Vec<FreshSendTimeoutV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTraceValidationErrorV4 {
    Payload(NetworkTraceValidationError),
    UnsupportedCreation,
    InvalidProfiles,
    NonCanonicalNode,
    InvalidReference,
    DuplicateProducer,
    MissingProducer,
    InvalidProgress,
    InvalidEntryCut,
    InvalidEntryFrontier,
    DependencyCycle,
    EventAfterRetirement,
    InvalidNativeObservation,
    Overflow,
}
impl fmt::Display for NetworkTraceValidationErrorV4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid V4 network trace: {self:?}")
    }
}
impl Error for NetworkTraceValidationErrorV4 {}
impl From<NetworkTraceValidationError> for NetworkTraceValidationErrorV4 {
    fn from(e: NetworkTraceValidationError) -> Self {
        Self::Payload(e)
    }
}

#[derive(Debug)]
pub enum NetworkTraceCodecErrorV4 {
    Frame(NetworkTraceCodecError),
    Validation(NetworkTraceValidationErrorV4),
}
impl fmt::Display for NetworkTraceCodecErrorV4 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "V4 network codec error: {self:?}")
    }
}
impl Error for NetworkTraceCodecErrorV4 {}
impl From<NetworkTraceCodecError> for NetworkTraceCodecErrorV4 {
    fn from(e: NetworkTraceCodecError) -> Self {
        Self::Frame(e)
    }
}
impl From<NetworkTraceValidationErrorV4> for NetworkTraceCodecErrorV4 {
    fn from(e: NetworkTraceValidationErrorV4) -> Self {
        Self::Validation(e)
    }
}

type Validation = Result<(), NetworkTraceValidationErrorV4>;
use NetworkTraceValidationErrorV4 as Invalid;

fn index(value: u64, length: usize) -> Result<usize, Invalid> {
    let n = usize::try_from(value).map_err(|_| Invalid::InvalidReference)?;
    if n < length {
        Ok(n)
    } else {
        Err(Invalid::InvalidReference)
    }
}
fn add(a: u64, b: u64) -> Result<u64, Invalid> {
    a.checked_add(b).ok_or(Invalid::Overflow)
}
fn canonical(ids: &[NetworkReleaseNodeIdV4], count: usize) -> Validation {
    let mut last = None;
    for id in ids {
        index(id.0, count)?;
        if last.is_some_and(|n| n >= id.0) {
            return Err(Invalid::InvalidReference);
        }
        last = Some(id.0);
    }
    Ok(())
}

fn validate_channel_creation(c: &NetworkChannelV2) -> Validation {
    if !matches!(
        (c.role, c.transport),
        (
            NetworkEndpointRoleV2::OutboundClient,
            NetworkTransportV2::Tcp
        ) | (
            NetworkEndpointRoleV2::Datagram,
            NetworkTransportV2::Udp | NetworkTransportV2::UnixDatagram
        )
    ) {
        return Err(Invalid::UnsupportedCreation);
    }
    Ok(())
}

impl NetworkTraceV4 {
    /// Pure canonical frontier of the specified producer-ledger prefix. This
    /// can inspect an incomplete recorder journal, so it does not validate the
    /// whole trace or grant authority. The actual issuer must own these nodes
    /// and freeze this result at entry, before private Peek or other effects.
    pub fn entry_frontier(
        &self,
        cut: NetworkReceiveEntryCutV4,
    ) -> Result<Vec<NetworkReleaseNodeIdV4>, Invalid> {
        let nodes = self.release_model.nodes();
        let end = usize::try_from(cut.0).map_err(|_| Invalid::InvalidEntryCut)?;
        if end > nodes.len() {
            return Err(Invalid::InvalidEntryCut);
        }
        let channels: BTreeSet<_> = self.channels.iter().map(|c| c.id).collect();
        let mut frontier = BTreeMap::new();
        for (n, node) in nodes[..end].iter().enumerate() {
            if node.id.0 != n as u64 {
                return Err(Invalid::NonCanonicalNode);
            }
            if let NetworkReleaseNodeKindV4::Progress { channel, milestone } = &node.kind {
                if !channels.contains(channel) {
                    return Err(Invalid::InvalidReference);
                }
                let (component, subkey) = frontier_key(milestone);
                frontier.insert((*channel, component, subkey), node.id);
            }
        }
        Ok(frontier
            .into_values()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }
    pub fn epoch_global_time(&self) -> Result<LogicalTime, NetworkTraceValidationError> {
        epoch_global_time(self.epoch)
    }

    /// Validate structural claims. This cannot authenticate native issue timing,
    /// kernel-committed bytes, a stopped task or the actual receive-entry permit.
    pub fn validate(&self) -> Validation {
        let epoch = self.epoch_global_time()?;
        // Private payload-only projection. Placeholder V2 gates are never
        // returned, encoded or given to an engine. V4 releases are checked below.
        let payload = NetworkTraceV2 {
            epoch: self.epoch,
            channels: self.channels.clone(),
            inputs: self
                .inputs
                .iter()
                .map(|i| NetworkInputEventV2 {
                    ordinal: i.ordinal,
                    channel: i.channel,
                    event: i.event.clone(),
                    release: NetworkReleaseV2 {
                        not_before_global_time: epoch,
                        after_transmitted_offset: 0,
                    },
                })
                .collect(),
            outputs: self.outputs.clone(),
        };
        payload.validate_payload(true)?;
        drop(payload);
        for c in &self.channels {
            validate_channel_creation(c)?;
        }
        self.validate_profiles()?;
        self.validate_graph(epoch)?;
        self.validate_observations()
    }

    pub fn write_framed<W: Write>(&self, writer: W) -> Result<(), NetworkTraceCodecErrorV4> {
        self.validate()?;
        write_payload(writer, NETWORK_TRACE_VERSION_V4, self)?;
        Ok(())
    }

    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecErrorV4> {
        let (version, payload) = read_payload(reader)?;
        if version != NETWORK_TRACE_VERSION_V4 {
            return Err(NetworkTraceCodecError::UnsupportedVersion(version).into());
        }
        Self::decode_payload(&payload)
    }

    /// Shared bounded V4 payload decoder for typed and umbrella framing.
    pub(super) fn decode_payload(payload: &[u8]) -> Result<Self, NetworkTraceCodecErrorV4> {
        // Preserve old decoders exactly. This format independently bounds
        // decoded claims as well as the common 64 MiB framed payload.
        let (trace, consumed): (Self, _) = bincode::serde::decode_from_slice(
            payload,
            bincode::config::standard()
                .with_limit::<{ MAX_NETWORK_TRACE_PAYLOAD_BYTES as usize }>(),
        )
        .map_err(NetworkTraceCodecError::Decode)?;
        if consumed != payload.len() {
            return Err(NetworkTraceCodecError::TrailingPayloadData.into());
        }
        trace.validate()?;
        Ok(trace)
    }

    fn validate_profiles(&self) -> Validation {
        let mut previous = None;
        for profile in &self.fresh_stream_profiles {
            profile.validate().map_err(|_| Invalid::InvalidProfiles)?;
            if previous.is_some_and(|d| d >= profile.key.domain) {
                return Err(Invalid::InvalidProfiles);
            }
            previous = Some(profile.key.domain);
        }
        if self.fresh_send_timeouts.len() != self.fresh_stream_profiles.len() {
            return Err(Invalid::InvalidProfiles);
        }
        for (send, receive) in self
            .fresh_send_timeouts
            .iter()
            .zip(&self.fresh_stream_profiles)
        {
            if send.key != receive.key || send.timeout != ReceiveTimeoutV3::Infinite {
                return Err(Invalid::InvalidProfiles);
            }
        }
        let mut previous = None;
        let channels: BTreeMap<_, _> = self.channels.iter().map(|c| (c.id, c)).collect();
        let mut covered = BTreeSet::new();
        for binding in &self.channel_socket_classes {
            if previous.is_some_and(|c| c >= binding.channel) {
                return Err(Invalid::InvalidProfiles);
            }
            previous = Some(binding.channel);
            let c = channels
                .get(&binding.channel)
                .ok_or(Invalid::InvalidProfiles)?;
            if c.transport != NetworkTransportV2::Tcp
                || !self
                    .fresh_stream_profiles
                    .iter()
                    .any(|p| p.key == binding.key)
            {
                return Err(Invalid::InvalidProfiles);
            }
            for address in [c.local_address.as_ref(), c.peer_address.as_ref()]
                .into_iter()
                .flatten()
            {
                let domain = match address {
                    NetworkAddressV2::Inet4 { .. } => 2,
                    NetworkAddressV2::Inet6 { .. } => 10,
                    _ => return Err(Invalid::InvalidProfiles),
                };
                if binding.key.domain != domain {
                    return Err(Invalid::InvalidProfiles);
                }
            }
            covered.insert(c.id);
        }
        if covered
            != self
                .channels
                .iter()
                .filter(|c| c.transport == NetworkTransportV2::Tcp)
                .map(|c| c.id)
                .collect()
        {
            return Err(Invalid::InvalidProfiles);
        }
        Ok(())
    }

    fn validate_graph(&self, epoch: LogicalTime) -> Validation {
        let nodes = self.release_model.nodes();
        let channels: BTreeMap<_, _> = self.channels.iter().map(|c| (c.id, c)).collect();
        let mut input_nodes = vec![None; self.inputs.len()];
        let mut edges = BTreeSet::new();
        for (n, node) in nodes.iter().enumerate() {
            if node.id.0 != n as u64 {
                return Err(Invalid::NonCanonicalNode);
            }
            canonical(&node.prerequisites, nodes.len())?;
            for parent in &node.prerequisites {
                edges.insert((index(parent.0, nodes.len())?, n));
            }
            if let NetworkReleaseNodeKindV4::Input { input_ordinal } = node.kind {
                let i = index(input_ordinal, self.inputs.len())?;
                if input_nodes[i].replace(n).is_some() {
                    return Err(Invalid::DuplicateProducer);
                }
                if node.prerequisites != self.inputs[i].release.prerequisites {
                    return Err(Invalid::InvalidEntryFrontier);
                }
            }
        }
        if input_nodes.iter().any(Option::is_none) {
            return Err(Invalid::MissingProducer);
        }
        // Index each channel's monotonic payload coordinates once. Each row
        // obtains its producer exactly once, even with many partial milestones.
        let mut output_coordinates: BTreeMap<_, Vec<(u64, u64, usize)>> = channels
            .keys()
            .map(|channel| (*channel, Vec::new()))
            .collect();
        for (i, output) in self.outputs.iter().enumerate() {
            let coordinate = match &output.event {
                NetworkOutputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                }
                | NetworkOutputKindV2::StreamMessage {
                    stream_offset,
                    bytes,
                    ..
                } => Some((add(*stream_offset, bytes.len() as u64)?, bytes.len() as u64)),
                NetworkOutputKindV2::Datagram(d) => {
                    Some((add(d.sequence, 1)?, d.bytes.len() as u64))
                }
                NetworkOutputKindV2::DatagramExact(d) => {
                    Some((add(d.datagram.sequence, 1)?, d.datagram.bytes.len() as u64))
                }
                _ => None,
            };
            if let Some((end, bytes)) = coordinate {
                output_coordinates
                    .get_mut(&output.channel)
                    .ok_or(Invalid::InvalidReference)?
                    .push((end, bytes, i));
            }
        }
        let mut output_producers = vec![None; self.outputs.len()];
        let mut states: BTreeMap<_, ProgressState> = channels
            .keys()
            .map(|c| (*c, ProgressState::default()))
            .collect();
        // Each prefix cut is checked against the complete frontier immediately
        // before that ledger position, without cloning one set per input/node.
        let mut cuts: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        let mut last_input: BTreeMap<NetworkChannelId, (usize, LogicalTime, u64)> = BTreeMap::new();
        let mut last_poll = BTreeMap::new();
        for (i, input) in self.inputs.iter().enumerate() {
            let n = input_nodes[i].unwrap();
            let cut = usize::try_from(input.release.receive_entry_cut.0)
                .map_err(|_| Invalid::InvalidEntryCut)?;
            if cut > n {
                return Err(Invalid::InvalidEntryCut);
            }
            canonical(&input.release.prerequisites, nodes.len())?;
            if input.release.not_before_global_time < epoch {
                return Err(Invalid::Payload(
                    NetworkTraceValidationError::ReleaseBeforeEpoch,
                ));
            }
            if let NetworkInputKindV2::RawTcpPollState { consumed_prefix, .. } = input.event {
                let observation = (consumed_prefix, input.release.not_before_global_time);
                // Applying eligible state to closure must not erase an
                // observable zero/ready boundary. Ledger order alone does not
                // separate two observations at the same byte cut and time,
                // even if their masks happen to be identical.
                if last_poll.insert(input.channel, observation) == Some(observation) {
                    return Err(Invalid::InvalidNativeObservation);
                }
            }
            if let Some((prior, time, old_cut)) = last_input.insert(
                input.channel,
                (n, input.release.not_before_global_time, cut as u64),
            ) {
                if time > input.release.not_before_global_time || old_cut > cut as u64 {
                    return Err(Invalid::InvalidEntryCut);
                }
                edges.insert((prior, n));
            }
            cuts.entry(cut).or_default().push(i);
        }
        let mut frontier: BTreeMap<(NetworkChannelId, u8, u64), usize> = BTreeMap::new();
        for (n, node) in nodes.iter().enumerate() {
            if let Some(inputs) = cuts.get(&n) {
                let expected: Vec<_> = frontier
                    .values()
                    .copied()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|n| NetworkReleaseNodeIdV4(n as u64))
                    .collect();
                for i in inputs {
                    if self.inputs[*i].release.prerequisites != expected {
                        return Err(Invalid::InvalidEntryFrontier);
                    }
                }
            }
            match &node.kind {
                NetworkReleaseNodeKindV4::Input { input_ordinal } => {
                    let input = &self.inputs[index(*input_ordinal, self.inputs.len())?];
                    let state = states
                        .get(&input.channel)
                        .ok_or(Invalid::InvalidReference)?;
                    if state.retired.is_some() {
                        return Err(Invalid::EventAfterRetirement);
                    }
                    if !matches!(input.event, NetworkInputKindV2::Connect(_) | NetworkInputKindV2::ConnectEstablished) {
                        let established = state.established.ok_or(Invalid::MissingProducer)?;
                        edges.insert((established, n));
                    }
                    for prerequisite in &input.release.prerequisites {
                        if prerequisite.0 >= input.release.receive_entry_cut.0
                            || !matches!(
                                nodes[index(prerequisite.0, nodes.len())?].kind,
                                NetworkReleaseNodeKindV4::Progress { .. }
                            )
                        {
                            return Err(Invalid::InvalidEntryCut);
                        }
                    }
                }
                NetworkReleaseNodeKindV4::Progress { channel, milestone } => {
                    let definition = channels.get(channel).ok_or(Invalid::InvalidReference)?;
                    let state = states.get_mut(channel).unwrap();
                    if state.retired.is_some() {
                        return Err(Invalid::EventAfterRetirement);
                    }
                    if let Some(previous) = state.previous {
                        edges.insert((previous, n));
                    }
                    state.previous = Some(n);
                    match milestone {
                        NetworkProgressV4::Established { source } => {
                            if state.established.replace(n).is_some() {
                                return Err(Invalid::DuplicateProducer);
                            }
                            match source {
                                NetworkEstablishmentV4::ConnectedInput { input_ordinal } => {
                                    let i = index(*input_ordinal, self.inputs.len())?;
                                    if definition.role != NetworkEndpointRoleV2::OutboundClient
                                        || self.inputs[i].channel != *channel
                                        || !matches!(self.inputs[i].event,
                                            NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected)
                                                | NetworkInputKindV2::ConnectEstablished)
                                    {
                                        return Err(Invalid::InvalidProgress);
                                    }
                                    edges.insert((input_nodes[i].unwrap(), n));
                                }
                                NetworkEstablishmentV4::DatagramSetup
                                    if definition.role == NetworkEndpointRoleV2::Datagram => {}
                                _ => return Err(Invalid::InvalidProgress),
                            }
                        }
                        NetworkProgressV4::StreamPrefix { exclusive_offset } => {
                            if definition.transport.is_datagram()
                                || *exclusive_offset <= state.bytes
                                || state.established.is_none()
                            {
                                return Err(Invalid::InvalidProgress);
                            }
                            let coordinates = &output_coordinates[channel];
                            if coordinates
                                .last()
                                .is_none_or(|(end, _, _)| exclusive_offset > end)
                            {
                                return Err(Invalid::InvalidProgress);
                            }
                            while let Some((end, _, i)) = coordinates.get(state.covered_outputs) {
                                if end > exclusive_offset {
                                    break;
                                }
                                output_producers[*i] = Some(n);
                                state.covered_outputs += 1;
                            }
                            state.bytes = *exclusive_offset;
                        }
                        NetworkProgressV4::DatagramPrefix { completed } => {
                            if !definition.transport.is_datagram()
                                || *completed <= state.datagrams
                                || state.established.is_none()
                            {
                                return Err(Invalid::InvalidProgress);
                            }
                            let coordinates = &output_coordinates[channel];
                            if coordinates.last().is_none_or(|(end, _, _)| completed > end) {
                                return Err(Invalid::InvalidProgress);
                            }
                            while let Some((end, bytes, i)) = coordinates.get(state.covered_outputs)
                            {
                                if end > completed {
                                    break;
                                }
                                output_producers[*i] = Some(n);
                                state.bytes = add(state.bytes, *bytes)?;
                                state.covered_outputs += 1;
                            }
                            state.datagrams = *completed;
                        }
                        NetworkProgressV4::LocalShutdown { output_ordinal }
                        | NetworkProgressV4::OutputError { output_ordinal } => {
                            let i = index(*output_ordinal, self.outputs.len())?;
                            let out = &self.outputs[i];
                            if out.channel != *channel || output_producers[i].replace(n).is_some() {
                                return Err(Invalid::InvalidProgress);
                            }
                            match (milestone, &out.event) {
                                (
                                    NetworkProgressV4::LocalShutdown { .. },
                                    NetworkOutputKindV2::Shutdown { stream_offset, .. },
                                ) if state.established.is_some()
                                    && *stream_offset == state.bytes => {}
                                (
                                    NetworkProgressV4::OutputError { .. },
                                    NetworkOutputKindV2::SocketError { stream_offset, .. },
                                ) => {
                                    if *stream_offset != state.bytes {
                                        return Err(Invalid::InvalidProgress);
                                    }
                                }
                                _ => return Err(Invalid::InvalidProgress),
                            }
                        }
                        NetworkProgressV4::Retired => {
                            state.retired = Some(n);
                        }
                    };
                    let (component, subkey) = frontier_key(milestone);
                    frontier.insert((*channel, component, subkey), n);
                }
            }
        }
        // Coverage is mandatory even when no input advertises an output as a
        // prerequisite. Otherwise omitted producers could hide after retirement.
        if output_producers.iter().any(Option::is_none) {
            return Err(Invalid::MissingProducer);
        }
        let mut previous_output = BTreeMap::new();
        for (i, output) in self.outputs.iter().enumerate() {
            let producer = output_producers[i].unwrap();
            if let Some(previous) = previous_output.insert(output.channel, producer)
                && previous != producer
            {
                edges.insert((previous, producer));
            }
            if let Some(retired) = states[&output.channel].retired {
                edges.insert((producer, retired));
            }
        }
        // Detect actual represented cycles, including cross-channel and implicit
        // output/lifecycle predecessor edges; never recurse over hostile input.
        let mut incoming = vec![0usize; nodes.len()];
        let mut following = vec![Vec::new(); nodes.len()];
        for (from, to) in &edges {
            incoming[*to] = incoming[*to].checked_add(1).ok_or(Invalid::Overflow)?;
            following[*from].push(*to);
        }
        let mut ready: VecDeque<_> = incoming
            .iter()
            .enumerate()
            .filter(|(_, n)| **n == 0)
            .map(|(i, _)| i)
            .collect();
        let mut visited = 0;
        while let Some(n) = ready.pop_front() {
            visited += 1;
            for next in &following[n] {
                incoming[*next] -= 1;
                if incoming[*next] == 0 {
                    ready.push_back(*next);
                }
            }
        }
        if visited != nodes.len() {
            return Err(Invalid::DependencyCycle);
        }
        if edges.iter().any(|(from, to)| from >= to) {
            return Err(Invalid::InvalidProgress);
        }
        // Structural ledger order does not authenticate real timing. In
        // particular a forged later cut cannot be detected from claims alone.
        for (n, node) in nodes.iter().enumerate() {
            if matches!(node.kind, NetworkReleaseNodeKindV4::Progress { .. })
                && node.prerequisites.iter().any(|p| p.0 >= n as u64)
            {
                return Err(Invalid::InvalidProgress);
            }
        }
        Ok(())
    }

    fn validate_observations(&self) -> Validation {
        let mut ranges: BTreeMap<NetworkChannelId, Vec<(u64, u64)>> = BTreeMap::new();
        for input in &self.inputs {
            match &input.event {
                NetworkInputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                } => {
                    ranges
                        .entry(input.channel)
                        .or_default()
                        .push((*stream_offset, add(*stream_offset, bytes.len() as u64)?));
                }
                // This finite native-copy model has no ancillary-store issuer.
                NetworkInputKindV2::StreamMessage { .. } => {
                    return Err(Invalid::InvalidNativeObservation);
                }
                _ => {}
            }
        }
        let mut covered: BTreeMap<NetworkChannelId, u64> = BTreeMap::new();
        let mut physical_cursors: BTreeMap<NetworkChannelId, u64> = BTreeMap::new();
        for (n, observation) in self.native_receive_observations.iter().enumerate() {
            let start = observation.stream_offset;
            let end = add(start, observation.length)?;
            if observation.ordinal != n as u64
                || observation.length == 0
                || covered.get(&observation.channel).copied().unwrap_or(0) != start
            {
                return Err(Invalid::InvalidNativeObservation);
            }
            let available = ranges
                .get(&observation.channel)
                .ok_or(Invalid::InvalidNativeObservation)?;
            if available.last().is_none_or(|(_, last)| end > *last) {
                return Err(Invalid::InvalidNativeObservation);
            }
            let recorded_end = available.last().unwrap().1;
            let physical = physical_cursors.entry(observation.channel).or_default();
            let mut copied_end = None;
            let mut selected_covered = start;
            let mut disposition = None;
            for fragment in &observation.fragments {
                let copied_after = add(fragment.stream_offset, fragment.copied)?;
                if fragment.requested == 0
                    || fragment.copied != fragment.requested
                    || fragment.requested > fragment.available
                    || fragment.source_offset > fragment.storage_length
                    || fragment.available != fragment.storage_length - fragment.source_offset
                    || fragment.storage_length > u64::from(u32::MAX)
                    || fragment.nonlinear_length > fragment.storage_length
                    || copied_end.is_some_and(|before| before != fragment.stream_offset)
                    || disposition.is_some_and(|prior| prior != fragment.disposition)
                {
                    return Err(Invalid::InvalidNativeObservation);
                }
                add(fragment.stream_offset, fragment.available)?;
                if fragment.physical_before != *physical {
                    return Err(Invalid::InvalidNativeObservation);
                }
                match fragment.disposition {
                    NetworkNativeCopyDispositionV4::Observe => {
                        if fragment.physical_after != fragment.physical_before
                            || fragment.stream_offset < fragment.physical_before
                        {
                            return Err(Invalid::InvalidNativeObservation);
                        }
                    }
                    NetworkNativeCopyDispositionV4::Consume => {
                        if fragment.stream_offset != fragment.physical_before
                            || fragment.physical_after != copied_after
                            || copied_after > recorded_end
                        {
                            return Err(Invalid::InvalidNativeObservation);
                        }
                        *physical = fragment.physical_after;
                    }
                }
                if fragment.stream_offset <= selected_covered && copied_after > selected_covered {
                    selected_covered = copied_after.min(end);
                }
                copied_end = Some(copied_after);
                disposition = Some(fragment.disposition);
            }
            if selected_covered != end {
                return Err(Invalid::InvalidNativeObservation);
            }
            covered.insert(observation.channel, end);
        }
        for (channel, ranges) in ranges {
            if covered.get(&channel).copied() != ranges.last().map(|(_, end)| *end) {
                return Err(Invalid::InvalidNativeObservation);
            }
        }
        Ok(())
    }
}

fn frontier_key(milestone: &NetworkProgressV4) -> (u8, u64) {
    match milestone {
        NetworkProgressV4::Established { .. } => (0, 0),
        NetworkProgressV4::StreamPrefix { .. } => (1, 0),
        NetworkProgressV4::DatagramPrefix { .. } => (2, 0),
        NetworkProgressV4::LocalShutdown { output_ordinal }
        | NetworkProgressV4::OutputError { output_ordinal } => (3, *output_ordinal),
        NetworkProgressV4::Retired => (4, 0),
    }
}

#[derive(Default)]
struct ProgressState {
    covered_outputs: usize,
    bytes: u64,
    datagrams: u64,
    established: Option<usize>,
    retired: Option<usize>,
    previous: Option<usize>,
}

#[cfg(test)]
mod tests;
