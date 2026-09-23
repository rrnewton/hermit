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

pub(crate) mod accepted;
use accepted::AcceptedRuntime;
pub use accepted::NetworkAcceptLeaseId;
pub use accepted::NetworkAcceptReservation;
pub use accepted::NetworkAcceptedChild;
pub use accepted::NetworkAcceptedCompletion;
mod fd_mutation;
pub(crate) mod lifetime;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::error::Error;
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

pub use chrono::DateTime;
pub use chrono::Utc;
use detcore_model::fd::FilesId;
use detcore_model::fd::OpenFileId;
use detcore_model::network_trace::ChannelSocketClassV3;
use detcore_model::network_trace::FreshStreamSocketProfileV3;
use detcore_model::network_trace::LinuxReceiveNormalizationV3;
use detcore_model::network_trace::MAX_NETWORK_TRACE_PAYLOAD_BYTES;
use detcore_model::network_trace::NETWORK_TRACE_MAGIC;
use detcore_model::network_trace::NetworkAddressV1;
use detcore_model::network_trace::NetworkAddressV2;
use detcore_model::network_trace::NetworkAncillaryDataV2;
use detcore_model::network_trace::NetworkChannelId;
use detcore_model::network_trace::NetworkChannelV2;
use detcore_model::network_trace::NetworkConnectionResultV2;
use detcore_model::network_trace::NetworkDatagramExactV2;
use detcore_model::network_trace::NetworkDatagramV2;
use detcore_model::network_trace::NetworkEndpointRoleV2;
use detcore_model::network_trace::NetworkInputEventV2;
use detcore_model::network_trace::NetworkInputKindV1;
use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkObjectId;
use detcore_model::network_trace::NetworkOutputEventV2;
use detcore_model::network_trace::NetworkOutputKindV2;
use detcore_model::network_trace::NetworkReadinessV2;
use detcore_model::network_trace::NetworkReleaseV2;
use detcore_model::network_trace::NetworkShutdownV2;
use detcore_model::network_trace::NetworkTrace;
use detcore_model::network_trace::NetworkTraceCodecError;
use detcore_model::network_trace::NetworkTraceV1;
use detcore_model::network_trace::NetworkTraceV2;
use detcore_model::network_trace::NetworkTraceV3;
use detcore_model::network_trace::NetworkTraceValidationError;
use detcore_model::network_trace::NetworkTransportV2;
use detcore_model::network_trace::ReceiveCopyUnitV1;
use detcore_model::network_trace::ReceiveEnvironmentV3;
use detcore_model::network_trace::ReceiveModelV1;
use detcore_model::network_trace::StreamSocketKeyV3;
use detcore_model::network_trace::StreamSocketOptionsV3;
use detcore_model::time::LogicalTime;
use fd_mutation::FdLifecycleState;
pub use fd_mutation::NetworkFdMutationAdmission;
pub use fd_mutation::NetworkFdMutationBegin;
pub use fd_mutation::NetworkFdMutationKind;
pub use fd_mutation::NetworkFdMutationReply;
pub use fd_mutation::NetworkFdMutationRequest;
pub(crate) use fd_mutation::backend_fd_table_capability;
use lifetime::NetworkLifetime;
use lifetime::SlotInstallationSource;
use lifetime::SlotPublicationBatch;
use lifetime::SlotPublicationEntry;
use lifetime::SlotPublicationResult;
use lifetime::TaskOwner;
use serde::Deserialize;
use serde::Serialize;

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

/// Kernel-object category represented by one trace-stable ancillary object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAncillaryObjectKind {
    /// Open file description transferred through `SCM_RIGHTS`.
    FileDescriptor,
}

/// Resolved ancillary object used by the adapter to install a guest fd alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkAncillaryObject {
    /// Trace-stable object identity.
    pub id: NetworkObjectId,
    /// Stable Detcore open-file-description identity.
    pub open_file: OpenFileId,
    /// Object category.
    pub kind: NetworkAncillaryObjectKind,
    /// Number of currently installed descriptor aliases.
    pub alias_count: u64,
}

/// One deterministic epoll result, ordered by target OFD identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetworkEpollEvent {
    /// Ready target open-file description.
    pub target: OpenFileId,
    /// Linux `EPOLL*` result bits.
    pub events: u32,
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

/// Endpoint facts supplied when a runtime OFD needs a trace channel.
///
/// The trace identity is never chosen from an OFD, thread or socket creation
/// ordinal. An unknown requested local bind is not an observed ephemeral port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkChannelBinding {
    /// Actual socket transport, established by the adapter's socket queries.
    pub transport: NetworkTransportV2,
    /// Connection role; accepted sockets also require listener ancestry.
    pub role: NetworkEndpointRoleV2,
    /// Exact requested/accepted peer, including IPv6 and Unix address fields.
    pub peer_address: Option<NetworkAddressV2>,
    /// A known exact local endpoint constraint, never inferred from an
    /// automatically assigned address. This is not wildcard/port-zero bind
    /// semantics. None means unknown/unconstrained here; this slice does not
    /// claim to intercept or model successful bind inputs.
    pub requested_local_constraint: Option<NetworkAddressV2>,
    /// Separately observed local metadata, for creation during Record only.
    pub observed_local_address: Option<NetworkAddressV2>,
    /// Exact trace listener which produced this accepted connection.
    pub accepted_from: Option<NetworkChannelId>,
    /// Replay accept selects the channel named by its actual released Accept
    /// event. Other roles use endpoint matching in stored occurrence order.
    pub selected_channel: Option<NetworkChannelId>,
}

/// Bounded RPC view, not a limit on a guest's receive length.
pub const NETWORK_STREAM_CHUNK_LIMIT: usize = 512;

/// Backend-authenticated task incarnation which owns a physical operation.
/// This identity never participates in trace channel matching or replay order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NetworkStreamOwner {
    /// Backend-authenticated logical task identifier.
    pub thread: detcore_model::pid::DetTid,
    /// Address-space incarnation carried by that task's ThreadState.
    pub mm: detcore_model::futex::MmId,
}

/// Opaque, checked, run-wide operation identity, never reused after completion.
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
pub struct NetworkStreamLeaseId(u64);

/// A result from a positive-capacity nonblocking receive into owned scratch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkIngressObservation {
    /// Nonempty bytes recovered from the submitted receive's owned scratch.
    Bytes(Vec<u8>),
    /// Zero returned by a positive-capacity receive on a connected stream.
    EndOfFile,
    /// The submitted nonblocking receive actually returned EAGAIN.
    NoArrival,
    /// The submitted receive returned EINTR without transferring any bytes.
    Interrupted,
    /// Positive transport-origin errno; consumer-local errors are refused.
    TransportError(i32),
}

/// Queue facts; terminal outcomes may follow buffered payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStreamQueueStatus {
    /// Contiguous payload length preceding the first terminal/control outcome.
    pub queued_bytes: usize,
    /// Peer write shutdown is known, possibly following buffered payload.
    pub eof: bool,
    /// Guest-local SHUT_RD/Both succeeded; later ingress remains possible.
    pub local_read_shutdown: bool,
    /// Atomic runtime readiness, retaining the legacy wire representation.
    pub readiness: NetworkReadinessV2,
    /// First pending transport error, possibly following buffered payload.
    pub error: Option<i32>,
    /// This endpoint is a listener and must never be harvested as a data stream.
    pub listener: bool,
    /// A submitted physical receive currently owns the ingress direction.
    pub ingress_busy: bool,
    /// A consumer holds an immutable selection pending acknowledgement.
    pub delivery_busy: bool,
    /// Successful logical byte consumption epoch, shared by all aliases.
    pub consume_epoch: u64,
}

/// Immutable proposed guest outcome, awaiting explicit completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamChunkOutcome {
    /// First bounded view of the selected payload unit, at most 512 bytes.
    Bytes(Vec<u8>),
    /// Reserved peer write shutdown after the preceding payload.
    EndOfFile,
    /// Reserved positive transport errno at the current stream offset.
    Error(i32),
}

/// A data or terminal observation must retain its receipt until acknowledged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamChunk {
    /// Immutable selection retained until one explicit finish acknowledgement.
    Reserved {
        /// Opaque receipt bound to the authenticated consumer incarnation.
        lease: NetworkStreamLeaseId,
        /// Entire selected publication unit, not just the first RPC view.
        selection_len: usize,
        /// First bounded payload view or the selected terminal outcome.
        outcome: NetworkStreamChunkOutcome,
    },
    /// No currently available payload or terminal outcome was reserved.
    Empty,
    /// The selected receive position is empty after local SHUT_RD. No queue
    /// outcome is reserved or consumed; a later arrival remains readable.
    LocalReadClosed,
}

/// Known outcome of the current bounded guest copy. This vocabulary does not
/// assert that an ingress fragment is a Linux SKB; that adapter obligation is
/// separately qualified before claiming general TCP fault compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamChunkDisposition {
    /// The entire selected payload or exact terminal outcome was accepted.
    Consumed,
    /// Payload or EOF was observed without consuming the selected unit.
    Peeked,
    /// Current-unit copy failed; guest memory may have changed, but it stays queued.
    CopyFailed,
}

#[derive(Debug, Clone)]
struct StreamOperation {
    owner: NetworkStreamOwner,
    open_file: OpenFileId,
    channel: NetworkChannelId,
    abandoned: bool,
    kind: StreamOperationKind,
}

#[derive(Debug, Clone)]
enum StreamOperationKind {
    /// Written before kernel submission. Missing completion never means zero
    /// effects, including cancellation before the caller can learn the result.
    SubmittedIngress { began: LogicalTime },
    Delivery {
        at_offset: u64,
        peek_offset: usize,
        selection_len: usize,
        outcome: NetworkStreamChunkOutcome,
    },
}

/// One active guest syscall's OFD reference, independent of short exclusion.
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
pub struct NetworkStreamCallId(u64);

#[cfg(test)]
impl NetworkStreamCallId {
    pub(crate) fn controlled_fixture(value: u64) -> Self {
        Self(value)
    }
}

/// Call reference allocated before the physical OFD pin acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStreamCall {
    /// Opaque checked identity owned by the authenticated task/MM.
    pub id: NetworkStreamCallId,
    /// Exact captured OFD; never resolved again using a recycled descriptor.
    pub open_file: OpenFileId,
    /// Record needs a backend-authenticated physical pin; Replay is logical.
    pub physical_pin_required: bool,
}

/// A known result from the just-submitted physical pin acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamPinOutcome {
    /// The adapter owns the exact pinned host handle until release completes.
    Acquired,
    /// Acquisition completed unsuccessfully; no handle was installed.
    Failed(i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCallPhase {
    PinAcquireSubmitted,
    Active,
    PinReleaseSubmitted,
}

#[derive(Debug, Clone)]
struct StreamCallState {
    owner: NetworkStreamOwner,
    open_file: OpenFileId,
    physical_pin_required: bool,
    phase: StreamCallPhase,
    abandoned: bool,
}

/// Atomic zero-capacity recv result. No payload reservation or drain is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkZeroStreamReceive {
    /// At least one byte is available at the requested peek position; return 0.
    Ready,
    /// Read-side shutdown is observable; return 0 without consuming payload.
    EndOfFile,
    /// A pending error was consumed atomically, even after a nonzero peek offset.
    Error(i32),
    /// No data or terminal state is available; apply blocking/timeout semantics.
    Empty,
    /// The empty decision and input-generation snapshot share one atomic receipt.
    Waiting(NetworkZeroStreamWaitId),
}

impl NetworkReplayEngine {
    fn owned_stream_call(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<&StreamCallState, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let state = self
            .stream_calls
            .get(&call)
            .ok_or(NetworkReplayError::UnknownStreamCall(call))?;
        if state.owner != owner {
            return Err(NetworkReplayError::StreamCallOwnerMismatch(call));
        }
        if state.abandoned {
            return Err(NetworkReplayError::UnresolvedStreamCall(call));
        }
        Ok(state)
    }

    /// The caller holds the short OFD control and has revalidated fd->OFD.
    /// Record latches possible host handle acquisition before pidfd_getfd.
    /// The actual backend, not a caller-supplied PID, authenticates that pin.
    pub fn begin_stream_call(
        &mut self,
        owner: NetworkStreamOwner,
        control_lease: NetworkStreamLeaseId,
    ) -> Result<NetworkStreamCall, NetworkReplayError> {
        let control = self.owned_socket_control(owner, control_lease)?;
        if !control.physical.can_release_unchanged() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(control_lease));
        }
        let open_file = control.open_file;
        if self.retired_open_files.contains(&open_file) {
            return Err(NetworkReplayError::OpenFileRetired(open_file));
        }
        let next = self
            .next_stream_call
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let id = NetworkStreamCallId(self.next_stream_call);
        let physical_pin_required = matches!(self.mode, EngineState::Record(_));
        let phase = if physical_pin_required {
            StreamCallPhase::PinAcquireSubmitted
        } else {
            StreamCallPhase::Active
        };
        self.retain_stream_call_lifetime(owner, id, open_file)?;
        self.next_stream_call = next;
        self.stream_calls.insert(
            id,
            StreamCallState {
                owner,
                open_file,
                physical_pin_required,
                phase,
                abandoned: false,
            },
        );
        Ok(NetworkStreamCall {
            id,
            open_file,
            physical_pin_required,
        })
    }

    /// Unknown acquisition leaves PinAcquireSubmitted intact. A known failed
    /// acquisition drops only this semantic reference; it cannot validate a
    /// replacement descriptor or resolve another receipt.
    pub fn confirm_stream_call_pin(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        outcome: NetworkStreamPinOutcome,
    ) -> Result<(), NetworkReplayError> {
        let state = self.owned_stream_call(owner, call)?;
        if state.phase != StreamCallPhase::PinAcquireSubmitted || !state.physical_pin_required {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        match outcome {
            NetworkStreamPinOutcome::Acquired => {
                self.stream_calls
                    .get_mut(&call)
                    .expect("validated call")
                    .phase = StreamCallPhase::Active;
            }
            NetworkStreamPinOutcome::Failed(errno) => {
                if !(1..=4095).contains(&errno) {
                    return Err(NetworkTraceValidationError::InvalidErrno.into());
                }
                let open_file = state.open_file;
                self.release_stream_call_lifetime(owner, call, open_file)?;
                self.stream_calls.remove(&call);
                self.complete_deferred_retirement(open_file);
            }
        }
        Ok(())
    }

    /// Resolve an active call to its captured OFD. Guest descriptor reuse does
    /// not change this mapping; a retired alias can still have a live syscall.
    pub fn stream_call_open_file(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<OpenFileId, NetworkReplayError> {
        let state = self.owned_stream_call(owner, call)?;
        if state.phase != StreamCallPhase::Active {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        Ok(state.open_file)
    }

    /// Durable release intent precedes closing the recorder's owned physical
    /// reference. The adapter must not claim release from future destruction.
    pub fn begin_stream_call_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        self.check_accept_call_release(call)?;
        let state = self.owned_stream_call(owner, call)?;
        if state.phase != StreamCallPhase::Active {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        let open_file = state.open_file;
        if let Some((id, _)) = self
            .zero_stream_waits
            .iter()
            .find(|(_, wait)| wait.call == call)
        {
            return Err(NetworkReplayError::UnresolvedZeroStreamWait(*id));
        }
        if let Some(control) = self
            .socket_controls
            .get(&open_file)
            .filter(|control| control.owner == owner)
        {
            return Err(NetworkReplayError::StreamOperationBusy(control.lease));
        }
        if let Some((lease, _)) = self
            .stream_operations
            .iter()
            .find(|(_, operation)| operation.owner == owner && operation.open_file == open_file)
        {
            return Err(NetworkReplayError::StreamOperationBusy(*lease));
        }
        self.stream_calls
            .get_mut(&call)
            .expect("validated call")
            .phase = StreamCallPhase::PinReleaseSubmitted;
        Ok(())
    }

    /// Acknowledges known release, then removes the logical call reference.
    /// If the final descriptor alias closed earlier, retire only after every
    /// active call and short operation has completed. Unknown close keeps the
    /// submitted release and forbids successful trace finalization.
    pub fn finish_stream_call_release(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<(), NetworkReplayError> {
        let state = self.owned_stream_call(owner, call)?;
        if state.phase != StreamCallPhase::PinReleaseSubmitted {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        let open_file = state.open_file;
        self.release_stream_call_lifetime(owner, call, open_file)?;
        self.stream_calls.remove(&call);
        self.complete_deferred_retirement(open_file);
        Ok(())
    }

    fn has_stream_references(&self, open_file: OpenFileId) -> bool {
        self.stream_calls
            .values()
            .any(|call| call.open_file == open_file)
            || self.socket_controls.contains_key(&open_file)
            || self
                .stream_operations
                .values()
                .any(|operation| operation.open_file == open_file)
    }

    fn complete_deferred_retirement(&mut self, open_file: OpenFileId) -> Option<NetworkChannelId> {
        if !self.retired_open_files.contains(&open_file) || self.has_stream_references(open_file) {
            return None;
        }
        self.epolls.remove(&open_file);
        for interests in self.epolls.values_mut() {
            interests.remove(&open_file);
        }
        let channel = self.bindings.remove(&open_file);
        if let Some(channel) = channel {
            self.reverse_bindings.remove(&channel);
            self.retired_channels.insert(channel);
        }
        channel
    }

    /// Atomic availability/error decision for recv(0), separate from read(0).
    /// Data remains queued and no delivery receipt is created. This entry must
    /// be admitted through the same short exclusion as nonzero receives.
    pub fn zero_stream_receive(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        peek_offset: usize,
    ) -> Result<NetworkZeroStreamReceive, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        self.check_socket_control_available(open_file)?;
        self.check_unreserved_stream_delivery(open_file)?;
        let channel = self.bound_channel(open_file)?;
        let state = self.channels.get(&channel).expect("bound channel exists");
        if state.transport.is_datagram()
            || self.stream_role(channel)? == NetworkEndpointRoleV2::Listener
        {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let mut skip = peek_offset;
        let mut error_index = None;
        for (index, item) in state.inbound.iter().enumerate() {
            match item {
                InboundOutcome::Stream {
                    bytes,
                    requires_message_io,
                    ..
                } => {
                    if *requires_message_io {
                        return Err(NetworkReplayError::AncillaryRequiresMessageIo(channel));
                    }
                    if skip < bytes.len() {
                        return Ok(NetworkZeroStreamReceive::Ready);
                    }
                    skip -= bytes.len();
                }
                InboundOutcome::Error { errno, .. } => {
                    error_index = Some((index, *errno));
                    break;
                }
                InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                } => return Ok(NetworkZeroStreamReceive::EndOfFile),
                InboundOutcome::Control(_) | InboundOutcome::Datagram { .. } => {
                    return Err(NetworkReplayError::OperationOrderMismatch(channel));
                }
                _ => break,
            }
        }
        if let Some((index, errno)) = error_index {
            let state = self.channels.get_mut(&channel).expect("validated channel");
            state.inbound.remove(index);
            state.refresh_readiness();
            return Ok(NetworkZeroStreamReceive::Error(errno));
        }
        Ok(if state.peer_write_closed || state.local_read_shutdown {
            NetworkZeroStreamReceive::EndOfFile
        } else {
            NetworkZeroStreamReceive::Empty
        })
    }
}

/// Descriptor-close operations have different failure ownership semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkDescriptorEffect {
    /// Linux close releases the descriptor before most later error reports.
    CloseDescriptor,
    /// A failed dup2/dup3 replacement leaves its target descriptor installed.
    ReplaceDescriptor,
}

/// Snapshot admitted while exact OFD control exclusion is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSocketControl {
    /// Authenticated short exclusion receipt.
    pub lease: NetworkStreamLeaseId,
    /// First modeled pending error; physical hard/soft reconciliation is separate.
    pub pending_error: Option<i32>,
    /// Actual registered options, absent for a legacy unenrolled socket.
    pub options: Option<StreamSocketOptionsV3>,
    /// Successful logical byte consumption epoch.
    pub consume_epoch: u64,
}

/// Explicit end of a short socket control, separate from long call references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkSocketControlFinish {
    /// SO_ERROR read completion; rejected until matched error authority joins.
    ErrorTaken,
    /// No descriptor was released and no effect remains pending.
    Unchanged,
    /// The exact descriptor release completed and local ownership reconciled.
    Closed {
        /// Semantic descriptor-table result; never inferred from host Arc count.
        last_alias: bool,
    },
}

#[derive(Debug, Clone, Default)]
struct SocketControlPhysical {
    pending: Option<NetworkDescriptorEffect>,
    descriptor_released: Option<bool>,
    option_pending: Option<(
        NetworkStreamSocketOption,
        Result<StreamSocketOptionsV3, i32>,
    )>,
    shutdown_pending: Option<NetworkShutdownV2>,
}

impl SocketControlPhysical {
    fn can_release_unchanged(&self) -> bool {
        self.pending.is_none()
            && self.option_pending.is_none()
            && self.shutdown_pending.is_none()
            && self.descriptor_released != Some(true)
    }
}

#[derive(Debug, Clone)]
struct SocketControlState {
    lease: NetworkStreamLeaseId,
    owner: NetworkStreamOwner,
    open_file: OpenFileId,
    abandoned: bool,
    physical: SocketControlPhysical,
}

impl NetworkReplayEngine {
    fn owned_socket_control(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<&SocketControlState, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let control = self
            .socket_controls
            .values()
            .find(|control| control.lease == lease)
            .ok_or(NetworkReplayError::UnknownStreamLease(lease))?;
        if control.owner != owner {
            return Err(NetworkReplayError::StreamLeaseOwnerMismatch(lease));
        }
        if control.abandoned {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        Ok(control)
    }

    fn check_socket_control_available(
        &self,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if let Some(control) = self.socket_controls.get(&open_file) {
            return Err(if control.abandoned {
                NetworkReplayError::UnresolvedStreamOperation(control.lease)
            } else {
                NetworkReplayError::StreamOperationBusy(control.lease)
            });
        }
        Ok(())
    }

    /// Acquire the unique sorted set atomically. Failure installs no lease and
    /// advances no identifier; the global wrapper may then subscribe/recheck
    /// with none held. Unbound enrolled sockets can use this before connect.
    pub fn begin_socket_controls(
        &mut self,
        owner: NetworkStreamOwner,
        open_files: Vec<OpenFileId>,
    ) -> Result<Vec<(OpenFileId, NetworkStreamLeaseId)>, NetworkReplayError> {
        self.begin_socket_controls_inner(owner, open_files, None)
    }

    /// Reacquire only short exclusion for an already active syscall, even after
    /// the last descriptor closed. The call, not a bare OFD, authorizes this.
    pub fn begin_stream_call_control(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NetworkStreamLeaseId, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        let controls = self.begin_socket_controls_inner(owner, vec![open_file], Some(open_file))?;
        Ok(controls[0].1)
    }

    fn begin_socket_controls_inner(
        &mut self,
        owner: NetworkStreamOwner,
        open_files: Vec<OpenFileId>,
        retired_call_file: Option<OpenFileId>,
    ) -> Result<Vec<(OpenFileId, NetworkStreamLeaseId)>, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let files: BTreeSet<_> = open_files.into_iter().collect();
        for open_file in &files {
            if !open_file.is_socket() {
                return Err(NetworkReplayError::NonSocketOpenFile(*open_file));
            }
            if self.retired_open_files.contains(open_file) && retired_call_file != Some(*open_file)
            {
                return Err(NetworkReplayError::OpenFileRetired(*open_file));
            }
            self.check_socket_control_available(*open_file)?;
            if let Some(operation) = self
                .stream_operations
                .iter()
                .find(|(_, operation)| operation.open_file == *open_file)
            {
                return Err(if operation.1.abandoned {
                    NetworkReplayError::UnresolvedStreamOperation(*operation.0)
                } else {
                    NetworkReplayError::StreamOperationBusy(*operation.0)
                });
            }
        }
        let count = u64::try_from(files.len()).map_err(|_| NetworkReplayError::Overflow)?;
        let next = self
            .next_stream_lease
            .checked_add(count)
            .ok_or(NetworkReplayError::Overflow)?;
        let start = self.next_stream_lease;
        let controls = files
            .into_iter()
            .enumerate()
            .map(|(offset, open_file)| (open_file, NetworkStreamLeaseId(start + offset as u64)))
            .collect::<Vec<_>>();
        self.next_stream_lease = next;
        for &(open_file, lease) in &controls {
            self.socket_controls.insert(
                open_file,
                SocketControlState {
                    lease,
                    owner,
                    open_file,
                    abandoned: false,
                    physical: SocketControlPhysical::default(),
                },
            );
        }
        Ok(controls)
    }

    /// Persist the exact descriptor-effect kind before kernel submission.
    pub fn submit_descriptor_effect(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: NetworkDescriptorEffect,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        if control.physical.pending.is_some()
            || control.physical.descriptor_released.is_some()
            || control.physical.shutdown_pending.is_some()
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = control.open_file;
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical
            .pending = Some(effect);
        Ok(())
    }

    /// Match one known close/replace result. Missing completion remains durable.
    /// ERESTARTSYS is retained as the existing adapter's restart/no-release
    /// outcome; EINTR/EIO are not confused with a failed dup replacement.
    pub fn confirm_descriptor_effect(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        result: Result<(), i32>,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let Some(effect) = control.physical.pending else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if let Err(errno) = result
            && !(1..=4095).contains(&errno)
        {
            return Err(NetworkTraceValidationError::InvalidErrno.into());
        }
        let released = match (effect, result) {
            (_, Ok(())) => true,
            (NetworkDescriptorEffect::ReplaceDescriptor, Err(_)) => false,
            (NetworkDescriptorEffect::CloseDescriptor, Err(errno)) => {
                errno != libc::EBADF && errno != reverie::Errno::ERESTARTSYS.into_raw()
            }
        };
        let open_file = control.open_file;
        let physical = &mut self
            .socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical;
        physical.pending = None;
        physical.descriptor_released = Some(released);
        Ok(())
    }

    /// Publish final descriptor retirement before releasing exclusion. The
    /// binding/queue remain pinned by any active syscall references.
    pub fn finish_socket_control(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        disposition: NetworkSocketControlFinish,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        if self.shadow_probes.contains_key(&lease) {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        match disposition {
            NetworkSocketControlFinish::ErrorTaken => {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            NetworkSocketControlFinish::Unchanged if !control.physical.can_release_unchanged() => {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            NetworkSocketControlFinish::Closed { .. }
                if control.physical.pending.is_some()
                    || control.physical.descriptor_released != Some(true) =>
            {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            _ => {}
        }
        if matches!(
            disposition,
            NetworkSocketControlFinish::Closed { last_alias: true }
        ) {
            self.retired_open_files.insert(open_file);
        }
        self.socket_controls.remove(&open_file);
        self.complete_deferred_retirement(open_file);
        Ok(())
    }
}

/// Run-local namespace proof. Host inode numbers never become trace identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStreamNamespace {
    /// Namespace handle's st_dev from the authenticated backend task.
    pub device: u64,
    /// Namespace handle's st_ino, pinned and rechecked by the adapter.
    pub inode: u64,
}

/// Actual receive state of one enrolled OFD, available before channel binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkStreamSocketState {
    /// Actual class, including protocol rather than the socket(2) shorthand 0.
    pub key: StreamSocketKeyV3,
    /// Recorded normalization facts, never queried from a Replay placeholder.
    pub normalization: LinuxReceiveNormalizationV3,
    /// Current semantic options shared by every alias.
    pub options: StreamSocketOptionsV3,
    /// Successful consume generation; a PEEK caller can detect another reader.
    pub consume_epoch: u64,
    /// Explicitly modeled only in the appended accepted-child receive variant.
    pub send_timeout: Option<detcore_model::network_trace::ReceiveTimeoutV3>,
    /// Successful inheritable option commits, never host time or accepter order.
    pub option_generation: u64,
}

#[derive(Debug)]
struct ShadowReceiveState {
    environment: ReceiveEnvironmentV3,
    namespace: Option<NetworkStreamNamespace>,
    profiles: BTreeMap<StreamSocketKeyV3, FreshStreamSocketProfileV3>,
    channel_classes: BTreeMap<NetworkChannelId, StreamSocketKeyV3>,
    sockets: BTreeMap<OpenFileId, NetworkStreamSocketState>,
    units: Vec<ReceiveCopyUnitV1>,
    accepted: Option<AcceptedRuntime>,
}

impl NetworkReplayEngine {
    /// Construct the explicitly selected V3 recorder. Legacy record() remains
    /// V2 and cannot acquire declared copy units by implicit upgrade.
    pub fn record_shadow(epoch: DateTime<Utc>) -> Self {
        let mut engine = Self::record(epoch);
        engine.shadow = Some(ShadowReceiveState {
            environment: ReceiveEnvironmentV3::SingleRecorderNamespaceV1,
            namespace: None,
            profiles: BTreeMap::new(),
            channel_classes: BTreeMap::new(),
            sockets: BTreeMap::new(),
            units: Vec::new(),
            accepted: None,
        });
        engine
    }

    /// Validate the complete V3 frame before creating any runtime state.
    pub fn replay_shadow(trace: NetworkTraceV3) -> Result<Self, NetworkReplayError> {
        trace.validate()?;
        let NetworkTraceV3 {
            history,
            receive_model,
            fresh_stream_profiles,
            receive_environment,
            channel_socket_classes,
        } = trace;
        let (units, accepted) = match receive_model {
            ReceiveModelV1::DeclaredCopyUnitsV1 { units } => (units, None),
            ReceiveModelV1::DeclaredCopyUnitsWithAcceptV2 { units, accepted } => {
                (units, Some(AcceptedRuntime::replay(accepted)))
            }
        };
        let mut engine = Self::replay(history)?;
        engine.shadow = Some(ShadowReceiveState {
            environment: receive_environment,
            namespace: None,
            profiles: fresh_stream_profiles
                .into_iter()
                .map(|profile| (profile.key, profile))
                .collect(),
            channel_classes: channel_socket_classes
                .into_iter()
                .map(|class| (class.channel, class.key))
                .collect(),
            sockets: BTreeMap::new(),
            units,
            accepted,
        });
        Ok(engine)
    }

    /// The adapter must dispatch legacy and V3 receive paths explicitly.
    pub fn shadow_mode(&self) -> bool {
        self.shadow.is_some()
    }

    /// Enroll an actual fresh TCP socket before any guest option mutation.
    /// Profile and namespace comparisons precede every state mutation. The
    /// adapter supplies pinned backend namespace evidence, not endpoint guesses.
    pub fn register_stream_socket(
        &mut self,
        open_file: OpenFileId,
        key: StreamSocketKeyV3,
        namespace: NetworkStreamNamespace,
        observed_profile: Option<FreshStreamSocketProfileV3>,
    ) -> Result<NetworkStreamSocketState, NetworkReplayError> {
        if !open_file.is_socket() {
            return Err(NetworkReplayError::NonSocketOpenFile(open_file));
        }
        if self.retired_open_files.contains(&open_file) {
            return Err(NetworkReplayError::OpenFileRetired(open_file));
        }
        let shadow = self.shadow.as_ref().ok_or(NetworkReplayError::WrongMode)?;
        if namespace.inode == 0 || shadow.namespace.is_some_and(|first| first != namespace) {
            return Err(NetworkReplayError::StreamNamespaceMismatch);
        }
        let profile = match (&self.mode, observed_profile) {
            (EngineState::Record(_), Some(profile)) => {
                profile
                    .validate()
                    .map_err(|_| NetworkReplayError::StreamProfileMismatch(key))?;
                if profile.key != key
                    || shadow
                        .profiles
                        .get(&key)
                        .is_some_and(|first| *first != profile)
                {
                    return Err(NetworkReplayError::StreamProfileMismatch(key));
                }
                profile
            }
            (EngineState::Replay(_), None) => shadow
                .profiles
                .get(&key)
                .cloned()
                .ok_or(NetworkReplayError::StreamProfileMismatch(key))?,
            _ => return Err(NetworkReplayError::WrongMode),
        };
        if shadow
            .accepted
            .as_ref()
            .is_some_and(|accepted| !accepted.fresh_send.contains_key(&key))
        {
            return Err(NetworkReplayError::StreamProfileMismatch(key));
        }
        let state = NetworkStreamSocketState {
            key,
            normalization: profile.normalization,
            options: profile.initial.clone(),
            consume_epoch: 0,
            send_timeout: shadow
                .accepted
                .as_ref()
                .and_then(|accepted| accepted.fresh_send.get(&key).copied()),
            option_generation: 0,
        };
        if let Some(prior) = shadow.sockets.get(&open_file) {
            // Re-enrollment must never reset guest-mutated options or cursor.
            return if *prior == state {
                Ok(prior.clone())
            } else {
                Err(NetworkReplayError::StreamProfileMismatch(key))
            };
        }
        let shadow = self.shadow.as_mut().expect("validated shadow state");
        shadow.namespace.get_or_insert(namespace);
        shadow.profiles.entry(key).or_insert(profile);
        shadow.sockets.insert(open_file, state.clone());
        Ok(state)
    }

    /// Bare-OFD queries cannot resurrect a final retired descriptor.
    pub fn stream_socket_state(
        &self,
        open_file: OpenFileId,
    ) -> Result<Option<NetworkStreamSocketState>, NetworkReplayError> {
        if self.retired_open_files.contains(&open_file) {
            return Err(NetworkReplayError::OpenFileRetired(open_file));
        }
        Ok(self
            .shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&open_file))
            .cloned())
    }

    /// An admitted syscall's reference remains usable after final alias close.
    pub fn stream_call_socket_state(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NetworkStreamSocketState, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        self.shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&open_file))
            .cloned()
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))
    }

    fn shadow_channel_matches(&self, open_file: OpenFileId, channel: NetworkChannelId) -> bool {
        let Some(shadow) = &self.shadow else {
            return true;
        };
        match shadow.sockets.get(&open_file) {
            Some(socket) => shadow.channel_classes.get(&channel) == Some(&socket.key),
            None => !shadow.channel_classes.contains_key(&channel),
        }
    }

    fn check_shadow_channel_request(
        &self,
        open_file: OpenFileId,
        request: &NetworkChannelBinding,
    ) -> Result<(), NetworkReplayError> {
        let Some(shadow) = &self.shadow else {
            return Ok(());
        };
        if request.transport != NetworkTransportV2::Tcp {
            return Ok(());
        }
        let socket = shadow
            .sockets
            .get(&open_file)
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?;
        if socket.key.transport != request.transport {
            return Err(NetworkReplayError::StreamProfileMismatch(socket.key));
        }
        for address in [
            &request.peer_address,
            &request.observed_local_address,
            &request.requested_local_constraint,
        ]
        .into_iter()
        .flatten()
        {
            let domain = match address {
                NetworkAddressV2::Inet4 { .. } => libc::AF_INET,
                NetworkAddressV2::Inet6 { .. } => libc::AF_INET6,
                _ => return Err(NetworkReplayError::StreamProfileMismatch(socket.key)),
            };
            if domain != socket.key.domain {
                return Err(NetworkReplayError::StreamProfileMismatch(socket.key));
            }
        }
        Ok(())
    }

    /// New V3 poll terminal condition: peer write EOF is RDHUP/read readiness,
    /// never evidence of the full POLLHUP bit. Physical capture owns that bit.
    pub fn terminal_readiness(&self, open_file: OpenFileId) -> Result<bool, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
        let local_hangup = self.shadow.as_ref().is_some_and(|shadow| {
            shadow.channel_classes.contains_key(&channel)
                && state.local_write_closed
                && state.receive_half_closed()
        });
        Ok(local_hangup
            || state.explicit_readiness.hangup
            || state.explicit_readiness.error
            || state
                .inbound
                .iter()
                .any(|item| matches!(item, InboundOutcome::Error { .. })))
    }

    /// Finalize the explicitly selected frame; unresolved ownership always
    /// fails before the output envelope is constructed.
    pub fn into_recorded_versioned_trace(self) -> Result<NetworkTrace, NetworkReplayError> {
        self.check_stream_operations_finished()?;
        let EngineState::Record(history) = self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        match self.shadow {
            None => {
                history.validate()?;
                Ok(NetworkTrace::V2(history))
            }
            Some(shadow) => {
                let trace = NetworkTraceV3 {
                    history,
                    receive_model: match shadow.accepted {
                        None => ReceiveModelV1::DeclaredCopyUnitsV1 {
                            units: shadow.units,
                        },
                        Some(accepted) => ReceiveModelV1::DeclaredCopyUnitsWithAcceptV2 {
                            units: shadow.units,
                            accepted: accepted.into_model()?,
                        },
                    },
                    fresh_stream_profiles: shadow.profiles.into_values().collect(),
                    receive_environment: shadow.environment,
                    channel_socket_classes: shadow
                        .channel_classes
                        .into_iter()
                        .map(|(channel, key)| ChannelSocketClassV3 { channel, key })
                        .collect(),
                };
                trace.validate()?;
                Ok(NetworkTrace::V3(trace))
            }
        }
    }
}

/// One nonconsuming physical observation owns short exclusion, not the call pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkShadowProbe {
    /// Exact receipt, authenticated to the current task/MM and active call.
    pub lease: NetworkStreamLeaseId,
    /// Published bytes still physically present, derived from P minus C.
    pub retained_prefix: usize,
    /// Absolute published input frontier at acquisition, independent of readers.
    pub captured_through: u64,
    /// Local SHUT_RD already accounts for physical zero/RDHUP observations.
    pub local_read_shutdown: bool,
}

/// Host operations whose completion cannot be inferred from future destruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamPhysicalEffect {
    /// Physical in Record, modeled from the recorded environment in Replay.
    SetSocketOption {
        /// Raw option value whose normalization is owned by the engine.
        option: NetworkStreamSocketOption,
    },
    /// Consuming SO_ERROR observation; requires the separate control protocol.
    ReadSocketError,
    /// Observe the actual shared cursor before changing it for a probe.
    ReadPeekOffset,
    /// Temporarily disable/restore the cursor, or commit a selected guest PEEK.
    SetPeekOffset {
        /// Exact shared cursor value required by the receipt.
        value: i32,
    },
    /// Nonconsuming full-prefix plus one bounded publication-unit observation.
    Peek {
        /// Bounded physical transfer capacity.
        maximum: usize,
    },
    /// Actual poll0 after this receipt's PEEK.
    PollState,
    /// Actual FIONREAD after this receipt's poll0.
    QueuedBytes,
    /// Exact successful-copy selection drain, at most 512 bytes per view.
    Drain {
        /// Bounded physical transfer capacity.
        maximum: usize,
    },
    /// Guest-local shutdown submitted under the same OFD control.
    Shutdown {
        /// Exact requested Linux shutdown direction.
        direction: NetworkShutdownV2,
    },
}

/// Typed raw Linux option values, after ABI length and user access validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamSocketOption {
    /// Shared signed cursor, including negative disabled values.
    PeekOffset(i32),
    /// Raw signed low-water request.
    ReceiveLowWater(i32),
    /// Raw signed timeval; a negative seconds value is finite zero timeout.
    ReceiveTimeout {
        /// Original signed seconds.
        seconds: i64,
        /// Original signed microseconds, validated before seconds.
        microseconds: i64,
    },
    /// Explicit accepted-model send timeout; old receive traces retain fallback.
    SendTimeout {
        /// Signed seconds, including finite-zero negative values.
        seconds: i64,
        /// Microseconds, validated before the seconds sign.
        microseconds: i64,
    },
    /// Raw normal receive-buffer request.
    ReceiveBuffer(i32),
    /// Privileged request; still needs authenticated capability authority.
    ForcedReceiveBuffer(i32),
    /// Other options are not implied irrelevant to receive state.
    Other {
        /// Linux option protocol level.
        level: i32,
        /// Linux option identifier.
        name: i32,
    },
}
impl NetworkReplayEngine {
    fn socket_option_state(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        option: &NetworkStreamSocketOption,
    ) -> Result<Result<StreamSocketOptionsV3, i32>, NetworkReplayError> {
        use detcore_model::network_trace::ReceiveOptionErrorV3;
        let control = self.owned_socket_control(owner, lease)?;
        let state = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&control.open_file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(
                control.open_file,
            ))?;
        let mut options = state.options.clone();
        let err = |error| match error {
            ReceiveOptionErrorV3::Domain => libc::EDOM,
            ReceiveOptionErrorV3::Permission => libc::EPERM,
            ReceiveOptionErrorV3::InvalidContract => libc::EINVAL,
        };
        match *option {
            NetworkStreamSocketOption::PeekOffset(value) => {
                if !state.normalization.peek_offset_set_supported {
                    return Ok(Err(libc::EOPNOTSUPP));
                }
                options.peek_offset = Some(value);
            }
            NetworkStreamSocketOption::ReceiveTimeout {
                seconds,
                microseconds,
            } => match state.normalization.normalize_timeout(seconds, microseconds) {
                Ok(value) => options.receive_timeout = value,
                Err(error) => return Ok(Err(err(error))),
            },
            NetworkStreamSocketOption::SendTimeout {
                seconds,
                microseconds,
            } => {
                if state.send_timeout.is_none() {
                    return Err(NetworkReplayError::WrongMode);
                }
                if let Err(error) = state.normalization.normalize_timeout(seconds, microseconds) {
                    return Ok(Err(err(error)));
                }
            }
            NetworkStreamSocketOption::ReceiveBuffer(value) => match state
                .normalization
                .normalize_receive_buffer(value, options.receive_buffer)
            {
                Ok(value) => options.receive_buffer = value,
                Err(error) => return Ok(Err(err(error))),
            },
            NetworkStreamSocketOption::ReceiveLowWater(value) => {
                // The initial ratio is verified at fresh-socket enrollment. A
                // received skb may change it; that later authority must join
                // before this implementation can normalize an unlocked buffer.
                let received = self
                    .channel_for(control.open_file)
                    .and_then(|channel| self.channels.get(&channel))
                    .is_some_and(|state| {
                        state
                            .published_ingress
                            .is_some_and(|frontier| frontier.stream_offset != 0)
                            || state.inbound_consumed != 0
                            || state
                                .inbound
                                .iter()
                                .any(|input| matches!(input, InboundOutcome::Stream { .. }))
                    });
                if received && !options.receive_buffer.user_locked {
                    return Err(NetworkReplayError::UnresolvedSocketNormalization(
                        control.open_file,
                    ));
                }
                match state
                    .normalization
                    .receive_buffer_after_low_water(value, options.receive_buffer)
                {
                    Ok((low, buffer)) => {
                        options.receive_low_water = low;
                        options.receive_buffer = buffer
                    }
                    Err(error) => return Ok(Err(err(error))),
                }
            }
            NetworkStreamSocketOption::ForcedReceiveBuffer(_)
            | NetworkStreamSocketOption::Other { .. } => {
                return Err(NetworkReplayError::UnresolvedSocketNormalization(
                    control.open_file,
                ));
            }
        }
        Ok(Ok(options))
    }
    /// Read-only semantic result. Replay never asks its placeholder to normalize.
    pub fn preview_socket_option(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        option: &NetworkStreamSocketOption,
    ) -> Result<Result<(), i32>, NetworkReplayError> {
        self.socket_option_state(owner, lease, option)
            .map(|result| result.map(|_| ()))
    }
    fn submit_socket_option(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        option: NetworkStreamSocketOption,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        if !control.physical.can_release_unchanged() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = control.open_file;
        let expected = if matches!(option, NetworkStreamSocketOption::ForcedReceiveBuffer(_)) {
            // Actual guest-context completion, in either mode, is the authority
            // for capability admission. Do not predict it from tracer privileges.
            Ok(self
                .shadow
                .as_ref()
                .and_then(|s| s.sockets.get(&open_file))
                .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?
                .options
                .clone())
        } else {
            self.socket_option_state(owner, lease, &option)?
        };
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical
            .option_pending = Some((option, expected));
        Ok(())
    }
    fn confirm_socket_option(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        result: Result<(), i32>,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        let (option, expected) = control
            .physical
            .option_pending
            .as_ref()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        let requested_low_water = match option {
            NetworkStreamSocketOption::ReceiveLowWater(value) => Some(*value),
            _ => None,
        };
        let next = if let NetworkStreamSocketOption::ForcedReceiveBuffer(value) = option {
            match result {
                Ok(()) => {
                    let state = self
                        .shadow
                        .as_ref()
                        .and_then(|s| s.sockets.get(&open_file))
                        .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?;
                    let mut options = state.options.clone();
                    options.receive_buffer = state
                        .normalization
                        .normalize_forced_receive_buffer(*value, true, options.receive_buffer)
                        .map_err(|_| {
                            NetworkReplayError::UnresolvedSocketNormalization(open_file)
                        })?;
                    Ok(options)
                }
                Err(errno) if (1..=4095).contains(&errno) => Err(errno),
                _ => return Err(NetworkReplayError::UnresolvedStreamOperation(lease)),
            }
        } else {
            if expected.as_ref().map(|_| ()).map_err(|error| *error) != result {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            expected.clone()
        };
        let send_timeout = if let NetworkStreamSocketOption::SendTimeout {
            seconds,
            microseconds,
        } = option
        {
            let state = self
                .shadow
                .as_ref()
                .unwrap()
                .sockets
                .get(&open_file)
                .unwrap();
            state
                .normalization
                .normalize_timeout(*seconds, *microseconds)
                .ok()
        } else {
            None
        };
        // Check generation exhaustion before any state mutation or receipt ACK.
        let next_generation = self.shadow.as_ref().unwrap().sockets[&open_file]
            .option_generation
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        if let Ok(options) = next {
            let effective_low_water = options.receive_low_water;
            let state = self
                .shadow
                .as_mut()
                .unwrap()
                .sockets
                .get_mut(&open_file)
                .unwrap();
            state.option_generation = next_generation;
            if send_timeout.is_some() {
                state.send_timeout = send_timeout;
            }
            self.shadow
                .as_mut()
                .expect("validated shadow")
                .sockets
                .get_mut(&open_file)
                .expect("validated socket")
                .options = options;
            if let Some(requested) = requested_low_water {
                // The engine lock still excludes readiness checks here. This
                // observes the semantic commit, not a later syscall return.
                tracing::trace!(
                    "[network-lowat-committed] dtid={} mm={:?} ofd={:?} lease={:?} requested={} effective={}",
                    owner.thread,
                    owner.mm,
                    open_file,
                    lease,
                    requested,
                    effective_low_water,
                );
            }
        }
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical
            .option_pending = None;
        Ok(())
    }
}

/// Exact result matched against the just-submitted physical operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkStreamPhysicalResult {
    /// Record kernel result or Replay's explicitly modeled result.
    SocketOption {
        /// Success or the positive Linux errno; mismatches remain unresolved.
        result: Result<(), i32>,
    },
    /// Actual consuming SO_ERROR result, not an invented readiness label.
    SocketError(i32),
    /// A requested cursor mutation completed successfully.
    Unit,
    /// Actual signed SO_PEEK_OFF getter result.
    PeekOffset(i32),
    /// The nonconsuming syscall returned this count, including retained prefix.
    Peeked {
        /// Actual successful physical syscall byte count.
        count: usize,
    },
    /// Actual poll0 output, not a caller-invented readiness category.
    PollState {
        /// Actual kernel poll result bits.
        revents: i16,
    },
    /// Actual kernel queue length from the ordered FIONREAD.
    QueuedBytes {
        /// Actual successful physical syscall byte count.
        count: usize,
    },
    /// Physical bytes removed; must match the immutable selection exactly.
    Drained {
        /// Physical bytes, checked against the selected logical prefix.
        bytes: Vec<u8>,
    },
    /// Known completed Linux error, classified using the submitted operation.
    Errno(i32),
    /// Known shutdown result; successful publication and local state are atomic.
    Shutdown {
        /// Record kernel result or Replay's validated expected local control.
        result: Result<(), i32>,
    },
}

#[derive(Debug, Clone)]
struct ShadowProbeState {
    call: NetworkStreamCallId,
    channel: NetworkChannelId,
    began: LogicalTime,
    retained_prefix: usize,
    captured_through: u64,
    cursor_observed: bool,
    original_cursor: Option<i32>,
    current_cursor: Option<i32>,
    peek: Option<Result<usize, i32>>,
    poll: Option<i16>,
    queued: Option<usize>,
    pending: Option<NetworkStreamPhysicalEffect>,
}

impl ShadowProbeState {
    fn cursor_restored(&self) -> bool {
        self.cursor_observed && self.current_cursor == self.original_cursor
    }
}

impl NetworkReplayEngine {
    /// Begin after paid physical wait completion. Pin ownership survives waits;
    /// this short control excludes every competing probe, delivery and option.
    pub fn begin_shadow_probe(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        now: LogicalTime,
    ) -> Result<NetworkShadowProbe, NetworkReplayError> {
        if !self.shadow_mode() || self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let open_file = self.stream_call_open_file(owner, call)?;
        let options = self.stream_call_socket_state(owner, call)?;
        let channel = self.bound_channel(open_file)?;
        if self.stream_role(channel)? == NetworkEndpointRoleV2::Listener {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let state = self.channels.get(&channel).expect("bound channel");
        let frontier = state.published_ingress.unwrap_or_default();
        let local_read_shutdown = state.local_read_shutdown;
        let retained = frontier
            .stream_offset
            .checked_sub(state.inbound_consumed)
            .ok_or(NetworkReplayError::Overflow)?;
        let retained_prefix =
            usize::try_from(retained).map_err(|_| NetworkReplayError::Overflow)?;
        retained_prefix
            .checked_add(1024)
            .ok_or(NetworkReplayError::Overflow)?;
        let lease = self.begin_stream_call_control(owner, call)?;
        self.shadow_probes.insert(
            lease,
            ShadowProbeState {
                call,
                channel,
                began: now,
                retained_prefix,
                captured_through: frontier.stream_offset,
                cursor_observed: false,
                original_cursor: options.options.peek_offset,
                current_cursor: options.options.peek_offset,
                peek: None,
                poll: None,
                queued: None,
                pending: None,
            },
        );
        Ok(NetworkShadowProbe {
            lease,
            retained_prefix,
            captured_through: frontier.stream_offset,
            local_read_shutdown,
        })
    }

    fn owned_shadow_probe(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<&ShadowProbeState, NetworkReplayError> {
        self.owned_socket_control(owner, lease)?;
        let probe = self
            .shadow_probes
            .get(&lease)
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        self.stream_call_open_file(owner, probe.call)?;
        Ok(probe)
    }

    /// Persist exactly one physical effect before host execution. A failed or
    /// canceled coordinator RPC never authorizes optimistic completion.
    pub fn submit_shadow_probe_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: NetworkStreamPhysicalEffect,
    ) -> Result<(), NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?;
        if probe.pending.is_some() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let valid = match effect {
            NetworkStreamPhysicalEffect::ReadPeekOffset => {
                !probe.cursor_observed && probe.peek.is_none()
            }
            NetworkStreamPhysicalEffect::SetPeekOffset { value } => {
                probe.cursor_observed
                    && match probe.original_cursor {
                        Some(original) if original >= 0 => {
                            (probe.peek.is_none()
                                && probe.current_cursor == Some(original)
                                && value == -1)
                                || (probe.peek.is_some()
                                    && probe.current_cursor == Some(-1)
                                    && value == original)
                        }
                        _ => false,
                    }
            }
            NetworkStreamPhysicalEffect::Peek { maximum } => {
                probe.cursor_observed
                    && probe.peek.is_none()
                    && probe.current_cursor.is_none_or(|value| value < 0)
                    && probe.retained_prefix.checked_add(1024) == Some(maximum)
            }
            NetworkStreamPhysicalEffect::PollState => {
                probe.peek.is_some() && probe.cursor_restored() && probe.poll.is_none()
            }
            NetworkStreamPhysicalEffect::QueuedBytes => {
                probe.poll.is_some() && probe.queued.is_none()
            }
            NetworkStreamPhysicalEffect::Drain { .. }
            | NetworkStreamPhysicalEffect::ReadSocketError
            | NetworkStreamPhysicalEffect::SetSocketOption { .. }
            | NetworkStreamPhysicalEffect::Shutdown { .. } => false,
        };
        if !valid {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        }
        self.shadow_probes
            .get_mut(&lease)
            .expect("validated probe")
            .pending = Some(effect);
        Ok(())
    }

    /// Validate completion in a cloned small receipt before replacing it.
    /// Inconsistent byte counts and operation/result pairs retain the original
    /// submitted latch because their physical effects have not been reconciled.
    pub fn confirm_shadow_probe_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        result: NetworkStreamPhysicalResult,
    ) -> Result<(), NetworkReplayError> {
        let mut next = self.owned_shadow_probe(owner, lease)?.clone();
        let effect = next
            .pending
            .clone()
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        if let NetworkStreamPhysicalResult::Errno(errno) = result
            && !(1..=4095).contains(&errno)
        {
            return Err(NetworkTraceValidationError::InvalidErrno.into());
        }
        match (effect, result) {
            (
                NetworkStreamPhysicalEffect::ReadPeekOffset,
                NetworkStreamPhysicalResult::PeekOffset(value),
            ) if next.original_cursor == Some(value) => {
                next.cursor_observed = true;
                next.current_cursor = Some(value);
            }
            (
                NetworkStreamPhysicalEffect::ReadPeekOffset,
                NetworkStreamPhysicalResult::Errno(errno),
            ) if next.original_cursor.is_none()
                && matches!(errno, libc::ENOPROTOOPT | libc::EOPNOTSUPP) =>
            {
                next.cursor_observed = true;
                next.current_cursor = None;
            }
            (
                NetworkStreamPhysicalEffect::SetPeekOffset { value },
                NetworkStreamPhysicalResult::Unit,
            ) => next.current_cursor = Some(value),
            (
                NetworkStreamPhysicalEffect::Peek { maximum },
                NetworkStreamPhysicalResult::Peeked { count },
            ) if count >= next.retained_prefix && count <= maximum => next.peek = Some(Ok(count)),
            (
                NetworkStreamPhysicalEffect::Peek { .. },
                NetworkStreamPhysicalResult::Errno(errno),
            ) if next.retained_prefix == 0
                && !matches!(errno, libc::EFAULT | libc::EBADF | libc::EIO) =>
            {
                next.peek = Some(Err(errno))
            }
            (
                NetworkStreamPhysicalEffect::PollState,
                NetworkStreamPhysicalResult::PollState { revents },
            ) if revents & libc::POLLNVAL == 0 => next.poll = Some(revents),
            (
                NetworkStreamPhysicalEffect::QueuedBytes,
                NetworkStreamPhysicalResult::QueuedBytes { count },
            ) => {
                let observed = next
                    .peek
                    .expect("submission required PEEK")
                    .unwrap_or_default();
                if count < next.retained_prefix || count < observed {
                    return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
                }
                next.queued = Some(count);
            }
            _ => return Err(NetworkReplayError::UnresolvedStreamOperation(lease)),
        }
        next.pending = None;
        self.shadow_probes.insert(lease, next);
        Ok(())
    }

    /// Append one positive observation and optional proven EOF as one engine
    /// transaction. Publication time/frontier/output watermark are engine-owned.
    pub fn complete_shadow_probe(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        now: LogicalTime,
        bytes: Vec<u8>,
        eof: bool,
    ) -> Result<(), NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?.clone();
        if probe.pending.is_some() || !probe.cursor_restored() || now < probe.began {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let (Some(peek), Some(revents), Some(queued)) = (probe.peek, probe.poll, probe.queued)
        else {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        };
        let suffix = match peek {
            Ok(count) => count - probe.retained_prefix,
            Err(_) => 0,
        };
        if bytes.len() != suffix || bytes.len() > 1024 {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let seen = probe
            .retained_prefix
            .checked_add(bytes.len())
            .ok_or(NetworkReplayError::Overflow)?;
        let local_read_shutdown = self
            .channels
            .get(&probe.channel)
            .expect("probe pins channel")
            .local_read_shutdown;
        // Local SHUT_RD also produces zero and RDHUP. Neither establishes peer
        // FIN; keep the ingress frontier open for later physical payload.
        let proven_eof = !local_read_shutdown && revents & libc::POLLRDHUP != 0 && queued == seen;
        if eof != proven_eof {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = self.stream_call_open_file(owner, probe.call)?;
        let channel = probe.channel;
        let state = self.channels.get(&channel).expect("probe pins channel");
        let EngineState::Record(trace) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let mut frontier = state.published_ingress.unwrap_or_default();
        if frontier.stream_offset != probe.captured_through
            || state
                .inbound_consumed
                .checked_add(probe.retained_prefix as u64)
                != Some(probe.captured_through)
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let previous = Self::prior_connection_release(trace, channel, state.published_ingress)?;
        let release = NetworkReleaseV2 {
            not_before_global_time: now,
            after_transmitted_offset: recorded_stream_output_offset(trace, channel)?,
        };
        if now < trace.epoch_global_time()?
            || previous.is_some_and(|prior| {
                now < prior.not_before_global_time
                    || release.after_transmitted_offset < prior.after_transmitted_offset
            })
        {
            return Err(NetworkTraceValidationError::NonMonotonicRelease.into());
        }
        let mut kinds = Vec::new();
        if !bytes.is_empty() {
            if frontier.terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal.into());
            }
            kinds.push(NetworkInputKindV2::StreamBytes {
                stream_offset: frontier.stream_offset,
                bytes,
            });
            frontier.stream_offset = frontier
                .stream_offset
                .checked_add(suffix as u64)
                .ok_or(NetworkReplayError::Overflow)?;
        }
        if let Err(errno) = peek
            && !matches!(errno, libc::EAGAIN | libc::EINTR)
        {
            if frontier.terminal {
                return Err(NetworkTraceValidationError::EventAfterTerminal.into());
            }
            kinds.push(NetworkInputKindV2::SocketError {
                stream_offset: frontier.stream_offset,
                errno,
            });
        }
        if eof && !frontier.terminal {
            kinds.push(NetworkInputKindV2::PeerShutdown {
                stream_offset: frontier.stream_offset,
                direction: NetworkShutdownV2::Write,
            });
            frontier.terminal = true;
        }
        let readiness = NetworkReadinessV2 {
            readable: false,
            writable: revents & libc::POLLOUT != 0,
            error: revents & libc::POLLERR != 0,
            // A locally completed pair of shutdown directions is guest-causal,
            // not a newly observed independent external hangup.
            hangup: revents & libc::POLLHUP != 0
                && !(state.local_write_closed
                    && (state.local_read_shutdown || state.receive_half_closed() || eof)),
        };
        if state.explicit_readiness != readiness {
            kinds.push(NetworkInputKindV2::Readiness(readiness));
        }
        if !self.shadow_channel_matches(open_file, channel) {
            return Err(NetworkReplayError::InvalidShadowPublication(channel));
        }
        let count = u64::try_from(trace.inputs.len()).map_err(|_| NetworkReplayError::Overflow)?;
        count
            .checked_add(kinds.len() as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        let inputs = kinds
            .into_iter()
            .enumerate()
            .map(|(index, event)| NetworkInputEventV2 {
                ordinal: count + index as u64,
                channel,
                release,
                event,
            })
            .collect::<Vec<_>>();
        let units = inputs
            .iter()
            .filter_map(|input| match &input.event {
                NetworkInputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                } => Some(ReceiveCopyUnitV1 {
                    input_ordinal: input.ordinal,
                    channel,
                    stream_offset: *stream_offset,
                    length: bytes.len() as u64,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !inputs.is_empty() {
            frontier.last_release = Some(release);
        }
        // All fallible checks precede the first journal/queue/receipt mutation.
        let EngineState::Record(trace) = &mut self.mode else {
            unreachable!()
        };
        let state = self.channels.get_mut(&channel).expect("validated channel");
        for input in inputs {
            state.release_at(input.ordinal, input.event.clone());
            trace.inputs.push(input);
        }
        state.published_ingress = Some(frontier);
        self.shadow
            .as_mut()
            .expect("validated shadow")
            .units
            .extend(units);
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&open_file);
        self.complete_deferred_retirement(open_file);
        Ok(())
    }

    /// Abort is only legal before PEEK and with the actual cursor restored;
    /// successful capture or unresolved effects cannot be discarded here.
    pub fn abort_shadow_probe_known(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        let probe = self.owned_shadow_probe(owner, lease)?;
        if probe.pending.is_some() || probe.peek.is_some() || !probe.cursor_restored() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = self.stream_call_open_file(owner, probe.call)?;
        self.shadow_probes.remove(&lease);
        self.socket_controls.remove(&open_file);
        self.complete_deferred_retirement(open_file);
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ShadowDeliveryState {
    call: NetworkStreamCallId,
    selected_len: usize,
    cursor_before: Option<i32>,
    next_consume_epoch: u64,
    drain_started: bool,
    drained: usize,
    peek_cursor_confirmed: bool,
    pending: Option<NetworkStreamPhysicalEffect>,
}

impl ShadowDeliveryState {
    fn peek_cursor(&self) -> Option<i32> {
        self.cursor_before.map(|value| {
            if value >= 0 {
                value.wrapping_add(self.selected_len as i32).max(0)
            } else {
                value
            }
        })
    }
    fn consumed_cursor(&self) -> Option<i32> {
        self.cursor_before.map(|value| {
            if value >= 0 {
                value.wrapping_sub(self.selected_len as i32).max(0)
            } else {
                value
            }
        })
    }
}

impl NetworkReplayEngine {
    /// Active-call admission keeps a retired OFD alive without authorizing a
    /// new descriptor operation. A zero-capacity receive has its own atomic API.
    pub fn reserve_stream_call_chunk(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        maximum: usize,
        peek_offset: usize,
    ) -> Result<NetworkStreamChunk, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        if maximum == 0 {
            return Err(NetworkReplayError::ZeroStreamReservation);
        }
        let socket = self.stream_call_socket_state(owner, call)?;
        let next_consume_epoch = socket
            .consume_epoch
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let mut chunk = self.reserve_stream_chunk(owner, open_file, maximum, peek_offset)?;
        if chunk == NetworkStreamChunk::Empty {
            let channel = self.bound_channel(open_file)?;
            if self
                .channels
                .get(&channel)
                .expect("live call channel")
                .local_read_shutdown
            {
                chunk = NetworkStreamChunk::LocalReadClosed;
            }
        }
        if let NetworkStreamChunk::Reserved {
            lease,
            selection_len,
            ..
        } = &chunk
        {
            self.shadow_deliveries.insert(
                *lease,
                ShadowDeliveryState {
                    call,
                    selected_len: *selection_len,
                    cursor_before: socket.options.peek_offset,
                    next_consume_epoch,
                    drain_started: false,
                    drained: 0,
                    peek_cursor_confirmed: false,
                    pending: None,
                },
            );
        }
        Ok(chunk)
    }

    /// Queue lookup uses the admitted syscall reference, not a recycled fd.
    pub fn stream_call_queue_status(
        &self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
    ) -> Result<NetworkStreamQueueStatus, NetworkReplayError> {
        let open_file = self.stream_call_open_file(owner, call)?;
        self.stream_queue_status(open_file)
    }

    fn owned_shadow_delivery(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<&ShadowDeliveryState, NetworkReplayError> {
        let operation = self.owned_stream_operation(owner, lease)?;
        let state = self
            .shadow_deliveries
            .get(&lease)
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        if self.stream_call_open_file(owner, state.call)? != operation.open_file {
            return Err(NetworkReplayError::StreamDeliveryChanged(lease));
        }
        Ok(state)
    }

    /// The caller has completed the entire selected user copy. No logical
    /// consumption happens until every actual drain byte is confirmed.
    pub fn begin_record_drain(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        if self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        let state = self.owned_shadow_delivery(owner, lease)?;
        let operation = self.owned_stream_operation(owner, lease)?;
        if state.drain_started
            || state.pending.is_some()
            || !matches!(
                operation.kind,
                StreamOperationKind::Delivery {
                    peek_offset: 0,
                    outcome: NetworkStreamChunkOutcome::Bytes(_),
                    ..
                }
            )
        {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        }
        self.shadow_deliveries
            .get_mut(&lease)
            .expect("validated delivery")
            .drain_started = true;
        Ok(())
    }

    /// Persist one operation in the probe or selected-delivery receipt.
    pub fn submit_stream_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        effect: NetworkStreamPhysicalEffect,
    ) -> Result<(), NetworkReplayError> {
        if let NetworkStreamPhysicalEffect::SetSocketOption { option } = effect {
            return self.submit_socket_option(owner, lease, option);
        }
        if let NetworkStreamPhysicalEffect::Shutdown { direction } = effect {
            return self.submit_socket_shutdown(owner, lease, direction);
        }
        if self.shadow_probes.contains_key(&lease) {
            return self.submit_shadow_probe_physical(owner, lease, effect);
        }
        let state = self.owned_shadow_delivery(owner, lease)?;
        if self.mode() != NetworkEngineMode::Record {
            return Err(NetworkReplayError::WrongMode);
        }
        if state.pending.is_some() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let valid = match effect {
            NetworkStreamPhysicalEffect::Drain { maximum } => {
                state.drain_started
                    && maximum > 0
                    && maximum <= NETWORK_STREAM_CHUNK_LIMIT
                    && maximum <= state.selected_len - state.drained
            }
            NetworkStreamPhysicalEffect::SetPeekOffset { value } => {
                !state.drain_started
                    && !state.peek_cursor_confirmed
                    && state.cursor_before.is_some_and(|value| value >= 0)
                    && state.peek_cursor() == Some(value)
            }
            _ => false,
        };
        if !valid {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        }
        self.shadow_deliveries
            .get_mut(&lease)
            .expect("validated delivery")
            .pending = Some(effect);
        Ok(())
    }

    /// Verify physical progress against the receipt's exact immutable bytes.
    pub fn confirm_stream_physical(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        result: NetworkStreamPhysicalResult,
    ) -> Result<(), NetworkReplayError> {
        if let NetworkStreamPhysicalResult::SocketOption { result } = result {
            return self.confirm_socket_option(owner, lease, result);
        }
        if let NetworkStreamPhysicalResult::Shutdown { result } = result {
            return self.confirm_socket_shutdown(owner, lease, result);
        }
        if self.shadow_probes.contains_key(&lease) {
            return self.confirm_shadow_probe_physical(owner, lease, result);
        }
        let state = self.owned_shadow_delivery(owner, lease)?;
        let Some(pending) = state.pending.clone() else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        let mut next = state.clone();
        match (pending, result) {
            (
                NetworkStreamPhysicalEffect::Drain { maximum },
                NetworkStreamPhysicalResult::Drained { bytes },
            ) => {
                if bytes.is_empty() || bytes.len() > maximum {
                    return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
                }
                let expected =
                    self.read_stream_chunk_view(owner, lease, state.drained, bytes.len())?;
                if bytes != expected {
                    return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
                }
                next.drained += bytes.len();
            }
            (
                NetworkStreamPhysicalEffect::SetPeekOffset { .. },
                NetworkStreamPhysicalResult::Unit,
            ) => next.peek_cursor_confirmed = true,
            // Even a known drain error follows a successful guest copy and
            // potentially prior physical progress. It cannot become EAGAIN or
            // resolve the outstanding prefix as though no effects happened.
            _ => return Err(NetworkReplayError::UnresolvedStreamOperation(lease)),
        }
        next.pending = None;
        self.shadow_deliveries.insert(lease, next);
        Ok(())
    }

    /// Atomically consume the selection only after its full physical drain.
    pub fn finish_record_drain(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<(), NetworkReplayError> {
        self.finish_stream_chunk_inner(owner, lease, NetworkStreamChunkDisposition::Consumed, true)
    }

    fn validate_shadow_delivery_finish(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        disposition: NetworkStreamChunkDisposition,
        record_drain: bool,
    ) -> Result<(), NetworkReplayError> {
        let Some(state) = self.shadow_deliveries.get(&lease) else {
            return if record_drain {
                Err(NetworkReplayError::StreamLeaseKindMismatch(lease))
            } else {
                Ok(())
            };
        };
        self.owned_shadow_delivery(owner, lease)?;
        if state.pending.is_some() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let operation = self.owned_stream_operation(owner, lease)?;
        let bytes = matches!(
            operation.kind,
            StreamOperationKind::Delivery {
                outcome: NetworkStreamChunkOutcome::Bytes(_),
                ..
            }
        );
        if disposition == NetworkStreamChunkDisposition::Consumed
            && bytes
            && self.mode() == NetworkEngineMode::Record
        {
            if !record_drain || !state.drain_started || state.drained != state.selected_len {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
        } else if record_drain || state.drain_started {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        if disposition == NetworkStreamChunkDisposition::Peeked
            && bytes
            && self.mode() == NetworkEngineMode::Record
            && state.cursor_before.is_some_and(|value| value >= 0)
            && !state.peek_cursor_confirmed
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        if disposition == NetworkStreamChunkDisposition::CopyFailed && state.peek_cursor_confirmed {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        Ok(())
    }

    fn commit_shadow_delivery_finish(
        &mut self,
        lease: NetworkStreamLeaseId,
        open_file: OpenFileId,
        disposition: NetworkStreamChunkDisposition,
        bytes: bool,
    ) {
        let Some(state) = self.shadow_deliveries.remove(&lease) else {
            return;
        };
        let socket = self
            .shadow
            .as_mut()
            .expect("validated shadow")
            .sockets
            .get_mut(&open_file)
            .expect("call pins socket");
        if bytes {
            match disposition {
                NetworkStreamChunkDisposition::Consumed => {
                    socket.options.peek_offset = state.consumed_cursor();
                    socket.consume_epoch = state.next_consume_epoch;
                }
                NetworkStreamChunkDisposition::Peeked => {
                    socket.options.peek_offset = state.peek_cursor()
                }
                NetworkStreamChunkDisposition::CopyFailed => {}
            }
        }
    }
}

impl NetworkReplayEngine {
    /// Return only facts belonging to an already held exact receipt.
    pub fn socket_control_view(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<NetworkSocketControl, NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let state = self
            .shadow
            .as_ref()
            .and_then(|s| s.sockets.get(&control.open_file));
        let pending_error = self
            .channel_for(control.open_file)
            .and_then(|channel| self.channels.get(&channel))
            .and_then(|channel| {
                channel.inbound.iter().find_map(|input| match input {
                    InboundOutcome::Error { errno, .. } => Some(*errno),
                    _ => None,
                })
            });
        Ok(NetworkSocketControl {
            lease,
            pending_error,
            options: state.map(|s| s.options.clone()),
            consume_epoch: state.map_or(0, |s| s.consume_epoch),
        })
    }
}

/// Transient wait receipt; never a recorded external input or a caller ordinal.
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
pub struct NetworkZeroStreamWaitId(u64);
#[derive(Debug, Clone)]
struct ZeroStreamWaitState {
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    record_operation: Option<crate::resources::ExternalOpId>,
    channel: NetworkChannelId,
    input_generation: Option<u64>,
    local_control_generation: u64,
    armed: bool,
    entered: bool,
    abandoned: bool,
}
impl NetworkReplayEngine {
    fn owned_zero_stream_wait(
        &self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<&ZeroStreamWaitState, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let wait = self
            .zero_stream_waits
            .get(&id)
            .ok_or(NetworkReplayError::UnknownZeroStreamWait(id))?;
        if wait.owner != owner {
            return Err(NetworkReplayError::StreamCallOwnerMismatch(wait.call));
        }
        if wait.abandoned {
            return Err(NetworkReplayError::UnresolvedZeroStreamWait(id));
        }
        self.stream_call_open_file(owner, wait.call)?;
        Ok(wait)
    }

    /// Atomic empty decision and arrival snapshot for the runtime recv(0) path.
    /// One receipt survives every maintenance timer until explicit resolution.
    pub fn prepare_zero_stream_receive(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        peek_offset: usize,
    ) -> Result<NetworkZeroStreamReceive, NetworkReplayError> {
        let existing = self
            .zero_stream_waits
            .iter()
            .find(|(_, wait)| wait.call == call)
            .map(|(id, _)| *id);
        if let Some(id) = existing
            && self.zero_stream_wait_entered(owner, id)?
            && self.zero_stream_wait_ready(owner, id)?
        {
            // sk_wait_data's tail change ends a zero-length receive even when
            // the new tail is still below a positive peek offset. Do not take
            // an error or change the cursor on this post-decision arrival.
            return Ok(NetworkZeroStreamReceive::Ready);
        }
        let decision = self.zero_stream_receive(owner, call, peek_offset)?;
        if decision != NetworkZeroStreamReceive::Empty {
            return Ok(decision);
        }
        let open_file = self.stream_call_open_file(owner, call)?;
        let channel = self.bound_channel(open_file)?;
        let state = self.channels.get(&channel).expect("bound channel exists");
        let input_generation = state.receive_input_generation;
        let local_control_generation = state.local_control_generation;
        if let Some(id) = existing {
            // Before scheduler entry this is still tcp_recvmsg's normal
            // bytes/error/EOF decision, not a completed sk_wait_data. A
            // nonblocking or expired call must not turn a below-offset
            // observation into success merely because it was first captured.
            let wait = self.zero_stream_waits.get_mut(&id).expect("validated wait");
            if !wait.entered {
                wait.input_generation = input_generation;
                wait.local_control_generation = local_control_generation;
            }
            return Ok(NetworkZeroStreamReceive::Waiting(id));
        }
        let next = self
            .next_zero_stream_wait
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let id = NetworkZeroStreamWaitId(self.next_zero_stream_wait);
        self.next_zero_stream_wait = next;
        self.zero_stream_waits.insert(
            id,
            ZeroStreamWaitState {
                owner,
                call,
                record_operation: None,
                channel,
                input_generation,
                local_control_generation,
                armed: false,
                entered: false,
                abandoned: false,
            },
        );
        Ok(NetworkZeroStreamReceive::Waiting(id))
    }

    /// Data, EOF or error publication after the atomic decision ends recv(0).
    /// Queue consumption and repeated readiness observations do not change it.
    pub fn zero_stream_wait_ready(
        &self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<bool, NetworkReplayError> {
        let wait = self.owned_zero_stream_wait(owner, id)?;
        let state = self
            .channels
            .get(&wait.channel)
            .expect("live call retains channel");
        Ok(state.receive_input_generation != wait.input_generation
            || state.local_control_generation != wait.local_control_generation)
    }

    /// Authenticate the exact active call referenced by a scheduler wait.
    pub fn zero_stream_wait_call(
        &self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<NetworkStreamCallId, NetworkReplayError> {
        Ok(self.owned_zero_stream_wait(owner, id)?.call)
    }

    /// Read actual scheduler entry without consuming the persistent receipt.
    pub fn zero_stream_wait_entered(
        &self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<bool, NetworkReplayError> {
        Ok(self.owned_zero_stream_wait(owner, id)?.entered)
    }

    /// Arm the already prepared receipt for the exact scheduler operation.
    pub fn begin_zero_stream_wait(
        &mut self,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        record_operation: Option<crate::resources::ExternalOpId>,
    ) -> Result<NetworkZeroStreamWaitId, NetworkReplayError> {
        self.stream_call_open_file(owner, call)?;
        if (self.mode() == NetworkEngineMode::Record) != record_operation.is_some() {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        let id = self
            .zero_stream_waits
            .iter()
            .find(|(_, wait)| wait.call == call)
            .map(|(id, _)| *id)
            .ok_or(NetworkReplayError::ZeroStreamWaitNotPrepared(call))?;
        let wait = self.owned_zero_stream_wait(owner, id)?;
        if wait.armed && wait.record_operation != record_operation {
            return Err(NetworkReplayError::StreamCallPhaseMismatch(call));
        }
        let wait = self.zero_stream_waits.get_mut(&id).expect("validated wait");
        wait.armed = true;
        wait.record_operation = record_operation;
        Ok(id)
    }
    /// Scheduler alone marks actual logical wait entry, after pending-signal check.
    pub fn enter_zero_stream_wait(
        &mut self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<(), NetworkReplayError> {
        let wait = self.owned_zero_stream_wait(owner, id)?;
        if !wait.armed {
            return Err(NetworkReplayError::ZeroStreamWaitNotPrepared(wait.call));
        }
        if wait.entered {
            return Ok(());
        }
        let call = wait.call;
        let record_operation = wait.record_operation;
        self.zero_stream_waits
            .get_mut(&id)
            .expect("validated wait")
            .entered = true;
        crate::detlog!(
            "[network-zero-wait] entered owner={owner:?} call={call:?} wait={id:?} record_operation={record_operation:?}"
        );
        Ok(())
    }
    /// Bind Record's existing single-resource background grant to its receipt.
    pub fn enter_record_zero_stream_wait(
        &mut self,
        owner: NetworkStreamOwner,
        operation: crate::resources::ExternalOpId,
    ) -> Result<(), NetworkReplayError> {
        let ids: Vec<_> = self
            .zero_stream_waits
            .iter()
            .filter(|(_, w)| w.owner == owner && w.record_operation == Some(operation))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.enter_zero_stream_wait(owner, id)?;
        }
        Ok(())
    }
    /// Resolve the same transient wait once, preserving its scheduler entry bit.
    pub fn finish_zero_stream_wait(
        &mut self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<bool, NetworkReplayError> {
        self.owned_zero_stream_wait(owner, id)?;
        Ok(self
            .zero_stream_waits
            .remove(&id)
            .expect("validated wait")
            .entered)
    }

    /// Cancel a prepared/immediately interrupted wait that never entered.
    /// Entered waits require explicit completion, never a guessed cancellation.
    pub fn cancel_zero_stream_wait(
        &mut self,
        owner: NetworkStreamOwner,
        id: NetworkZeroStreamWaitId,
    ) -> Result<(), NetworkReplayError> {
        if self.owned_zero_stream_wait(owner, id)?.entered {
            return Err(NetworkReplayError::UnresolvedZeroStreamWait(id));
        }
        self.zero_stream_waits.remove(&id);
        Ok(())
    }
}

impl NetworkReplayEngine {
    fn validate_replay_shutdown(
        &self,
        open_file: OpenFileId,
        direction: NetworkShutdownV2,
    ) -> Result<NetworkChannelId, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
        if !matches!(state.outbound.front(), Some(OutboundOutcome::Shutdown {
            stream_offset, direction: expected,
        }) if *stream_offset == state.transmitted && *expected == direction)
        {
            return Err(NetworkReplayError::UnexpectedShutdown(channel));
        }
        Ok(channel)
    }

    fn submit_socket_shutdown(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        direction: NetworkShutdownV2,
    ) -> Result<(), NetworkReplayError> {
        if !self.shadow_mode() {
            return Err(NetworkReplayError::WrongMode);
        }
        let control = self.owned_socket_control(owner, lease)?;
        if !control.physical.can_release_unchanged() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        let open_file = control.open_file;
        if self.mode() == NetworkEngineMode::Replay {
            self.validate_replay_shutdown(open_file, direction)?;
        }
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical
            .shutdown_pending = Some(direction);
        Ok(())
    }

    fn confirm_socket_shutdown(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        result: Result<(), i32>,
    ) -> Result<(), NetworkReplayError> {
        let control = self.owned_socket_control(owner, lease)?;
        let open_file = control.open_file;
        let direction = control
            .physical
            .shutdown_pending
            .ok_or(NetworkReplayError::StreamLeaseKindMismatch(lease))?;
        if let Err(errno) = result {
            if self.mode() != NetworkEngineMode::Record || !(1..=4095).contains(&errno) {
                return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
            }
            self.socket_controls
                .get_mut(&open_file)
                .expect("validated control")
                .physical
                .shutdown_pending = None;
            return Ok(());
        }
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
        let closes_read = matches!(direction, NetworkShutdownV2::Read | NetworkShutdownV2::Both);
        let next_generation = if closes_read {
            state
                .local_control_generation
                .checked_add(1)
                .ok_or(NetworkReplayError::Overflow)?
        } else {
            state.local_control_generation
        };
        // Validate every fallible condition before journal/state mutation.
        match &self.mode {
            EngineState::Record(trace) => {
                recorded_stream_output_offset(trace, channel)?;
            }
            EngineState::Replay(_) => {
                self.validate_replay_shutdown(open_file, direction)?;
            }
        }
        match &mut self.mode {
            EngineState::Record(trace) => {
                let stream_offset = recorded_stream_output_offset(trace, channel)?;
                trace.outputs.push(NetworkOutputEventV2 {
                    channel,
                    event: NetworkOutputKindV2::Shutdown {
                        stream_offset,
                        direction,
                    },
                });
            }
            EngineState::Replay(_) => {
                self.channels
                    .get_mut(&channel)
                    .expect("validated channel")
                    .outbound
                    .pop_front();
            }
        }
        let state = self.channels.get_mut(&channel).expect("validated channel");
        state.local_read_shutdown |= closes_read;
        state.local_control_generation = next_generation;
        state.local_write_closed |= matches!(
            direction,
            NetworkShutdownV2::Write | NetworkShutdownV2::Both
        );
        state.refresh_readiness();
        self.socket_controls
            .get_mut(&open_file)
            .expect("validated control")
            .physical
            .shutdown_pending = None;
        Ok(())
    }

    /// Receive-half shutdown is separate from full hangup and does not consume data.
    pub fn receive_half_closed(&self, open_file: OpenFileId) -> Result<bool, NetworkReplayError> {
        let status = self.stream_queue_status(open_file)?;
        Ok(!status.delivery_busy
            && (status.eof
                || status.local_read_shutdown
                || status.error.is_some()
                || status.readiness.error
                || status.readiness.hangup))
    }
}

/// Pure state machine shared by capture and replay policies.
#[derive(Debug)]
pub struct NetworkReplayEngine {
    lifetime: NetworkLifetime,
    fd_lifecycle: FdLifecycleState,
    fd_publications: HashMap<FilesId, FdPublicationState>,
    fd_installations: BTreeMap<NetworkStreamLeaseId, ConfirmedFdInstallation>,
    fd_publication_history: HashMap<(FilesId, u64), NetworkFdPublicationBatch>,
    mode: EngineState,
    shadow: Option<ShadowReceiveState>,
    shadow_probes: BTreeMap<NetworkStreamLeaseId, ShadowProbeState>,
    shadow_deliveries: BTreeMap<NetworkStreamLeaseId, ShadowDeliveryState>,
    /// Runtime queues have the same owner in capture and replay. Legacy capture
    /// remains journal-only until its adapter moves to `publish_ingress`.
    channels: BTreeMap<NetworkChannelId, ChannelState>,
    next_record_channel: u64,
    next_stream_lease: u64,
    next_stream_call: u64,
    next_zero_stream_wait: u64,
    zero_stream_waits: BTreeMap<NetworkZeroStreamWaitId, ZeroStreamWaitState>,
    stream_calls: BTreeMap<NetworkStreamCallId, StreamCallState>,
    socket_controls: BTreeMap<OpenFileId, SocketControlState>,
    stream_operations: BTreeMap<NetworkStreamLeaseId, StreamOperation>,
    stream_ingress: BTreeMap<OpenFileId, NetworkStreamLeaseId>,
    stream_delivery: BTreeMap<OpenFileId, NetworkStreamLeaseId>,
    gone_stream_owners: HashSet<NetworkStreamOwner>,
    bindings: BTreeMap<OpenFileId, NetworkChannelId>,
    reverse_bindings: BTreeMap<NetworkChannelId, OpenFileId>,
    /// A trace channel identity is single-use even after its last OFD alias is
    /// retired; fd/OFD reuse must never resurrect an old connection.
    retired_channels: BTreeSet<NetworkChannelId>,
    retired_open_files: BTreeSet<OpenFileId>,
    ancillary_objects: BTreeMap<NetworkObjectId, AncillaryObjectState>,
    ancillary_by_open_file: BTreeMap<OpenFileId, NetworkObjectId>,
    retired_ancillary_objects: BTreeSet<NetworkObjectId>,
    retired_ancillary_open_files: BTreeSet<OpenFileId>,
    epolls: BTreeMap<OpenFileId, BTreeMap<OpenFileId, EpollInterestState>>,
}

#[derive(Debug)]
struct AncillaryObjectState {
    open_file: OpenFileId,
    kind: NetworkAncillaryObjectKind,
    alias_count: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct ReadinessGeneration {
    readable: u64,
    writable: u64,
    error: u64,
    hangup: u64,
}

#[derive(Debug)]
struct EpollInterestState {
    events: u32,
    edge_triggered: bool,
    one_shot: bool,
    enabled: bool,
    seen: ReadinessGeneration,
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PublishedIngress {
    stream_offset: u64,
    last_release: Option<NetworkReleaseV2>,
    terminal: bool,
}

#[derive(Debug)]
struct ChannelState {
    transport: NetworkTransportV2,
    inbound_consumed: u64,
    /// Last receive-affecting input ordinal; independent of level and dequeue.
    receive_input_generation: Option<u64>,
    inbound: VecDeque<InboundOutcome>,
    explicit_readiness: NetworkReadinessV2,
    transmitted: u64,
    outbound: VecDeque<OutboundOutcome>,
    local_write_closed: bool,
    local_read_shutdown: bool,
    local_control_generation: u64,
    peer_write_closed: bool,
    readiness: NetworkReadinessV2,
    readiness_generation: ReadinessGeneration,
    /// Physical ingress frontier, independent of which reader consumed bytes.
    /// `None` also distinguishes legacy journal-only capture from publication.
    published_ingress: Option<PublishedIngress>,
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

impl EpollInterestState {
    fn new(events: u32, generation: ReadinessGeneration, rearm: bool) -> Self {
        let seen = if rearm {
            ReadinessGeneration {
                readable: generation.readable.saturating_sub(1),
                writable: generation.writable.saturating_sub(1),
                error: generation.error.saturating_sub(1),
                hangup: generation.hangup.saturating_sub(1),
            }
        } else {
            ReadinessGeneration::default()
        };
        Self {
            events,
            edge_triggered: events & libc::EPOLLET as u32 != 0,
            one_shot: events & libc::EPOLLONESHOT as u32 != 0,
            enabled: true,
            seen,
        }
    }
}

fn epoll_events(readiness: NetworkReadinessV2, interest: u32) -> u32 {
    let mut events = 0;
    if readiness.readable && interest & libc::EPOLLIN as u32 != 0 {
        events |= libc::EPOLLIN as u32;
    }
    if readiness.writable && interest & libc::EPOLLOUT as u32 != 0 {
        events |= libc::EPOLLOUT as u32;
    }
    // Linux reports ERR/HUP whether or not the caller requested those bits.
    if readiness.error {
        events |= libc::EPOLLERR as u32;
    }
    if readiness.hangup {
        events |= libc::EPOLLHUP as u32;
        if interest & libc::EPOLLRDHUP as u32 != 0 {
            events |= libc::EPOLLRDHUP as u32;
        }
    }
    events
}

fn readiness_transitioned(
    readiness: NetworkReadinessV2,
    generation: ReadinessGeneration,
    seen: ReadinessGeneration,
    interest: u32,
) -> bool {
    (readiness.readable
        && interest & libc::EPOLLIN as u32 != 0
        && generation.readable > seen.readable)
        || (readiness.writable
            && interest & libc::EPOLLOUT as u32 != 0
            && generation.writable > seen.writable)
        || (readiness.error && generation.error > seen.error)
        || (readiness.hangup && generation.hangup > seen.hangup)
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

/// Decode once and require exact agreement with the pre-container run epoch.
pub fn replay_from_reader_with_expected_epoch<R: Read>(
    reader: R,
    expected_epoch: DateTime<Utc>,
) -> Result<NetworkReplayEngine, NetworkReplayError> {
    let trace = NetworkTrace::read_framed(reader).map_err(NetworkReplayError::Codec)?;
    NetworkReplayEngine::replay_versioned_with_expected_epoch(trace, expected_epoch)
}

/// Largest complete framed trace accepted from a host resource.
const MAX_NETWORK_TRACE_FILE_BYTES: u64 =
    MAX_NETWORK_TRACE_PAYLOAD_BYTES + NETWORK_TRACE_MAGIC.len() as u64 + 4 + 8;

/// Open a trace once in the host namespace and read it through that exact
/// descriptor, refusing links, non-regular files, and over-sized inputs before
/// the codec allocates its payload.
pub fn open_bounded_network_trace(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        // O_NONBLOCK makes opening a FIFO safe: fstat below then refuses it.
        // Linux ignores this flag for regular files.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    read_bounded_network_trace(file)
}

/// Read one already-open network trace without trusting its pathname again.
pub fn read_bounded_network_trace(mut file: File) -> io::Result<Vec<u8>> {
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network trace is not a regular file",
        ));
    }
    if metadata.len() > MAX_NETWORK_TRACE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "network trace is too large: {} bytes (maximum {MAX_NETWORK_TRACE_FILE_BYTES})",
                metadata.len()
            ),
        ));
    }

    // Recheck the bound while reading: a regular file may grow after fstat.
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.seek(SeekFrom::Start(0))?;
    file.by_ref()
        .take(MAX_NETWORK_TRACE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_NETWORK_TRACE_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("network trace grew beyond {MAX_NETWORK_TRACE_FILE_BYTES} bytes while reading"),
        ));
    }
    Ok(bytes)
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

    /// Decode once and require exact agreement with the pre-container epoch.
    pub fn replay_from_reader_with_expected_epoch<R: Read>(
        reader: R,
        expected_epoch: DateTime<Utc>,
    ) -> Result<Self, NetworkReplayError> {
        replay_from_reader_with_expected_epoch(reader, expected_epoch)
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
            channels: BTreeMap::new(),
            shadow: None,
            shadow_probes: BTreeMap::new(),
            shadow_deliveries: BTreeMap::new(),
            lifetime: NetworkLifetime::default(),
            fd_lifecycle: FdLifecycleState::default(),
            fd_publications: HashMap::new(),
            fd_installations: BTreeMap::new(),
            fd_publication_history: HashMap::new(),
            next_record_channel: 1,
            next_stream_lease: 1,
            next_stream_call: 1,
            next_zero_stream_wait: 1,
            zero_stream_waits: BTreeMap::new(),
            stream_calls: BTreeMap::new(),
            socket_controls: BTreeMap::new(),
            stream_operations: BTreeMap::new(),
            stream_ingress: BTreeMap::new(),
            stream_delivery: BTreeMap::new(),
            gone_stream_owners: HashSet::new(),
            bindings: BTreeMap::new(),
            reverse_bindings: BTreeMap::new(),
            retired_channels: BTreeSet::new(),
            retired_open_files: BTreeSet::new(),
            ancillary_objects: BTreeMap::new(),
            ancillary_by_open_file: BTreeMap::new(),
            retired_ancillary_objects: BTreeSet::new(),
            retired_ancillary_open_files: BTreeSet::new(),
            epolls: BTreeMap::new(),
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
        for state in channels.values_mut() {
            state.refresh_readiness();
        }
        let released = vec![false; trace.inputs.len()];
        Ok(Self {
            mode: EngineState::Replay(ReplayState { trace, released }),
            channels,
            shadow: None,
            shadow_probes: BTreeMap::new(),
            shadow_deliveries: BTreeMap::new(),
            lifetime: NetworkLifetime::default(),
            fd_lifecycle: FdLifecycleState::default(),
            fd_publications: HashMap::new(),
            fd_installations: BTreeMap::new(),
            fd_publication_history: HashMap::new(),
            next_record_channel: 1,
            next_stream_lease: 1,
            next_stream_call: 1,
            next_zero_stream_wait: 1,
            zero_stream_waits: BTreeMap::new(),
            stream_calls: BTreeMap::new(),
            socket_controls: BTreeMap::new(),
            stream_operations: BTreeMap::new(),
            stream_ingress: BTreeMap::new(),
            stream_delivery: BTreeMap::new(),
            gone_stream_owners: HashSet::new(),
            bindings: BTreeMap::new(),
            reverse_bindings: BTreeMap::new(),
            retired_channels: BTreeSet::new(),
            retired_open_files: BTreeSet::new(),
            ancillary_objects: BTreeMap::new(),
            ancillary_by_open_file: BTreeMap::new(),
            retired_ancillary_objects: BTreeSet::new(),
            retired_ancillary_open_files: BTreeSet::new(),
            epolls: BTreeMap::new(),
        })
    }

    /// Create a shared-engine replay from either supported codec version.
    /// V1's declared single outbound TCP envelope is upgraded without changing
    /// its release gates, byte offsets, or stable socket identity.
    pub fn replay_versioned(trace: NetworkTrace) -> Result<Self, NetworkReplayError> {
        match trace {
            NetworkTrace::V1(trace) => Self::replay(upgrade_v1_trace(trace)?),
            NetworkTrace::V2(trace) => Self::replay(trace),
            NetworkTrace::V3(trace) => Self::replay_shadow(trace),
        }
    }

    /// Replay either codec version only when its authoritative epoch exactly
    /// matches the epoch already resolved for this run.
    pub fn replay_versioned_with_expected_epoch(
        trace: NetworkTrace,
        expected_epoch: DateTime<Utc>,
    ) -> Result<Self, NetworkReplayError> {
        let actual_epoch = match &trace {
            NetworkTrace::V1(trace) => trace.epoch,
            NetworkTrace::V2(trace) => trace.epoch,
            NetworkTrace::V3(trace) => trace.history.epoch,
        };
        if actual_epoch != expected_epoch {
            return Err(NetworkReplayError::EpochMismatch {
                expected: expected_epoch,
                actual: actual_epoch,
            });
        }
        Self::replay_versioned(trace)
    }

    /// Current capture/replay mode.
    pub fn mode(&self) -> NetworkEngineMode {
        match self.mode {
            EngineState::Record(_) => NetworkEngineMode::Record,
            EngineState::Replay(_) => NetworkEngineMode::Replay,
        }
    }

    /// Exact epoch persisted by the active record or replay trace.
    pub fn trace_epoch(&self) -> DateTime<Utc> {
        match &self.mode {
            EngineState::Record(trace) => trace.epoch,
            EngineState::Replay(replay) => replay.trace.epoch,
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
        self.channels
            .insert(channel.id, ChannelState::new(&channel));
        trace.channels.push(channel);
        Ok(())
    }

    /// Append one legacy, journal-only input observation while recording.
    /// Do not mix this post-syscall path with pre-delivery ingress publication
    /// on the same channel: that would duplicate or omit stream bytes.
    pub fn record_input(
        &mut self,
        mut input: NetworkInputEventV2,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Record(trace) = &mut self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if self
            .channels
            .get(&input.channel)
            .is_some_and(|state| state.published_ingress.is_some())
        {
            return Err(NetworkReplayError::MixedIngressCapture(input.channel));
        }
        input.ordinal = trace.inputs.len() as u64;
        trace.inputs.push(input);
        Ok(())
    }

    /// Publish one physical stream ingress observation before guest delivery.
    ///
    /// This preparatory API is deliberately unused by the legacy syscall
    /// adapter. The caller must serialize physical receives on this OFD before
    /// calling it; the input offset is checked against the engine-owned ingress
    /// frontier, not the consumer's receive offset. Bytes, peer shutdown and
    /// externally observed socket errors are supported here. Connection and
    /// message observations need their own typed publication contract.
    ///
    /// EAGAIN, EINTR and EFAULT are not transport arrivals: they describe local
    /// availability, signal/restart or copy outcomes and must not enter this
    /// queue. Other errnos still require an actual transport observation from
    /// the caller, not a failed guest syscall reinterpreted as stream input.
    ///
    /// Every fallible check precedes journal, queue and frontier mutation. No
    /// guest copy or receive consumption is performed by publication.
    pub fn publish_ingress(
        &mut self,
        open_file: OpenFileId,
        mut input: NetworkInputEventV2,
    ) -> Result<(), NetworkReplayError> {
        let EngineState::Record(trace) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let channel = self.bound_channel(open_file)?;
        if channel != input.channel {
            return Err(NetworkReplayError::IngressChannelMismatch {
                bound: channel,
                supplied: input.channel,
            });
        }
        let state = self.channels.get(&channel).expect("bound channel exists");
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let prior_release =
            Self::prior_connection_release(trace, channel, state.published_ingress)?;
        let mut next = state.published_ingress.unwrap_or_default();
        if next.last_release.is_none() {
            next.last_release = prior_release;
        }
        if input.release.not_before_global_time < trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        if next.last_release.is_some_and(|last| {
            input.release.not_before_global_time < last.not_before_global_time
                || input.release.after_transmitted_offset < last.after_transmitted_offset
        }) {
            return Err(NetworkTraceValidationError::NonMonotonicRelease.into());
        }
        // The journal is the sole recorded-output authority for this staged
        // API. Do not consult tool_global's legacy per-OFD progress map. A later
        // physical-send API will maintain this frontier incrementally itself.
        if input.release.after_transmitted_offset > recorded_stream_output_offset(trace, channel)? {
            return Err(NetworkTraceValidationError::UnreachableTransmitWatermark.into());
        }
        if next.terminal {
            return Err(NetworkTraceValidationError::EventAfterTerminal.into());
        }
        let (offset, count, terminal) = match &input.event {
            NetworkInputKindV2::StreamBytes {
                stream_offset,
                bytes,
            } => {
                if bytes.is_empty() {
                    return Err(NetworkTraceValidationError::EmptyByteChunk.into());
                }
                (*stream_offset, bytes.len(), false)
            }
            NetworkInputKindV2::PeerShutdown {
                stream_offset,
                direction,
            } => (
                *stream_offset,
                0,
                matches!(
                    direction,
                    NetworkShutdownV2::Write | NetworkShutdownV2::Both
                ),
            ),
            NetworkInputKindV2::SocketError {
                stream_offset,
                errno,
            } => {
                if !(1..=4095).contains(errno) {
                    return Err(NetworkTraceValidationError::InvalidErrno.into());
                }
                if matches!(*errno, libc::EAGAIN | libc::EINTR | libc::EFAULT) {
                    return Err(NetworkReplayError::ConsumerLocalIngressError(*errno));
                }
                (*stream_offset, 0, false)
            }
            _ => return Err(NetworkReplayError::UnsupportedIngressObservation(channel)),
        };
        if offset != next.stream_offset {
            return Err(NetworkTraceValidationError::NonContiguousInput.into());
        }
        next.stream_offset = next
            .stream_offset
            .checked_add(count as u64)
            .ok_or(NetworkReplayError::Overflow)?;
        next.last_release = Some(input.release);
        next.terminal = terminal;
        input.ordinal = trace.inputs.len() as u64;
        let unit = if self.shadow.is_some() {
            match &input.event {
                NetworkInputKindV2::StreamBytes {
                    stream_offset,
                    bytes,
                } => {
                    if bytes.len() > 1024 || !self.shadow_channel_matches(open_file, channel) {
                        return Err(NetworkReplayError::InvalidShadowPublication(channel));
                    }
                    Some(ReceiveCopyUnitV1 {
                        input_ordinal: input.ordinal,
                        channel,
                        stream_offset: *stream_offset,
                        length: bytes.len() as u64,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        let queued = input.event.clone();
        let input_ordinal = input.ordinal;

        // Exclusive &mut ownership spans this commit. No error return or await
        // follows the first mutation, so callers cannot observe half an append.
        let EngineState::Record(trace) = &mut self.mode else {
            unreachable!()
        };
        trace.inputs.push(input);
        let state = self
            .channels
            .get_mut(&channel)
            .expect("bound channel exists");
        state.release_at(input_ordinal, queued);
        state.published_ingress = Some(next);
        if let Some(unit) = unit {
            self.shadow
                .as_mut()
                .expect("validated shadow")
                .units
                .push(unit);
        }
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
        if self.shadow.is_some() {
            return Err(NetworkReplayError::WrongMode);
        }
        self.check_stream_operations_finished()?;
        let EngineState::Record(trace) = self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        trace.validate()?;
        Ok(trace)
    }

    /// Ensure one endpoint binding without deriving trace identity from an OFD.
    ///
    /// For otherwise indistinguishable peers, Replay chooses the first still
    /// available matching occurrence in trace order. This is a reproducible
    /// environment choice, not recovery of the original thread's connection.
    /// All validation precedes changes to identities, metadata or bindings.
    pub fn ensure_channel(
        &mut self,
        open_file: OpenFileId,
        request: NetworkChannelBinding,
    ) -> Result<NetworkChannelId, NetworkReplayError> {
        if !open_file.is_socket() {
            return Err(NetworkReplayError::NonSocketOpenFile(open_file));
        }
        if self.retired_open_files.contains(&open_file) {
            return Err(NetworkReplayError::OpenFileRetired(open_file));
        }
        self.check_shadow_channel_request(open_file, &request)?;
        match &self.mode {
            EngineState::Record(_) if request.selected_channel.is_some() => {
                return Err(NetworkReplayError::InvalidChannelSelection);
            }
            EngineState::Replay(_) => {
                if request.observed_local_address.is_some() {
                    return Err(NetworkReplayError::ObservedLocalDuringReplay);
                }
                if (request.role == NetworkEndpointRoleV2::Accepted)
                    != request.selected_channel.is_some()
                {
                    return Err(NetworkReplayError::InvalidChannelSelection);
                }
            }
            EngineState::Record(_) => {}
        }

        if let Some(channel) = self.channel_for(open_file) {
            let definition = self
                .channel_definitions()
                .iter()
                .find(|definition| definition.id == channel)
                .expect("bound channel exists");
            if request
                .selected_channel
                .is_some_and(|selected| selected != channel)
                || !binding_matches(definition, &request)
                || !self.shadow_channel_matches(open_file, channel)
            {
                return Err(NetworkReplayError::ChannelEndpointMismatch(channel));
            }
            return Ok(channel);
        }

        match &self.mode {
            EngineState::Record(trace) => {
                if request.requested_local_constraint.is_some()
                    && request.requested_local_constraint != request.observed_local_address
                {
                    return Err(NetworkReplayError::UnverifiedLocalConstraint);
                }
                // Manual legacy RecordChannel entries can coexist during the
                // migration. Skip their IDs without ever consulting an OFD.
                let mut ordinal = self.next_record_channel;
                while self.channels.contains_key(&NetworkChannelId(ordinal)) {
                    ordinal = ordinal.checked_add(1).ok_or(NetworkReplayError::Overflow)?;
                }
                let next = ordinal.checked_add(1).ok_or(NetworkReplayError::Overflow)?;
                let channel = NetworkChannelId(ordinal);
                let definition = NetworkChannelV2 {
                    id: channel,
                    transport: request.transport,
                    role: request.role,
                    local_address: request.observed_local_address,
                    peer_address: request.peer_address,
                    accepted_from: request.accepted_from,
                };
                // Reuse the schema's actual metadata/ancestry validator. This
                // does not revalidate or clone the accumulated payload journal.
                let mut definitions = vec![definition.clone()];
                if definition.role == NetworkEndpointRoleV2::Accepted
                    && let Some(listener) = definition.accepted_from
                {
                    let parent = trace
                        .channels
                        .iter()
                        .find(|channel| channel.id == listener)
                        .ok_or(NetworkReplayError::UnknownChannel(listener))?;
                    definitions.push(parent.clone());
                }
                NetworkTraceV2 {
                    epoch: trace.epoch,
                    channels: definitions,
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                }
                .validate()?;
                let runtime = ChannelState::new(&definition);

                let EngineState::Record(trace) = &mut self.mode else {
                    unreachable!()
                };
                trace.channels.push(definition);
                if let Some(shadow) = &mut self.shadow
                    && let Some(socket) = shadow.sockets.get(&open_file)
                {
                    shadow.channel_classes.insert(channel, socket.key);
                }
                self.channels.insert(channel, runtime);
                self.bindings.insert(open_file, channel);
                self.reverse_bindings.insert(channel, open_file);
                self.next_record_channel = next;
                Ok(channel)
            }
            EngineState::Replay(replay) => {
                let definition = if let Some(selected) = request.selected_channel {
                    let definition = replay
                        .trace
                        .channels
                        .iter()
                        .find(|definition| definition.id == selected)
                        .ok_or(NetworkReplayError::UnknownChannel(selected))?;
                    if !binding_matches(definition, &request)
                        || !self.shadow_channel_matches(open_file, selected)
                    {
                        return Err(NetworkReplayError::ChannelEndpointMismatch(selected));
                    }
                    if self.retired_channels.contains(&selected) {
                        return Err(NetworkReplayError::ChannelRetired(selected));
                    }
                    if self.reverse_bindings.contains_key(&selected) {
                        return Err(NetworkReplayError::ChannelAlreadyBound(selected));
                    }
                    definition
                } else {
                    replay
                        .trace
                        .channels
                        .iter()
                        .find(|definition| {
                            !self.retired_channels.contains(&definition.id)
                                && !self.reverse_bindings.contains_key(&definition.id)
                                && binding_matches(definition, &request)
                                && self.shadow_channel_matches(open_file, definition.id)
                        })
                        .ok_or(NetworkReplayError::NoMatchingChannel)?
                };
                let channel = definition.id;
                self.bindings.insert(open_file, channel);
                self.reverse_bindings.insert(channel, open_file);
                Ok(channel)
            }
        }
    }

    /// Bind one live OFD to one explicit trace channel. Runtime endpoint
    /// discovery uses `ensure_channel` so metadata is validated as well.
    pub fn bind(
        &mut self,
        open_file: OpenFileId,
        channel: NetworkChannelId,
    ) -> Result<(), NetworkReplayError> {
        if !open_file.is_socket() {
            return Err(NetworkReplayError::NonSocketOpenFile(open_file));
        }
        if self.retired_open_files.contains(&open_file) {
            return Err(NetworkReplayError::OpenFileRetired(open_file));
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
        // Descriptor retirement fences new calls immediately. Active syscall
        // references preserve the OFD and queue until their known release.
        self.retired_open_files.insert(open_file);
        self.complete_deferred_retirement(open_file)
    }

    /// Resolve a stable OFD binding.
    pub fn channel_for(&self, open_file: OpenFileId) -> Option<NetworkChannelId> {
        self.bindings.get(&open_file).copied()
    }

    /// Register or confirm one trace-stable ancillary object. Re-registering
    /// the same identity is idempotent; remapping either side fails closed.
    pub fn register_ancillary_object(
        &mut self,
        id: NetworkObjectId,
        open_file: OpenFileId,
        kind: NetworkAncillaryObjectKind,
    ) -> Result<(), NetworkReplayError> {
        if self.retired_ancillary_objects.contains(&id) {
            return Err(NetworkReplayError::AncillaryObjectRetired(id));
        }
        if self.retired_ancillary_open_files.contains(&open_file) {
            return Err(NetworkReplayError::AncillaryOpenFileRetired(open_file));
        }
        if let Some(existing) = self.ancillary_objects.get(&id) {
            return if existing.open_file == open_file && existing.kind == kind {
                Ok(())
            } else {
                Err(NetworkReplayError::AncillaryObjectRemap(id))
            };
        }
        if self.ancillary_by_open_file.contains_key(&open_file) {
            return Err(NetworkReplayError::AncillaryOpenFileAlreadyRegistered(
                open_file,
            ));
        }
        self.ancillary_objects.insert(
            id,
            AncillaryObjectState {
                open_file,
                kind,
                alias_count: 0,
            },
        );
        self.ancillary_by_open_file.insert(open_file, id);
        Ok(())
    }

    /// Resolve an active ancillary object for SCM_RIGHTS installation.
    pub fn ancillary_object(
        &self,
        id: NetworkObjectId,
    ) -> Result<NetworkAncillaryObject, NetworkReplayError> {
        if self.retired_ancillary_objects.contains(&id) {
            return Err(NetworkReplayError::AncillaryObjectRetired(id));
        }
        let state = self
            .ancillary_objects
            .get(&id)
            .ok_or(NetworkReplayError::UnknownAncillaryObject(id))?;
        Ok(NetworkAncillaryObject {
            id,
            open_file: state.open_file,
            kind: state.kind,
            alias_count: state.alias_count,
        })
    }

    /// Reverse-resolve an active trace object from an OFD.
    pub fn ancillary_object_for_open_file(
        &self,
        open_file: OpenFileId,
    ) -> Result<NetworkAncillaryObject, NetworkReplayError> {
        let id = self
            .ancillary_by_open_file
            .get(&open_file)
            .copied()
            .ok_or(NetworkReplayError::UnknownAncillaryOpenFile(open_file))?;
        self.ancillary_object(id)
    }

    /// Account for one newly installed fd alias and return its stable OFD.
    pub fn retain_ancillary_alias(
        &mut self,
        id: NetworkObjectId,
    ) -> Result<OpenFileId, NetworkReplayError> {
        if self.retired_ancillary_objects.contains(&id) {
            return Err(NetworkReplayError::AncillaryObjectRetired(id));
        }
        let state = self
            .ancillary_objects
            .get_mut(&id)
            .ok_or(NetworkReplayError::UnknownAncillaryObject(id))?;
        state.alias_count = state
            .alias_count
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        Ok(state.open_file)
    }

    /// Account for one closed fd alias without retiring the trace object.
    pub fn release_ancillary_alias(
        &mut self,
        id: NetworkObjectId,
    ) -> Result<u64, NetworkReplayError> {
        let state = self.ancillary_objects.get_mut(&id).ok_or_else(|| {
            if self.retired_ancillary_objects.contains(&id) {
                NetworkReplayError::AncillaryObjectRetired(id)
            } else {
                NetworkReplayError::UnknownAncillaryObject(id)
            }
        })?;
        state.alias_count = state
            .alias_count
            .checked_sub(1)
            .ok_or(NetworkReplayError::AncillaryAliasUnderflow(id))?;
        Ok(state.alias_count)
    }

    /// Permanently retire an object after all installed aliases have closed.
    pub fn retire_ancillary_object(
        &mut self,
        id: NetworkObjectId,
    ) -> Result<OpenFileId, NetworkReplayError> {
        if self.retired_ancillary_objects.contains(&id) {
            return Err(NetworkReplayError::AncillaryObjectRetired(id));
        }
        let state = self
            .ancillary_objects
            .get(&id)
            .ok_or(NetworkReplayError::UnknownAncillaryObject(id))?;
        if state.alias_count != 0 {
            return Err(NetworkReplayError::AncillaryAliasesRemain {
                id,
                count: state.alias_count,
            });
        }
        let state = self.ancillary_objects.remove(&id).unwrap();
        self.ancillary_by_open_file.remove(&state.open_file);
        self.retired_ancillary_objects.insert(id);
        self.retired_ancillary_open_files.insert(state.open_file);
        Ok(state.open_file)
    }

    /// Register a network OFD with an epoll OFD (`EPOLL_CTL_ADD`).
    pub fn epoll_add(
        &mut self,
        epoll: OpenFileId,
        target: OpenFileId,
        events: u32,
    ) -> Result<(), NetworkReplayError> {
        self.validate_epoll_registration(epoll, target, events)?;
        let generation = self.readiness_state(target)?.1;
        let interests = self.epolls.entry(epoll).or_default();
        if interests.contains_key(&target) {
            return Err(NetworkReplayError::EpollInterestAlreadyExists { epoll, target });
        }
        interests.insert(target, EpollInterestState::new(events, generation, false));
        Ok(())
    }

    /// Replace an interest and rearm `EPOLLONESHOT` (`EPOLL_CTL_MOD`).
    pub fn epoll_modify(
        &mut self,
        epoll: OpenFileId,
        target: OpenFileId,
        events: u32,
    ) -> Result<(), NetworkReplayError> {
        self.validate_epoll_registration(epoll, target, events)?;
        let generation = self.readiness_state(target)?.1;
        let interest = self
            .epolls
            .get_mut(&epoll)
            .and_then(|interests| interests.get_mut(&target))
            .ok_or(NetworkReplayError::UnknownEpollInterest { epoll, target })?;
        *interest = EpollInterestState::new(events, generation, true);
        Ok(())
    }

    /// Delete an interest (`EPOLL_CTL_DEL`).
    pub fn epoll_delete(
        &mut self,
        epoll: OpenFileId,
        target: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        let interests = self
            .epolls
            .get_mut(&epoll)
            .ok_or(NetworkReplayError::UnknownEpollInterest { epoll, target })?;
        if interests.remove(&target).is_none() {
            return Err(NetworkReplayError::UnknownEpollInterest { epoll, target });
        }
        if interests.is_empty() {
            self.epolls.remove(&epoll);
        }
        Ok(())
    }

    /// Return current events in deterministic target-OFD order.
    pub fn epoll_ready(
        &mut self,
        epoll: OpenFileId,
    ) -> Result<Vec<NetworkEpollEvent>, NetworkReplayError> {
        let targets: Vec<_> = self
            .epolls
            .get(&epoll)
            .ok_or(NetworkReplayError::UnknownEpollInstance(epoll))?
            .keys()
            .copied()
            .collect();
        let snapshots: BTreeMap<_, _> = targets
            .into_iter()
            .map(|target| self.readiness_state(target).map(|state| (target, state)))
            .collect::<Result<_, _>>()?;
        let interests = self.epolls.get_mut(&epoll).unwrap();
        let mut ready = Vec::new();
        for (target, interest) in interests {
            if !interest.enabled {
                continue;
            }
            let (readiness, generation) = snapshots[target];
            let current = epoll_events(readiness, interest.events);
            let transitioned = !interest.edge_triggered
                || readiness_transitioned(readiness, generation, interest.seen, interest.events);
            if current != 0 && transitioned {
                ready.push(NetworkEpollEvent {
                    target: *target,
                    events: current,
                });
                interest.seen = generation;
                if interest.one_shot {
                    interest.enabled = false;
                }
            }
        }
        Ok(ready)
    }

    /// Release every currently eligible external observation.
    ///
    /// Events on different channels do not block one another. Within a channel,
    /// a later event cannot overtake an earlier unavailable event.
    pub fn release_eligible(
        &mut self,
        now: LogicalTime,
    ) -> Result<BTreeSet<NetworkChannelId>, NetworkReplayError> {
        let mut ready = self.release_accepted_children(now)?;
        let EngineState::Replay(replay_view) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let accepted_ready: Vec<bool> = replay_view
            .trace
            .inputs
            .iter()
            .map(|input| self.accepted_input_ready(input.channel, &input.event))
            .collect();
        let EngineState::Replay(replay) = &mut self.mode else {
            unreachable!()
        };
        let mut blocked = BTreeSet::new();
        for index in 0..replay.trace.inputs.len() {
            if replay.released[index] {
                continue;
            }
            let event = &replay.trace.inputs[index];
            if blocked.contains(&event.channel) {
                continue;
            }
            let channel = self
                .channels
                .get(&event.channel)
                .expect("validated channel");
            if !accepted_ready[index] || !event.release.is_eligible(now, channel.transmitted) {
                blocked.insert(event.channel);
                continue;
            }
            let event = event.clone();
            self.channels
                .get_mut(&event.channel)
                .expect("validated channel")
                .release_at(event.ordinal, event.event);
            replay.released[index] = true;
            ready.insert(event.channel);
        }
        let children = self.release_accepted_children(now)?;
        if !children.is_empty() {
            ready.extend(children);
            ready.extend(self.release_eligible(now)?);
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
        let mut earliest: Option<LogicalTime> = self.next_child_release();
        for (index, event) in replay.trace.inputs.iter().enumerate() {
            if replay.released[index] || blocked.contains(&event.channel) {
                continue;
            }
            let channel = self
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
        self.check_unreserved_stream_delivery(open_file)?;
        let supported = libc::MSG_PEEK | libc::MSG_WAITALL | libc::MSG_DONTWAIT;
        if options.flags & !supported != 0 {
            return Err(NetworkReplayError::UnsupportedReceiveFlags(
                options.flags & !supported,
            ));
        }
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
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
        self.check_unreserved_stream_delivery(open_file)?;
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel_mut(channel)?;
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
                    state.refresh_readiness();
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
                        state.refresh_readiness();
                        return Ok(StreamReceiveOutcome::EndOfFile);
                    }
                }
                Some(_) => break,
                None => break,
            }
        }
        state.refresh_readiness();
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
        self.check_unreserved_stream_delivery(open_file)?;
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel_mut(channel)?;
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let outcome = match state.inbound.front_mut() {
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
        };
        state.refresh_readiness();
        outcome
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
        let state = self.runtime_channel_mut(channel)?;
        if !state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let outcome = match state.inbound.front() {
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
        };
        state.refresh_readiness();
        outcome
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
        let outcome = match state.outbound.front_mut() {
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
        };
        state.refresh_readiness();
        outcome
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
        state.refresh_readiness();
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
        state.refresh_readiness();
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
        state.refresh_readiness();
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
        state.refresh_readiness();
        Ok(())
    }

    /// Consume one ready connect or accept observation.
    pub fn take_connection_outcome(
        &mut self,
        open_file: OpenFileId,
    ) -> Result<Option<ConnectionOutcome>, NetworkReplayError> {
        self.check_unreserved_stream_delivery(open_file)?;
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel_mut(channel)?;
        if !matches!(state.inbound.front(), Some(InboundOutcome::Control(_))) {
            return Ok(None);
        }
        let Some(InboundOutcome::Control(outcome)) = state.inbound.pop_front() else {
            unreachable!()
        };
        state.refresh_readiness();
        Ok(Some(outcome))
    }

    /// Readiness derived from available modeled state, not waiter identity.
    pub fn readiness(
        &self,
        open_file: OpenFileId,
    ) -> Result<NetworkReadinessV2, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
        let mut readiness = state.readiness;
        if self
            .shadow
            .as_ref()
            .is_some_and(|shadow| shadow.channel_classes.contains_key(&channel))
        {
            readiness.readable |= state.local_read_shutdown;
            readiness.hangup = state.explicit_readiness.hangup
                || state.local_write_closed && state.receive_half_closed();
        }
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
        self.finish_fd_mutations()?;
        self.check_stream_operations_finished()?;
        let EngineState::Replay(replay) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        if replay.released.iter().any(|released| !released) {
            return Err(NetworkReplayError::UnconsumedTrace);
        }
        for (channel, state) in &self.channels {
            if !state.inbound.is_empty() || !state.outbound.is_empty() {
                return Err(NetworkReplayError::UnconsumedChannel(*channel));
            }
        }
        Ok(())
    }

    fn has_channel(&self, channel: NetworkChannelId) -> bool {
        match &self.mode {
            EngineState::Record(trace) => trace.channels.iter().any(|item| item.id == channel),
            EngineState::Replay(_) => self.channels.contains_key(&channel),
        }
    }

    fn channel_definitions(&self) -> &[NetworkChannelV2] {
        match &self.mode {
            EngineState::Record(trace) => &trace.channels,
            EngineState::Replay(replay) => &replay.trace.channels,
        }
    }

    fn bound_channel(&self, open_file: OpenFileId) -> Result<NetworkChannelId, NetworkReplayError> {
        self.bindings
            .get(&open_file)
            .copied()
            .ok_or(NetworkReplayError::UnboundOpenFile(open_file))
    }

    fn readiness_state(
        &self,
        open_file: OpenFileId,
    ) -> Result<(NetworkReadinessV2, ReadinessGeneration), NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        let state = self.runtime_channel(channel)?;
        Ok((state.readiness, state.readiness_generation))
    }

    fn validate_epoll_registration(
        &self,
        epoll: OpenFileId,
        target: OpenFileId,
        events: u32,
    ) -> Result<(), NetworkReplayError> {
        if epoll == target {
            return Err(NetworkReplayError::EpollSelfRegistration(epoll));
        }
        let exclusive = libc::EPOLLEXCLUSIVE as u32;
        if events & exclusive != 0 {
            return Err(NetworkReplayError::UnsupportedEpollFlags(exclusive));
        }
        let supported = (libc::EPOLLIN
            | libc::EPOLLOUT
            | libc::EPOLLERR
            | libc::EPOLLHUP
            | libc::EPOLLRDHUP
            | libc::EPOLLET
            | libc::EPOLLONESHOT) as u32;
        if events & !supported != 0 {
            return Err(NetworkReplayError::UnsupportedEpollFlags(
                events & !supported,
            ));
        }
        self.readiness_state(target)?;
        Ok(())
    }

    fn runtime_channel(
        &self,
        channel: NetworkChannelId,
    ) -> Result<&ChannelState, NetworkReplayError> {
        self.channels
            .get(&channel)
            .ok_or(NetworkReplayError::UnknownChannel(channel))
    }

    fn runtime_channel_mut(
        &mut self,
        channel: NetworkChannelId,
    ) -> Result<&mut ChannelState, NetworkReplayError> {
        self.channels
            .get_mut(&channel)
            .ok_or(NetworkReplayError::UnknownChannel(channel))
    }

    fn replay_channel_mut(
        &mut self,
        channel: NetworkChannelId,
    ) -> Result<&mut ChannelState, NetworkReplayError> {
        if !matches!(self.mode, EngineState::Replay(_)) {
            return Err(NetworkReplayError::WrongMode);
        }
        self.runtime_channel_mut(channel)
    }
}

impl ChannelState {
    fn new(channel: &NetworkChannelV2) -> Self {
        Self {
            transport: channel.transport,
            inbound_consumed: 0,
            receive_input_generation: None,
            inbound: VecDeque::new(),
            explicit_readiness: NetworkReadinessV2::default(),
            transmitted: 0,
            outbound: VecDeque::new(),
            local_write_closed: false,
            local_read_shutdown: false,
            local_control_generation: 0,
            peer_write_closed: false,
            readiness: NetworkReadinessV2::default(),
            readiness_generation: ReadinessGeneration::default(),
            published_ingress: None,
        }
    }

    fn receive_half_closed(&self) -> bool {
        self.local_read_shutdown
            || self.peer_write_closed
            || self.inbound.iter().any(|input| {
                matches!(
                    input,
                    InboundOutcome::PeerShutdown {
                        direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                        ..
                    }
                )
            })
    }

    fn computed_readiness(&self) -> NetworkReadinessV2 {
        let mut readiness = self.explicit_readiness;
        readiness.readable |= !self.inbound.is_empty() || self.peer_write_closed;
        readiness.writable |= !self.local_write_closed && !self.outbound.is_empty();
        readiness.error |= matches!(self.inbound.front(), Some(InboundOutcome::Error { .. }));
        readiness.hangup |= self.peer_write_closed
            || matches!(
                self.inbound.front(),
                Some(InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                })
            );
        readiness
    }

    fn refresh_readiness(&mut self) {
        let next = self.computed_readiness();
        if !self.readiness.readable && next.readable {
            self.readiness_generation.readable += 1;
        }
        if !self.readiness.writable && next.writable {
            self.readiness_generation.writable += 1;
        }
        if !self.readiness.error && next.error {
            self.readiness_generation.error += 1;
        }
        if !self.readiness.hangup && next.hangup {
            self.readiness_generation.hangup += 1;
        }
        self.readiness = next;
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

    fn release_at(&mut self, ordinal: u64, input: NetworkInputKindV2) {
        if matches!(
            &input,
            NetworkInputKindV2::StreamBytes { .. }
                | NetworkInputKindV2::StreamMessage { .. }
                | NetworkInputKindV2::SocketError { .. }
                | NetworkInputKindV2::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                }
        ) {
            assert!(
                self.receive_input_generation
                    .is_none_or(|prior| ordinal > prior)
            );
            self.receive_input_generation = Some(ordinal);
        }
        self.release(input);
    }

    fn release(&mut self, input: NetworkInputKindV2) {
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
        self.refresh_readiness();
    }
}

fn binding_matches(definition: &NetworkChannelV2, request: &NetworkChannelBinding) -> bool {
    definition.transport == request.transport
        && definition.role == request.role
        && definition.peer_address == request.peer_address
        && definition.accepted_from == request.accepted_from
        && request
            .requested_local_constraint
            .as_ref()
            .is_none_or(|constraint| definition.local_address.as_ref() == Some(constraint))
        && request
            .observed_local_address
            .as_ref()
            .is_none_or(|observed| definition.local_address.as_ref() == Some(observed))
}

/// Derive the actual recorded transmit frontier without duplicating the legacy
/// adapter's progress map. This staged input API is not yet on the runtime hot
/// path; physical-send publication will own an incremental frontier later.
fn recorded_stream_output_offset(
    trace: &NetworkTraceV2,
    channel: NetworkChannelId,
) -> Result<u64, NetworkReplayError> {
    let mut offset = 0u64;
    for output in trace
        .outputs
        .iter()
        .filter(|output| output.channel == channel)
    {
        let (at, count) = match &output.event {
            NetworkOutputKindV2::StreamBytes {
                stream_offset,
                bytes,
            }
            | NetworkOutputKindV2::StreamMessage {
                stream_offset,
                bytes,
                ..
            } => (*stream_offset, bytes.len()),
            NetworkOutputKindV2::SocketError { stream_offset, .. }
            | NetworkOutputKindV2::Shutdown { stream_offset, .. } => (*stream_offset, 0),
            _ => return Err(NetworkReplayError::TransportMismatch(channel)),
        };
        if at != offset {
            return Err(NetworkTraceValidationError::NonContiguousOutput.into());
        }
        offset = offset
            .checked_add(count as u64)
            .ok_or(NetworkReplayError::Overflow)?;
    }
    Ok(offset)
}

impl NetworkReplayEngine {
    fn check_stream_owner(&self, owner: NetworkStreamOwner) -> Result<(), NetworkReplayError> {
        if self.gone_stream_owners.contains(&owner) {
            Err(NetworkReplayError::StreamOwnerGone(owner))
        } else {
            Ok(())
        }
    }

    fn allocate_stream_lease(&mut self) -> Result<NetworkStreamLeaseId, NetworkReplayError> {
        let next = self
            .next_stream_lease
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let lease = NetworkStreamLeaseId(self.next_stream_lease);
        self.next_stream_lease = next;
        Ok(lease)
    }

    fn owned_stream_operation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
    ) -> Result<&StreamOperation, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let operation = self
            .stream_operations
            .get(&lease)
            .ok_or(NetworkReplayError::UnknownStreamLease(lease))?;
        if operation.owner != owner {
            return Err(NetworkReplayError::StreamLeaseOwnerMismatch(lease));
        }
        if operation.abandoned {
            return Err(NetworkReplayError::UnresolvedStreamOperation(lease));
        }
        Ok(operation)
    }

    fn stream_role(
        &self,
        channel: NetworkChannelId,
    ) -> Result<NetworkEndpointRoleV2, NetworkReplayError> {
        self.channel_definitions()
            .iter()
            .find(|definition| definition.id == channel)
            .map(|definition| definition.role)
            .ok_or(NetworkReplayError::UnknownChannel(channel))
    }

    /// Latch possible kernel effects before submitting a consuming receive.
    /// The global wrapper owns contention waiting; this method never blocks.
    pub fn begin_stream_ingress(
        &mut self,
        owner: NetworkStreamOwner,
        open_file: OpenFileId,
        began: LogicalTime,
    ) -> Result<NetworkStreamLeaseId, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        self.check_socket_control_available(open_file)?;
        let EngineState::Record(trace) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let channel = self.bound_channel(open_file)?;
        let state = self.channels.get(&channel).expect("bound channel exists");
        if state.transport.is_datagram()
            || self.stream_role(channel)? == NetworkEndpointRoleV2::Listener
        {
            return Err(NetworkReplayError::UnsupportedIngressObservation(channel));
        }
        if let Some(lease) = self.stream_ingress.get(&open_file) {
            return Err(
                if self
                    .stream_operations
                    .get(lease)
                    .is_some_and(|op| op.abandoned)
                {
                    NetworkReplayError::UnresolvedStreamOperation(*lease)
                } else {
                    NetworkReplayError::StreamOperationBusy(*lease)
                },
            );
        }
        if state.published_ingress.is_some_and(|p| p.terminal) {
            return Err(NetworkTraceValidationError::EventAfterTerminal.into());
        }
        if began < trace.epoch_global_time()? {
            return Err(NetworkTraceValidationError::ReleaseBeforeEpoch.into());
        }
        // Connected is a previously delivered control outcome, not stream data.
        // Validate this one boundary explicitly; arbitrary legacy input remains
        // incompatible. No existing mixed-data check is suppressed.
        Self::prior_connection_release(trace, channel, state.published_ingress)?;
        let lease = self.allocate_stream_lease()?;
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner,
                open_file,
                channel,
                abandoned: false,
                kind: StreamOperationKind::SubmittedIngress { began },
            },
        );
        self.stream_ingress.insert(open_file, lease);
        Ok(lease)
    }

    /// Complete with the paid coordinator clock sampled by this RPC. The
    /// caller supplies no trace offset, channel, ordinal, or output watermark.
    /// On any rejection the submitted receipt remains unresolved.
    pub fn complete_stream_ingress(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        observed_at: LogicalTime,
        observation: NetworkIngressObservation,
    ) -> Result<(), NetworkReplayError> {
        let operation = self.owned_stream_operation(owner, lease)?.clone();
        let StreamOperationKind::SubmittedIngress { began } = operation.kind else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if observed_at < began {
            return Err(NetworkTraceValidationError::NonMonotonicRelease.into());
        }
        let EngineState::Record(trace) = &self.mode else {
            return Err(NetworkReplayError::WrongMode);
        };
        let state = self
            .channels
            .get(&operation.channel)
            .expect("receipt pins channel");
        let offset = state.published_ingress.map_or(0, |p| p.stream_offset);
        let event = match observation {
            NetworkIngressObservation::Bytes(bytes) => Some(NetworkInputKindV2::StreamBytes {
                stream_offset: offset,
                bytes,
            }),
            NetworkIngressObservation::EndOfFile => Some(NetworkInputKindV2::PeerShutdown {
                stream_offset: offset,
                direction: NetworkShutdownV2::Write,
            }),
            NetworkIngressObservation::TransportError(errno) => {
                Some(NetworkInputKindV2::SocketError {
                    stream_offset: offset,
                    errno,
                })
            }
            NetworkIngressObservation::NoArrival | NetworkIngressObservation::Interrupted => None,
        };
        if let Some(event) = event {
            let watermark = recorded_stream_output_offset(trace, operation.channel)?;
            self.publish_ingress(
                operation.open_file,
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel: operation.channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: observed_at,
                        after_transmitted_offset: watermark,
                    },
                    event,
                },
            )?;
        }
        self.stream_ingress.remove(&operation.open_file);
        self.stream_operations.remove(&lease);
        self.complete_deferred_retirement(operation.open_file);
        Ok(())
    }

    fn prior_connection_release(
        trace: &NetworkTraceV2,
        channel: NetworkChannelId,
        published: Option<PublishedIngress>,
    ) -> Result<Option<NetworkReleaseV2>, NetworkReplayError> {
        if let Some(published) = published {
            return Ok(published.last_release);
        }
        let mut prior = None;
        for input in trace.inputs.iter().filter(|input| input.channel == channel) {
            if prior.is_some()
                || !matches!(
                    input.event,
                    NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected)
                )
            {
                return Err(NetworkReplayError::MixedIngressCapture(channel));
            }
            prior = Some(input.release);
        }
        Ok(prior)
    }

    /// Inspect payload and terminal facts without claiming consumer ownership.
    /// A delivery reservation is reported separately; it is never idle data.
    pub fn stream_queue_status(
        &self,
        open_file: OpenFileId,
    ) -> Result<NetworkStreamQueueStatus, NetworkReplayError> {
        let channel = self.bound_channel(open_file)?;
        if let Some(control) = self.socket_controls.get(&open_file)
            && control.abandoned
        {
            return Err(NetworkReplayError::UnresolvedStreamOperation(control.lease));
        }
        for lease in [
            self.stream_ingress.get(&open_file),
            self.stream_delivery.get(&open_file),
        ]
        .into_iter()
        .flatten()
        {
            if self
                .stream_operations
                .get(lease)
                .is_some_and(|operation| operation.abandoned)
            {
                return Err(NetworkReplayError::UnresolvedStreamOperation(*lease));
            }
        }
        let state = self.channels.get(&channel).expect("bound channel exists");
        if state.transport.is_datagram() {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let mut queued_bytes = 0usize;
        let mut eof = state.peer_write_closed
            || state.inbound.iter().any(|item| {
                matches!(
                    item,
                    InboundOutcome::PeerShutdown {
                        direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                        ..
                    }
                )
            });
        let mut error = None;
        for item in &state.inbound {
            match item {
                InboundOutcome::Stream { bytes, .. } => {
                    queued_bytes = queued_bytes
                        .checked_add(bytes.len())
                        .ok_or(NetworkReplayError::Overflow)?;
                }
                InboundOutcome::Error { errno, .. } => {
                    error = Some(*errno);
                    break;
                }
                InboundOutcome::PeerShutdown { direction, .. } => {
                    eof |= matches!(
                        direction,
                        NetworkShutdownV2::Write | NetworkShutdownV2::Both
                    );
                    break;
                }
                _ => break,
            }
        }
        Ok(NetworkStreamQueueStatus {
            consume_epoch: self
                .shadow
                .as_ref()
                .and_then(|state| state.sockets.get(&open_file))
                .map_or(0, |state| state.consume_epoch),
            queued_bytes,
            eof,
            local_read_shutdown: state.local_read_shutdown,
            readiness: self.readiness(open_file)?,
            error,
            listener: self.stream_role(channel)? == NetworkEndpointRoleV2::Listener,
            ingress_busy: self.stream_ingress.contains_key(&open_file),
            delivery_busy: self.stream_delivery.contains_key(&open_file)
                || self.socket_controls.contains_key(&open_file),
        })
    }

    /// Poll uses the current socket option, unlike a receive operation's
    /// entry-fixed target. An admitted call may still own this state after
    /// final descriptor close, so this is not a new bare-descriptor admission.
    pub fn poll_readable(&self, open_file: OpenFileId) -> Result<bool, NetworkReplayError> {
        let minimum = self
            .shadow
            .as_ref()
            .and_then(|shadow| shadow.sockets.get(&open_file))
            .ok_or(NetworkReplayError::UnregisteredStreamSocket(open_file))?
            .options
            .receive_low_water as usize;
        self.poll_readable_at_least(open_file, minimum)
    }

    /// V3 poll readable threshold includes unconditional ERR/HUP reporting.
    /// A readiness bit is not itself a consumable receive error.
    pub fn poll_readable_at_least(
        &self,
        open_file: OpenFileId,
        minimum: usize,
    ) -> Result<bool, NetworkReplayError> {
        let status = self.stream_queue_status(open_file)?;
        Ok(!status.delivery_busy
            && (status.queued_bytes >= minimum.max(1)
                || status.eof
                || status.local_read_shutdown
                || status.error.is_some()
                || status.readiness.error
                || status.readiness.hangup))
    }

    /// Scheduler threshold predicate. Busy delivery cannot admit a waiter;
    /// release/owner-death is handled as an ownership event, not readiness.
    pub fn stream_ready_at_least(
        &self,
        open_file: OpenFileId,
        minimum: usize,
    ) -> Result<bool, NetworkReplayError> {
        let status = self.stream_queue_status(open_file)?;
        Ok(!status.delivery_busy
            && (status.queued_bytes >= minimum.max(1)
                || status.eof
                || status.local_read_shutdown
                || status.error.is_some()))
    }

    /// Reserve one bounded, immutable payload fragment or terminal observation.
    /// Appended ingress may proceed, but no other delivery consumes its prefix.
    pub fn reserve_stream_chunk(
        &mut self,
        owner: NetworkStreamOwner,
        open_file: OpenFileId,
        maximum: usize,
        peek_offset: usize,
    ) -> Result<NetworkStreamChunk, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        self.check_socket_control_available(open_file)?;
        let channel = self.bound_channel(open_file)?;
        if let Some(lease) = self.stream_delivery.get(&open_file) {
            return Err(
                if self
                    .stream_operations
                    .get(lease)
                    .is_some_and(|op| op.abandoned)
                {
                    NetworkReplayError::UnresolvedStreamOperation(*lease)
                } else {
                    NetworkReplayError::StreamOperationBusy(*lease)
                },
            );
        }
        let state = self.channels.get(&channel).expect("bound channel exists");
        if state.transport.is_datagram()
            || self.stream_role(channel)? == NetworkEndpointRoleV2::Listener
        {
            return Err(NetworkReplayError::TransportMismatch(channel));
        }
        let at_offset = state.inbound_consumed;
        let mut skip = peek_offset;
        let mut outcome = None;
        let mut selection_len = 0;
        for item in &state.inbound {
            match item {
                InboundOutcome::Stream {
                    bytes,
                    requires_message_io,
                    ..
                } => {
                    if *requires_message_io {
                        return Err(NetworkReplayError::AncillaryRequiresMessageIo(channel));
                    }
                    if skip >= bytes.len() && skip != 0 {
                        skip -= bytes.len();
                        continue;
                    }
                    selection_len = maximum.min(bytes.len() - skip);
                    outcome = Some(NetworkStreamChunkOutcome::Bytes(
                        bytes
                            .iter()
                            .skip(skip)
                            .take(selection_len.min(NETWORK_STREAM_CHUNK_LIMIT))
                            .copied()
                            .collect(),
                    ));
                    break;
                }
                InboundOutcome::Error { errno, .. } => {
                    outcome = Some(NetworkStreamChunkOutcome::Error(*errno));
                    break;
                }
                InboundOutcome::PeerShutdown {
                    direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                    ..
                } => {
                    outcome = Some(NetworkStreamChunkOutcome::EndOfFile);
                    break;
                }
                InboundOutcome::Control(_) | InboundOutcome::Datagram { .. } => {
                    return Err(NetworkReplayError::OperationOrderMismatch(channel));
                }
                _ => break,
            }
        }
        if outcome.is_none() && state.peer_write_closed {
            outcome = Some(NetworkStreamChunkOutcome::EndOfFile);
        }
        let Some(outcome) = outcome else {
            return Ok(NetworkStreamChunk::Empty);
        };
        let lease = self.allocate_stream_lease()?;
        self.stream_operations.insert(
            lease,
            StreamOperation {
                owner,
                open_file,
                channel,
                abandoned: false,
                kind: StreamOperationKind::Delivery {
                    at_offset,
                    peek_offset,
                    selection_len,
                    outcome: outcome.clone(),
                },
            },
        );
        self.stream_delivery.insert(open_file, lease);
        Ok(NetworkStreamChunk::Reserved {
            lease,
            selection_len,
            outcome,
        })
    }

    /// Read a bounded view of the already reserved immutable selection. The
    /// whole selection stays reserved until one final acknowledgement, so a
    /// fault on a later RPC view cannot commit an earlier part of this unit.
    pub fn read_stream_chunk_view(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        offset: usize,
        maximum: usize,
    ) -> Result<Vec<u8>, NetworkReplayError> {
        if maximum > NETWORK_STREAM_CHUNK_LIMIT {
            return Err(NetworkReplayError::StreamChunkTooLarge(maximum));
        }
        let operation = self.owned_stream_operation(owner, lease)?;
        let StreamOperationKind::Delivery {
            at_offset,
            peek_offset,
            selection_len,
            outcome: NetworkStreamChunkOutcome::Bytes(_),
        } = &operation.kind
        else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        if offset > *selection_len {
            return Err(NetworkReplayError::StreamDeliveryChanged(lease));
        }
        let state = self
            .channels
            .get(&operation.channel)
            .expect("receipt pins channel");
        if state.inbound_consumed != *at_offset {
            return Err(NetworkReplayError::StreamDeliveryChanged(lease));
        }
        let mut skip = peek_offset
            .checked_add(offset)
            .ok_or(NetworkReplayError::Overflow)?;
        let wanted = maximum.min(selection_len - offset);
        if wanted == 0 {
            return Ok(Vec::new());
        }
        for item in &state.inbound {
            let InboundOutcome::Stream {
                bytes,
                requires_message_io: false,
                ..
            } = item
            else {
                return Err(NetworkReplayError::StreamDeliveryChanged(lease));
            };
            if skip >= bytes.len() {
                skip -= bytes.len();
                continue;
            }
            if bytes.len() - skip < wanted {
                return Err(NetworkReplayError::StreamDeliveryChanged(lease));
            }
            return Ok(bytes.iter().skip(skip).take(wanted).copied().collect());
        }
        Err(NetworkReplayError::StreamDeliveryChanged(lease))
    }

    /// Acknowledge the whole selected unit after all bounded views. The adapter must separately prove
    /// which Linux copy unit was accepted; memory-written count is insufficient.
    pub fn finish_stream_chunk(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        disposition: NetworkStreamChunkDisposition,
    ) -> Result<(), NetworkReplayError> {
        self.finish_stream_chunk_inner(owner, lease, disposition, false)
    }

    fn finish_stream_chunk_inner(
        &mut self,
        owner: NetworkStreamOwner,
        lease: NetworkStreamLeaseId,
        disposition: NetworkStreamChunkDisposition,
        record_drain: bool,
    ) -> Result<(), NetworkReplayError> {
        self.validate_shadow_delivery_finish(owner, lease, disposition, record_drain)?;
        let operation = self.owned_stream_operation(owner, lease)?.clone();
        let StreamOperationKind::Delivery {
            at_offset,
            peek_offset,
            selection_len,
            outcome,
        } = &operation.kind
        else {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        };
        let state = self
            .channels
            .get(&operation.channel)
            .expect("receipt pins channel");
        if state.inbound_consumed != *at_offset {
            return Err(NetworkReplayError::StreamDeliveryChanged(lease));
        }
        if disposition == NetworkStreamChunkDisposition::Consumed {
            if *peek_offset != 0 && !matches!(outcome, NetworkStreamChunkOutcome::Error(_)) {
                return Err(NetworkReplayError::StreamDeliveryChanged(lease));
            }
            let error_index = state
                .inbound
                .iter()
                .position(|item| matches!(item, InboundOutcome::Error { .. }));
            // All checks precede the first mutation; publishing later data cannot
            // invalidate this exact prefix or make an error follow guest effects.
            let count = match outcome {
                NetworkStreamChunkOutcome::Bytes(expected) => {
                    let Some(InboundOutcome::Stream { bytes, .. }) = state.inbound.front() else {
                        return Err(NetworkReplayError::StreamDeliveryChanged(lease));
                    };
                    if !bytes.iter().take(expected.len()).eq(expected.iter())
                        || bytes.len() < *selection_len
                    {
                        return Err(NetworkReplayError::StreamDeliveryChanged(lease));
                    }
                    *selection_len
                }
                NetworkStreamChunkOutcome::Error(expected) => {
                    if !matches!(error_index.and_then(|index|state.inbound.get(index)), Some(InboundOutcome::Error { errno, .. }) if errno == expected)
                    {
                        return Err(NetworkReplayError::StreamDeliveryChanged(lease));
                    }
                    0
                }
                NetworkStreamChunkOutcome::EndOfFile => {
                    if !state.peer_write_closed
                        && !matches!(
                            state.inbound.front(),
                            Some(InboundOutcome::PeerShutdown {
                                direction: NetworkShutdownV2::Write | NetworkShutdownV2::Both,
                                ..
                            })
                        )
                    {
                        return Err(NetworkReplayError::StreamDeliveryChanged(lease));
                    }
                    0
                }
            };
            let new_offset = at_offset
                .checked_add(count as u64)
                .ok_or(NetworkReplayError::Overflow)?;
            let state = self
                .channels
                .get_mut(&operation.channel)
                .expect("receipt pins channel");
            match outcome {
                NetworkStreamChunkOutcome::Bytes(_) => {
                    let Some(InboundOutcome::Stream { bytes, .. }) = state.inbound.front_mut()
                    else {
                        unreachable!()
                    };
                    bytes.drain(..count);
                    if bytes.is_empty() {
                        state.inbound.pop_front();
                    }
                    state.inbound_consumed = new_offset;
                }
                NetworkStreamChunkOutcome::Error(_) => {
                    state
                        .inbound
                        .remove(error_index.expect("validated error index"));
                }
                NetworkStreamChunkOutcome::EndOfFile => {
                    if matches!(
                        state.inbound.front(),
                        Some(InboundOutcome::PeerShutdown { .. })
                    ) {
                        state.inbound.pop_front();
                    }
                    state.peer_write_closed = true;
                }
            }
            state.refresh_readiness();
        } else if !matches!(outcome, NetworkStreamChunkOutcome::Bytes(_))
            && !(disposition == NetworkStreamChunkDisposition::Peeked
                && matches!(outcome, NetworkStreamChunkOutcome::EndOfFile))
        {
            return Err(NetworkReplayError::StreamLeaseKindMismatch(lease));
        }
        self.commit_shadow_delivery_finish(
            lease,
            operation.open_file,
            disposition,
            matches!(outcome, NetworkStreamChunkOutcome::Bytes(_)),
        );
        self.stream_delivery.remove(&operation.open_file);
        self.stream_operations.remove(&lease);
        self.complete_deferred_retirement(operation.open_file);
        Ok(())
    }

    /// Backend-confirmed death of this exact owner. This wakes waiters through
    /// the global wrapper but NEVER acknowledges or removes possible effects.
    pub fn stream_owner_gone(&mut self, owner: NetworkStreamOwner) {
        self.accepted_owner_gone(owner);
        self.fd_publication_owner_gone(owner);
        self.gone_stream_owners.insert(owner);
        for wait in self
            .zero_stream_waits
            .values_mut()
            .filter(|wait| wait.owner == owner)
        {
            wait.abandoned = true;
        }
        for call in self
            .stream_calls
            .values_mut()
            .filter(|call| call.owner == owner)
        {
            call.abandoned = true;
        }
        for control in self
            .socket_controls
            .values_mut()
            .filter(|control| control.owner == owner)
        {
            control.abandoned = true;
        }
        for operation in self
            .stream_operations
            .values_mut()
            .filter(|operation| operation.owner == owner)
        {
            operation.abandoned = true;
        }
    }

    fn check_unreserved_stream_delivery(
        &self,
        open_file: OpenFileId,
    ) -> Result<(), NetworkReplayError> {
        if let Some(lease) = self.stream_delivery.get(&open_file) {
            return Err(
                if self
                    .stream_operations
                    .get(lease)
                    .is_some_and(|op| op.abandoned)
                {
                    NetworkReplayError::UnresolvedStreamOperation(*lease)
                } else {
                    NetworkReplayError::StreamOperationBusy(*lease)
                },
            );
        }
        Ok(())
    }

    fn check_stream_operations_finished(&self) -> Result<(), NetworkReplayError> {
        self.check_accepted_finished()?;
        if let Some(id) = self.zero_stream_waits.keys().next() {
            return Err(NetworkReplayError::UnresolvedZeroStreamWait(*id));
        }
        if let Some(call) = self.stream_calls.keys().next() {
            return Err(NetworkReplayError::UnresolvedStreamCall(*call));
        }
        if let Some(control) = self.socket_controls.values().next() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(control.lease));
        }
        if let Some(lease) = self.stream_operations.keys().next() {
            return Err(NetworkReplayError::UnresolvedStreamOperation(*lease));
        }
        Ok(())
    }
}

/// Fail-closed network capture/replay error.
#[derive(Debug)]
pub enum NetworkReplayError {
    /// A creation/provider/descriptor fact did not match the exact operation.
    InvalidAcceptedReceipt,
    /// No such durable accept receipt exists.
    UnknownAcceptLease(NetworkAcceptLeaseId),
    /// Physical allocation, copyout, or custody has not been reconciled.
    UnresolvedAccept(NetworkAcceptLeaseId),
    /// A created child has no authenticated terminal disposition.
    UnresolvedAcceptedChild(detcore_model::network_trace::ChildCreationIdV1),
    /// Invalid descriptor publication authority; always an internal failure.
    FdPublicationProtocol(String),
    /// Capability or post-ingress normalization authority has not been established.
    UnresolvedSocketNormalization(OpenFileId),
    /// The exact transient wait is absent or already acknowledged.
    UnknownZeroStreamWait(NetworkZeroStreamWaitId),
    /// No atomic empty-decision receipt exists for this active receive call.
    ZeroStreamWaitNotPrepared(NetworkStreamCallId),
    /// A prepared, entered or abandoned wait has not been explicitly resolved.
    UnresolvedZeroStreamWait(NetworkZeroStreamWaitId),
    /// A scheduler resource supplied a different call than the wait receipt.
    ZeroStreamWaitCallMismatch(NetworkZeroStreamWaitId),
    /// An operation belongs to a terminal task incarnation.
    StreamOwnerGone(NetworkStreamOwner),
    /// Zero-capacity recv requires its atomic availability/error operation.
    ZeroStreamReservation,
    /// A second actual socket disagreed with its declared fresh class profile.
    StreamProfileMismatch(StreamSocketKeyV3),
    /// SingleRecorderNamespaceV1 was not proved for this socket.
    StreamNamespaceMismatch,
    /// TCP shadow access preceded actual Socket/accept enrollment.
    UnregisteredStreamSocket(OpenFileId),
    /// Payload lacks the exact bounded copy-unit/class authority.
    InvalidShadowPublication(NetworkChannelId),
    /// Active-call identity is absent or has already been released.
    UnknownStreamCall(NetworkStreamCallId),
    /// Call was presented by a different authenticated task/MM.
    StreamCallOwnerMismatch(NetworkStreamCallId),
    /// A physical pin acquisition/release or abandoned call remains unresolved.
    UnresolvedStreamCall(NetworkStreamCallId),
    /// Pin acquisition, active use, and release were not in protocol order.
    StreamCallPhaseMismatch(NetworkStreamCallId),
    /// Receipt is absent or has already been consumed.
    UnknownStreamLease(NetworkStreamLeaseId),
    /// A different authenticated owner supplied this receipt.
    StreamLeaseOwnerMismatch(NetworkStreamLeaseId),
    /// Receipt belongs to a different operation kind.
    StreamLeaseKindMismatch(NetworkStreamLeaseId),
    /// An independently progressing operation currently owns this direction.
    StreamOperationBusy(NetworkStreamLeaseId),
    /// Possible effects were never acknowledged; success is forbidden.
    UnresolvedStreamOperation(NetworkStreamLeaseId),
    /// A reserved prefix changed in violation of exclusive delivery ownership.
    StreamDeliveryChanged(NetworkStreamLeaseId),
    /// One RPC view exceeds the bounded scratch contract.
    StreamChunkTooLarge(usize),
    /// Operation belongs to the other engine mode.
    WrongMode,
    /// This stable OFD was retired and cannot describe a new connection.
    OpenFileRetired(OpenFileId),
    /// A known or explicitly selected channel differs from requested metadata.
    ChannelEndpointMismatch(NetworkChannelId),
    /// No unused trace occurrence has the requested endpoint metadata.
    NoMatchingChannel,
    /// Only Replay accept may select an explicit recorded channel.
    InvalidChannelSelection,
    /// Replay metadata must come from the trace, not a local placeholder socket.
    ObservedLocalDuringReplay,
    /// Record cannot establish a requested bind without matching observation.
    UnverifiedLocalConstraint,
    /// Legacy post-syscall journaling and pre-delivery ingress were mixed.
    MixedIngressCapture(NetworkChannelId),
    /// Publication named a channel different from the supplied OFD's binding.
    IngressChannelMismatch {
        /// Channel owned by the supplied OFD.
        bound: NetworkChannelId,
        /// Channel named by the observation.
        supplied: NetworkChannelId,
    },
    /// A consumer-local availability, signal or copy result is not ingress.
    ConsumerLocalIngressError(i32),
    /// Connection/message publication is outside the stream-ingress API.
    UnsupportedIngressObservation(NetworkChannelId),
    /// Trace failed semantic validation.
    InvalidTrace(NetworkTraceValidationError),
    /// Versioned trace framing or payload failed validation.
    Codec(NetworkTraceCodecError),
    /// Host-side trace handle or atomic publication failed.
    Io(io::Error),
    /// Resolved run epoch disagrees with the authoritative trace epoch.
    EpochMismatch {
        /// Epoch resolved before container entry.
        expected: DateTime<Utc>,
        /// Epoch persisted in the trace.
        actual: DateTime<Utc>,
    },
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
    /// Trace object identity was never registered.
    UnknownAncillaryObject(NetworkObjectId),
    /// OFD has no trace object identity.
    UnknownAncillaryOpenFile(OpenFileId),
    /// Retired trace object identity cannot be reused.
    AncillaryObjectRetired(NetworkObjectId),
    /// Existing trace object cannot be remapped to another OFD or kind.
    AncillaryObjectRemap(NetworkObjectId),
    /// One OFD cannot have two trace object identities.
    AncillaryOpenFileAlreadyRegistered(OpenFileId),
    /// A retired OFD identity cannot be assigned to a new trace object.
    AncillaryOpenFileRetired(OpenFileId),
    /// An alias release had no matching retain.
    AncillaryAliasUnderflow(NetworkObjectId),
    /// Object retirement requires every installed alias to be closed.
    AncillaryAliasesRemain {
        /// Trace object identity.
        id: NetworkObjectId,
        /// Installed alias count.
        count: u64,
    },
    /// Edge-triggered or one-shot readiness requires adapter-owned interest state.
    UnsupportedReadinessMode,
    /// Epoll flags are not modeled and therefore cannot be replayed safely.
    UnsupportedEpollFlags(u32),
    /// An epoll OFD cannot watch itself.
    EpollSelfRegistration(OpenFileId),
    /// `EPOLL_CTL_ADD` found an existing target registration.
    EpollInterestAlreadyExists {
        /// Epoll open-file description.
        epoll: OpenFileId,
        /// Watched open-file description.
        target: OpenFileId,
    },
    /// `EPOLL_CTL_MOD`/`DEL` found no target registration.
    UnknownEpollInterest {
        /// Epoll open-file description.
        epoll: OpenFileId,
        /// Watched open-file description.
        target: OpenFileId,
    },
    /// Epoll OFD has no registered interest set.
    UnknownEpollInstance(OpenFileId),
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
    use std::ffi::CString;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStrExt;
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

    fn endpoint_binding(peer: NetworkAddressV2) -> NetworkChannelBinding {
        NetworkChannelBinding {
            transport: NetworkTransportV2::Tcp,
            role: NetworkEndpointRoleV2::OutboundClient,
            peer_address: Some(peer),
            requested_local_constraint: None,
            observed_local_address: None,
            accepted_from: None,
            selected_channel: None,
        }
    }

    fn endpoint(port: u16) -> NetworkAddressV2 {
        NetworkAddressV2::Inet4 {
            address: [192, 0, 2, 1],
            port,
        }
    }

    fn listener_binding() -> NetworkChannelBinding {
        let mut binding = endpoint_binding(endpoint(443));
        binding.role = NetworkEndpointRoleV2::Listener;
        binding.peer_address = None;
        binding
    }

    #[test]
    fn channel_ids_are_capture_ordinals_not_ofd_or_creator_identity() {
        let mut a = NetworkReplayEngine::record(epoch());
        let mut b = NetworkReplayEngine::record(epoch());
        let requests = [
            endpoint_binding(endpoint(443)),
            endpoint_binding(endpoint(8443)),
        ];
        for (index, request) in requests.into_iter().enumerate() {
            let a_id = a
                .ensure_channel(open_file(index as u64), request.clone())
                .unwrap();
            let other = OpenFileId::new_socket(DetTid::from_raw(91), 900 - index as u64);
            let b_id = b.ensure_channel(other, request).unwrap();
            assert_eq!(a_id, NetworkChannelId(index as u64 + 1));
            assert_eq!(a_id, b_id);
        }
        assert_eq!(
            a.into_recorded_trace().unwrap(),
            b.into_recorded_trace().unwrap()
        );
    }

    #[test]
    fn replay_matches_distinct_endpoints_after_socket_creation_order_changes() {
        let mut record = NetworkReplayEngine::record(epoch());
        let a = endpoint_binding(endpoint(443));
        let b = endpoint_binding(endpoint(8443));
        let a_id = record.ensure_channel(open_file(0), a.clone()).unwrap();
        let b_id = record.ensure_channel(open_file(1), b.clone()).unwrap();
        let mut replay =
            NetworkReplayEngine::replay(record.into_recorded_trace().unwrap()).unwrap();
        assert_eq!(
            replay.ensure_channel(open_file(0), b.clone()).unwrap(),
            b_id
        );
        assert_eq!(
            replay.ensure_channel(open_file(1), a.clone()).unwrap(),
            a_id
        );
        // An alias repeats the same OFD and checks, rather than choosing another
        // occurrence; an already bound wrong-peer request is never ignored.
        assert_eq!(replay.ensure_channel(open_file(0), b).unwrap(), b_id);
        let before = format!("{replay:?}");
        assert!(
            matches!(replay.ensure_channel(open_file(0), a), Err(NetworkReplayError::ChannelEndpointMismatch(id)) if id == b_id)
        );
        assert_eq!(format!("{replay:?}"), before);
    }

    #[test]
    fn identical_peers_select_stored_occurrence_not_numeric_id_order() {
        let mut first = channel();
        first.id = NetworkChannelId(99);
        let mut second = first.clone();
        second.id = NetworkChannelId(4);
        let trace = NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![first.clone(), second],
            inputs: vec![],
            outputs: vec![],
        };
        let request = endpoint_binding(first.peer_address.unwrap());
        let mut replay = NetworkReplayEngine::replay(trace).unwrap();
        assert_eq!(
            replay
                .ensure_channel(open_file(7), request.clone())
                .unwrap(),
            NetworkChannelId(99)
        );
        replay.retire_open_file(open_file(7));
        assert_eq!(
            replay
                .ensure_channel(open_file(2), request.clone())
                .unwrap(),
            NetworkChannelId(4)
        );
        let before = format!("{replay:?}");
        assert!(matches!(
            replay.ensure_channel(open_file(3), request.clone()),
            Err(NetworkReplayError::NoMatchingChannel)
        ));
        assert_eq!(format!("{replay:?}"), before);
        assert!(matches!(
            replay.ensure_channel(open_file(7), request),
            Err(NetworkReplayError::OpenFileRetired(_))
        ));
        assert_eq!(format!("{replay:?}"), before);
    }

    #[test]
    fn bound_metadata_checks_ipv6_extras_and_unix_binary_peer_bytes() {
        let original = NetworkAddressV2::Inet6 {
            address: [1; 16],
            port: 443,
            flowinfo: 7,
            scope_id: 9,
        };
        let mut record = NetworkReplayEngine::record(epoch());
        let id = record
            .ensure_channel(open_file(0), endpoint_binding(original))
            .unwrap();
        let alternatives = [
            NetworkAddressV2::Inet6 {
                address: [2; 16],
                port: 443,
                flowinfo: 7,
                scope_id: 9,
            },
            NetworkAddressV2::Inet6 {
                address: [1; 16],
                port: 444,
                flowinfo: 7,
                scope_id: 9,
            },
            NetworkAddressV2::Inet6 {
                address: [1; 16],
                port: 443,
                flowinfo: 8,
                scope_id: 9,
            },
            NetworkAddressV2::Inet6 {
                address: [1; 16],
                port: 443,
                flowinfo: 7,
                scope_id: 10,
            },
        ];
        for peer in alternatives {
            let before = format!("{record:?}");
            assert!(
                matches!(record.ensure_channel(open_file(0), endpoint_binding(peer)), Err(NetworkReplayError::ChannelEndpointMismatch(actual)) if actual == id)
            );
            assert_eq!(format!("{record:?}"), before);
        }
        let mut request = endpoint_binding(NetworkAddressV2::UnixAbstract(vec![0, 1, 0, 255]));
        request.transport = NetworkTransportV2::UnixStream;
        record
            .ensure_channel(open_file(1), request.clone())
            .unwrap();
        request.peer_address = Some(NetworkAddressV2::UnixAbstract(vec![0, 1, 0, 254]));
        let before = format!("{record:?}");
        assert!(matches!(
            record.ensure_channel(open_file(1), request),
            Err(NetworkReplayError::ChannelEndpointMismatch(_))
        ));
        assert_eq!(format!("{record:?}"), before);
    }

    #[test]
    fn replay_accept_requires_selected_channel_and_exact_listener_ancestry() {
        let mut record = NetworkReplayEngine::record(epoch());
        let listener = record
            .ensure_channel(open_file(0), listener_binding())
            .unwrap();
        let other_listener = record
            .ensure_channel(open_file(1), listener_binding())
            .unwrap();
        let mut accepted = endpoint_binding(endpoint(12345));
        accepted.role = NetworkEndpointRoleV2::Accepted;
        accepted.accepted_from = Some(listener);
        let accepted_id = record
            .ensure_channel(open_file(2), accepted.clone())
            .unwrap();
        let mut replay =
            NetworkReplayEngine::replay(record.into_recorded_trace().unwrap()).unwrap();
        let before = format!("{replay:?}");
        assert!(matches!(
            replay.ensure_channel(open_file(8), accepted.clone()),
            Err(NetworkReplayError::InvalidChannelSelection)
        ));
        assert_eq!(format!("{replay:?}"), before);
        accepted.selected_channel = Some(accepted_id);
        let mut wrong = accepted.clone();
        wrong.accepted_from = Some(other_listener);
        assert!(matches!(
            replay.ensure_channel(open_file(8), wrong),
            Err(NetworkReplayError::ChannelEndpointMismatch(_))
        ));
        assert_eq!(format!("{replay:?}"), before);
        assert_eq!(
            replay
                .ensure_channel(open_file(8), accepted.clone())
                .unwrap(),
            accepted_id
        );
        let after = format!("{replay:?}");
        assert!(matches!(
            replay.ensure_channel(open_file(9), accepted),
            Err(NetworkReplayError::ChannelAlreadyBound(_))
        ));
        assert_eq!(format!("{replay:?}"), after);
    }

    #[test]
    fn observed_ephemeral_local_address_is_not_a_requested_bind() {
        let mut record = NetworkReplayEngine::record(epoch());
        let mut captured = endpoint_binding(endpoint(443));
        captured.observed_local_address = Some(NetworkAddressV2::Inet4 {
            address: [10, 0, 0, 2],
            port: 40123,
        });
        let id = record
            .ensure_channel(open_file(0), captured.clone())
            .unwrap();
        let trace = record.into_recorded_trace().unwrap();
        let mut replay = NetworkReplayEngine::replay(trace.clone()).unwrap();
        assert_eq!(
            replay
                .ensure_channel(open_file(99), endpoint_binding(endpoint(443)))
                .unwrap(),
            id
        );
        let mut replay = NetworkReplayEngine::replay(trace).unwrap();
        let before = format!("{replay:?}");
        assert!(matches!(
            replay.ensure_channel(open_file(99), captured.clone()),
            Err(NetworkReplayError::ObservedLocalDuringReplay)
        ));
        assert_eq!(format!("{replay:?}"), before);
        let mut constrained = endpoint_binding(endpoint(443));
        constrained.requested_local_constraint = Some(endpoint(40124));
        assert!(matches!(
            replay.ensure_channel(open_file(99), constrained.clone()),
            Err(NetworkReplayError::NoMatchingChannel)
        ));
        assert_eq!(format!("{replay:?}"), before);
        constrained.requested_local_constraint = captured.observed_local_address;
        assert_eq!(
            replay.ensure_channel(open_file(99), constrained).unwrap(),
            id
        );
    }

    #[test]
    fn channel_allocation_refusals_do_not_consume_ids_or_rebind_retired_ofds() {
        let mut record = NetworkReplayEngine::record(epoch());
        let mut invalid = listener_binding();
        invalid.peer_address = Some(endpoint(443));
        let before = format!("{record:?}");
        assert!(matches!(
            record.ensure_channel(open_file(0), invalid),
            Err(NetworkReplayError::InvalidTrace(
                NetworkTraceValidationError::InvalidChannelRelationship
            ))
        ));
        assert_eq!(format!("{record:?}"), before);
        let mut unverified = endpoint_binding(endpoint(443));
        unverified.requested_local_constraint = Some(endpoint(9999));
        assert!(matches!(
            record.ensure_channel(open_file(0), unverified),
            Err(NetworkReplayError::UnverifiedLocalConstraint)
        ));
        assert_eq!(format!("{record:?}"), before);
        assert_eq!(
            record
                .ensure_channel(open_file(0), endpoint_binding(endpoint(443)))
                .unwrap(),
            NetworkChannelId(1)
        );
        record.retire_open_file(open_file(0));
        let before = format!("{record:?}");
        assert!(matches!(
            record.ensure_channel(open_file(0), endpoint_binding(endpoint(8443))),
            Err(NetworkReplayError::OpenFileRetired(_))
        ));
        assert_eq!(format!("{record:?}"), before);
        assert_eq!(
            record
                .ensure_channel(open_file(1), endpoint_binding(endpoint(8443)))
                .unwrap(),
            NetworkChannelId(2)
        );
        assert!(matches!(
            record.bind(open_file(0), NetworkChannelId(2)),
            Err(NetworkReplayError::OpenFileRetired(_))
        ));
        record.next_record_channel = u64::MAX;
        let before = format!("{record:?}");
        assert!(matches!(
            record.ensure_channel(open_file(2), endpoint_binding(endpoint(9443))),
            Err(NetworkReplayError::Overflow)
        ));
        assert_eq!(format!("{record:?}"), before);
    }

    #[test]
    fn final_retirement_fences_never_bound_ofd_publication() {
        let mut record = NetworkReplayEngine::record(epoch());
        let retired = open_file(8);
        assert_eq!(record.retire_open_file(retired), None);
        let before = format!("{record:?}");
        assert!(matches!(
            record.ensure_channel(retired, endpoint_binding(endpoint(443))),
            Err(NetworkReplayError::OpenFileRetired(id)) if id == retired
        ));
        assert_eq!(format!("{record:?}"), before);
        assert_eq!(
            record
                .ensure_channel(open_file(9), endpoint_binding(endpoint(443)))
                .unwrap(),
            NetworkChannelId(1)
        );
        let mut replay =
            NetworkReplayEngine::replay(record.into_recorded_trace().unwrap()).unwrap();
        assert_eq!(replay.retire_open_file(retired), None);
        let before = format!("{replay:?}");
        assert!(matches!(
            replay.ensure_channel(retired, endpoint_binding(endpoint(443))),
            Err(NetworkReplayError::OpenFileRetired(id)) if id == retired
        ));
        assert_eq!(format!("{replay:?}"), before);
    }

    #[test]
    fn binding_rechecks_transport_role_and_ancestry_before_idempotent_return() {
        let mut record = NetworkReplayEngine::record(epoch());
        let request = endpoint_binding(endpoint(443));
        record
            .ensure_channel(open_file(0), request.clone())
            .unwrap();
        let mut wrong_transport = request.clone();
        wrong_transport.transport = NetworkTransportV2::UnixStream;
        let mut wrong_role = request.clone();
        wrong_role.role = NetworkEndpointRoleV2::Listener;
        let mut wrong_ancestry = request;
        wrong_ancestry.accepted_from = Some(NetworkChannelId(91));
        for wrong in [wrong_transport, wrong_role, wrong_ancestry] {
            let before = format!("{record:?}");
            assert!(matches!(
                record.ensure_channel(open_file(0), wrong),
                Err(NetworkReplayError::ChannelEndpointMismatch(_))
            ));
            assert_eq!(format!("{record:?}"), before);
        }
    }

    #[test]
    fn endpoint_lookup_does_not_relax_v2_duplicate_connect_validation() {
        let mut record = NetworkReplayEngine::record(epoch());
        let request = endpoint_binding(endpoint(443));
        let id = record
            .ensure_channel(open_file(0), request.clone())
            .unwrap();
        assert_eq!(record.ensure_channel(open_file(0), request).unwrap(), id);
        for at in [1, 2] {
            record
                .record_input(NetworkInputEventV2 {
                    ordinal: 0,
                    channel: id,
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(at),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
                })
                .unwrap();
        }
        assert!(matches!(
            record.into_recorded_trace(),
            Err(NetworkReplayError::InvalidTrace(
                NetworkTraceValidationError::DuplicateConnect
            ))
        ));
    }

    fn ingress(at: u64, event: NetworkInputKindV2) -> NetworkInputEventV2 {
        NetworkInputEventV2 {
            ordinal: 999, // Publication, not a transport caller, owns this index.
            channel: channel_id(),
            release: NetworkReleaseV2 {
                not_before_global_time: time(at),
                after_transmitted_offset: 0,
            },
            event,
        }
    }

    fn ingress_bytes(offset: u64, at: u64, bytes: &[u8]) -> NetworkInputEventV2 {
        ingress(
            at,
            NetworkInputKindV2::StreamBytes {
                stream_offset: offset,
                bytes: bytes.to_vec(),
            },
        )
    }

    fn shared_recorder() -> (NetworkReplayEngine, OpenFileId) {
        let mut engine = NetworkReplayEngine::record(epoch());
        engine.record_channel(channel()).unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        (engine, ofd)
    }

    #[test]
    fn published_ingress_record_and_replay_use_the_same_receive_queue() {
        let (mut record, ofd) = shared_recorder();
        record
            .publish_ingress(ofd, ingress_bytes(0, 1, b"firstnext"))
            .unwrap();
        record
            .publish_ingress(
                ofd,
                ingress(
                    2,
                    NetworkInputKindV2::SocketError {
                        stream_offset: 9,
                        errno: libc::ECONNRESET,
                    },
                ),
            )
            .unwrap();
        record
            .publish_ingress(
                ofd,
                ingress(
                    4,
                    NetworkInputKindV2::PeerShutdown {
                        stream_offset: 9,
                        direction: NetworkShutdownV2::Write,
                    },
                ),
            )
            .unwrap();
        let expected = [
            StreamReceiveOutcome::Bytes(b"fi".to_vec()),
            StreamReceiveOutcome::Bytes(b"rstnext".to_vec()),
            StreamReceiveOutcome::Error(libc::ECONNRESET),
            StreamReceiveOutcome::EndOfFile,
        ];
        let maxima = [2, 32, 32, 32];
        for (maximum, outcome) in maxima.into_iter().zip(&expected) {
            assert_eq!(&record.receive_stream(ofd, maximum, true).unwrap(), outcome);
        }
        let trace = record.into_recorded_trace().unwrap();
        assert_eq!(
            trace
                .inputs
                .iter()
                .map(|input| input.ordinal)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let mut replay = NetworkReplayEngine::replay(trace).unwrap();
        replay.bind(ofd, channel_id()).unwrap();
        replay.release_eligible(time(4)).unwrap();
        for (maximum, outcome) in maxima.into_iter().zip(&expected) {
            assert_eq!(&replay.receive_stream(ofd, maximum, true).unwrap(), outcome);
        }
        replay.finish().unwrap();
    }

    #[test]
    fn ingress_frontier_is_independent_of_consumption_and_aliases() {
        let (mut engine, ofd) = shared_recorder();
        // dup/fork aliases carry this same stable OFD, not another channel.
        let alias = ofd;
        engine.bind(alias, channel_id()).unwrap();
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"abc"))
            .unwrap();
        assert_eq!(
            engine.receive_stream(alias, 1, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"a".to_vec())
        );
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.publish_ingress(alias, ingress_bytes(1, 2, b"d")),
            Err(NetworkReplayError::InvalidTrace(
                NetworkTraceValidationError::NonContiguousInput
            ))
        ));
        assert_eq!(format!("{engine:?}"), before);
        engine
            .publish_ingress(alias, ingress_bytes(3, 2, b"de"))
            .unwrap();
        assert_eq!(
            engine.receive_stream(ofd, 10, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"bcde".to_vec())
        );
        assert!(matches!(
            engine.bind(open_file(1), channel_id()),
            Err(NetworkReplayError::ChannelAlreadyBound(_))
        ));
        engine.into_recorded_trace().unwrap();
    }

    #[test]
    fn ingress_append_preserves_an_owned_nonconsuming_view() {
        let (mut engine, ofd) = shared_recorder();
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"before"))
            .unwrap();
        // This is the existing owned PEEK snapshot, not a copy/commit lease.
        // It exercises append-vs-view lifetime without claiming fault semantics.
        let view = engine
            .receive_stream_with_options(
                ofd,
                NetworkReceiveOptions {
                    maximum: 32,
                    nonblocking: true,
                    flags: libc::MSG_PEEK,
                    receive_low_water: 1,
                },
            )
            .unwrap();
        engine
            .publish_ingress(ofd, ingress_bytes(6, 2, b"after"))
            .unwrap();
        assert_eq!(view, StreamReceiveOutcome::Bytes(b"before".to_vec()));
        assert_eq!(
            engine.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"beforeafter".to_vec())
        );
        assert_eq!(
            engine.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::WouldBlock
        );
    }

    #[test]
    fn invalid_ingress_does_not_change_journal_queue_or_frontier() {
        let (mut engine, ofd) = shared_recorder();
        let mut before_epoch = ingress_bytes(0, 1, b"x");
        before_epoch.release.not_before_global_time = LogicalTime::from_nanos(0);
        let mut unreachable = ingress_bytes(0, 1, b"x");
        unreachable.release.after_transmitted_offset = 1;
        let mut wrong_channel = ingress_bytes(0, 1, b"x");
        wrong_channel.channel = NetworkChannelId(2);
        let invalid = [
            ingress_bytes(1, 1, b"x"),
            ingress_bytes(0, 1, b""),
            before_epoch,
            unreachable,
            wrong_channel,
            ingress(
                1,
                NetworkInputKindV2::SocketError {
                    stream_offset: 0,
                    errno: 0,
                },
            ),
            ingress(
                1,
                NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
            ),
        ];
        for input in invalid {
            let before = format!("{engine:?}");
            assert!(engine.publish_ingress(ofd, input).is_err());
            assert_eq!(format!("{engine:?}"), before);
        }
        for errno in [libc::EAGAIN, libc::EINTR, libc::EFAULT] {
            let before = format!("{engine:?}");
            assert!(matches!(
                engine.publish_ingress(ofd, ingress(1, NetworkInputKindV2::SocketError { stream_offset: 0, errno })),
                Err(NetworkReplayError::ConsumerLocalIngressError(actual)) if actual == errno
            ));
            assert_eq!(format!("{engine:?}"), before);
        }
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"valid"))
            .unwrap();
        assert_eq!(
            engine.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"valid".to_vec())
        );
        assert_eq!(engine.into_recorded_trace().unwrap().inputs.len(), 1);
    }

    #[test]
    fn ingress_release_gates_are_validated_before_commit() {
        let (mut engine, ofd) = shared_recorder();
        engine
            .record_output(NetworkOutputEventV2 {
                channel: channel_id(),
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"sent".to_vec(),
                },
            })
            .unwrap();
        let mut first = ingress_bytes(0, 2, b"a");
        first.release.after_transmitted_offset = 2;
        engine.publish_ingress(ofd, first).unwrap();
        for (at, watermark, expected) in [
            (1, 2, NetworkTraceValidationError::NonMonotonicRelease),
            (2, 1, NetworkTraceValidationError::NonMonotonicRelease),
            (
                3,
                5,
                NetworkTraceValidationError::UnreachableTransmitWatermark,
            ),
        ] {
            let mut input = ingress_bytes(1, at, b"b");
            input.release.after_transmitted_offset = watermark;
            let before = format!("{engine:?}");
            assert!(
                matches!(engine.publish_ingress(ofd, input), Err(NetworkReplayError::InvalidTrace(error)) if error == expected)
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        let mut next = ingress_bytes(1, 3, b"b");
        next.release.after_transmitted_offset = 4;
        engine.publish_ingress(ofd, next).unwrap();
        let trace = engine.into_recorded_trace().unwrap();
        let mut replay = NetworkReplayEngine::replay(trace).unwrap();
        replay.bind(ofd, channel_id()).unwrap();
        assert!(replay.release_eligible(time(100)).unwrap().is_empty());
        replay.transmit_stream(ofd, b"se").unwrap();
        replay.release_eligible(time(2)).unwrap();
        assert_eq!(
            replay.receive_stream(ofd, 10, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"a".to_vec())
        );
        replay.transmit_stream(ofd, b"nt").unwrap();
        replay.release_eligible(time(3)).unwrap();
        assert_eq!(
            replay.receive_stream(ofd, 10, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"b".to_vec())
        );
        replay.finish().unwrap();
    }

    #[test]
    fn ingress_cannot_mix_with_legacy_journal_only_capture() {
        let (mut legacy, ofd) = shared_recorder();
        legacy.record_input(ingress_bytes(0, 1, b"legacy")).unwrap();
        assert_eq!(
            legacy.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::WouldBlock
        );
        let before = format!("{legacy:?}");
        assert!(matches!(
            legacy.publish_ingress(ofd, ingress_bytes(6, 2, b"new")),
            Err(NetworkReplayError::MixedIngressCapture(_))
        ));
        assert_eq!(format!("{legacy:?}"), before);
        let (mut shared, ofd) = shared_recorder();
        shared
            .publish_ingress(ofd, ingress_bytes(0, 1, b"new"))
            .unwrap();
        let before = format!("{shared:?}");
        assert!(matches!(
            shared.record_input(ingress_bytes(3, 2, b"legacy")),
            Err(NetworkReplayError::MixedIngressCapture(_))
        ));
        assert_eq!(format!("{shared:?}"), before);
        assert_eq!(
            shared.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"new".to_vec())
        );
    }

    #[test]
    fn terminal_ingress_refuses_later_arrivals_without_state_change() {
        let (mut engine, ofd) = shared_recorder();
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"data"))
            .unwrap();
        engine
            .publish_ingress(
                ofd,
                ingress(
                    2,
                    NetworkInputKindV2::PeerShutdown {
                        stream_offset: 4,
                        direction: NetworkShutdownV2::Write,
                    },
                ),
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.publish_ingress(ofd, ingress_bytes(4, 3, b"late")),
            Err(NetworkReplayError::InvalidTrace(
                NetworkTraceValidationError::EventAfterTerminal
            ))
        ));
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"data".to_vec())
        );
        assert_eq!(
            engine.receive_stream(ofd, 32, true).unwrap(),
            StreamReceiveOutcome::EndOfFile
        );
        engine.into_recorded_trace().unwrap();
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
    fn record_epoch_is_explicit_and_replay_requires_exact_match() {
        let explicit = epoch();
        assert_eq!(
            NetworkReplayEngine::record(explicit).trace_epoch(),
            explicit
        );
        let wrong = explicit + chrono::Duration::nanoseconds(1);
        assert!(matches!(
            NetworkReplayEngine::replay_versioned_with_expected_epoch(
                NetworkTrace::V2(trace()),
                wrong,
            ),
            Err(NetworkReplayError::EpochMismatch { expected, actual })
                if expected == wrong && actual == explicit
        ));
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
    fn ancillary_registry_preserves_alias_identity_and_tombstones_retirement() {
        let mut engine = NetworkReplayEngine::replay(trace()).unwrap();
        let object = NetworkObjectId(41);
        let underlying = OpenFileId::new(DetTid::from_raw(7), 3);
        engine
            .register_ancillary_object(
                object,
                underlying,
                NetworkAncillaryObjectKind::FileDescriptor,
            )
            .unwrap();
        // Dup and fork install distinct descriptor aliases for the same OFD.
        assert_eq!(engine.retain_ancillary_alias(object).unwrap(), underlying);
        assert_eq!(engine.retain_ancillary_alias(object).unwrap(), underlying);
        assert_eq!(engine.ancillary_object(object).unwrap().alias_count, 2);
        assert_eq!(
            engine
                .ancillary_object_for_open_file(underlying)
                .unwrap()
                .id,
            object
        );
        assert!(matches!(
            engine.retire_ancillary_object(object),
            Err(NetworkReplayError::AncillaryAliasesRemain { count: 2, .. })
        ));
        assert_eq!(engine.release_ancillary_alias(object).unwrap(), 1);
        assert_eq!(engine.release_ancillary_alias(object).unwrap(), 0);
        assert_eq!(engine.retire_ancillary_object(object).unwrap(), underlying);
        assert!(matches!(
            engine.ancillary_object(object),
            Err(NetworkReplayError::AncillaryObjectRetired(id)) if id == object
        ));
        assert!(matches!(
            engine.register_ancillary_object(
                object,
                OpenFileId::new(DetTid::from_raw(7), 4),
                NetworkAncillaryObjectKind::FileDescriptor,
            ),
            Err(NetworkReplayError::AncillaryObjectRetired(id)) if id == object
        ));
        assert!(matches!(
            engine.register_ancillary_object(
                NetworkObjectId(42),
                underlying,
                NetworkAncillaryObjectKind::FileDescriptor,
            ),
            Err(NetworkReplayError::AncillaryOpenFileRetired(ofd)) if ofd == underlying
        ));
        engine
            .register_ancillary_object(
                NetworkObjectId(42),
                OpenFileId::new(DetTid::from_raw(7), 4),
                NetworkAncillaryObjectKind::FileDescriptor,
            )
            .unwrap();
    }

    fn readiness_trace(events: Vec<NetworkReadinessV2>) -> NetworkTraceV2 {
        NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![channel()],
            outputs: vec![],
            inputs: events
                .into_iter()
                .enumerate()
                .map(|(index, readiness)| NetworkInputEventV2 {
                    ordinal: index as u64,
                    channel: channel_id(),
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(index as u64 + 1),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::Readiness(readiness),
                })
                .collect(),
        }
    }

    #[test]
    fn epoll_edge_does_not_repeat_until_a_new_transition() {
        let readable = NetworkReadinessV2 {
            readable: true,
            ..NetworkReadinessV2::default()
        };
        let mut engine = NetworkReplayEngine::replay(readiness_trace(vec![
            readable,
            NetworkReadinessV2::default(),
            readable,
        ]))
        .unwrap();
        let target = open_file(0);
        let epoll = OpenFileId::new(DetTid::from_raw(1), 90);
        engine.bind(target, channel_id()).unwrap();
        engine
            .epoll_add(epoll, target, (libc::EPOLLIN | libc::EPOLLET) as u32)
            .unwrap();
        engine.release_eligible(time(1)).unwrap();
        assert_eq!(engine.epoll_ready(epoll).unwrap().len(), 1);
        assert!(engine.epoll_ready(epoll).unwrap().is_empty());
        engine.release_eligible(time(2)).unwrap();
        assert!(engine.epoll_ready(epoll).unwrap().is_empty());
        engine.release_eligible(time(3)).unwrap();
        assert_eq!(engine.epoll_ready(epoll).unwrap().len(), 1);
    }

    #[test]
    fn epoll_oneshot_mod_rearms_and_err_hup_are_unconditional() {
        let readiness = NetworkReadinessV2 {
            readable: true,
            error: true,
            hangup: true,
            ..NetworkReadinessV2::default()
        };
        let mut engine = NetworkReplayEngine::replay(readiness_trace(vec![readiness])).unwrap();
        let target = open_file(0);
        let epoll = OpenFileId::new(DetTid::from_raw(1), 91);
        engine.bind(target, channel_id()).unwrap();
        let events = libc::EPOLLONESHOT as u32;
        engine.epoll_add(epoll, target, events).unwrap();
        engine.release_eligible(time(1)).unwrap();
        let first = engine.epoll_ready(epoll).unwrap();
        assert_eq!(first.len(), 1);
        assert_ne!(first[0].events & libc::EPOLLERR as u32, 0);
        assert_ne!(first[0].events & libc::EPOLLHUP as u32, 0);
        assert!(engine.epoll_ready(epoll).unwrap().is_empty());
        engine.epoll_modify(epoll, target, events).unwrap();
        assert_eq!(engine.epoll_ready(epoll).unwrap().len(), 1);
    }

    #[test]
    fn epoll_orders_by_ofd_retires_interests_and_refuses_exclusive() {
        let second_channel = NetworkChannelId(2);
        let mut second = channel();
        second.id = second_channel;
        let ready = NetworkReadinessV2 {
            readable: true,
            ..NetworkReadinessV2::default()
        };
        let mut two = NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![channel(), second],
            outputs: vec![],
            inputs: vec![],
        };
        for (ordinal, channel) in [second_channel, channel_id()].into_iter().enumerate() {
            two.inputs.push(NetworkInputEventV2 {
                ordinal: ordinal as u64,
                channel,
                release: NetworkReleaseV2 {
                    not_before_global_time: time(1),
                    after_transmitted_offset: 0,
                },
                event: NetworkInputKindV2::Readiness(ready),
            });
        }
        let mut engine = NetworkReplayEngine::replay(two).unwrap();
        let first = open_file(1);
        let second = open_file(2);
        let epoll = OpenFileId::new(DetTid::from_raw(1), 92);
        engine.bind(first, channel_id()).unwrap();
        engine.bind(second, second_channel).unwrap();
        engine
            .epoll_add(epoll, second, libc::EPOLLIN as u32)
            .unwrap();
        engine
            .epoll_add(epoll, first, libc::EPOLLIN as u32)
            .unwrap();
        assert!(matches!(
            engine.epoll_add(
                OpenFileId::new(DetTid::from_raw(1), 93),
                first,
                libc::EPOLLEXCLUSIVE as u32,
            ),
            Err(NetworkReplayError::UnsupportedEpollFlags(flags))
                if flags == libc::EPOLLEXCLUSIVE as u32
        ));
        engine.release_eligible(time(1)).unwrap();
        let ready = engine.epoll_ready(epoll).unwrap();
        assert_eq!(
            ready.iter().map(|event| event.target).collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(engine.retire_open_file(first), Some(channel_id()));
        assert_eq!(
            engine.epoll_ready(epoll).unwrap(),
            vec![NetworkEpollEvent {
                target: second,
                events: libc::EPOLLIN as u32,
            }]
        );
        engine.retire_open_file(epoll);
        assert!(matches!(
            engine.epoll_ready(epoll),
            Err(NetworkReplayError::UnknownEpollInstance(id)) if id == epoll
        ));
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

    #[test]
    fn bounded_trace_open_refuses_links_fifos_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.trace");
        std::fs::write(&target, b"trace").unwrap();
        let link = directory.path().join("link.trace");
        symlink(&target, &link).unwrap();
        assert!(open_bounded_network_trace(&link).is_err());

        let fifo = directory.path().join("trace.fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is a valid NUL-terminated pathname in a private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let fifo_error = open_bounded_network_trace(&fifo).unwrap_err();
        assert_eq!(fifo_error.kind(), io::ErrorKind::InvalidData);
        assert!(fifo_error.to_string().contains("regular file"));

        let oversized = directory.path().join("oversized.trace");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_NETWORK_TRACE_FILE_BYTES + 1)
            .unwrap();
        let size_error = open_bounded_network_trace(&oversized).unwrap_err();
        assert_eq!(size_error.kind(), io::ErrorKind::InvalidData);
        assert!(size_error.to_string().contains("too large"));
    }

    fn stream_owner(tid: i32) -> NetworkStreamOwner {
        let thread = DetTid::from_raw(tid);
        NetworkStreamOwner {
            thread,
            mm: detcore_model::futex::MmId::initial(thread),
        }
    }

    fn reserved_chunk(
        chunk: NetworkStreamChunk,
    ) -> (NetworkStreamLeaseId, NetworkStreamChunkOutcome) {
        match chunk {
            NetworkStreamChunk::Reserved { lease, outcome, .. } => (lease, outcome),
            NetworkStreamChunk::Empty | NetworkStreamChunk::LocalReadClosed => {
                panic!("expected an actual reserved observation")
            }
        }
    }

    #[test]
    fn submitted_stream_effect_cannot_finalize_or_be_resolved_by_another_owner() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let receipt = engine.begin_stream_ingress(owner, ofd, time(1)).unwrap();
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.complete_stream_ingress(
                stream_owner(2),
                receipt,
                time(2),
                NetworkIngressObservation::NoArrival
            ),
            Err(NetworkReplayError::StreamLeaseOwnerMismatch(_))
        ));
        assert_eq!(format!("{engine:?}"), before);
        engine.retire_open_file(ofd);
        engine.stream_owner_gone(owner);
        assert!(matches!(
            engine.into_recorded_trace(),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
    }

    #[test]
    fn only_actual_known_nonarrival_completes_without_an_input_event() {
        for observation in [
            NetworkIngressObservation::NoArrival,
            NetworkIngressObservation::Interrupted,
        ] {
            let (mut engine, ofd) = shared_recorder();
            let owner = stream_owner(1);
            let receipt = engine.begin_stream_ingress(owner, ofd, time(1)).unwrap();
            engine
                .complete_stream_ingress(owner, receipt, time(2), observation)
                .unwrap();
            let next = engine.begin_stream_ingress(owner, ofd, time(3)).unwrap();
            assert_ne!(next, receipt);
            assert!(matches!(
                engine.complete_stream_ingress(
                    owner,
                    receipt,
                    time(3),
                    NetworkIngressObservation::NoArrival
                ),
                Err(NetworkReplayError::UnknownStreamLease(_))
            ));
            engine
                .complete_stream_ingress(owner, next, time(4), NetworkIngressObservation::NoArrival)
                .unwrap();
            assert!(engine.into_recorded_trace().unwrap().inputs.is_empty());
        }
    }

    #[test]
    fn invalid_physical_completion_preserves_possible_effect_receipt_and_queue() {
        for observation in [
            NetworkIngressObservation::Bytes(Vec::new()),
            NetworkIngressObservation::TransportError(libc::EFAULT),
            NetworkIngressObservation::TransportError(libc::EINTR),
            NetworkIngressObservation::TransportError(libc::EAGAIN),
        ] {
            let (mut engine, ofd) = shared_recorder();
            let owner = stream_owner(1);
            let receipt = engine.begin_stream_ingress(owner, ofd, time(1)).unwrap();
            let before = format!("{engine:?}");
            assert!(
                engine
                    .complete_stream_ingress(owner, receipt, time(2), observation)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
            assert!(matches!(
                engine.into_recorded_trace(),
                Err(NetworkReplayError::UnresolvedStreamOperation(_))
            ));
        }
    }

    #[test]
    fn completion_uses_one_engine_frontier_after_delivered_connect() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        engine
            .record_input(ingress(
                1,
                NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
            ))
            .unwrap();
        for (at, bytes) in [(2, b"abc".as_slice()), (4, b"def".as_slice())] {
            let receipt = engine.begin_stream_ingress(owner, ofd, time(at)).unwrap();
            engine
                .complete_stream_ingress(
                    owner,
                    receipt,
                    time(at + 1),
                    NetworkIngressObservation::Bytes(bytes.to_vec()),
                )
                .unwrap();
        }
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 6);
        let mut received = Vec::new();
        for _ in 0..2 {
            let (receipt, outcome) =
                reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 512, 0).unwrap());
            let NetworkStreamChunkOutcome::Bytes(bytes) = outcome else {
                panic!("expected payload")
            };
            received.extend(bytes);
            engine
                .finish_stream_chunk(owner, receipt, NetworkStreamChunkDisposition::Consumed)
                .unwrap();
        }
        assert_eq!(received, b"abcdef");
        let trace = engine.into_recorded_trace().unwrap();
        assert_eq!(trace.inputs.len(), 3);
        assert_eq!(trace.inputs[1].release.not_before_global_time, time(3));
        assert_eq!(trace.inputs[2].release.not_before_global_time, time(5));
        assert!(
            matches!(&trace.inputs[2].event, NetworkInputKindV2::StreamBytes { stream_offset: 3, bytes } if bytes == b"def")
        );
        let mut replay = NetworkReplayEngine::replay(trace).unwrap();
        replay.bind(ofd, channel_id()).unwrap();
        replay.release_eligible(time(5)).unwrap();
        assert!(matches!(
            replay.take_connection_outcome(ofd).unwrap(),
            Some(ConnectionOutcome::Connect(
                NetworkConnectionResultV2::Connected
            ))
        ));
        let mut replayed = Vec::new();
        for _ in 0..2 {
            let (receipt, outcome) = reserved_chunk(
                replay
                    .reserve_stream_chunk(stream_owner(2), ofd, 512, 0)
                    .unwrap(),
            );
            let NetworkStreamChunkOutcome::Bytes(bytes) = outcome else {
                panic!("expected payload")
            };
            replayed.extend(bytes);
            replay
                .finish_stream_chunk(
                    stream_owner(2),
                    receipt,
                    NetworkStreamChunkDisposition::Consumed,
                )
                .unwrap();
        }
        assert_eq!(replayed, received);
        replay.finish().unwrap();
    }

    #[test]
    fn reserved_prefix_survives_appended_ingress_and_rejects_legacy_consumption() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"abc"))
            .unwrap();
        let (delivery, outcome) =
            reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 2, 0).unwrap());
        assert_eq!(outcome, NetworkStreamChunkOutcome::Bytes(b"ab".to_vec()));
        let ingress = engine
            .begin_stream_ingress(stream_owner(2), ofd, time(2))
            .unwrap();
        engine
            .complete_stream_ingress(
                stream_owner(2),
                ingress,
                time(3),
                NetworkIngressObservation::Bytes(b"def".to_vec()),
            )
            .unwrap();
        assert!(!engine.stream_ready_at_least(ofd, 1).unwrap());
        assert!(matches!(
            engine.receive_stream(ofd, 1, true),
            Err(NetworkReplayError::StreamOperationBusy(_))
        ));
        engine
            .finish_stream_chunk(owner, delivery, NetworkStreamChunkDisposition::Consumed)
            .unwrap();
        assert_eq!(
            engine.receive_stream(ofd, 8, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"cdef".to_vec())
        );
    }

    #[test]
    fn failed_copy_and_peek_do_not_consume_the_reserved_fragment() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"abcdefgh"))
            .unwrap();
        for disposition in [
            NetworkStreamChunkDisposition::CopyFailed,
            NetworkStreamChunkDisposition::Peeked,
        ] {
            let (receipt, outcome) =
                reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 8, 0).unwrap());
            assert_eq!(
                outcome,
                NetworkStreamChunkOutcome::Bytes(b"abcdefgh".to_vec())
            );
            engine
                .finish_stream_chunk(owner, receipt, disposition)
                .unwrap();
            assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 8);
        }
        assert_eq!(
            engine.receive_stream(ofd, 8, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"abcdefgh".to_vec())
        );
    }

    #[test]
    fn peek_past_payload_acknowledges_eof_without_consuming_payload_or_terminal() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"abc"))
            .unwrap();
        engine
            .publish_ingress(
                ofd,
                ingress(
                    2,
                    NetworkInputKindV2::PeerShutdown {
                        stream_offset: 3,
                        direction: NetworkShutdownV2::Write,
                    },
                ),
            )
            .unwrap();
        let (receipt, outcome) =
            reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 512, 3).unwrap());
        assert_eq!(outcome, NetworkStreamChunkOutcome::EndOfFile);
        engine
            .finish_stream_chunk(owner, receipt, NetworkStreamChunkDisposition::Peeked)
            .unwrap();
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 3);
        assert_eq!(
            engine.receive_stream(ofd, 3, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"abc".to_vec())
        );
        assert_eq!(
            engine.receive_stream(ofd, 3, true).unwrap(),
            StreamReceiveOutcome::EndOfFile
        );
    }

    #[test]
    fn abandoned_delivery_fails_threshold_and_finish_instead_of_sleeping_forever() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"abc"))
            .unwrap();
        let (receipt, _) = reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 3, 0).unwrap());
        engine.stream_owner_gone(owner);
        assert!(matches!(
            engine.stream_ready_at_least(ofd, 1),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
        assert!(
            engine
                .finish_stream_chunk(owner, receipt, NetworkStreamChunkDisposition::Consumed)
                .is_err()
        );
        assert!(matches!(
            engine.into_recorded_trace(),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
    }

    #[test]
    fn stale_and_same_mm_owner_cleanup_do_not_cancel_other_exact_receipts() {
        let (mut engine, ofd) = shared_recorder();
        let old = stream_owner(1);
        let new = NetworkStreamOwner {
            thread: old.thread,
            mm: old.mm.for_exec(old.thread),
        };
        let receipt = engine.begin_stream_ingress(new, ofd, time(1)).unwrap();
        engine.stream_owner_gone(old);
        engine.stream_owner_gone(NetworkStreamOwner {
            thread: DetTid::from_raw(2),
            mm: new.mm,
        });
        engine
            .complete_stream_ingress(new, receipt, time(2), NetworkIngressObservation::NoArrival)
            .unwrap();
        engine.into_recorded_trace().unwrap();
    }

    #[test]
    fn stream_threshold_observes_terminal_after_partial_data_and_bounds_reservations() {
        let (mut engine, ofd) = shared_recorder();
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, b"a"))
            .unwrap();
        assert!(!engine.stream_ready_at_least(ofd, 3).unwrap());
        let (receipt, outcome) = reserved_chunk(
            engine
                .reserve_stream_chunk(stream_owner(1), ofd, usize::MAX, 0)
                .unwrap(),
        );
        assert_eq!(outcome, NetworkStreamChunkOutcome::Bytes(b"a".to_vec()));
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.read_stream_chunk_view(stream_owner(1), receipt, 0, usize::MAX),
            Err(NetworkReplayError::StreamChunkTooLarge(_))
        ));
        assert_eq!(format!("{engine:?}"), before);
        engine
            .finish_stream_chunk(
                stream_owner(1),
                receipt,
                NetworkStreamChunkDisposition::Peeked,
            )
            .unwrap();
        engine
            .publish_ingress(
                ofd,
                ingress(
                    2,
                    NetworkInputKindV2::SocketError {
                        stream_offset: 1,
                        errno: libc::ECONNRESET,
                    },
                ),
            )
            .unwrap();
        assert!(engine.stream_ready_at_least(ofd, 3).unwrap());
        assert_eq!(
            engine.stream_queue_status(ofd).unwrap().error,
            Some(libc::ECONNRESET)
        );
    }

    #[test]
    fn later_view_copy_failure_keeps_whole_selected_publication_unit() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let payload: Vec<u8> = (0..1500).map(|index| (index % 251) as u8).collect();
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, &payload))
            .unwrap();
        let before = format!("{:?}", engine.channels.get(&channel_id()).unwrap().inbound);
        let NetworkStreamChunk::Reserved {
            lease,
            selection_len,
            outcome,
        } = engine
            .reserve_stream_chunk(owner, ofd, payload.len(), 0)
            .unwrap()
        else {
            panic!("expected selection")
        };
        assert_eq!(selection_len, payload.len());
        assert_eq!(
            outcome,
            NetworkStreamChunkOutcome::Bytes(payload[..512].to_vec())
        );
        let second = engine
            .read_stream_chunk_view(owner, lease, 512, 512)
            .unwrap();
        assert_eq!(second, payload[512..1024]);
        // The actual adapter's failed second-view user copy has already had
        // possible guest-memory effects. Its acknowledgement must not retain
        // the old bug of committing the first RPC view as a separate unit.
        engine
            .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
            .unwrap();
        assert_eq!(
            format!("{:?}", engine.channels.get(&channel_id()).unwrap().inbound),
            before
        );
        assert_eq!(
            engine.channels.get(&channel_id()).unwrap().inbound_consumed,
            0
        );
        assert_eq!(
            engine.receive_stream(ofd, 1500, true).unwrap(),
            StreamReceiveOutcome::Bytes(payload)
        );
    }

    #[test]
    fn invalid_or_stale_view_requests_never_change_or_release_a_selection() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let payload = vec![42; 1025];
        engine
            .publish_ingress(ofd, ingress_bytes(0, 1, &payload))
            .unwrap();
        let (lease, _) = reserved_chunk(engine.reserve_stream_chunk(owner, ofd, 1025, 0).unwrap());
        let before = format!("{engine:?}");
        assert!(
            engine
                .read_stream_chunk_view(stream_owner(2), lease, 0, 1)
                .is_err()
        );
        assert!(
            engine
                .read_stream_chunk_view(owner, lease, 1026, 1)
                .is_err()
        );
        assert!(engine.read_stream_chunk_view(owner, lease, 0, 513).is_err());
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine
                .read_stream_chunk_view(owner, lease, 1024, 512)
                .unwrap(),
            vec![42]
        );
        engine
            .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::Peeked)
            .unwrap();
        let after = format!("{engine:?}");
        assert!(matches!(
            engine.read_stream_chunk_view(owner, lease, 0, 1),
            Err(NetworkReplayError::UnknownStreamLease(_))
        ));
        assert_eq!(format!("{engine:?}"), after);
    }

    fn active_pinned_call(
        engine: &mut NetworkReplayEngine,
        ofd: OpenFileId,
        owner: NetworkStreamOwner,
    ) -> NetworkStreamCallId {
        let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, lease).unwrap();
        if call.physical_pin_required {
            engine
                .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
                .unwrap();
        }
        engine
            .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        call.id
    }

    fn prepared_zero_wait(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        offset: usize,
    ) -> NetworkZeroStreamWaitId {
        match engine
            .prepare_zero_stream_receive(owner, call, offset)
            .unwrap()
        {
            NetworkZeroStreamReceive::Waiting(id) => id,
            other => panic!("expected atomic empty receipt, got {other:?}"),
        }
    }

    #[derive(Clone, Default)]
    struct ZeroEntryCapture(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
    struct ZeroEntryVisitor(Option<String>);
    impl tracing::field::Visit for ZeroEntryVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }
    impl tracing::Subscriber for ZeroEntryCapture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::INFO
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = ZeroEntryVisitor(None);
            event.record(&mut visitor);
            if let Some(message) = visitor.0
                && message.contains("[network-zero-wait]")
            {
                assert_eq!(*event.metadata().level(), tracing::Level::INFO);
                self.0.lock().unwrap().push(message);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    #[test]
    fn zero_wait_entry_log_once_after_authenticated_record_transition() {
        let (mut e, _, owner, call) = shadow_probe_fixture();
        let wait = prepared_zero_wait(&mut e, owner, call, 0);
        let op = crate::resources::ExternalOpId::new(owner.thread, 91);
        let capture = ZeroEntryCapture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            assert!(e.enter_zero_stream_wait(owner, wait).is_err());
            assert!(!e.zero_stream_wait_entered(owner, wait).unwrap());
            assert!(capture.0.lock().unwrap().is_empty());
            e.begin_zero_stream_wait(owner, call, Some(op)).unwrap();
            e.enter_record_zero_stream_wait(
                owner,
                crate::resources::ExternalOpId::new(owner.thread, 92),
            )
            .unwrap();
            assert!(!e.zero_stream_wait_entered(owner, wait).unwrap());
            assert!(capture.0.lock().unwrap().is_empty());
            e.enter_record_zero_stream_wait(owner, op).unwrap();
            assert!(e.zero_stream_wait_entered(owner, wait).unwrap());
            assert_eq!(capture.0.lock().unwrap().len(), 1);
            e.enter_record_zero_stream_wait(owner, op).unwrap();
            e.enter_zero_stream_wait(owner, wait).unwrap();
            assert_eq!(capture.0.lock().unwrap().len(), 1);
            assert!(e.finish_zero_stream_wait(owner, wait).unwrap());
            assert!(e.enter_zero_stream_wait(owner, wait).is_err());
            assert_eq!(capture.0.lock().unwrap().len(), 1);
        });
        let lines = capture.0.lock().unwrap();
        let expected = format!(
            "DETLOG [network-zero-wait] entered owner={owner:?} call={call:?} wait={wait:?} record_operation={:?}{}",
            Some(op),
            crate::detlog::record_suffix(crate::detlog::DetLogEvent::Other)
        );
        assert_eq!(lines.as_slice(), [expected]);
    }

    #[test]
    fn zero_wait_entry_log_rejects_wrong_owner_stale_mm_and_abandoned_receipt() {
        let (mut e, _, owner, call) = shadow_probe_fixture();
        let wait = prepared_zero_wait(&mut e, owner, call, 0);
        e.begin_zero_stream_wait(
            owner,
            call,
            Some(crate::resources::ExternalOpId::new(owner.thread, 1)),
        )
        .unwrap();
        let capture = ZeroEntryCapture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            assert!(e.enter_zero_stream_wait(stream_owner(2), wait).is_err());
            let stale = NetworkStreamOwner {
                thread: owner.thread,
                mm: detcore_model::futex::MmId::initial(DetTid::from_raw(999)),
            };
            assert!(e.enter_zero_stream_wait(stale, wait).is_err());
            assert!(
                e.enter_zero_stream_wait(owner, NetworkZeroStreamWaitId(wait.0 + 1))
                    .is_err()
            );
            assert!(!e.zero_stream_wait_entered(owner, wait).unwrap());
            e.stream_owner_gone(owner);
            assert!(e.enter_zero_stream_wait(owner, wait).is_err());
            assert!(!e.zero_stream_waits[&wait].entered);
            assert!(capture.0.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn zero_wait_entry_log_replay_receipt_is_kept_and_compared_by_strict_schema() {
        let (mut record, ofd, owner, call) = shadow_probe_fixture();
        record.begin_stream_call_release(owner, call).unwrap();
        record.finish_stream_call_release(owner, call).unwrap();
        let trace = record.into_recorded_versioned_trace().unwrap();
        let mut bytes = Vec::new();
        trace.write_framed(&mut bytes).unwrap();
        let mut e = NetworkReplayEngine::replay_from_reader(std::io::Cursor::new(bytes)).unwrap();
        e.register_stream_socket(
            ofd,
            test_fresh_profile(libc::AF_INET).key,
            test_socket_namespace(),
            None,
        )
        .unwrap();
        e.ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        let call = active_pinned_call(&mut e, ofd, owner);
        let wait = prepared_zero_wait(&mut e, owner, call, 0);
        e.begin_zero_stream_wait(owner, call, None).unwrap();
        let capture = ZeroEntryCapture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            e.enter_zero_stream_wait(owner, wait).unwrap();
            e.enter_zero_stream_wait(owner, wait).unwrap();
        });
        let lines = capture.0.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("record_operation=None"));
        let (_, record) = crate::detlog::DetLogRecord::split(&lines[0]).unwrap();
        assert_eq!(
            record,
            Some(crate::detlog::DetLogRecord::new(
                crate::detlog::DetLogEvent::Other
            ))
        );
        let left = format!(
            "2026-09-22T00:00:00.000000Z INFO detcore::network_replay: {}\n",
            lines[0]
        );
        let changed = left.replace("[network-zero-wait] entered", "[network-zero-wait] missing");
        assert_ne!(left, changed);
        let compare = |right: &str| {
            crate::logdiff::try_compare_bitwise_info_v1_bytes_with_records(
                left.as_bytes(),
                right.as_bytes(),
                crate::logdiff::ComparisonSideLabels::new("left", "right"),
            )
            .unwrap()
        };
        let (equal, l, r) = compare(&left);
        assert!(equal.matched_with_evidence());
        assert_eq!((l, r), (1, 1));
        let (different, l, r) = compare(&changed);
        assert!(different.diff_found);
        assert_eq!((l, r), (1, 1));
        let (missing, l, r) = compare("");
        assert!(!missing.matched_with_evidence());
        assert_eq!((l, r), (1, 0));
        let mut rendered = Vec::new();
        assert_eq!(
            crate::logdiff::write_bitwise_info_v1_bytes(left.as_bytes(), "left", &mut rendered)
                .unwrap(),
            1
        );
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("[network-zero-wait] entered")
        );
    }

    #[test]
    fn zero_wait_must_resolve_before_call_release_and_trace_finalization() {
        for entered in [false, true] {
            let (mut engine, ofd) = shared_recorder();
            let owner = stream_owner(1);
            let call = active_pinned_call(&mut engine, ofd, owner);
            assert!(matches!(
                engine.begin_zero_stream_wait(owner, call, Some(crate::resources::ExternalOpId::new(owner.thread, 1))),
                Err(NetworkReplayError::ZeroStreamWaitNotPrepared(actual)) if actual == call
            ));
            let id = prepared_zero_wait(&mut engine, owner, call, 0);
            assert_eq!(prepared_zero_wait(&mut engine, owner, call, 0), id);
            if entered {
                engine
                    .begin_zero_stream_wait(
                        owner,
                        call,
                        Some(crate::resources::ExternalOpId::new(owner.thread, 1)),
                    )
                    .unwrap();
                engine.enter_zero_stream_wait(owner, id).unwrap();
            }
            let before = format!("{engine:?}");
            assert!(
                matches!(engine.begin_stream_call_release(owner, call), Err(NetworkReplayError::UnresolvedZeroStreamWait(actual)) if actual == id)
            );
            assert!(
                matches!(engine.finish(), Err(NetworkReplayError::UnresolvedZeroStreamWait(actual)) if actual == id)
            );
            assert_eq!(format!("{engine:?}"), before);
            if entered {
                assert!(
                    matches!(engine.cancel_zero_stream_wait(owner, id), Err(NetworkReplayError::UnresolvedZeroStreamWait(actual)) if actual == id)
                );
                assert_eq!(format!("{engine:?}"), before);
                assert!(engine.finish_zero_stream_wait(owner, id).unwrap());
            } else {
                engine.cancel_zero_stream_wait(owner, id).unwrap();
            }
            assert!(
                matches!(engine.finish_zero_stream_wait(owner, id), Err(NetworkReplayError::UnknownZeroStreamWait(actual)) if actual == id)
            );
            engine.begin_stream_call_release(owner, call).unwrap();
            engine.finish_stream_call_release(owner, call).unwrap();
            engine.into_recorded_trace().unwrap();
        }
    }

    #[test]
    fn zero_wait_owner_exit_and_stale_mm_cannot_acknowledge_receipt() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        let id = prepared_zero_wait(&mut engine, owner, call, 8);
        let stale = NetworkStreamOwner {
            thread: owner.thread,
            mm: owner.mm.for_exec(owner.thread),
        };
        let before = format!("{engine:?}");
        assert!(engine.finish_zero_stream_wait(stale, id).is_err());
        assert!(engine.cancel_zero_stream_wait(stale, id).is_err());
        assert!(engine.zero_stream_wait_ready(stale, id).is_err());
        assert_eq!(format!("{engine:?}"), before);
        engine.stream_owner_gone(stale);
        assert!(!engine.zero_stream_wait_entered(owner, id).unwrap());
        engine.stream_owner_gone(owner);
        assert!(engine.finish_zero_stream_wait(owner, id).is_err());
        assert!(engine.cancel_zero_stream_wait(owner, id).is_err());
        assert!(engine.begin_stream_call_release(owner, call).is_err());
        assert!(
            matches!(engine.into_recorded_trace(), Err(NetworkReplayError::UnresolvedZeroStreamWait(actual)) if actual == id)
        );
    }

    #[test]
    fn zero_wait_generation_ignores_readiness_and_dequeue_but_keeps_small_arrival() {
        let mut inputs = vec![
            ingress_bytes(0, 1, b"abc"),
            ingress(
                2,
                NetworkInputKindV2::Readiness(NetworkReadinessV2 {
                    readable: true,
                    ..Default::default()
                }),
            ),
            ingress_bytes(3, 3, b"d"),
        ];
        for (ordinal, input) in inputs.iter_mut().enumerate() {
            input.ordinal = ordinal as u64;
        }
        let mut engine = NetworkReplayEngine::replay(NetworkTraceV2 {
            epoch: epoch(),
            channels: vec![channel()],
            outputs: vec![],
            inputs,
        })
        .unwrap();
        let ofd = open_file(0);
        engine.bind(ofd, channel_id()).unwrap();
        engine.release_eligible(time(1)).unwrap();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        let id = prepared_zero_wait(&mut engine, owner, call, 8);
        engine.begin_zero_stream_wait(owner, call, None).unwrap();
        engine.enter_zero_stream_wait(owner, id).unwrap();
        engine.release_eligible(time(2)).unwrap();
        assert!(!engine.zero_stream_wait_ready(owner, id).unwrap());
        assert_eq!(
            engine.receive_stream(ofd, 3, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"abc".to_vec())
        );
        assert!(!engine.zero_stream_wait_ready(owner, id).unwrap());
        assert_eq!(prepared_zero_wait(&mut engine, owner, call, 8), id);
        engine.release_eligible(time(3)).unwrap();
        assert!(engine.zero_stream_wait_ready(owner, id).unwrap());
        assert_eq!(
            engine.prepare_zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::Ready
        );
        assert_eq!(
            engine
                .stream_call_queue_status(owner, call)
                .unwrap()
                .queued_bytes,
            1
        );
        assert!(engine.finish_zero_stream_wait(owner, id).unwrap());
        assert_eq!(
            engine.receive_stream(ofd, 8, true).unwrap(),
            StreamReceiveOutcome::Bytes(b"d".to_vec())
        );
    }

    #[test]
    fn zero_wait_unentered_arrival_below_offset_still_waits_in_record_and_replay() {
        for replay in [false, true] {
            let (mut recorder, ofd) = shared_recorder();
            recorder
                .publish_ingress(ofd, ingress_bytes(0, 1, b"abc"))
                .unwrap();
            let mut engine = if replay {
                recorder
                    .publish_ingress(ofd, ingress_bytes(3, 2, b"d"))
                    .unwrap();
                let mut engine =
                    NetworkReplayEngine::replay(recorder.into_recorded_trace().unwrap()).unwrap();
                engine.bind(ofd, channel_id()).unwrap();
                engine.release_eligible(time(1)).unwrap();
                engine
            } else {
                recorder
            };
            let owner = stream_owner(1);
            let call = active_pinned_call(&mut engine, ofd, owner);
            let id = prepared_zero_wait(&mut engine, owner, call, 8);
            if replay {
                engine.release_eligible(time(2)).unwrap();
            } else {
                engine
                    .publish_ingress(ofd, ingress_bytes(3, 2, b"d"))
                    .unwrap();
            }
            let operation =
                (!replay).then_some(crate::resources::ExternalOpId::new(owner.thread, 1));
            assert_eq!(
                engine
                    .begin_zero_stream_wait(owner, call, operation)
                    .unwrap(),
                id
            );
            assert!(engine.zero_stream_wait_ready(owner, id).unwrap());
            assert!(!engine.zero_stream_wait_entered(owner, id).unwrap());
            assert_eq!(
                engine.prepare_zero_stream_receive(owner, call, 8).unwrap(),
                NetworkZeroStreamReceive::Waiting(id)
            );
            assert!(!engine.zero_stream_wait_ready(owner, id).unwrap());
            assert!(!engine.zero_stream_wait_entered(owner, id).unwrap());
            engine.cancel_zero_stream_wait(owner, id).unwrap();
            assert_eq!(
                engine.receive_stream(ofd, 8, true).unwrap(),
                StreamReceiveOutcome::Bytes(b"abcd".to_vec())
            );
        }
    }

    #[test]
    fn zero_wait_unentered_error_uses_normal_error_consumption_priority() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        let id = prepared_zero_wait(&mut engine, owner, call, 8);
        engine
            .publish_ingress(
                ofd,
                ingress(
                    1,
                    NetworkInputKindV2::SocketError {
                        stream_offset: 0,
                        errno: libc::ECONNRESET,
                    },
                ),
            )
            .unwrap();
        assert_eq!(
            engine.prepare_zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::Error(libc::ECONNRESET)
        );
        assert_eq!(
            engine.stream_call_queue_status(owner, call).unwrap().error,
            None
        );
        engine.cancel_zero_stream_wait(owner, id).unwrap();
    }

    #[test]
    fn zero_wait_v3_shadow_arrival_retains_payload_cursor_and_same_timer_receipt() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        let control = engine.begin_stream_call_control(owner, call).unwrap();
        engine
            .submit_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalEffect::SetSocketOption {
                    option: NetworkStreamSocketOption::PeekOffset(8),
                },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
            )
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let operation = crate::resources::ExternalOpId::new(owner.thread, 1);
        let id = prepared_zero_wait(&mut engine, owner, call, 8);
        for _ in 0..3 {
            assert_eq!(
                engine
                    .begin_zero_stream_wait(owner, call, Some(operation))
                    .unwrap(),
                id
            );
            engine
                .enter_record_zero_stream_wait(owner, operation)
                .unwrap();
            assert!(engine.zero_stream_wait_entered(owner, id).unwrap());
            assert!(!engine.zero_stream_wait_ready(owner, id).unwrap());
            assert_eq!(prepared_zero_wait(&mut engine, owner, call, 8), id);
        }
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        // The probe temporarily normalizes the physical cursor and restores 8.
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(8),
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(owner, probe.lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::Peeked { count: 1 },
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: 8 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(owner, probe.lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::PollState,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PollState {
                    revents: libc::POLLIN,
                },
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::QueuedBytes,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::QueuedBytes { count: 1 },
            )
            .unwrap();
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), b"d".to_vec(), false)
            .unwrap();
        assert_eq!(
            engine.prepare_zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::Ready
        );
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .options
                .peek_offset,
            Some(8)
        );
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 1);
        assert!(engine.finish_zero_stream_wait(owner, id).unwrap());
        engine.begin_stream_call_release(owner, call).unwrap();
        engine.finish_stream_call_release(owner, call).unwrap();
        let captured = engine.into_recorded_versioned_trace().unwrap();
        let NetworkTrace::V3(captured) = captured else {
            panic!("expected V3")
        };
        assert!(
            matches!(&captured.history.inputs[0].event, NetworkInputKindV2::StreamBytes { bytes, .. } if bytes == b"d")
        );
    }

    #[test]
    fn socket_control_set_is_atomic_canonical_and_does_not_reserve_on_contention() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let other = stream_owner(2);
        let second = open_file(2);
        let held = engine.begin_socket_controls(owner, vec![second]).unwrap()[0].1;
        let next = engine.next_stream_lease;
        assert!(
            matches!(engine.begin_socket_controls(other, vec![second,ofd,ofd]), Err(NetworkReplayError::StreamOperationBusy(id)) if id == held)
        );
        assert_eq!(engine.next_stream_lease, next);
        assert!(!engine.socket_controls.contains_key(&ofd));
        engine
            .finish_socket_control(owner, held, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let controls = engine
            .begin_socket_controls(other, vec![second, ofd, ofd])
            .unwrap();
        assert_eq!(controls.len(), 2);
        assert!(controls[0].0 < controls[1].0);
        for (_, lease) in controls {
            engine
                .finish_socket_control(other, lease, NetworkSocketControlFinish::Unchanged)
                .unwrap();
        }
    }

    #[test]
    fn final_alias_close_preserves_active_call_then_retires_after_known_pin_release() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        engine
            .submit_descriptor_effect(owner, lease, NetworkDescriptorEffect::CloseDescriptor)
            .unwrap();
        engine
            .confirm_descriptor_effect(owner, lease, Err(libc::EINTR))
            .unwrap();
        engine
            .finish_socket_control(
                owner,
                lease,
                NetworkSocketControlFinish::Closed { last_alias: true },
            )
            .unwrap();
        assert!(engine.retired_open_files.contains(&ofd));
        assert_eq!(engine.channel_for(ofd), Some(channel_id()));
        assert_eq!(engine.stream_call_open_file(owner, call).unwrap(), ofd);
        assert!(
            matches!(engine.begin_socket_controls(owner,vec![ofd]),Err(NetworkReplayError::OpenFileRetired(id)) if id==ofd)
        );
        let existing = engine.begin_stream_call_control(owner, call).unwrap();
        assert!(
            matches!(engine.begin_stream_call(owner,existing),Err(NetworkReplayError::OpenFileRetired(id)) if id==ofd)
        );
        engine
            .finish_socket_control(owner, existing, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        engine.begin_stream_call_release(owner, call).unwrap();
        assert!(
            matches!(engine.stream_call_open_file(owner,call),Err(NetworkReplayError::StreamCallPhaseMismatch(id)) if id==call)
        );
        engine.finish_stream_call_release(owner, call).unwrap();
        assert_eq!(engine.channel_for(ofd), None);
        assert!(engine.retired_channels.contains(&channel_id()));
        assert!(
            matches!(engine.finish_stream_call_release(owner,call),Err(NetworkReplayError::UnknownStreamCall(id)) if id==call)
        );
    }

    #[test]
    fn unknown_pin_acquisition_release_and_owner_death_never_finalize_successfully() {
        for phase in 0..4 {
            let (mut engine, ofd) = shared_recorder();
            let owner = stream_owner(1);
            let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
            let call = engine.begin_stream_call(owner, lease).unwrap();
            if phase != 0 {
                engine
                    .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
                    .unwrap();
            }
            engine
                .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
                .unwrap();
            if phase == 2 {
                engine.begin_stream_call_release(owner, call.id).unwrap();
            }
            if phase == 3 {
                engine.stream_owner_gone(owner);
            }
            assert!(
                matches!(engine.into_recorded_trace(),Err(NetworkReplayError::UnresolvedStreamCall(id)) if id==call.id)
            );
        }
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, lease).unwrap();
        assert!(matches!(
            engine.confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Failed(0)),
            Err(NetworkReplayError::InvalidTrace(
                NetworkTraceValidationError::InvalidErrno
            ))
        ));
        assert_eq!(
            engine.stream_calls[&call.id].phase,
            StreamCallPhase::PinAcquireSubmitted
        );
        engine
            .confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Failed(libc::EPERM))
            .unwrap();
        engine
            .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        engine.into_recorded_trace().unwrap();
    }

    #[test]
    fn call_release_refuses_live_short_effect_and_stale_mm_cannot_acknowledge_pin() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        let lease = engine.begin_stream_call_control(owner, call).unwrap();
        engine
            .submit_descriptor_effect(owner, lease, NetworkDescriptorEffect::CloseDescriptor)
            .unwrap();
        assert!(
            matches!(engine.begin_stream_call_release(owner,call),Err(NetworkReplayError::StreamOperationBusy(id)) if id==lease)
        );
        let stale = NetworkStreamOwner {
            thread: owner.thread,
            mm: detcore_model::futex::MmId::initial(DetTid::from_raw(9)),
        };
        assert!(
            matches!(engine.stream_call_open_file(stale,call),Err(NetworkReplayError::StreamCallOwnerMismatch(id)) if id==call)
        );
        engine.stream_owner_gone(stale);
        assert!(!engine.stream_calls[&call].abandoned);
        engine.stream_owner_gone(owner);
        assert!(engine.stream_calls[&call].abandoned);
        assert!(engine.socket_controls[&ofd].abandoned);
        assert!(matches!(
            engine.confirm_descriptor_effect(owner, lease, Ok(())),
            Err(NetworkReplayError::StreamOwnerGone(_))
        ));
    }

    #[test]
    fn descriptor_close_and_failed_replacement_have_distinct_release_results() {
        for (effect, result, released) in [
            (NetworkDescriptorEffect::CloseDescriptor, Ok(()), true),
            (
                NetworkDescriptorEffect::CloseDescriptor,
                Err(libc::EBADF),
                false,
            ),
            (
                NetworkDescriptorEffect::CloseDescriptor,
                Err(reverie::Errno::ERESTARTSYS.into_raw()),
                false,
            ),
            (
                NetworkDescriptorEffect::CloseDescriptor,
                Err(libc::EINTR),
                true,
            ),
            (
                NetworkDescriptorEffect::CloseDescriptor,
                Err(libc::EIO),
                true,
            ),
            (
                NetworkDescriptorEffect::ReplaceDescriptor,
                Err(libc::EINTR),
                false,
            ),
            (
                NetworkDescriptorEffect::ReplaceDescriptor,
                Err(libc::EMFILE),
                false,
            ),
            (NetworkDescriptorEffect::ReplaceDescriptor, Ok(()), true),
        ] {
            let (mut engine, ofd) = shared_recorder();
            let owner = stream_owner(1);
            let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
            engine
                .submit_descriptor_effect(owner, lease, effect)
                .unwrap();
            assert!(
                matches!(engine.finish_socket_control(owner,lease,NetworkSocketControlFinish::Unchanged),Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id==lease)
            );
            engine
                .confirm_descriptor_effect(owner, lease, result)
                .unwrap();
            assert_eq!(
                engine.socket_controls[&ofd].physical.descriptor_released,
                Some(released)
            );
            let finish = if released {
                NetworkSocketControlFinish::Closed { last_alias: true }
            } else {
                NetworkSocketControlFinish::Unchanged
            };
            engine.finish_socket_control(owner, lease, finish).unwrap();
            assert_eq!(engine.channel_for(ofd).is_none(), released);
        }
    }

    #[test]
    fn zero_recv_atomically_sees_append_and_takes_error_beyond_peeked_payload() {
        let (mut engine, ofd) = shared_recorder();
        let owner = stream_owner(1);
        let call = active_pinned_call(&mut engine, ofd, owner);
        assert_eq!(
            engine.zero_stream_receive(owner, call, 0).unwrap(),
            NetworkZeroStreamReceive::Empty
        );
        let ingress = engine.begin_stream_ingress(owner, ofd, time(1)).unwrap();
        engine
            .complete_stream_ingress(
                owner,
                ingress,
                time(1),
                NetworkIngressObservation::Bytes(b"abc".to_vec()),
            )
            .unwrap();
        assert_eq!(
            engine.zero_stream_receive(owner, call, 0).unwrap(),
            NetworkZeroStreamReceive::Ready
        );
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 3);
        assert!(engine.stream_delivery.is_empty());
        let ingress = engine.begin_stream_ingress(owner, ofd, time(2)).unwrap();
        engine
            .complete_stream_ingress(
                owner,
                ingress,
                time(2),
                NetworkIngressObservation::TransportError(libc::ECONNRESET),
            )
            .unwrap();
        assert_eq!(
            engine.zero_stream_receive(owner, call, 3).unwrap(),
            NetworkZeroStreamReceive::Error(libc::ECONNRESET)
        );
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 3);
        assert_eq!(engine.stream_queue_status(ofd).unwrap().error, None);
        assert_eq!(engine.channels[&channel_id()].inbound_consumed, 0);
        assert!(engine.stream_delivery.is_empty());
    }

    fn test_fresh_profile(domain: i32) -> FreshStreamSocketProfileV3 {
        use detcore_model::network_trace::LinuxReceiveHzV3;
        use detcore_model::network_trace::ReceiveBufferStateV3;
        use detcore_model::network_trace::ReceiveTimeoutV3;
        FreshStreamSocketProfileV3 {
            key: StreamSocketKeyV3 {
                transport: NetworkTransportV2::Tcp,
                domain,
                socket_type: libc::SOCK_STREAM,
                protocol: libc::IPPROTO_TCP,
            },
            normalization: LinuxReceiveNormalizationV3 {
                hz: LinuxReceiveHzV3::Hz1000,
                peek_offset_set_supported: true,
                system_rmem_max: 20_971_520,
                namespace_tcp_rmem_max: 6_291_456,
                minimum_receive_buffer: 2304,
            },
            initial: StreamSocketOptionsV3 {
                peek_offset: Some(-1),
                receive_low_water: 1,
                receive_timeout: ReceiveTimeoutV3::Infinite,
                receive_buffer: ReceiveBufferStateV3 {
                    bytes: 262_144,
                    user_locked: false,
                    tcp_scaling_ratio: 128,
                },
            },
        }
    }

    fn test_socket_namespace() -> NetworkStreamNamespace {
        NetworkStreamNamespace {
            device: 4,
            inode: 100,
        }
    }

    #[test]
    fn shadow_profile_refusals_preserve_all_state_and_replay_never_uses_placeholder() {
        let mut record = NetworkReplayEngine::record_shadow(epoch());
        let profile = test_fresh_profile(libc::AF_INET);
        let ofd = open_file(20);
        record
            .register_stream_socket(
                ofd,
                profile.key,
                test_socket_namespace(),
                Some(profile.clone()),
            )
            .unwrap();
        let before = format!("{record:?}");
        let mut changed = profile.clone();
        changed.initial.receive_buffer.bytes *= 2;
        assert!(matches!(
            record.register_stream_socket(
                open_file(21),
                profile.key,
                test_socket_namespace(),
                Some(changed)
            ),
            Err(NetworkReplayError::StreamProfileMismatch(_))
        ));
        assert_eq!(format!("{record:?}"), before);
        assert!(matches!(
            record.register_stream_socket(
                open_file(21),
                profile.key,
                NetworkStreamNamespace {
                    device: 4,
                    inode: 101
                },
                Some(profile.clone())
            ),
            Err(NetworkReplayError::StreamNamespaceMismatch)
        ));
        assert_eq!(format!("{record:?}"), before);
        let trace = record.into_recorded_versioned_trace().unwrap();
        let mut replay = NetworkReplayEngine::replay_versioned(trace).unwrap();
        let replay_ns = NetworkStreamNamespace {
            device: 99,
            inode: 777,
        };
        let state = replay
            .register_stream_socket(open_file(70), profile.key, replay_ns, None)
            .unwrap();
        assert_eq!(state.options, profile.initial);
        let before = format!("{replay:?}");
        assert!(matches!(
            replay.register_stream_socket(
                open_file(71),
                profile.key,
                replay_ns,
                Some(profile.clone())
            ),
            Err(NetworkReplayError::WrongMode)
        ));
        assert_eq!(format!("{replay:?}"), before);
        let mut unknown = profile.key;
        unknown.domain = libc::AF_INET6;
        assert!(matches!(
            replay.register_stream_socket(open_file(71), unknown, replay_ns, None),
            Err(NetworkReplayError::StreamProfileMismatch(_))
        ));
        assert_eq!(format!("{replay:?}"), before);
    }

    #[test]
    fn shadow_class_binding_and_copy_units_round_trip_without_ofd_identity() {
        let mut record = NetworkReplayEngine::record_shadow(epoch());
        let profile = test_fresh_profile(libc::AF_INET);
        let ofd = open_file(33);
        record
            .register_stream_socket(
                ofd,
                profile.key,
                test_socket_namespace(),
                Some(profile.clone()),
            )
            .unwrap();
        let channel = record
            .ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        record
            .publish_ingress(
                ofd,
                NetworkInputEventV2 {
                    ordinal: 99,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(1),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::StreamBytes {
                        stream_offset: 0,
                        bytes: b"abc".to_vec(),
                    },
                },
            )
            .unwrap();
        let trace = record.into_recorded_versioned_trace().unwrap();
        let NetworkTrace::V3(v3) = &trace else {
            panic!("shadow writer must keep V3 metadata")
        };
        assert_eq!(
            v3.channel_socket_classes,
            vec![ChannelSocketClassV3 {
                channel,
                key: profile.key
            }]
        );
        assert_eq!(
            v3.receive_model,
            ReceiveModelV1::DeclaredCopyUnitsV1 {
                units: vec![ReceiveCopyUnitV1 {
                    input_ordinal: 0,
                    channel,
                    stream_offset: 0,
                    length: 3
                }]
            }
        );
        let mut frame = Vec::new();
        trace.write_framed(&mut frame).unwrap();
        assert_eq!(
            NetworkTrace::read_framed(std::io::Cursor::new(&frame)).unwrap(),
            trace
        );
        let mut replay =
            NetworkReplayEngine::replay_from_reader(std::io::Cursor::new(frame)).unwrap();
        let alias = open_file(91);
        replay
            .register_stream_socket(
                alias,
                profile.key,
                NetworkStreamNamespace {
                    device: 88,
                    inode: 999,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            replay
                .ensure_channel(alias, endpoint_binding(endpoint(443)))
                .unwrap(),
            channel
        );
        replay.release_eligible(time(1)).unwrap();
        assert_eq!(replay.stream_queue_status(alias).unwrap().queued_bytes, 3);
        assert!(replay.shadow_mode());
    }

    fn poll_readiness_review_publish(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        flags: i16,
        first: bool,
    ) {
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        complete_probe_evidence(engine, owner, &probe, 1, 1, flags);
        engine
            .complete_shadow_probe(
                owner,
                probe.lease,
                time(1),
                if first { b"a".to_vec() } else { Vec::new() },
                false,
            )
            .unwrap();
    }

    #[test]
    fn poll_readiness_review_core_error_and_hup_are_not_invented_receive_errors() {
        for flags in [libc::POLLERR, libc::POLLHUP] {
            let (mut e, ofd, owner, call) = shadow_probe_fixture();
            poll_readiness_review_publish(&mut e, owner, call, flags, true);
            let status = e.stream_call_queue_status(owner, call).unwrap();
            assert_eq!(status.queued_bytes, 1);
            assert_eq!(status.error, None);
            assert!(!status.eof && !status.local_read_shutdown);
            assert!(!e.stream_ready_at_least(ofd, 8).unwrap());
            assert!(e.terminal_readiness(ofd).unwrap());
            assert!(e.receive_half_closed(ofd).unwrap());
            assert!(e.poll_readable_at_least(ofd, 8).unwrap());
            poll_readiness_review_publish(&mut e, owner, call, 0, false);
            assert!(!e.terminal_readiness(ofd).unwrap());
            assert!(!e.receive_half_closed(ofd).unwrap());
            assert!(!e.poll_readable_at_least(ofd, 8).unwrap());
            assert!(e.poll_readable_at_least(ofd, 1).unwrap());
            assert_eq!(
                e.stream_call_queue_status(owner, call)
                    .unwrap()
                    .queued_bytes,
                1
            );
        }
    }

    #[test]
    fn poll_readiness_review_core_control_and_delivery_block_then_reenable_poll() {
        for flags in [libc::POLLERR, libc::POLLHUP] {
            let (mut e, ofd, owner, call) = shadow_probe_fixture();
            poll_readiness_review_publish(&mut e, owner, call, flags, true);
            let other = stream_owner(2);
            let control = e.begin_socket_controls(other, vec![ofd]).unwrap()[0].1;
            assert!(!e.receive_half_closed(ofd).unwrap());
            assert!(!e.poll_readable_at_least(ofd, 8).unwrap());
            e.finish_socket_control(other, control, NetworkSocketControlFinish::Unchanged)
                .unwrap();
            let chunk = e.reserve_stream_call_chunk(owner, call, 1, 0).unwrap();
            let (lease, _) = reserved_chunk(chunk);
            assert!(!e.receive_half_closed(ofd).unwrap());
            assert!(!e.poll_readable_at_least(ofd, 8).unwrap());
            e.finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
                .unwrap();
            assert!(e.receive_half_closed(ofd).unwrap());
            assert!(e.poll_readable_at_least(ofd, 8).unwrap());
            let control = e.begin_socket_controls(other, vec![ofd]).unwrap()[0].1;
            e.stream_owner_gone(other);
            assert!(
                matches!(e.receive_half_closed(ofd), Err(NetworkReplayError::UnresolvedStreamOperation(found)) if found==control)
            );
            assert!(
                matches!(e.poll_readable_at_least(ofd, 8), Err(NetworkReplayError::UnresolvedStreamOperation(found)) if found==control)
            );
        }
    }

    #[test]
    fn shadow_terminal_interest_excludes_read_half_close_and_ordinary_readiness() {
        let mut engine = NetworkReplayEngine::record_shadow(epoch());
        let profile = test_fresh_profile(libc::AF_INET);
        let ofd = open_file(10);
        engine
            .register_stream_socket(ofd, profile.key, test_socket_namespace(), Some(profile))
            .unwrap();
        let channel = engine
            .ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        engine
            .publish_ingress(
                ofd,
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(1),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::PeerShutdown {
                        stream_offset: 0,
                        direction: NetworkShutdownV2::Write,
                    },
                },
            )
            .unwrap();
        assert!(engine.readiness(ofd).unwrap().readable);
        assert!(!engine.readiness(ofd).unwrap().hangup);
        assert!(!engine.terminal_readiness(ofd).unwrap());
        engine
            .channels
            .get_mut(&channel)
            .unwrap()
            .release(NetworkInputKindV2::Readiness(NetworkReadinessV2 {
                readable: false,
                writable: true,
                error: false,
                hangup: false,
            }));
        assert!(!engine.terminal_readiness(ofd).unwrap());
        engine
            .channels
            .get_mut(&channel)
            .unwrap()
            .release(NetworkInputKindV2::Readiness(NetworkReadinessV2 {
                readable: false,
                writable: false,
                error: false,
                hangup: true,
            }));
        assert!(engine.terminal_readiness(ofd).unwrap());
    }

    #[test]
    fn shadow_invalid_unit_preserves_journal_queue_and_frontier() {
        let mut record = NetworkReplayEngine::record_shadow(epoch());
        let profile = test_fresh_profile(libc::AF_INET);
        let ofd = open_file(40);
        record
            .register_stream_socket(ofd, profile.key, test_socket_namespace(), Some(profile))
            .unwrap();
        let channel = record
            .ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        let before = format!("{record:?}");
        let input = NetworkInputEventV2 {
            ordinal: 0,
            channel,
            release: NetworkReleaseV2 {
                not_before_global_time: time(1),
                after_transmitted_offset: 0,
            },
            event: NetworkInputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: vec![7; 1025],
            },
        };
        assert!(
            matches!(record.publish_ingress(ofd,input),Err(NetworkReplayError::InvalidShadowPublication(id)) if id==channel)
        );
        assert_eq!(format!("{record:?}"), before);
    }

    fn poll_current_profile_set_lowat(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        ofd: OpenFileId,
        minimum: i32,
    ) {
        let control = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        // User-lock the buffer through the modeled setter; LOWAT changes after
        // ingress must not bypass the unresolved autotuning guard.
        for option in [
            NetworkStreamSocketOption::ReceiveBuffer(131072),
            NetworkStreamSocketOption::ReceiveLowWater(minimum),
        ] {
            engine
                .submit_stream_physical(
                    owner,
                    control,
                    NetworkStreamPhysicalEffect::SetSocketOption { option },
                )
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    control,
                    NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
                )
                .unwrap();
        }
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }

    #[test]
    fn poll_current_profile_lower_raise_and_unchanged_preserve_receive_target() {
        let (mut e, ofd, owner, call) = shadow_probe_fixture();
        poll_readiness_review_publish(&mut e, owner, call, 0, true);
        for (lowat, ready) in [(10, false), (10, false), (1, true), (10, false)] {
            poll_current_profile_set_lowat(&mut e, owner, ofd, lowat);
            assert_eq!(e.poll_readable(ofd).unwrap(), ready);
            assert!(!e.stream_ready_at_least(ofd, 10).unwrap());
            assert!(e.stream_ready_at_least(ofd, 1).unwrap());
            assert!(!e.poll_readable_at_least(ofd, 10).unwrap());
            assert_eq!(e.stream_queue_status(ofd).unwrap().queued_bytes, 1);
        }
    }

    #[test]
    fn poll_current_profile_unrelated_ofd_update_cannot_wake_socket() {
        let (mut e, ofd, owner, call) = shadow_probe_fixture();
        poll_readiness_review_publish(&mut e, owner, call, 0, true);
        poll_current_profile_set_lowat(&mut e, owner, ofd, 10);
        let other = open_file(61);
        let profile = test_fresh_profile(libc::AF_INET);
        e.register_stream_socket(other, profile.key, test_socket_namespace(), Some(profile))
            .unwrap();
        e.ensure_channel(other, endpoint_binding(endpoint(444)))
            .unwrap();
        poll_current_profile_set_lowat(&mut e, owner, other, 1);
        assert!(!e.poll_readable(ofd).unwrap());
        assert!(!e.poll_readable(other).unwrap());
        assert_eq!(e.stream_queue_status(ofd).unwrap().queued_bytes, 1);
        assert_eq!(e.stream_queue_status(other).unwrap().queued_bytes, 0);
    }

    #[test]
    fn poll_current_profile_control_and_delivery_exclusion_preserve_errors() {
        for flags in [libc::POLLERR, libc::POLLHUP] {
            let (mut e, ofd, owner, call) = shadow_probe_fixture();
            poll_current_profile_set_lowat(&mut e, owner, ofd, 10);
            poll_readiness_review_publish(&mut e, owner, call, flags, true);
            assert!(e.poll_readable(ofd).unwrap());
            assert!(!e.stream_ready_at_least(ofd, 10).unwrap());
            let other = stream_owner(2);
            let control = e.begin_socket_controls(other, vec![ofd]).unwrap()[0].1;
            assert!(!e.poll_readable(ofd).unwrap());
            e.finish_socket_control(other, control, NetworkSocketControlFinish::Unchanged)
                .unwrap();
            let (lease, _) =
                reserved_chunk(e.reserve_stream_call_chunk(owner, call, 1, 0).unwrap());
            assert!(!e.poll_readable(ofd).unwrap());
            e.finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
                .unwrap();
            assert!(e.poll_readable(ofd).unwrap());
            let control = e.begin_socket_controls(other, vec![ofd]).unwrap()[0].1;
            e.stream_owner_gone(other);
            assert!(matches!(e.poll_readable(ofd),
                Err(NetworkReplayError::UnresolvedStreamOperation(found)) if found == control));
        }
    }

    fn shadow_probe_fixture() -> (
        NetworkReplayEngine,
        OpenFileId,
        NetworkStreamOwner,
        NetworkStreamCallId,
    ) {
        let mut engine = NetworkReplayEngine::record_shadow(epoch());
        let profile = test_fresh_profile(libc::AF_INET);
        let ofd = open_file(60);
        let owner = stream_owner(1);
        engine
            .register_stream_socket(ofd, profile.key, test_socket_namespace(), Some(profile))
            .unwrap();
        engine
            .ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        let call = active_pinned_call(&mut engine, ofd, owner);
        (engine, ofd, owner, call)
    }

    fn complete_probe_evidence(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        probe: &NetworkShadowProbe,
        count: usize,
        queued: usize,
        revents: i16,
    ) {
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(-1),
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::Peek {
                    maximum: probe.retained_prefix + 1024,
                },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::Peeked { count },
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::PollState,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PollState { revents },
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::QueuedBytes,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::QueuedBytes { count: queued },
            )
            .unwrap();
    }

    #[test]
    fn shadow_probe_frontier_harvests_fixed_horizon_before_readiness_decision() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        for index in 0..4 {
            let probe = engine
                .begin_shadow_probe(owner, call, time(index + 1))
                .unwrap();
            assert_eq!(probe.captured_through, index * 1024);
            assert_eq!(probe.retained_prefix, index as usize * 1024);
            complete_probe_evidence(
                &mut engine,
                owner,
                &probe,
                (index as usize + 1) * 1024,
                4096,
                libc::POLLIN | libc::POLLOUT,
            );
            engine
                .complete_shadow_probe(
                    owner,
                    probe.lease,
                    time(index + 1),
                    vec![index as u8; 1024],
                    false,
                )
                .unwrap();
            assert_eq!(
                engine.stream_queue_status(ofd).unwrap().queued_bytes,
                (index as usize + 1) * 1024
            );
            assert_eq!(engine.stream_ready_at_least(ofd, 4096).unwrap(), index == 3);
        }
        assert_eq!(engine.shadow.as_ref().unwrap().units.len(), 4);
        let EngineState::Record(trace) = &engine.mode else {
            unreachable!()
        };
        assert_eq!(
            trace
                .inputs
                .iter()
                .filter(|input| matches!(input.event, NetworkInputKindV2::Readiness(_)))
                .count(),
            1
        );
        assert!(
            !engine.channels[&engine.channel_for(ofd).unwrap()]
                .explicit_readiness
                .readable
        );
    }

    #[test]
    fn incomplete_probe_or_wrong_count_never_loses_the_submitted_effect() {
        let (mut engine, _, owner, call) = shadow_probe_fixture();
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        assert!(
            matches!(engine.finish_socket_control(owner,probe.lease,NetworkSocketControlFinish::Unchanged),Err(NetworkReplayError::UnresolvedStreamOperation(id)) if id==probe.lease)
        );
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(-1),
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::Peeked { count: 1025 }
            ),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
        assert_eq!(format!("{engine:?}"), before);
        assert!(engine.shadow_probes[&probe.lease].pending.is_some());
        engine.stream_owner_gone(owner);
        assert!(matches!(
            engine.into_recorded_versioned_trace(),
            Err(NetworkReplayError::UnresolvedStreamCall(_))
        ));
    }

    #[test]
    fn shadow_eof_needs_ordered_shutdown_and_complete_kernel_queue_evidence() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        assert!(matches!(
            engine.submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::QueuedBytes
            ),
            Err(NetworkReplayError::StreamLeaseKindMismatch(_))
        ));
        complete_probe_evidence(
            &mut engine,
            owner,
            &probe,
            1024,
            2048,
            libc::POLLRDHUP | libc::POLLIN,
        );
        let before = format!("{engine:?}");
        assert!(matches!(
            engine.complete_shadow_probe(owner, probe.lease, time(1), vec![1; 1024], true),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
        assert_eq!(format!("{engine:?}"), before);
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), vec![1; 1024], false)
            .unwrap();
        let probe = engine.begin_shadow_probe(owner, call, time(2)).unwrap();
        complete_probe_evidence(
            &mut engine,
            owner,
            &probe,
            2048,
            2048,
            libc::POLLRDHUP | libc::POLLIN,
        );
        engine
            .complete_shadow_probe(owner, probe.lease, time(2), vec![2; 1024], true)
            .unwrap();
        let status = engine.stream_queue_status(ofd).unwrap();
        assert_eq!(status.queued_bytes, 2048);
        assert!(status.eof);
        assert!(!engine.readiness(ofd).unwrap().hangup);
    }

    #[test]
    fn temporary_nonnegative_cursor_must_be_restored_before_publication() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        engine
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&ofd)
            .unwrap()
            .options
            .peek_offset = Some(8);
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(8),
            )
            .unwrap();
        assert!(
            engine
                .submit_shadow_probe_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::Peek { maximum: 1024 }
                )
                .is_err()
        );
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(owner, probe.lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::Peeked { count: 3 },
            )
            .unwrap();
        assert!(
            engine
                .submit_shadow_probe_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::PollState
                )
                .is_err()
        );
        assert!(
            engine
                .submit_shadow_probe_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::SetPeekOffset { value: 7 }
                )
                .is_err()
        );
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: 8 },
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(owner, probe.lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::PollState,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PollState {
                    revents: libc::POLLIN,
                },
            )
            .unwrap();
        engine
            .submit_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::QueuedBytes,
            )
            .unwrap();
        engine
            .confirm_shadow_probe_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::QueuedBytes { count: 3 },
            )
            .unwrap();
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), b"abc".to_vec(), false)
            .unwrap();
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .options
                .peek_offset,
            Some(8)
        );
    }

    fn publish_shadow_test_unit(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        call: NetworkStreamCallId,
        bytes: &[u8],
    ) {
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        complete_probe_evidence(
            engine,
            owner,
            &probe,
            probe.retained_prefix + bytes.len(),
            probe.retained_prefix + bytes.len(),
            libc::POLLIN,
        );
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), bytes.to_vec(), false)
            .unwrap();
    }

    fn confirm_local_shutdown(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        ofd: OpenFileId,
        direction: NetworkShutdownV2,
    ) {
        let control = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        engine
            .submit_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalEffect::Shutdown { direction },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalResult::Shutdown { result: Ok(()) },
            )
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }

    #[test]
    fn local_read_shutdown_wakes_zero_without_forging_peer_fin_and_keeps_later_data() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        let channel = engine.channel_for(ofd).unwrap();
        let NetworkZeroStreamReceive::Waiting(wait) =
            engine.prepare_zero_stream_receive(owner, call, 8).unwrap()
        else {
            panic!("empty wait");
        };
        engine
            .begin_zero_stream_wait(
                owner,
                call,
                Some(crate::resources::ExternalOpId::new(owner.thread, 2)),
            )
            .unwrap();
        engine.enter_zero_stream_wait(owner, wait).unwrap();
        let input_generation = engine.channels[&channel].receive_input_generation;
        confirm_local_shutdown(&mut engine, stream_owner(2), ofd, NetworkShutdownV2::Read);
        assert!(engine.zero_stream_wait_ready(owner, wait).unwrap());
        assert_eq!(
            engine.channels[&channel].receive_input_generation,
            input_generation
        );
        assert_eq!(engine.channels[&channel].local_control_generation, 1);
        assert!(!engine.channels[&channel].local_write_closed);
        assert_eq!(
            engine.prepare_zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::Ready
        );
        assert!(engine.finish_zero_stream_wait(owner, wait).unwrap());
        assert_eq!(
            engine.zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::EndOfFile
        );
        assert_eq!(
            engine.reserve_stream_call_chunk(owner, call, 1, 0).unwrap(),
            NetworkStreamChunk::LocalReadClosed
        );
        let status = engine.stream_call_queue_status(owner, call).unwrap();
        assert!(status.local_read_shutdown && status.readiness.readable);
        assert!(!status.eof && !status.readiness.hangup);
        assert!(engine.stream_ready_at_least(ofd, 4096).unwrap());
        assert!(engine.receive_half_closed(ofd).unwrap());
        assert!(!engine.terminal_readiness(ofd).unwrap());
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        assert!(probe.local_read_shutdown);
        complete_probe_evidence(
            &mut engine,
            owner,
            &probe,
            0,
            0,
            libc::POLLIN | libc::POLLOUT | libc::POLLRDHUP,
        );
        let before = format!("{engine:?}");
        assert!(
            engine
                .complete_shadow_probe(owner, probe.lease, time(1), Vec::new(), true)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), Vec::new(), false)
            .unwrap();
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        complete_probe_evidence(
            &mut engine,
            owner,
            &probe,
            1,
            1,
            libc::POLLIN | libc::POLLOUT | libc::POLLRDHUP,
        );
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), b"d".to_vec(), false)
            .unwrap();
        assert_eq!(
            engine.zero_stream_receive(owner, call, 0).unwrap(),
            NetworkZeroStreamReceive::Ready
        );
        assert_eq!(
            engine.reserve_stream_call_chunk(owner, call, 1, 8).unwrap(),
            NetworkStreamChunk::LocalReadClosed
        );
        let NetworkStreamChunk::Reserved { lease, outcome, .. } =
            engine.reserve_stream_call_chunk(owner, call, 1, 0).unwrap()
        else {
            panic!("later byte lost");
        };
        assert_eq!(outcome, NetworkStreamChunkOutcome::Bytes(b"d".to_vec()));
        engine.begin_record_drain(owner, lease).unwrap();
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::Drain { maximum: 1 },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::Drained {
                    bytes: b"d".to_vec(),
                },
            )
            .unwrap();
        engine.finish_record_drain(owner, lease).unwrap();
        assert_eq!(
            engine
                .stream_call_queue_status(owner, call)
                .unwrap()
                .queued_bytes,
            0
        );
        assert_eq!(
            engine.zero_stream_receive(owner, call, 0).unwrap(),
            NetworkZeroStreamReceive::EndOfFile
        );
        assert!(
            !engine.channels[&channel]
                .published_ingress
                .unwrap()
                .terminal
        );
        let EngineState::Record(trace) = &engine.mode else {
            unreachable!()
        };
        assert!(
            !trace
                .inputs
                .iter()
                .any(|input| matches!(input.event, NetworkInputKindV2::PeerShutdown { .. }))
        );
        assert_eq!(trace.outputs.len(), 1);
    }

    #[test]
    fn local_shutdown_failure_and_unknown_effect_keep_exact_custody() {
        let (mut engine, ofd, owner, _call) = shadow_probe_fixture();
        let channel = engine.channel_for(ofd).unwrap();
        let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::Shutdown {
                    direction: NetworkShutdownV2::Read,
                },
            )
            .unwrap();
        assert!(engine.check_stream_operations_finished().is_err());
        assert!(
            engine
                .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
                .is_err()
        );
        assert!(!engine.channels[&channel].local_read_shutdown);
        engine
            .confirm_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::Shutdown {
                    result: Err(libc::ENOTCONN),
                },
            )
            .unwrap();
        assert!(!engine.channels[&channel].local_read_shutdown);
        assert_eq!(engine.channels[&channel].local_control_generation, 0);
        let EngineState::Record(trace) = &engine.mode else {
            unreachable!()
        };
        assert!(trace.outputs.is_empty());
        engine
            .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let lease = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::Shutdown {
                    direction: NetworkShutdownV2::Read,
                },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::Shutdown { result: Ok(()) },
            )
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .confirm_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalResult::Shutdown { result: Ok(()) }
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }

    #[test]
    fn local_shutdown_replay_keeps_preexisting_and_future_payload_and_write_direction() {
        let (mut record, ofd, owner, call) = shadow_probe_fixture();
        let channel = record.channel_for(ofd).unwrap();
        publish_shadow_test_unit(&mut record, owner, call, b"abc");
        confirm_local_shutdown(&mut record, owner, ofd, NetworkShutdownV2::Read);
        record
            .record_output(NetworkOutputEventV2 {
                channel,
                event: NetworkOutputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"Z".to_vec(),
                },
            })
            .unwrap();
        publish_shadow_test_unit(&mut record, owner, call, b"d");
        record.begin_stream_call_release(owner, call).unwrap();
        record.finish_stream_call_release(owner, call).unwrap();
        let trace = record.into_recorded_versioned_trace().unwrap();
        let mut replay = NetworkReplayEngine::replay_versioned(trace).unwrap();
        let profile = test_fresh_profile(libc::AF_INET);
        replay
            .register_stream_socket(ofd, profile.key, test_socket_namespace(), None)
            .unwrap();
        replay
            .ensure_channel(ofd, endpoint_binding(endpoint(443)))
            .unwrap();
        replay.release_eligible(time(1)).unwrap();
        assert_eq!(replay.stream_queue_status(ofd).unwrap().queued_bytes, 3);
        let lease = replay.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        let before = format!("{replay:?}");
        assert!(
            replay
                .submit_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalEffect::Shutdown {
                        direction: NetworkShutdownV2::Both
                    }
                )
                .is_err()
        );
        assert_eq!(format!("{replay:?}"), before);
        replay
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::Shutdown {
                    direction: NetworkShutdownV2::Read,
                },
            )
            .unwrap();
        replay
            .confirm_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalResult::Shutdown { result: Ok(()) },
            )
            .unwrap();
        let call = replay.begin_stream_call(owner, lease).unwrap();
        assert!(!call.physical_pin_required);
        replay
            .finish_socket_control(owner, lease, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        let status = replay.stream_call_queue_status(owner, call.id).unwrap();
        assert!(status.local_read_shutdown && !status.eof && !status.readiness.hangup);
        assert_eq!(status.queued_bytes, 3);
        assert_eq!(
            replay.transmit_stream(ofd, b"Z").unwrap(),
            StreamTransmitOutcome::Accepted(1)
        );
        replay.release_eligible(time(1)).unwrap();
        assert_eq!(
            replay
                .stream_call_queue_status(owner, call.id)
                .unwrap()
                .queued_bytes,
            4
        );
        for peek_offset in [0, 1, 3] {
            let NetworkStreamChunk::Reserved { lease, outcome, .. } = replay
                .reserve_stream_call_chunk(owner, call.id, 1, peek_offset)
                .unwrap()
            else {
                panic!("queued bytes lost");
            };
            assert_eq!(
                outcome,
                NetworkStreamChunkOutcome::Bytes(vec![b"abcd"[peek_offset]])
            );
            replay
                .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::Peeked)
                .unwrap();
        }
        assert_eq!(
            replay
                .reserve_stream_call_chunk(owner, call.id, 1, 8)
                .unwrap(),
            NetworkStreamChunk::LocalReadClosed
        );
        assert_eq!(
            replay
                .stream_call_queue_status(owner, call.id)
                .unwrap()
                .queued_bytes,
            4
        );
    }

    #[test]
    fn local_read_shutdown_preserves_error_priority_and_does_not_record_local_full_hangup() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        let channel = engine.channel_for(ofd).unwrap();
        confirm_local_shutdown(&mut engine, owner, ofd, NetworkShutdownV2::Both);
        let probe = engine.begin_shadow_probe(owner, call, time(1)).unwrap();
        complete_probe_evidence(
            &mut engine,
            owner,
            &probe,
            0,
            0,
            libc::POLLIN | libc::POLLOUT | libc::POLLRDHUP | libc::POLLHUP,
        );
        engine
            .complete_shadow_probe(owner, probe.lease, time(1), Vec::new(), false)
            .unwrap();
        assert!(engine.readiness(ofd).unwrap().hangup);
        assert!(engine.terminal_readiness(ofd).unwrap());
        assert!(!engine.channels[&channel].explicit_readiness.hangup);
        engine
            .publish_ingress(
                ofd,
                NetworkInputEventV2 {
                    ordinal: 0,
                    channel,
                    release: NetworkReleaseV2 {
                        not_before_global_time: time(1),
                        after_transmitted_offset: 0,
                    },
                    event: NetworkInputKindV2::SocketError {
                        stream_offset: 0,
                        errno: libc::ECONNRESET,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            engine.zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::Error(libc::ECONNRESET)
        );
        assert_eq!(
            engine.zero_stream_receive(owner, call, 8).unwrap(),
            NetworkZeroStreamReceive::EndOfFile
        );
    }

    #[test]
    fn shadow_delivery_keeps_whole_unit_after_second_view_fault_then_drains_exactly() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        let payload = (0..1024)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        publish_shadow_test_unit(&mut engine, owner, call, &payload);
        let NetworkStreamChunk::Reserved {
            lease,
            selection_len,
            outcome,
        } = engine
            .reserve_stream_call_chunk(owner, call, 4096, 0)
            .unwrap()
        else {
            panic!("queued unit")
        };
        assert_eq!(selection_len, 1024);
        assert_eq!(
            outcome,
            NetworkStreamChunkOutcome::Bytes(payload[..512].to_vec())
        );
        assert_eq!(
            engine
                .read_stream_chunk_view(owner, lease, 512, 512)
                .unwrap(),
            payload[512..]
        );
        engine
            .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
            .unwrap();
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 1024);
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .consume_epoch,
            0
        );
        let NetworkStreamChunk::Reserved { lease, .. } = engine
            .reserve_stream_call_chunk(owner, call, 4096, 0)
            .unwrap()
        else {
            unreachable!()
        };
        assert!(matches!(
            engine.finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::Consumed),
            Err(NetworkReplayError::UnresolvedStreamOperation(_))
        ));
        engine.begin_record_drain(owner, lease).unwrap();
        for offset in [0, 512] {
            engine
                .submit_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalEffect::Drain { maximum: 512 },
                )
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalResult::Drained {
                        bytes: payload[offset..offset + 512].to_vec(),
                    },
                )
                .unwrap();
            assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 1024);
        }
        engine.finish_record_drain(owner, lease).unwrap();
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 0);
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .consume_epoch,
            1
        );
    }

    #[test]
    fn shadow_drain_mismatch_and_unknown_completion_stay_unresolved() {
        for observed in [
            NetworkStreamPhysicalResult::Drained {
                bytes: b"abd".to_vec(),
            },
            NetworkStreamPhysicalResult::Drained { bytes: Vec::new() },
            NetworkStreamPhysicalResult::Errno(libc::EAGAIN),
        ] {
            let (mut engine, ofd, owner, call) = shadow_probe_fixture();
            publish_shadow_test_unit(&mut engine, owner, call, b"abc");
            let NetworkStreamChunk::Reserved { lease, .. } =
                engine.reserve_stream_call_chunk(owner, call, 3, 0).unwrap()
            else {
                unreachable!()
            };
            engine.begin_record_drain(owner, lease).unwrap();
            engine
                .submit_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalEffect::Drain { maximum: 3 },
                )
                .unwrap();
            let before = format!("{engine:?}");
            assert!(matches!(
                engine.confirm_stream_physical(owner, lease, observed),
                Err(NetworkReplayError::UnresolvedStreamOperation(_))
            ));
            assert_eq!(format!("{engine:?}"), before);
            assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 3);
            assert!(engine.finish_record_drain(owner, lease).is_err());
            assert!(
                engine
                    .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
                    .is_err()
            );
        }
    }

    #[test]
    fn shadow_peek_cursor_uses_kernel_signed_clamp_and_requires_physical_confirmation() {
        let (mut engine, ofd, owner, call) = shadow_probe_fixture();
        publish_shadow_test_unit(&mut engine, owner, call, b"abc");
        engine
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&ofd)
            .unwrap()
            .options
            .peek_offset = Some(i32::MAX - 1);
        let NetworkStreamChunk::Reserved { lease, .. } =
            engine.reserve_stream_call_chunk(owner, call, 3, 0).unwrap()
        else {
            unreachable!()
        };
        assert!(
            engine
                .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::Peeked)
                .is_err()
        );
        let before = format!("{engine:?}");
        assert!(
            engine
                .submit_stream_physical(
                    owner,
                    lease,
                    NetworkStreamPhysicalEffect::SetPeekOffset {
                        value: i32::MIN + 1
                    }
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: 0 },
            )
            .unwrap();
        engine
            .confirm_stream_physical(owner, lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
        engine
            .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::Peeked)
            .unwrap();
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .options
                .peek_offset,
            Some(0)
        );
        assert_eq!(engine.stream_queue_status(ofd).unwrap().queued_bytes, 3);
        assert_eq!(
            engine
                .stream_call_socket_state(owner, call)
                .unwrap()
                .consume_epoch,
            0
        );
    }
    fn accepted_record_fixture() -> (
        NetworkReplayEngine,
        OpenFileId,
        NetworkStreamOwner,
        accepted::AcceptedBackendCapability,
    ) {
        use detcore_model::network_trace::ReceiveTimeoutV3;
        let cap = accepted::AcceptedBackendCapability::controlled_fixture();
        let mut engine = NetworkReplayEngine::record_shadow_accepted(
            epoch(),
            accepted::AcceptedBackendCapability::controlled_fixture(),
        );
        let ofd = open_file(100);
        let owner = stream_owner(31);
        let profile = test_fresh_profile(libc::AF_INET);
        engine
            .register_accepted_fresh_send(profile.key, Some(ReceiveTimeoutV3::Infinite))
            .unwrap();
        engine
            .register_stream_socket(ofd, profile.key, test_socket_namespace(), Some(profile))
            .unwrap();
        engine
            .ensure_channel(
                ofd,
                NetworkChannelBinding {
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::Listener,
                    peer_address: None,
                    requested_local_constraint: None,
                    observed_local_address: Some(NetworkAddressV2::Inet4 {
                        address: [127, 0, 0, 1],
                        port: 12345,
                    }),
                    accepted_from: None,
                    selected_channel: None,
                },
            )
            .unwrap();
        engine
            .enroll_accepted_listener(
                ofd,
                accepted::AcceptedPhysicalIdentity {
                    provider: 7,
                    object: 10,
                    namespace: 9,
                },
                &cap,
            )
            .unwrap();
        (engine, ofd, owner, cap)
    }
    fn accepted_set(
        engine: &mut NetworkReplayEngine,
        ofd: OpenFileId,
        owner: NetworkStreamOwner,
        option: NetworkStreamSocketOption,
    ) {
        let control = engine.begin_socket_controls(owner, vec![ofd]).unwrap()[0].1;
        engine
            .submit_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalEffect::SetSocketOption { option },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
            )
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
    }
    fn accepted_observe(
        engine: &mut NetworkReplayEngine,
        ofd: OpenFileId,
        cap: &accepted::AcceptedBackendCapability,
    ) -> detcore_model::network_trace::ChildCreationIdV1 {
        let inherited = engine.stream_socket_state(ofd).unwrap().unwrap();
        engine
            .observe_child_creation(
                ofd,
                accepted::ChildCreationCertificate {
                    sequence: 1,
                    listener: accepted::AcceptedPhysicalIdentity {
                        provider: 7,
                        object: 10,
                        namespace: 9,
                    },
                    child: accepted::AcceptedPhysicalIdentity {
                        provider: 7,
                        object: 11,
                        namespace: 9,
                    },
                    listener_generation: inherited.option_generation,
                    inherited,
                    local: NetworkAddressV2::Inet4 {
                        address: [127, 0, 0, 1],
                        port: 12345,
                    },
                    peer: NetworkAddressV2::Inet4 {
                        address: [127, 0, 0, 1],
                        port: 54321,
                    },
                },
                time(10),
                cap,
            )
            .unwrap()
    }
    fn accepted_install(
        engine: &mut NetworkReplayEngine,
        listener: OpenFileId,
        owner: NetworkStreamOwner,
        child: detcore_model::network_trace::ChildCreationIdV1,
        cap: &accepted::AcceptedBackendCapability,
        installed: OpenFileId,
    ) -> NetworkAcceptedCompletion {
        let call = active_pinned_call(engine, listener, owner);
        let reservation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine
            .submit_accepted_socket(owner, reservation.lease)
            .unwrap();
        engine
            .confirm_accepted_installation(
                owner,
                reservation.lease,
                accepted::AcceptedInstallationFact::Installed {
                    child,
                    fd: 17,
                    open_file: installed,
                    slot_generation: 1,
                    physical: accepted::AcceptedPhysicalIdentity {
                        provider: 7,
                        object: 11,
                        namespace: 9,
                    },
                },
                cap,
            )
            .unwrap();
        let done = engine
            .complete_accepted_socket(owner, reservation.lease, Ok(17), Some(installed), time(20))
            .unwrap()
            .unwrap();
        assert_eq!(
            engine
                .complete_accepted_socket(
                    owner,
                    reservation.lease,
                    Ok(17),
                    Some(installed),
                    time(21)
                )
                .unwrap(),
            Some(done.clone())
        );
        engine.begin_stream_call_release(owner, call).unwrap();
        engine.finish_stream_call_release(owner, call).unwrap();
        done
    }
    fn accepted_trace_fixture() -> NetworkTraceV3 {
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveLowWater(3),
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveTimeout {
                seconds: 2,
                microseconds: 0,
            },
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::SendTimeout {
                seconds: 2,
                microseconds: 0,
            },
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::PeekOffset(1),
        );
        let child = accepted_observe(&mut engine, listener, &cap);
        accepted_install(&mut engine, listener, owner, child, &cap, open_file(101));
        let NetworkTrace::V3(trace) = engine.into_recorded_versioned_trace().unwrap() else {
            panic!("expected explicit V3")
        };
        trace
    }
    #[test]
    fn accepted_child_keeps_creation_options_after_listener_changes_and_exact_endpoints() {
        use detcore_model::network_trace::ReceiveTimeoutV3;
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveLowWater(3),
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveTimeout {
                seconds: 2,
                microseconds: 0,
            },
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::SendTimeout {
                seconds: 2,
                microseconds: 0,
            },
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::PeekOffset(1),
        );
        let child = accepted_observe(&mut engine, listener, &cap);
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveLowWater(9),
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::SendTimeout {
                seconds: -1,
                microseconds: 0,
            },
        );
        let ofd = open_file(101);
        accepted_install(&mut engine, listener, owner, child, &cap, ofd);
        let state = engine.stream_socket_state(ofd).unwrap().unwrap();
        assert_eq!(state.options.receive_low_water, 3);
        assert_eq!(
            state.options.receive_timeout,
            ReceiveTimeoutV3::FiniteTicks(2000)
        );
        assert_eq!(
            state.send_timeout,
            Some(ReceiveTimeoutV3::FiniteTicks(2000))
        );
        assert_eq!(state.options.peek_offset, Some(1));
        assert_eq!(
            engine
                .stream_socket_state(listener)
                .unwrap()
                .unwrap()
                .options
                .receive_low_water,
            9
        );
        assert_eq!(
            engine.accepted_endpoint(ofd, false).unwrap(),
            Some(NetworkAddressV2::Inet4 {
                address: [127, 0, 0, 1],
                port: 12345
            })
        );
        assert_eq!(
            engine.accepted_endpoint(ofd, true).unwrap(),
            Some(NetworkAddressV2::Inet4 {
                address: [127, 0, 0, 1],
                port: 54321
            })
        );
        let trace = engine.into_recorded_versioned_trace().unwrap();
        let NetworkTrace::V3(trace) = trace else {
            panic!("wrong model")
        };
        trace.validate().unwrap();
        assert!(matches!(
            trace.receive_model,
            ReceiveModelV1::DeclaredCopyUnitsWithAcceptV2 { .. }
        ));
    }
    #[test]
    fn accepted_replay_inherits_at_child_release_across_changed_listener_schedule() {
        use detcore_model::network_trace::ReceiveTimeoutV3;
        let trace = accepted_trace_fixture();
        for (before, after, expected) in [(3, 9, 3), (9, 3, 9)] {
            let mut engine = NetworkReplayEngine::replay_shadow(trace.clone()).unwrap();
            let listener = open_file(201);
            let owner = stream_owner(81);
            let profile = test_fresh_profile(libc::AF_INET);
            engine
                .register_accepted_fresh_send(profile.key, None)
                .unwrap();
            engine
                .register_stream_socket(listener, profile.key, test_socket_namespace(), None)
                .unwrap();
            engine
                .ensure_channel(
                    listener,
                    NetworkChannelBinding {
                        transport: NetworkTransportV2::Tcp,
                        role: NetworkEndpointRoleV2::Listener,
                        peer_address: None,
                        requested_local_constraint: None,
                        observed_local_address: None,
                        accepted_from: None,
                        selected_channel: None,
                    },
                )
                .unwrap();
            accepted_set(
                &mut engine,
                listener,
                owner,
                NetworkStreamSocketOption::ReceiveLowWater(before),
            );
            accepted_set(
                &mut engine,
                listener,
                owner,
                NetworkStreamSocketOption::SendTimeout {
                    seconds: -1,
                    microseconds: 0,
                },
            );
            assert_eq!(engine.next_child_release(), Some(time(10)));
            engine.release_eligible(time(9)).unwrap();
            engine.release_eligible(time(10)).unwrap();
            accepted_set(
                &mut engine,
                listener,
                owner,
                NetworkStreamSocketOption::ReceiveLowWater(after),
            );
            accepted_set(
                &mut engine,
                listener,
                owner,
                NetworkStreamSocketOption::SendTimeout {
                    seconds: 2,
                    microseconds: 0,
                },
            );
            engine.release_eligible(time(20)).unwrap();
            let cap = accepted::AcceptedBackendCapability::controlled_fixture();
            let ofd = open_file(202);
            accepted_install(
                &mut engine,
                listener,
                owner,
                detcore_model::network_trace::ChildCreationIdV1(1),
                &cap,
                ofd,
            );
            let state = engine.stream_socket_state(ofd).unwrap().unwrap();
            assert_eq!(state.options.receive_low_water, expected);
            assert_eq!(state.send_timeout, Some(ReceiveTimeoutV3::FiniteTicks(0)));
            engine.finish().unwrap();
        }
    }
    #[test]
    fn accepted_creation_rejects_wrong_generation_identity_and_sequence_without_mutation() {
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveLowWater(3),
        );
        let inherited = engine.stream_socket_state(listener).unwrap().unwrap();
        let cert = accepted::ChildCreationCertificate {
            sequence: 1,
            listener: accepted::AcceptedPhysicalIdentity {
                provider: 7,
                object: 10,
                namespace: 9,
            },
            child: accepted::AcceptedPhysicalIdentity {
                provider: 7,
                object: 11,
                namespace: 9,
            },
            listener_generation: inherited.option_generation,
            inherited,
            local: NetworkAddressV2::Inet4 {
                address: [127, 0, 0, 1],
                port: 12345,
            },
            peer: NetworkAddressV2::Inet4 {
                address: [127, 0, 0, 1],
                port: 54321,
            },
        };
        for case in 0..4 {
            let mut bad = cert.clone();
            match case {
                0 => bad.listener_generation += 1,
                1 => bad.sequence += 1,
                2 => bad.listener.object += 1,
                _ => bad.child.namespace += 1,
            };
            let before = format!("{engine:?}");
            assert!(
                engine
                    .observe_child_creation(listener, bad, time(10), &cap)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        engine
            .observe_child_creation(listener, cert, time(10), &cap)
            .unwrap();
        assert!(matches!(
            engine.check_stream_operations_finished(),
            Err(NetworkReplayError::UnresolvedAcceptedChild(_))
        ));
    }
    #[test]
    fn accepted_pending_operation_blocks_call_release_and_preserves_unknown_effects_on_exit() {
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        let call = active_pinned_call(&mut engine, listener, owner);
        let reservation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        assert!(
            reservation.child.is_none(),
            "Record must admit before a child exists"
        );
        assert!(
            matches!(engine.begin_stream_call_release(owner,call),Err(NetworkReplayError::UnresolvedAccept(id)) if id==reservation.lease)
        );
        let mut stale = owner;
        stale.mm = owner.mm.for_exec(owner.thread);
        assert!(
            engine
                .submit_accepted_socket(stale, reservation.lease)
                .is_err()
        );
        engine
            .cancel_accepted_socket(owner, reservation.lease)
            .unwrap();
        let reservation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine
            .submit_accepted_socket(owner, reservation.lease)
            .unwrap();
        assert!(
            engine
                .cancel_accepted_socket(owner, reservation.lease)
                .is_err()
        );
        assert!(
            engine
                .complete_accepted_socket(
                    owner,
                    reservation.lease,
                    Err(libc::EAGAIN),
                    None,
                    time(10)
                )
                .is_err()
        );
        engine
            .confirm_accepted_installation(
                owner,
                reservation.lease,
                accepted::AcceptedInstallationFact::NoConnection {
                    errno: libc::EAGAIN,
                },
                &cap,
            )
            .unwrap();
        assert_eq!(
            engine
                .complete_accepted_socket(
                    owner,
                    reservation.lease,
                    Err(libc::EAGAIN),
                    None,
                    time(10)
                )
                .unwrap(),
            None
        );
        let reservation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine
            .submit_accepted_socket(owner, reservation.lease)
            .unwrap();
        engine.stream_owner_gone(owner);
        assert!(
            engine
                .confirm_accepted_installation(
                    owner,
                    reservation.lease,
                    accepted::AcceptedInstallationFact::NoConnection { errno: libc::EINTR },
                    &cap
                )
                .is_err()
        );
        assert!(matches!(
            engine.check_stream_operations_finished(),
            Err(NetworkReplayError::UnresolvedAccept(_))
        ));
    }
    #[test]
    fn accepted_copy_fault_is_not_no_connection_or_successful_slot_installation() {
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        let child = accepted_observe(&mut engine, listener, &cap);
        let call = active_pinned_call(&mut engine, listener, owner);
        let reservation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine
            .submit_accepted_socket(owner, reservation.lease)
            .unwrap();
        engine
            .confirm_accepted_installation(
                owner,
                reservation.lease,
                accepted::AcceptedInstallationFact::DequeuedNoInstallation {
                    child,
                    errno: libc::EFAULT,
                },
                &cap,
            )
            .unwrap();
        let before = engine.bindings.clone();
        assert!(matches!(
            engine.complete_accepted_socket(
                owner,
                reservation.lease,
                Err(libc::EFAULT),
                None,
                time(20)
            ),
            Err(NetworkReplayError::UnresolvedAccept(_))
        ));
        assert_eq!(engine.bindings, before);
        assert!(engine.begin_stream_call_release(owner, call).is_err());
        assert!(engine.check_stream_operations_finished().is_err());
    }
    #[test]
    fn accepted_installation_requires_exact_child_identity_slot_and_kernel_result() {
        let (mut engine, listener, owner, cap) = accepted_record_fixture();
        let child = accepted_observe(&mut engine, listener, &cap);
        let call = active_pinned_call(&mut engine, listener, owner);
        let receipt = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine.submit_accepted_socket(owner, receipt.lease).unwrap();
        let fact = accepted::AcceptedInstallationFact::Installed {
            child,
            fd: 17,
            open_file: open_file(101),
            slot_generation: 1,
            physical: accepted::AcceptedPhysicalIdentity {
                provider: 7,
                object: 11,
                namespace: 9,
            },
        };
        for case in 0..3 {
            let mut bad = fact.clone();
            if let accepted::AcceptedInstallationFact::Installed {
                child,
                slot_generation,
                physical,
                ..
            } = &mut bad
            {
                match case {
                    0 => child.0 += 1,
                    1 => *slot_generation = 0,
                    _ => physical.object += 1,
                }
            }
            let before = format!("{engine:?}");
            assert!(
                engine
                    .confirm_accepted_installation(owner, receipt.lease, bad, &cap)
                    .is_err()
            );
            assert_eq!(format!("{engine:?}"), before);
        }
        engine
            .confirm_accepted_installation(owner, receipt.lease, fact, &cap)
            .unwrap();
        let before = format!("{engine:?}");
        assert!(
            engine
                .complete_accepted_socket(
                    owner,
                    receipt.lease,
                    Ok(18),
                    Some(open_file(101)),
                    time(20)
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(
            engine
                .complete_accepted_socket(
                    owner,
                    receipt.lease,
                    Ok(17),
                    Some(open_file(102)),
                    time(20)
                )
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let done = engine
            .complete_accepted_socket(owner, receipt.lease, Ok(17), Some(open_file(101)), time(20))
            .unwrap()
            .unwrap();
        assert_eq!(done.fd, 17);
        assert!(
            engine
                .complete_accepted_socket(
                    stream_owner(99),
                    receipt.lease,
                    Ok(17),
                    Some(open_file(101)),
                    time(20)
                )
                .is_err()
        );
    }
    #[test]
    fn accepted_two_concurrent_reservations_keep_fifo_selection_and_exact_completion() {
        use detcore_model::network_trace::ChildCreationIdV1;
        use detcore_model::network_trace::ChildDispositionV1;
        let mut trace = accepted_trace_fixture();
        let mut second = trace
            .history
            .channels
            .iter()
            .find(|c| c.role == NetworkEndpointRoleV2::Accepted)
            .unwrap()
            .clone();
        second.id = NetworkChannelId(3);
        second.peer_address = Some(NetworkAddressV2::Inet4 {
            address: [127, 0, 0, 1],
            port: 54322,
        });
        let second_peer = second.peer_address.clone();
        trace.history.channels.push(second);
        let mut input = trace.history.inputs[0].clone();
        input.ordinal = 1;
        input.event = NetworkInputKindV2::Accept {
            accepted: NetworkChannelId(3),
            peer: second_peer.clone(),
            ancillary: None,
        };
        trace.history.inputs.push(input);
        let mut class = trace
            .channel_socket_classes
            .iter()
            .find(|c| c.channel == NetworkChannelId(2))
            .unwrap()
            .clone();
        class.channel = NetworkChannelId(3);
        trace.channel_socket_classes.push(class);
        let ReceiveModelV1::DeclaredCopyUnitsWithAcceptV2 { accepted, .. } =
            &mut trace.receive_model
        else {
            panic!("explicit model required")
        };
        let mut child = accepted.children[0].clone();
        child.id = ChildCreationIdV1(2);
        child.peer = second_peer.unwrap();
        child.disposition = ChildDispositionV1::Accepted {
            channel: NetworkChannelId(3),
            input_ordinal: 1,
        };
        accepted.children.push(child);
        trace.validate().unwrap();
        for cancel_first in [false, true] {
            let mut engine = NetworkReplayEngine::replay_shadow(trace.clone()).unwrap();
            let listener = open_file(301);
            let profile = test_fresh_profile(libc::AF_INET);
            engine
                .register_accepted_fresh_send(profile.key, None)
                .unwrap();
            engine
                .register_stream_socket(listener, profile.key, test_socket_namespace(), None)
                .unwrap();
            engine
                .ensure_channel(
                    listener,
                    NetworkChannelBinding {
                        transport: NetworkTransportV2::Tcp,
                        role: NetworkEndpointRoleV2::Listener,
                        peer_address: None,
                        requested_local_constraint: None,
                        observed_local_address: None,
                        accepted_from: None,
                        selected_channel: None,
                    },
                )
                .unwrap();
            engine.release_eligible(time(20)).unwrap();
            let owner_a = stream_owner(301);
            let owner_b = stream_owner(302);
            let call_a = active_pinned_call(&mut engine, listener, owner_a);
            let call_b = active_pinned_call(&mut engine, listener, owner_b);
            let mut a = engine
                .begin_accepted_socket(owner_a, call_a)
                .unwrap()
                .unwrap();
            assert_eq!(a.child.as_ref().unwrap().child, ChildCreationIdV1(1));
            let b=engine.begin_accepted_socket(owner_b,call_b).unwrap().expect("second queued connection must not become EAGAIN while first accepter is stalled");
            assert_eq!(b.child.as_ref().unwrap().child, ChildCreationIdV1(2));
            assert_eq!(
                b.child.as_ref().unwrap().peer,
                NetworkAddressV2::Inet4 {
                    address: [127, 0, 0, 1],
                    port: 54322
                }
            );
            let cap = accepted::AcceptedBackendCapability::controlled_fixture();
            engine.submit_accepted_socket(owner_b, b.lease).unwrap();
            if cancel_first {
                engine.cancel_accepted_socket(owner_a, a.lease).unwrap();
                a = engine
                    .begin_accepted_socket(owner_a, call_a)
                    .unwrap()
                    .unwrap();
                assert_eq!(a.child.as_ref().unwrap().child, ChildCreationIdV1(1));
            }
            engine
                .confirm_accepted_installation(
                    owner_b,
                    b.lease,
                    accepted::AcceptedInstallationFact::Installed {
                        child: ChildCreationIdV1(2),
                        fd: 18,
                        open_file: open_file(303),
                        slot_generation: 1,
                        physical: accepted::AcceptedPhysicalIdentity {
                            provider: 7,
                            object: 12,
                            namespace: 9,
                        },
                    },
                    &cap,
                )
                .unwrap();
            let done = engine
                .complete_accepted_socket(owner_b, b.lease, Ok(18), Some(open_file(303)), time(20))
                .unwrap()
                .unwrap();
            assert_eq!(done.channel, NetworkChannelId(3));
            assert!(
                matches!(engine.channels[&NetworkChannelId(1)].inbound.front(),Some(InboundOutcome::Control(ConnectionOutcome::Accept {accepted,..})) if *accepted==NetworkChannelId(2))
            );
            engine.submit_accepted_socket(owner_a, a.lease).unwrap();
            engine
                .confirm_accepted_installation(
                    owner_a,
                    a.lease,
                    accepted::AcceptedInstallationFact::Installed {
                        child: ChildCreationIdV1(1),
                        fd: 17,
                        open_file: open_file(302),
                        slot_generation: 1,
                        physical: accepted::AcceptedPhysicalIdentity {
                            provider: 7,
                            object: 11,
                            namespace: 9,
                        },
                    },
                    &cap,
                )
                .unwrap();
            let done = engine
                .complete_accepted_socket(owner_a, a.lease, Ok(17), Some(open_file(302)), time(20))
                .unwrap()
                .unwrap();
            assert_eq!(done.channel, NetworkChannelId(2));
            for (owner, call) in [(owner_a, call_a), (owner_b, call_b)] {
                engine.begin_stream_call_release(owner, call).unwrap();
                engine.finish_stream_call_release(owner, call).unwrap();
            }
            engine.finish().unwrap();
        }
    }
    #[test]
    fn accepted_provider_creation_drains_exact_identity_without_issuing_installed() {
        use crate::network_runtime::accepted_creation::ObservedCreation;
        let (mut engine, listener, owner, _) = accepted_record_fixture();
        let state = engine.stream_socket_state(listener).unwrap().unwrap();
        let physical_listener = accepted::AcceptedPhysicalIdentity {
            provider: 7,
            object: 10,
            namespace: 9,
        };
        let physical_child = accepted::AcceptedPhysicalIdentity {
            object: 11,
            ..physical_listener
        };
        let evidence =
            ObservedCreation::controlled_fixture(physical_listener, physical_child, 1, 101, &state);
        let matched = crate::network_runtime::accepted::Resolved {
            physical: physical_child,
            creation: 1,
            cookie: 101,
        };
        assert!(!engine.provider_child_published(matched).unwrap());
        let child = engine
            .confirm_provider_child_creation(&evidence, time(10))
            .unwrap();
        assert_eq!(child.0, 1);
        assert!(engine.provider_child_published(matched).unwrap());
        assert!(
            engine
                .provider_child_published(crate::network_runtime::accepted::Resolved {
                    cookie: 102,
                    ..matched
                })
                .is_err()
        );
        let call = active_pinned_call(&mut engine, listener, owner);
        let operation = engine.begin_accepted_socket(owner, call).unwrap().unwrap();
        engine
            .submit_accepted_socket(owner, operation.lease)
            .unwrap();
        assert!(matches!(
            engine.complete_accepted_socket(
                owner,
                operation.lease,
                Ok(8),
                Some(open_file(101)),
                time(20)
            ),
            Err(NetworkReplayError::UnresolvedAccept(_))
        ));
        assert_eq!(engine.channel_for(open_file(101)), None);
    }
    #[test]
    fn accepted_provider_creation_rejects_late_listener_state_without_partial_publication() {
        use crate::network_runtime::accepted_creation::ObservedCreation;
        let (mut engine, listener, owner, _) = accepted_record_fixture();
        let state = engine.stream_socket_state(listener).unwrap().unwrap();
        let physical_listener = accepted::AcceptedPhysicalIdentity {
            provider: 7,
            object: 10,
            namespace: 9,
        };
        let evidence = ObservedCreation::controlled_fixture(
            physical_listener,
            accepted::AcceptedPhysicalIdentity {
                object: 11,
                ..physical_listener
            },
            1,
            101,
            &state,
        );
        accepted_set(
            &mut engine,
            listener,
            owner,
            NetworkStreamSocketOption::ReceiveLowWater(9),
        );
        let before = format!("{engine:?}");
        assert!(
            engine
                .confirm_provider_child_creation(&evidence, time(10))
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
}

// Proposed exported network_replay types; all gating authority lives in the
// existing run-global engine, never in a deserialized local Mutex.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Physical syscall family which produced a descriptor installation.
pub enum NetworkFdInstallKind {
    /// One fresh socket descriptor.
    Socket,
    /// Both descriptors from one atomic socketpair result.
    SocketPair,
    /// A descriptor returned by dup.
    Dup,
    /// A successful dup2 replacement.
    Dup2,
    /// A successful dup3 replacement.
    Dup3,
    /// A descriptor returned by F_DUPFD or F_DUPFD_CLOEXEC.
    FcntlDup,
    /// A descriptor installed from an authenticated transferred object.
    ScmRights,
    /// A regular descriptor which replaced a network slot.
    RegularReplacement,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Exact confirmed physical effect associated with one local installation.
pub struct NetworkFdEffectAssociation {
    /// Exact task and address-space incarnation which owns this receipt.
    pub owner: NetworkStreamOwner,
    /// Checked run-wide operation identity; never a numeric descriptor identity.
    pub lease: NetworkStreamLeaseId,
    /// Confirmed physical installation family.
    pub kind: NetworkFdInstallKind,
    /// Index within the physical operation's complete returned descriptor set.
    pub result_index: u32,
    /// Actual kernel-returned descriptor number.
    pub returned_fd: i32,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// One ordered slot replacement and its independently checked effect receipt.
pub struct NetworkFdPublicationEntry {
    /// Exact superseded and installed slot incarnations.
    pub replacement: detcore_model::fd::NetworkFdSlotReplacement,
    /// Confirmed physical receipt required to authenticate this replacement.
    pub effect: NetworkFdEffectAssociation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Exact ordered installation prefix for one descriptor table.
pub struct NetworkFdPublicationBatch {
    /// Exact descriptor-table identity.
    pub files: detcore_model::fd::FilesId,
    /// Next ordered publication sequence for this table.
    pub sequence: u64,
    /// Globally acknowledged generation preceding this prefix.
    pub previous_generation: u64,
    /// Generation high-water after the entire prefix, including regular-only gaps.
    pub through_generation: u64,
    /// Ordered network-related replacements; multi-result effects remain atomic.
    pub entries: Vec<NetworkFdPublicationEntry>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Exclusive logical publication authority allocated by the global engine.
pub struct NetworkFdPublicationPermit {
    /// Exact descriptor-table identity.
    pub files: detcore_model::fd::FilesId,
    /// Exact task and address-space incarnation which owns this receipt.
    pub owner: NetworkStreamOwner,
    /// Checked run-wide operation identity; never a numeric descriptor identity.
    pub lease: NetworkStreamLeaseId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Global permit and recovery state returned after authenticated admission.
pub struct NetworkFdPublicationAdmission {
    /// Current exact global publication permit.
    pub permit: NetworkFdPublicationPermit,
    // Current run-global ledger cursor, never inferred from local gate creation.
    /// Last publication sequence applied by the global ledger.
    pub acknowledged_sequence: u64,
    /// Last descriptor generation applied by the global ledger.
    pub acknowledged_generation: u64,
    // Durable submitted prefix from a canceled/retired publisher. It is retained
    // until local ACK reaches the server, including after global application.
    /// Exact durable prefix awaiting local/server acknowledgement after interruption.
    pub recovery: Option<NetworkFdPublicationBatch>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Authenticated descriptor-prefix publication protocol carried over RPC.
pub enum NetworkFdPublicationRequest {
    /// Acquire the table's sole global publication permit.
    Acquire {
        /// Exact descriptor-table identity.
        files: detcore_model::fd::FilesId,
    },
    /// Apply the exact prefix and retire its completed owners before replying.
    Publish {
        /// Current exact global publication permit.
        permit: NetworkFdPublicationPermit,
        /// Exact prefix; equality includes all physical receipt associations.
        batch: NetworkFdPublicationBatch,
    },
    /// Acknowledge local application and prune completed full receipt payloads.
    Acknowledge {
        /// Current exact global publication permit.
        permit: NetworkFdPublicationPermit,
        /// Exact prefix; equality includes all physical receipt associations.
        batch: NetworkFdPublicationBatch,
    },
    /// Release a permit only when no recovery prefix remains.
    ReleaseEmpty {
        /// Current exact global publication permit.
        permit: NetworkFdPublicationPermit,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Exact publication response from the global descriptor-table authority.
pub enum NetworkFdPublicationReply {
    /// Permit plus authenticated current cursor and optional recovery prefix.
    Admitted(NetworkFdPublicationAdmission),
    /// Exact globally applied prefix; not a guest syscall success marker.
    Published(NetworkFdPublicationBatch),
    /// Permit released after the required acknowledgement.
    Released,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
/// Serialized local prefix state; contains no independently constructed lock.
pub struct NetworkFdLocalPublication {
    /// Last publication sequence applied by the global ledger.
    pub acknowledged_sequence: u64,
    /// Last descriptor generation applied by the global ledger.
    pub acknowledged_generation: u64,
    /// Prefix recorded before the local caller awaits its publication response.
    pub in_flight: Option<NetworkFdPublicationBatch>,
    /// Most recent exact prefix committed to local metadata.
    pub last_acknowledged: Option<NetworkFdPublicationBatch>,
    /// Locally committed prefix whose server ACK response is still outstanding.
    pub awaiting_global_ack: Option<NetworkFdPublicationBatch>,
    /// Installation generation to confirmed physical receipt association.
    pub effects: std::collections::BTreeMap<u64, NetworkFdEffectAssociation>,
}

// Proposed fields on NetworkReplayEngine:
// fd_publications: HashMap<FilesId, FdPublicationState>,
// fd_installations: BTreeMap<NetworkStreamLeaseId, ConfirmedFdInstallation>,
// consumed_fd_installations: BTreeMap<(NetworkStreamLeaseId,u32),(FilesId,u64)>,
// fd_publication_history: BTreeMap<(FilesId,u64),NetworkFdPublicationBatch>,
// Existing NetworkLifetime is the sole table/slot authority.

#[derive(Debug, Clone, Default)]
struct FdPublicationState {
    active: Option<NetworkFdPublicationPermit>,
    pending: Option<NetworkFdPublicationBatch>,
}
#[derive(Debug, Clone)]
struct ConfirmedFdInstallation {
    // Constructed ONLY by matching the persistent submitted physical receipt.
    // An installation journal/RPC payload cannot manufacture this record.
    owner: NetworkStreamOwner,
    files: FilesId,
    kind: NetworkFdInstallKind,
    returned_fds: Vec<i32>,
    installations: Vec<detcore_model::fd::NetworkFdSlotReplacement>,
    open_files: Vec<Option<OpenFileId>>,
    sources: Vec<SlotInstallationSource>,
}

impl NetworkReplayEngine {
    fn publication_owner(
        &self,
        owner: NetworkStreamOwner,
        files: FilesId,
    ) -> Result<TaskOwner, NetworkReplayError> {
        self.check_stream_owner(owner)?;
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        if self
            .lifetime
            .task_files(task)
            .map_err(|e| NetworkReplayError::FdPublicationProtocol(e.to_string()))?
            != files
        {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "sender does not own publication table".into(),
            ));
        }
        Ok(task)
    }
    fn validate_publication_permit(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<TaskOwner, NetworkReplayError> {
        let task = self.publication_owner(owner, permit.files)?;
        if permit.owner != owner
            || self
                .fd_publications
                .get(&permit.files)
                .and_then(|state| state.active)
                != Some(permit)
        {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "stale publication permit".into(),
            ));
        }
        Ok(task)
    }
    /// Acquire the existing global table authority without reconstructing a local gate.
    pub fn acquire_fd_publication(
        &mut self,
        owner: NetworkStreamOwner,
        files: FilesId,
    ) -> Result<NetworkFdPublicationAdmission, NetworkReplayError> {
        let task = self.publication_owner(owner, files)?;
        if let Some(active) = self
            .fd_publications
            .get(&files)
            .and_then(|state| state.active)
        {
            // recv_stream_operation's existing enabled-Notify wait handles this
            // only for NoSeq contention, and revalidates MM/FilesId after wake.
            return Err(NetworkReplayError::StreamOperationBusy(active.lease));
        }
        let lease = NetworkStreamLeaseId(self.next_stream_lease);
        self.next_stream_lease = self
            .next_stream_lease
            .checked_add(1)
            .ok_or(NetworkReplayError::Overflow)?;
        let permit = NetworkFdPublicationPermit {
            files,
            owner,
            lease,
        };
        let (acknowledged_sequence, acknowledged_generation) = self
            .lifetime
            .publication_cursor(task)
            .map_err(|e| NetworkReplayError::FdPublicationProtocol(e.to_string()))?;
        let state = self.fd_publications.entry(files).or_default();
        state.active = Some(permit);
        Ok(NetworkFdPublicationAdmission {
            permit,
            acknowledged_sequence,
            acknowledged_generation,
            recovery: state.pending.clone(),
        })
    }
    /// Validate and atomically apply an exact prefix before exposing its receipt.
    pub fn publish_fd_publication(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        batch: &NetworkFdPublicationBatch,
    ) -> Result<NetworkFdPublicationBatch, NetworkReplayError> {
        let task = self.validate_publication_permit(owner, permit)?;
        if batch.files != permit.files {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "batch table differs from permit".into(),
            ));
        }
        let pending = &self.fd_publications[&permit.files].pending;
        if pending.as_ref().is_some_and(|previous| previous != batch) {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "pending prefix changed".into(),
            ));
        }
        let (current_sequence, current_generation) = self
            .lifetime
            .publication_cursor(task)
            .map_err(|e| NetworkReplayError::FdPublicationProtocol(e.to_string()))?;
        if batch.sequence <= current_sequence {
            // A new permit does not resurrect old historical authority. Only
            // the currently pending exact recovery batch can replay a receipt.
            if batch.sequence != current_sequence
                || batch.through_generation != current_generation
                || pending.as_ref() != Some(batch)
                || self
                    .fd_publication_history
                    .get(&(batch.files, batch.sequence))
                    != Some(batch)
            {
                return Err(NetworkReplayError::FdPublicationProtocol(
                    "stale or changed acknowledged prefix".into(),
                ));
            }
            return Ok(batch.clone());
        }
        let mut entries = Vec::with_capacity(batch.entries.len());
        let mut used = BTreeSet::new();
        for entry in &batch.entries {
            let association = entry.effect;
            let physical = self
                .fd_installations
                .get(&association.lease)
                .ok_or_else(|| {
                    NetworkReplayError::FdPublicationProtocol(
                        "installation has no matched confirmed physical receipt".into(),
                    )
                })?;
            let index = association.result_index as usize;
            if physical.owner != association.owner
                || physical.files != batch.files
                || physical.kind != association.kind
                || physical.returned_fds.get(index) != Some(&association.returned_fd)
                || physical.installations.get(index) != Some(&entry.replacement)
                || !used.insert((association.lease, association.result_index))
            {
                return Err(NetworkReplayError::FdPublicationProtocol(
                    "physical installation receipt mismatch/reuse".into(),
                ));
            }
            let represented = entry
                .replacement
                .after
                .or(entry.replacement.before)
                .ok_or_else(|| {
                    NetworkReplayError::FdPublicationProtocol("empty installation".into())
                })?;
            if represented.binding.slot.fd != association.returned_fd
                || physical.open_files.get(index)
                    != Some(&entry.replacement.after.map(|slot| slot.binding.open_file))
            {
                return Err(NetworkReplayError::FdPublicationProtocol(
                    "physical result does not identify journal installation".into(),
                ));
            }
            let source = physical
                .sources
                .get(index)
                .ok_or_else(|| {
                    NetworkReplayError::FdPublicationProtocol("missing confirmed provenance".into())
                })?
                .clone();
            entries.push(SlotPublicationEntry {
                replacement: entry.replacement,
                source,
            });
        }
        // Socketpair and other multi-result effects are one physical publication
        // unit. A caller cannot publish only the convenient returned descriptor.
        for &(lease, _) in &used {
            let physical = &self.fd_installations[&lease];
            if physical.returned_fds.len() != physical.installations.len()
                || physical.returned_fds.len() != physical.open_files.len()
                || physical.returned_fds.len() != physical.sources.len()
                || (0..physical.returned_fds.len()).any(|index| {
                    u32::try_from(index).map_or(true, |index| !used.contains(&(lease, index)))
                })
            {
                return Err(NetworkReplayError::FdPublicationProtocol(
                    "partial multi-result installation receipt".into(),
                ));
            }
        }
        let derived = SlotPublicationBatch {
            files: batch.files,
            sequence: batch.sequence,
            previous_generation: batch.previous_generation,
            through_generation: batch.through_generation,
            entries,
        };
        let mut next_lifetime = self.lifetime.clone();
        let result = next_lifetime
            .publish_installation_batch(task, &derived)
            .map_err(|e| NetworkReplayError::FdPublicationProtocol(e.to_string()))?;
        let retired = match result {
            SlotPublicationResult::Applied { retired } => retired,
            SlotPublicationResult::AlreadyApplied => {
                return Err(NetworkReplayError::FdPublicationProtocol(
                    "ledger receipt lacks matching wire/effect receipt".into(),
                ));
            }
        };
        // All operation pins must also be enrolled in the sole lifetime ledger.
        // An inconsistency is not permission to drop a live call's channel.
        if retired.iter().any(|ofd| self.has_stream_references(*ofd)) {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "retirement still has engine operation references".into(),
            ));
        }
        self.validate_fd_mutation_publication(owner, permit)?;
        // This entire method runs under the existing engine mutex with no await.
        // Retirement is applied BEFORE the RPC can emit a reply or be canceled.
        self.lifetime = next_lifetime;
        self.retire_lifetime_open_files(retired);
        // Full physical confirmations are needed only until atomic publication.
        // Removing them fences reuse: the global checked allocator never issues
        // their lease IDs again, and an absent confirmation cannot authenticate
        // a later installation. Pending duplicate replies use the exact batch.
        for lease in used
            .into_iter()
            .map(|(lease, _)| lease)
            .collect::<BTreeSet<_>>()
        {
            assert!(self.fd_installations.remove(&lease).is_some());
        }
        self.fd_publication_history
            .insert((batch.files, batch.sequence), batch.clone());
        self.fd_publications.get_mut(&permit.files).unwrap().pending = Some(batch.clone());
        // The real installation and exact recovery prefix are now durable.
        // Release its short controls before replying; local ACK loss cannot
        // strand a dead owner's controls or require repeating a kernel effect.
        self.complete_fd_mutation_publication(owner, permit)
            .expect("publication controls were prevalidated in the same engine transaction");
        Ok(batch.clone())
    }
    /// Acknowledge the current exact prefix and prune its completed full payloads.
    pub fn acknowledge_fd_publication(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        batch: &NetworkFdPublicationBatch,
    ) -> Result<(), NetworkReplayError> {
        self.validate_publication_permit(owner, permit)?;
        if self.fd_publications[&permit.files].pending.as_ref() != Some(batch)
            || self
                .fd_publication_history
                .get(&(batch.files, batch.sequence))
                != Some(batch)
        {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "uncommitted or mismatched local ACK".into(),
            ));
        }
        let task = self.publication_owner(owner, permit.files)?;
        self.lifetime
            .acknowledge_publication_batch(task, batch.sequence, batch.through_generation)
            .map_err(|e| NetworkReplayError::FdPublicationProtocol(e.to_string()))?;
        self.fd_publication_history
            .remove(&(batch.files, batch.sequence));
        let state = self.fd_publications.get_mut(&permit.files).unwrap();
        state.pending = None;
        state.active = None;
        Ok(())
    }
    /// Release only a permit which has no pending recovery prefix.
    pub fn release_empty_fd_publication(
        &mut self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> Result<(), NetworkReplayError> {
        self.validate_publication_permit(owner, permit)?;
        let state = self.fd_publications.get_mut(&permit.files).unwrap();
        if state.pending.is_some() {
            return Err(NetworkReplayError::FdPublicationProtocol(
                "release would erase a pending prefix".into(),
            ));
        }
        state.active = None;
        Ok(())
    }
    fn fd_publication_owner_gone(&mut self, owner: NetworkStreamOwner) {
        let unresolved: BTreeSet<_> = self
            .fd_publications
            .values()
            .filter_map(|state| {
                state
                    .active
                    .filter(|permit| self.physical_fd_mutation_pending(*permit))
                    .map(|p| p.lease)
            })
            .collect();
        for state in self.fd_publications.values_mut() {
            if state
                .active
                .is_some_and(|permit| permit.owner == owner && !unresolved.contains(&permit.lease))
            {
                state.active = None;
                // Preserve pending and history: successor gets exact recovery,
                // including an applied prefix whose reply/local ACK was lost.
            }
        }
    }
}

// These helpers install explicit trusted-effect fixtures. They are not physical
// execution evidence and cannot be called by a production adapter.
#[cfg(test)]
impl NetworkReplayEngine {
    pub(crate) fn fd_publication_fixture_register(
        &mut self,
        owner: NetworkStreamOwner,
        share: Option<NetworkStreamOwner>,
    ) -> FilesId {
        let task = TaskOwner {
            tid: owner.thread,
            mm: owner.mm,
        };
        if let Some(parent) = share {
            self.lifetime
                .share_table(
                    TaskOwner {
                        tid: parent.thread,
                        mm: parent.mm,
                    },
                    task,
                    parent.thread,
                )
                .unwrap();
        } else {
            self.lifetime
                .register(task, owner.thread, FilesId::initial(owner.thread))
                .unwrap();
        }
        self.lifetime.task_files(task).unwrap()
    }
    pub(crate) fn fd_publication_fixture_effect(
        &mut self,
        owner: NetworkStreamOwner,
        replacement: detcore_model::fd::NetworkFdSlotReplacement,
    ) -> NetworkFdEffectAssociation {
        let lease = self.allocate_stream_lease().unwrap();
        let fd = replacement
            .after
            .or(replacement.before)
            .unwrap()
            .binding
            .slot
            .fd;
        let kind = if replacement.after.is_some() {
            NetworkFdInstallKind::Socket
        } else {
            NetworkFdInstallKind::RegularReplacement
        };
        self.fd_installations.insert(
            lease,
            ConfirmedFdInstallation {
                owner,
                files: replacement.files,
                kind,
                returned_fds: vec![fd],
                installations: vec![replacement],
                open_files: vec![replacement.after.map(|slot| slot.binding.open_file)],
                sources: vec![if replacement.after.is_some() {
                    SlotInstallationSource::Fresh
                } else {
                    SlotInstallationSource::NonNetwork
                }],
            },
        );
        NetworkFdEffectAssociation {
            owner,
            lease,
            kind,
            result_index: 0,
            returned_fd: fd,
        }
    }
    pub(crate) fn fd_publication_fixture_cursor(&self, owner: NetworkStreamOwner) -> (u64, u64) {
        self.lifetime
            .publication_cursor(TaskOwner {
                tid: owner.thread,
                mm: owner.mm,
            })
            .unwrap()
    }
    pub(crate) fn fd_publication_fixture_is_retired(&self, ofd: OpenFileId) -> bool {
        self.retired_open_files.contains(&ofd)
    }
}

#[cfg(test)]
mod fd_publication_authority_tests {
    use chrono::TimeZone;
    use detcore_model::fd::FdSlot;
    use detcore_model::fd::FdSlotBinding;
    use detcore_model::fd::NetworkFdSlot;
    use detcore_model::fd::NetworkFdSlotReplacement;

    use super::*;
    use crate::types::DetTid;
    use crate::types::MmId;

    fn setup() -> (NetworkReplayEngine, NetworkStreamOwner) {
        let owner = NetworkStreamOwner {
            thread: DetTid::from_raw(61),
            mm: MmId::initial(DetTid::from_raw(61)),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        engine.fd_publication_fixture_register(owner, None);
        (engine, owner)
    }
    fn installation(
        owner: NetworkStreamOwner,
        generation: u64,
        fd: i32,
        before: Option<NetworkFdSlot>,
        network: bool,
    ) -> NetworkFdSlotReplacement {
        let files = FilesId::initial(owner.thread);
        NetworkFdSlotReplacement {
            files,
            installation_generation: generation,
            before,
            after: network.then_some(NetworkFdSlot {
                binding: FdSlotBinding {
                    slot: FdSlot { files, fd },
                    generation,
                    open_file: OpenFileId::new_socket(owner.thread, generation),
                },
                cloexec: false,
            }),
        }
    }
    fn batch(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        change: NetworkFdSlotReplacement,
    ) -> NetworkFdPublicationBatch {
        let (sequence, previous_generation) = engine.fd_publication_fixture_cursor(owner);
        let effect = engine.fd_publication_fixture_effect(owner, change);
        NetworkFdPublicationBatch {
            files: change.files,
            sequence: sequence + 1,
            previous_generation,
            through_generation: change.installation_generation,
            entries: vec![NetworkFdPublicationEntry {
                replacement: change,
                effect,
            }],
        }
    }
    fn publish_and_ack(
        engine: &mut NetworkReplayEngine,
        owner: NetworkStreamOwner,
        batch: &NetworkFdPublicationBatch,
    ) {
        let permit = engine
            .acquire_fd_publication(owner, batch.files)
            .unwrap()
            .permit;
        assert_eq!(
            engine.publish_fd_publication(owner, permit, batch).unwrap(),
            *batch
        );
        engine
            .acknowledge_fd_publication(owner, permit, batch)
            .unwrap();
    }
    #[test]
    fn stale_exact_batch_cannot_be_reaccepted_under_fresh_current_permit() {
        let (mut engine, owner) = setup();
        let first = batch(&mut engine, owner, installation(owner, 1, 7, None, true));
        publish_and_ack(&mut engine, owner, &first);
        let second = batch(&mut engine, owner, installation(owner, 2, 8, None, true));
        publish_and_ack(&mut engine, owner, &second);
        let admission = engine.acquire_fd_publication(owner, first.files).unwrap();
        assert_eq!(
            (
                admission.acknowledged_sequence,
                admission.acknowledged_generation
            ),
            (2, 2)
        );
        assert_eq!(admission.recovery, None);
        for stale in [&first, &second] {
            assert!(matches!(
                engine.publish_fd_publication(owner, admission.permit, stale),
                Err(NetworkReplayError::FdPublicationProtocol(_))
            ));
            assert_eq!(engine.fd_publication_fixture_cursor(owner), (2, 2));
            assert!(engine.fd_publications[&first.files].pending.is_none());
        }
        engine
            .release_empty_fd_publication(owner, admission.permit)
            .unwrap();
    }
    #[test]
    fn completed_receipt_payloads_prune_while_unpublished_confirmation_remains() {
        let (mut engine, owner) = setup();
        // Explicit unresolved publication input: completing other receipts must
        // not remove it merely to satisfy a bounded-population assertion.
        let outstanding =
            engine.fd_publication_fixture_effect(owner, installation(owner, 1000, 99, None, true));
        let mut before = None;
        for generation in 1..=64 {
            let network = generation % 2 == 1;
            let change = installation(owner, generation, 7, before, network);
            let next = change.after;
            let value = batch(&mut engine, owner, change);
            let permit = engine
                .acquire_fd_publication(owner, value.files)
                .unwrap()
                .permit;
            engine
                .publish_fd_publication(owner, permit, &value)
                .unwrap();
            assert_eq!(engine.fd_publication_history.len(), 1);
            assert_eq!(engine.lifetime.pending_publication_payloads_for_test(), 1);
            assert_eq!(engine.fd_installations.len(), 1);
            assert!(engine.fd_installations.contains_key(&outstanding.lease));
            if !network {
                assert!(
                    engine.fd_publication_fixture_is_retired(before.unwrap().binding.open_file)
                );
            }
            // Duplicate lost-reply recovery remains exact before ACK, even
            // after the consumed full physical receipt has been pruned.
            assert_eq!(
                engine
                    .publish_fd_publication(owner, permit, &value)
                    .unwrap(),
                value
            );
            engine
                .acknowledge_fd_publication(owner, permit, &value)
                .unwrap();
            assert!(engine.fd_publication_history.is_empty());
            assert_eq!(engine.lifetime.pending_publication_payloads_for_test(), 0);
            assert_eq!(engine.fd_installations.len(), 1);
            before = next;
        }
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (64, 64));
        assert_eq!(engine.fd_publications.len(), 1);
    }
    #[test]
    fn partial_multiresult_publication_rejects_without_consuming_confirmation() {
        let (mut engine, owner) = setup();
        let a = installation(owner, 1, 7, None, true);
        let b = installation(owner, 2, 8, None, true);
        let lease = engine.allocate_stream_lease().unwrap();
        engine.fd_installations.insert(
            lease,
            ConfirmedFdInstallation {
                owner,
                files: a.files,
                kind: NetworkFdInstallKind::SocketPair,
                returned_fds: vec![7, 8],
                installations: vec![a, b],
                open_files: vec![
                    a.after.map(|slot| slot.binding.open_file),
                    b.after.map(|slot| slot.binding.open_file),
                ],
                sources: vec![SlotInstallationSource::Fresh, SlotInstallationSource::Fresh],
            },
        );
        let entry = |change: NetworkFdSlotReplacement, result_index, returned_fd| {
            NetworkFdPublicationEntry {
                replacement: change,
                effect: NetworkFdEffectAssociation {
                    owner,
                    lease,
                    kind: NetworkFdInstallKind::SocketPair,
                    result_index,
                    returned_fd,
                },
            }
        };
        let mut value = NetworkFdPublicationBatch {
            files: a.files,
            sequence: 1,
            previous_generation: 0,
            through_generation: 2,
            entries: vec![entry(a, 0, 7)],
        };
        let permit = engine
            .acquire_fd_publication(owner, a.files)
            .unwrap()
            .permit;
        assert!(matches!(
            engine.publish_fd_publication(owner, permit, &value),
            Err(NetworkReplayError::FdPublicationProtocol(_))
        ));
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (0, 0));
        assert_eq!(engine.fd_installations.len(), 1);
        value.entries.push(entry(b, 1, 8));
        engine
            .publish_fd_publication(owner, permit, &value)
            .unwrap();
        assert_eq!(engine.fd_publication_fixture_cursor(owner), (1, 2));
        assert!(engine.fd_installations.is_empty());
        engine
            .acknowledge_fd_publication(owner, permit, &value)
            .unwrap();
    }
}
