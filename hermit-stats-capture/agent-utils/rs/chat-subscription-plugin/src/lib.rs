//! Bounded process framing for provider-neutral ordered-subscription backends.
//!
//! Rust traits are never passed through a dynamic-library ABI. A plugin is an ordinary process
//! that reads and writes length-prefixed JSON frames. The host and plugin negotiate an explicit
//! protocol version and capability report before subscribing. Each frame is bounded before
//! allocation, and the server emits at most one receipt-bearing batch before waiting for commit.
//!
//! [`PluginBackend`] adapts a connected process into the same object-safe trait used by statically
//! linked backends. [`serve`] hosts any statically linked backend over the protocol. Process
//! discovery, containment, credential setup, and lifecycle supervision belong to the caller.

#![doc = include_str!("../README.md")]
#![deny(unsafe_code)]

/// Linux process hosting with bounded control phases and event-driven supervision.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod process;
/// Explicit refusal surface on targets without Linux pidfd/clone3 supervision.
#[cfg(not(target_os = "linux"))]
#[path = "process_unsupported.rs"]
pub mod process;
// Linux CI also type-checks and exercises the non-Linux refusal surface because cross standard
// libraries are not guaranteed to be installed on every development host.
#[cfg(all(test, target_os = "linux"))]
#[path = "process_unsupported.rs"]
pub mod process_unsupported_compile_test;

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use chat_subscription::{
    BackendCapabilities, BackendConfiguration, BackendFailure, CancellationError, ChannelId,
    ChatSubscription, ChatSubscriptionBackend, ChatSubscriptionCancellation, CommittableEvent,
    DeliveryBatch, DeliveryId, EventKind, EventSequence, Heartbeat, InboundMessage, MessageId,
    ProviderCursor, ProviderPayload, ReconciliationGap, ReplaySupport, SenderId, SubscribeRequest,
    SubscriptionError, SubscriptionItem, ThreadId,
};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Serialize};

/// Independent, sticky authority that interrupts one host-frame input actor.
#[derive(Clone)]
pub struct HostFrameInputCancellation {
    cancelled: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl HostFrameInputCancellation {
    /// Build an authority whose callback makes an in-progress [`Read::read`] return promptly.
    ///
    /// The callback may run more than once and must remain nonblocking. The sticky cancellation
    /// bit is published before it runs, so the reader can distinguish an internal shutdown wake
    /// from peer EOF or malformed input.
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(wake),
        }
    }

    fn passive() -> Self {
        Self::new(|| {})
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        (self.wake)();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// A host-frame reader with independent termination for the concurrent protocol input actor.
///
/// `input_cancellation` must make an in-progress read return promptly. This transport contract is
/// separate from provider cancellation and keeps [`ChatSubscriptionBackend`] process-agnostic.
pub trait HostFrameInput: Read + Send {
    /// Return sticky authority for this exact reader generation.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if an independent wake handle cannot be created.
    fn input_cancellation(&self) -> io::Result<HostFrameInputCancellation>;
}

impl<T: AsRef<[u8]> + Send> HostFrameInput for Cursor<T> {
    fn input_cancellation(&self) -> io::Result<HostFrameInputCancellation> {
        // Cursor reads never block, so the actor observes EOF without an external wake.
        Ok(HostFrameInputCancellation::passive())
    }
}

#[cfg(unix)]
impl HostFrameInput for std::os::unix::net::UnixStream {
    fn input_cancellation(&self) -> io::Result<HostFrameInputCancellation> {
        use std::net::Shutdown;

        let stream = self.try_clone()?;
        Ok(HostFrameInputCancellation::new(move || {
            let _ = stream.shutdown(Shutdown::Read);
        }))
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod interruptible_fd_input {
    use super::{HostFrameInput, HostFrameInputCancellation};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::sync::Arc;

    pub(super) struct InterruptibleFdInput<R> {
        reader: R,
        wake_descriptor: Arc<OwnedFd>,
        cancellation: HostFrameInputCancellation,
    }

    pub(super) fn signal_eventfd_with(mut write_once: impl FnMut() -> io::Result<()>) {
        loop {
            match write_once() {
                Ok(()) => return,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // A saturated eventfd is already readable, so EAGAIN proves that the sole wake
                // cannot be lost. The separate sticky bit covers cancellation before polling.
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => {
                    debug_assert!(false, "cannot wake host-frame input actor: {error}");
                    return;
                }
            }
        }
    }

    impl<R: AsRawFd> InterruptibleFdInput<R> {
        pub(super) fn new(reader: R) -> io::Result<Self> {
            // SAFETY: eventfd takes scalar flags and returns a fresh descriptor on success.
            let descriptor = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful eventfd returned one fresh descriptor now owned here.
            let wake_descriptor = Arc::new(unsafe { OwnedFd::from_raw_fd(descriptor) });
            let wake = Arc::clone(&wake_descriptor);
            let cancellation = HostFrameInputCancellation::new(move || {
                signal_eventfd_with(|| {
                    let value = 1_u64.to_ne_bytes();
                    // SAFETY: wake owns a live eventfd and value is initialized for this call.
                    let result = unsafe {
                        libc::write(wake.as_raw_fd(), value.as_ptr().cast(), value.len())
                    };
                    if result < 0 {
                        Err(io::Error::last_os_error())
                    } else if usize::try_from(result).ok() == Some(value.len()) {
                        Ok(())
                    } else {
                        Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "eventfd accepted a partial cancellation wake",
                        ))
                    }
                });
            });
            Ok(Self {
                reader,
                wake_descriptor,
                cancellation,
            })
        }
    }

    impl InterruptibleFdInput<File> {
        pub(super) fn stdin() -> io::Result<Self> {
            // Do not wrap `std::io::Stdin`: its internal buffer can read a later frame before this
            // wrapper polls fd 0, leaving the bytes invisible to poll and the actor stuck. A dup
            // shares the pipe stream without sharing that userspace buffer and is independently
            // owned for the duration of the server.
            // SAFETY: fcntl receives the live process stdin descriptor and returns a fresh
            // close-on-exec descriptor on success.
            let descriptor = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
            if descriptor < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful F_DUPFD_CLOEXEC returned one fresh descriptor now owned here.
            let input = File::from(unsafe { OwnedFd::from_raw_fd(descriptor) });
            Self::new(input)
        }
    }

    impl<R: Read + AsRawFd> Read for InterruptibleFdInput<R> {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if bytes.is_empty() {
                return Ok(0);
            }
            loop {
                if self.cancellation.is_cancelled() {
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "host-frame input actor was cancelled",
                    ));
                }
                let mut descriptors = [
                    libc::pollfd {
                        fd: self.reader.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                    libc::pollfd {
                        fd: self.wake_descriptor.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    },
                ];
                // SAFETY: descriptors is an initialized two-element array retained for the call.
                let result = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, -1) };
                if result > 0 {
                    if descriptors[1].revents != 0 || self.cancellation.is_cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "host-frame input actor was cancelled",
                        ));
                    }
                    if descriptors[0].revents != 0 {
                        return self.reader.read(bytes);
                    }
                    continue;
                }
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }

    impl<R: Read + AsRawFd + Send> HostFrameInput for InterruptibleFdInput<R> {
        fn input_cancellation(&self) -> io::Result<HostFrameInputCancellation> {
            Ok(self.cancellation.clone())
        }
    }
}

/// The only currently supported process-protocol version.
pub const PROTOCOL_VERSION: u16 = 1;
/// Stable wire-protocol identity recorded by plugin manifests.
pub const PROTOCOL_NAME: &str = "agentctl-chat-subscription";
/// Maximum serialized payload bytes in one frame, excluding its four-byte length prefix.
pub const MAX_FRAME_BYTES: usize = 1_048_576;
// Each event's fixed field names, punctuation, tags, boolean/null value, and separating comma fit
// conservatively within 256 bytes; all variable JSON values are counted by MAX_BATCH_BYTES.
const MAX_WIRE_EVENT_FIXED_BYTES: usize = 256;
// The Item/Batch envelopes, sequence digits, array punctuation, and other fixed fields are much
// smaller than this deliberately auditable allowance.
const MAX_WIRE_BATCH_FIXED_BYTES: usize = 1_024;
const MAX_JSON_ESCAPED_TOKEN_BYTES: usize = 2 + 6 * chat_subscription::MAX_TOKEN_BYTES;
/// Conservative upper bound for any process frame made from a core-valid delivery batch.
pub const MAX_CORE_BATCH_FRAME_BYTES: usize = chat_subscription::MAX_BATCH_BYTES
    + chat_subscription::MAX_BATCH_EVENTS * MAX_WIRE_EVENT_FIXED_BYTES
    + 2 * MAX_JSON_ESCAPED_TOKEN_BYTES
    + MAX_WIRE_BATCH_FIXED_BYTES;
const _: () = assert!(MAX_CORE_BATCH_FRAME_BYTES <= MAX_FRAME_BYTES);

/// A bounded framing, negotiation, or remote-backend failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginError {
    code: &'static str,
    detail: String,
}

impl PluginError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        if detail.len() > 2_000 {
            let mut boundary = 2_000;
            while !detail.is_char_boundary(boundary) {
                boundary -= 1;
            }
            detail.truncate(boundary);
        }
        Self { code, detail }
    }

    /// Return the stable machine-readable classification.
    #[must_use]
    pub fn code(&self) -> &str {
        self.code
    }

    /// Return the bounded human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for PluginError {}

impl From<io::Error> for PluginError {
    fn from(error: io::Error) -> Self {
        Self::new("plugin_io", error.to_string())
    }
}

fn backend_failure(error: PluginError) -> BackendFailure {
    let retryable = matches!(
        error.code(),
        "plugin_io" | "unexpected_eof" | "truncated_frame" | "commit_outcome_unknown"
    );
    BackendFailure::new(error.code(), error.detail(), retryable).unwrap_or_else(|_| {
        BackendFailure::new(
            "plugin_protocol",
            "plugin failed with an invalid or oversized diagnostic",
            false,
        )
        .expect("constant fallback backend failure is valid")
    })
}

struct FramedIo<R, W> {
    reader: R,
    writer: W,
}

struct StrictJsonSeed;

impl<'de> DeserializeSeed<'de> for StrictJsonSeed {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("strict JSON without duplicate object keys")
    }

    fn visit_bool<E: de::Error>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E: de::Error>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E: de::Error>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E: de::Error>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E: de::Error>(self, _value: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        StrictJsonSeed.deserialize(deserializer)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        while sequence.next_element_seed(StrictJsonSeed)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            map.next_value_seed(StrictJsonSeed)?;
        }
        Ok(())
    }

    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        StrictJsonSeed.deserialize(deserializer)
    }

    fn visit_bytes<E: de::Error>(self, _value: &[u8]) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_byte_buf<E: de::Error>(self, _value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(())
    }
}

fn validate_strict_json(payload: &[u8]) -> Result<(), PluginError> {
    let mut deserializer = serde_json::Deserializer::from_slice(payload);
    StrictJsonSeed
        .deserialize(&mut deserializer)
        .and_then(|()| deserializer.end())
        .map_err(|error| PluginError::new("invalid_frame", error.to_string()))
}

/// Decode one JSON document after rejecting duplicate object keys at every depth.
///
/// This is also suitable for manifests that select this protocol: accepting last-key-wins JSON
/// there could make the reviewed identity differ from the identity used at runtime.
///
/// # Errors
///
/// Returns [`PluginError`] when the document is malformed, contains a duplicate key, has trailing
/// data, or does not satisfy `T`'s serde contract.
pub fn decode_strict_json<T: for<'de> Deserialize<'de>>(payload: &[u8]) -> Result<T, PluginError> {
    validate_strict_json(payload)?;
    serde_json::from_slice(payload)
        .map_err(|error| PluginError::new("invalid_frame", error.to_string()))
}

impl<R: Read, W: Write> FramedIo<R, W> {
    fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }

    fn send<T: Serialize>(&mut self, value: &T) -> Result<(), PluginError> {
        let payload = serde_json::to_vec(value)
            .map_err(|error| PluginError::new("invalid_local_frame", error.to_string()))?;
        if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
            return Err(PluginError::new(
                "frame_limit_exceeded",
                format!(
                    "outbound plugin frame contains {} bytes; maximum is {MAX_FRAME_BYTES}",
                    payload.len()
                ),
            ));
        }
        let length = u32::try_from(payload.len()).map_err(|_| {
            PluginError::new("frame_limit_exceeded", "plugin frame length exceeds u32")
        })?;
        // One write_all call lets process adapters serialize an entire frame under one writer
        // lock. Header and payload must never interleave with an independently requested Close.
        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(&payload);
        self.writer.write_all(&frame)?;
        self.writer.flush()?;
        Ok(())
    }

    fn receive<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, PluginError> {
        let mut header = [0_u8; 4];
        loop {
            match self.reader.read(&mut header[..1]) {
                Ok(0) => return Ok(None),
                Ok(1) => break,
                Ok(_) => unreachable!("one-byte read returned more than one byte"),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        self.reader.read_exact(&mut header[1..]).map_err(|error| {
            PluginError::new(
                "truncated_frame",
                format!("plugin frame ended inside its length prefix: {error}"),
            )
        })?;
        let length = u32::from_be_bytes(header) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(PluginError::new(
                "frame_limit_exceeded",
                format!("plugin announced a {length}-byte frame; maximum is {MAX_FRAME_BYTES}"),
            ));
        }
        let mut payload = vec![0_u8; length];
        self.reader.read_exact(&mut payload).map_err(|error| {
            PluginError::new(
                "truncated_frame",
                format!("plugin frame ended before {length} payload bytes: {error}"),
            )
        })?;
        decode_strict_json(&payload).map(Some)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ClientFrame {
    Hello {
        min_version: u16,
        max_version: u16,
    },
    Start {
        channel_ids: Vec<String>,
        allowed_senders: Vec<String>,
        max_uncommitted: u16,
        resume_from: Option<String>,
        backend_configuration: Option<WireBackendConfiguration>,
    },
    Commit {
        sequence: u64,
        delivery_id: String,
    },
    Close,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ServerFrame {
    Hello {
        version: u16,
        capabilities: WireCapabilities,
    },
    Subscribed,
    Item {
        item: WireItem,
    },
    Committed {
        sequence: u64,
    },
    End,
    Error {
        code: String,
        detail: String,
        retryable: bool,
        fatal: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCapabilities {
    backend_name: String,
    replay: WireReplay,
    full_message_data: bool,
    max_uncommitted: u16,
    event_kinds: Vec<WireEventKind>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireReplay {
    CurrentOnly,
    Cursor,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireEventKind {
    MessageCreated,
    Checkpoint,
    Gap,
    Heartbeat,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireItem {
    Batch {
        sequence: u64,
        provider_cursor: String,
        delivery_id: String,
        events: Vec<WireEvent>,
    },
    Heartbeat {
        sequence: u64,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireEvent {
    MessageCreated(Box<WireMessageCreated>),
    Checkpoint,
    Gap { reason: Option<String> },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMessageCreated {
    channel_id: String,
    message_id: String,
    thread_id: String,
    sender_id: String,
    text: String,
    created_at: String,
    thread_reply: bool,
    provider_payload: Option<WireProviderPayload>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProviderPayload {
    schema: String,
    data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBackendConfiguration {
    schema: String,
    data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum BorrowedServerFrame<'a> {
    Item { item: BorrowedWireItem<'a> },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BorrowedWireItem<'a> {
    Batch {
        sequence: u64,
        provider_cursor: &'a str,
        delivery_id: &'a str,
        events: BorrowedWireEvents<'a>,
    },
    Heartbeat {
        sequence: u64,
    },
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BorrowedWireEvent<'a> {
    MessageCreated(BorrowedWireMessageCreated<'a>),
    Checkpoint,
    Gap { reason: Option<&'a str> },
}

#[derive(Serialize)]
struct BorrowedWireMessageCreated<'a> {
    channel_id: &'a str,
    message_id: &'a str,
    thread_id: &'a str,
    sender_id: &'a str,
    text: &'a str,
    created_at: &'a str,
    thread_reply: bool,
    provider_payload: Option<BorrowedWireProviderPayload<'a>>,
}

#[derive(Serialize)]
struct BorrowedWireProviderPayload<'a> {
    schema: &'a str,
    data: &'a serde_json::Map<String, serde_json::Value>,
}

struct BorrowedWireEvents<'a>(&'a [CommittableEvent]);

impl Serialize for BorrowedWireEvents<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for event in self.0 {
            sequence.serialize_element(&BorrowedWireEvent::from(event))?;
        }
        sequence.end()
    }
}

impl<'a> From<&'a SubscriptionItem> for BorrowedWireItem<'a> {
    fn from(value: &'a SubscriptionItem) -> Self {
        match value {
            SubscriptionItem::Heartbeat(heartbeat) => Self::Heartbeat {
                sequence: heartbeat.sequence().get(),
            },
            SubscriptionItem::Batch(batch) => Self::Batch {
                sequence: batch.sequence().get(),
                provider_cursor: batch.cursor().as_str(),
                delivery_id: batch.delivery_id().as_str(),
                events: BorrowedWireEvents(batch.events()),
            },
        }
    }
}

impl<'a> From<&'a CommittableEvent> for BorrowedWireEvent<'a> {
    fn from(value: &'a CommittableEvent) -> Self {
        match value {
            CommittableEvent::MessageCreated(message) => {
                Self::MessageCreated(BorrowedWireMessageCreated {
                    channel_id: message.channel_id().as_str(),
                    message_id: message.message_id().as_str(),
                    thread_id: message.thread_id().as_str(),
                    sender_id: message.sender_id().as_str(),
                    text: message.text(),
                    created_at: message.created_at(),
                    thread_reply: message.is_thread_reply(),
                    provider_payload: message.provider_payload().map(|payload| {
                        BorrowedWireProviderPayload {
                            schema: payload.schema(),
                            data: payload.data(),
                        }
                    }),
                })
            }
            CommittableEvent::Checkpoint => Self::Checkpoint,
            CommittableEvent::Gap(gap) => Self::Gap {
                reason: gap.reason(),
            },
        }
    }
}

impl From<&BackendCapabilities> for WireCapabilities {
    fn from(value: &BackendCapabilities) -> Self {
        Self {
            backend_name: value.backend_name().to_owned(),
            replay: match value.replay() {
                ReplaySupport::CurrentOnly => WireReplay::CurrentOnly,
                ReplaySupport::Cursor => WireReplay::Cursor,
            },
            full_message_data: value.full_message_data(),
            max_uncommitted: value.max_uncommitted().get(),
            event_kinds: value
                .event_kinds()
                .iter()
                .map(|kind| match kind {
                    EventKind::MessageCreated => WireEventKind::MessageCreated,
                    EventKind::Checkpoint => WireEventKind::Checkpoint,
                    EventKind::Gap => WireEventKind::Gap,
                    EventKind::Heartbeat => WireEventKind::Heartbeat,
                })
                .collect(),
        }
    }
}

impl TryFrom<WireCapabilities> for BackendCapabilities {
    type Error = PluginError;

    fn try_from(value: WireCapabilities) -> Result<Self, Self::Error> {
        let max_uncommitted = NonZeroU16::new(value.max_uncommitted).ok_or_else(|| {
            PluginError::new("invalid_capabilities", "max_uncommitted must be nonzero")
        })?;
        BackendCapabilities::new(
            value.backend_name,
            match value.replay {
                WireReplay::CurrentOnly => ReplaySupport::CurrentOnly,
                WireReplay::Cursor => ReplaySupport::Cursor,
            },
            value.full_message_data,
            max_uncommitted,
            value
                .event_kinds
                .into_iter()
                .map(|kind| match kind {
                    WireEventKind::MessageCreated => EventKind::MessageCreated,
                    WireEventKind::Checkpoint => EventKind::Checkpoint,
                    WireEventKind::Gap => EventKind::Gap,
                    WireEventKind::Heartbeat => EventKind::Heartbeat,
                })
                .collect(),
        )
        .map_err(|error| PluginError::new("invalid_capabilities", error.to_string()))
    }
}

impl From<&SubscriptionItem> for WireItem {
    fn from(value: &SubscriptionItem) -> Self {
        match value {
            SubscriptionItem::Heartbeat(heartbeat) => Self::Heartbeat {
                sequence: heartbeat.sequence().get(),
            },
            SubscriptionItem::Batch(batch) => Self::Batch {
                sequence: batch.sequence().get(),
                provider_cursor: batch.cursor().as_str().to_owned(),
                delivery_id: batch.delivery_id().as_str().to_owned(),
                events: batch.events().iter().map(WireEvent::from).collect(),
            },
        }
    }
}

impl From<&CommittableEvent> for WireEvent {
    fn from(value: &CommittableEvent) -> Self {
        match value {
            CommittableEvent::MessageCreated(message) => {
                Self::MessageCreated(Box::new(WireMessageCreated {
                    channel_id: message.channel_id().as_str().to_owned(),
                    message_id: message.message_id().as_str().to_owned(),
                    thread_id: message.thread_id().as_str().to_owned(),
                    sender_id: message.sender_id().as_str().to_owned(),
                    text: message.text().to_owned(),
                    created_at: message.created_at().to_owned(),
                    thread_reply: message.is_thread_reply(),
                    provider_payload: message.provider_payload().map(|payload| {
                        WireProviderPayload {
                            schema: payload.schema().to_owned(),
                            data: payload.data().clone(),
                        }
                    }),
                }))
            }
            CommittableEvent::Checkpoint => Self::Checkpoint,
            CommittableEvent::Gap(gap) => Self::Gap {
                reason: gap.reason().map(str::to_owned),
            },
        }
    }
}

impl TryFrom<WireItem> for SubscriptionItem {
    type Error = PluginError;

    fn try_from(value: WireItem) -> Result<Self, Self::Error> {
        match value {
            WireItem::Heartbeat { sequence } => Ok(Self::Heartbeat(Heartbeat::new(
                sequence_from_wire(sequence)?,
            ))),
            WireItem::Batch {
                sequence,
                provider_cursor,
                delivery_id,
                events,
            } => {
                let events = events
                    .into_iter()
                    .map(CommittableEvent::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                DeliveryBatch::new(
                    sequence_from_wire(sequence)?,
                    ProviderCursor::new(provider_cursor)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    DeliveryId::new(delivery_id)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    events,
                )
                .map(Self::Batch)
                .map_err(|error| PluginError::new("invalid_item", error.to_string()))
            }
        }
    }
}

fn sequence_from_wire(value: u64) -> Result<EventSequence, PluginError> {
    EventSequence::new(value).map_err(|error| PluginError::new("invalid_item", error.to_string()))
}

impl TryFrom<WireEvent> for CommittableEvent {
    type Error = PluginError;

    fn try_from(value: WireEvent) -> Result<Self, Self::Error> {
        match value {
            WireEvent::Checkpoint => Ok(Self::Checkpoint),
            WireEvent::Gap { reason } => ReconciliationGap::new(reason)
                .map(Self::Gap)
                .map_err(|error| PluginError::new("invalid_item", error.to_string())),
            WireEvent::MessageCreated(message) => {
                let WireMessageCreated {
                    channel_id,
                    message_id,
                    thread_id,
                    sender_id,
                    text,
                    created_at,
                    thread_reply,
                    provider_payload,
                } = *message;
                let provider_payload = provider_payload
                    .map(|payload| ProviderPayload::new(payload.schema, payload.data))
                    .transpose()
                    .map_err(|error| PluginError::new("invalid_item", error.to_string()))?;
                let message = InboundMessage::new(
                    ChannelId::new(channel_id)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    MessageId::new(message_id)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    ThreadId::new(thread_id)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    SenderId::new(sender_id)
                        .map_err(|error| PluginError::new("invalid_item", error.to_string()))?,
                    text,
                    created_at,
                    thread_reply,
                )
                .map_err(|error| PluginError::new("invalid_item", error.to_string()))?;
                let message = match provider_payload {
                    Some(provider_payload) => message.with_provider_payload(provider_payload),
                    None => message,
                };
                Ok(Self::message_created(message))
            }
        }
    }
}

fn remote_failure(code: String, detail: String, retryable: bool) -> BackendFailure {
    BackendFailure::new(code, detail, retryable).unwrap_or_else(|_| {
        BackendFailure::new(
            "invalid_remote_error",
            "plugin returned an invalid or oversized error",
            false,
        )
        .expect("constant fallback backend failure is valid")
    })
}

/// A connected process plugin that implements the ordinary backend factory trait.
///
/// Construction performs the version/capability handshake. One instance opens at most one
/// subscription because its byte streams move into the resulting driver.
pub struct PluginBackend<R, W> {
    io: Option<FramedIo<R, W>>,
    capabilities: BackendCapabilities,
}

impl<R: Read + Send + 'static, W: Write + Send + 'static> PluginBackend<R, W> {
    /// Negotiate the protocol and read the plugin's capability report.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] for framing, version, or capability failures.
    pub fn connect(reader: R, writer: W) -> Result<Self, PluginError> {
        let mut io = FramedIo::new(reader, writer);
        io.send(&ClientFrame::Hello {
            min_version: PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
        })?;
        let response: ServerFrame = io.receive()?.ok_or_else(|| {
            PluginError::new(
                "unexpected_eof",
                "plugin exited before protocol negotiation",
            )
        })?;
        let capabilities = match response {
            ServerFrame::Hello {
                version,
                capabilities,
            } if version == PROTOCOL_VERSION => capabilities.try_into()?,
            ServerFrame::Hello { version, .. } => {
                return Err(PluginError::new(
                    "incompatible_protocol",
                    format!("plugin selected unsupported protocol version {version}"),
                ));
            }
            ServerFrame::Error { code, detail, .. } => {
                return Err(PluginError::new(
                    "remote_refused",
                    format!("{code}: {detail}"),
                ));
            }
            _ => {
                return Err(PluginError::new(
                    "unexpected_frame",
                    "plugin did not begin with a hello frame",
                ));
            }
        };
        Ok(Self {
            io: Some(io),
            capabilities,
        })
    }

    /// Cooperatively close a negotiated connection before a subscription starts.
    ///
    /// Once [`ChatSubscriptionBackend::subscribe`] moves the streams into a driver, call
    /// [`ChatSubscription::close`] instead. Neither path replaces bounded child-process
    /// supervision: a plugin blocked in provider code may not read the close frame.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] if the close frame cannot be written.
    pub fn close(&mut self) -> Result<(), PluginError> {
        if let Some(mut io) = self.io.take() {
            io.send(&ClientFrame::Close)?;
        }
        Ok(())
    }
}

impl<R: Read + Send + 'static, W: Write + Send + 'static> PluginBackend<R, W> {
    pub(crate) fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }

    pub(crate) fn subscribe(
        &mut self,
        request: &SubscribeRequest,
    ) -> Result<PluginDriver<R, W>, BackendFailure> {
        let mut io = self.io.take().ok_or_else(|| {
            BackendFailure::new(
                "plugin_already_subscribed",
                "one plugin connection can open only one subscription",
                false,
            )
            .expect("constant backend failure is valid")
        })?;
        io.send(&ClientFrame::Start {
            channel_ids: request
                .channel_ids()
                .iter()
                .map(|channel| channel.as_str().to_owned())
                .collect(),
            allowed_senders: request
                .allowed_senders()
                .iter()
                .map(|sender| sender.as_str().to_owned())
                .collect(),
            max_uncommitted: request.max_uncommitted().get(),
            resume_from: request
                .resume_from()
                .map(|cursor| cursor.as_str().to_owned()),
            backend_configuration: request.backend_configuration().map(|configuration| {
                WireBackendConfiguration {
                    schema: configuration.schema().to_owned(),
                    data: configuration.data().clone(),
                }
            }),
        })
        .map_err(backend_failure)?;
        match io.receive::<ServerFrame>().map_err(backend_failure)? {
            Some(ServerFrame::Subscribed) => Ok(PluginDriver {
                io,
                pending: None,
                terminal: false,
            }),
            Some(ServerFrame::Error {
                code,
                detail,
                retryable,
                ..
            }) => Err(remote_failure(code, detail, retryable)),
            Some(_) => Err(backend_failure(PluginError::new(
                "unexpected_frame",
                "plugin did not acknowledge the subscription",
            ))),
            None => Err(backend_failure(PluginError::new(
                "unexpected_eof",
                "plugin exited before acknowledging the subscription",
            ))),
        }
    }
}

pub(crate) struct PluginDriver<R, W> {
    io: FramedIo<R, W>,
    pending: Option<(EventSequence, DeliveryId)>,
    terminal: bool,
}

impl<R: Read + Send, W: Write + Send> PluginDriver<R, W> {
    pub(crate) fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
        if self.pending.is_some() {
            return Err(BackendFailure::new(
                "plugin_backpressure_violation",
                "plugin driver was asked for another item before commit",
                false,
            )
            .expect("constant backend failure is valid"));
        }
        if self.terminal {
            return Ok(None);
        }
        let frame = self.io.receive::<ServerFrame>().map_err(backend_failure)?;
        match frame {
            Some(ServerFrame::Item { item }) => {
                let item = SubscriptionItem::try_from(item).map_err(backend_failure)?;
                if let SubscriptionItem::Batch(batch) = &item {
                    self.pending = Some((batch.sequence(), batch.delivery_id().clone()));
                }
                Ok(Some(item))
            }
            Some(ServerFrame::End) => {
                self.terminal = true;
                Ok(None)
            }
            None => Err(backend_failure(PluginError::new(
                "unexpected_eof",
                "plugin connection closed without an explicit end frame",
            ))),
            Some(ServerFrame::Error {
                code,
                detail,
                retryable,
                ..
            }) => Err(remote_failure(code, detail, retryable)),
            Some(_) => Err(backend_failure(PluginError::new(
                "unexpected_frame",
                "plugin emitted a control frame while an event was expected",
            ))),
        }
    }

    pub(crate) fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
        let (sequence, expected_delivery_id) = self.pending.as_ref().ok_or_else(|| {
            BackendFailure::new(
                "plugin_no_pending_delivery",
                "plugin driver has no delivery to acknowledge",
                false,
            )
            .expect("constant backend failure is valid")
        })?;
        if expected_delivery_id != delivery_id {
            return Err(BackendFailure::new(
                "plugin_delivery_id_mismatch",
                "acknowledgement id does not match the pending delivery batch",
                false,
            )
            .expect("constant backend failure is valid"));
        }
        let sequence = *sequence;
        if let Err(error) = self.io.send(&ClientFrame::Commit {
            sequence: sequence.get(),
            delivery_id: delivery_id.as_str().to_owned(),
        }) {
            return Err(commit_outcome_unknown(error));
        }
        let confirmation = self
            .io
            .receive::<ServerFrame>()
            .map_err(commit_outcome_unknown)?;
        match confirmation {
            Some(ServerFrame::Committed { sequence: observed }) if observed == sequence.get() => {
                self.pending = None;
                Ok(())
            }
            Some(ServerFrame::Error {
                code,
                detail,
                retryable,
                ..
            }) => Err(commit_outcome_unknown(format!(
                "plugin returned error {code} (retryable={retryable}): {detail}"
            ))),
            Some(frame) => Err(commit_outcome_unknown(format!(
                "plugin returned non-matching commit confirmation: {frame:?}"
            ))),
            None => Err(commit_outcome_unknown(
                "plugin exited before confirming the commit",
            )),
        }
    }

    pub(crate) fn close(&mut self) -> Result<(), BackendFailure> {
        if self.terminal {
            return Ok(());
        }
        self.io.send(&ClientFrame::Close).map_err(backend_failure)?;
        self.pending = None;
        self.terminal = true;
        Ok(())
    }
}

fn commit_outcome_unknown(cause: impl fmt::Display) -> BackendFailure {
    backend_failure(PluginError::new(
        "commit_outcome_unknown",
        format!("commit may have been applied but confirmation failed: {cause}"),
    ))
}

fn send_backend_error<R: Read, W: Write>(
    io: &mut FramedIo<R, W>,
    error: &BackendFailure,
    fatal: bool,
) -> Result<(), PluginError> {
    io.send(&ServerFrame::Error {
        code: error.code().to_owned(),
        detail: error.detail().to_owned(),
        retryable: error.retryable(),
        fatal,
    })
}

fn subscription_failure(error: SubscriptionError) -> BackendFailure {
    match error {
        SubscriptionError::Backend(error) => error,
        other => BackendFailure::new("subscription_contract", other.to_string(), false)
            .expect("core contract diagnostics are bounded"),
    }
}

fn serve_with<R: Read, W: Write>(
    backend: &mut dyn ChatSubscriptionBackend,
    reader: R,
    writer: W,
    run_subscription: impl FnOnce(&mut ChatSubscription, FramedIo<R, W>) -> Result<(), PluginError>,
) -> Result<(), PluginError> {
    let mut io = FramedIo::new(reader, writer);
    let hello = io.receive::<ClientFrame>()?.ok_or_else(|| {
        PluginError::new(
            "unexpected_eof",
            "host disconnected before protocol negotiation",
        )
    })?;
    match hello {
        ClientFrame::Hello {
            min_version,
            max_version,
        } if min_version <= PROTOCOL_VERSION && PROTOCOL_VERSION <= max_version => {
            io.send(&ServerFrame::Hello {
                version: PROTOCOL_VERSION,
                capabilities: WireCapabilities::from(&backend.capabilities()),
            })?;
        }
        ClientFrame::Hello { .. } => {
            io.send(&ServerFrame::Error {
                code: "incompatible_protocol".to_owned(),
                detail: format!("plugin supports only protocol version {PROTOCOL_VERSION}"),
                retryable: false,
                fatal: true,
            })?;
            return Ok(());
        }
        _ => {
            return Err(PluginError::new(
                "unexpected_frame",
                "host did not begin with a hello frame",
            ));
        }
    }

    let request = match io.receive::<ClientFrame>()? {
        Some(ClientFrame::Start {
            channel_ids,
            allowed_senders,
            max_uncommitted,
            resume_from,
            backend_configuration,
        }) => {
            if max_uncommitted != 1 {
                return Err(PluginError::new(
                    "invalid_start",
                    "protocol v1 requires max_uncommitted=1",
                ));
            }
            let channel_ids = channel_ids
                .into_iter()
                .map(ChannelId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| PluginError::new("invalid_start", error.to_string()))?;
            let allowed_senders = allowed_senders
                .into_iter()
                .map(SenderId::new)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| PluginError::new("invalid_start", error.to_string()))?;
            let request = SubscribeRequest::new(
                channel_ids,
                allowed_senders,
                resume_from
                    .map(ProviderCursor::new)
                    .transpose()
                    .map_err(|error| PluginError::new("invalid_start", error.to_string()))?,
            )
            .map_err(|error| PluginError::new("invalid_start", error.to_string()))?;
            match backend_configuration {
                Some(configuration) => request.with_backend_configuration(
                    BackendConfiguration::new(configuration.schema, configuration.data)
                        .map_err(|error| PluginError::new("invalid_start", error.to_string()))?,
                ),
                None => request,
            }
        }
        Some(ClientFrame::Close) | None => return Ok(()),
        Some(_) => {
            return Err(PluginError::new(
                "unexpected_frame",
                "host did not send start after negotiation",
            ));
        }
    };
    let mut subscription = match ChatSubscription::open(backend, &request) {
        Ok(subscription) => subscription,
        Err(error) => {
            let error = subscription_failure(error);
            send_backend_error(&mut io, &error, true)?;
            return Ok(());
        }
    };
    let result = io
        .send(&ServerFrame::Subscribed)
        .and_then(|()| run_subscription(&mut subscription, io));
    let close_result = subscription
        .close()
        .map_err(|error| PluginError::new("backend_close", error.to_string()));
    match (result, close_result) {
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Host one object-safe backend on a bounded framed connection.
///
/// This compatibility entry point accepts every [`Read`] implementation and retains the original
/// sequential protocol behavior. It cannot observe a host Close while the backend is blocked in
/// `next_item`; process plugins and other hosts that require concurrent semantic Close should use
/// [`serve_interruptible`] with an independently cancellable input.
///
/// # Errors
///
/// Returns [`PluginError`] for malformed frames, incompatible versions, or connection failures.
pub fn serve<R: Read, W: Write>(
    backend: &mut dyn ChatSubscriptionBackend,
    reader: R,
    writer: W,
) -> Result<(), PluginError> {
    serve_with(backend, reader, writer, |subscription, mut io| {
        serve_subscription_legacy(subscription, &mut io)
    })
}

/// Host one backend with a concurrent, independently cancellable host-frame reader.
///
/// This variant makes semantic Close reachable while provider `next_item` blocks and also retires
/// its reader actor after natural provider End. The input's cancellation contract is stronger than
/// ordinary [`Read`], so callers must opt in explicitly instead of changing [`serve`]'s public API.
///
/// # Errors
///
/// Returns [`PluginError`] for malformed frames, incompatible versions, cancellation failures, or
/// connection failures.
pub fn serve_interruptible<R: HostFrameInput, W: Write>(
    backend: &mut dyn ChatSubscriptionBackend,
    reader: R,
    writer: W,
) -> Result<(), PluginError> {
    serve_with(backend, reader, writer, |subscription, io| {
        let FramedIo { reader, writer } = io;
        serve_subscription_interruptible(subscription, reader, writer)
    })
}

fn serve_subscription_legacy<R: Read, W: Write>(
    subscription: &mut ChatSubscription,
    io: &mut FramedIo<R, W>,
) -> Result<(), PluginError> {
    loop {
        let item = match subscription.next_item() {
            Ok(Some(item)) => item,
            Ok(None) => {
                io.send(&ServerFrame::End)?;
                return Ok(());
            }
            Err(error) => {
                let error = subscription_failure(error);
                send_backend_error(io, &error, true)?;
                return Ok(());
            }
        };
        io.send(&BorrowedServerFrame::Item {
            item: BorrowedWireItem::from(&item),
        })?;
        let SubscriptionItem::Batch(delivery) = item else {
            continue;
        };
        match io.receive::<ClientFrame>()? {
            Some(ClientFrame::Commit {
                sequence,
                delivery_id,
            }) if sequence == delivery.sequence().get()
                && delivery_id == delivery.delivery_id().as_str() =>
            {
                if let Err(error) = subscription.commit_durable(&delivery) {
                    let error = subscription_failure(error);
                    send_backend_error(io, &error, true)?;
                    return Ok(());
                }
                io.send(&ServerFrame::Committed { sequence })?;
            }
            Some(ClientFrame::Close) | None => return Ok(()),
            Some(ClientFrame::Commit { .. }) => {
                let error = BackendFailure::new(
                    "commit_mismatch",
                    "host commit does not match the outstanding delivery",
                    false,
                )
                .expect("constant backend failure is valid");
                send_backend_error(io, &error, true)?;
                return Ok(());
            }
            Some(_) => {
                return Err(PluginError::new(
                    "unexpected_frame",
                    "host sent a non-commit frame while delivery was outstanding",
                ));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadPhase {
    WaitingNext,
    NextActive,
    ExpectCommit,
    CommitReceived,
    CommitActive,
    Terminal,
}

struct HostReadState {
    phase: ReadPhase,
    terminal_intent: bool,
}

struct HostReadSynchronization {
    state: Mutex<HostReadState>,
    changed: Condvar,
}

impl HostReadSynchronization {
    fn new() -> Self {
        Self {
            state: Mutex::new(HostReadState {
                phase: ReadPhase::WaitingNext,
                terminal_intent: false,
            }),
            changed: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HostReadState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn admit_commit(&self) -> bool {
        let mut state = self.lock();
        if state.terminal_intent || state.phase != ReadPhase::ExpectCommit {
            return false;
        }
        state.phase = ReadPhase::CommitReceived;
        self.changed.notify_all();
        true
    }

    fn register_terminal_intent(&self) -> bool {
        let mut state = self.lock();
        state.terminal_intent = true;
        let cancel = matches!(state.phase, ReadPhase::WaitingNext | ReadPhase::NextActive);
        self.changed.notify_all();
        cancel
    }

    fn begin_next(&self) -> bool {
        let mut state = self.lock();
        if state.terminal_intent {
            return false;
        }
        debug_assert_eq!(state.phase, ReadPhase::WaitingNext);
        state.phase = ReadPhase::NextActive;
        self.changed.notify_all();
        true
    }

    fn finish_next(&self, next_phase: ReadPhase) -> bool {
        let mut state = self.lock();
        debug_assert_eq!(state.phase, ReadPhase::NextActive);
        if state.terminal_intent {
            return false;
        }
        state.phase = next_phase;
        self.changed.notify_all();
        true
    }

    fn set_phase(&self, phase: ReadPhase) {
        let mut state = self.lock();
        state.phase = phase;
        self.changed.notify_all();
    }
}

enum HostControl {
    Commit { sequence: u64, delivery_id: String },
    Close(Result<(), CancellationError>),
    Eof(Result<(), CancellationError>),
    Invalid(PluginError, Result<(), CancellationError>),
}

fn cancel_for_host_control(
    cancellation: &Arc<dyn ChatSubscriptionCancellation>,
) -> Result<(), CancellationError> {
    cancellation.cancel()
}

fn register_terminal_intent(
    synchronization: &HostReadSynchronization,
    cancellation: &Arc<dyn ChatSubscriptionCancellation>,
) -> Result<(), CancellationError> {
    if synchronization.register_terminal_intent() {
        cancel_for_host_control(cancellation)
    } else {
        Ok(())
    }
}

fn pump_host_frames<R: Read>(
    mut io: FramedIo<R, io::Sink>,
    synchronization: &HostReadSynchronization,
    cancellation: Arc<dyn ChatSubscriptionCancellation>,
    input_cancellation: HostFrameInputCancellation,
    controls: mpsc::Sender<HostControl>,
) {
    loop {
        let frame = io.receive::<ClientFrame>();
        if input_cancellation.is_cancelled() {
            return;
        }
        let control = match frame {
            Ok(Some(ClientFrame::Commit {
                sequence,
                delivery_id,
            })) if synchronization.admit_commit() => HostControl::Commit {
                sequence,
                delivery_id,
            },
            Ok(Some(ClientFrame::Close)) => {
                HostControl::Close(register_terminal_intent(synchronization, &cancellation))
            }
            Ok(None) => HostControl::Eof(register_terminal_intent(synchronization, &cancellation)),
            Ok(Some(frame)) => HostControl::Invalid(
                PluginError::new(
                    "unexpected_frame",
                    format!("host sent {frame:?} outside the commit phase"),
                ),
                register_terminal_intent(synchronization, &cancellation),
            ),
            Err(error) => HostControl::Invalid(
                error,
                register_terminal_intent(synchronization, &cancellation),
            ),
        };
        let terminal = !matches!(control, HostControl::Commit { .. });
        if controls.send(control).is_err() || terminal {
            return;
        }
    }
}

fn cancellation_control_result(result: Result<(), CancellationError>) -> Result<(), PluginError> {
    result.map_err(|error| PluginError::new("backend_cancel", error.to_string()))
}

fn terminal_host_control(control: HostControl) -> Result<(), PluginError> {
    match control {
        HostControl::Close(result) | HostControl::Eof(result) => {
            cancellation_control_result(result)
        }
        HostControl::Invalid(error, cancellation) => {
            cancellation_control_result(cancellation)?;
            Err(error)
        }
        HostControl::Commit { .. } => Err(PluginError::new(
            "unexpected_frame",
            "host sent Commit while no delivery was outstanding",
        )),
    }
}

fn serve_subscription_interruptible<R: HostFrameInput, W: Write>(
    subscription: &mut ChatSubscription,
    reader: R,
    writer: W,
) -> Result<(), PluginError> {
    serve_subscription_synchronized(
        subscription,
        reader,
        writer,
        Arc::new(HostReadSynchronization::new()),
    )
}

fn serve_subscription_synchronized<R: HostFrameInput, W: Write>(
    subscription: &mut ChatSubscription,
    reader: R,
    writer: W,
    synchronization: Arc<HostReadSynchronization>,
) -> Result<(), PluginError> {
    let cancellation = subscription.cancellation();
    let input_cancellation = reader.input_cancellation()?;
    let (control_sender, control_receiver) = mpsc::channel();
    let mut output = FramedIo::new(io::empty(), writer);
    thread::scope(|scope| {
        let input = FramedIo::new(reader, io::sink());
        let reader_cancellation = input_cancellation.clone();
        let reader = scope.spawn(|| {
            pump_host_frames(
                input,
                &synchronization,
                cancellation,
                reader_cancellation,
                control_sender,
            );
        });
        let result = loop {
            if !synchronization.begin_next() {
                let control = control_receiver.recv().map_err(|_| {
                    PluginError::new(
                        "host_control_stopped",
                        "host control reader stopped before the next receive",
                    )
                })?;
                break terminal_host_control(control);
            }
            let item = subscription.next_item();
            let item = match item {
                Ok(Some(item)) => {
                    let next_phase = if matches!(item, SubscriptionItem::Batch(_)) {
                        ReadPhase::ExpectCommit
                    } else {
                        ReadPhase::WaitingNext
                    };
                    if !synchronization.finish_next(next_phase) {
                        let control = control_receiver.recv().map_err(|_| {
                            PluginError::new(
                                "host_control_stopped",
                                "host control reader stopped during cancellation",
                            )
                        })?;
                        break terminal_host_control(control);
                    }
                    item
                }
                Ok(None) => {
                    if !synchronization.finish_next(ReadPhase::Terminal) {
                        let control = control_receiver.recv().map_err(|_| {
                            PluginError::new(
                                "host_control_stopped",
                                "host control reader stopped as the subscription ended",
                            )
                        })?;
                        break terminal_host_control(control);
                    }
                    output.send(&ServerFrame::End)?;
                    break Ok(());
                }
                Err(error) => {
                    if !synchronization.finish_next(ReadPhase::Terminal) {
                        let control = control_receiver.recv().map_err(|_| {
                            PluginError::new(
                                "host_control_stopped",
                                "host control reader stopped during cancellation",
                            )
                        })?;
                        break terminal_host_control(control);
                    }
                    let error = subscription_failure(error);
                    send_backend_error(&mut output, &error, true)?;
                    break Ok(());
                }
            };
            output.send(&BorrowedServerFrame::Item {
                item: BorrowedWireItem::from(&item),
            })?;
            let SubscriptionItem::Batch(delivery) = item else {
                continue;
            };
            match control_receiver.recv().map_err(|_| {
                PluginError::new(
                    "host_control_stopped",
                    "host control reader stopped while a delivery was outstanding",
                )
            })? {
                HostControl::Commit {
                    sequence,
                    delivery_id,
                } if sequence == delivery.sequence().get()
                    && delivery_id == delivery.delivery_id().as_str() =>
                {
                    synchronization.set_phase(ReadPhase::CommitActive);
                    if let Err(error) = subscription.commit_durable(&delivery) {
                        synchronization.set_phase(ReadPhase::Terminal);
                        let error = subscription_failure(error);
                        send_backend_error(&mut output, &error, true)?;
                        break Ok(());
                    }
                    output.send(&ServerFrame::Committed { sequence })?;
                    synchronization.set_phase(ReadPhase::WaitingNext);
                }
                HostControl::Commit { .. } => {
                    synchronization.set_phase(ReadPhase::Terminal);
                    let error = BackendFailure::new(
                        "commit_mismatch",
                        "host commit does not match the outstanding delivery",
                        false,
                    )
                    .expect("constant backend failure is valid");
                    send_backend_error(&mut output, &error, true)?;
                    break Ok(());
                }
                control => break terminal_host_control(control),
            }
        };
        synchronization.set_phase(ReadPhase::Terminal);
        input_cancellation.cancel();
        let reader_result = reader
            .join()
            .map_err(|_| PluginError::new("host_control_panicked", "host control reader panicked"));
        match (result, reader_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    })
}

/// Host a backend on this process's standard input and output.
///
/// # Errors
///
/// Returns [`PluginError`] as [`serve_interruptible`] on Linux and [`serve`] elsewhere.
pub fn serve_stdio(backend: &mut dyn ChatSubscriptionBackend) -> Result<(), PluginError> {
    #[cfg(target_os = "linux")]
    {
        let input = interruptible_fd_input::InterruptibleFdInput::stdin()?;
        serve_interruptible(backend, input, io::stdout().lock())
    }
    #[cfg(not(target_os = "linux"))]
    {
        serve(backend, io::stdin().lock(), io::stdout().lock())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chat_subscription::{
        CancellationError, ChatSubscriptionCancellation, ChatSubscriptionDriver,
    };
    use std::io::Cursor;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    struct ProtocolBackend {
        input: Option<Vec<u8>>,
    }

    struct LegacyReadOnlyInput {
        inner: Cursor<Vec<u8>>,
        _not_send: Rc<()>,
    }

    impl Read for LegacyReadOnlyInput {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.inner.read(bytes)
        }
    }

    struct NoopCancellation;

    impl ChatSubscriptionCancellation for NoopCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            Ok(())
        }
    }

    fn noop_cancellation() -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(NoopCancellation)
    }

    struct ProtocolDriver(PluginDriver<Cursor<Vec<u8>>, Vec<u8>>);

    impl ChatSubscriptionDriver for ProtocolDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            self.0.next_item()
        }

        fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            self.0.acknowledge(delivery_id)
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            self.0.close()
        }
    }

    impl ChatSubscriptionBackend for ProtocolBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "protocol-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(ProtocolDriver(PluginDriver {
                io: FramedIo::new(
                    Cursor::new(self.input.take().expect("one subscription")),
                    Vec::<u8>::new(),
                ),
                pending: None,
                terminal: false,
            })))
        }
    }

    fn fixture_request() -> SubscribeRequest {
        SubscribeRequest::new(
            vec![ChannelId::new("channels/one").expect("channel")],
            vec![SenderId::new("users/owner").expect("sender")],
            Some(ProviderCursor::new("cursor-before").expect("cursor")),
        )
        .expect("fixture request")
    }

    fn frame(raw: &[u8]) -> Vec<u8> {
        let mut framed = Vec::with_capacity(raw.len() + 4);
        framed.extend_from_slice(
            &u32::try_from(raw.len())
                .expect("frame fits u32")
                .to_be_bytes(),
        );
        framed.extend_from_slice(raw);
        framed
    }

    fn append_frame<T: Serialize>(bytes: &mut Vec<u8>, value: &T) {
        let payload = serde_json::to_vec(value).expect("serialize fixture frame");
        bytes.extend(frame(&payload));
    }

    struct CloseCountingBackend {
        closes: Arc<AtomicUsize>,
        acknowledgements: Arc<AtomicUsize>,
    }

    impl ChatSubscriptionBackend for CloseCountingBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "close-counting-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Checkpoint, EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(CloseCountingDriver {
                closes: Arc::clone(&self.closes),
                acknowledgements: Arc::clone(&self.acknowledgements),
                emitted: false,
            }))
        }
    }

    struct CloseCountingDriver {
        closes: Arc<AtomicUsize>,
        acknowledgements: Arc<AtomicUsize>,
        emitted: bool,
    }

    struct NaturalEndBackend {
        closes: Arc<AtomicUsize>,
    }

    struct NaturalEndDriver {
        closes: Arc<AtomicUsize>,
        ended: bool,
    }

    impl ChatSubscriptionBackend for NaturalEndBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "natural-end-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(NaturalEndDriver {
                closes: Arc::clone(&self.closes),
                ended: false,
            }))
        }
    }

    impl ChatSubscriptionDriver for NaturalEndDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            assert!(!self.ended, "natural End is terminal");
            self.ended = true;
            Ok(None)
        }

        fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            panic!("natural End has no delivery to acknowledge")
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct BlockingCloseState {
        entered: bool,
        cancelled: bool,
        closed: bool,
    }

    struct BlockingCloseBackend {
        state: Arc<(Mutex<BlockingCloseState>, Condvar)>,
    }

    #[derive(Clone)]
    struct BlockingCloseCancellation {
        state: Arc<(Mutex<BlockingCloseState>, Condvar)>,
    }

    struct BlockingCloseDriver {
        state: Arc<(Mutex<BlockingCloseState>, Condvar)>,
    }

    #[derive(Default)]
    struct CommitCloseState {
        next_calls: usize,
        commit_entered: bool,
        release_commit: bool,
        cancellations: usize,
        closes: usize,
    }

    struct CommitCloseBackend {
        state: Arc<(Mutex<CommitCloseState>, Condvar)>,
    }

    #[derive(Clone)]
    struct CommitCloseCancellation {
        state: Arc<(Mutex<CommitCloseState>, Condvar)>,
    }

    struct CommitCloseDriver {
        state: Arc<(Mutex<CommitCloseState>, Condvar)>,
    }

    #[derive(Default)]
    struct WaitingBoundaryState {
        next_calls: usize,
        cancellations: usize,
        closes: usize,
    }

    struct WaitingBoundaryBackend {
        state: Arc<(Mutex<WaitingBoundaryState>, Condvar)>,
    }

    #[derive(Clone)]
    struct WaitingBoundaryCancellation {
        state: Arc<(Mutex<WaitingBoundaryState>, Condvar)>,
    }

    struct WaitingBoundaryDriver {
        state: Arc<(Mutex<WaitingBoundaryState>, Condvar)>,
    }

    #[derive(Clone, Copy, Debug)]
    enum TerminalInput {
        Close,
        Eof,
        Invalid,
    }

    #[derive(Default)]
    struct FrameWriteBarrierState {
        frames: usize,
        reached: bool,
        released: bool,
    }

    struct FrameWriteBarrier {
        writer: UnixStream,
        target_frame: usize,
        state: Arc<(Mutex<FrameWriteBarrierState>, Condvar)>,
    }

    impl ChatSubscriptionCancellation for BlockingCloseCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("blocking close state");
            state.cancelled = true;
            changed.notify_all();
            Ok(())
        }
    }

    impl ChatSubscriptionBackend for BlockingCloseBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(BlockingCloseCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "blocking-close-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(BlockingCloseDriver {
                state: Arc::clone(&self.state),
            }))
        }
    }

    impl ChatSubscriptionDriver for BlockingCloseDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(BlockingCloseCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("blocking close state");
            state.entered = true;
            changed.notify_all();
            while !state.cancelled {
                state = changed.wait(state).expect("blocking close wait");
            }
            Err(BackendFailure::new(
                "fixture_cancelled",
                "fixture receive was interrupted by Close",
                true,
            )
            .expect("fixture cancellation failure"))
        }

        fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            Ok(())
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("blocking close state");
            assert!(state.cancelled, "Close must follow receive cancellation");
            state.closed = true;
            changed.notify_all();
            Ok(())
        }
    }

    impl ChatSubscriptionCancellation for CommitCloseCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            let (state, changed) = &*self.state;
            state.lock().expect("commit-close state").cancellations += 1;
            changed.notify_all();
            Ok(())
        }
    }

    impl ChatSubscriptionBackend for CommitCloseBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(CommitCloseCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "commit-close-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Checkpoint, EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(CommitCloseDriver {
                state: Arc::clone(&self.state),
            }))
        }
    }

    impl ChatSubscriptionDriver for CommitCloseDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(CommitCloseCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            let (state, _) = &*self.state;
            let mut state = state.lock().expect("commit-close state");
            state.next_calls += 1;
            assert_eq!(
                state.next_calls, 1,
                "Close must be consumed before another Next"
            );
            drop(state);
            DeliveryBatch::new(
                EventSequence::new(1).expect("sequence"),
                ProviderCursor::new("cursor-one").expect("cursor"),
                DeliveryId::new("delivery-one").expect("delivery"),
                vec![CommittableEvent::Checkpoint],
            )
            .map(SubscriptionItem::Batch)
            .map(Some)
            .map_err(|error| {
                BackendFailure::new("fixture", error.to_string(), false)
                    .expect("fixture diagnostic")
            })
        }

        fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            assert_eq!(delivery_id.as_str(), "delivery-one");
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("commit-close state");
            state.commit_entered = true;
            changed.notify_all();
            while !state.release_commit {
                state = changed.wait(state).expect("commit-close wait");
            }
            Ok(())
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            self.state.0.lock().expect("commit-close state").closes += 1;
            Ok(())
        }
    }

    impl ChatSubscriptionCancellation for WaitingBoundaryCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("waiting-boundary state");
            state.cancellations += 1;
            changed.notify_all();
            Ok(())
        }
    }

    impl ChatSubscriptionBackend for WaitingBoundaryBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(WaitingBoundaryCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities::new(
                "waiting-boundary-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Heartbeat],
            )
            .expect("fixture capabilities")
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(WaitingBoundaryDriver {
                state: Arc::clone(&self.state),
            }))
        }
    }

    impl ChatSubscriptionDriver for WaitingBoundaryDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(WaitingBoundaryCancellation {
                state: Arc::clone(&self.state),
            })
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("waiting-boundary state");
            state.next_calls += 1;
            assert_eq!(
                state.next_calls, 1,
                "terminal intent must be consumed before another Next"
            );
            changed.notify_all();
            Ok(Some(SubscriptionItem::Heartbeat(Heartbeat::new(
                EventSequence::new(1).expect("sequence"),
            ))))
        }

        fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            panic!("heartbeat fixture cannot be acknowledged")
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            let (state, changed) = &*self.state;
            state.lock().expect("waiting-boundary state").closes += 1;
            changed.notify_all();
            Ok(())
        }
    }

    impl Write for FrameWriteBarrier {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writer.write_all(bytes)?;
            let (state, changed) = &*self.state;
            let mut state = state.lock().expect("frame-write barrier state");
            state.frames += 1;
            if state.frames == self.target_frame {
                state.reached = true;
                changed.notify_all();
                while !state.released {
                    state = changed.wait(state).expect("frame-write barrier wait");
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.writer.flush()
        }
    }

    fn wait_for_write_barrier(state: &Arc<(Mutex<FrameWriteBarrierState>, Condvar)>) {
        let (state, changed) = &**state;
        let state = state.lock().expect("frame-write barrier state");
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.reached)
            .expect("frame-write barrier wait");
        assert!(!timeout.timed_out(), "server did not reach write barrier");
        assert!(state.reached);
    }

    fn release_write_barrier(state: &Arc<(Mutex<FrameWriteBarrierState>, Condvar)>) {
        let (state, changed) = &**state;
        state.lock().expect("frame-write barrier state").released = true;
        changed.notify_all();
    }

    fn wait_for_terminal_intent(
        synchronization: &HostReadSynchronization,
        expected_phase: ReadPhase,
    ) {
        let state = synchronization.lock();
        let (state, timeout) = synchronization
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| {
                !state.terminal_intent || state.phase != expected_phase
            })
            .expect("host-read synchronization wait");
        assert!(
            !timeout.timed_out(),
            "terminal intent was not registered in {expected_phase:?}; observed {:?}",
            state.phase
        );
        assert!(state.terminal_intent);
        assert_eq!(state.phase, expected_phase);
    }

    fn send_terminal_input(input: TerminalInput, client: &mut FramedIo<UnixStream, UnixStream>) {
        match input {
            TerminalInput::Close => client.send(&ClientFrame::Close).expect("send Close"),
            TerminalInput::Eof => client
                .writer
                .shutdown(Shutdown::Write)
                .expect("close client write half"),
            TerminalInput::Invalid => client
                .send(&ClientFrame::Hello {
                    min_version: PROTOCOL_VERSION,
                    max_version: PROTOCOL_VERSION,
                })
                .expect("send invalid control frame"),
        }
    }

    fn assert_terminal_result(input: TerminalInput, result: Result<(), PluginError>) {
        match input {
            TerminalInput::Close | TerminalInput::Eof => {
                result.expect("Close and EOF are clean terminal controls")
            }
            TerminalInput::Invalid => {
                let error = result.expect_err("invalid control must fail the protocol");
                assert_eq!(error.code(), "unexpected_frame");
            }
        }
    }

    struct FailAfterFirstFrame {
        first_frame_flushed: bool,
    }

    struct AlwaysReadError;

    impl Read for AlwaysReadError {
        fn read(&mut self, _bytes: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "fixture confirmation read failed",
            ))
        }
    }

    struct PartialCommitWriter {
        remaining: usize,
    }

    impl Write for PartialCommitWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "fixture partial commit write failed",
                ));
            }
            let written = self.remaining.min(bytes.len());
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for FailAfterFirstFrame {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.first_frame_flushed {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "fixture refuses the subscribed frame",
                ))
            } else {
                Ok(bytes.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            self.first_frame_flushed = true;
            Ok(())
        }
    }

    impl ChatSubscriptionDriver for CloseCountingDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            assert!(
                !self.emitted,
                "fixture must be cancelled before another read"
            );
            self.emitted = true;
            DeliveryBatch::new(
                EventSequence::new(1).expect("sequence"),
                ProviderCursor::new("cursor-one").expect("cursor"),
                DeliveryId::new("delivery-one").expect("delivery"),
                vec![CommittableEvent::Checkpoint],
            )
            .map(SubscriptionItem::Batch)
            .map(Some)
            .map_err(|error| {
                BackendFailure::new("fixture", error.to_string(), false)
                    .expect("fixture diagnostic")
            })
        }

        fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            self.acknowledgements.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn run_server_cancellation(include_close_frame: bool) -> (usize, usize) {
        let closes = Arc::new(AtomicUsize::new(0));
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let mut backend = CloseCountingBackend {
            closes: Arc::clone(&closes),
            acknowledgements: Arc::clone(&acknowledgements),
        };
        let mut input = Vec::new();
        append_frame(
            &mut input,
            &ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            },
        );
        append_frame(
            &mut input,
            &ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            },
        );
        if include_close_frame {
            append_frame(&mut input, &ClientFrame::Close);
        }

        serve(&mut backend, Cursor::new(input), Vec::<u8>::new())
            .expect("host cancellation is clean");
        (
            closes.load(Ordering::SeqCst),
            acknowledgements.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn legacy_serve_accepts_a_read_only_non_send_input() {
        let closes = Arc::new(AtomicUsize::new(0));
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let mut backend = CloseCountingBackend {
            closes: Arc::clone(&closes),
            acknowledgements: Arc::clone(&acknowledgements),
        };
        let mut input = Vec::new();
        append_frame(
            &mut input,
            &ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            },
        );
        append_frame(
            &mut input,
            &ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            },
        );
        append_frame(&mut input, &ClientFrame::Close);
        serve(
            &mut backend,
            LegacyReadOnlyInput {
                inner: Cursor::new(input),
                _not_send: Rc::new(()),
            },
            Vec::<u8>::new(),
        )
        .expect("legacy generic serve remains compatible");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(acknowledgements.load(Ordering::SeqCst), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eventfd_cancellation_wake_retries_eintr_then_succeeds() {
        let mut attempts = 0_u8;
        interruptible_fd_input::signal_eventfd_with(|| {
            attempts = attempts.saturating_add(1);
            if attempts == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(())
            }
        });
        assert_eq!(attempts, 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eventfd_cancellation_wake_retries_eintr_then_accepts_saturation() {
        let mut attempts = 0_u8;
        interruptible_fd_input::signal_eventfd_with(|| {
            attempts = attempts.saturating_add(1);
            if attempts == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        });
        assert_eq!(attempts, 2);
    }

    #[test]
    fn natural_end_interrupts_and_joins_host_input_actor_with_peer_open() {
        let closes = Arc::new(AtomicUsize::new(0));
        let (client, server) = UnixStream::pair().expect("create protocol socket pair");
        let server_reader = server.try_clone().expect("clone server reader");
        let server_closes = Arc::clone(&closes);
        let (completed, completed_receiver) = mpsc::sync_channel(1);
        let server_thread = thread::spawn(move || {
            let mut backend = NaturalEndBackend {
                closes: server_closes,
            };
            let result = serve_interruptible(&mut backend, server_reader, server);
            completed
                .send(result)
                .expect("publish natural-End server result");
        });
        let client_reader = client.try_clone().expect("clone client reader");
        let mut client = FramedIo::new(client_reader, client);
        client
            .send(&ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            })
            .expect("send Hello");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Hello"),
            Some(ServerFrame::Hello { .. })
        ));
        client
            .send(&ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            })
            .expect("send Start");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Subscribed"),
            Some(ServerFrame::Subscribed)
        ));
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive End"),
            Some(ServerFrame::End)
        ));

        completed_receiver
            .recv_timeout(Duration::from_millis(250))
            .expect("server joins its input actor without peer EOF")
            .expect("natural End is clean");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        drop(client);
        server_thread.join().expect("server thread joins");
    }

    #[test]
    fn server_close_frame_reaches_provider_without_acknowledging() {
        assert_eq!(run_server_cancellation(true), (1, 0));
    }

    #[test]
    fn server_eof_reaches_provider_without_acknowledging() {
        assert_eq!(run_server_cancellation(false), (1, 0));
    }

    #[test]
    fn close_interrupts_blocked_next_and_runs_semantic_driver_cleanup() {
        let state = Arc::new((Mutex::new(BlockingCloseState::default()), Condvar::new()));
        let server_state = Arc::clone(&state);
        let (client, server) = UnixStream::pair().expect("create protocol socket pair");
        let server_reader = server.try_clone().expect("clone server reader");
        let server_thread = thread::spawn(move || {
            let mut backend = BlockingCloseBackend {
                state: server_state,
            };
            serve_interruptible(&mut backend, server_reader, server)
        });
        let client_reader = client.try_clone().expect("clone client reader");
        let mut client = FramedIo::new(client_reader, client);
        client
            .send(&ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            })
            .expect("send Hello");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Hello"),
            Some(ServerFrame::Hello { .. })
        ));
        client
            .send(&ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            })
            .expect("send Start");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Subscribed"),
            Some(ServerFrame::Subscribed)
        ));
        let (state_lock, changed) = &*state;
        let mut observed = state_lock.lock().expect("blocking close state");
        while !observed.entered {
            observed = changed.wait(observed).expect("wait for blocked Next");
        }
        drop(observed);

        client.send(&ClientFrame::Close).expect("send Close");
        server_thread
            .join()
            .expect("server thread joins")
            .expect("Close is clean");
        let observed = state_lock.lock().expect("blocking close state");
        assert!(
            observed.cancelled,
            "Close must interrupt the blocked receive"
        );
        assert!(
            observed.closed,
            "semantic driver cleanup must complete before serve returns"
        );
    }

    #[test]
    fn close_during_commit_queues_without_cancelling_and_prevents_another_next() {
        let state = Arc::new((Mutex::new(CommitCloseState::default()), Condvar::new()));
        let server_state = Arc::clone(&state);
        let (client, server) = UnixStream::pair().expect("create protocol socket pair");
        let server_reader = server.try_clone().expect("clone server reader");
        let server_thread = thread::spawn(move || {
            let mut backend = CommitCloseBackend {
                state: server_state,
            };
            serve_interruptible(&mut backend, server_reader, server)
        });
        let client_reader = client.try_clone().expect("clone client reader");
        let mut client = FramedIo::new(client_reader, client);
        client
            .send(&ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            })
            .expect("send Hello");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Hello"),
            Some(ServerFrame::Hello { .. })
        ));
        client
            .send(&ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            })
            .expect("send Start");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Subscribed"),
            Some(ServerFrame::Subscribed)
        ));
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive delivery"),
            Some(ServerFrame::Item { .. })
        ));
        client
            .send(&ClientFrame::Commit {
                sequence: 1,
                delivery_id: "delivery-one".to_owned(),
            })
            .expect("send Commit");
        let (state_lock, changed) = &*state;
        let mut observed = state_lock.lock().expect("commit-close state");
        while !observed.commit_entered {
            observed = changed.wait(observed).expect("wait for Commit");
        }
        drop(observed);
        client
            .send(&ClientFrame::Close)
            .expect("queue Close during Commit");
        thread::sleep(Duration::from_millis(20));
        let mut observed = state_lock.lock().expect("commit-close state");
        assert_eq!(observed.cancellations, 0, "Close cannot cancel Commit");
        assert_eq!(observed.closes, 0, "semantic Close waits for Commit");
        assert_eq!(observed.next_calls, 1);
        observed.release_commit = true;
        changed.notify_all();
        drop(observed);

        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Committed"),
            Some(ServerFrame::Committed { sequence: 1 })
        ));
        server_thread
            .join()
            .expect("server thread joins")
            .expect("queued Close is clean");
        let observed = state_lock.lock().expect("commit-close state");
        assert_eq!(observed.cancellations, 0);
        assert_eq!(observed.closes, 1);
        assert_eq!(observed.next_calls, 1);
    }

    #[test]
    fn every_terminal_control_at_commit_tail_is_consumed_before_another_next() {
        for input in [
            TerminalInput::Close,
            TerminalInput::Eof,
            TerminalInput::Invalid,
        ] {
            let state = Arc::new((
                Mutex::new(CommitCloseState {
                    release_commit: true,
                    ..CommitCloseState::default()
                }),
                Condvar::new(),
            ));
            let synchronization = Arc::new(HostReadSynchronization::new());
            let write_barrier = Arc::new((
                Mutex::new(FrameWriteBarrierState::default()),
                Condvar::new(),
            ));
            let server_state = Arc::clone(&state);
            let server_synchronization = Arc::clone(&synchronization);
            let server_write_barrier = Arc::clone(&write_barrier);
            let (client, server) = UnixStream::pair().expect("create protocol socket pair");
            let server_reader = server.try_clone().expect("clone server reader");
            let server_thread = thread::spawn(move || {
                let mut backend = CommitCloseBackend {
                    state: server_state,
                };
                let mut subscription =
                    ChatSubscription::open(&mut backend, &fixture_request()).expect("subscribe");
                let result = serve_subscription_synchronized(
                    &mut subscription,
                    server_reader,
                    FrameWriteBarrier {
                        writer: server,
                        target_frame: 2,
                        state: server_write_barrier,
                    },
                    server_synchronization,
                );
                subscription.close().expect("semantic Close");
                result
            });
            let client_reader = client.try_clone().expect("clone client reader");
            let mut client = FramedIo::new(client_reader, client);
            assert!(matches!(
                client.receive::<ServerFrame>().expect("receive delivery"),
                Some(ServerFrame::Item { .. })
            ));
            client
                .send(&ClientFrame::Commit {
                    sequence: 1,
                    delivery_id: "delivery-one".to_owned(),
                })
                .expect("send Commit");
            wait_for_write_barrier(&write_barrier);

            send_terminal_input(input, &mut client);
            wait_for_terminal_intent(&synchronization, ReadPhase::CommitActive);
            assert_eq!(
                state.0.lock().expect("commit-tail state").cancellations,
                0,
                "{input:?} must not cancel an active Commit"
            );
            release_write_barrier(&write_barrier);

            assert!(matches!(
                client.receive::<ServerFrame>().expect("receive Committed"),
                Some(ServerFrame::Committed { sequence: 1 })
            ));
            let result = server_thread.join().expect("server thread joins");
            assert_terminal_result(input, result);
            let observed = state.0.lock().expect("commit-tail state");
            assert!(observed.commit_entered);
            assert_eq!(observed.cancellations, 0);
            assert_eq!(observed.closes, 1);
            assert_eq!(observed.next_calls, 1);
        }
    }

    #[test]
    fn every_terminal_control_at_waiting_next_arms_sticky_cancellation() {
        for input in [
            TerminalInput::Close,
            TerminalInput::Eof,
            TerminalInput::Invalid,
        ] {
            let state = Arc::new((Mutex::new(WaitingBoundaryState::default()), Condvar::new()));
            let synchronization = Arc::new(HostReadSynchronization::new());
            let write_barrier = Arc::new((
                Mutex::new(FrameWriteBarrierState::default()),
                Condvar::new(),
            ));
            let server_state = Arc::clone(&state);
            let server_synchronization = Arc::clone(&synchronization);
            let server_write_barrier = Arc::clone(&write_barrier);
            let (client, server) = UnixStream::pair().expect("create protocol socket pair");
            let server_reader = server.try_clone().expect("clone server reader");
            let server_thread = thread::spawn(move || {
                let mut backend = WaitingBoundaryBackend {
                    state: server_state,
                };
                let mut subscription =
                    ChatSubscription::open(&mut backend, &fixture_request()).expect("subscribe");
                let result = serve_subscription_synchronized(
                    &mut subscription,
                    server_reader,
                    FrameWriteBarrier {
                        writer: server,
                        target_frame: 1,
                        state: server_write_barrier,
                    },
                    server_synchronization,
                );
                subscription.close().expect("semantic Close");
                result
            });
            let client_reader = client.try_clone().expect("clone client reader");
            let mut client = FramedIo::new(client_reader, client);
            wait_for_write_barrier(&write_barrier);
            {
                let observed = synchronization.lock();
                assert_eq!(observed.phase, ReadPhase::WaitingNext);
                assert!(!observed.terminal_intent);
            }

            send_terminal_input(input, &mut client);
            wait_for_terminal_intent(&synchronization, ReadPhase::WaitingNext);
            {
                let (state_lock, changed) = &*state;
                let observed = state_lock.lock().expect("waiting-boundary state");
                let (observed, timeout) = changed
                    .wait_timeout_while(observed, Duration::from_secs(2), |state| {
                        state.cancellations == 0
                    })
                    .expect("wait for sticky cancellation");
                assert!(
                    !timeout.timed_out(),
                    "{input:?} did not arm sticky cancellation"
                );
                assert_eq!(observed.cancellations, 1);
            }
            release_write_barrier(&write_barrier);

            assert!(matches!(
                client.receive::<ServerFrame>().expect("receive heartbeat"),
                Some(ServerFrame::Item {
                    item: WireItem::Heartbeat { sequence: 1 }
                })
            ));
            let result = server_thread.join().expect("server thread joins");
            assert_terminal_result(input, result);
            let observed = state.0.lock().expect("waiting-boundary state");
            assert_eq!(observed.cancellations, 1);
            assert_eq!(observed.closes, 1);
            assert_eq!(observed.next_calls, 1);
        }
    }

    #[test]
    fn duplicate_commit_is_rejected_without_overlapping_commit_or_starting_next() {
        let state = Arc::new((Mutex::new(CommitCloseState::default()), Condvar::new()));
        let server_state = Arc::clone(&state);
        let (client, server) = UnixStream::pair().expect("create protocol socket pair");
        let server_reader = server.try_clone().expect("clone server reader");
        let server_thread = thread::spawn(move || {
            let mut backend = CommitCloseBackend {
                state: server_state,
            };
            serve_interruptible(&mut backend, server_reader, server)
        });
        let client_reader = client.try_clone().expect("clone client reader");
        let mut client = FramedIo::new(client_reader, client);
        client
            .send(&ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            })
            .expect("send Hello");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Hello"),
            Some(ServerFrame::Hello { .. })
        ));
        client
            .send(&ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            })
            .expect("send Start");
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive Subscribed"),
            Some(ServerFrame::Subscribed)
        ));
        assert!(matches!(
            client.receive::<ServerFrame>().expect("receive delivery"),
            Some(ServerFrame::Item { .. })
        ));
        let commit = ClientFrame::Commit {
            sequence: 1,
            delivery_id: "delivery-one".to_owned(),
        };
        client.send(&commit).expect("send first Commit");
        let (state_lock, changed) = &*state;
        let mut observed = state_lock.lock().expect("commit-close state");
        while !observed.commit_entered {
            observed = changed.wait(observed).expect("wait for Commit");
        }
        drop(observed);
        client.send(&commit).expect("send duplicate Commit");
        thread::sleep(Duration::from_millis(20));
        let mut observed = state_lock.lock().expect("commit-close state");
        assert_eq!(observed.next_calls, 1);
        observed.release_commit = true;
        changed.notify_all();
        drop(observed);
        assert!(matches!(
            client
                .receive::<ServerFrame>()
                .expect("receive first Committed"),
            Some(ServerFrame::Committed { sequence: 1 })
        ));
        let error = server_thread
            .join()
            .expect("server thread joins")
            .expect_err("duplicate Commit is a protocol failure");
        assert_eq!(error.code(), "unexpected_frame");
        let observed = state_lock.lock().expect("commit-close state");
        assert_eq!(observed.cancellations, 0);
        assert_eq!(observed.closes, 1);
        assert_eq!(observed.next_calls, 1);
    }

    #[test]
    fn subscribed_write_failure_closes_open_provider_without_acknowledging() {
        let closes = Arc::new(AtomicUsize::new(0));
        let acknowledgements = Arc::new(AtomicUsize::new(0));
        let mut backend = CloseCountingBackend {
            closes: Arc::clone(&closes),
            acknowledgements: Arc::clone(&acknowledgements),
        };
        let mut input = Vec::new();
        append_frame(
            &mut input,
            &ClientFrame::Hello {
                min_version: PROTOCOL_VERSION,
                max_version: PROTOCOL_VERSION,
            },
        );
        append_frame(
            &mut input,
            &ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-zero".to_owned()),
                backend_configuration: None,
            },
        );

        let error = serve(
            &mut backend,
            Cursor::new(input),
            FailAfterFirstFrame {
                first_frame_flushed: false,
            },
        )
        .expect_err("subscribed write failure is reported");
        assert_eq!(error.code(), "plugin_io");
        assert_eq!(closes.load(Ordering::SeqCst), 1);
        assert_eq!(acknowledgements.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inbound_length_is_refused_before_payload_allocation() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(
            &(u32::try_from(MAX_FRAME_BYTES).expect("fits u32") + 1).to_be_bytes(),
        );
        let mut io = FramedIo::new(Cursor::new(bytes), Vec::new());
        let error = io
            .receive::<ClientFrame>()
            .expect_err("oversized frame must be refused");
        assert_eq!(error.code(), "frame_limit_exceeded");
    }

    #[test]
    fn truncated_payload_is_not_clean_eof() {
        let bytes = [0_u8, 0, 0, 4, b'{'];
        let mut io = FramedIo::new(Cursor::new(bytes), Vec::new());
        let error = io
            .receive::<ClientFrame>()
            .expect_err("truncated payload must fail");
        assert_eq!(error.code(), "truncated_frame");
    }

    #[test]
    fn empty_stream_is_clean_eof() {
        let mut io = FramedIo::new(Cursor::new(Vec::<u8>::new()), Vec::new());
        assert!(io
            .receive::<ClientFrame>()
            .expect("empty stream is readable")
            .is_none());
    }

    #[test]
    fn explicit_end_is_the_only_stable_clean_termination() {
        let payload = serde_json::to_vec(&ServerFrame::End).expect("serialize end");
        let mut backend = ProtocolBackend {
            input: Some(frame(&payload)),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &fixture_request()).expect("subscription opens");
        assert_eq!(subscription.next_item().expect("explicit end"), None);
        assert_eq!(subscription.next_item().expect("stable end"), None);
        assert!(!subscription.is_poisoned());
    }

    #[test]
    fn raw_eof_is_retryable_failure_and_poisons_core_generation() {
        let mut backend = ProtocolBackend {
            input: Some(Vec::new()),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &fixture_request()).expect("subscription opens");
        let error = subscription
            .next_item()
            .expect_err("EOF without end is not graceful");
        let SubscriptionError::Backend(failure) = error else {
            panic!("expected backend failure, found {error:?}");
        };
        assert_eq!(failure.code(), "unexpected_eof");
        assert!(failure.retryable());
        assert!(subscription.is_poisoned());
    }

    #[test]
    fn deterministic_protocol_violations_are_not_retryable() {
        let cases = [
            (
                frame(br#"{"type":"item","item":{"kind":"heartbeat","sequence":0}}"#),
                "invalid_item",
            ),
            (
                frame(br#"{"type":"item","item":not-json}"#),
                "invalid_frame",
            ),
            (
                (u32::try_from(MAX_FRAME_BYTES).expect("frame bound fits u32") + 1)
                    .to_be_bytes()
                    .to_vec(),
                "frame_limit_exceeded",
            ),
        ];
        for (input, expected_code) in cases {
            let mut backend = ProtocolBackend { input: Some(input) };
            let mut subscription = ChatSubscription::open(&mut backend, &fixture_request())
                .expect("subscription opens");
            let error = subscription
                .next_item()
                .expect_err("invalid protocol input must poison the generation");
            let SubscriptionError::Backend(failure) = error else {
                panic!("expected backend failure, found {error:?}");
            };
            assert_eq!(failure.code(), expected_code);
            assert!(!failure.retryable());
            assert!(subscription.is_poisoned());
        }
    }

    #[test]
    fn close_is_a_public_single_frame_cancellation_path() {
        let mut driver = PluginDriver {
            io: FramedIo::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new()),
            pending: Some((
                EventSequence::new(7).expect("sequence"),
                DeliveryId::new("delivery-seven").expect("delivery"),
            )),
            terminal: false,
        };
        driver.close().expect("close frame is written");
        let written_after_first = driver.io.writer.len();
        driver.close().expect("repeated close is idempotent");
        assert_eq!(driver.io.writer.len(), written_after_first);
        assert!(driver.pending.is_none());
        assert!(driver.terminal);

        let mut reader = FramedIo::new(Cursor::new(&driver.io.writer), Vec::<u8>::new());
        assert!(matches!(
            reader.receive::<ClientFrame>().expect("decode close"),
            Some(ClientFrame::Close)
        ));
        assert!(reader
            .receive::<ClientFrame>()
            .expect("no duplicate close")
            .is_none());
    }

    fn pending_commit() -> (EventSequence, DeliveryId) {
        (
            EventSequence::new(7).expect("sequence"),
            DeliveryId::new("delivery-seven").expect("delivery"),
        )
    }

    #[test]
    fn every_non_exact_commit_confirmation_preserves_pending_and_is_unknown() {
        let delivery_id = pending_commit().1;

        let mut partial_write = PluginDriver {
            io: FramedIo::new(
                Cursor::new(Vec::<u8>::new()),
                PartialCommitWriter { remaining: 2 },
            ),
            pending: Some(pending_commit()),
            terminal: false,
        };
        let error = partial_write
            .acknowledge(&delivery_id)
            .expect_err("partial Commit write is ambiguous");
        assert_eq!(error.code(), "commit_outcome_unknown");
        assert!(error.detail().contains("partial commit write failed"));
        assert_eq!(partial_write.pending.as_ref(), Some(&pending_commit()));

        let mut read_error = PluginDriver {
            io: FramedIo::new(AlwaysReadError, Vec::<u8>::new()),
            pending: Some(pending_commit()),
            terminal: false,
        };
        let error = read_error
            .acknowledge(&delivery_id)
            .expect_err("Commit confirmation I/O failure is ambiguous");
        assert_eq!(error.code(), "commit_outcome_unknown");
        assert!(error.detail().contains("confirmation read failed"));
        assert_eq!(read_error.pending.as_ref(), Some(&pending_commit()));

        let mut eof = PluginDriver {
            io: FramedIo::new(Cursor::new(Vec::<u8>::new()), Vec::<u8>::new()),
            pending: Some(pending_commit()),
            terminal: false,
        };
        let error = eof
            .acknowledge(&delivery_id)
            .expect_err("EOF after Commit is ambiguous");
        assert_eq!(error.code(), "commit_outcome_unknown");
        assert!(error.detail().contains("before confirming"));
        assert_eq!(eof.pending.as_ref(), Some(&pending_commit()));

        let cases = [
            (
                ServerFrame::Error {
                    code: "provider_failed".to_owned(),
                    detail: "remote diagnostic".to_owned(),
                    retryable: false,
                    fatal: true,
                },
                "remote diagnostic",
            ),
            (ServerFrame::Committed { sequence: 8 }, "non-matching"),
            (ServerFrame::End, "non-matching"),
        ];
        for (confirmation, expected_detail) in cases {
            let mut input = Vec::new();
            append_frame(&mut input, &confirmation);
            let mut driver = PluginDriver {
                io: FramedIo::new(Cursor::new(input), Vec::<u8>::new()),
                pending: Some(pending_commit()),
                terminal: false,
            };
            let error = driver
                .acknowledge(&delivery_id)
                .expect_err("non-exact Commit response is ambiguous");
            assert_eq!(error.code(), "commit_outcome_unknown");
            assert!(error.detail().contains(expected_detail), "{error}");
            assert_eq!(driver.pending.as_ref(), Some(&pending_commit()));
        }

        let mut input = Vec::new();
        append_frame(&mut input, &ServerFrame::Committed { sequence: 7 });
        let mut exact = PluginDriver {
            io: FramedIo::new(Cursor::new(input), Vec::<u8>::new()),
            pending: Some(pending_commit()),
            terminal: false,
        };
        exact
            .acknowledge(&delivery_id)
            .expect("only exact Committed confirmation succeeds");
        assert!(exact.pending.is_none());
    }

    #[test]
    fn plugin_diagnostic_truncation_preserves_utf8_boundaries() {
        let error = PluginError::new("fixture", format!("{}😀", "x".repeat(1_999)));
        assert_eq!(error.detail().len(), 1_999);
        assert!(error.detail().chars().all(|character| character == 'x'));
    }

    #[test]
    fn duplicate_keys_are_refused_at_every_object_depth() {
        let top_level = br#"{"type":"hello","type":"close","min_version":1,"max_version":1}"#;
        let payload_object = br#"{"type":"item","item":{"kind":"batch","sequence":1,"provider_cursor":"cursor","delivery_id":"delivery","events":[{"kind":"message_created","channel_id":"channels/one","message_id":"messages/one","thread_id":"threads/one","sender_id":"users/one","text":"","created_at":"2026-01-01T00:00:00Z","thread_reply":false,"provider_payload":{"schema":"fixture.message.v1","data":{"resource":1,"resource":2}}}]}}"#;
        let nested_payload_object = br#"{"type":"item","item":{"kind":"batch","sequence":1,"provider_cursor":"cursor","delivery_id":"delivery","events":[{"kind":"message_created","channel_id":"channels/one","message_id":"messages/one","thread_id":"threads/one","sender_id":"users/one","text":"","created_at":"2026-01-01T00:00:00Z","thread_reply":false,"provider_payload":{"schema":"fixture.message.v1","data":{"resource":{"name":"one","name":"two"}}}}]}}"#;
        let configuration_object = br#"{"type":"start","channel_ids":["channels/one"],"allowed_senders":["users/one"],"max_uncommitted":1,"resume_from":null,"backend_configuration":{"schema":"fixture.config.v1","data":{"subscription":"one","subscription":"two"}}}"#;
        for raw in [
            top_level.as_slice(),
            payload_object,
            nested_payload_object,
            configuration_object,
        ] {
            let mut io = FramedIo::new(Cursor::new(frame(raw)), Vec::new());
            let error = io
                .receive::<ServerFrame>()
                .expect_err("duplicate object key must fail");
            assert_eq!(error.code(), "invalid_frame");
            assert!(error.detail().contains("duplicate JSON object key"));
        }
    }

    #[test]
    fn canonical_start_and_message_batch_field_names_are_stable() {
        assert_eq!(PROTOCOL_NAME, "agentctl-chat-subscription");
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(
            serde_json::to_value(ClientFrame::Hello {
                min_version: 1,
                max_version: 1,
            })
            .expect("serialize client hello"),
            serde_json::json!({"type": "hello", "min_version": 1, "max_version": 1})
        );
        assert_eq!(
            serde_json::to_value(ServerFrame::Hello {
                version: 1,
                capabilities: WireCapabilities {
                    backend_name: "fixture".to_owned(),
                    replay: WireReplay::Cursor,
                    full_message_data: true,
                    max_uncommitted: 1,
                    event_kinds: vec![WireEventKind::MessageCreated, WireEventKind::Heartbeat],
                },
            })
            .expect("serialize server hello"),
            serde_json::json!({
                "type": "hello",
                "version": 1,
                "capabilities": {
                    "backend_name": "fixture",
                    "replay": "cursor",
                    "full_message_data": true,
                    "max_uncommitted": 1,
                    "event_kinds": ["message_created", "heartbeat"]
                }
            })
        );
        assert_eq!(
            serde_json::to_value(ClientFrame::Start {
                channel_ids: vec!["channels/one".to_owned()],
                allowed_senders: vec!["users/owner".to_owned()],
                max_uncommitted: 1,
                resume_from: Some("cursor-before".to_owned()),
                backend_configuration: Some(WireBackendConfiguration {
                    schema: "fixture.config.v1".to_owned(),
                    data: serde_json::Map::from_iter([(
                        "subscription".to_owned(),
                        serde_json::Value::String("subscriptions/one".to_owned()),
                    )]),
                }),
            })
            .expect("serialize start"),
            serde_json::json!({
                "type": "start",
                "channel_ids": ["channels/one"],
                "allowed_senders": ["users/owner"],
                "max_uncommitted": 1,
                "resume_from": "cursor-before",
                "backend_configuration": {
                    "schema": "fixture.config.v1",
                    "data": {"subscription": "subscriptions/one"}
                }
            })
        );
        assert_eq!(
            serde_json::to_value(ServerFrame::Item {
                item: WireItem::Batch {
                    sequence: 7,
                    provider_cursor: "provider-cursor".to_owned(),
                    delivery_id: "delivery-id".to_owned(),
                    events: vec![WireEvent::MessageCreated(Box::new(WireMessageCreated {
                        channel_id: "channels/one".to_owned(),
                        message_id: "messages/one".to_owned(),
                        thread_id: "threads/one".to_owned(),
                        sender_id: "users/owner".to_owned(),
                        text: String::new(),
                        created_at: "2026-01-01T00:00:00Z".to_owned(),
                        thread_reply: false,
                        provider_payload: Some(WireProviderPayload {
                            schema: "fixture.message.v1".to_owned(),
                            data: serde_json::Map::from_iter([(
                                "resource".to_owned(),
                                serde_json::json!({"name": "messages/one"}),
                            )]),
                        }),
                    }))],
                }
            })
            .expect("serialize item"),
            serde_json::json!({
                "type": "item",
                "item": {
                    "kind": "batch",
                    "sequence": 7,
                    "provider_cursor": "provider-cursor",
                    "delivery_id": "delivery-id",
                    "events": [{
                        "kind": "message_created",
                        "channel_id": "channels/one",
                        "message_id": "messages/one",
                        "thread_id": "threads/one",
                        "sender_id": "users/owner",
                        "text": "",
                        "created_at": "2026-01-01T00:00:00Z",
                        "thread_reply": false,
                        "provider_payload": {
                            "schema": "fixture.message.v1",
                            "data": {"resource": {"name": "messages/one"}}
                        }
                    }]
                }
            })
        );
        for (frame, expected) in [
            (
                serde_json::to_value(WireEvent::Checkpoint).expect("serialize checkpoint"),
                serde_json::json!({"kind": "checkpoint"}),
            ),
            (
                serde_json::to_value(WireEvent::Gap {
                    reason: Some("retention boundary".to_owned()),
                })
                .expect("serialize gap"),
                serde_json::json!({"kind": "gap", "reason": "retention boundary"}),
            ),
            (
                serde_json::to_value(ClientFrame::Commit {
                    sequence: 7,
                    delivery_id: "delivery-id".to_owned(),
                })
                .expect("serialize commit"),
                serde_json::json!({"type": "commit", "sequence": 7, "delivery_id": "delivery-id"}),
            ),
            (
                serde_json::to_value(ClientFrame::Close).expect("serialize close"),
                serde_json::json!({"type": "close"}),
            ),
            (
                serde_json::to_value(ServerFrame::Subscribed).expect("serialize subscribed"),
                serde_json::json!({"type": "subscribed"}),
            ),
            (
                serde_json::to_value(ServerFrame::Item {
                    item: WireItem::Heartbeat { sequence: 8 },
                })
                .expect("serialize heartbeat"),
                serde_json::json!({"type": "item", "item": {"kind": "heartbeat", "sequence": 8}}),
            ),
            (
                serde_json::to_value(ServerFrame::Committed { sequence: 7 })
                    .expect("serialize committed"),
                serde_json::json!({"type": "committed", "sequence": 7}),
            ),
            (
                serde_json::to_value(ServerFrame::End).expect("serialize end"),
                serde_json::json!({"type": "end"}),
            ),
            (
                serde_json::to_value(ServerFrame::Error {
                    code: "provider_unavailable".to_owned(),
                    detail: "fixture".to_owned(),
                    retryable: true,
                    fatal: true,
                })
                .expect("serialize error"),
                serde_json::json!({
                    "type": "error",
                    "code": "provider_unavailable",
                    "detail": "fixture",
                    "retryable": true,
                    "fatal": true
                }),
            ),
        ] {
            assert_eq!(frame, expected);
        }
    }

    #[test]
    fn every_core_valid_worst_case_batch_fits_one_protocol_frame() {
        let message = || {
            InboundMessage::new(
                ChannelId::new("\0".repeat(chat_subscription::MAX_RESOURCE_ID_BYTES))
                    .expect("maximum escaped channel"),
                MessageId::new("\0".repeat(chat_subscription::MAX_RESOURCE_ID_BYTES))
                    .expect("maximum escaped message"),
                ThreadId::new("\0".repeat(chat_subscription::MAX_RESOURCE_ID_BYTES))
                    .expect("maximum escaped thread"),
                SenderId::new("\0".repeat(chat_subscription::MAX_SENDER_ID_BYTES))
                    .expect("maximum escaped sender"),
                "\0".repeat(chat_subscription::MAX_MESSAGE_TEXT_BYTES),
                "2026-01-01T00:00:00Z",
                false,
            )
            .expect("maximum escaped normalized message")
        };
        let mut events = vec![
            CommittableEvent::message_created(message()),
            CommittableEvent::message_created(message()),
        ];
        for _ in 0..5 {
            events.push(CommittableEvent::Gap(
                ReconciliationGap::new(Some("\0".repeat(chat_subscription::MAX_GAP_REASON_BYTES)))
                    .expect("maximum escaped gap"),
            ));
        }
        events.resize(
            chat_subscription::MAX_BATCH_EVENTS,
            CommittableEvent::Checkpoint,
        );
        let batch = DeliveryBatch::new(
            EventSequence::new(u64::MAX).expect("maximum sequence"),
            ProviderCursor::new("\0".repeat(chat_subscription::MAX_TOKEN_BYTES))
                .expect("maximum escaped cursor"),
            DeliveryId::new("\0".repeat(chat_subscription::MAX_TOKEN_BYTES))
                .expect("maximum escaped delivery"),
            events,
        )
        .expect("encoded variable payload remains inside the core batch bound");
        let item = SubscriptionItem::Batch(batch);
        let owned_payload = serde_json::to_vec(&ServerFrame::Item {
            item: WireItem::from(&item),
        })
        .expect("serialize worst-case valid item");
        let payload = serde_json::to_vec(&BorrowedServerFrame::Item {
            item: BorrowedWireItem::from(&item),
        })
        .expect("serialize borrowed worst-case valid item");
        assert_eq!(payload, owned_payload);
        assert!(
            payload.len() <= MAX_CORE_BATCH_FRAME_BYTES,
            "{}",
            payload.len()
        );
    }

    #[test]
    fn start_accepts_absent_backend_configuration_as_none() {
        let frame: ClientFrame = decode_strict_json(
            br#"{"type":"start","channel_ids":["channels/one"],"allowed_senders":["users/owner"],"max_uncommitted":1,"resume_from":null}"#,
        )
        .expect("v1 start without backend configuration");
        assert!(matches!(
            frame,
            ClientFrame::Start {
                backend_configuration: None,
                ..
            }
        ));
    }
}
