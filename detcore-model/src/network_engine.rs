/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The pure state machine behind network record and replay.
//!
//! The engine models the guest-visible state of each outbound TCP channel:
//! which inbound bytes have been released to the guest, whether the peer
//! closed or reset the connection, how many bytes the guest transmitted, and
//! the receive low-water mark. It performs no I/O. The runtime asks it what a
//! guest `recv`, `poll` or `send` observes at a given global time, and in
//! record mode feeds it the bytes it pulled from the host.
//!
//! Determinism rests on the release rule of [`NetworkReleaseV1`]: an input
//! becomes visible once global time has reached the time at which the
//! recording first observed it *and* the guest has transmitted at least as
//! many bytes on that channel as it had by then. Both quantities are
//! functions of the guest's own deterministic execution, so a replay never
//! depends on host timing, and a replay that follows the recorded schedule
//! observes every input at the same check that the recording did.
//!
//! A blocking receive completes once enough bytes are available to satisfy
//! its target, all at once. Linux may copy a partial segment into the buffer
//! of a waiting reader before the rest arrives; the engine instead hands out
//! the bytes as if they had arrived together, which is one of the outcomes
//! Linux permits.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::fd::OpenFileId;
use crate::network_trace::NetworkAddressV1;
use crate::network_trace::NetworkChannelV2;
use crate::network_trace::NetworkEndpointRoleV1;
use crate::network_trace::NetworkInputEventV1;
use crate::network_trace::NetworkInputKindV1;
use crate::network_trace::NetworkOutputV1;
use crate::network_trace::NetworkReleaseV1;
use crate::network_trace::NetworkTraceV2;
use crate::network_trace::NetworkTraceValidationError;
use crate::network_trace::NetworkTransportV1;
use crate::time::LogicalTime;

/// Something the recorder pulled from a host socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkArrival {
    Bytes(Vec<u8>),
    /// The peer shut down its write side (the host `recv` returned 0).
    PeerWriteClosed,
    /// The host socket reported this positive errno.
    Error(i32),
}

/// What a guest receive observes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRecvOutcome {
    Data(Vec<u8>),
    /// End of stream: `recv` returns 0.
    Eof,
    /// A pending socket error, consumed by this receive.
    Error(i32),
    /// Not enough bytes for the receive's target yet.
    WouldBlock,
}

/// A failure that ends the run: the guest asked for something the recording
/// cannot answer, or that this version does not model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkEngineError {
    /// The operation needs a different mode (for example a replay-only call
    /// during record).
    WrongMode,
    UnknownChannel(OpenFileId),
    /// A channel connected twice.
    ChannelExists(OpenFileId),
    /// Replay connected a socket the recording never connected.
    NotInTrace(OpenFileId),
    PeerMismatch {
        channel: OpenFileId,
        recorded: NetworkAddressV1,
        attempted: NetworkAddressV1,
    },
    /// Replay transmitted bytes that differ from the recording.
    OutboundMismatch {
        channel: OpenFileId,
        offset: u64,
    },
    /// Replay transmitted more bytes than the recording holds.
    OutboundBeyondRecording {
        channel: OpenFileId,
        offset: u64,
    },
    /// The recorder fed an arrival after the stream ended, or an empty one.
    InvalidArrival(OpenFileId),
    /// The finished recording failed validation.
    Trace(NetworkTraceValidationError),
}

impl fmt::Display for NetworkEngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongMode => write!(f, "network engine called in the wrong mode"),
            Self::UnknownChannel(id) => write!(f, "network trace: {id} is not connected"),
            Self::ChannelExists(id) => write!(
                f,
                "network trace: {id} connected twice, which network record/replay does not support"
            ),
            Self::NotInTrace(id) => write!(f, "network trace has no recorded connection for {id}"),
            Self::PeerMismatch {
                channel,
                recorded,
                attempted,
            } => write!(
                f,
                "network trace mismatch: {channel} connected to {attempted:?}, but the recording connected it to {recorded:?}"
            ),
            Self::OutboundMismatch { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} sent different bytes than the recording at stream offset {offset}"
            ),
            Self::OutboundBeyondRecording { channel, offset } => write!(
                f,
                "network outbound mismatch: {channel} sent bytes past the end of the recording at stream offset {offset}"
            ),
            Self::InvalidArrival(id) => write!(
                f,
                "network record received input for {id} after its stream ended"
            ),
            Self::Trace(error) => write!(f, "network trace: {error}"),
        }
    }
}

impl Error for NetworkEngineError {}

/// One engine operation, carried from a guest thread to the global state.
///
/// Every request that observes inbound state carries the arrivals the
/// recorder pulled from the host just before it, so that one round trip
/// both records the pull and answers the guest at the same global time.
/// Replay sends no arrivals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkRequest {
    /// See [`NetworkEngine::record_connect`].
    RecordConnect {
        id: OpenFileId,
        peer: NetworkAddressV1,
        local: Option<NetworkAddressV1>,
        errno: i32,
        nonblocking: bool,
    },
    /// See [`NetworkEngine::replay_connect`].
    ReplayConnect {
        id: OpenFileId,
        peer: NetworkAddressV1,
        nonblocking: bool,
    },
    /// See [`NetworkEngine::recv`].
    Recv {
        id: OpenFileId,
        arrivals: Vec<NetworkArrival>,
        max_len: usize,
        target: usize,
        peek: bool,
    },
    /// The readiness of each channel given its `SO_RCVLOWAT`; see
    /// [`NetworkEngine::readiness`].
    Readiness(Vec<(OpenFileId, usize, Vec<NetworkArrival>)>),
    /// See [`NetworkEngine::take_error`].
    TakeError(OpenFileId),
    /// See [`NetworkEngine::send_failure`].
    SendFailure(OpenFileId),
    /// See [`NetworkEngine::record_send`].
    RecordSend { id: OpenFileId, bytes: Vec<u8> },
    /// See [`NetworkEngine::replay_send`].
    ReplaySend { id: OpenFileId, bytes: Vec<u8> },
    /// See [`NetworkEngine::shutdown`].
    Shutdown { id: OpenFileId, how: i32 },
    /// See [`NetworkEngine::addresses`].
    Addresses(OpenFileId),
}

/// The answer to a [`NetworkRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkReply {
    /// `0` or a positive errno.
    Errno(i32),
    /// A positive errno, or `None` for success.
    Failure(Option<i32>),
    Recv(NetworkRecvOutcome),
    /// Poll events, one per requested channel.
    Events(Vec<i16>),
    Sent(usize),
    /// Local (if connected) and peer address, or `None` for an unknown channel.
    Addresses(Option<(Option<NetworkAddressV1>, NetworkAddressV1)>),
}

#[derive(Debug)]
enum Mode {
    Record {
        epoch: DateTime<Utc>,
        inputs: Vec<NetworkInputEventV1>,
        outputs: Vec<NetworkOutputV1>,
    },
    Replay {
        /// Recorded channels the guest has not connected yet, each holding
        /// its input and output streams.
        unconnected: BTreeMap<OpenFileId, Channel>,
    },
}

#[derive(Debug)]
struct Channel {
    record: NetworkChannelV2,
    /// Released bytes the guest has not read yet.
    rx: VecDeque<u8>,
    /// Stream offset just past the last released byte.
    rx_end: u64,
    peer_closed: bool,
    /// The connection was reset or failed: both directions are shut down.
    reset: bool,
    /// A socket error the guest has not consumed yet.
    pending_error: Option<i32>,
    /// Bytes the guest has transmitted.
    tx: u64,
    shut_rd: bool,
    shut_wr: bool,
    /// Replay: inputs not yet released, in order.
    pending_inputs: VecDeque<NetworkInputEventV1>,
    /// Replay: outbound fragments not yet fully matched, in order.
    expected_outputs: VecDeque<NetworkOutputV1>,
}

impl Channel {
    fn new(record: NetworkChannelV2) -> Self {
        Channel {
            reset: record.connect_errno != 0,
            pending_error: None,
            record,
            rx: VecDeque::new(),
            rx_end: 0,
            peer_closed: false,
            tx: 0,
            shut_rd: false,
            shut_wr: false,
            pending_inputs: VecDeque::new(),
            expected_outputs: VecDeque::new(),
        }
    }

    fn input_ended(&self) -> bool {
        self.peer_closed || self.reset
    }

    fn apply(&mut self, event: NetworkInputKindV1) {
        match event {
            NetworkInputKindV1::InboundBytes { bytes, .. } => {
                self.rx_end += bytes.len() as u64;
                self.rx.extend(bytes);
            }
            NetworkInputKindV1::PeerWriteClosed { .. } => self.peer_closed = true,
            NetworkInputKindV1::SocketError { errno, .. } => {
                self.reset = true;
                self.pending_error = Some(errno);
            }
        }
    }

    fn release(&mut self, now: LogicalTime) {
        while let Some(input) = self.pending_inputs.front() {
            if !input.release.is_eligible(now, self.tx) {
                break;
            }
            let input = self.pending_inputs.pop_front().expect("front exists");
            self.apply(input.event);
        }
    }
}

/// The guest-visible network state of one record or replay run.
#[derive(Debug)]
pub struct NetworkEngine {
    mode: Mode,
    channels: BTreeMap<OpenFileId, Channel>,
}

impl NetworkEngine {
    /// Start recording; `epoch` is the run's configured epoch.
    pub fn new_record(epoch: DateTime<Utc>) -> Self {
        NetworkEngine {
            mode: Mode::Record {
                epoch,
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
            channels: BTreeMap::new(),
        }
    }

    /// Replay `trace`, which must already have passed validation.
    pub fn new_replay(trace: NetworkTraceV2) -> Self {
        let mut unconnected: BTreeMap<OpenFileId, Channel> = trace
            .channels
            .into_iter()
            .map(|record| (record.id, Channel::new(record)))
            .collect();
        for input in trace.inputs {
            if let Some(channel) = unconnected.get_mut(&input.channel) {
                channel.pending_inputs.push_back(input);
            }
        }
        for output in trace.outputs {
            if let Some(channel) = unconnected.get_mut(&output.channel) {
                channel.expected_outputs.push_back(output);
            }
        }
        NetworkEngine {
            mode: Mode::Replay { unconnected },
            channels: BTreeMap::new(),
        }
    }

    pub fn is_record(&self) -> bool {
        matches!(self.mode, Mode::Record { .. })
    }

    /// Whether the guest connected `id` as a network channel.
    pub fn is_channel(&self, id: OpenFileId) -> bool {
        self.channels.contains_key(&id)
    }

    fn channel(&mut self, id: OpenFileId) -> Result<&mut Channel, NetworkEngineError> {
        self.channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))
    }

    fn check_new(&self, id: OpenFileId) -> Result<(), NetworkEngineError> {
        if self.channels.contains_key(&id) {
            return Err(NetworkEngineError::ChannelExists(id));
        }
        Ok(())
    }

    /// Start the guest-visible life of `channel` and return what the guest
    /// `connect` returns: `0` or a positive errno. A nonblocking connect
    /// always returns `EINPROGRESS` and leaves the outcome for `SO_ERROR`,
    /// which is one of the behaviours Linux permits even on loopback.
    fn connected(&mut self, mut channel: Channel, nonblocking: bool) -> i32 {
        let errno = channel.record.connect_errno;
        let result = if nonblocking {
            channel.pending_error = (errno != 0).then_some(errno);
            libc::EINPROGRESS
        } else {
            errno
        };
        self.channels.insert(channel.record.id, channel);
        result
    }

    /// Record a connect the runtime performed on the host. `errno` is `0` on
    /// success, and `local` is the host-assigned local address.
    pub fn record_connect(
        &mut self,
        id: OpenFileId,
        peer: NetworkAddressV1,
        local: Option<NetworkAddressV1>,
        errno: i32,
        nonblocking: bool,
    ) -> Result<i32, NetworkEngineError> {
        if !self.is_record() {
            return Err(NetworkEngineError::WrongMode);
        }
        self.check_new(id)?;
        let record = NetworkChannelV2 {
            id,
            transport: NetworkTransportV1::Tcp,
            role: NetworkEndpointRoleV1::OutboundClient,
            local_address: if errno == 0 { local } else { None },
            peer_address: peer,
            connect_errno: errno,
        };
        Ok(self.connected(Channel::new(record), nonblocking))
    }

    /// Replay the recorded connect of `id`, which must target the recorded peer.
    pub fn replay_connect(
        &mut self,
        id: OpenFileId,
        peer: &NetworkAddressV1,
        nonblocking: bool,
    ) -> Result<i32, NetworkEngineError> {
        self.check_new(id)?;
        let Mode::Replay { unconnected } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        let recorded = &unconnected
            .get(&id)
            .ok_or(NetworkEngineError::NotInTrace(id))?
            .record
            .peer_address;
        if recorded != peer {
            return Err(NetworkEngineError::PeerMismatch {
                channel: id,
                recorded: recorded.clone(),
                attempted: peer.clone(),
            });
        }
        let channel = unconnected.remove(&id).expect("checked above");
        Ok(self.connected(channel, nonblocking))
    }

    /// The local and peer addresses of a connected channel, for
    /// `getsockname` and `getpeername`.
    pub fn addresses(
        &self,
        id: OpenFileId,
    ) -> Option<(Option<&NetworkAddressV1>, &NetworkAddressV1)> {
        self.channels.get(&id).map(|channel| {
            (
                channel.record.local_address.as_ref(),
                &channel.record.peer_address,
            )
        })
    }

    /// Whether the recorder should pull more input for `id` from the host.
    pub fn needs_pull(&self, id: OpenFileId) -> bool {
        self.is_record()
            && self
                .channels
                .get(&id)
                .is_some_and(|channel| !channel.input_ended())
    }

    /// Record what the runtime pulled from the host for `id` at global time
    /// `now`, and make it visible to the guest at once.
    pub fn record_arrival(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        arrival: NetworkArrival,
    ) -> Result<(), NetworkEngineError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))?;
        let Mode::Record { inputs, .. } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        if channel.input_ended() || matches!(&arrival, NetworkArrival::Bytes(b) if b.is_empty()) {
            return Err(NetworkEngineError::InvalidArrival(id));
        }
        let stream_offset = channel.rx_end;
        let event = match arrival {
            NetworkArrival::Bytes(bytes) => NetworkInputKindV1::InboundBytes {
                stream_offset,
                bytes,
            },
            NetworkArrival::PeerWriteClosed => {
                NetworkInputKindV1::PeerWriteClosed { stream_offset }
            }
            NetworkArrival::Error(errno) => NetworkInputKindV1::SocketError {
                stream_offset,
                errno,
            },
        };
        inputs.push(NetworkInputEventV1 {
            ordinal: inputs.len() as u64,
            channel: id,
            release: NetworkReleaseV1 {
                not_before_global_time: now,
                after_transmitted_offset: channel.tx,
            },
            event: event.clone(),
        });
        channel.apply(event);
        Ok(())
    }

    /// A guest receive of at most `max_len` bytes at global time `now`.
    ///
    /// `target` is the byte count that completes a blocking receive:
    /// `min(SO_RCVLOWAT, max_len)`, or `max_len` under `MSG_WAITALL`. A
    /// nonblocking receive, or one whose timeout expired, passes `1`. With
    /// `peek` the bytes stay queued.
    pub fn recv(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        max_len: usize,
        target: usize,
        peek: bool,
    ) -> Result<NetworkRecvOutcome, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        if channel.shut_rd {
            return Ok(NetworkRecvOutcome::Eof);
        }
        let available = channel.rx.len();
        if available > 0
            && (available >= target.max(1)
                || channel.input_ended()
                || channel.pending_error.is_some())
        {
            let len = available.min(max_len);
            let bytes = if peek {
                channel.rx.iter().take(len).copied().collect()
            } else {
                channel.rx.drain(..len).collect()
            };
            return Ok(NetworkRecvOutcome::Data(bytes));
        }
        if available == 0 {
            // Linux consumes a pending error even under MSG_PEEK.
            if let Some(errno) = channel.pending_error.take() {
                return Ok(NetworkRecvOutcome::Error(errno));
            }
            if channel.input_ended() {
                return Ok(NetworkRecvOutcome::Eof);
            }
        }
        Ok(NetworkRecvOutcome::WouldBlock)
    }

    /// The `poll` events `id` reports at global time `now`, before masking
    /// with the requested events. Mirrors Linux `tcp_poll`; `lowat` is the
    /// socket's `SO_RCVLOWAT`, which the guest may set before it connects.
    pub fn readiness(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        lowat: usize,
    ) -> Result<i16, NetworkEngineError> {
        let channel = self.channel(id)?;
        channel.release(now);
        let rcv_shutdown = channel.shut_rd || channel.input_ended();
        let snd_shutdown = channel.shut_wr || channel.reset;
        let mut events = 0;
        if (rcv_shutdown && snd_shutdown) || channel.reset {
            events |= libc::POLLHUP;
        }
        if rcv_shutdown {
            events |= libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP;
        } else if channel.rx.len() >= lowat.max(1) {
            events |= libc::POLLIN | libc::POLLRDNORM;
        }
        // Linux reports a socket shut down for writing as writable, so that
        // the write fails instead of blocking.
        events |= libc::POLLOUT | libc::POLLWRNORM;
        if channel.pending_error.is_some() {
            events |= libc::POLLERR;
        }
        Ok(events)
    }

    /// Consume and return the pending socket error, as `SO_ERROR` does;
    /// `0` when there is none.
    pub fn take_error(&mut self, id: OpenFileId) -> Result<i32, NetworkEngineError> {
        Ok(self.channel(id)?.pending_error.take().unwrap_or(0))
    }

    /// The errno a guest send fails with before transmitting anything, or
    /// `None` when the send may proceed. A pending error is consumed first;
    /// otherwise a socket shut down for writing fails with `EPIPE`.
    pub fn send_failure(&mut self, id: OpenFileId) -> Result<Option<i32>, NetworkEngineError> {
        let channel = self.channel(id)?;
        if channel.reset || channel.shut_wr {
            return Ok(Some(channel.pending_error.take().unwrap_or(libc::EPIPE)));
        }
        Ok(None)
    }

    /// Record the bytes a host send accepted.
    pub fn record_send(&mut self, id: OpenFileId, sent: &[u8]) -> Result<(), NetworkEngineError> {
        let channel = self
            .channels
            .get_mut(&id)
            .ok_or(NetworkEngineError::UnknownChannel(id))?;
        let Mode::Record { outputs, .. } = &mut self.mode else {
            return Err(NetworkEngineError::WrongMode);
        };
        if sent.is_empty() {
            return Ok(());
        }
        outputs.push(NetworkOutputV1 {
            channel: id,
            stream_offset: channel.tx,
            bytes: sent.to_vec(),
        });
        channel.tx += sent.len() as u64;
        Ok(())
    }

    /// Match a guest send against the recording and return how many bytes
    /// it transmits: the bytes left in the current recorded fragment, at
    /// most `bytes.len()`. A recorded short write therefore replays as the
    /// same short write.
    pub fn replay_send(
        &mut self,
        id: OpenFileId,
        bytes: &[u8],
    ) -> Result<usize, NetworkEngineError> {
        if self.is_record() {
            return Err(NetworkEngineError::WrongMode);
        }
        let channel = self.channel(id)?;
        if bytes.is_empty() {
            return Ok(0);
        }
        let offset = channel.tx;
        let fragment = channel.expected_outputs.front().ok_or(
            NetworkEngineError::OutboundBeyondRecording {
                channel: id,
                offset,
            },
        )?;
        let start = (offset - fragment.stream_offset) as usize;
        let remaining = &fragment.bytes[start..];
        let len = remaining.len().min(bytes.len());
        if remaining[..len] != bytes[..len] {
            let first_difference = remaining
                .iter()
                .zip(bytes)
                .position(|(expected, actual)| expected != actual)
                .unwrap_or(len);
            return Err(NetworkEngineError::OutboundMismatch {
                channel: id,
                offset: offset + first_difference as u64,
            });
        }
        channel.tx += len as u64;
        if len == remaining.len() {
            channel.expected_outputs.pop_front();
        }
        Ok(len)
    }

    /// Apply `shutdown(how)`; returns `0` or the errno the guest sees.
    pub fn shutdown(&mut self, id: OpenFileId, how: i32) -> Result<i32, NetworkEngineError> {
        let channel = self.channel(id)?;
        if channel.record.connect_errno != 0 {
            return Ok(libc::ENOTCONN);
        }
        match how {
            libc::SHUT_RD => channel.shut_rd = true,
            libc::SHUT_WR => channel.shut_wr = true,
            libc::SHUT_RDWR => {
                channel.shut_rd = true;
                channel.shut_wr = true;
            }
            _ => return Ok(libc::EINVAL),
        }
        Ok(0)
    }

    /// Record pulled arrivals at `now`. Pulls that follow the end of the
    /// stream are dropped: a drained host socket keeps reporting it.
    fn record_arrivals(
        &mut self,
        id: OpenFileId,
        now: LogicalTime,
        arrivals: Vec<NetworkArrival>,
    ) -> Result<(), NetworkEngineError> {
        for arrival in arrivals {
            if !self.needs_pull(id) {
                break;
            }
            self.record_arrival(id, now, arrival)?;
        }
        Ok(())
    }

    /// Perform `request` at global time `now`.
    pub fn apply(
        &mut self,
        now: LogicalTime,
        request: NetworkRequest,
    ) -> Result<NetworkReply, NetworkEngineError> {
        Ok(match request {
            NetworkRequest::RecordConnect {
                id,
                peer,
                local,
                errno,
                nonblocking,
            } => NetworkReply::Errno(self.record_connect(id, peer, local, errno, nonblocking)?),
            NetworkRequest::ReplayConnect {
                id,
                peer,
                nonblocking,
            } => NetworkReply::Errno(self.replay_connect(id, &peer, nonblocking)?),
            NetworkRequest::Recv {
                id,
                arrivals,
                max_len,
                target,
                peek,
            } => {
                self.record_arrivals(id, now, arrivals)?;
                NetworkReply::Recv(self.recv(id, now, max_len, target, peek)?)
            }
            NetworkRequest::Readiness(channels) => {
                let mut events = Vec::with_capacity(channels.len());
                for (id, lowat, arrivals) in channels {
                    self.record_arrivals(id, now, arrivals)?;
                    events.push(self.readiness(id, now, lowat)?);
                }
                NetworkReply::Events(events)
            }
            NetworkRequest::TakeError(id) => NetworkReply::Errno(self.take_error(id)?),
            NetworkRequest::SendFailure(id) => NetworkReply::Failure(self.send_failure(id)?),
            NetworkRequest::RecordSend { id, bytes } => {
                self.record_send(id, &bytes)?;
                NetworkReply::Sent(bytes.len())
            }
            NetworkRequest::ReplaySend { id, bytes } => {
                NetworkReply::Sent(self.replay_send(id, &bytes)?)
            }
            NetworkRequest::Shutdown { id, how } => NetworkReply::Errno(self.shutdown(id, how)?),
            NetworkRequest::Addresses(id) => NetworkReply::Addresses(
                self.addresses(id)
                    .map(|(local, peer)| (local.cloned(), peer.clone())),
            ),
        })
    }

    /// The trace of a record run. Fails if the run was a replay or the
    /// recording does not validate.
    pub fn finish(mut self) -> Result<NetworkTraceV2, NetworkEngineError> {
        let Mode::Record {
            epoch,
            inputs,
            outputs,
        } = self.mode
        else {
            return Err(NetworkEngineError::WrongMode);
        };
        let trace = NetworkTraceV2 {
            epoch,
            channels: std::mem::take(&mut self.channels)
                .into_values()
                .map(|channel| channel.record)
                .collect(),
            inputs,
            outputs,
        };
        trace.validate().map_err(NetworkEngineError::Trace)?;
        Ok(trace)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use chrono::TimeZone;

    use super::*;
    use crate::pid::DetTid;

    fn epoch() -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap()
    }

    /// Global time `n` microseconds after the epoch.
    fn at(n: u64) -> LogicalTime {
        LogicalTime::from_nanos(1_790_000_000 * 1_000_000_000 + n * 1_000)
    }

    fn sock(n: u64) -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), n)
    }

    fn peer() -> NetworkAddressV1 {
        NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 8080,
        }
    }

    fn local() -> Option<NetworkAddressV1> {
        Some(NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 40_000,
        })
    }

    fn data(bytes: &[u8]) -> NetworkRecvOutcome {
        NetworkRecvOutcome::Data(bytes.to_vec())
    }

    /// A request/response exchange: the response is pulled at time 20,
    /// after the guest sent "req", and the peer closes at time 30.
    fn recorded_exchange() -> NetworkTraceV2 {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        assert_eq!(engine.record_connect(id, peer(), local(), 0, false), Ok(0));
        assert!(engine.needs_pull(id));
        assert_eq!(
            engine.recv(id, at(10), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        engine.record_send(id, b"req").unwrap();
        engine
            .record_arrival(id, at(20), NetworkArrival::Bytes(b"abcdef".to_vec()))
            .unwrap();
        assert_eq!(engine.recv(id, at(20), 3, 1, false), Ok(data(b"abc")));
        engine
            .record_arrival(id, at(30), NetworkArrival::PeerWriteClosed)
            .unwrap();
        assert!(!engine.needs_pull(id));
        assert_eq!(engine.recv(id, at(30), 64, 1, false), Ok(data(b"def")));
        assert_eq!(
            engine.recv(id, at(31), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        engine.finish().unwrap()
    }

    #[test]
    fn record_produces_a_valid_trace_that_round_trips() {
        let trace = recorded_exchange();
        assert_eq!(trace.inputs.len(), 2);
        assert_eq!(
            trace.inputs[0].release,
            NetworkReleaseV1 {
                not_before_global_time: at(20),
                after_transmitted_offset: 3,
            }
        );
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        assert_eq!(
            NetworkTraceV2::read_framed(Cursor::new(bytes)).unwrap(),
            trace
        );
    }

    #[test]
    fn replay_with_the_recorded_schedule_observes_the_same_outcomes() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        assert_eq!(engine.replay_connect(id, &peer(), false), Ok(0));
        assert_eq!(
            engine.recv(id, at(10), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(engine.replay_send(id, b"req"), Ok(3));
        assert_eq!(engine.recv(id, at(20), 3, 1, false), Ok(data(b"abc")));
        assert_eq!(engine.recv(id, at(30), 64, 1, false), Ok(data(b"def")));
        assert_eq!(
            engine.recv(id, at(31), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
    }

    #[test]
    fn replay_input_waits_for_both_time_and_transmission() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        let id = sock(0);
        engine.replay_connect(id, &peer(), false).unwrap();
        // Late enough, but the request has not been sent.
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(engine.replay_send(id, b"req"), Ok(3));
        // Sent, but too early.
        assert_eq!(
            engine.recv(id, at(19), 64, 1, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        // Both: everything due by now is released together.
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(data(b"abcdef"))
        );
        assert_eq!(
            engine.recv(id, at(1_000), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
    }

    #[test]
    fn receive_target_follows_the_low_water_mark() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Bytes(b"ab".to_vec()))
            .unwrap();
        assert_eq!(
            engine.readiness(id, at(1), 3),
            Ok(libc::POLLOUT | libc::POLLWRNORM)
        );
        assert_eq!(
            engine.recv(id, at(1), 3, 3, false),
            Ok(NetworkRecvOutcome::WouldBlock)
        );
        assert_eq!(engine.recv(id, at(1), 3, 1, true), Ok(data(b"ab")));
        engine
            .record_arrival(id, at(2), NetworkArrival::Bytes(b"cd".to_vec()))
            .unwrap();
        assert_eq!(
            engine.readiness(id, at(2), 3),
            Ok(libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM)
        );
        assert_eq!(engine.recv(id, at(2), 3, 3, false), Ok(data(b"abc")));
        // With the stream ended, fewer bytes than the target complete the receive.
        engine
            .record_arrival(id, at(3), NetworkArrival::PeerWriteClosed)
            .unwrap();
        assert_eq!(engine.recv(id, at(3), 3, 3, false), Ok(data(b"d")));
        assert_eq!(
            engine.readiness(id, at(3), 3),
            Ok(libc::POLLIN
                | libc::POLLRDNORM
                | libc::POLLRDHUP
                | libc::POLLOUT
                | libc::POLLWRNORM)
        );
    }

    #[test]
    fn a_reset_delivers_queued_bytes_then_the_error_once_then_eof() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Bytes(b"x".to_vec()))
            .unwrap();
        engine
            .record_arrival(id, at(1), NetworkArrival::Error(libc::ECONNRESET))
            .unwrap();
        assert_eq!(
            engine.record_arrival(id, at(2), NetworkArrival::PeerWriteClosed),
            Err(NetworkEngineError::InvalidArrival(id))
        );
        let readiness = engine.readiness(id, at(1), 1).unwrap();
        assert_eq!(readiness & libc::POLLERR, libc::POLLERR);
        assert_eq!(readiness & libc::POLLHUP, libc::POLLHUP);
        assert_eq!(engine.recv(id, at(1), 64, 64, false), Ok(data(b"x")));
        assert_eq!(
            engine.recv(id, at(1), 64, 1, true),
            Ok(NetworkRecvOutcome::Error(libc::ECONNRESET))
        );
        assert_eq!(
            engine.recv(id, at(1), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        assert_eq!(engine.send_failure(id), Ok(Some(libc::EPIPE)));
        engine.finish().unwrap();
    }

    #[test]
    fn failed_connects_replay_without_the_host() {
        let mut engine = NetworkEngine::new_record(epoch());
        assert_eq!(
            engine.record_connect(sock(0), peer(), None, libc::ECONNREFUSED, false),
            Ok(libc::ECONNREFUSED)
        );
        assert_eq!(
            engine.record_connect(sock(1), peer(), None, libc::ECONNREFUSED, true),
            Ok(libc::EINPROGRESS)
        );
        let trace = engine.finish().unwrap();

        let mut engine = NetworkEngine::new_replay(trace);
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Ok(libc::ECONNREFUSED)
        );
        assert_eq!(engine.take_error(sock(0)), Ok(0));
        assert_eq!(
            engine.replay_connect(sock(1), &peer(), true),
            Ok(libc::EINPROGRESS)
        );
        let readiness = engine.readiness(sock(1), at(1), 1).unwrap();
        assert_eq!(
            readiness & (libc::POLLOUT | libc::POLLERR | libc::POLLHUP),
            libc::POLLOUT | libc::POLLERR | libc::POLLHUP
        );
        assert_eq!(engine.take_error(sock(1)), Ok(libc::ECONNREFUSED));
        assert_eq!(engine.take_error(sock(1)), Ok(0));
        assert_eq!(engine.shutdown(sock(1), libc::SHUT_WR), Ok(libc::ENOTCONN));
    }

    #[test]
    fn replay_refuses_connections_the_recording_lacks() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        assert_eq!(
            engine.replay_connect(sock(5), &peer(), false),
            Err(NetworkEngineError::NotInTrace(sock(5)))
        );
        let other = NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 9,
        };
        assert!(matches!(
            engine.replay_connect(sock(0), &other, false),
            Err(NetworkEngineError::PeerMismatch { .. })
        ));
        engine.replay_connect(sock(0), &peer(), false).unwrap();
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Err(NetworkEngineError::ChannelExists(sock(0)))
        );
    }

    #[test]
    fn replay_send_matches_bytes_and_preserves_fragments() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        // A short write: the host accepted 4 of the guest's 8 bytes.
        engine.record_send(id, b"requ").unwrap();
        engine.record_send(id, b"est\n").unwrap();
        let trace = engine.finish().unwrap();

        let mut engine = NetworkEngine::new_replay(trace.clone());
        engine.replay_connect(id, &peer(), false).unwrap();
        assert_eq!(engine.replay_send(id, b"request\n"), Ok(4));
        assert_eq!(engine.replay_send(id, b"es"), Ok(2));
        assert_eq!(engine.replay_send(id, b"t\n"), Ok(2));
        assert_eq!(
            engine.replay_send(id, b"more"),
            Err(NetworkEngineError::OutboundBeyondRecording {
                channel: id,
                offset: 8
            })
        );

        let mut engine = NetworkEngine::new_replay(trace);
        engine.replay_connect(id, &peer(), false).unwrap();
        let error = engine.replay_send(id, b"reqX").unwrap_err();
        assert_eq!(
            error,
            NetworkEngineError::OutboundMismatch {
                channel: id,
                offset: 3
            }
        );
        let message = error.to_string();
        for word in ["network", "outbound", "mismatch"] {
            assert!(message.contains(word), "{message}");
        }
    }

    #[test]
    fn shutdown_follows_linux() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        engine
            .record_connect(id, peer(), local(), 0, false)
            .unwrap();
        assert_eq!(engine.send_failure(id), Ok(None));
        assert_eq!(engine.shutdown(id, libc::SHUT_WR), Ok(0));
        assert_eq!(engine.send_failure(id), Ok(Some(libc::EPIPE)));
        assert_eq!(engine.shutdown(id, 7), Ok(libc::EINVAL));
        assert_eq!(engine.shutdown(id, libc::SHUT_RD), Ok(0));
        assert_eq!(
            engine.recv(id, at(1), 64, 1, false),
            Ok(NetworkRecvOutcome::Eof)
        );
        assert_eq!(
            engine.readiness(id, at(1), 1).unwrap() & libc::POLLHUP,
            libc::POLLHUP
        );
    }

    #[test]
    fn requests_record_pulls_at_the_time_they_answer() {
        let mut engine = NetworkEngine::new_record(epoch());
        let id = sock(0);
        let reply = engine.apply(
            at(1),
            NetworkRequest::RecordConnect {
                id,
                peer: peer(),
                local: local(),
                errno: 0,
                nonblocking: false,
            },
        );
        assert_eq!(reply, Ok(NetworkReply::Errno(0)));
        let reply = engine.apply(
            at(5),
            NetworkRequest::Readiness(vec![(id, 1, vec![NetworkArrival::Bytes(b"hi".to_vec())])]),
        );
        assert_eq!(
            reply,
            Ok(NetworkReply::Events(vec![
                libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT | libc::POLLWRNORM
            ]))
        );
        // A drained host socket keeps reporting end of stream; only the
        // first report is recorded.
        let reply = engine.apply(
            at(9),
            NetworkRequest::Recv {
                id,
                arrivals: vec![
                    NetworkArrival::PeerWriteClosed,
                    NetworkArrival::PeerWriteClosed,
                ],
                max_len: 64,
                target: 1,
                peek: false,
            },
        );
        assert_eq!(reply, Ok(NetworkReply::Recv(data(b"hi"))));
        assert_eq!(
            engine.apply(at(9), NetworkRequest::Addresses(id)),
            Ok(NetworkReply::Addresses(Some((local(), peer()))))
        );
        let trace = engine.finish().unwrap();
        assert_eq!(trace.inputs.len(), 2);
        assert_eq!(trace.inputs[0].release.not_before_global_time, at(5));
        assert_eq!(trace.inputs[1].release.not_before_global_time, at(9));
    }

    #[test]
    fn modes_are_not_interchangeable() {
        let mut engine = NetworkEngine::new_replay(recorded_exchange());
        assert_eq!(
            engine.record_connect(sock(0), peer(), local(), 0, false),
            Err(NetworkEngineError::WrongMode)
        );
        assert!(matches!(
            engine.finish(),
            Err(NetworkEngineError::WrongMode)
        ));
        let mut engine = NetworkEngine::new_record(epoch());
        assert_eq!(
            engine.replay_connect(sock(0), &peer(), false),
            Err(NetworkEngineError::WrongMode)
        );
        assert_eq!(
            engine.recv(sock(9), at(1), 1, 1, false),
            Err(NetworkEngineError::UnknownChannel(sock(9)))
        );
    }
}
