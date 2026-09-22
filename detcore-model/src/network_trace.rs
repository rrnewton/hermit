/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Versioned data model for schedule-independent external network input.
//!
//! This module deliberately contains no recorder, replayer, or scheduler hook.
//! V1 is the deliberately narrow, single-client prototype. V2 is the runtime
//! format: it assigns trace-stable channel identities independently of guest
//! thread and syscall order, and represents TCP streams, datagram boundaries,
//! readiness, ancillary data, partial progress, shutdown, and socket errors.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::path::PathBuf;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

use crate::fd::OpenFileId;
use crate::time::LogicalTime;

/// The on-disk format magic. The version is stored in the following four bytes.
pub const NETWORK_TRACE_MAGIC: [u8; 16] = *b"HERMIT-NET-TRACE";
/// The legacy single-client network trace format.
pub const NETWORK_TRACE_VERSION_V1: u32 = 1;
/// The multi-channel schedule-independent network trace format.
pub const NETWORK_TRACE_VERSION_V2: u32 = 2;
/// Refuse hostile or corrupt length headers before allocating memory.
pub const MAX_NETWORK_TRACE_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;

const FRAME_HEADER_LEN: usize = NETWORK_TRACE_MAGIC.len() + 4 + 8;

/// Guest network policy selected for one run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// Refuse external networking. This is the deterministic default.
    #[serde(alias = "off")]
    #[default]
    Deny,
    /// Capture supported external networking in a new trace.
    Record,
    /// Replay a trace without consulting the host network.
    Replay,
    /// Consult the live host network without recording it.
    UnsafeLive,
}

/// Configuration for the shared external-network engine.
///
/// `network_perturb_seed` is intentionally optional and has no fallback to the
/// scheduler or global seed. A perturbation layer must consume only this
/// seed so varying `sched_seed` cannot change the external input stream.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceConfig {
    /// Policy for external sockets.
    #[serde(alias = "mode")]
    pub policy: NetworkPolicy,
    /// Trace path for record and replay policies.
    pub path: Option<PathBuf>,
    /// Replay-only perturbation seed, independent of scheduler seeds.
    pub network_perturb_seed: Option<u64>,
}

impl NetworkTraceConfig {
    /// Deterministic fail-closed policy.
    pub fn deny() -> Self {
        Self::default()
    }

    /// Record external network observations at `path`.
    pub fn record(path: impl Into<PathBuf>) -> Self {
        Self {
            policy: NetworkPolicy::Record,
            path: Some(path.into()),
            network_perturb_seed: None,
        }
    }

    /// Replay `path`, optionally perturbing only network delivery.
    pub fn replay(path: impl Into<PathBuf>, network_perturb_seed: Option<u64>) -> Self {
        Self {
            policy: NetworkPolicy::Replay,
            path: Some(path.into()),
            network_perturb_seed,
        }
    }

    /// Explicitly opt into nondeterministic live host networking.
    pub fn unsafe_live() -> Self {
        Self {
            policy: NetworkPolicy::UnsafeLive,
            path: None,
            network_perturb_seed: None,
        }
    }

    /// Whether the container must expose the host network.
    pub fn needs_host_network(&self) -> bool {
        matches!(
            self.policy,
            NetworkPolicy::Record | NetworkPolicy::UnsafeLive
        )
    }

    /// Whether this policy consumes or produces a trace.
    pub fn uses_trace(&self) -> bool {
        matches!(self.policy, NetworkPolicy::Record | NetworkPolicy::Replay)
    }

    /// Validate configuration independently of any scheduler configuration.
    pub fn validate(&self) -> Result<(), NetworkTraceConfigError> {
        match self.policy {
            NetworkPolicy::Deny | NetworkPolicy::UnsafeLive => {
                if self.path.is_some() || self.network_perturb_seed.is_some() {
                    return Err(NetworkTraceConfigError::OptionsWithoutTrace);
                }
            }
            NetworkPolicy::Record => {
                validate_trace_path(self.path.as_ref())?;
                if self.network_perturb_seed.is_some() {
                    return Err(NetworkTraceConfigError::PerturbationDuringRecord);
                }
            }
            NetworkPolicy::Replay => validate_trace_path(self.path.as_ref())?,
        }
        Ok(())
    }
}

fn validate_trace_path(path: Option<&PathBuf>) -> Result<(), NetworkTraceConfigError> {
    let Some(path) = path else {
        return Err(NetworkTraceConfigError::MissingPath);
    };
    if path.as_os_str().is_empty() {
        return Err(NetworkTraceConfigError::EmptyPath);
    }
    Ok(())
}

/// Invalid combinations in [`NetworkTraceConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkTraceConfigError {
    OptionsWithoutTrace,
    MissingPath,
    EmptyPath,
    PerturbationDuringRecord,
}

impl fmt::Display for NetworkTraceConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OptionsWithoutTrace => {
                write!(f, "network trace options require record or replay mode")
            }
            Self::MissingPath => write!(f, "network trace record/replay requires a path"),
            Self::EmptyPath => write!(f, "network trace path must not be empty"),
            Self::PerturbationDuringRecord => {
                write!(f, "network perturbation is replay-only")
            }
        }
    }
}

impl Error for NetworkTraceConfigError {}

/// Internet address recorded without host-layout `sockaddr` padding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkAddressV1 {
    Inet4 {
        address: [u8; 4],
        port: u16,
    },
    Inet6 {
        address: [u8; 16],
        port: u16,
        flowinfo: u32,
        scope_id: u32,
    },
}

impl NetworkAddressV1 {
    fn is_supported_external_peer(&self) -> bool {
        match self {
            Self::Inet4 { address, port } => {
                *port != 0 && is_supported_external_ipv4(Ipv4Addr::from(*address))
            }
            Self::Inet6 { address, port, .. } => {
                let address = Ipv6Addr::from(*address);
                *port != 0
                    && match address.to_ipv4_mapped() {
                        Some(mapped) => is_supported_external_ipv4(mapped),
                        None => {
                            !address.is_loopback()
                                && !address.is_unspecified()
                                && !address.is_multicast()
                        }
                    }
            }
        }
    }

    fn same_family(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Inet4 { .. }, Self::Inet4 { .. }) | (Self::Inet6 { .. }, Self::Inet6 { .. })
        )
    }
}

fn is_supported_external_ipv4(address: Ipv4Addr) -> bool {
    !address.is_loopback()
        && !address.is_unspecified()
        && !address.is_multicast()
        && !address.is_broadcast()
}

/// Transport admitted by the v1 trace envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTransportV1 {
    Tcp,
}

/// Connection role admitted by v1. Server-side `accept` needs a different
/// identity and arrival model and is intentionally not representable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkEndpointRoleV1 {
    OutboundClient,
}

/// The only channel supported by v1: one outbound TCP client connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkChannelV1 {
    /// Stable Detcore open-file-description identity, never a raw fd.
    pub id: OpenFileId,
    pub transport: NetworkTransportV1,
    pub role: NetworkEndpointRoleV1,
    pub local_address: NetworkAddressV1,
    pub peer_address: NetworkAddressV1,
    /// The recorder must prove socket allocation occurred before competing
    /// guest threads could make this identity schedule-dependent.
    pub created_before_competing_threads: bool,
}

/// Conditions that must both hold before an input becomes observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReleaseV1 {
    /// Absolute time in exactly the domain returned by `GlobalTime::as_nanos`.
    /// It includes the trace epoch; it is not a duration since that epoch.
    pub not_before_global_time: LogicalTime,
    /// Number of validated outbound bytes required before release.
    pub after_transmitted_offset: u64,
}

impl NetworkReleaseV1 {
    /// Whether both scheduler-owned eligibility conditions have been met.
    pub fn is_eligible(
        &self,
        committed_global_time: LogicalTime,
        validated_transmitted_offset: u64,
    ) -> bool {
        committed_global_time >= self.not_before_global_time
            && validated_transmitted_offset >= self.after_transmitted_offset
    }
}

/// Supported external observations for the initial TCP byte-stream model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkInputKindV1 {
    InboundBytes {
        stream_offset: u64,
        bytes: Vec<u8>,
    },
    PeerWriteClosed {
        stream_offset: u64,
    },
    /// A terminal stream error, not a retryable or interrupted receive attempt.
    SocketError {
        stream_offset: u64,
        errno: i32,
    },
}

/// One globally ordered external observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInputEventV1 {
    pub ordinal: u64,
    pub channel: OpenFileId,
    pub release: NetworkReleaseV1,
    pub event: NetworkInputKindV1,
}

/// One contiguous recorded fragment of the expected outbound TCP byte stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutputV1 {
    pub channel: OpenFileId,
    pub stream_offset: u64,
    pub bytes: Vec<u8>,
}

/// Version-one schedule-independent network input trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceV1 {
    pub epoch: DateTime<Utc>,
    pub channels: Vec<NetworkChannelV1>,
    pub inputs: Vec<NetworkInputEventV1>,
    pub outputs: Vec<NetworkOutputV1>,
}

impl NetworkTraceV1 {
    /// Convert the recorded epoch to the absolute starting time used by
    /// `GlobalTime`, rejecting Chrono values outside its `u64` nanosecond
    /// domain. `GlobalTime` truncates the epoch to microseconds, so this does
    /// the same conversion without an unchecked signed cast or multiplication.
    pub fn epoch_global_time(&self) -> Result<LogicalTime, NetworkTraceValidationError> {
        let seconds = u64::try_from(self.epoch.timestamp())
            .map_err(|_| NetworkTraceValidationError::EpochOutOfRange)?;
        let whole_seconds = seconds
            .checked_mul(1_000_000_000)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
        let fractional_micros = u64::from(self.epoch.timestamp_subsec_micros())
            .checked_mul(1_000)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)?;
        whole_seconds
            .checked_add(fractional_micros)
            .map(LogicalTime::from_nanos)
            .ok_or(NetworkTraceValidationError::EpochOutOfRange)
    }

    /// Enforce the narrow v1 envelope before either writing or consuming a trace.
    pub fn validate(&self) -> Result<(), NetworkTraceValidationError> {
        if self.channels.len() != 1 {
            return Err(NetworkTraceValidationError::ChannelCount(
                self.channels.len(),
            ));
        }
        let channel = &self.channels[0];
        if !channel.id.is_socket() {
            return Err(NetworkTraceValidationError::NonSocketChannelIdentity);
        }
        if !channel.created_before_competing_threads {
            return Err(NetworkTraceValidationError::ScheduleDependentChannelIdentity);
        }
        if !channel.local_address.same_family(&channel.peer_address) {
            return Err(NetworkTraceValidationError::AddressFamilyMismatch);
        }
        if !channel.peer_address.is_supported_external_peer() {
            return Err(NetworkTraceValidationError::UnsupportedPeerAddress);
        }

        let mut inbound_offset = 0u64;
        let epoch_start = self.epoch_global_time()?;
        let mut previous_time = epoch_start;
        let mut previous_tx_watermark = 0u64;
        let mut terminal = false;

        let total_output = validate_outputs(&self.outputs, channel.id)?;
        for (index, input) in self.inputs.iter().enumerate() {
            if input.ordinal != index as u64 {
                return Err(NetworkTraceValidationError::NonCanonicalOrdinal);
            }
            if input.channel != channel.id {
                return Err(NetworkTraceValidationError::UnknownChannel);
            }
            if input.release.not_before_global_time < epoch_start {
                return Err(NetworkTraceValidationError::ReleaseBeforeEpoch);
            }
            if input.release.not_before_global_time < previous_time
                || input.release.after_transmitted_offset < previous_tx_watermark
            {
                return Err(NetworkTraceValidationError::NonMonotonicRelease);
            }
            if input.release.after_transmitted_offset > total_output {
                return Err(NetworkTraceValidationError::UnreachableTransmitWatermark);
            }
            if terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal);
            }

            match &input.event {
                NetworkInputKindV1::InboundBytes {
                    stream_offset,
                    bytes,
                } => {
                    if bytes.is_empty() {
                        return Err(NetworkTraceValidationError::EmptyByteChunk);
                    }
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    inbound_offset = inbound_offset
                        .checked_add(bytes.len() as u64)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                }
                NetworkInputKindV1::PeerWriteClosed { stream_offset } => {
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    terminal = true;
                }
                NetworkInputKindV1::SocketError {
                    stream_offset,
                    errno,
                } => {
                    if *stream_offset != inbound_offset {
                        return Err(NetworkTraceValidationError::NonContiguousInput);
                    }
                    if !(1..=4095).contains(errno) {
                        return Err(NetworkTraceValidationError::InvalidErrno);
                    }
                    // EWOULDBLOCK equals EAGAIN on the supported Linux hosts.
                    // Neither readiness nor signal interruption terminates the
                    // peer's byte stream, so neither belongs in this variant.
                    if matches!(*errno, libc::EAGAIN | libc::EINTR) {
                        return Err(NetworkTraceValidationError::NonTerminalSocketError);
                    }
                    terminal = true;
                }
            }
            previous_time = input.release.not_before_global_time;
            previous_tx_watermark = input.release.after_transmitted_offset;
        }
        Ok(())
    }

    /// Write one complete, length-delimited v1 trace.
    pub fn write_framed<W: Write>(&self, writer: W) -> Result<(), NetworkTraceCodecError> {
        self.validate()?;
        write_payload(writer, NETWORK_TRACE_VERSION_V1, self)
    }

    /// Read exactly one complete trace, rejecting truncation, trailing bytes,
    /// unknown versions, malformed payloads, and invalid v1 semantics.
    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecError> {
        let (version, payload) = read_payload(reader)?;
        if version != NETWORK_TRACE_VERSION_V1 {
            return Err(NetworkTraceCodecError::UnsupportedVersion(version));
        }
        let trace: Self = decode_payload(&payload)?;
        trace.validate()?;
        Ok(trace)
    }
}

/// Trace-stable identity of a socket channel.
///
/// This is deliberately not an fd, [`OpenFileId`], thread id, or syscall
/// ordinal. The runtime binds one live open-file description to this identity;
/// dup and fork aliases therefore retain the binding while fd-number reuse
/// cannot inherit it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct NetworkChannelId(pub u64);

/// Trace-stable identity of an object transferred in ancillary data.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize
)]
pub struct NetworkObjectId(pub u64);

/// A host-layout-independent socket address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkAddressV2 {
    /// IPv4 address and host-order port.
    Inet4 { address: [u8; 4], port: u16 },
    /// IPv6 address and host-order port.
    Inet6 {
        address: [u8; 16],
        port: u16,
        flowinfo: u32,
        scope_id: u32,
    },
    /// Exact filesystem Unix-domain `sun_path` bytes. A trailing NUL, when
    /// supplied by the guest or returned by Linux, is preserved.
    UnixPath(Vec<u8>),
    /// Linux abstract Unix-domain address, without its leading NUL.
    UnixAbstract(Vec<u8>),
    /// An unnamed Unix-domain address.
    UnixUnnamed,
}

/// Socket transport represented by a V2 channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTransportV2 {
    /// Reliable byte stream.
    Tcp,
    /// Boundary-preserving datagrams.
    Udp,
    /// Unix-domain stream.
    UnixStream,
    /// Unix-domain datagrams.
    UnixDatagram,
}

impl NetworkTransportV2 {
    /// Whether this transport preserves datagram boundaries.
    pub fn is_datagram(self) -> bool {
        matches!(self, Self::Udp | Self::UnixDatagram)
    }
}

/// How a recorded channel came into existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkEndpointRoleV2 {
    /// Socket which initiated a connection.
    OutboundClient,
    /// Listening socket.
    Listener,
    /// Socket returned by accept.
    Accepted,
    /// Connectionless endpoint.
    Datagram,
}

/// Metadata for one stable network channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkChannelV2 {
    /// Trace-stable channel identity.
    pub id: NetworkChannelId,
    /// Transport semantics.
    pub transport: NetworkTransportV2,
    /// Creation role.
    pub role: NetworkEndpointRoleV2,
    /// Bound local address, if known.
    pub local_address: Option<NetworkAddressV2>,
    /// Connected peer address, if any.
    pub peer_address: Option<NetworkAddressV2>,
    /// Listener from which an accepted channel originated.
    pub accepted_from: Option<NetworkChannelId>,
}

/// Conditions which must both hold before an observation becomes available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReleaseV2 {
    /// Absolute continuous virtual time; never host wall time.
    pub not_before_global_time: LogicalTime,
    /// Validated bytes transmitted on this channel before release.
    pub after_transmitted_offset: u64,
}

impl NetworkReleaseV2 {
    /// Test both release conditions without rounding or changing the clock.
    pub fn is_eligible(self, now: LogicalTime, transmitted: u64) -> bool {
        now >= self.not_before_global_time && transmitted >= self.after_transmitted_offset
    }
}

/// Readiness bits independent of a host `pollfd` layout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkReadinessV2 {
    /// A receive can make progress without blocking.
    pub readable: bool,
    /// A transmit can make progress without blocking.
    pub writable: bool,
    /// A pending socket error is observable.
    pub error: bool,
    /// The peer closed or shut down its write side.
    pub hangup: bool,
}

impl NetworkReadinessV2 {
    /// Whether no readiness condition is present.
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

/// Direction for `shutdown(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkShutdownV2 {
    /// Stop receiving.
    Read,
    /// Stop transmitting.
    Write,
    /// Stop both directions.
    Both,
}

/// Typed metadata for kernel objects embedded in ancillary bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkAncillaryObjectV2 {
    /// One descriptor carried by `SCM_RIGHTS`.
    FileDescriptor { object: NetworkObjectId },
    /// Credentials carried by `SCM_CREDENTIALS`.
    Credentials { pid: i32, uid: u32, gid: u32 },
}

/// Exact control bytes plus typed object relocation metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkAncillaryDataV2 {
    /// Exact logical control-message bytes before fd-number relocation.
    pub bytes: Vec<u8>,
    /// Objects and byte offsets of their native-endian integer slots.
    pub objects: Vec<NetworkAncillaryObjectRefV2>,
    /// Linux `MSG_CTRUNC` observation.
    pub truncated: bool,
}

/// Location and identity of one ancillary object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkAncillaryObjectRefV2 {
    /// Byte offset within [`NetworkAncillaryDataV2::bytes`].
    pub byte_offset: u32,
    /// Object represented at that offset.
    pub object: NetworkAncillaryObjectV2,
}

/// Boundary-preserving datagram payload and addressing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDatagramV2 {
    /// Per-channel datagram sequence, independent of consuming thread.
    pub sequence: u64,
    /// Entire datagram payload before a receiving buffer truncates it.
    pub bytes: Vec<u8>,
    /// Sender address returned to the receiver.
    pub source: Option<NetworkAddressV2>,
    /// Destination address supplied by the sender, when explicit.
    pub destination: Option<NetworkAddressV2>,
    /// Ancillary bytes and object metadata.
    pub ancillary: Option<NetworkAncillaryDataV2>,
    /// Linux message flags observed on receive.
    pub message_flags: i32,
}

/// Datagram metadata which preserves the kernel-reported socket-address
/// lengths in addition to the host-layout-independent decoded addresses.
///
/// This is a separate envelope rather than new fields on [`NetworkDatagramV2`]
/// so existing version-two frames continue to decode byte-for-byte. New
/// recordings use this representation whenever an address was present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkDatagramExactV2 {
    /// Boundary-preserving payload and decoded addresses.
    pub datagram: NetworkDatagramV2,
    /// Original `msg_namelen`/`addrlen` for the source address.
    pub source_length: Option<u32>,
    /// Original address length supplied for an explicit destination.
    pub destination_length: Option<u32>,
}

/// Result of connect or accept, including asynchronous failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkConnectionResultV2 {
    /// Operation completed successfully.
    Connected,
    /// Operation failed with this positive Linux errno.
    Error(i32),
}

/// One inbound external observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkInputKindV2 {
    /// Completion of a connect attempt.
    Connect(NetworkConnectionResultV2),
    /// One accepted connection became available on a listener.
    Accept {
        accepted: NetworkChannelId,
        peer: Option<NetworkAddressV2>,
        ancillary: Option<NetworkAncillaryDataV2>,
    },
    /// Bytes appended to a stream at an exact stream offset.
    StreamBytes { stream_offset: u64, bytes: Vec<u8> },
    /// One complete datagram.
    Datagram(NetworkDatagramV2),
    /// Peer half-close at an exact stream offset.
    PeerShutdown {
        stream_offset: u64,
        direction: NetworkShutdownV2,
    },
    /// Socket operation failed. Retryable errors are represented explicitly
    /// rather than being mistaken for stream termination.
    SocketError { stream_offset: u64, errno: i32 },
    /// Host readiness transition which cannot be derived from buffered data.
    Readiness(NetworkReadinessV2),
    /// One stream message with ancillary data tied to its first unread byte.
    /// Plain `read(2)` adaptation must not silently materialize these objects.
    StreamMessage {
        stream_offset: u64,
        bytes: Vec<u8>,
        ancillary: NetworkAncillaryDataV2,
        message_flags: i32,
    },
    /// One complete datagram with exact socket-address lengths.
    DatagramExact(NetworkDatagramExactV2),
}

/// One globally ordered observation with schedule-independent release gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkInputEventV2 {
    /// Canonical trace order, used only between external observations.
    pub ordinal: u64,
    /// Channel affected by this observation.
    pub channel: NetworkChannelId,
    /// Continuous-time and outbound-progress gate.
    pub release: NetworkReleaseV2,
    /// Observation payload.
    pub event: NetworkInputKindV2,
}

/// Expected guest output. Stream fragments may be split or coalesced by replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkOutputKindV2 {
    /// Contiguous expected stream bytes.
    StreamBytes { stream_offset: u64, bytes: Vec<u8> },
    /// One exact datagram boundary.
    Datagram(NetworkDatagramV2),
    /// Local shutdown transition.
    Shutdown {
        stream_offset: u64,
        direction: NetworkShutdownV2,
    },
    /// A transmit attempt failed at this offset.
    SocketError { stream_offset: u64, errno: i32 },
    /// Stream bytes emitted with one exact ancillary control message.
    StreamMessage {
        stream_offset: u64,
        bytes: Vec<u8>,
        ancillary: NetworkAncillaryDataV2,
        message_flags: i32,
    },
    /// One exact datagram boundary with exact socket-address lengths.
    DatagramExact(NetworkDatagramExactV2),
}

/// One expected outbound observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkOutputEventV2 {
    /// Channel which emitted the output.
    pub channel: NetworkChannelId,
    /// Expected output.
    pub event: NetworkOutputKindV2,
}

/// Multi-channel schedule-independent network trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkTraceV2 {
    /// Trace epoch in the same absolute domain as `GlobalTime`.
    pub epoch: DateTime<Utc>,
    /// Stable channel definitions.
    pub channels: Vec<NetworkChannelV2>,
    /// External observations in canonical arrival order.
    pub inputs: Vec<NetworkInputEventV2>,
    /// Expected output, grouped into per-channel progress during validation.
    pub outputs: Vec<NetworkOutputEventV2>,
}

/// A decoded trace with an explicit on-disk version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTrace {
    /// Legacy single-client trace.
    V1(NetworkTraceV1),
    /// Current multi-channel trace.
    V2(NetworkTraceV2),
}

impl NetworkTraceV2 {
    /// Convert the epoch to Hermit's absolute logical-time domain.
    pub fn epoch_global_time(&self) -> Result<LogicalTime, NetworkTraceValidationError> {
        epoch_global_time(self.epoch)
    }

    /// Validate all identities, stream offsets, datagram boundaries, ancillary
    /// relocations, release gates, and terminal states before runtime use.
    pub fn validate(&self) -> Result<(), NetworkTraceValidationError> {
        let epoch = self.epoch_global_time()?;
        let mut channels = BTreeMap::new();
        for channel in &self.channels {
            if channels.insert(channel.id, channel).is_some() {
                return Err(NetworkTraceValidationError::DuplicateChannel);
            }
            validate_channel(channel)?;
        }

        for channel in &self.channels {
            if let Some(listener) = channel.accepted_from {
                let Some(parent) = channels.get(&listener) else {
                    return Err(NetworkTraceValidationError::UnknownChannel);
                };
                if parent.role != NetworkEndpointRoleV2::Listener
                    || channel.role != NetworkEndpointRoleV2::Accepted
                {
                    return Err(NetworkTraceValidationError::InvalidChannelRelationship);
                }
            }
        }

        #[derive(Default)]
        struct Progress {
            input_offset: u64,
            output_offset: u64,
            input_datagram: u64,
            output_datagram: u64,
            input_terminal: bool,
            output_terminal: bool,
            last_release_time: Option<LogicalTime>,
            last_release_output: u64,
            connect_seen: bool,
        }
        let mut progress: BTreeMap<_, Progress> = channels
            .keys()
            .copied()
            .map(|id| (id, Progress::default()))
            .collect();
        let mut accepted_channels = BTreeSet::new();

        for output in &self.outputs {
            let Some(channel) = channels.get(&output.channel) else {
                return Err(NetworkTraceValidationError::UnknownChannel);
            };
            let state = progress.get_mut(&output.channel).unwrap();
            if state.output_terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal);
            }
            match &output.event {
                NetworkOutputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                } => {
                    require_stream(channel)?;
                    require_nonempty(bytes)?;
                    check_offset(*stream_offset, state.output_offset, true)?;
                    state.output_offset = checked_advance(state.output_offset, bytes.len())?;
                }
                NetworkOutputKindV2::StreamMessage {
                    stream_offset,
                    bytes,
                    ancillary,
                    ..
                } => {
                    require_stream(channel)?;
                    require_nonempty(bytes)?;
                    check_offset(*stream_offset, state.output_offset, true)?;
                    validate_ancillary(ancillary)?;
                    state.output_offset = checked_advance(state.output_offset, bytes.len())?;
                }
                NetworkOutputKindV2::Datagram(datagram) => {
                    require_datagram(channel)?;
                    validate_datagram(datagram, state.output_datagram)?;
                    state.output_datagram = state
                        .output_datagram
                        .checked_add(1)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                    state.output_offset =
                        checked_advance(state.output_offset, datagram.bytes.len())?;
                }
                NetworkOutputKindV2::DatagramExact(exact) => {
                    require_datagram(channel)?;
                    validate_exact_datagram(exact, state.output_datagram)?;
                    state.output_datagram = state
                        .output_datagram
                        .checked_add(1)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                    state.output_offset =
                        checked_advance(state.output_offset, exact.datagram.bytes.len())?;
                }
                NetworkOutputKindV2::Shutdown {
                    stream_offset,
                    direction,
                } => {
                    require_stream(channel)?;
                    check_offset(*stream_offset, state.output_offset, true)?;
                    if matches!(
                        direction,
                        NetworkShutdownV2::Write | NetworkShutdownV2::Both
                    ) {
                        state.output_terminal = true;
                    }
                }
                NetworkOutputKindV2::SocketError {
                    stream_offset,
                    errno,
                } => {
                    check_offset(*stream_offset, state.output_offset, true)?;
                    validate_errno(*errno)?;
                }
            }
        }

        for (index, input) in self.inputs.iter().enumerate() {
            if input.ordinal != index as u64 {
                return Err(NetworkTraceValidationError::NonCanonicalOrdinal);
            }
            let Some(channel) = channels.get(&input.channel) else {
                return Err(NetworkTraceValidationError::UnknownChannel);
            };
            let state = progress.get_mut(&input.channel).unwrap();
            if input.release.not_before_global_time < epoch {
                return Err(NetworkTraceValidationError::ReleaseBeforeEpoch);
            }
            if state
                .last_release_time
                .is_some_and(|time| input.release.not_before_global_time < time)
                || input.release.after_transmitted_offset < state.last_release_output
            {
                return Err(NetworkTraceValidationError::NonMonotonicRelease);
            }
            if input.release.after_transmitted_offset > state.output_offset {
                return Err(NetworkTraceValidationError::UnreachableTransmitWatermark);
            }
            state.last_release_time = Some(input.release.not_before_global_time);
            state.last_release_output = input.release.after_transmitted_offset;
            if state.input_terminal && !matches!(input.event, NetworkInputKindV2::Readiness(_)) {
                return Err(NetworkTraceValidationError::EventAfterTerminal);
            }
            match &input.event {
                NetworkInputKindV2::Connect(result) => {
                    if channel.role != NetworkEndpointRoleV2::OutboundClient {
                        return Err(NetworkTraceValidationError::InvalidChannelRelationship);
                    }
                    if state.connect_seen {
                        return Err(NetworkTraceValidationError::DuplicateConnect);
                    }
                    state.connect_seen = true;
                    if let NetworkConnectionResultV2::Error(errno) = result {
                        validate_errno(*errno)?;
                    }
                }
                NetworkInputKindV2::Accept {
                    accepted,
                    ancillary,
                    ..
                } => {
                    if channel.role != NetworkEndpointRoleV2::Listener
                        || channels.get(accepted).is_none_or(|accepted_channel| {
                            accepted_channel.accepted_from != Some(input.channel)
                        })
                    {
                        return Err(NetworkTraceValidationError::InvalidChannelRelationship);
                    }
                    if !accepted_channels.insert(*accepted) {
                        return Err(NetworkTraceValidationError::DuplicateAcceptedChannel);
                    }
                    if let Some(ancillary) = ancillary {
                        validate_ancillary(ancillary)?;
                    }
                }
                NetworkInputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                } => {
                    require_stream(channel)?;
                    require_nonempty(bytes)?;
                    check_offset(*stream_offset, state.input_offset, false)?;
                    state.input_offset = checked_advance(state.input_offset, bytes.len())?;
                }
                NetworkInputKindV2::StreamMessage {
                    stream_offset,
                    bytes,
                    ancillary,
                    ..
                } => {
                    require_stream(channel)?;
                    require_nonempty(bytes)?;
                    check_offset(*stream_offset, state.input_offset, false)?;
                    validate_ancillary(ancillary)?;
                    state.input_offset = checked_advance(state.input_offset, bytes.len())?;
                }
                NetworkInputKindV2::Datagram(datagram) => {
                    require_datagram(channel)?;
                    validate_datagram(datagram, state.input_datagram)?;
                    state.input_datagram = state
                        .input_datagram
                        .checked_add(1)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                }
                NetworkInputKindV2::DatagramExact(exact) => {
                    require_datagram(channel)?;
                    validate_exact_datagram(exact, state.input_datagram)?;
                    state.input_datagram = state
                        .input_datagram
                        .checked_add(1)
                        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
                }
                NetworkInputKindV2::PeerShutdown {
                    stream_offset,
                    direction,
                } => {
                    require_stream(channel)?;
                    check_offset(*stream_offset, state.input_offset, false)?;
                    if matches!(
                        direction,
                        NetworkShutdownV2::Write | NetworkShutdownV2::Both
                    ) {
                        state.input_terminal = true;
                    }
                }
                NetworkInputKindV2::SocketError {
                    stream_offset,
                    errno,
                } => {
                    check_offset(*stream_offset, state.input_offset, false)?;
                    validate_errno(*errno)?;
                }
                // An empty state is a meaningful readiness-clear transition.
                NetworkInputKindV2::Readiness(_) => {}
            }
        }

        Ok(())
    }

    /// Write the current V2 frame.
    pub fn write_framed<W: Write>(&self, writer: W) -> Result<(), NetworkTraceCodecError> {
        self.validate()?;
        write_payload(writer, NETWORK_TRACE_VERSION_V2, self)
    }

    /// Read a V2 frame, refusing V1 rather than silently migrating semantics.
    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecError> {
        let (version, payload) = read_payload(reader)?;
        if version != NETWORK_TRACE_VERSION_V2 {
            return Err(NetworkTraceCodecError::UnsupportedVersion(version));
        }
        let trace: Self = decode_payload(&payload)?;
        trace.validate()?;
        Ok(trace)
    }
}

impl NetworkTrace {
    /// Decode either explicitly supported version.
    pub fn read_framed<R: Read>(reader: R) -> Result<Self, NetworkTraceCodecError> {
        let (version, payload) = read_payload(reader)?;
        match version {
            NETWORK_TRACE_VERSION_V1 => {
                let trace: NetworkTraceV1 = decode_payload(&payload)?;
                trace.validate()?;
                Ok(Self::V1(trace))
            }
            NETWORK_TRACE_VERSION_V2 => {
                let trace: NetworkTraceV2 = decode_payload(&payload)?;
                trace.validate()?;
                Ok(Self::V2(trace))
            }
            version => Err(NetworkTraceCodecError::UnsupportedVersion(version)),
        }
    }

    /// Write the variant using its explicit version number.
    pub fn write_framed<W: Write>(&self, writer: W) -> Result<(), NetworkTraceCodecError> {
        match self {
            Self::V1(trace) => trace.write_framed(writer),
            Self::V2(trace) => trace.write_framed(writer),
        }
    }
}

fn epoch_global_time(epoch: DateTime<Utc>) -> Result<LogicalTime, NetworkTraceValidationError> {
    let seconds = u64::try_from(epoch.timestamp())
        .map_err(|_| NetworkTraceValidationError::EpochOutOfRange)?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|whole| whole.checked_add(u64::from(epoch.timestamp_subsec_micros()) * 1_000))
        .map(LogicalTime::from_nanos)
        .ok_or(NetworkTraceValidationError::EpochOutOfRange)
}

fn validate_channel(channel: &NetworkChannelV2) -> Result<(), NetworkTraceValidationError> {
    match channel.role {
        NetworkEndpointRoleV2::OutboundClient => {
            if channel.peer_address.is_none() || channel.accepted_from.is_some() {
                return Err(NetworkTraceValidationError::InvalidChannelRelationship);
            }
        }
        NetworkEndpointRoleV2::Listener => {
            if channel.peer_address.is_some() || channel.accepted_from.is_some() {
                return Err(NetworkTraceValidationError::InvalidChannelRelationship);
            }
        }
        NetworkEndpointRoleV2::Accepted => {
            if channel.accepted_from.is_none() {
                return Err(NetworkTraceValidationError::InvalidChannelRelationship);
            }
        }
        NetworkEndpointRoleV2::Datagram => {
            if !channel.transport.is_datagram() || channel.accepted_from.is_some() {
                return Err(NetworkTraceValidationError::InvalidChannelRelationship);
            }
        }
    }
    if channel.transport.is_datagram() != matches!(channel.role, NetworkEndpointRoleV2::Datagram) {
        return Err(NetworkTraceValidationError::TransportRoleMismatch);
    }
    Ok(())
}

fn require_stream(channel: &NetworkChannelV2) -> Result<(), NetworkTraceValidationError> {
    if channel.transport.is_datagram() {
        Err(NetworkTraceValidationError::TransportEventMismatch)
    } else {
        Ok(())
    }
}

fn require_datagram(channel: &NetworkChannelV2) -> Result<(), NetworkTraceValidationError> {
    if channel.transport.is_datagram() {
        Ok(())
    } else {
        Err(NetworkTraceValidationError::TransportEventMismatch)
    }
}

fn require_nonempty(bytes: &[u8]) -> Result<(), NetworkTraceValidationError> {
    if bytes.is_empty() {
        Err(NetworkTraceValidationError::EmptyByteChunk)
    } else {
        Ok(())
    }
}

fn check_offset(
    actual: u64,
    expected: u64,
    output: bool,
) -> Result<(), NetworkTraceValidationError> {
    if actual == expected {
        Ok(())
    } else if output {
        Err(NetworkTraceValidationError::NonContiguousOutput)
    } else {
        Err(NetworkTraceValidationError::NonContiguousInput)
    }
}

fn checked_advance(offset: u64, count: usize) -> Result<u64, NetworkTraceValidationError> {
    offset
        .checked_add(count as u64)
        .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)
}

fn validate_errno(errno: i32) -> Result<(), NetworkTraceValidationError> {
    if (1..=4095).contains(&errno) {
        Ok(())
    } else {
        Err(NetworkTraceValidationError::InvalidErrno)
    }
}

fn validate_datagram(
    datagram: &NetworkDatagramV2,
    expected_sequence: u64,
) -> Result<(), NetworkTraceValidationError> {
    if datagram.sequence != expected_sequence {
        return Err(NetworkTraceValidationError::NonCanonicalDatagramSequence);
    }
    if let Some(ancillary) = &datagram.ancillary {
        validate_ancillary(ancillary)?;
    }
    Ok(())
}

fn validate_exact_datagram(
    exact: &NetworkDatagramExactV2,
    expected_sequence: u64,
) -> Result<(), NetworkTraceValidationError> {
    validate_datagram(&exact.datagram, expected_sequence)?;
    if exact.source_length.is_some() != exact.datagram.source.is_some()
        || exact.destination_length.is_some() != exact.datagram.destination.is_some()
    {
        return Err(NetworkTraceValidationError::AddressLengthMismatch);
    }
    Ok(())
}

fn validate_ancillary(
    ancillary: &NetworkAncillaryDataV2,
) -> Result<(), NetworkTraceValidationError> {
    let mut occupied = BTreeSet::new();
    for object in &ancillary.objects {
        let offset = object.byte_offset as usize;
        let object_len = match object.object {
            NetworkAncillaryObjectV2::FileDescriptor { .. } => std::mem::size_of::<i32>(),
            NetworkAncillaryObjectV2::Credentials { .. } => 3 * std::mem::size_of::<i32>(),
        };
        let Some(end) = offset.checked_add(object_len) else {
            return Err(NetworkTraceValidationError::InvalidAncillaryObjectOffset);
        };
        if end > ancillary.bytes.len() || (offset..end).any(|byte| !occupied.insert(byte)) {
            return Err(NetworkTraceValidationError::InvalidAncillaryObjectOffset);
        }
    }
    Ok(())
}

fn write_payload<W: Write, T: Serialize>(
    mut writer: W,
    version: u32,
    trace: &T,
) -> Result<(), NetworkTraceCodecError> {
    let payload = bincode::serde::encode_to_vec(trace, bincode::config::standard())
        .map_err(NetworkTraceCodecError::Encode)?;
    let payload_len = u64::try_from(payload.len()).map_err(|_| NetworkTraceCodecError::TooLarge)?;
    if payload_len > MAX_NETWORK_TRACE_PAYLOAD_BYTES {
        return Err(NetworkTraceCodecError::TooLarge);
    }
    writer.write_all(&NETWORK_TRACE_MAGIC)?;
    writer.write_all(&version.to_le_bytes())?;
    writer.write_all(&payload_len.to_le_bytes())?;
    writer.write_all(&payload)?;
    Ok(())
}

fn read_payload<R: Read>(mut reader: R) -> Result<(u32, Vec<u8>), NetworkTraceCodecError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    read_exact_or_truncated(&mut reader, &mut header)?;
    if header[..NETWORK_TRACE_MAGIC.len()] != NETWORK_TRACE_MAGIC {
        return Err(NetworkTraceCodecError::BadMagic);
    }
    let version_start = NETWORK_TRACE_MAGIC.len();
    let version = u32::from_le_bytes(header[version_start..version_start + 4].try_into().unwrap());
    let len_start = version_start + 4;
    let payload_len = u64::from_le_bytes(header[len_start..len_start + 8].try_into().unwrap());
    if payload_len > MAX_NETWORK_TRACE_PAYLOAD_BYTES {
        return Err(NetworkTraceCodecError::TooLarge);
    }
    let payload_len = usize::try_from(payload_len).map_err(|_| NetworkTraceCodecError::TooLarge)?;
    let mut payload = vec![0; payload_len];
    read_exact_or_truncated(&mut reader, &mut payload)?;
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(NetworkTraceCodecError::TrailingData);
    }
    Ok((version, payload))
}

fn decode_payload<T: for<'de> Deserialize<'de>>(
    payload: &[u8],
) -> Result<T, NetworkTraceCodecError> {
    let (trace, consumed) = bincode::serde::decode_from_slice(payload, bincode::config::standard())
        .map_err(NetworkTraceCodecError::Decode)?;
    if consumed != payload.len() {
        return Err(NetworkTraceCodecError::TrailingPayloadData);
    }
    Ok(trace)
}

fn validate_outputs(
    outputs: &[NetworkOutputV1],
    channel: OpenFileId,
) -> Result<u64, NetworkTraceValidationError> {
    let mut expected_offset = 0u64;
    for output in outputs {
        if output.channel != channel {
            return Err(NetworkTraceValidationError::UnknownChannel);
        }
        if output.bytes.is_empty() {
            return Err(NetworkTraceValidationError::EmptyByteChunk);
        }
        if output.stream_offset != expected_offset {
            return Err(NetworkTraceValidationError::NonContiguousOutput);
        }
        expected_offset = expected_offset
            .checked_add(output.bytes.len() as u64)
            .ok_or(NetworkTraceValidationError::StreamOffsetOverflow)?;
    }
    Ok(expected_offset)
}

fn read_exact_or_truncated<R: Read>(
    reader: &mut R,
    bytes: &mut [u8],
) -> Result<(), NetworkTraceCodecError> {
    reader.read_exact(bytes).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            NetworkTraceCodecError::Truncated
        } else {
            NetworkTraceCodecError::Io(error)
        }
    })
}

/// Semantically invalid v1 trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTraceValidationError {
    ChannelCount(usize),
    DuplicateChannel,
    EpochOutOfRange,
    NonSocketChannelIdentity,
    ScheduleDependentChannelIdentity,
    AddressFamilyMismatch,
    UnsupportedPeerAddress,
    UnknownChannel,
    NonCanonicalOrdinal,
    ReleaseBeforeEpoch,
    NonMonotonicRelease,
    UnreachableTransmitWatermark,
    EmptyByteChunk,
    NonContiguousInput,
    NonContiguousOutput,
    StreamOffsetOverflow,
    InvalidErrno,
    NonTerminalSocketError,
    EventAfterTerminal,
    InvalidChannelRelationship,
    TransportRoleMismatch,
    TransportEventMismatch,
    NonCanonicalDatagramSequence,
    InvalidAncillaryObjectOffset,
    DuplicateConnect,
    DuplicateAcceptedChannel,
    AddressLengthMismatch,
    /// Retained so callers matching errors from early V2 implementations keep
    /// compiling. Empty readiness is now a valid clear transition.
    EmptyReadiness,
}

impl fmt::Display for NetworkTraceValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid network trace: {self:?}")
    }
}

impl Error for NetworkTraceValidationError {}

/// Failure to decode or encode the framed v1 representation.
#[derive(Debug)]
pub enum NetworkTraceCodecError {
    Io(io::Error),
    Truncated,
    BadMagic,
    UnsupportedVersion(u32),
    TooLarge,
    TrailingData,
    TrailingPayloadData,
    Encode(bincode::error::EncodeError),
    Decode(bincode::error::DecodeError),
    Validation(NetworkTraceValidationError),
}

impl fmt::Display for NetworkTraceCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "network trace codec error: {self:?}")
    }
}

impl Error for NetworkTraceCodecError {}

impl From<io::Error> for NetworkTraceCodecError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<NetworkTraceValidationError> for NetworkTraceCodecError {
    fn from(error: NetworkTraceValidationError) -> Self {
        Self::Validation(error)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;
    use crate::config::Config;
    use crate::pid::DetTid;
    use crate::time::GlobalTime;

    fn channel_id() -> OpenFileId {
        OpenFileId::new_socket(DetTid::from_raw(1), 0)
    }

    fn trace_epoch() -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap()
    }

    fn global_time_after_epoch(nanos: u64) -> LogicalTime {
        let config = Config {
            epoch: trace_epoch(),
            ..Config::default()
        };
        GlobalTime::new(&config).as_nanos() + LogicalTime::from_nanos(nanos)
    }

    fn valid_trace() -> NetworkTraceV1 {
        let channel = channel_id();
        NetworkTraceV1 {
            epoch: trace_epoch(),
            channels: vec![NetworkChannelV1 {
                id: channel,
                transport: NetworkTransportV1::Tcp,
                role: NetworkEndpointRoleV1::OutboundClient,
                local_address: NetworkAddressV1::Inet4 {
                    address: [10, 0, 0, 2],
                    port: 40_000,
                },
                peer_address: NetworkAddressV1::Inet4 {
                    address: [192, 0, 2, 10],
                    port: 443,
                },
                created_before_competing_threads: true,
            }],
            inputs: vec![
                NetworkInputEventV1 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV1 {
                        not_before_global_time: global_time_after_epoch(10),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV1::InboundBytes {
                        stream_offset: 0,
                        bytes: b"response".to_vec(),
                    },
                },
                NetworkInputEventV1 {
                    ordinal: 1,
                    channel,
                    release: NetworkReleaseV1 {
                        not_before_global_time: global_time_after_epoch(20),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV1::PeerWriteClosed { stream_offset: 8 },
                },
            ],
            outputs: vec![NetworkOutputV1 {
                channel,
                stream_offset: 0,
                bytes: b"req".to_vec(),
            }],
        }
    }

    fn framed(trace: &NetworkTraceV1) -> Vec<u8> {
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        bytes
    }

    fn framed_without_validation(trace: &NetworkTraceV1) -> Vec<u8> {
        let payload = bincode::serde::encode_to_vec(trace, bincode::config::standard()).unwrap();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NETWORK_TRACE_MAGIC);
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        bytes
    }

    #[test]
    fn network_trace_v1_round_trips_exactly() {
        let trace = valid_trace();
        let decoded = NetworkTraceV1::read_framed(Cursor::new(framed(&trace))).unwrap();
        assert_eq!(decoded, trace);
    }

    #[test]
    fn transient_socket_errors_are_rejected_by_validation_writer_and_reader() {
        assert_eq!(libc::EWOULDBLOCK, libc::EAGAIN);
        for errno in [libc::EAGAIN, libc::EINTR] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::NonTerminalSocketError),
                "transient errno {errno} cannot terminate the stream"
            );

            let mut output = b"existing output".to_vec();
            assert!(matches!(
                trace.write_framed(&mut output),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::NonTerminalSocketError
                ))
            ));
            assert_eq!(output, b"existing output");
            // Bypass the writer to prove untrusted serialized input is refused
            // independently, rather than relying on the writer's validation.
            assert!(matches!(
                NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::NonTerminalSocketError
                ))
            ));
        }
    }

    #[test]
    fn terminal_socket_errors_preserve_framing_and_the_stream_boundary() {
        for errno in [libc::ECONNRESET, libc::ETIMEDOUT] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(trace.validate(), Ok(()));
            let bytes = framed(&trace);
            assert_eq!(bytes, framed_without_validation(&trace));
            assert_eq!(
                NetworkTraceV1::read_framed(Cursor::new(bytes)).unwrap(),
                trace
            );

            let mut late = trace.inputs[0].clone();
            late.ordinal = 2;
            late.release = trace.inputs[1].release;
            late.event = NetworkInputKindV1::InboundBytes {
                stream_offset: 8,
                bytes: b"late".to_vec(),
            };
            trace.inputs.push(late);
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::EventAfterTerminal)
            );
        }
    }

    #[test]
    fn socket_errno_range_is_still_checked_before_framing() {
        for errno in [-1, 0, 4096] {
            let mut trace = valid_trace();
            trace.inputs[1].event = NetworkInputKindV1::SocketError {
                stream_offset: 8,
                errno,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::InvalidErrno)
            );
            assert!(matches!(
                trace.write_framed(Vec::new()),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::InvalidErrno
                ))
            ));
            assert!(matches!(
                NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
                Err(NetworkTraceCodecError::Validation(
                    NetworkTraceValidationError::InvalidErrno
                ))
            ));
        }
    }

    #[test]
    fn every_strict_prefix_is_reported_as_truncated() {
        let bytes = framed(&valid_trace());
        for end in 0..bytes.len() {
            assert!(
                matches!(
                    NetworkTraceV1::read_framed(Cursor::new(&bytes[..end])),
                    Err(NetworkTraceCodecError::Truncated)
                ),
                "prefix of length {end} was not rejected as truncation"
            );
        }
    }

    #[test]
    fn unknown_versions_are_rejected_before_payload_decode() {
        let mut bytes = framed(&valid_trace());
        let start = NETWORK_TRACE_MAGIC.len();
        bytes[start..start + 4].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn oversized_length_is_rejected_before_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&NETWORK_TRACE_MAGIC);
        bytes.extend_from_slice(&NETWORK_TRACE_VERSION_V1.to_le_bytes());
        bytes.extend_from_slice(&(MAX_NETWORK_TRACE_PAYLOAD_BYTES + 1).to_le_bytes());
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::TooLarge)
        ));
    }

    #[test]
    fn trailing_file_data_is_rejected() {
        let mut bytes = framed(&valid_trace());
        bytes.push(0);
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(bytes)),
            Err(NetworkTraceCodecError::TrailingData)
        ));
    }

    #[test]
    fn reader_and_writer_both_refuse_semantically_invalid_traces() {
        let mut trace = valid_trace();
        trace.inputs[0].ordinal = 7;

        assert!(matches!(
            trace.write_framed(Vec::new()),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::NonCanonicalOrdinal
            ))
        ));
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::NonCanonicalOrdinal
            ))
        ));
    }

    #[test]
    fn deserialized_pre_unix_epoch_is_a_typed_validation_error() {
        let mut trace = valid_trace();
        trace.epoch = Utc.timestamp_opt(-1, 0).unwrap();
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::EpochOutOfRange
            ))
        ));
    }

    #[test]
    fn deserialized_epoch_beyond_global_time_is_a_typed_validation_error() {
        let mut trace = valid_trace();
        trace.epoch = Utc.with_ymd_and_hms(9999, 1, 1, 0, 0, 0).unwrap();
        assert!(matches!(
            NetworkTraceV1::read_framed(Cursor::new(framed_without_validation(&trace))),
            Err(NetworkTraceCodecError::Validation(
                NetworkTraceValidationError::EpochOutOfRange
            ))
        ));
    }

    #[test]
    fn config_is_off_by_default_and_perturbation_has_no_seed_fallback() {
        let config = NetworkTraceConfig::default();
        assert_eq!(config.policy, NetworkPolicy::Deny);
        assert_eq!(config.network_perturb_seed, None);
        assert_eq!(config.validate(), Ok(()));

        let replay = NetworkTraceConfig::replay("trace.net", Some(17));
        assert_eq!(replay.network_perturb_seed, Some(17));
        assert_eq!(replay.validate(), Ok(()));

        let encoded = serde_json::to_string(&replay).unwrap();
        assert_eq!(
            serde_json::from_str::<NetworkTraceConfig>(&encoded).unwrap(),
            replay
        );
        assert_eq!(
            serde_json::from_str::<NetworkTraceConfig>(
                r#"{"mode":"off","path":null,"network_perturb_seed":null}"#
            )
            .unwrap(),
            NetworkTraceConfig::deny()
        );
        assert!(
            serde_json::from_str::<NetworkTraceConfig>(
                r#"{"mode":"off","path":null,"network_perturb_seed":null,"unexpected":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn config_rejects_ambiguous_or_record_perturbation_combinations() {
        assert_eq!(
            NetworkTraceConfig {
                path: Some("trace.net".into()),
                ..NetworkTraceConfig::default()
            }
            .validate(),
            Err(NetworkTraceConfigError::OptionsWithoutTrace)
        );
        assert_eq!(
            NetworkTraceConfig {
                policy: NetworkPolicy::Replay,
                ..NetworkTraceConfig::default()
            }
            .validate(),
            Err(NetworkTraceConfigError::MissingPath)
        );
        assert_eq!(
            NetworkTraceConfig {
                policy: NetworkPolicy::Record,
                path: Some("trace.net".into()),
                network_perturb_seed: Some(1),
            }
            .validate(),
            Err(NetworkTraceConfigError::PerturbationDuringRecord)
        );
    }

    #[test]
    fn release_eligibility_uses_the_absolute_global_time_domain() {
        let mut config = Config {
            epoch: trace_epoch(),
            ..Config::default()
        };
        // A different scheduler seed must not affect the clock-domain conversion.
        config.sched_seed = Some(999);
        let mut global_time = GlobalTime::new(&config);
        let start = global_time.as_nanos();
        assert_eq!(valid_trace().epoch_global_time(), Ok(start));
        let release = NetworkReleaseV1 {
            not_before_global_time: start + LogicalTime::from_nanos(10),
            after_transmitted_offset: 3,
        };

        assert!(!release.is_eligible(global_time.as_nanos(), 3));
        global_time.add_extra_time(Duration::from_nanos(10));
        assert_eq!(global_time.as_nanos(), release.not_before_global_time);
        assert!(!release.is_eligible(global_time.as_nanos(), 2));
        assert!(release.is_eligible(global_time.as_nanos(), 3));
    }

    #[test]
    fn v1_validation_rejects_channels_outside_the_supported_envelope() {
        let mut trace = valid_trace();
        trace.channels.push(trace.channels[0].clone());
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ChannelCount(2))
        );

        let mut trace = valid_trace();
        let non_socket = OpenFileId::new(DetTid::from_raw(1), 0);
        trace.channels[0].id = non_socket;
        for input in &mut trace.inputs {
            input.channel = non_socket;
        }
        for output in &mut trace.outputs {
            output.channel = non_socket;
        }
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonSocketChannelIdentity)
        );

        let mut trace = valid_trace();
        trace.channels[0].peer_address = NetworkAddressV1::Inet4 {
            address: [127, 0, 0, 1],
            port: 443,
        };
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::UnsupportedPeerAddress)
        );

        for mapped in [
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(224, 0, 0, 1),
        ] {
            let mut trace = valid_trace();
            trace.channels[0].local_address = NetworkAddressV1::Inet6 {
                address: Ipv6Addr::LOCALHOST.octets(),
                port: 40_000,
                flowinfo: 0,
                scope_id: 0,
            };
            trace.channels[0].peer_address = NetworkAddressV1::Inet6 {
                address: mapped.to_ipv6_mapped().octets(),
                port: 443,
                flowinfo: 0,
                scope_id: 0,
            };
            assert_eq!(
                trace.validate(),
                Err(NetworkTraceValidationError::UnsupportedPeerAddress),
                "IPv4-mapped {mapped} escaped the IPv4 policy"
            );
        }

        let mut trace = valid_trace();
        trace.channels[0].created_before_competing_threads = false;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ScheduleDependentChannelIdentity)
        );
    }

    #[test]
    fn v1_validation_rejects_noncanonical_streams_and_release_conditions() {
        let mut trace = valid_trace();
        trace.inputs[0].release.not_before_global_time = LogicalTime::ZERO;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::ReleaseBeforeEpoch)
        );

        let mut trace = valid_trace();
        trace.inputs[0].ordinal = 1;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonCanonicalOrdinal)
        );

        let mut trace = valid_trace();
        trace.inputs[0].release.after_transmitted_offset = 4;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::UnreachableTransmitWatermark)
        );

        let mut trace = valid_trace();
        trace.outputs[0].stream_offset = 1;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonContiguousOutput)
        );

        let mut trace = valid_trace();
        trace.inputs.push(NetworkInputEventV1 {
            ordinal: 2,
            channel: channel_id(),
            release: NetworkReleaseV1 {
                not_before_global_time: global_time_after_epoch(21),
                after_transmitted_offset: 3,
            },
            event: NetworkInputKindV1::InboundBytes {
                stream_offset: 8,
                bytes: b"late".to_vec(),
            },
        });
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::EventAfterTerminal)
        );
    }

    fn valid_v2_trace() -> NetworkTraceV2 {
        let client = NetworkChannelId(10);
        let datagram = NetworkChannelId(20);
        NetworkTraceV2 {
            epoch: trace_epoch(),
            channels: vec![
                NetworkChannelV2 {
                    id: client,
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    local_address: Some(NetworkAddressV2::Inet4 {
                        address: [10, 0, 0, 2],
                        port: 40_000,
                    }),
                    peer_address: Some(NetworkAddressV2::Inet4 {
                        address: [192, 0, 2, 10],
                        port: 443,
                    }),
                    accepted_from: None,
                },
                NetworkChannelV2 {
                    id: datagram,
                    transport: NetworkTransportV2::Udp,
                    role: NetworkEndpointRoleV2::Datagram,
                    local_address: Some(NetworkAddressV2::Inet4 {
                        address: [10, 0, 0, 2],
                        port: 50_000,
                    }),
                    peer_address: None,
                    accepted_from: None,
                },
            ],
            outputs: vec![NetworkOutputEventV2 {
                channel: client,
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"request".to_vec(),
                },
            }],
            inputs: vec![
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel: client,
                    release: NetworkReleaseV2 {
                        not_before_global_time: global_time_after_epoch(10),
                        after_transmitted_offset: 3,
                    },
                    event: NetworkInputKindV2::StreamBytes {
                        stream_offset: 0,
                        bytes: b"response".to_vec(),
                    },
                },
                NetworkInputEventV2 {
                    ordinal: 1,
                    channel: datagram,
                    release: NetworkReleaseV2 {
                        not_before_global_time: global_time_after_epoch(11),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::Datagram(NetworkDatagramV2 {
                        sequence: 0,
                        bytes: b"packet".to_vec(),
                        source: Some(NetworkAddressV2::Inet4 {
                            address: [198, 51, 100, 2],
                            port: 53,
                        }),
                        destination: Some(NetworkAddressV2::Inet4 {
                            address: [10, 0, 0, 2],
                            port: 50_000,
                        }),
                        ancillary: Some(NetworkAncillaryDataV2 {
                            bytes: vec![0; 16],
                            objects: vec![NetworkAncillaryObjectRefV2 {
                                byte_offset: 4,
                                object: NetworkAncillaryObjectV2::Credentials {
                                    pid: 7,
                                    uid: 1000,
                                    gid: 1000,
                                },
                            }],
                            truncated: false,
                        }),
                        message_flags: 0,
                    }),
                },
            ],
        }
    }

    #[test]
    fn v2_round_trips_with_multi_channel_datagram_and_ancillary_metadata() {
        let trace = valid_v2_trace();
        trace.validate().unwrap();
        let mut bytes = Vec::new();
        NetworkTrace::V2(trace.clone())
            .write_framed(&mut bytes)
            .unwrap();
        assert_eq!(
            NetworkTrace::read_framed(Cursor::new(bytes)).unwrap(),
            NetworkTrace::V2(trace)
        );
    }

    #[test]
    fn v2_codec_binds_the_exact_submicrosecond_epoch() {
        let exact_epoch = Utc.timestamp_opt(1_790_000_000, 123_456_789).unwrap();
        let trace = NetworkTraceV2 {
            epoch: exact_epoch,
            channels: vec![],
            inputs: vec![],
            outputs: vec![],
        };
        let mut frame = Vec::new();
        trace.write_framed(&mut frame).unwrap();
        let decoded = NetworkTraceV2::read_framed(Cursor::new(frame)).unwrap();
        assert_eq!(decoded.epoch, exact_epoch);
    }

    #[test]
    fn versioned_reader_preserves_v1_decode_compatibility() {
        let v1 = valid_trace();
        assert_eq!(
            NetworkTrace::read_framed(Cursor::new(framed(&v1))).unwrap(),
            NetworkTrace::V1(v1)
        );
    }

    #[test]
    fn v2_refuses_broken_datagram_boundaries_and_ancillary_relocations() {
        let mut trace = valid_v2_trace();
        let NetworkInputKindV2::Datagram(datagram) = &mut trace.inputs[1].event else {
            unreachable!()
        };
        datagram.sequence = 2;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::NonCanonicalDatagramSequence)
        );

        let mut trace = valid_v2_trace();
        let NetworkInputKindV2::Datagram(datagram) = &mut trace.inputs[1].event else {
            unreachable!()
        };
        datagram.ancillary.as_mut().unwrap().objects[0].byte_offset = 15;
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::InvalidAncillaryObjectOffset)
        );

        let mut trace = valid_v2_trace();
        let NetworkInputKindV2::Datagram(datagram) = &mut trace.inputs[1].event else {
            unreachable!()
        };
        let ancillary = datagram.ancillary.as_mut().unwrap();
        ancillary.objects = vec![
            NetworkAncillaryObjectRefV2 {
                byte_offset: 4,
                object: NetworkAncillaryObjectV2::FileDescriptor {
                    object: NetworkObjectId(1),
                },
            },
            NetworkAncillaryObjectRefV2 {
                byte_offset: 6,
                object: NetworkAncillaryObjectV2::FileDescriptor {
                    object: NetworkObjectId(2),
                },
            },
        ];
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::InvalidAncillaryObjectOffset)
        );
    }

    #[test]
    fn v2_accepts_readiness_clear_but_rejects_duplicate_connect() {
        let mut trace = valid_v2_trace();
        trace.inputs.push(NetworkInputEventV2 {
            ordinal: 2,
            channel: NetworkChannelId(10),
            release: NetworkReleaseV2 {
                not_before_global_time: global_time_after_epoch(12),
                after_transmitted_offset: 3,
            },
            event: NetworkInputKindV2::Readiness(NetworkReadinessV2::default()),
        });
        trace.validate().unwrap();

        let connect = |ordinal, delta| NetworkInputEventV2 {
            ordinal,
            channel: NetworkChannelId(10),
            release: NetworkReleaseV2 {
                not_before_global_time: global_time_after_epoch(delta),
                after_transmitted_offset: 0,
            },
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        };
        trace.inputs = vec![connect(0, 1), connect(1, 2)];
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::DuplicateConnect)
        );
    }

    #[test]
    fn exact_datagram_requires_address_lengths_to_match_addresses() {
        let mut trace = valid_v2_trace();
        let NetworkInputKindV2::Datagram(datagram) = trace.inputs.remove(1).event else {
            unreachable!()
        };
        trace.inputs.push(NetworkInputEventV2 {
            ordinal: 1,
            channel: NetworkChannelId(20),
            release: NetworkReleaseV2 {
                not_before_global_time: global_time_after_epoch(11),
                after_transmitted_offset: 0,
            },
            event: NetworkInputKindV2::DatagramExact(NetworkDatagramExactV2 {
                datagram,
                source_length: None,
                destination_length: Some(16),
            }),
        });
        assert_eq!(
            trace.validate(),
            Err(NetworkTraceValidationError::AddressLengthMismatch)
        );
    }
}
