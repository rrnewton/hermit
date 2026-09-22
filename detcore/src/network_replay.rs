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
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use detcore_model::fd::OpenFileId;
use detcore_model::network_trace::NetworkAddressV1;
use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkAncillaryDataV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkChannelV2;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkDatagramExactV2;
use detcore_model::network_trace::NetworkDatagramV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV1;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkOutputEventV2;
use detcore_model::network_trace::NetworkOutputKindV2;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTrace;
use detcore_model::network_trace::NetworkTraceCodecError;
use detcore_model::network_trace::NetworkTraceV1;
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
    /// Syscall return value after applying the caller's `MSG_TRUNC` flag.
    pub return_len: usize,
    /// Sender address.
    pub source: Option<detcore_model::network_trace::NetworkAddressV2>,
    /// Destination address recorded for this packet.
    pub destination: Option<detcore_model::network_trace::NetworkAddressV2>,
    /// Kernel-reported source address length before guest-buffer truncation.
    pub source_length: Option<u32>,
    /// Address length originally supplied for an explicit destination.
    pub destination_length: Option<u32>,
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

/// One stream `recvmsg(2)` delivery with control metadata tied to the first
/// byte. The ancillary value is returned once even when payload consumption is
/// partial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamMessageDelivery {
    /// Stream payload copied into guest buffers.
    pub bytes: Vec<u8>,
    /// Control data, returned only on the first partial delivery.
    pub ancillary: Option<NetworkAncillaryDataV2>,
    /// Recorded message flags.
    pub message_flags: i32,
}

/// Result of a stream message receive attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamMessageReceiveOutcome {
    /// One payload/control delivery.
    Message(StreamMessageDelivery),
    /// Peer write side is closed.
    EndOfFile,
    /// Recorded positive Linux errno.
    Error(i32),
    /// Nonblocking receive found no available outcome.
    WouldBlock,
    /// Blocking receive must wait for a release.
    Pending,
}

/// Normalized receive behavior shared by `read`, `recv`, and `recvmsg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkReceiveOptions {
    /// Maximum payload bytes the guest can accept.
    pub maximum: usize,
    /// Effective nonblocking state, including `MSG_DONTWAIT`.
    pub nonblocking: bool,
    /// Linux receive flags relevant to data movement.
    pub flags: i32,
    /// Effective `SO_RCVLOWAT`, clamped to at least one by the engine.
    pub receive_low_water: usize,
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
    /// A trace channel identity is single-use even after its last OFD alias is
    /// retired; fd/OFD reuse must never resurrect an old connection.
    retired_channels: BTreeSet<NetworkChannelId>,
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
    inbound_consumed: u64,
    inbound: VecDeque<InboundOutcome>,
    explicit_readiness: NetworkReadinessV2,
    transmitted: u64,
    outbound: VecDeque<OutboundOutcome>,
    local_write_closed: bool,
    peer_write_closed: bool,
}

#[derive(Debug)]
enum InboundOutcome {
    Stream {
        bytes: VecDeque<u8>,
        ancillary: Option<NetworkAncillaryDataV2>,
        message_flags: i32,
        requires_message_io: bool,
    },
    Datagram {
        datagram: NetworkDatagramV2,
        source_length: Option<u32>,
        destination_length: Option<u32>,
    },
    Error {
        stream_offset: u64,
        errno: i32,
    },
    PeerShutdown {
        stream_offset: u64,
        direction: NetworkShutdownV2,
    },
    Control(ConnectionOutcome),
}

#[derive(Debug)]
enum OutboundOutcome {
    Stream {
        bytes: Vec<u8>,
        consumed: usize,
        ancillary: Option<NetworkAncillaryDataV2>,
        message_flags: i32,
    },
    Datagram(NetworkDatagramV2),
    DatagramExact(NetworkDatagramExactV2),
    Error {
        stream_offset: u64,
        errno: i32,
    },
    Shutdown {
        stream_offset: u64,
        direction: NetworkShutdownV2,
    },
}

/// Upgrade the fully validated V1 single-client envelope into the V2 shared
/// engine model. This is a semantic adapter, not a best-effort decode: every
/// release condition and stream offset is retained exactly.
pub fn upgrade_v1_trace(trace: NetworkTraceV1) -> Result<NetworkTraceV2, NetworkReplayError> {
    trace.validate()?;
    let legacy_channel = &trace.channels[0];
    let channel = NetworkChannelId(legacy_channel.id.deterministic_socket_cookie());
    let map_address = |address: NetworkAddressV1| match address {
        NetworkAddressV1::Inet4 { address, port } => NetworkAddressV2::Inet4 { address, port },
        NetworkAddressV1::Inet6 {
            address,
            port,
            flowinfo,
            scope_id,
        } => NetworkAddressV2::Inet6 {
            address,
            port,
            flowinfo,
            scope_id,
        },
    };
    let channels = vec![NetworkChannelV2 {
        id: channel,
        transport: NetworkTransportV2::Tcp,
        role: detcore_model::network_trace::NetworkEndpointRoleV2::OutboundClient,
        local_address: Some(map_address(legacy_channel.local_address.clone())),
        peer_address: Some(map_address(legacy_channel.peer_address.clone())),
        accepted_from: None,
    }];
    let inputs = trace
        .inputs
        .into_iter()
        .map(|input| NetworkInputEventV2 {
            ordinal: input.ordinal,
            channel,
            release: detcore_model::network_trace::NetworkReleaseV2 {
                not_before_global_time: input.release.not_before_global_time,
                after_transmitted_offset: input.release.after_transmitted_offset,
            },
            event: match input.event {
                NetworkInputKindV1::InboundBytes {
                    stream_offset,
                    bytes,
                } => NetworkInputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                },
                NetworkInputKindV1::PeerWriteClosed { stream_offset } => {
                    NetworkInputKindV2::PeerShutdown {
                        stream_offset,
                        direction: NetworkShutdownV2::Write,
                    }
                }
                NetworkInputKindV1::SocketError {
                    stream_offset,
                    errno,
                } => NetworkInputKindV2::SocketError {
                    stream_offset,
                    errno,
                },
            },
        })
        .collect();
    let outputs = trace
        .outputs
        .into_iter()
        .map(|output| NetworkOutputEventV2 {
            channel,
            event: NetworkOutputKindV2::StreamBytes {
                stream_offset: output.stream_offset,
                bytes: output.bytes,
            },
        })
        .collect();
    let upgraded = NetworkTraceV2 {
        epoch: trace.epoch,
        channels,
        inputs,
        outputs,
    };
    upgraded.validate()?;
    Ok(upgraded)
}

/// Decode exactly once from an already-open, host-namespace resource and
/// initialize the shared engine. Callers may hash these same bytes before
/// passing the handle; the engine never reopens a path.
pub fn replay_from_reader<R: Read>(reader: R) -> Result<NetworkReplayEngine, NetworkReplayError> {
    let trace = NetworkTrace::read_framed(reader).map_err(NetworkReplayError::Codec)?;
    NetworkReplayEngine::replay_versioned(trace)
}

static PUBLICATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Host-side reservation for atomic trace publication.
///
/// The destination directory and private temporary file are opened with
/// `O_NOFOLLOW` before container entry. Publication uses `RENAME_NOREPLACE`, so
/// neither a pre-existing file nor a replacement symlink is overwritten.
#[derive(Debug)]
pub struct NetworkTracePublication {
    directory: File,
    temporary: File,
    temporary_name: CString,
    destination_name: CString,
    committed: bool,
}

impl NetworkTracePublication {
    /// Reserve a private temporary file beside a destination which must not
    /// already exist. Invoke this in the host namespace before entering the
    /// guest container.
    pub fn reserve(destination: &Path) -> io::Result<Self> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let destination_name = destination.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "trace path has no filename")
        })?;
        let parent = CString::new(parent.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "trace parent contains NUL")
        })?;
        let destination_name = CString::new(destination_name.as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "trace filename contains NUL")
        })?;
        let directory_fd = unsafe {
            libc::open(
                parent.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if directory_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let directory = unsafe { File::from_raw_fd(directory_fd) };

        // Refuse an existing destination at reservation time; renameat2 below
        // repeats the check atomically at publication time.
        let mut existing_stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let existing = unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                destination_name.as_ptr(),
                existing_stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if existing == 0 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "network trace destination already exists",
            ));
        }
        let stat_error = io::Error::last_os_error();
        if stat_error.raw_os_error() != Some(libc::ENOENT) {
            return Err(stat_error);
        }

        for _ in 0..128 {
            let sequence = PUBLICATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temporary_name = CString::new(format!(
                ".hermit-network-trace.{}.{}.tmp",
                std::process::id(),
                sequence
            ))
            .expect("generated trace temporary name has no NUL");
            let temporary_fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    temporary_name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_CLOEXEC
                        | libc::O_NOFOLLOW,
                    0o600,
                )
            };
            if temporary_fd >= 0 {
                return Ok(Self {
                    directory,
                    temporary: unsafe { File::from_raw_fd(temporary_fd) },
                    temporary_name,
                    destination_name,
                    committed: false,
                });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique network trace temporary file",
        ))
    }

    /// The already-open host resource into which Detcore must finalize.
    pub fn writer(&mut self) -> &mut File {
        &mut self.temporary
    }

    /// Duplicate the exact reserved writer for transfer into Detcore. The
    /// caller owns inheritance/closure policy; the publication token remains
    /// host-side and commits only after Detcore finishes successfully.
    pub fn try_clone_writer(&self) -> io::Result<File> {
        self.temporary.try_clone()
    }

    /// Borrow the reserved writer descriptor for same-process configuration.
    pub fn writer_fd(&self) -> std::os::fd::RawFd {
        self.temporary.as_raw_fd()
    }

    /// Validate, sync, and atomically publish a finalized V2 trace.
    pub fn publish(mut self, trace: &NetworkTraceV2) -> Result<(), NetworkReplayError> {
        self.temporary.set_len(0).map_err(NetworkReplayError::Io)?;
        self.temporary
            .seek(SeekFrom::Start(0))
            .map_err(NetworkReplayError::Io)?;
        trace
            .write_framed(&mut self.temporary)
            .map_err(NetworkReplayError::Codec)?;
        self.commit()
    }

    /// Validate and atomically publish bytes written through [`Self::writer`].
    /// This consumes the exact already-open handle; no path is reopened.
    pub fn commit(mut self) -> Result<(), NetworkReplayError> {
        self.temporary.sync_all().map_err(NetworkReplayError::Io)?;
        self.temporary
            .seek(SeekFrom::Start(0))
            .map_err(NetworkReplayError::Io)?;
        NetworkTrace::read_framed(&mut self.temporary).map_err(NetworkReplayError::Codec)?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                self.directory.as_raw_fd(),
                self.temporary_name.as_ptr(),
                self.directory.as_raw_fd(),
                self.destination_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result < 0 {
            return Err(NetworkReplayError::Io(io::Error::last_os_error()));
        }
        self.committed = true;
        self.directory.sync_all().map_err(NetworkReplayError::Io)
    }
}

impl Drop for NetworkTracePublication {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.temporary_name.as_ptr(), 0);
            }
        }
    }
}

impl NetworkReplayEngine {
    /// Decode once from an already-open host resource and initialize replay.
    pub fn replay_from_reader<R: Read>(reader: R) -> Result<Self, NetworkReplayError> {
        replay_from_reader(reader)
    }

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
            retired_channels: BTreeSet::new(),
        }
    }

    /// Capture the host wall clock exactly once as an explicit recorded input.
    /// Replay subsequently uses only the epoch serialized in the trace.
    pub fn record_now() -> Self {
        Self::record(Utc::now())
    }

    /// Use an explicitly supplied epoch or capture the current wall clock once
    /// when the caller did not supply one.
    pub fn record_with_optional_epoch(epoch: Option<DateTime<Utc>>) -> Self {
        epoch.map_or_else(Self::record_now, Self::record)
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
            retired_channels: BTreeSet::new(),
        })
    }

    /// Create a shared-engine replay from either supported codec version.
    /// V1's declared single outbound TCP envelope is upgraded without changing
    /// its release gates, byte offsets, or stable socket identity.
    pub fn replay_versioned(trace: NetworkTrace) -> Result<Self, NetworkReplayError> {
        match trace {
            NetworkTrace::V1(trace) => Self::replay(upgrade_v1_trace(trace)?),
            NetworkTrace::V2(trace) => Self::replay(trace),
        }
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
        if self.retired_channels.contains(&channel) {
            return Err(NetworkReplayError::ChannelRetired(channel));
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
        self.retired_channels.insert(channel);
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
        self.receive_stream_with_options(
            open_file,
            NetworkReceiveOptions {
                maximum,
                nonblocking,
                flags: 0,
                receive_low_water: 1,
            },
        )
    }

    /// Normalized receive used by `read`, `recv`, and `recvmsg`. Unsupported
    /// flags fail closed; `MSG_PEEK`, `MSG_WAITALL`, `MSG_DONTWAIT`, and the
    /// effective `SO_RCVLOWAT` are modeled centrally.
    pub fn receive_stream_with_options(
        &mut self,
        open_file: OpenFileId,
        options: NetworkReceiveOptions,
    ) -> Result<StreamReceiveOutcome, NetworkReplayError> {
        let supported = libc::MSG_PEEK | libc::MSG_WAITALL | libc::MSG_DONTWAIT;
        if options.flags & !supported != 0 {
            return Err(NetworkReplayError::UnsupportedReceiveFlags(
                options.flags & !supported,
            ));
        }
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel(channel)?;
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        if matches!(
            state.inbound.front(),
            Some(InboundOutcome::Stream {
                requires_message_io: true,
                ..
            })
        ) {
            return Err(NetworkReplayError::AncillaryRequiresMessageIo(channel));
        }
        let mut available = 0usize;
        let mut terminal_after_available = state.peer_write_closed;
        for outcome in &state.inbound {
            match outcome {
                InboundOutcome::Stream {
                    bytes,
                    requires_message_io: false,
                    ..
                } => available = available.saturating_add(bytes.len()),
                InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                }
                | InboundOutcome::Error { .. } => {
                    terminal_after_available = true;
                    break;
                }
                _ => break,
            }
        }
        let nonblocking = options.nonblocking || options.flags & libc::MSG_DONTWAIT != 0;
        let required = if options.flags & libc::MSG_WAITALL != 0 {
            options.maximum
        } else {
            options.receive_low_water.max(1).min(options.maximum.max(1))
        };
        if available < required && !nonblocking && !terminal_after_available {
            return Ok(StreamReceiveOutcome::Pending);
        }
        if options.flags & libc::MSG_PEEK != 0 && available != 0 {
            let mut bytes = Vec::with_capacity(options.maximum.min(available));
            for outcome in &state.inbound {
                let InboundOutcome::Stream {
                    bytes: fragment,
                    requires_message_io: false,
                    ..
                } = outcome
                else {
                    break;
                };
                bytes.extend(fragment.iter().copied().take(options.maximum - bytes.len()));
                if bytes.len() == options.maximum {
                    break;
                }
            }
            return Ok(StreamReceiveOutcome::Bytes(bytes));
        }
        self.receive_stream_available(open_file, options.maximum, nonblocking)
    }

    fn receive_stream_available(
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
        let mut received = Vec::new();
        while received.len() < maximum {
            match state.inbound.front_mut() {
                Some(InboundOutcome::Stream {
                    requires_message_io: true,
                    ..
                }) => {
                    if received.is_empty() {
                        return Err(NetworkReplayError::AncillaryRequiresMessageIo(channel));
                    }
                    break;
                }
                Some(InboundOutcome::Stream { bytes, .. }) => {
                    let count = (maximum - received.len()).min(bytes.len());
                    received.extend(bytes.drain(..count));
                    state.inbound_consumed = state
                        .inbound_consumed
                        .checked_add(count as u64)
                        .ok_or(NetworkReplayError::Overflow)?;
                    if bytes.is_empty() {
                        state.inbound.pop_front();
                    }
                }
                Some(InboundOutcome::Error {
                    stream_offset,
                    errno,
                }) if *stream_offset == state.inbound_consumed && received.is_empty() => {
                    let errno = *errno;
                    state.inbound.pop_front();
                    return Ok(StreamReceiveOutcome::Error(errno));
                }
                Some(InboundOutcome::PeerShutdown {
                    stream_offset,
                    direction,
                }) if *stream_offset == state.inbound_consumed && received.is_empty() => {
                    let closes_write = matches!(
                        direction,
                        NetworkShutdownV2::Write | NetworkShutdownV2::Both
                    );
                    state.inbound.pop_front();
                    if closes_write {
                        state.peer_write_closed = true;
                        return Ok(StreamReceiveOutcome::EndOfFile);
                    }
                }
                Some(_) => break,
                None => break,
            }
        }
        if !received.is_empty() {
            return Ok(StreamReceiveOutcome::Bytes(received));
        }
        if state.peer_write_closed {
            return Ok(StreamReceiveOutcome::EndOfFile);
        }
        Ok(if nonblocking {
            StreamReceiveOutcome::WouldBlock
        } else {
            StreamReceiveOutcome::Pending
        })
    }

    /// Consume a stream message while preserving ancillary object identity and
    /// its association with the first delivered payload byte.
    pub fn receive_stream_message(
        &mut self,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
        peek: bool,
    ) -> Result<StreamMessageReceiveOutcome, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        match state.inbound.front_mut() {
            Some(InboundOutcome::Stream {
                bytes,
                ancillary,
                message_flags,
                requires_message_io,
            }) => {
                let count = maximum.min(bytes.len());
                let payload: Vec<_> = bytes.iter().copied().take(count).collect();
                let control = ancillary.clone();
                let flags = *message_flags;
                if !peek {
                    bytes.drain(..count);
                    state.inbound_consumed = state
                        .inbound_consumed
                        .checked_add(count as u64)
                        .ok_or(NetworkReplayError::Overflow)?;
                    // SCM_RIGHTS/SCM_CREDENTIALS are delivered once, even if
                    // the payload itself was only partially consumed.
                    ancillary.take();
                    *requires_message_io = false;
                    if bytes.is_empty() {
                        state.inbound.pop_front();
                    }
                }
                Ok(StreamMessageReceiveOutcome::Message(
                    StreamMessageDelivery {
                        bytes: payload,
                        ancillary: control,
                        message_flags: flags,
                    },
                ))
            }
            Some(InboundOutcome::Error { errno, .. }) => {
                let errno = *errno;
                state.inbound.pop_front();
                Ok(StreamMessageReceiveOutcome::Error(errno))
            }
            Some(InboundOutcome::PeerShutdown {
                direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                ..
            }) => {
                if !peek {
                    state.inbound.pop_front();
                    state.peer_write_closed = true;
                }
                Ok(StreamMessageReceiveOutcome::EndOfFile)
            }
            Some(_) => Err(NetworkReplayError::OperationOrderMismatch(channel)),
            None if state.peer_write_closed => Ok(StreamMessageReceiveOutcome::EndOfFile),
            None if nonblocking => Ok(StreamMessageReceiveOutcome::WouldBlock),
            None => Ok(StreamMessageReceiveOutcome::Pending),
        }
    }

    /// Consume one recorded datagram boundary.
    pub fn receive_datagram(
        &mut self,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
    ) -> Result<DatagramReceiveOutcome, NetworkReplayError> {
        self.receive_datagram_with_flags(open_file, maximum, nonblocking, 0)
    }

    /// Consume one datagram and apply Linux `MSG_TRUNC` copy/return semantics.
    pub fn receive_datagram_with_flags(
        &mut self,
        open_file: OpenFileId,
        maximum: usize,
        nonblocking: bool,
        receive_flags: i32,
    ) -> Result<DatagramReceiveOutcome, NetworkReplayError> {
        self.receive_datagram_with_options(
            open_file,
            NetworkReceiveOptions {
                maximum,
                nonblocking,
                flags: receive_flags,
                receive_low_water: 1,
            },
        )
    }

    /// Normalized datagram receive with boundary, peek, and truncation rules.
    pub fn receive_datagram_with_options(
        &mut self,
        open_file: OpenFileId,
        options: NetworkReceiveOptions,
    ) -> Result<DatagramReceiveOutcome, NetworkReplayError> {
        let supported = libc::MSG_PEEK | libc::MSG_TRUNC | libc::MSG_DONTWAIT;
        if options.flags & !supported != 0 {
            return Err(NetworkReplayError::UnsupportedReceiveFlags(
                options.flags & !supported,
            ));
        }
        let maximum = options.maximum;
        let nonblocking = options.nonblocking || options.flags & libc::MSG_DONTWAIT != 0;
        let receive_flags = options.flags;
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if !state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        match state.inbound.front() {
            Some(InboundOutcome::Error { errno, .. }) => {
                let errno = *errno;
                state.inbound.pop_front();
                Ok(DatagramReceiveOutcome::Error(errno))
            }
            Some(InboundOutcome::Datagram { .. }) => {
                let Some(InboundOutcome::Datagram {
                    datagram,
                    source_length,
                    destination_length,
                }) = state.inbound.front()
                else {
                    unreachable!()
                };
                let datagram = datagram.clone();
                let source_length = *source_length;
                let destination_length = *destination_length;
                if receive_flags & libc::MSG_PEEK == 0 {
                    state.inbound.pop_front();
                }
                let original_len = datagram.bytes.len();
                let truncated = maximum < original_len;
                let mut message_flags = datagram.message_flags;
                if truncated {
                    message_flags |= libc::MSG_TRUNC;
                }
                Ok(DatagramReceiveOutcome::Datagram(DatagramDelivery {
                    bytes: datagram.bytes[..maximum.min(original_len)].to_vec(),
                    original_len,
                    return_len: if receive_flags & libc::MSG_TRUNC != 0 {
                        original_len
                    } else {
                        maximum.min(original_len)
                    },
                    source: datagram.source,
                    destination: datagram.destination,
                    source_length,
                    destination_length,
                    ancillary: datagram.ancillary,
                    message_flags,
                }))
            }
            Some(_) => Err(NetworkReplayError::OperationOrderMismatch(channel)),
            None => Ok(if nonblocking {
                DatagramReceiveOutcome::WouldBlock
            } else {
                DatagramReceiveOutcome::Pending
            }),
        }
    }

    /// Validate stream bytes. Callers may split a recorded fragment, while a
    /// recorded fragment boundary preserves the corresponding partial-write
    /// progress and cannot be coalesced away.
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
        match state.outbound.front_mut() {
            Some(OutboundOutcome::Error {
                stream_offset,
                errno,
            }) if *stream_offset == state.transmitted => {
                let errno = *errno;
                state.outbound.pop_front();
                Ok(StreamTransmitOutcome::Error(errno))
            }
            Some(OutboundOutcome::Stream {
                ancillary: Some(_), ..
            }) => Err(NetworkReplayError::AncillaryRequiresMessageIo(channel)),
            Some(OutboundOutcome::Stream {
                bytes: expected,
                consumed,
                ..
            }) => {
                let accepted = bytes.len().min(expected.len() - *consumed);
                if expected[*consumed..*consumed + accepted] != bytes[..accepted] {
                    return Err(NetworkReplayError::OutboundMismatch {
                        channel,
                        offset: state.transmitted,
                    });
                }
                if accepted == 0 && !bytes.is_empty() {
                    return Err(NetworkReplayError::TraceExhausted(channel));
                }
                *consumed += accepted;
                state.transmitted = state
                    .transmitted
                    .checked_add(accepted as u64)
                    .ok_or(NetworkReplayError::Overflow)?;
                if *consumed == expected.len() {
                    state.outbound.pop_front();
                }
                Ok(StreamTransmitOutcome::Accepted(accepted))
            }
            Some(_) => Err(NetworkReplayError::OperationOrderMismatch(channel)),
            None if bytes.is_empty() => Ok(StreamTransmitOutcome::Accepted(0)),
            None => Err(NetworkReplayError::TraceExhausted(channel)),
        }
    }

    /// Validate one stream `sendmsg(2)` fragment and its ancillary metadata.
    pub fn transmit_stream_message(
        &mut self,
        open_file: OpenFileId,
        bytes: &[u8],
        ancillary: &NetworkAncillaryDataV2,
        message_flags: i32,
    ) -> Result<StreamTransmitOutcome, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if state.transport.is_datagram() || state.local_write_closed {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let Some(OutboundOutcome::Stream {
            bytes: expected,
            consumed,
            ancillary: expected_ancillary,
            message_flags: expected_flags,
        }) = state.outbound.front_mut()
        else {
            return Err(NetworkReplayError::OperationOrderMismatch(channel));
        };
        if *consumed != 0
            || expected_ancillary.as_ref() != Some(ancillary)
            || *expected_flags != message_flags
        {
            return Err(NetworkReplayError::OutboundMismatch {
                channel,
                offset: state.transmitted,
            });
        }
        let accepted = bytes.len().min(expected.len());
        if expected[..accepted] != bytes[..accepted] || accepted == 0 && !bytes.is_empty() {
            return Err(NetworkReplayError::OutboundMismatch {
                channel,
                offset: state.transmitted,
            });
        }
        *consumed = accepted;
        expected_ancillary.take();
        state.transmitted = state
            .transmitted
            .checked_add(accepted as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        if accepted == expected.len() {
            state.outbound.pop_front();
        }
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
        let Some(expected) = state.outbound.pop_front() else {
            return Err(NetworkReplayError::TraceExhausted(channel));
        };
        if !matches!(expected, OutboundOutcome::Datagram(ref item) if item == datagram) {
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

    /// Validate a datagram including exact source/destination address lengths.
    pub fn transmit_datagram_exact(
        &mut self,
        open_file: OpenFileId,
        datagram: &NetworkDatagramExactV2,
    ) -> Result<(), NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel_mut(channel)?;
        if !state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let Some(OutboundOutcome::DatagramExact(expected)) = state.outbound.pop_front() else {
            return Err(NetworkReplayError::OperationOrderMismatch(channel));
        };
        if expected != *datagram {
            return Err(NetworkReplayError::OutboundMismatch {
                channel,
                offset: state.transmitted,
            });
        }
        state.transmitted = state
            .transmitted
            .checked_add(datagram.datagram.bytes.len() as u64)
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
        let Some(OutboundOutcome::Shutdown {
            stream_offset: offset,
            direction: expected,
        }) = state.outbound.pop_front()
        else {
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
        let state = self.replay_channel_mut(channel)?;
        if !matches!(state.inbound.front(), Some(InboundOutcome::Control(_))) {
            return Ok(None);
        }
        let Some(InboundOutcome::Control(outcome)) = state.inbound.pop_front() else {
            unreachable!()
        };
        Ok(Some(outcome))
    }

    /// Readiness derived from available modeled state, not waiter identity.
    pub fn readiness(
        &self,
        open_file: OpenFileId,
    ) -> Result<NetworkReadinessV2, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.replay_channel(channel)?;
        let mut readiness = state.explicit_readiness;
        readiness.readable |= !state.inbound.is_empty() || state.peer_write_closed;
        readiness.writable |= !state.local_write_closed && !state.outbound.is_empty();
        readiness.error |= matches!(state.inbound.front(), Some(InboundOutcome::Error { .. }));
        readiness.hangup |= state.peer_write_closed
            || matches!(
                state.inbound.front(),
                Some(InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                })
            );
        Ok(readiness)
    }

    /// Query readiness for a registration. Edge-triggered and one-shot epoll
    /// require per-registration state outside this per-OFD engine and are
    /// therefore refused until the adapter supplies that state explicitly.
    pub fn readiness_for_interest(
        &self,
        open_file: OpenFileId,
        edge_triggered: bool,
        one_shot: bool,
    ) -> Result<NetworkReadinessV2, NetworkReplayError> {
        if edge_triggered || one_shot {
            return Err(NetworkReplayError::UnsupportedReadinessMode);
        }
        self.readiness(open_file)
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
            if !state.inbound.is_empty() || !state.outbound.is_empty() {
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
            inbound_consumed: 0,
            inbound: VecDeque::new(),
            explicit_readiness: NetworkReadinessV2::default(),
            transmitted: 0,
            outbound: VecDeque::new(),
            local_write_closed: false,
            peer_write_closed: false,
        }
    }

    fn append_expected_output(&mut self, output: &NetworkOutputKindV2) {
        match output {
            NetworkOutputKindV2::StreamBytes { bytes, .. } => {
                self.outbound.push_back(OutboundOutcome::Stream {
                    bytes: bytes.clone(),
                    consumed: 0,
                    ancillary: None,
                    message_flags: 0,
                })
            }
            NetworkOutputKindV2::StreamMessage {
                bytes,
                ancillary,
                message_flags,
                ..
            } => self.outbound.push_back(OutboundOutcome::Stream {
                bytes: bytes.clone(),
                consumed: 0,
                ancillary: Some(ancillary.clone()),
                message_flags: *message_flags,
            }),
            NetworkOutputKindV2::Datagram(datagram) => self
                .outbound
                .push_back(OutboundOutcome::Datagram(datagram.clone())),
            NetworkOutputKindV2::DatagramExact(datagram) => self
                .outbound
                .push_back(OutboundOutcome::DatagramExact(datagram.clone())),
            NetworkOutputKindV2::Shutdown {
                stream_offset,
                direction,
            } => self.outbound.push_back(OutboundOutcome::Shutdown {
                stream_offset: *stream_offset,
                direction: *direction,
            }),
            NetworkOutputKindV2::SocketError {
                stream_offset,
                errno,
            } => self.outbound.push_back(OutboundOutcome::Error {
                stream_offset: *stream_offset,
                errno: *errno,
            }),
        }
    }

    fn release(&mut self, input: NetworkInputKindV2) -> Result<(), NetworkReplayError> {
        match input {
            NetworkInputKindV2::Connect(result) => self
                .inbound
                .push_back(InboundOutcome::Control(ConnectionOutcome::Connect(result))),
            NetworkInputKindV2::Accept {
                accepted,
                peer,
                ancillary,
            } => self
                .inbound
                .push_back(InboundOutcome::Control(ConnectionOutcome::Accept {
                    accepted,
                    peer,
                    ancillary,
                })),
            NetworkInputKindV2::StreamBytes { bytes, .. } => {
                self.inbound.push_back(InboundOutcome::Stream {
                    bytes: bytes.into(),
                    ancillary: None,
                    message_flags: 0,
                    requires_message_io: false,
                })
            }
            NetworkInputKindV2::StreamMessage {
                bytes,
                ancillary,
                message_flags,
                ..
            } => self.inbound.push_back(InboundOutcome::Stream {
                bytes: bytes.into(),
                ancillary: Some(ancillary),
                message_flags,
                requires_message_io: true,
            }),
            NetworkInputKindV2::Datagram(datagram) => {
                self.inbound.push_back(InboundOutcome::Datagram {
                    datagram,
                    source_length: None,
                    destination_length: None,
                })
            }
            NetworkInputKindV2::DatagramExact(exact) => {
                self.inbound.push_back(InboundOutcome::Datagram {
                    datagram: exact.datagram,
                    source_length: exact.source_length,
                    destination_length: exact.destination_length,
                })
            }
            NetworkInputKindV2::PeerShutdown {
                stream_offset,
                direction,
            } => self.inbound.push_back(InboundOutcome::PeerShutdown {
                stream_offset,
                direction,
            }),
            NetworkInputKindV2::SocketError {
                stream_offset,
                errno,
            } => self.inbound.push_back(InboundOutcome::Error {
                stream_offset,
                errno,
            }),
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
    /// Versioned trace framing or payload failed validation.
    Codec(NetworkTraceCodecError),
    /// Host-side trace handle or atomic publication failed.
    Io(io::Error),
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
    /// Channel identity was permanently retired and cannot be rebound.
    ChannelRetired(NetworkChannelId),
    /// No channel is bound to this OFD.
    UnboundOpenFile(OpenFileId),
    /// Operation does not match channel transport.
    TransportMismatch(NetworkChannelId),
    /// The next recorded outcome belongs to a different operation.
    OperationOrderMismatch(NetworkChannelId),
    /// Ancillary objects require the normalized sendmsg/recvmsg path.
    AncillaryRequiresMessageIo(NetworkChannelId),
    /// Edge-triggered or one-shot readiness requires adapter-owned interest state.
    UnsupportedReadinessMode,
    /// Receive flags outside the normalized modeled subset.
    UnsupportedReceiveFlags(i32),
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
    use std::io::Cursor;
    use std::os::unix::fs::symlink;

    use chrono::TimeZone;
    use detcore_model::network_trace::NetworkAddressV1;
    use detcore_model::network_trace::NetworkAddressV2;
    use detcore_model::network_trace::NetworkChannelV1;
    use detcore_model::network_trace::NetworkEndpointRoleV1;
    use detcore_model::network_trace::NetworkEndpointRoleV2;
    use detcore_model::network_trace::NetworkInputEventV1;
    use detcore_model::network_trace::NetworkOutputV1;
    use detcore_model::network_trace::NetworkReleaseV1;
    use detcore_model::network_trace::NetworkReleaseV2;
    use detcore_model::network_trace::NetworkTransportV1;
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
    fn output_matching_allows_split_but_refuses_first_bad_byte() {
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
        assert!(matches!(
            engine.bind(reused_numeric_fd_but_new_ofd, channel_id()),
            Err(NetworkReplayError::ChannelRetired(id)) if id == channel_id()
        ));
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

    #[test]
    fn record_epoch_is_explicit_or_captured_once_within_call_range() {
        let explicit = epoch();
        assert_eq!(
            NetworkReplayEngine::record_with_optional_epoch(Some(explicit))
                .into_recorded_trace()
                .unwrap()
                .epoch,
            explicit
        );
        let before = Utc::now();
        let captured = NetworkReplayEngine::record_with_optional_epoch(None)
            .into_recorded_trace()
            .unwrap()
            .epoch;
        let after = Utc::now();
        assert!(captured >= before && captured <= after);
    }

    #[test]
    fn released_errors_and_data_keep_trace_order() {
        let mut ordered = trace();
        ordered.outputs.clear();
        ordered.inputs = vec![
            NetworkInputEventV2 {
                ordinal: 0,
                channel: channel_id(),
                release: NetworkReleaseV2 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::SocketError {
                    stream_offset: 0,
                    errno: libc::EAGAIN,
                },
            },
            NetworkInputEventV2 {
                ordinal: 1,
                channel: channel_id(),
                release: NetworkReleaseV2 {
                    not_before_global_time: time(2),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"ready".to_vec(),
                },
            },
        ];
        let mut engine = NetworkReplayEngine::replay(ordered).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        engine.release_eligible(time(2)).unwrap();
        assert_eq!(
            engine.receive_stream(ofd, 10, true).unwrap(),
            StreamReceiveOutcome::Error(libc::EAGAIN)
        );
        assert_eq!(
            engine.receive_stream(ofd, 10, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"ready".to_vec())
        );
    }

    #[test]
    fn readiness_clear_and_unsupported_epoll_modes_fail_closed() {
        let mut cleared = trace();
        cleared.outputs.clear();
        cleared.inputs = vec![
            NetworkInputEventV2 {
                ordinal: 0,
                channel: channel_id(),
                release: NetworkReleaseV2 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::Readiness(NetworkReadinessV2 {
                    readable: true,
                    ..NetworkReadinessV2::default()
                }),
            },
            NetworkInputEventV2 {
                ordinal: 1,
                channel: channel_id(),
                release: NetworkReleaseV2 {
                    not_before_global_time: time(2),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::Readiness(NetworkReadinessV2::default()),
            },
        ];
        let mut engine = NetworkReplayEngine::replay(cleared).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        engine.release_eligible(time(2)).unwrap();
        assert_eq!(
            engine.readiness(ofd).unwrap(),
            NetworkReadinessV2::default()
        );
        assert!(matches!(
            engine.readiness_for_interest(ofd, true, false),
            Err(NetworkReplayError::UnsupportedReadinessMode)
        ));
    }

    #[test]
    fn datagram_peek_preserves_boundary_and_truncation_return_count() {
        let datagram_channel = NetworkChannelId(2);
        let packet = NetworkDatagramV2 {
            sequence: 0,
            bytes: b"packet".to_vec(),
            source: Some(NetworkAddressV2::Inet4 {
                address: [192, 0, 2, 2],
                port: 53,
            }),
            destination: None,
            ancillary: None,
            message_flags: 0,
        };
        let datagram_trace = NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![NetworkChannelV2 {
                id: datagram_channel,
                transport: NetworkTransportV2::Udp,
                role: NetworkEndpointRoleV2::Datagram,
                local_address: None,
                peer_address: None,
                accepted_from: None,
            }],
            outputs: vec![],
            inputs: vec![NetworkInputEventV2 {
                ordinal: 0,
                channel: datagram_channel,
                release: NetworkReleaseV2 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::DatagramExact(NetworkDatagramExactV2 {
                    datagram: packet,
                    source_length: Some(16),
                    destination_length: None,
                }),
            }],
        };
        let mut engine = NetworkReplayEngine::replay(datagram_trace).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, datagram_channel).unwrap();
        engine.release_eligible(time(1)).unwrap();
        let options = NetworkReceiveOptions {
            maximum: 3,
            nonblocking: false,
            flags: libc::MSG_PEEK | libc::MSG_TRUNC,
            receive_low_water: 1,
        };
        let DatagramReceiveOutcome::Datagram(first) =
            engine.receive_datagram_with_options(ofd, options).unwrap()
        else {
            panic!("expected datagram")
        };
        assert_eq!(first.bytes, b"pac");
        assert_eq!(first.return_len, 6);
        assert_eq!(first.source_length, Some(16));
        assert_ne!(first.message_flags & libc::MSG_TRUNC, 0);
        let DatagramReceiveOutcome::Datagram(second) = engine
            .receive_datagram_with_options(
                ofd,
                NetworkReceiveOptions {
                    flags: libc::MSG_TRUNC,
                    ..options
                },
            )
            .unwrap()
        else {
            panic!("expected datagram")
        };
        assert_eq!(second.bytes, b"pac");
        assert_eq!(second.return_len, 6);
    }

    #[test]
    fn stream_ancillary_requires_message_receive_and_is_delivered_once() {
        let ancillary = NetworkAncillaryDataV2 {
            bytes: vec![0; 4],
            objects: vec![detcore_model::network_trace::NetworkAncillaryObjectRefV2 {
                byte_offset: 0,
                object: detcore_model::network_trace::NetworkAncillaryObjectV2::FileDescriptor {
                    object: detcore_model::network_trace::NetworkObjectId(7),
                },
            }],
            truncated: false,
        };
        let mut message = trace();
        message.outputs.clear();
        message.inputs = vec![NetworkInputEventV2 {
            ordinal: 0,
            channel: channel_id(),
            release: NetworkReleaseV2 {
                not_before_global_time: time(1),
                after_transmitted_offset: 0,
            },
            event: NetworkInputKindV2::StreamMessage {
                stream_offset: 0,
                bytes: b"fd".to_vec(),
                ancillary: ancillary.clone(),
                message_flags: 0,
            },
        }];
        let mut engine = NetworkReplayEngine::replay(message).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        engine.release_eligible(time(1)).unwrap();
        assert!(matches!(
            engine.receive_stream(ofd, 1, false),
            Err(NetworkReplayError::AncillaryRequiresMessageIo(_))
        ));
        let StreamMessageReceiveOutcome::Message(first) =
            engine.receive_stream_message(ofd, 1, false, false).unwrap()
        else {
            panic!("expected stream message")
        };
        assert_eq!(first.bytes, b"f");
        assert_eq!(first.ancillary, Some(ancillary));
        let StreamMessageReceiveOutcome::Message(second) =
            engine.receive_stream_message(ofd, 1, false, false).unwrap()
        else {
            panic!("expected stream message")
        };
        assert_eq!(second.bytes, b"d");
        assert_eq!(second.ancillary, None);
    }

    #[test]
    fn stream_receive_models_low_water_waitall_and_peek() {
        let mut input = trace();
        input.outputs.clear();
        input.inputs.truncate(1);
        input.inputs[0].release.after_transmitted_offset = 0;
        let mut engine = NetworkReplayEngine::replay(input).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        engine.release_eligible(time(10)).unwrap();
        let options = NetworkReceiveOptions {
            maximum: 8,
            nonblocking: false,
            flags: libc::MSG_PEEK,
            receive_low_water: 3,
        };
        assert_eq!(
            engine.receive_stream_with_options(ofd, options).unwrap(),
            StreamReceiveOutcome::Bytes(b"response".to_vec())
        );
        assert_eq!(
            engine.receive_stream_with_options(ofd, options).unwrap(),
            StreamReceiveOutcome::Bytes(b"response".to_vec())
        );
        assert_eq!(
            engine
                .receive_stream_with_options(
                    ofd,
                    NetworkReceiveOptions {
                        flags: libc::MSG_WAITALL,
                        ..options
                    },
                )
                .unwrap(),
            StreamReceiveOutcome::Bytes(b"response".to_vec())
        );
    }

    #[test]
    fn output_fragment_boundary_preserves_recorded_short_write() {
        let mut partial = trace();
        partial.inputs.clear();
        partial.outputs = vec![
            NetworkOutputEventV2 {
                channel: channel_id(),
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"ab".to_vec(),
                },
            },
            NetworkOutputEventV2 {
                channel: channel_id(),
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 2,
                    bytes: b"cd".to_vec(),
                },
            },
        ];
        let mut engine = NetworkReplayEngine::replay(partial).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        assert_eq!(
            engine.transmit_stream(ofd, b"abcd").unwrap(),
            StreamTransmitOutcome::Accepted(2)
        );
        assert_eq!(
            engine.transmit_stream(ofd, b"cd").unwrap(),
            StreamTransmitOutcome::Accepted(2)
        );
    }

    #[test]
    fn v1_fixture_replays_through_shared_engine() {
        let ofd = open_file(9);
        let legacy = NetworkTraceV1 {
            epoch: epoch(),
            channels: vec![NetworkChannelV1 {
                id: ofd,
                transport: NetworkTransportV1::Tcp,
                role: NetworkEndpointRoleV1::OutboundClient,
                local_address: NetworkAddressV1::Inet4 {
                    address: [10, 0, 0, 2],
                    port: 40_000,
                },
                peer_address: NetworkAddressV1::Inet4 {
                    address: [192, 0, 2, 1],
                    port: 443,
                },
                created_before_competing_threads: true,
            }],
            inputs: vec![NetworkInputEventV1 {
                ordinal: 0,
                channel: ofd,
                release: NetworkReleaseV1 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 1,
                },
                event: NetworkInputKindV1::InboundBytes {
                    stream_offset: 0,
                    bytes: b"v1".to_vec(),
                },
            }],
            outputs: vec![NetworkOutputV1 {
                channel: ofd,
                stream_offset: 0,
                bytes: b"x".to_vec(),
            }],
        };
        let mut frame = Vec::new();
        legacy.write_framed(&mut frame).unwrap();
        let mut engine = replay_from_reader(Cursor::new(frame)).unwrap();
        let channel = NetworkChannelId(ofd.deterministic_socket_cookie());
        engine.bind(ofd, channel).unwrap();
        engine.transmit_stream(ofd, b"x").unwrap();
        engine.release_eligible(time(1)).unwrap();
        assert_eq!(
            engine.receive_stream(ofd, 2, false).unwrap(),
            StreamReceiveOutcome::Bytes(b"v1".to_vec())
        );
        engine.finish().unwrap();
    }

    #[test]
    fn atomic_publication_refuses_overwrite_and_replacement_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("network.trace");
        let empty = NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![],
            inputs: vec![],
            outputs: vec![],
        };
        NetworkTracePublication::reserve(&destination)
            .unwrap()
            .publish(&empty)
            .unwrap();
        assert!(matches!(
            NetworkTracePublication::reserve(&destination),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists
        ));

        let replacement = directory.path().join("replacement.trace");
        let target = directory.path().join("target");
        std::fs::write(&target, b"untouched").unwrap();
        let publication = NetworkTracePublication::reserve(&replacement).unwrap();
        symlink(&target, &replacement).unwrap();
        assert!(matches!(
            publication.publish(&empty),
            Err(NetworkReplayError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
    }
}
