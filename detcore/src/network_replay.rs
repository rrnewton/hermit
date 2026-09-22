/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Pure schedule-independent network capture and replay state.
//!
//! The scheduler owns this state machine and decides which waiter runs. The
//! engine never matches a thread id, syscall count, raw fd, or call ordinal.
//! It binds stable open-file descriptions to trace-stable channels, releases
//! external observations from continuous virtual time plus per-channel
//! outbound progress, and lets whichever scheduled reader arrives first
//! consume available bytes.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

use chrono::DateTime;
use chrono::Utc;
use detcore_model::fd::OpenFileId;
use detcore_model::network_trace::NetworkAncillaryDataV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkChannelV2;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkDatagramV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkOutputEventV2;
use detcore_model::network_trace::NetworkOutputKindV2;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTraceV2;
use detcore_model::network_trace::NetworkTraceValidationError;
use detcore_model::network_trace::NetworkTransportV2;
use detcore_model::time::LogicalTime;

/// Pure capture/replay mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkEngineMode {
    /// Append validated live observations to a V2 trace.
    Record,
    /// Consume a prevalidated V2 trace without host networking.
    Replay,
}

/// Result of a stream receive attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamReceiveOutcome {
    /// Bytes consumed from currently available stream data.
    Bytes(Vec<u8>),
    /// The peer write side is closed and all preceding bytes were consumed.
    EndOfFile,
    /// A recorded Linux errno is observable at the current stream offset.
    Error(i32),
    /// A nonblocking operation would block.
    WouldBlock,
    /// A blocking operation must register a scheduler waiter.
    Pending,
}

/// Result of a stream transmit attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTransmitOutcome {
    /// This many bytes matched and advanced outbound progress.
    Accepted(usize),
    /// A recorded Linux errno is observable at the current stream offset.
    Error(i32),
}

/// One replayed datagram after applying the guest buffer bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatagramDelivery {
    /// Bytes copied to the guest; possibly truncated.
    pub bytes: Vec<u8>,
    /// Original boundary length, used by `MSG_TRUNC` semantics.
    pub original_len: usize,
    /// Sender address.
    pub source: Option<detcore_model::network_trace::NetworkAddressV2>,
    /// Destination address recorded for this packet.
    pub destination: Option<detcore_model::network_trace::NetworkAddressV2>,
    /// Exact control bytes and object relocation metadata.
    pub ancillary: Option<NetworkAncillaryDataV2>,
    /// Recorded message flags, with truncation added by the adapter if needed.
    pub message_flags: i32,
}

/// Result of a datagram receive attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DatagramReceiveOutcome {
    /// One complete recorded datagram boundary.
    Datagram(DatagramDelivery),
    /// A recorded Linux errno.
    Error(i32),
    /// A nonblocking operation would block.
    WouldBlock,
    /// A blocking operation must wait.
    Pending,
}

/// Connect or accept observation ready for syscall adaptation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionOutcome {
    /// Connect completed, possibly with a typed errno.
    Connect(NetworkConnectionResultV2),
    /// Listener accepted the named trace channel.
    Accept {
        /// Newly accepted trace channel.
        accepted: NetworkChannelId,
        /// Peer address returned by Linux.
        peer: Option<detcore_model::network_trace::NetworkAddressV2>,
        /// Ancillary metadata associated with creation.
        ancillary: Option<NetworkAncillaryDataV2>,
    },
}

/// Pure state machine shared by capture and replay policies.
#[derive(Debug)]
pub struct NetworkReplayEngine {
    mode: EngineState,
    bindings: BTreeMap<OpenFileId, NetworkChannelId>,
    reverse_bindings: BTreeMap<NetworkChannelId, OpenFileId>,
}

#[derive(Debug)]
enum EngineState {
    Record(NetworkTraceV2),
    Replay(ReplayState),
}

#[derive(Debug)]
struct ReplayState {
    trace: NetworkTraceV2,
    released: Vec<bool>,
    channels: BTreeMap<NetworkChannelId, ChannelState>,
}

#[derive(Debug)]
struct ChannelState {
    transport: NetworkTransportV2,
    inbound_stream: VecDeque<u8>,
    inbound_consumed: u64,
    inbound_datagrams: VecDeque<NetworkDatagramV2>,
    inbound_errors: VecDeque<(u64, i32)>,
    peer_write_closed_at: Option<u64>,
    control: VecDeque<ConnectionOutcome>,
    explicit_readiness: NetworkReadinessV2,
    expected_stream: Vec<u8>,
    transmitted: u64,
    outbound_datagrams: VecDeque<NetworkDatagramV2>,
    outbound_errors: VecDeque<(u64, i32)>,
    outbound_shutdowns: VecDeque<(u64, NetworkShutdownV2)>,
    local_write_closed: bool,
}

impl NetworkReplayEngine {
    /// Create an empty recorder in the supplied absolute epoch.
    pub fn record(epoch: DateTime<Utc>) -> Self {
        Self {
            mode: EngineState::Record(NetworkTraceV2 {
                epoch,
                channels: Vec::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
            }),
            bindings: BTreeMap::new(),
            reverse_bindings: BTreeMap::new(),
        }
    }

    /// Create a fail-closed replayer after validating the entire trace.
    pub fn replay(trace: NetworkTraceV2) -> Result<Self, NetworkReplayError> {
        trace.validate()?;
        let mut channels = BTreeMap::new();
        for definition in &trace.channels {
            channels.insert(definition.id, ChannelState::new(definition));
        }
        for output in &trace.outputs {
            channels
                .get_mut(&output.channel)
                .expect("validated channel")
                .append_expected_output(&output.event);
        }
        let released = vec![false; trace.inputs.len()];
        Ok(Self {
            mode: EngineState::Replay(ReplayState {
                trace,
                released,
                channels,
            }),
            bindings: BTreeMap::new(),
            reverse_bindings: BTreeMap::new(),
        })
    }

    /// Current capture/replay mode.
    pub fn mode(&self) -> NetworkEngineMode {
        match self.mode {
            EngineState::Record(_) => NetworkEngineMode::Record,
            EngineState::Replay(_) => NetworkEngineMode::Replay,
        }
    }

    /// Add one channel definition while recording.
    pub fn record_channel(&mut self, channel: NetworkChannelV2) -> Result<(), NetworkReplayError> {
        let EngineState::Record(trace) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if trace
            .channels
            .iter()
            .any(|existing| existing.id == channel.id)
        {
            return Err(NetworkReplayError::ChannelAlreadyExists(channel.id));
        }
        trace.channels.push(channel);
        Ok(())
    }

    /// Append one live input observation while recording.
    pub fn record_input(
        &mut self,
        mut input: NetworkInputEventV2,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Record(trace) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        input.ordinal = trace.inputs.len() as u64;
        trace.inputs.push(input);
        Ok(())
    }

    /// Append one guest output observation while recording.
    pub fn record_output(
        &mut self,
        output: NetworkOutputEventV2,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Record(trace) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        trace.outputs.push(output);
        Ok(())
    }

    /// Finalize and validate a recorded trace.
    pub fn into_recorded_trace(self) -> Result<NetworkTraceV2, NetworkReplayError> {
        let EngineState::Record(trace) = self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        trace.validate()?;
        Ok(trace)
    }

    /// Bind one live OFD to one trace channel.
    pub fn bind(
        &mut self,
        open_file: OpenFileId,
        channel: NetworkChannelId,
    ) -> Result<(), NetworkReplayError> {
        if !open_file.is_socket() {
            return Err(NetworkReplayError::NonSocketOpenFile(open_file));
        }
        if !self.has_channel(channel) {
            return Err(NetworkReplayError::UnknownChannel(channel));
        }
        if let Some(existing) = self.bindings.get(&open_file) {
            return if *existing == channel {
                Ok(())
            } else {
                Err(NetworkReplayError::OpenFileAlreadyBound(open_file))
            };
        }
        if self.reverse_bindings.contains_key(&channel) {
            return Err(NetworkReplayError::ChannelAlreadyBound(channel));
        }
        self.bindings.insert(open_file, channel);
        self.reverse_bindings.insert(channel, open_file);
        Ok(())
    }

    /// Remove a binding only when the caller has proved the final OFD alias closed.
    pub fn retire_open_file(&mut self, open_file: OpenFileId) -> Option<NetworkChannelId> {
        let channel = self.bindings.remove(&open_file)?;
        self.reverse_bindings.remove(&channel);
        Some(channel)
    }

    /// Resolve a stable OFD binding.
    pub fn channel_for(&self, open_file: OpenFileId) -> Option<NetworkChannelId> {
        self.bindings.get(&open_file).copied()
    }

    /// Release every currently eligible external observation.
    ///
    /// Events on different channels do not block one another. Within a channel,
    /// a later event cannot overtake an earlier unavailable event.
    pub fn release_eligible(
        &mut self,
        now: LogicalTime,
    ) -> Result<BTreeSet<NetworkChannelId>, NetworkReplayError> {
        let EngineState::Replay(replay) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let mut blocked = BTreeSet::new();
        let mut ready = BTreeSet::new();
        for index in 0..replay.trace.inputs.len() {
            if replay.released[index] {
                continue;
            }
            let event = &replay.trace.inputs[index];
            if blocked.contains(&event.channel) {
                continue;
            }
            let channel = replay
                .channels
                .get(&event.channel)
                .expect("validated channel");
            if !event.release.is_eligible(now, channel.transmitted) {
                blocked.insert(event.channel);
                continue;
            }
            let event = event.clone();
            replay
                .channels
                .get_mut(&event.channel)
                .expect("validated channel")
                .release(event.event)?;
            replay.released[index] = true;
            ready.insert(event.channel);
        }
        Ok(ready)
    }

    /// Earliest finite release time whose per-channel transmit watermark is met.
    ///
    /// The scheduler may advance its existing continuous clock to this exact
    /// time; this function never rounds, resets, or mutates time itself.
    pub fn next_release_time(&self) -> Result<Option<LogicalTime>, NetworkReplayError> {
        let EngineState::Replay(replay) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let mut blocked = BTreeSet::new();
        let mut earliest: Option<LogicalTime> = None;
        for (index, event) in replay.trace.inputs.iter().enumerate() {
            if replay.released[index] || blocked.contains(&event.channel) {
                continue;
            }
            let channel = replay
                .channels
                .get(&event.channel)
                .expect("validated channel");
            if event.release.after_transmitted_offset > channel.transmitted {
                blocked.insert(event.channel);
                continue;
            }
            earliest = Some(
                earliest.map_or(event.release.not_before_global_time, |current| {
                    current.min(event.release.not_before_global_time)
                }),
            );
            blocked.insert(event.channel);
        }
        Ok(earliest)
    }

    /// Consume available stream bytes for whichever scheduled reader called.
    pub fn receive_stream(
        &mut self,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
    ) -> Result<StreamReceiveOutcome, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        if maximum == 0 {
            return Ok(StreamReceiveOutcome::Bytes(Vec::new()));
        }
        if let Some((offset, errno)) = state.inbound_errors.front().copied()
            && offset == state.inbound_consumed
            && state.inbound_stream.is_empty()
        {
            state.inbound_errors.pop_front();
            return Ok(StreamReceiveOutcome::Error(errno));
        }
        if !state.inbound_stream.is_empty() {
            let count = maximum.min(state.inbound_stream.len());
            let bytes: Vec<_> = state.inbound_stream.drain(..count).collect();
            state.inbound_consumed += bytes.len() as u64;
            return Ok(StreamReceiveOutcome::Bytes(bytes));
        }
        if state.peer_write_closed_at == Some(state.inbound_consumed) {
            return Ok(StreamReceiveOutcome::EndOfFile);
        }
        Ok(if nonblocking {
            StreamReceiveOutcome::WouldBlock
        } else {
            StreamReceiveOutcome::Pending
        })
    }

    /// Consume one recorded datagram boundary.
    pub fn receive_datagram(
        &mut self,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
    ) -> Result<DatagramReceiveOutcome, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if !state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        if let Some((_, errno)) = state.inbound_errors.pop_front() {
            return Ok(DatagramReceiveOutcome::Error(errno));
        }
        let Some(datagram) = state.inbound_datagrams.pop_front() else {
            return Ok(if nonblocking {
                DatagramReceiveOutcome::WouldBlock
            } else {
                DatagramReceiveOutcome::Pending
            });
        };
        let original_len = datagram.bytes.len();
        Ok(DatagramReceiveOutcome::Datagram(DatagramDelivery {
            bytes: datagram.bytes[..maximum.min(original_len)].to_vec(),
            original_len,
            source: datagram.source,
            destination: datagram.destination,
            ancillary: datagram.ancillary,
            message_flags: datagram.message_flags,
        }))
    }

    /// Validate stream bytes, allowing syscall chunks to split or coalesce
    /// recorded fragments while preserving exact byte order.
    pub fn transmit_stream(
        &mut self,
        open_file: OpenFileId,
        bytes: &[u8],
    ) -> Result<StreamTransmitOutcome, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if state.transport.is_datagram() || state.local_write_closed {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        if let Some((offset, errno)) = state.outbound_errors.front().copied()
            && offset == state.transmitted
        {
            state.outbound_errors.pop_front();
            return Ok(StreamTransmitOutcome::Error(errno));
        }
        let start = usize::try_from(state.transmitted).map_err(|_| NetworkReplayError::Overflow)?;
        let next_error = state
            .outbound_errors
            .front()
            .map_or(state.expected_stream.len(), |(offset, _)| *offset as usize);
        let accepted = bytes
            .len()
            .min(next_error.saturating_sub(start))
            .min(state.expected_stream.len().saturating_sub(start));
        if accepted == 0 && !bytes.is_empty() {
            return Err(NetworkReplayError::TraceExhausted(channel));
        }
        let expected = &state.expected_stream[start..start + accepted];
        if expected != &bytes[..accepted] {
            return Err(NetworkReplayError::OutboundMismatch {
                channel,
                offset: state.transmitted,
            });
        }
        state.transmitted += accepted as u64;
        Ok(StreamTransmitOutcome::Accepted(accepted))
    }

    /// Validate one exact outbound datagram boundary and metadata.
    pub fn transmit_datagram(
        &mut self,
        open_file: OpenFileId,
        datagram: &NetworkDatagramV2,
    ) -> Result<(), NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if !state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let expected = state
            .outbound_datagrams
            .pop_front()
            .ok_or(NetworkReplayError::TraceExhausted(channel))?;
        if expected != *datagram {
            return Err(NetworkReplayError::OutboundMismatch {
                channel,
                offset: state.transmitted,
            });
        }
        state.transmitted = state
            .transmitted
            .checked_add(datagram.bytes.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        Ok(())
    }

    /// Validate a local shutdown transition at current outbound progress.
    pub fn shutdown(
        &mut self,
        open_file: OpenFileId,
        direction: NetworkShutdownV2,
    ) -> Result<(), NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        let Some((offset, expected)) = state.outbound_shutdowns.pop_front() else {
            return Err(NetworkReplayError::UnexpectedShutdown(channel));
        };
        if offset != state.transmitted || expected != direction {
            return Err(NetworkReplayError::UnexpectedShutdown(channel));
        }
        if matches!(
            direction,
            NetworkShutdownV2::Write | NetworkShutdownV2::Both
        ) {
            state.local_write_closed = true;
        }
        Ok(())
    }

    /// Consume one ready connect or accept observation.
    pub fn take_connection_outcome(
        &mut self,
        open_file: OpenFileId,
    ) -> Result<Option<ConnectionOutcome>, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        Ok(self.replay_channel_mut(channel)?.control.pop_front())
    }

    /// Readiness derived from available modeled state, not waiter identity.
    pub fn readiness(
        &self,
        open_file: OpenFileId,
    ) -> Result<NetworkReadinessV2, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel(channel)?;
        let mut readiness = state.explicit_readiness;
        readiness.readable |= !state.inbound_stream.is_empty()
            || !state.inbound_datagrams.is_empty()
            || !state.inbound_errors.is_empty()
            || !state.control.is_empty()
            || state.peer_write_closed_at == Some(state.inbound_consumed);
        readiness.writable |=
            !state.local_write_closed && (state.transmitted as usize) < state.expected_stream.len();
        readiness.error |= !state.inbound_errors.is_empty();
        readiness.hangup |= state.peer_write_closed_at == Some(state.inbound_consumed);
        Ok(readiness)
    }

    /// Prove that replay consumed every required input and validated every
    /// output. Any remainder is a fail-closed mismatch.
    pub fn finish(&self) -> Result<(), NetworkReplayError> {
        let EngineState::Replay(replay) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if replay.released.iter().any(|released| !released) {
            return Err(NetworkReplayError::UnconsumedTrace);
        }
        for (channel, state) in &replay.channels {
            if !state.inbound_stream.is_empty()
                || !state.inbound_datagrams.is_empty()
                || !state.inbound_errors.is_empty()
                || !state.control.is_empty()
                || (!state.transport.is_datagram()
                    && state.transmitted as usize != state.expected_stream.len())
                || !state.outbound_datagrams.is_empty()
                || !state.outbound_errors.is_empty()
                || !state.outbound_shutdowns.is_empty()
            {
                return Err(NetworkReplayError::UnconsumedChannel(*channel));
            }
        }
        Ok(())
    }

    fn has_channel(&self, channel: NetworkChannelId) -> bool {
        match &self.mode {
            EngineState::Record(trace) => trace.channels.iter().any(|item| item.id == channel),
            EngineState::Replay(replay) => replay.channels.contains_key(&channel),
        }
    }

    fn bound_channel(&self, open_file: OpenFileId) -> Result<NetworkChannelId, NetworkReplayError> {
        self.bindings
            .get(&open_file)
            .copied()
            .ok_or(NetworkReplayError::UnboundOpenFile(open_file))
    }

    fn replay_channel(
        &self,
        channel: NetworkChannelId,
    ) -> Result<&ChannelState, NetworkReplayError> {
        let EngineState::Replay(replay) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        Ok(replay.channels.get(&channel).expect("bound channel exists"))
    }

    fn replay_channel_mut(
        &mut self,
        channel: NetworkChannelId,
    ) -> Result<&mut ChannelState, NetworkReplayError> {
        let EngineState::Replay(replay) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        Ok(replay
            .channels
            .get_mut(&channel)
            .expect("bound channel exists"))
    }
}

impl ChannelState {
    fn new(channel: &NetworkChannelV2) -> Self {
        Self {
            transport: channel.transport,
            inbound_stream: VecDeque::new(),
            inbound_consumed: 0,
            inbound_datagrams: VecDeque::new(),
            inbound_errors: VecDeque::new(),
            peer_write_closed_at: None,
            control: VecDeque::new(),
            explicit_readiness: NetworkReadinessV2::default(),
            expected_stream: Vec::new(),
            transmitted: 0,
            outbound_datagrams: VecDeque::new(),
            outbound_errors: VecDeque::new(),
            outbound_shutdowns: VecDeque::new(),
            local_write_closed: false,
        }
    }

    fn append_expected_output(&mut self, output: &NetworkOutputKindV2) {
        match output {
            NetworkOutputKindV2::StreamBytes { bytes, .. } => {
                self.expected_stream.extend_from_slice(bytes)
            }
            NetworkOutputKindV2::Datagram(datagram) => {
                self.outbound_datagrams.push_back(datagram.clone())
            }
            NetworkOutputKindV2::Shutdown {
                stream_offset,
                direction,
            } => self
                .outbound_shutdowns
                .push_back((*stream_offset, *direction)),
            NetworkOutputKindV2::SocketError {
                stream_offset,
                errno,
            } => self.outbound_errors.push_back((*stream_offset, *errno)),
        }
    }

    fn release(&mut self, input: NetworkInputKindV2) -> Result<(), NetworkReplayError> {
        match input {
            NetworkInputKindV2::Connect(result) => {
                self.control.push_back(ConnectionOutcome::Connect(result))
            }
            NetworkInputKindV2::Accept {
                accepted,
                peer,
                ancillary,
            } => self.control.push_back(ConnectionOutcome::Accept {
                accepted,
                peer,
                ancillary,
            }),
            NetworkInputKindV2::StreamBytes { bytes, .. } => self.inbound_stream.extend(bytes),
            NetworkInputKindV2::Datagram(datagram) => self.inbound_datagrams.push_back(datagram),
            NetworkInputKindV2::PeerShutdown {
                stream_offset,
                direction,
            } => {
                if matches!(
                    direction,
                    NetworkShutdownV2::Write | NetworkShutdownV2::Both
                ) {
                    self.peer_write_closed_at = Some(stream_offset);
                }
            }
            NetworkInputKindV2::SocketError {
                stream_offset,
                errno,
            } => self.inbound_errors.push_back((stream_offset, errno)),
            NetworkInputKindV2::Readiness(readiness) => self.explicit_readiness = readiness,
        }
        Ok(())
    }
}

/// Fail-closed network capture/replay error.
#[derive(Debug)]
pub enum NetworkReplayError {
    /// Operation belongs to the other engine mode.
    WrongMode,
    /// Trace failed semantic validation.
    InvalidTrace(NetworkTraceValidationError),
    /// OFD is not a socket identity.
    NonSocketOpenFile(OpenFileId),
    /// Trace channel is unknown.
    UnknownChannel(NetworkChannelId),
    /// Recorder already contains the channel.
    ChannelAlreadyExists(NetworkChannelId),
    /// OFD is already bound to another channel.
    OpenFileAlreadyBound(OpenFileId),
    /// Channel is already bound to another OFD.
    ChannelAlreadyBound(NetworkChannelId),
    /// No channel is bound to this OFD.
    UnboundOpenFile(OpenFileId),
    /// Operation does not match channel transport.
    TransportMismatch(NetworkChannelId),
    /// Guest output bytes or datagram metadata differ.
    OutboundMismatch {
        /// Mismatched channel.
        channel: NetworkChannelId,
        /// First mismatched stream offset.
        offset: u64,
    },
    /// Trace has no output remaining for the guest operation.
    TraceExhausted(NetworkChannelId),
    /// Shutdown did not match the recorded direction or offset.
    UnexpectedShutdown(NetworkChannelId),
    /// Offset arithmetic overflowed.
    Overflow,
    /// Some external observations never became eligible.
    UnconsumedTrace,
    /// A channel retained unread input or unmatched output.
    UnconsumedChannel(NetworkChannelId),
}

impl fmt::Display for NetworkReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "network replay mismatch: {self:?}")
    }
}

impl Error for NetworkReplayError {}

impl From<NetworkTraceValidationError> for NetworkReplayError {
    fn from(error: NetworkTraceValidationError) -> Self {
        Self::InvalidTrace(error)
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use detcore_model::network_trace::NetworkAddressV2;
    use detcore_model::network_trace::NetworkEndpointRoleV2;
    use detcore_model::network_trace::NetworkReleaseV2;
    use detcore_model::pid::DetTid;

    use super::*;

    fn channel_id() -> NetworkChannelId {
        NetworkChannelId(1)
    }

    fn open_file(sequence: u64) -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), sequence)
    }

    fn epoch() -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap()
    }

    fn time(delta: u64) -> LogicalTime {
        LogicalTime::from_nanos(1_790_000_000_000_000_000 + delta)
    }

    fn channel() -> NetworkChannelV2 {
        NetworkChannelV2 {
            id: channel_id(),
            transport: NetworkTransportV2::Tcp,
            role: NetworkEndpointRoleV2::OutboundClient,
            local_address: Some(NetworkAddressV2::Inet4 {
                address: [10, 0, 0, 2],
                port: 40_000,
            }),
            peer_address: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 1],
                port: 443,
            }),
            accepted_from: None,
        }
    }

    fn trace() -> NetworkTraceV2 {
        NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![channel()],
            outputs: vec![NetworkOutputEventV2 {
                channel: channel_id(),
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"request".to_vec(),
                },
            }],
            inputs: vec![
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel: channel_id(),
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(10),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV2::StreamBytes {
                        stream_offset: 0,
                        bytes: b"response".to_vec(),
                    },
                },
                NetworkInputEventV2 {
                    ordinal: 1,
                    channel: channel_id(),
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(20),
                        after_transmitted_offset: 7,
                    },
                    event: NetworkInputKindV2::PeerShutdown {
                        stream_offset: 8,
                        direction: NetworkShutdownV2::Write,
                    },
                },
            ],
        }
    }

    #[test]
    fn different_readers_can_consume_one_available_stream() {
        let mut engine = NetworkReplayEngine::replay(trace()).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        assert_eq!(
            engine.transmit_stream(ofd, b"req").unwrap(),
            StreamTransmitOutcome::Accepted(3)
        );
        engine.release_eligible(time(10)).unwrap();

        // No reader identity was registered: whichever scheduled caller reaches
        // the engine first consumes the prefix, and the next gets the suffix.
        assert_eq!(
            engine.receive_stream(ofd, 3, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"res".to_vec())
        );
        assert_eq!(
            engine.receive_stream(ofd, 32, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"ponse".to_vec())
        );
    }

    #[test]
    fn release_requires_time_and_per_channel_outbound_progress() {
        let mut engine = NetworkReplayEngine::replay(trace()).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        assert!(engine.release_eligible(time(100)).unwrap().is_empty());
        assert_eq!(engine.next_release_time().unwrap(), None);
        engine.transmit_stream(ofd, b"req").unwrap();
        assert_eq!(engine.next_release_time().unwrap(), Some(time(10)));
        assert!(engine.release_eligible(time(9)).unwrap().is_empty());
        assert_eq!(
            engine.release_eligible(time(10)).unwrap(),
            [channel_id()].into()
        );
    }

    #[test]
    fn output_matching_allows_split_and_coalesce_but_refuses_first_bad_byte() {
        let mut split = NetworkReplayEngine::replay(trace()).unwrap();
        let ofd = open_file(0);
        split.bind(ofd, channel_id()).unwrap();
        assert_eq!(
            split.transmit_stream(ofd, b"re").unwrap(),
            StreamTransmitOutcome::Accepted(2)
        );
        assert_eq!(
            split.transmit_stream(ofd, b"quest").unwrap(),
            StreamTransmitOutcome::Accepted(5)
        );

        let mut bad = NetworkReplayEngine::replay(trace()).unwrap();
        bad.bind(ofd, channel_id()).unwrap();
        assert!(matches!(
            bad.transmit_stream(ofd, b"reqX"),
            Err(NetworkReplayError::OutboundMismatch { offset: 0, .. })
        ));
    }

    #[test]
    fn binding_is_ofd_stable_and_fd_reuse_cannot_retarget_it() {
        let mut engine = NetworkReplayEngine::replay(trace()).unwrap();
        let original = open_file(0);
        engine.bind(original, channel_id()).unwrap();
        assert_eq!(engine.channel_for(original), Some(channel_id()));
        assert_eq!(engine.retire_open_file(original), Some(channel_id()));
        let reused_numeric_fd_but_new_ofd = open_file(1);
        assert_eq!(engine.channel_for(reused_numeric_fd_but_new_ofd), None);
        engine
            .bind(reused_numeric_fd_but_new_ofd, channel_id())
            .unwrap();
    }

    #[test]
    fn record_path_canonicalizes_ordinals_and_validates_on_finish() {
        let mut engine = NetworkReplayEngine::record(epoch());
        engine.record_channel(channel()).unwrap();
        engine
            .record_output(NetworkOutputEventV2 {
                channel: channel_id(),
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"x".to_vec(),
                },
            })
            .unwrap();
        engine
            .record_input(NetworkInputEventV2 {
                ordinal: 999,
                channel: channel_id(),
                release: NetworkReleaseV2 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 1,
                },
                event: NetworkInputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"y".to_vec(),
                },
            })
            .unwrap();
        let recorded = engine.into_recorded_trace().unwrap();
        assert_eq!(recorded.inputs[0].ordinal, 0);
    }
}
