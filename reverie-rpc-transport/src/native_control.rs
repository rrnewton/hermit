/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Opt-in, single-request native control while a Tool RPC remains borrowed.
//!
//! This transport grants no scheduler or signal-publication capability. A
//! native refusal is a literal result. An accepted Release/Cancel acknowledges
//! phase 3 (intent); the ordinary reply then permits callback return and the
//! existing dispatcher, not this actor, retires the native guard.

use std::collections::BTreeMap;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use reverie::GlobalTool;
use reverie::Tid;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::RequestEnvelope;
use crate::RpcError;
use crate::codec::DEFAULT_MAX_FRAME_LEN;
use crate::codec::encode;
use crate::codec::read_message;
use crate::codec::write_message;

pub const ENV: &str = "HERMIT_DBT_RPC_CONTROL_V1";
const MAGIC: u64 = 0x5245564354524c31;
const VERSION: u32 = 2;

pub fn protocol_error(message: &str) -> RpcError {
    std::io::Error::other(message).into()
}

/// Control frames must consume their entire bounded payload. Legacy decoding
/// remains unchanged for connections that did not opt in.
pub fn decode_exact<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, RpcError> {
    let (value, consumed) = bincode::serde::decode_from_slice(bytes, bincode::config::legacy())?;
    if consumed != bytes.len() {
        return Err(protocol_error("native control frame has trailing bytes"));
    }
    Ok(value)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    magic: u64,
    version: u32,
    pub connection: u64,
    pub config: Vec<u8>,
}

impl Hello {
    fn new(connection: u64, config: Vec<u8>) -> Self {
        Self {
            magic: MAGIC,
            version: VERSION,
            connection,
            config,
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RpcError> {
        let value: Self = decode_exact(bytes)?;
        if value.magic != MAGIC || value.version != VERSION || value.connection == 0 {
            return Err(protocol_error("unsupported native control handshake"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct NativeOwner {
    pub pid: i32,
    pub tid: i32,
    pub image_epoch: u64,
    pub callback_epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct CaptureHandle {
    pub owner_epoch: u64,
    pub token: u64,
    pub owner_pid: i32,
    pub owner_tid: i32,
}

/// Exact value snapshot of the SDK receipt, never a pending-allocation pointer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct CaptureReceipt {
    pub handle: CaptureHandle,
    pub version: u64,
    pub raw_entries: u64,
    pub linked: u64,
    pub coalesced: u64,
    pub guest_mask: u64,
    pub saved_physical_mask: u64,
    pub pending_records: u64,
    pub siginfo_words: [u64; 16],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct PublicationOwner {
    pub image_epoch: u64,
    pub callback_epoch: u64,
    pub admission_token: u64,
    pub callback_kind: u32,
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct SignalDomainRecord {
    pub generation: u64,
    pub ownership: u32,
    pub reserved: u32,
    pub siginfo_words: [u64; 16],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct SignalDomainSnapshot {
    pub owner: PublicationOwner,
    pub domain: u64,
    pub version: u64,
    pub last_retired: u64,
    pub retirements: u64,
    pub guest_mask: u64,
    pub interval: u64,
    pub admission_token: u64,
    pub owner_pid: i32,
    pub owner_tid: i32,
    pub phase: u32,
    pub queued: u32,
    pub active: u32,
    pub returning: u32,
    pub record_count: u32,
    pub reserved: u32,
    pub records: [SignalDomainRecord; 4],
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct PublicationRequest {
    pub owner: PublicationOwner,
    pub domain: u64,
    pub reservation: u64,
    pub child_generation: u64,
    pub child_pid: i32,
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct PublicationHandle {
    pub request: PublicationRequest,
    pub interval: u64,
    pub owner_pid: i32,
    pub owner_tid: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct PublicationReceipt {
    pub handle: PublicationHandle,
    pub version: u64,
    pub raw_entries: u64,
    pub outcome: u32,
    pub reserved: u32,
    pub allocation: u64,
    pub victim: u64,
    pub guest_mask: u64,
    pub saved_physical_mask: u64,
    pub incoming_siginfo_words: [u64; 16],
    pub victim_siginfo_words: [u64; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum NativeControlOperation {
    Begin {
        owner_epoch: u64,
    },
    Close {
        handle: CaptureHandle,
    },
    Release {
        receipt: CaptureReceipt,
    },
    Cancel {
        handle: CaptureHandle,
    },
    DomainObserve,
    PublicationArm {
        request: PublicationRequest,
    },
    /// Fixed actual-parent non-reaping waitid followed by SDK capture Close.
    PublicationClose {
        handle: PublicationHandle,
    },
    PublicationRelease {
        receipt: PublicationReceipt,
    },
    PublicationCancel {
        handle: PublicationHandle,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[repr(C)]
pub struct NativeControlReply {
    /// SDK status: OK=0, UNAVAILABLE=1, STALE=2, NO_RECORD=3,
    /// KERNEL_BLOCKED=4, ERROR=5, structural UNSUPPORTED=6.
    pub status: i32,
    /// Native control intent phase; 3 is not dispatcher completion.
    pub phase: u32,
    pub owner: NativeOwner,
    pub handle: CaptureHandle,
    pub receipt: CaptureReceipt,
    pub saved_physical_mask: u64,
    pub current_physical_mask: u64,
    /// A physical-mask observation failed after the SDK operation. This never
    /// replaces the operation status or erases an acquired/consumed handle.
    pub observation_error: i32,
    pub reserved: u32,
    pub domain: SignalDomainSnapshot,
    pub publication: PublicationReceipt,
    /// Literal fixed waitid result. Its siginfo is meaningful only on zero.
    pub publication_wait_result: i64,
    pub publication_wait_siginfo_words: [u64; 16],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallIdentity {
    pub connection: u64,
    pub request: u64,
    pub owner: NativeOwner,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientFrame {
    Request {
        call: CallIdentity,
        payload: Vec<u8>,
    },
    ControlReply {
        call: CallIdentity,
        operation: u64,
        reply: NativeControlReply,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerFrame {
    OrdinaryReply {
        call: CallIdentity,
        payload: Vec<u8>,
    },
    Control {
        call: CallIdentity,
        operation: u64,
        command: NativeControlOperation,
    },
}

struct Command {
    operation: NativeControlOperation,
    result: Option<oneshot::Sender<Result<NativeControlReply, String>>>,
}

struct PendingState {
    open: bool,
    sender: mpsc::Sender<Command>,
    reply_reservation: Option<u64>,
    reservation_aborted: bool,
    lease_active: bool,
}

/// A handle to exactly one authenticated connection/request, not a PID mailbox.
#[derive(Clone)]
pub struct PendingNativeCall {
    pub identity: CallIdentity,
    state: Arc<Mutex<PendingState>>,
    changed: Arc<Notify>,
    peer: Option<NativePeerIdentity>,
}

/// Kernel connect-time identity retained independently of a numeric PID.
#[derive(Clone, Debug)]
pub struct NativePeerIdentity {
    pub connection: u64,
    pub pid: i32,
    pub pidfd: Arc<OwnedFd>,
}

impl PendingNativeCall {
    pub fn peer_identity(&self) -> Option<NativePeerIdentity> {
        self.peer.clone()
    }

    /// Finishes when this exact request is retired, including actor failure.
    pub async fn closed(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.state.lock().unwrap().open {
                return;
            }
            changed.await;
        }
    }

    /// Reserve this exact ordinary reply before an irreversible publication.
    /// This linearizes against `PendingGuard::finish_if_empty`, not against a
    /// later observation of whether the Tool future happens to be ready.
    pub fn reserve_reply(&self, sequence: u64) -> Result<NativeReplyReservation, String> {
        let mut state = self.state.lock().unwrap();
        if sequence == 0
            || !state.open
            || state.reservation_aborted
            || state.reply_reservation.is_some()
        {
            return Err("native reply cannot be reserved in its current state".into());
        }
        state.reply_reservation = Some(sequence);
        Ok(NativeReplyReservation {
            call: self.clone(),
            sequence,
            finished: false,
        })
    }

    /// Bounded outbox; a full/closed outbox refuses without sending an operation.
    /// Cancellation after enqueue is indeterminate and must never be replayed.
    pub async fn command(
        &self,
        operation: NativeControlOperation,
    ) -> Result<NativeControlReply, String> {
        let (result, receive) = oneshot::channel();
        {
            let state = self.state.lock().unwrap();
            if !state.open {
                return Err("native call is no longer pending".into());
            }
            state
                .sender
                .try_send(Command {
                    operation,
                    result: Some(result),
                })
                .map_err(|_| "native control outbox is full or closed".to_string())?;
        }
        receive.await.map_err(|_| {
            "native operation lost its connection; outcome indeterminate".to_string()
        })?
    }
}

/// Consuming ownership of a pending ordinary reply. Dropping this after an
/// irreversible operation aborts the actor; it never authorizes an ordinary
/// reply or describes an automatic native Cancel as successful publication.
pub struct NativeReplyReservation {
    call: PendingNativeCall,
    sequence: u64,
    finished: bool,
}

impl NativeReplyReservation {
    pub fn call(&self) -> &PendingNativeCall {
        &self.call
    }

    /// The caller must already have consumed its publication outcome and a
    /// native Release/Cancel ACK. The actor independently refuses to expose
    /// an ordinary reply while its native handle is still active.
    pub fn finish(mut self) -> Result<(), String> {
        {
            let mut state = self.call.state.lock().unwrap();
            if !state.open
                || state.reservation_aborted
                || state.lease_active
                || state.reply_reservation != Some(self.sequence)
            {
                return Err("native reply reservation changed before completion".into());
            }
            state.reply_reservation = None;
        }
        self.finished = true;
        self.call.changed.notify_one();
        Ok(())
    }
}

impl Drop for NativeReplyReservation {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.call.state.lock().unwrap();
            if state.open && state.reply_reservation == Some(self.sequence) {
                state.reservation_aborted = true;
            }
            drop(state);
            self.call.changed.notify_one();
        }
    }
}

/// Outbox registry for a supervised native-control endpoint. The owner must
/// bound and cancel its serving task if a controller abandons a still-pending
/// Tool future or its peer dies. Dropping that actor closes the socket, allowing
/// a live client to Cancel locally. This hub does not monitor process death or
/// claim autonomous cleanup of an abandoned Tool future.
#[derive(Clone, Default)]
pub struct NativeControlHub {
    calls: Arc<Mutex<BTreeMap<i32, PendingNativeCall>>>,
    next_connection: Arc<AtomicU64>,
}

impl NativeControlHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn pending(&self, from: Tid) -> Option<PendingNativeCall> {
        self.calls.lock().unwrap().get(&from.as_raw()).cloned()
    }

    pub(crate) fn connection(&self) -> Result<u64, RpcError> {
        self.next_connection
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(1)
            })
            .map(|old| old + 1)
            .map_err(|_| protocol_error("native connection generation exhausted"))
    }

    fn register(
        &self,
        from: Tid,
        identity: CallIdentity,
        peer: Option<NativePeerIdentity>,
    ) -> Result<(PendingGuard, mpsc::Receiver<Command>), RpcError> {
        let (sender, receiver) = mpsc::channel(4);
        let pending = PendingNativeCall {
            identity,
            state: Arc::new(Mutex::new(PendingState {
                open: true,
                sender,
                reply_reservation: None,
                reservation_aborted: false,
                lease_active: false,
            })),
            changed: Arc::new(Notify::new()),
            peer,
        };
        let mut calls = self.calls.lock().unwrap();
        if calls.contains_key(&from.as_raw()) {
            return Err(protocol_error(
                "multiple pending native calls from one identity",
            ));
        }
        calls.insert(from.as_raw(), pending.clone());
        Ok((
            PendingGuard {
                hub: self.clone(),
                from,
                pending,
            },
            receiver,
        ))
    }
}

struct PendingGuard {
    hub: NativeControlHub,
    from: Tid,
    pending: PendingNativeCall,
}

enum ReplyReadiness {
    Finished,
    Reserved,
    Command(Command),
    Cancel(NativeControlOperation),
}

impl PendingGuard {
    /// Close atomically against command enqueue. A queued command wins over an
    /// ordinary reply; otherwise no later command can target this request.
    fn finish_if_empty(
        &self,
        receiver: &mut mpsc::Receiver<Command>,
        cancel: Option<NativeControlOperation>,
    ) -> Result<ReplyReadiness, RpcError> {
        let mut state = self.pending.state.lock().unwrap();
        if state.reservation_aborted {
            return Err(protocol_error(
                "native publication reply reservation was aborted",
            ));
        }
        match receiver.try_recv() {
            Ok(command) => Ok(ReplyReadiness::Command(command)),
            Err(_) if state.reply_reservation.is_some() => Ok(ReplyReadiness::Reserved),
            Err(_) => {
                state.open = false;
                Ok(match cancel {
                    Some(operation) => ReplyReadiness::Cancel(operation),
                    None => ReplyReadiness::Finished,
                })
            }
        }
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.state.lock().unwrap().open = false;
        self.hub.calls.lock().unwrap().remove(&self.from.as_raw());
        self.pending.changed.notify_waiters();
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ActiveLease {
    Legacy(CaptureHandle),
    Publication(PublicationHandle),
}

impl ActiveLease {
    fn cancel(self) -> NativeControlOperation {
        match self {
            Self::Legacy(handle) => NativeControlOperation::Cancel { handle },
            Self::Publication(handle) => NativeControlOperation::PublicationCancel { handle },
        }
    }
}

/// Separate opt-in actor. It owns the only writer and never cancels an
/// in-progress frame read/write. Only the Tool future and cancellation-safe
/// outbox receive participate in select; partial codec state cannot be lost.
pub(crate) async fn serve<G, S>(
    global: Arc<G>,
    config: G::Config,
    mut stream: S,
    hub: NativeControlHub,
    connection: u64,
    peer_pid: i32,
    peer: Option<NativePeerIdentity>,
    readiness: Option<Arc<AtomicBool>>,
    connection_readiness: Option<Arc<AtomicBool>>,
    mut admitted: Option<i32>,
) -> Result<(), RpcError>
where
    G: GlobalTool,
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let hello = encode(&Hello::new(connection, encode(&config)?))?;
    write_message(&mut stream, &hello).await?;
    if let Some(readiness) = connection_readiness {
        readiness.store(true, Ordering::Release);
    }
    let mut sequence = 0u64;
    loop {
        let bytes = match read_message(&mut stream, DEFAULT_MAX_FRAME_LEN).await {
            Ok(bytes) => bytes,
            Err(RpcError::Closed) => return Ok(()),
            Err(error) => return Err(error),
        };
        let ClientFrame::Request { call, payload } = decode_exact(&bytes)? else {
            return Err(protocol_error(
                "unexpected native ACK without a pending operation",
            ));
        };
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| protocol_error("native request identity exhausted"))?;
        if call.connection != connection
            || call.request != sequence
            || call.owner.pid != peer_pid
            || call.owner.tid <= 0
            || call.owner.image_epoch == 0
            || call.owner.callback_epoch == 0
        {
            return Err(protocol_error("native request owner/generation mismatch"));
        }
        let RequestEnvelope { from, request } =
            decode_exact::<RequestEnvelope<G::Request>>(&payload)?;
        if admitted.take().is_some_and(|pid| from.as_raw() != pid) {
            return Err(protocol_error(
                "native first request differs from admitted process",
            ));
        }
        let (guard, mut outbox) = hub.register(from, call, peer.clone())?;
        if let Some(readiness) = &readiness {
            readiness.store(true, Ordering::Release);
        }
        let future = global.receive_rpc(from, request);
        tokio::pin!(future);
        let mut response = None;
        let mut active: Option<ActiveLease> = None;
        let mut operation = 0u64;
        loop {
            let command = if response.is_some() {
                // This decision and closing the enqueue/reservation window are
                // one mutex operation. A concurrent reservation cannot race an
                // automatic Cancel selected from a stale readiness snapshot.
                match guard.finish_if_empty(&mut outbox, active.map(ActiveLease::cancel))? {
                    ReplyReadiness::Finished => break,
                    ReplyReadiness::Command(command) => command,
                    ReplyReadiness::Cancel(operation) => Command {
                        operation,
                        result: None,
                    },
                    ReplyReadiness::Reserved => {
                        tokio::select! {
                            _ = guard.pending.changed.notified() => continue,
                            command = outbox.recv() => command.ok_or_else(|| protocol_error("native outbox closed"))?,
                        }
                    }
                }
            } else {
                if guard.pending.state.lock().unwrap().reservation_aborted {
                    return Err(protocol_error(
                        "native publication reply reservation was aborted",
                    ));
                }
                tokio::select! {
                    value = &mut future => { response = Some(encode(&value)?); continue; }
                    _ = guard.pending.changed.notified() => continue,
                    command = outbox.recv() => command.ok_or_else(|| protocol_error("native outbox closed"))?,
                }
            };
            operation = operation
                .checked_add(1)
                .ok_or_else(|| protocol_error("native operation identity exhausted"))?;
            // Await each complete write and ACK. The Tool future remains pinned
            // in its original state, but is never entered from the native side.
            let frame = encode(&ServerFrame::Control {
                call,
                operation,
                command: command.operation,
            })?;
            write_message(&mut stream, &frame).await?;
            let bytes = read_message(&mut stream, DEFAULT_MAX_FRAME_LEN).await?;
            let ClientFrame::ControlReply {
                call: ack_call,
                operation: ack_operation,
                reply,
            } = decode_exact(&bytes)?
            else {
                return Err(protocol_error(
                    "ordinary request arrived during native control",
                ));
            };
            if ack_call != call || ack_operation != operation || reply.owner != call.owner {
                return Err(protocol_error("native ACK identity mismatch"));
            }
            if reply.status == 0 {
                match command.operation {
                    NativeControlOperation::Begin { owner_epoch } => {
                        if active.is_some()
                            || reply.phase != 1
                            || reply.handle.owner_epoch != owner_epoch
                            || reply.handle.token == 0
                            || reply.handle.owner_pid != call.owner.pid
                            || reply.handle.owner_tid != call.owner.tid
                        {
                            return Err(protocol_error("invalid native Begin ACK"));
                        }
                        active = Some(ActiveLease::Legacy(reply.handle));
                    }
                    NativeControlOperation::Cancel { handle }
                    | NativeControlOperation::Release {
                        receipt: CaptureReceipt { handle, .. },
                    } => {
                        if active != Some(ActiveLease::Legacy(handle)) || reply.phase != 3 {
                            return Err(protocol_error("invalid native release-intent ACK"));
                        }
                        active = None;
                    }
                    NativeControlOperation::Close { handle } => {
                        if active != Some(ActiveLease::Legacy(handle)) || reply.phase != 2 {
                            return Err(protocol_error("invalid native Close ACK"));
                        }
                    }
                    NativeControlOperation::DomainObserve => {}
                    NativeControlOperation::PublicationArm { request } => {
                        let handle = reply.publication.handle;
                        let mut expected = request;
                        if expected.domain == 0 {
                            expected.domain = reply.domain.domain;
                        }
                        if expected.domain == 0
                            || active.is_some()
                            || reply.phase != 1
                            || handle.request != expected
                            || handle.interval == 0
                            || handle.owner_pid != call.owner.pid
                            || handle.owner_tid != call.owner.tid
                            || request.owner.image_epoch != call.owner.image_epoch
                            || request.owner.callback_epoch != call.owner.callback_epoch
                        {
                            return Err(protocol_error("invalid publication Arm ACK"));
                        }
                        active = Some(ActiveLease::Publication(handle));
                    }
                    NativeControlOperation::PublicationClose { handle } => {
                        if active != Some(ActiveLease::Publication(handle)) || reply.phase != 2 {
                            return Err(protocol_error("invalid publication Close ACK"));
                        }
                    }
                    NativeControlOperation::PublicationCancel { handle }
                    | NativeControlOperation::PublicationRelease {
                        receipt: PublicationReceipt { handle, .. },
                    } => {
                        if active != Some(ActiveLease::Publication(handle)) || reply.phase != 3 {
                            return Err(protocol_error("invalid publication release-intent ACK"));
                        }
                        active = None;
                    }
                }
            }
            guard.pending.state.lock().unwrap().lease_active = active.is_some();
            if let Some(result) = command.result {
                if result.send(Ok(reply)).is_err() {
                    return Err(protocol_error(
                        "native command waiter abandoned an executed operation",
                    ));
                }
            } else if active.is_some() {
                return Err(protocol_error(
                    "native cancellation did not retire the completed call's lease",
                ));
            }
        }
        let frame = encode(&ServerFrame::OrdinaryReply {
            call,
            payload: response.unwrap(),
        })?;
        write_message(&mut stream, &frame).await?;
        drop(guard);
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[derive(Default)]
    struct Gate {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl GlobalTool for Gate {
        type Config = ();
        type Request = u64;
        type Response = u64;

        async fn receive_rpc(&self, _from: Tid, request: u64) -> u64 {
            self.entered.notify_one();
            self.release.notified().await;
            request
        }
    }

    async fn start() -> (
        tokio::io::DuplexStream,
        Arc<Gate>,
        PendingNativeCall,
        tokio::task::JoinHandle<Result<(), RpcError>>,
    ) {
        // Deliberately smaller than any complete frame: every I/O operation
        // must survive partial progress even on the successful path.
        let (mut client, server) = tokio::io::duplex(7);
        let global = Arc::new(Gate::default());
        let hub = NativeControlHub::new();
        let task = tokio::spawn(serve(
            global.clone(),
            (),
            server,
            hub.clone(),
            1,
            7,
            None,
            None,
            None,
            None,
        ));
        let hello = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert_eq!(Hello::decode(&hello).unwrap().connection, 1);
        let call = CallIdentity {
            connection: 1,
            request: 1,
            owner: NativeOwner {
                pid: 7,
                tid: 7,
                image_epoch: 1,
                callback_epoch: 10,
            },
        };
        let payload = encode(&RequestEnvelope {
            from: Tid::from_raw(3),
            request: 42u64,
        })
        .unwrap();
        let bytes = encode(&ClientFrame::Request { call, payload }).unwrap();
        write_message(&mut client, &bytes).await.unwrap();
        global.entered.notified().await;
        let pending = hub.pending(Tid::from_raw(3)).unwrap();
        (client, global, pending, task)
    }

    fn reply(call: CallIdentity, phase: u32) -> NativeControlReply {
        NativeControlReply {
            owner: call.owner,
            phase,
            handle: CaptureHandle {
                owner_epoch: 1,
                token: 9,
                owner_pid: 7,
                owner_tid: 7,
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn partial_ack_is_not_restarted_when_tool_finishes_and_cancel_precedes_reply() {
        let (mut client, global, pending, task) = start().await;
        let call = pending.identity;
        let command = tokio::spawn({
            let pending = pending.clone();
            async move {
                pending
                    .command(NativeControlOperation::Begin { owner_epoch: 1 })
                    .await
            }
        });
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::Control { call: actual, operation: 1, command: NativeControlOperation::Begin { owner_epoch: 1 } } if actual == call));
        let ack = encode(&ClientFrame::ControlReply {
            call,
            operation: 1,
            reply: reply(call, 1),
        })
        .unwrap();
        let header = (ack.len() as u32).to_be_bytes();
        client.write_all(&header[..2]).await.unwrap();
        global.release.notify_one();
        tokio::task::yield_now().await;
        client.write_all(&header[2..]).await.unwrap();
        for byte in ack {
            client.write_all(&[byte]).await.unwrap();
        }
        assert_eq!(command.await.unwrap().unwrap().phase, 1);
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::Control { call: actual, operation: 2, command: NativeControlOperation::Cancel { handle } }
                if actual == call && handle == reply(call, 1).handle));
        let ack = encode(&ClientFrame::ControlReply {
            call,
            operation: 2,
            reply: reply(call, 3),
        })
        .unwrap();
        write_message(&mut client, &ack).await.unwrap();
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        let ServerFrame::OrdinaryReply {
            call: actual,
            payload,
        } = decode_exact(&frame).unwrap()
        else {
            panic!("ordinary reply missing")
        };
        assert_eq!(actual, call);
        assert_eq!(decode_exact::<u64>(&payload).unwrap(), 42);
        assert!(
            pending
                .command(NativeControlOperation::Begin { owner_epoch: 2 })
                .await
                .is_err()
        );
        drop(client);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_ack_ends_connection_without_delivering_an_ordinary_reply() {
        let (mut client, _global, pending, task) = start().await;
        let call = pending.identity;
        let command = tokio::spawn(async move {
            pending
                .command(NativeControlOperation::Begin { owner_epoch: 1 })
                .await
        });
        read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        let ack = encode(&ClientFrame::ControlReply {
            call,
            operation: 2,
            reply: reply(call, 1),
        })
        .unwrap();
        write_message(&mut client, &ack).await.unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(command.await.unwrap().is_err());
        assert!(matches!(
            read_message(&mut client, DEFAULT_MAX_FRAME_LEN).await,
            Err(RpcError::Closed)
        ));
    }

    #[tokio::test]
    async fn mid_frame_disconnect_retains_an_indeterminate_operation() {
        let (mut client, _global, pending, task) = start().await;
        let command = tokio::spawn(async move {
            pending
                .command(NativeControlOperation::Begin { owner_epoch: 1 })
                .await
        });
        read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        client.write_all(&[0, 0]).await.unwrap();
        drop(client);
        assert!(
            matches!(task.await.unwrap(), Err(RpcError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
        );
        assert!(
            command
                .await
                .unwrap()
                .unwrap_err()
                .contains("indeterminate")
        );
    }

    #[test]
    fn negotiation_and_complete_frame_identity_are_required() {
        assert!(Hello::decode(&encode(&()).unwrap()).is_err());
        let mut hello = encode(&Hello::new(1, vec![])).unwrap();
        hello.push(0);
        assert!(Hello::decode(&hello).is_err());
        let mut wrong = Hello::new(1, vec![]);
        wrong.version += 1;
        assert!(Hello::decode(&encode(&wrong).unwrap()).is_err());
        assert_eq!(std::mem::size_of::<CaptureHandle>(), 24);
        assert_eq!(std::mem::size_of::<CaptureReceipt>(), 208);
        assert_eq!(std::mem::size_of::<NativeControlReply>(), 1512);
        assert_eq!(std::mem::size_of::<PublicationOwner>(), 32);
        assert_eq!(std::mem::size_of::<SignalDomainSnapshot>(), 696);
        assert_eq!(std::mem::size_of::<PublicationRequest>(), 64);
        assert_eq!(std::mem::size_of::<PublicationHandle>(), 80);
        assert_eq!(std::mem::size_of::<PublicationReceipt>(), 392);
    }

    #[tokio::test]
    async fn reserved_reply_survives_ready_tool_until_publication_release_ack() {
        let (mut client, global, pending, task) = start().await;
        let call = pending.identity;
        let reservation = pending.reserve_reply(71).unwrap();
        assert!(pending.reserve_reply(72).is_err());
        let request = PublicationRequest {
            owner: PublicationOwner {
                image_epoch: call.owner.image_epoch,
                callback_epoch: call.owner.callback_epoch,
                callback_kind: 1,
                ..Default::default()
            },
            // Exercise the SDK's documented canonicalization of zero domain.
            domain: 0,
            reservation: 71,
            child_generation: 2,
            child_pid: 8,
            reserved: 0,
        };
        let arm = tokio::spawn({
            let pending = pending.clone();
            async move {
                pending
                    .command(NativeControlOperation::PublicationArm { request })
                    .await
            }
        });
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(
            decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::Control {
                operation: 1,
                command: NativeControlOperation::PublicationArm { .. },
                ..
            }
        ));
        global.release.notify_one();
        let mut armed = reply(call, 1);
        armed.publication.handle = PublicationHandle {
            request: PublicationRequest {
                domain: 5,
                ..request
            },
            interval: 9,
            owner_pid: 7,
            owner_tid: 7,
        };
        armed.domain.domain = 5;
        let ack = encode(&ClientFrame::ControlReply {
            call,
            operation: 1,
            reply: armed,
        })
        .unwrap();
        write_message(&mut client, &ack).await.unwrap();
        assert_eq!(arm.await.unwrap().unwrap(), armed);
        // A read-only command must still be serviceable after ordinary work is
        // ready. An automatic Cancel or ordinary reply would fail this check.
        let observe = tokio::spawn({
            let pending = pending.clone();
            async move { pending.command(NativeControlOperation::DomainObserve).await }
        });
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(
            decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::Control {
                operation: 2,
                command: NativeControlOperation::DomainObserve,
                ..
            }
        ));
        write_message(
            &mut client,
            &encode(&ClientFrame::ControlReply {
                call,
                operation: 2,
                reply: armed,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        observe.await.unwrap().unwrap();
        let cancel = tokio::spawn({
            let pending = pending.clone();
            async move {
                pending
                    .command(NativeControlOperation::PublicationCancel {
                        handle: armed.publication.handle,
                    })
                    .await
            }
        });
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(
            decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::Control {
                operation: 3,
                command: NativeControlOperation::PublicationCancel { .. },
                ..
            }
        ));
        let mut cancelled = armed;
        cancelled.phase = 3;
        write_message(
            &mut client,
            &encode(&ClientFrame::ControlReply {
                call,
                operation: 3,
                reply: cancelled,
            })
            .unwrap(),
        )
        .await
        .unwrap();
        cancel.await.unwrap().unwrap();
        reservation.finish().unwrap();
        let frame = read_message(&mut client, DEFAULT_MAX_FRAME_LEN)
            .await
            .unwrap();
        assert!(matches!(decode_exact::<ServerFrame>(&frame).unwrap(),
            ServerFrame::OrdinaryReply { call: actual, .. } if actual == call));
        pending.closed().await;
        drop(client);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn abandoned_reply_reservation_aborts_a_pending_tool_without_ordinary_success() {
        let (mut client, _global, pending, task) = start().await;
        let reservation = pending.reserve_reply(1).unwrap();
        drop(reservation);
        assert!(task.await.unwrap().is_err());
        pending.closed().await;
        assert!(matches!(
            read_message(&mut client, DEFAULT_MAX_FRAME_LEN).await,
            Err(RpcError::Closed)
        ));
        assert!(pending.reserve_reply(2).is_err());
    }
}
