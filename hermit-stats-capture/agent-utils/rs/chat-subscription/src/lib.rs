//! Provider-neutral ordered inbound chat subscriptions.
//!
//! This crate deliberately models only the push side of chat. Request/response clients may have
//! richer message, thread, posting, and rate-limit APIs; they should use explicit adapters rather
//! than make this subscription trait pretend to be those APIs.
//!
//! A [`ChatSubscription`] keeps at most one receipt-bearing [`DeliveryBatch`] outstanding. The caller
//! must durably store the event and its [`ProviderCursor`] before calling
//! [`ChatSubscription::commit_durable`]. Only that call authorizes the backend to acknowledge the
//! provider delivery. A cursor is replay authority; a [`DeliveryId`] is an ephemeral handle
//! for one live delivery. Their distinct types prevent accidental interchange.
//!
//! Backends do no persistence through this API. Their hot event loop receives one item, blocks on
//! backpressure, and acknowledges only after the durable owner commits. A process plugin can
//! implement the same object-safe traits through a bounded wire adapter without exposing a Rust
//! dynamic-library ABI.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroU16;
use std::sync::Arc;

/// Maximum UTF-8 size of a replay cursor or live delivery receipt.
pub const MAX_TOKEN_BYTES: usize = 8_192;
/// Maximum UTF-8 size of a normalized message body.
pub const MAX_MESSAGE_TEXT_BYTES: usize = 32_000;
/// Maximum UTF-8 size of a channel, message, or thread identifier.
pub const MAX_RESOURCE_ID_BYTES: usize = 2_048;
/// Maximum UTF-8 size of a sender identifier.
pub const MAX_SENDER_ID_BYTES: usize = 256;
/// Maximum UTF-8 size of a normalized creation timestamp.
pub const MAX_TIMESTAMP_BYTES: usize = 128;
/// Maximum UTF-8 size of a reconciliation-gap reason.
pub const MAX_GAP_REASON_BYTES: usize = 2_000;
/// Maximum UTF-8 size of a backend name or machine-readable error code.
pub const MAX_LABEL_BYTES: usize = 128;
/// Maximum UTF-8 size of a backend error detail.
pub const MAX_ERROR_DETAIL_BYTES: usize = 2_000;
/// Maximum number of channel/space authorities in one ordered subscription.
pub const MAX_SUBSCRIPTION_CHANNELS: usize = 32;
/// Maximum number of authenticated sender authorities in one subscription.
pub const MAX_ALLOWED_SENDERS: usize = 256;
/// Maximum number of ordered child events sharing one provider acknowledgement.
pub const MAX_BATCH_EVENTS: usize = 256;
/// Maximum aggregate JSON-encoded variable payload bytes in one provider delivery batch.
///
/// Counting the encoded representation, including string escaping, keeps every valid batch
/// representable inside the process protocol's larger frame bound.
pub const MAX_BATCH_BYTES: usize = 512 * 1_024;
/// Maximum compact JSON bytes in one complete provider-specific `{schema,data}` envelope.
pub const MAX_PROVIDER_PAYLOAD_BYTES: usize = 256 * 1_024;
/// Maximum nesting accepted inside a provider-specific JSON object.
pub const MAX_PROVIDER_PAYLOAD_DEPTH: usize = 64;
/// Maximum encoded schema and JSON bytes in host-controlled backend configuration.
pub const MAX_BACKEND_CONFIGURATION_BYTES: usize = 64 * 1_024;
/// Maximum nesting accepted inside backend configuration.
pub const MAX_BACKEND_CONFIGURATION_DEPTH: usize = 64;
/// Number of distinct event categories representable by [`EventKind`].
pub const MAX_EVENT_KINDS: usize = 4;

fn validate_text(value: &str, label: &str, maximum: usize) -> Result<(), ValidationError> {
    let bytes = value.len();
    if value.is_empty() || bytes > maximum {
        return Err(ValidationError::new(format!(
            "{label} must contain 1-{maximum} UTF-8 bytes; found {bytes}"
        )));
    }
    Ok(())
}

fn validate_optional_text(value: &str, label: &str, maximum: usize) -> Result<(), ValidationError> {
    let bytes = value.len();
    if bytes > maximum {
        return Err(ValidationError::new(format!(
            "{label} must contain 0-{maximum} UTF-8 bytes; found {bytes}"
        )));
    }
    Ok(())
}

fn decimal(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0_u32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + u32::from(byte - b'0'))
    })
}

fn leap_year(year: u32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn valid_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't'))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return false;
    }
    let Some(year) = decimal(&bytes[0..4]) else {
        return false;
    };
    let Some(month) = decimal(&bytes[5..7]) else {
        return false;
    };
    let Some(day) = decimal(&bytes[8..10]) else {
        return false;
    };
    let Some(hour) = decimal(&bytes[11..13]) else {
        return false;
    };
    let Some(minute) = decimal(&bytes[14..16]) else {
        return false;
    };
    let Some(second) = decimal(&bytes[17..19]) else {
        return false;
    };
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    if day == 0 || day > month_days || hour > 23 || minute > 59 || second > 60 {
        return false;
    }

    let mut zone = 19;
    if bytes.get(zone) == Some(&b'.') {
        zone += 1;
        let fraction_start = zone;
        while bytes.get(zone).is_some_and(u8::is_ascii_digit) {
            zone += 1;
        }
        if zone == fraction_start {
            return false;
        }
    }
    match bytes.get(zone) {
        Some(b'Z' | b'z') => zone + 1 == bytes.len(),
        Some(b'+' | b'-') if zone + 6 == bytes.len() && bytes.get(zone + 3) == Some(&b':') => {
            decimal(&bytes[zone + 1..zone + 3]).is_some_and(|offset_hour| offset_hour <= 23)
                && decimal(&bytes[zone + 4..zone + 6])
                    .is_some_and(|offset_minute| offset_minute <= 59)
        }
        _ => false,
    }
}

macro_rules! bounded_string {
    ($name:ident, $doc:literal, $label:literal, $maximum:expr) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            #[doc = "Validate and retain one opaque value."]
            ///
            /// # Errors
            ///
            /// Returns [`ValidationError`] when the value is empty or exceeds its byte limit.
            pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
                let value = value.into();
                validate_text(&value, $label, $maximum)?;
                Ok(Self(value))
            }

            #[doc = "Borrow the unchanged provider value."]
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[doc = "Consume the wrapper and return the unchanged provider value."]
            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

bounded_string!(
    ChannelId,
    "A provider-stable channel or space identifier.",
    "channel id",
    MAX_RESOURCE_ID_BYTES
);
bounded_string!(
    MessageId,
    "A provider-stable message identifier.",
    "message id",
    MAX_RESOURCE_ID_BYTES
);
bounded_string!(
    ThreadId,
    "A provider-stable thread identifier.",
    "thread id",
    MAX_RESOURCE_ID_BYTES
);
bounded_string!(
    SenderId,
    "An authenticated provider-stable sender identifier.",
    "sender id",
    MAX_SENDER_ID_BYTES
);
bounded_string!(
    ProviderCursor,
    "An opaque durable replay position supplied on the next subscription.",
    "provider cursor",
    MAX_TOKEN_BYTES
);
bounded_string!(
    DeliveryId,
    "An opaque acknowledgement handle valid only for one live delivery batch.",
    "delivery id",
    MAX_TOKEN_BYTES
);

/// A validation failure at a public domain boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    detail: String,
}

impl ValidationError {
    fn new(detail: String) -> Self {
        Self { detail }
    }

    /// Borrow the bounded diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl std::error::Error for ValidationError {}

struct BoundedCounter {
    count: usize,
    maximum: usize,
}

impl Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.count.saturating_add(bytes.len());
        if next > self.maximum {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "JSON object exceeds its encoded byte limit",
            ));
        }
        self.count = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encoded_json_string_bytes(value: &str) -> usize {
    let mut counter = BoundedCounter {
        count: 0,
        maximum: usize::MAX,
    };
    serde_json::to_writer(&mut counter, value)
        .expect("serializing a bounded string into an infallible counter succeeds");
    counter.count
}

fn valid_payload_schema(value: &str) -> bool {
    value.len() <= MAX_LABEL_BYTES
        && value.split('.').count() >= 2
        && value.split('.').all(|segment| {
            let bytes = segment.as_bytes();
            !bytes.is_empty()
                && bytes[0].is_ascii_lowercase()
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        })
}

fn validate_json_object(
    data: &serde_json::Map<String, serde_json::Value>,
    label: &str,
    maximum: usize,
    maximum_depth: usize,
    require_nonempty: bool,
) -> Result<usize, ValidationError> {
    if require_nonempty && data.is_empty() {
        return Err(ValidationError::new(format!(
            "{label} must be a nonempty JSON object"
        )));
    }
    if data.len() > maximum {
        return Err(ValidationError::new(format!(
            "{label} exceeds the {maximum}-node pre-encoding budget"
        )));
    }
    let mut scheduled_nodes = data.len();
    let mut pending: Vec<(&serde_json::Value, usize)> = Vec::with_capacity(data.len());
    pending.extend(data.values().map(|value| (value, 1)));
    while let Some((value, depth)) = pending.pop() {
        if depth > maximum_depth {
            return Err(ValidationError::new(format!(
                "{label} exceeds {maximum_depth} nesting levels"
            )));
        }
        match value {
            serde_json::Value::Array(values) => {
                scheduled_nodes = scheduled_nodes.saturating_add(values.len());
                if scheduled_nodes > maximum {
                    return Err(ValidationError::new(format!(
                        "{label} exceeds the {maximum}-node pre-encoding budget"
                    )));
                }
                pending.extend(values.iter().map(|value| (value, depth + 1)));
            }
            serde_json::Value::Object(values) => {
                scheduled_nodes = scheduled_nodes.saturating_add(values.len());
                if scheduled_nodes > maximum {
                    return Err(ValidationError::new(format!(
                        "{label} exceeds the {maximum}-node pre-encoding budget"
                    )));
                }
                pending.extend(values.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    let mut counter = BoundedCounter { count: 0, maximum };
    serde_json::to_writer(&mut counter, data)
        .map_err(|_| ValidationError::new(format!("{label} exceeds {maximum} encoded bytes")))?;
    Ok(counter.count)
}

/// A bounded provider-specific full-resource object retained atomically with normalized fields.
///
/// Provider crates translate typed serde structs into this envelope. The core does not interpret
/// `data`, but validates its schema name, object shape, nesting, and encoded size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderPayload {
    schema: String,
    data: serde_json::Map<String, serde_json::Value>,
    encoded_bytes: usize,
}

impl ProviderPayload {
    /// Validate one provider-specific JSON object.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for an invalid schema, empty object, excessive nesting, or a
    /// complete encoded `{schema,data}` envelope larger than [`MAX_PROVIDER_PAYLOAD_BYTES`].
    pub fn new(
        schema: impl Into<String>,
        data: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, ValidationError> {
        let schema = schema.into();
        if !valid_payload_schema(&schema) {
            return Err(ValidationError::new(
                "provider payload schema must be a lowercase dotted slug of at most 128 bytes"
                    .to_owned(),
            ));
        }
        let data_bytes = validate_json_object(
            &data,
            "provider payload data",
            MAX_PROVIDER_PAYLOAD_BYTES,
            MAX_PROVIDER_PAYLOAD_DEPTH,
            true,
        )?;
        // Exact compact JSON overhead for `{"schema":"","data":}`. Provider schema slugs are
        // restricted to unescaped ASCII, so their UTF-8 length is also their encoded content size.
        let encoded_bytes = 21_usize
            .saturating_add(schema.len())
            .saturating_add(data_bytes);
        if encoded_bytes > MAX_PROVIDER_PAYLOAD_BYTES {
            return Err(ValidationError::new(format!(
                "provider payload schema and data contain {encoded_bytes} encoded bytes; maximum is {MAX_PROVIDER_PAYLOAD_BYTES}"
            )));
        }
        Ok(Self {
            schema,
            data,
            encoded_bytes,
        })
    }

    /// Return the provider-specific schema identifier.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Borrow the opaque full-resource object.
    #[must_use]
    pub fn data(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.data
    }

    /// Return the validated compact JSON size of the complete `{schema,data}` envelope.
    #[must_use]
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
}

/// Bounded, non-secret provider resource configuration supplied by the host.
///
/// Static backends and process plugins receive the same opaque object. Manifests cannot populate
/// it or request environment access; the operator-facing host owns its schema and values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendConfiguration {
    schema: String,
    data: serde_json::Map<String, serde_json::Value>,
    encoded_bytes: usize,
}

impl BackendConfiguration {
    /// Validate one host-controlled backend configuration object.
    ///
    /// An empty `data` object is valid for implementations whose ambient identity is sufficient.
    /// Secrets and credentials should use a provider credential service, not this object.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for an invalid schema, excessive nesting, or a schema plus JSON
    /// representation larger than [`MAX_BACKEND_CONFIGURATION_BYTES`].
    pub fn new(
        schema: impl Into<String>,
        data: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Self, ValidationError> {
        let schema = schema.into();
        if !valid_payload_schema(&schema) {
            return Err(ValidationError::new(
                "backend configuration schema must be a lowercase dotted slug of at most 128 bytes"
                    .to_owned(),
            ));
        }
        let data_bytes = validate_json_object(
            &data,
            "backend configuration data",
            MAX_BACKEND_CONFIGURATION_BYTES,
            MAX_BACKEND_CONFIGURATION_DEPTH,
            false,
        )?;
        // Exact compact JSON overhead for `{"schema":"","data":}`.
        let encoded_bytes = 21_usize
            .saturating_add(schema.len())
            .saturating_add(data_bytes);
        if encoded_bytes > MAX_BACKEND_CONFIGURATION_BYTES {
            return Err(ValidationError::new(format!(
                "backend configuration schema and data contain {encoded_bytes} encoded bytes; maximum is {MAX_BACKEND_CONFIGURATION_BYTES}"
            )));
        }
        Ok(Self {
            schema,
            data,
            encoded_bytes,
        })
    }

    /// Return the backend-specific configuration schema.
    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Borrow the opaque non-secret configuration object.
    #[must_use]
    pub fn data(&self) -> &serde_json::Map<String, serde_json::Value> {
        &self.data
    }

    /// Return the validated encoded schema and JSON byte count.
    #[must_use]
    pub fn encoded_bytes(&self) -> usize {
        self.encoded_bytes
    }
}

/// One monotonically increasing position within a live subscription generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventSequence(u64);

impl EventSequence {
    /// Construct a nonzero live-generation sequence number.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for zero, which is reserved for "no event".
    pub fn new(value: u64) -> Result<Self, ValidationError> {
        if value == 0 {
            return Err(ValidationError::new(
                "event sequence must be greater than zero".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// Return the numeric live-generation sequence.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

/// The normalized message subset carried by an inbound creation event.
///
/// This is an authenticated notification payload, not a replacement for a request/response
/// client's full message type. An adapter may enrich or translate it before exposing it through a
/// timeline API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundMessage {
    channel_id: ChannelId,
    message_id: MessageId,
    thread_id: ThreadId,
    sender_id: SenderId,
    text: String,
    created_at: String,
    thread_reply: bool,
    provider_payload: Option<ProviderPayload>,
}

impl InboundMessage {
    /// Construct one bounded normalized creation event.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when text is oversized or the provider timestamp is not a
    /// bounded RFC 3339 instant with an explicit offset.
    pub fn new(
        channel_id: ChannelId,
        message_id: MessageId,
        thread_id: ThreadId,
        sender_id: SenderId,
        text: impl Into<String>,
        created_at: impl Into<String>,
        thread_reply: bool,
    ) -> Result<Self, ValidationError> {
        let text = text.into();
        let created_at = created_at.into();
        validate_optional_text(&text, "message text", MAX_MESSAGE_TEXT_BYTES)?;
        validate_text(&created_at, "message timestamp", MAX_TIMESTAMP_BYTES)?;
        if !valid_rfc3339(&created_at) {
            return Err(ValidationError::new(
                "message timestamp must be an RFC3339 instant with an explicit offset or Z"
                    .to_owned(),
            ));
        }
        Ok(Self {
            channel_id,
            message_id,
            thread_id,
            sender_id,
            text,
            created_at,
            thread_reply,
            provider_payload: None,
        })
    }

    /// Attach a validated provider-specific full-resource object.
    #[must_use]
    pub fn with_provider_payload(mut self, provider_payload: ProviderPayload) -> Self {
        self.provider_payload = Some(provider_payload);
        self
    }

    /// Return the channel or space that owns the message.
    #[must_use]
    pub fn channel_id(&self) -> &ChannelId {
        &self.channel_id
    }

    /// Return the immutable provider message identity.
    #[must_use]
    pub fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    /// Return the provider thread identity.
    #[must_use]
    pub fn thread_id(&self) -> &ThreadId {
        &self.thread_id
    }

    /// Return the authenticated sender identity.
    #[must_use]
    pub fn sender_id(&self) -> &SenderId {
        &self.sender_id
    }

    /// Return the unchanged message body.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return the unchanged bounded provider timestamp.
    #[must_use]
    pub fn created_at(&self) -> &str {
        &self.created_at
    }

    /// Report whether provider metadata classified this as a thread reply.
    #[must_use]
    pub fn is_thread_reply(&self) -> bool {
        self.thread_reply
    }

    /// Borrow provider-specific full resource data, when the backend supplies it.
    #[must_use]
    pub fn provider_payload(&self) -> Option<&ProviderPayload> {
        self.provider_payload.as_ref()
    }
}

/// A bounded explanation for a replay or retention gap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciliationGap {
    reason: Option<String>,
}

impl ReconciliationGap {
    /// Construct a gap; a present reason must be nonempty and bounded.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when a supplied reason is empty or oversized.
    pub fn new(reason: Option<String>) -> Result<Self, ValidationError> {
        if let Some(reason) = &reason {
            validate_text(reason, "gap reason", MAX_GAP_REASON_BYTES)?;
        }
        Ok(Self { reason })
    }

    /// Borrow the optional provider explanation.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }
}

/// A receipt-bearing event that advances durable subscription state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittableEvent {
    /// A fully normalized newly created message.
    MessageCreated(Box<InboundMessage>),
    /// An upstream boundary with no user-visible message.
    Checkpoint,
    /// A boundary that requires reconciliation from a request/response API.
    Gap(ReconciliationGap),
}

impl CommittableEvent {
    /// Wrap one message creation event without exposing storage indirection to constructors.
    #[must_use]
    pub fn message_created(message: InboundMessage) -> Self {
        Self::MessageCreated(Box::new(message))
    }

    /// Return the capability kind needed to emit this event.
    #[must_use]
    pub fn kind(&self) -> EventKind {
        match self {
            Self::MessageCreated(_) => EventKind::MessageCreated,
            Self::Checkpoint => EventKind::Checkpoint,
            Self::Gap(_) => EventKind::Gap,
        }
    }

    /// Report whether the durable owner must schedule request/response reconciliation.
    #[must_use]
    pub fn requires_reconciliation(&self) -> bool {
        matches!(self, Self::Gap(_))
    }

    fn encoded_variable_bytes(&self) -> usize {
        match self {
            Self::MessageCreated(message) => {
                encoded_json_string_bytes(message.channel_id().as_str())
                    + encoded_json_string_bytes(message.message_id().as_str())
                    + encoded_json_string_bytes(message.thread_id().as_str())
                    + encoded_json_string_bytes(message.sender_id().as_str())
                    + encoded_json_string_bytes(message.text())
                    + encoded_json_string_bytes(message.created_at())
                    + message
                        .provider_payload()
                        .map_or(0, ProviderPayload::encoded_bytes)
            }
            Self::Checkpoint => 0,
            Self::Gap(gap) => gap.reason().map_or(0, encoded_json_string_bytes),
        }
    }
}

/// One ordered provider delivery committed only after all child events are durable.
///
/// A provider may group several message-created events under one upstream acknowledgement. The
/// batch preserves that atomic unit: a caller must not acknowledge individual children.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryBatch {
    sequence: EventSequence,
    cursor: ProviderCursor,
    delivery_id: DeliveryId,
    events: Vec<CommittableEvent>,
}

impl DeliveryBatch {
    /// Construct one bounded, nonempty acknowledgement unit.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] if the child count or aggregate JSON-encoded variable payload
    /// is unsafe.
    pub fn new(
        sequence: EventSequence,
        cursor: ProviderCursor,
        delivery_id: DeliveryId,
        events: Vec<CommittableEvent>,
    ) -> Result<Self, ValidationError> {
        if events.is_empty() || events.len() > MAX_BATCH_EVENTS {
            return Err(ValidationError::new(format!(
                "delivery batch must contain 1-{MAX_BATCH_EVENTS} events; found {}",
                events.len()
            )));
        }
        let payload_bytes = events
            .iter()
            .map(CommittableEvent::encoded_variable_bytes)
            .sum::<usize>();
        if payload_bytes > MAX_BATCH_BYTES {
            return Err(ValidationError::new(format!(
                "delivery batch encoded payload contains {payload_bytes} bytes; maximum is {MAX_BATCH_BYTES}"
            )));
        }
        Ok(Self {
            sequence,
            cursor,
            delivery_id,
            events,
        })
    }

    /// Return this event's live-generation order.
    #[must_use]
    pub fn sequence(&self) -> EventSequence {
        self.sequence
    }

    /// Return the replay position the owner must persist before commit.
    #[must_use]
    pub fn cursor(&self) -> &ProviderCursor {
        &self.cursor
    }

    /// Return the ephemeral live-delivery acknowledgement handle.
    #[must_use]
    pub fn delivery_id(&self) -> &DeliveryId {
        &self.delivery_id
    }

    /// Return all normalized child events in provider order.
    #[must_use]
    pub fn events(&self) -> &[CommittableEvent] {
        &self.events
    }
}

/// A liveness indication that advances only live ordering and requires no durable commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    sequence: EventSequence,
}

impl Heartbeat {
    /// Construct an ordered heartbeat.
    #[must_use]
    pub fn new(sequence: EventSequence) -> Self {
        Self { sequence }
    }

    /// Return this heartbeat's live-generation order.
    #[must_use]
    pub fn sequence(self) -> EventSequence {
        self.sequence
    }
}

/// One item emitted by a live subscription.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionItem {
    /// A durable provider batch that applies backpressure until committed.
    Batch(DeliveryBatch),
    /// A non-durable liveness indication.
    Heartbeat(Heartbeat),
}

impl SubscriptionItem {
    /// Return the live-generation sequence shared by every item kind.
    #[must_use]
    pub fn sequence(&self) -> EventSequence {
        match self {
            Self::Batch(delivery) => delivery.sequence(),
            Self::Heartbeat(heartbeat) => heartbeat.sequence(),
        }
    }
}

/// A capability-visible event category.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Newly created messages with full normalized resource data.
    MessageCreated,
    /// Cursor-only checkpoints.
    Checkpoint,
    /// Reconciliation gaps.
    Gap,
    /// Liveness heartbeats.
    Heartbeat,
}

/// Whether a backend can reconnect from a durable provider cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaySupport {
    /// Reconnection starts at the provider's current position and reports any loss as a gap.
    CurrentOnly,
    /// Reconnection accepts an opaque cursor and replays its boundary inclusively.
    Cursor,
}

/// Provider-neutral capability information available before a subscription starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendCapabilities {
    backend_name: String,
    replay: ReplaySupport,
    full_message_data: bool,
    max_uncommitted: NonZeroU16,
    event_kinds: Vec<EventKind>,
}

impl BackendCapabilities {
    /// Construct and validate a capability report.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for an invalid name, duplicate kinds, or missing heartbeat.
    pub fn new(
        backend_name: impl Into<String>,
        replay: ReplaySupport,
        full_message_data: bool,
        max_uncommitted: NonZeroU16,
        event_kinds: Vec<EventKind>,
    ) -> Result<Self, ValidationError> {
        let backend_name = backend_name.into();
        validate_text(&backend_name, "backend name", MAX_LABEL_BYTES)?;
        if event_kinds.len() > MAX_EVENT_KINDS {
            return Err(ValidationError::new(format!(
                "backend capabilities contain {} event kinds; maximum is {MAX_EVENT_KINDS}",
                event_kinds.len()
            )));
        }
        let mut seen = 0_u8;
        let mut has_heartbeat = false;
        for kind in &event_kinds {
            let bit = 1_u8 << (*kind as u8);
            if seen & bit != 0 {
                return Err(ValidationError::new(
                    "backend capabilities contain duplicate event kinds".to_owned(),
                ));
            }
            seen |= bit;
            has_heartbeat |= *kind == EventKind::Heartbeat;
        }
        if !has_heartbeat {
            return Err(ValidationError::new(
                "backend capabilities must include heartbeat".to_owned(),
            ));
        }
        Ok(Self {
            backend_name,
            replay,
            full_message_data,
            max_uncommitted,
            event_kinds,
        })
    }

    /// Return the stable human-readable backend name.
    #[must_use]
    pub fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Return the advertised replay behavior.
    #[must_use]
    pub fn replay(&self) -> ReplaySupport {
        self.replay
    }

    /// Report whether every message event carries a provider-specific full-resource envelope.
    #[must_use]
    pub fn full_message_data(&self) -> bool {
        self.full_message_data
    }

    /// Return the provider's upper bound; the core currently enforces one outstanding event.
    #[must_use]
    pub fn max_uncommitted(&self) -> NonZeroU16 {
        self.max_uncommitted
    }

    /// Return the exact advertised event categories.
    #[must_use]
    pub fn event_kinds(&self) -> &[EventKind] {
        &self.event_kinds
    }

    /// Report whether the backend advertised one category.
    #[must_use]
    pub fn supports(&self, kind: EventKind) -> bool {
        self.event_kinds.contains(&kind)
    }
}

/// One subscription authority and its last durably committed replay position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscribeRequest {
    channel_ids: Vec<ChannelId>,
    allowed_senders: Vec<SenderId>,
    channel_membership: HashSet<ChannelId>,
    sender_membership: HashSet<SenderId>,
    resume_from: Option<ProviderCursor>,
    backend_configuration: Option<BackendConfiguration>,
}

impl SubscribeRequest {
    /// Construct bounded routing authority. The cursor boundary is replayed inclusively and
    /// deduplicated by event identity by the durable owner.
    ///
    /// Credentials are intentionally absent. [`Self::with_backend_configuration`] carries bounded
    /// non-secret resource IDs; plugin launch supplies credentials through workload identity or a
    /// provider credential service rather than argv or this protocol.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for empty, duplicate, or oversized authority sets.
    pub fn new(
        channel_ids: Vec<ChannelId>,
        allowed_senders: Vec<SenderId>,
        resume_from: Option<ProviderCursor>,
    ) -> Result<Self, ValidationError> {
        if channel_ids.is_empty() || channel_ids.len() > MAX_SUBSCRIPTION_CHANNELS {
            return Err(ValidationError::new(format!(
                "subscription must contain 1-{MAX_SUBSCRIPTION_CHANNELS} channels; found {}",
                channel_ids.len()
            )));
        }
        if allowed_senders.is_empty() || allowed_senders.len() > MAX_ALLOWED_SENDERS {
            return Err(ValidationError::new(format!(
                "subscription must contain 1-{MAX_ALLOWED_SENDERS} allowed senders; found {}",
                allowed_senders.len()
            )));
        }
        let channel_membership = channel_ids.iter().cloned().collect::<HashSet<_>>();
        if channel_membership.len() != channel_ids.len() {
            return Err(ValidationError::new(
                "subscription channels must be unique".to_owned(),
            ));
        }
        let sender_membership = allowed_senders.iter().cloned().collect::<HashSet<_>>();
        if sender_membership.len() != allowed_senders.len() {
            return Err(ValidationError::new(
                "subscription allowed senders must be unique".to_owned(),
            ));
        }
        Ok(Self {
            channel_membership,
            sender_membership,
            channel_ids,
            allowed_senders,
            resume_from,
            backend_configuration: None,
        })
    }

    /// Attach explicit non-secret provider resource configuration from the host.
    #[must_use]
    pub fn with_backend_configuration(mut self, configuration: BackendConfiguration) -> Self {
        self.backend_configuration = Some(configuration);
        self
    }

    /// Return the exact configured channel/space authorities.
    #[must_use]
    pub fn channel_ids(&self) -> &[ChannelId] {
        &self.channel_ids
    }

    /// Return the authenticated sender identities permitted to create tasks.
    #[must_use]
    pub fn allowed_senders(&self) -> &[SenderId] {
        &self.allowed_senders
    }

    /// Return the v1 flow-control window. Future protocol versions may negotiate larger windows.
    #[must_use]
    pub fn max_uncommitted(&self) -> NonZeroU16 {
        NonZeroU16::new(1).expect("one is nonzero")
    }

    /// Return the durable replay boundary, when one exists.
    #[must_use]
    pub fn resume_from(&self) -> Option<&ProviderCursor> {
        self.resume_from.as_ref()
    }

    /// Borrow explicit host-controlled backend configuration, when supplied.
    #[must_use]
    pub fn backend_configuration(&self) -> Option<&BackendConfiguration> {
        self.backend_configuration.as_ref()
    }
}

/// A bounded provider/backend failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackendFailure {
    code: String,
    detail: String,
    retryable: bool,
}

impl BackendFailure {
    /// Construct a failure safe to cross the plugin boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the code or detail is empty or oversized.
    pub fn new(
        code: impl Into<String>,
        detail: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, ValidationError> {
        let code = code.into();
        let detail = detail.into();
        validate_text(&code, "backend error code", MAX_LABEL_BYTES)?;
        validate_text(&detail, "backend error detail", MAX_ERROR_DETAIL_BYTES)?;
        Ok(Self {
            code,
            detail,
            retryable,
        })
    }

    /// Borrow the stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Borrow the bounded diagnostic.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Report whether reconnecting from the durable cursor may succeed.
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.retryable
    }
}

impl fmt::Display for BackendFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for BackendFailure {}

/// Failure to prove that independent cancellation completed all backend cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancellationError {
    /// Cancellation returned promptly, but the backend cannot prove that all owned work stopped.
    CleanupUncertain(BackendFailure),
}

impl fmt::Display for CancellationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CleanupUncertain(error) => {
                write!(formatter, "cancellation cleanup is uncertain: {error}")
            }
        }
    }
}

impl std::error::Error for CancellationError {}

/// Independent, bounded cancellation authority for establishment and blocked receive operations.
///
/// Cancellation is terminal and sticky for one backend generation. Implementations must return
/// within their documented bound, and a receive racing immediately after cancellation starts must
/// also return promptly rather than missing a one-shot wakeup. `Ok(())` proves that the operation
/// was interrupted (or prevented from starting) and all backend-owned work was cleaned up. If that
/// proof is unavailable, they return [`CancellationError::CleanupUncertain`] rather than claiming
/// success.
pub trait ChatSubscriptionCancellation: Send + Sync {
    /// Permanently interrupt any in-progress or racing establishment/receive and complete bounded
    /// cleanup. A cancelled generation must not admit later blocking work.
    fn cancel(&self) -> Result<(), CancellationError>;
}

/// The provider-side operations behind one live ordered stream.
///
/// Implementations must not persist consumer state. [`ChatSubscription`] serializes calls so
/// `next_item` is never invoked with an uncommitted delivery outstanding.
pub trait ChatSubscriptionDriver: Send {
    /// Return cancellation authority usable concurrently with a blocked [`Self::next_item`].
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation>;

    /// Block until one ordered item is available, or return `None` after a clean terminal EOF.
    fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure>;

    /// Acknowledge exactly one live batch after the caller has durably committed all child events.
    fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure>;

    /// Cooperatively stop this live generation without acknowledging an outstanding delivery.
    ///
    /// Static backends that need no explicit cancellation may use this default. Process adapters
    /// should send their protocol's close frame. A concurrent protocol reader can use the sticky
    /// cancellation authority to retire a blocked `next_item`, but the host must still supervise
    /// and bound the process lifetime for an implementation that ignores cancellation.
    fn close(&mut self) -> Result<(), BackendFailure> {
        Ok(())
    }
}

/// Factory and capability hook for a statically linked or framed-plugin backend.
///
/// The trait is object-safe. Provider crates implement it directly; plugin adapters implement the
/// same trait while translating calls into a versioned process protocol.
pub trait ChatSubscriptionBackend: Send {
    /// Return cancellation authority before potentially blocking subscription establishment.
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation>;

    /// Return a bounded report without opening an event stream.
    fn capabilities(&self) -> BackendCapabilities;

    /// Open one stream using a durable cursor rather than any prior live receipt.
    fn subscribe(
        &mut self,
        request: &SubscribeRequest,
    ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure>;
}

/// A core-enforced ordered stream with single-delivery backpressure.
pub struct ChatSubscription {
    capabilities: BackendCapabilities,
    driver: Box<dyn ChatSubscriptionDriver>,
    cancellation: Arc<dyn ChatSubscriptionCancellation>,
    channel_membership: HashSet<ChannelId>,
    sender_membership: HashSet<SenderId>,
    last_sequence: Option<EventSequence>,
    outstanding: Option<DeliveryBatch>,
    needs_initial_boundary: bool,
    driver_close_attempted: bool,
    terminal: bool,
    poisoned: bool,
}

impl ChatSubscription {
    /// Open a subscription through an object-safe backend.
    ///
    /// If establishment may block, obtain [`ChatSubscriptionBackend::cancellation`] before this
    /// call and retain that clone on an independent supervisor thread. Once open returns, use
    /// [`Self::cancellation`] the same way around a potentially blocked receive.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError`] when resume is unsupported or the backend cannot connect.
    pub fn open(
        backend: &mut dyn ChatSubscriptionBackend,
        request: &SubscribeRequest,
    ) -> Result<Self, SubscriptionError> {
        let capabilities = backend.capabilities();
        if request.resume_from().is_some() && capabilities.replay() != ReplaySupport::Cursor {
            return Err(SubscriptionError::ResumeUnsupported);
        }
        let driver = backend
            .subscribe(request)
            .map_err(SubscriptionError::Backend)?;
        let cancellation = driver.cancellation();
        Ok(Self {
            capabilities,
            driver,
            cancellation,
            channel_membership: request.channel_membership.clone(),
            sender_membership: request.sender_membership.clone(),
            last_sequence: None,
            outstanding: None,
            needs_initial_boundary: request.resume_from().is_none(),
            driver_close_attempted: false,
            terminal: false,
            poisoned: false,
        })
    }

    /// Return the capability snapshot bound to this live stream.
    #[must_use]
    pub fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    /// Return authority that can interrupt a concurrently blocked receive.
    #[must_use]
    pub fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::clone(&self.cancellation)
    }

    /// Receive one ordered item.
    ///
    /// A receipt-bearing delivery applies backpressure: another call is refused until the exact
    /// delivery is committed. Backend or protocol failures poison this generation; reconnect from
    /// the last durably stored cursor instead of guessing whether the stream can continue.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError`] for backpressure, ordering/capability violations, or a
    /// provider failure.
    pub fn next_item(&mut self) -> Result<Option<SubscriptionItem>, SubscriptionError> {
        if self.poisoned {
            return Err(SubscriptionError::GenerationPoisoned);
        }
        if let Some(delivery) = &self.outstanding {
            return Err(SubscriptionError::OutstandingDelivery {
                sequence: delivery.sequence(),
            });
        }
        if self.terminal {
            return Ok(None);
        }
        let expected = match self.last_sequence {
            None => 1,
            Some(previous) => match previous.get().checked_add(1) {
                Some(expected) => expected,
                None => {
                    self.poisoned = true;
                    return Err(SubscriptionError::SequenceExhausted { previous });
                }
            },
        };
        let item = match self.driver.next_item() {
            Ok(Some(item)) => item,
            Ok(None) => {
                self.driver_close_attempted = true;
                if let Err(error) = self.driver.close() {
                    self.poisoned = true;
                    return Err(SubscriptionError::Backend(error));
                }
                self.outstanding = None;
                self.terminal = true;
                return Ok(None);
            }
            Err(error) => {
                self.poisoned = true;
                return Err(SubscriptionError::Backend(error));
            }
        };
        let sequence = item.sequence();
        if sequence.get() != expected {
            self.poisoned = true;
            return Err(SubscriptionError::SequenceViolation {
                expected,
                observed: sequence.get(),
            });
        }
        match &item {
            SubscriptionItem::Batch(batch) => {
                if self.needs_initial_boundary
                    && !matches!(
                        batch.events().first(),
                        Some(CommittableEvent::Checkpoint | CommittableEvent::Gap(_))
                    )
                {
                    self.poisoned = true;
                    return Err(SubscriptionError::MissingInitialBoundary);
                }
                for event in batch.events() {
                    let kind = event.kind();
                    if !self.capabilities.supports(kind) {
                        self.poisoned = true;
                        return Err(SubscriptionError::UnadvertisedEvent { kind });
                    }
                    if let CommittableEvent::MessageCreated(message) = event {
                        if self.capabilities.full_message_data()
                            && message.provider_payload().is_none()
                        {
                            self.poisoned = true;
                            return Err(SubscriptionError::MissingFullMessageData);
                        }
                        if !self.channel_membership.contains(message.channel_id()) {
                            self.poisoned = true;
                            return Err(SubscriptionError::MessageOutsideChannelAuthority);
                        }
                        if !self.sender_membership.contains(message.sender_id()) {
                            self.poisoned = true;
                            return Err(SubscriptionError::MessageOutsideSenderAuthority);
                        }
                    }
                }
            }
            SubscriptionItem::Heartbeat(_) => {
                if !self.capabilities.supports(EventKind::Heartbeat) {
                    self.poisoned = true;
                    return Err(SubscriptionError::UnadvertisedEvent {
                        kind: EventKind::Heartbeat,
                    });
                }
            }
        }
        self.last_sequence = Some(sequence);
        if let SubscriptionItem::Batch(delivery) = &item {
            self.needs_initial_boundary = false;
            self.outstanding = Some(delivery.clone());
        }
        Ok(Some(item))
    }

    /// Acknowledge one delivery only after its event and cursor are durable.
    ///
    /// A backend error makes the acknowledgement outcome ambiguous and poisons this generation.
    /// Reconnect from the durable cursor; do not substitute the ephemeral receipt as a cursor.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError`] for a stale/mismatched delivery or backend failure.
    pub fn commit_durable(&mut self, delivery: &DeliveryBatch) -> Result<(), SubscriptionError> {
        if self.poisoned {
            return Err(SubscriptionError::GenerationPoisoned);
        }
        let Some(outstanding) = &self.outstanding else {
            return Err(SubscriptionError::NoOutstandingDelivery);
        };
        if outstanding != delivery {
            return Err(SubscriptionError::CommitMismatch {
                expected: outstanding.sequence(),
                observed: delivery.sequence(),
            });
        }
        if let Err(error) = self.driver.acknowledge(delivery.delivery_id()) {
            self.poisoned = true;
            return Err(SubscriptionError::Backend(error));
        }
        self.outstanding = None;
        Ok(())
    }

    /// Cooperatively close this live generation.
    ///
    /// An outstanding delivery is deliberately left unacknowledged and may replay after the owner
    /// reconnects from its last durable cursor. A protocol host may pair Close with this
    /// generation's sticky cancellation authority to interrupt a blocked `next_item`; it must
    /// still retain a bounded child supervisor for an implementation that ignores cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionError::Backend`] if the backend could not accept cancellation. The
    /// generation is poisoned in that case and must not be reused.
    pub fn close(&mut self) -> Result<(), SubscriptionError> {
        if self.terminal {
            return Ok(());
        }
        if self.driver_close_attempted {
            self.poisoned = true;
            return Err(SubscriptionError::GenerationPoisoned);
        }
        self.driver_close_attempted = true;
        if let Err(error) = self.driver.close() {
            self.poisoned = true;
            return Err(SubscriptionError::Backend(error));
        }
        self.outstanding = None;
        self.terminal = true;
        Ok(())
    }

    /// Return the currently backpressured delivery for diagnostics.
    #[must_use]
    pub fn outstanding(&self) -> Option<&DeliveryBatch> {
        self.outstanding.as_ref()
    }

    /// Report whether this generation must be replaced from durable state.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

/// A core state-machine refusal or bounded backend failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubscriptionError {
    /// A durable cursor was supplied to a backend that cannot replay it.
    ResumeUnsupported,
    /// Another receipt-bearing event is still waiting for durable commit.
    OutstandingDelivery {
        /// Sequence of the event applying backpressure.
        sequence: EventSequence,
    },
    /// Commit was attempted without any outstanding delivery.
    NoOutstandingDelivery,
    /// Commit did not identify the exact outstanding delivery.
    CommitMismatch {
        /// Sequence currently awaiting commit.
        expected: EventSequence,
        /// Sequence supplied by the caller.
        observed: EventSequence,
    },
    /// A backend skipped, repeated, or reordered its live sequence.
    SequenceViolation {
        /// Next required sequence.
        expected: u64,
        /// Sequence emitted by the backend.
        observed: u64,
    },
    /// No sequence can follow the maximum live-generation position.
    SequenceExhausted {
        /// Last accepted sequence.
        previous: EventSequence,
    },
    /// A backend emitted an event it omitted from capability reporting.
    UnadvertisedEvent {
        /// Undeclared category.
        kind: EventKind,
    },
    /// A current-head subscription emitted data before a durable checkpoint or typed gap.
    MissingInitialBoundary,
    /// A message named a channel or space outside the start authority.
    MessageOutsideChannelAuthority,
    /// A message named a sender outside the start authority.
    MessageOutsideSenderAuthority,
    /// A backend advertising full message data omitted its provider envelope.
    MissingFullMessageData,
    /// A prior receive or acknowledgement failed ambiguously.
    GenerationPoisoned,
    /// A bounded backend failure.
    Backend(BackendFailure),
}

impl fmt::Display for SubscriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResumeUnsupported => {
                formatter.write_str("backend does not support cursor resume")
            }
            Self::OutstandingDelivery { sequence } => write!(
                formatter,
                "event {} remains uncommitted; refusing another receive",
                sequence.get()
            ),
            Self::NoOutstandingDelivery => {
                formatter.write_str("no delivery is waiting for durable commit")
            }
            Self::CommitMismatch { expected, observed } => write!(
                formatter,
                "commit identifies event {}, but event {} is outstanding",
                observed.get(),
                expected.get()
            ),
            Self::SequenceViolation { expected, observed } => write!(
                formatter,
                "subscription sequence violation: expected {expected}, observed {observed}"
            ),
            Self::SequenceExhausted { previous } => write!(
                formatter,
                "subscription sequence exhausted after {}",
                previous.get()
            ),
            Self::UnadvertisedEvent { kind } => {
                write!(
                    formatter,
                    "backend emitted unadvertised event kind {kind:?}"
                )
            }
            Self::MissingInitialBoundary => formatter.write_str(
                "subscription emitted data before its initial durable checkpoint or gap",
            ),
            Self::MessageOutsideChannelAuthority => {
                formatter.write_str("subscription message is outside channel authority")
            }
            Self::MessageOutsideSenderAuthority => {
                formatter.write_str("subscription message is outside sender authority")
            }
            Self::MissingFullMessageData => formatter
                .write_str("backend advertised full message data but omitted provider_payload"),
            Self::GenerationPoisoned => formatter.write_str(
                "subscription generation is poisoned; reconnect from the durable cursor",
            ),
            Self::Backend(error) => write!(formatter, "subscription backend failed: {error}"),
        }
    }
}

impl std::error::Error for SubscriptionError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Condvar, Mutex};
    use std::thread;
    use std::time::Duration;

    struct NoopCancellation;

    impl ChatSubscriptionCancellation for NoopCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            Ok(())
        }
    }

    fn noop_cancellation() -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(NoopCancellation)
    }

    #[derive(Clone)]
    struct BlockingCancellation {
        state: Arc<(Mutex<bool>, Condvar)>,
    }

    impl ChatSubscriptionCancellation for BlockingCancellation {
        fn cancel(&self) -> Result<(), CancellationError> {
            let (cancelled, wake) = &*self.state;
            *cancelled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            wake.notify_all();
            Ok(())
        }
    }

    fn wait_for_cancellation(state: &Arc<(Mutex<bool>, Condvar)>) {
        let (cancelled, wake) = &**state;
        let mut cancelled = cancelled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*cancelled {
            cancelled = wake
                .wait(cancelled)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn cancelled_failure() -> BackendFailure {
        BackendFailure::new("fixture_cancelled", "fixture operation was cancelled", true)
            .expect("constant failure is valid")
    }

    struct BlockingSubscribeBackend {
        cancellation: BlockingCancellation,
        entered: mpsc::SyncSender<()>,
    }

    impl ChatSubscriptionBackend for BlockingSubscribeBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(self.cancellation.clone())
        }

        fn capabilities(&self) -> BackendCapabilities {
            capabilities()
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            self.entered.send(()).expect("announce blocked subscribe");
            wait_for_cancellation(&self.cancellation.state);
            Err(cancelled_failure())
        }
    }

    struct BlockingReceiveBackend {
        cancellation: BlockingCancellation,
        entered: Option<mpsc::SyncSender<()>>,
    }

    impl ChatSubscriptionBackend for BlockingReceiveBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(self.cancellation.clone())
        }

        fn capabilities(&self) -> BackendCapabilities {
            capabilities()
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(BlockingReceiveDriver {
                cancellation: self.cancellation.clone(),
                entered: self.entered.take().expect("one receive driver"),
            }))
        }
    }

    struct BlockingReceiveDriver {
        cancellation: BlockingCancellation,
        entered: mpsc::SyncSender<()>,
    }

    impl ChatSubscriptionDriver for BlockingReceiveDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            Arc::new(self.cancellation.clone())
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            self.entered.send(()).expect("announce blocked receive");
            wait_for_cancellation(&self.cancellation.state);
            Err(cancelled_failure())
        }

        fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
            Ok(())
        }
    }

    fn provider_payload(value: serde_json::Value) -> ProviderPayload {
        ProviderPayload::new(
            "fixture.message.v1",
            serde_json::Map::from_iter([("resource".to_owned(), value)]),
        )
        .expect("valid provider payload")
    }

    fn message(
        channel: &str,
        sender: &str,
        text: &str,
        payload: Option<ProviderPayload>,
    ) -> InboundMessage {
        let message = InboundMessage::new(
            ChannelId::new(channel).expect("channel"),
            MessageId::new("messages/one").expect("message"),
            ThreadId::new("threads/one").expect("thread"),
            SenderId::new(sender).expect("sender"),
            text,
            "2026-01-01T00:00:00Z",
            false,
        )
        .expect("valid message");
        match payload {
            Some(payload) => message.with_provider_payload(payload),
            None => message,
        }
    }

    fn capabilities() -> BackendCapabilities {
        BackendCapabilities::new(
            "fixture",
            ReplaySupport::Cursor,
            true,
            NonZeroU16::new(1).expect("one is nonzero"),
            vec![
                EventKind::MessageCreated,
                EventKind::Checkpoint,
                EventKind::Gap,
                EventKind::Heartbeat,
            ],
        )
        .expect("valid fixture capabilities")
    }

    #[test]
    fn capabilities_refuse_impossible_cardinality_before_duplicate_work() {
        let error = BackendCapabilities::new(
            "fixture",
            ReplaySupport::Cursor,
            false,
            NonZeroU16::new(1).expect("one is nonzero"),
            vec![EventKind::Heartbeat; MAX_EVENT_KINDS + 1],
        )
        .expect_err("more kinds than the enum can represent must fail immediately");
        assert_eq!(
            error.detail(),
            "backend capabilities contain 5 event kinds; maximum is 4"
        );

        let duplicate = BackendCapabilities::new(
            "fixture",
            ReplaySupport::Cursor,
            false,
            NonZeroU16::new(1).expect("one is nonzero"),
            vec![EventKind::Heartbeat, EventKind::Heartbeat],
        )
        .expect_err("a duplicate inside the fixed bound must still fail");
        assert_eq!(
            duplicate.detail(),
            "backend capabilities contain duplicate event kinds"
        );
    }

    fn delivery(sequence: u64, receipt: &str) -> DeliveryBatch {
        DeliveryBatch::new(
            EventSequence::new(sequence).expect("nonzero sequence"),
            ProviderCursor::new(format!("cursor-{sequence}")).expect("bounded cursor"),
            DeliveryId::new(receipt).expect("bounded receipt"),
            vec![CommittableEvent::Checkpoint],
        )
        .expect("bounded fixture batch")
    }

    struct Driver {
        items: VecDeque<SubscriptionItem>,
        acknowledgements: Vec<DeliveryId>,
    }

    impl ChatSubscriptionDriver for Driver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            Ok(self.items.pop_front())
        }

        fn acknowledge(&mut self, receipt: &DeliveryId) -> Result<(), BackendFailure> {
            self.acknowledgements.push(receipt.clone());
            Ok(())
        }
    }

    struct Backend {
        capabilities: BackendCapabilities,
        items: Option<VecDeque<SubscriptionItem>>,
    }

    impl ChatSubscriptionBackend for Backend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            self.capabilities.clone()
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(Driver {
                items: self.items.take().expect("single subscription"),
                acknowledgements: Vec::new(),
            }))
        }
    }

    struct CountingDriver {
        receive_calls: Arc<AtomicUsize>,
        item: Option<SubscriptionItem>,
    }

    impl ChatSubscriptionDriver for CountingDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            self.receive_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.item.take())
        }

        fn acknowledge(&mut self, _receipt: &DeliveryId) -> Result<(), BackendFailure> {
            unreachable!("counting sequence driver emits no batch")
        }
    }

    struct CountingBackend {
        receive_calls: Arc<AtomicUsize>,
        item: Option<SubscriptionItem>,
    }

    impl ChatSubscriptionBackend for CountingBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            capabilities()
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(CountingDriver {
                receive_calls: Arc::clone(&self.receive_calls),
                item: self.item.take(),
            }))
        }
    }

    struct ClosingDriver {
        item: Option<SubscriptionItem>,
        close_calls: Arc<AtomicUsize>,
        fail_close: bool,
    }

    impl ChatSubscriptionDriver for ClosingDriver {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
            Ok(self.item.take())
        }

        fn acknowledge(&mut self, _receipt: &DeliveryId) -> Result<(), BackendFailure> {
            unreachable!("closing fixture leaves its delivery unacknowledged")
        }

        fn close(&mut self) -> Result<(), BackendFailure> {
            self.close_calls.fetch_add(1, Ordering::Relaxed);
            if self.fail_close {
                Err(
                    BackendFailure::new("close_failed", "fixture close failed", false)
                        .expect("fixture error is bounded"),
                )
            } else {
                Ok(())
            }
        }
    }

    struct ClosingBackend {
        item: Option<SubscriptionItem>,
        close_calls: Arc<AtomicUsize>,
        fail_close: bool,
    }

    impl ChatSubscriptionBackend for ClosingBackend {
        fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
            noop_cancellation()
        }

        fn capabilities(&self) -> BackendCapabilities {
            capabilities()
        }

        fn subscribe(
            &mut self,
            _request: &SubscribeRequest,
        ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
            Ok(Box::new(ClosingDriver {
                item: self.item.take(),
                close_calls: Arc::clone(&self.close_calls),
                fail_close: self.fail_close,
            }))
        }
    }

    fn request() -> SubscribeRequest {
        SubscribeRequest::new(
            vec![ChannelId::new("channels/one").expect("channel")],
            vec![SenderId::new("users/owner").expect("sender")],
            None,
        )
        .expect("request authority is valid")
    }

    #[test]
    fn one_uncommitted_delivery_applies_backpressure() {
        let first = delivery(1, "receipt-one");
        let second = delivery(2, "receipt-two");
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([
                SubscriptionItem::Batch(first.clone()),
                SubscriptionItem::Batch(second.clone()),
            ])),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        assert_eq!(
            subscription.next_item().expect("first receive"),
            Some(SubscriptionItem::Batch(first.clone()))
        );
        assert_eq!(
            subscription.next_item(),
            Err(SubscriptionError::OutstandingDelivery {
                sequence: first.sequence()
            })
        );
        subscription
            .commit_durable(&first)
            .expect("durable first delivery commits");
        assert_eq!(
            subscription.next_item().expect("second receive"),
            Some(SubscriptionItem::Batch(second))
        );
    }

    #[test]
    fn cooperative_close_leaves_outstanding_delivery_unacknowledged() {
        let close_calls = Arc::new(AtomicUsize::new(0));
        let batch = delivery(1, "receipt-one");
        let mut backend = ClosingBackend {
            item: Some(SubscriptionItem::Batch(batch.clone())),
            close_calls: Arc::clone(&close_calls),
            fail_close: false,
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        assert_eq!(
            subscription.next_item().expect("receive delivery"),
            Some(SubscriptionItem::Batch(batch))
        );
        subscription.close().expect("cooperative close succeeds");
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
        assert!(subscription.outstanding().is_none());
        assert_eq!(
            subscription.next_item().expect("closed stream is terminal"),
            None
        );
        subscription.close().expect("repeated close is idempotent");
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clean_end_closes_provider_exactly_once() {
        let close_calls = Arc::new(AtomicUsize::new(0));
        let mut backend = ClosingBackend {
            item: None,
            close_calls: Arc::clone(&close_calls),
            fail_close: false,
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");

        assert_eq!(subscription.next_item().expect("clean end"), None);
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            subscription.next_item().expect("end remains terminal"),
            None
        );
        subscription.close().expect("close after end is idempotent");
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clean_end_close_failure_is_reported_once_and_poisons_generation() {
        let close_calls = Arc::new(AtomicUsize::new(0));
        let mut backend = ClosingBackend {
            item: None,
            close_calls: Arc::clone(&close_calls),
            fail_close: true,
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");

        let error = subscription
            .next_item()
            .expect_err("clean end must report close-hook failure");
        assert!(matches!(error, SubscriptionError::Backend(_)));
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            subscription.close(),
            Err(SubscriptionError::GenerationPoisoned)
        );
        assert_eq!(close_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn commit_requires_the_exact_full_delivery_batch() {
        let delivered = delivery(1, "receipt-one");
        let forged_cursor = DeliveryBatch::new(
            delivered.sequence(),
            ProviderCursor::new("a separately retained durable cursor").expect("bounded cursor"),
            delivered.delivery_id().clone(),
            delivered.events().to_vec(),
        )
        .expect("alternate cursor remains bounded");
        let forged_events = DeliveryBatch::new(
            delivered.sequence(),
            delivered.cursor().clone(),
            delivered.delivery_id().clone(),
            vec![CommittableEvent::Gap(
                ReconciliationGap::new(Some("different payload".to_owned())).expect("bounded gap"),
            )],
        )
        .expect("alternate payload remains bounded");
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([SubscriptionItem::Batch(delivered.clone())])),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        let _ = subscription.next_item().expect("delivery is accepted");

        for forgery in [&forged_cursor, &forged_events] {
            assert_eq!(
                subscription.commit_durable(forgery),
                Err(SubscriptionError::CommitMismatch {
                    expected: delivered.sequence(),
                    observed: forgery.sequence(),
                })
            );
            assert_eq!(subscription.outstanding(), Some(&delivered));
        }
    }

    #[test]
    fn sequence_gap_poisons_generation() {
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([SubscriptionItem::Heartbeat(
                Heartbeat::new(EventSequence::new(2).expect("nonzero sequence")),
            )])),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        assert_eq!(
            subscription.next_item(),
            Err(SubscriptionError::SequenceViolation {
                expected: 1,
                observed: 2
            })
        );
        assert!(subscription.is_poisoned());
    }

    #[test]
    fn maximum_sequence_cannot_repeat_or_wrap() {
        for observed in [u64::MAX, 1] {
            let receive_calls = Arc::new(AtomicUsize::new(0));
            let mut backend = CountingBackend {
                receive_calls: Arc::clone(&receive_calls),
                item: Some(SubscriptionItem::Heartbeat(Heartbeat::new(
                    EventSequence::new(observed).expect("nonzero sequence"),
                ))),
            };
            let mut subscription =
                ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
            let maximum = EventSequence::new(u64::MAX).expect("maximum is nonzero");
            subscription.last_sequence = Some(maximum);
            assert_eq!(
                subscription.next_item(),
                Err(SubscriptionError::SequenceExhausted { previous: maximum })
            );
            assert!(subscription.is_poisoned());
            assert_eq!(receive_calls.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn cursor_and_receipt_have_separate_nominal_types_and_limits() {
        assert!(ProviderCursor::new("").is_err());
        assert!(DeliveryId::new("x".repeat(MAX_TOKEN_BYTES + 1)).is_err());
        let cursor = ProviderCursor::new("same bytes").expect("cursor");
        let receipt = DeliveryId::new("same bytes").expect("receipt");
        assert_eq!(cursor.as_str(), receipt.as_str());
    }

    #[test]
    fn normalized_message_enforces_hot_path_bounds() {
        let result = InboundMessage::new(
            ChannelId::new("channel").expect("channel"),
            MessageId::new("message").expect("message"),
            ThreadId::new("thread").expect("thread"),
            SenderId::new("sender").expect("sender"),
            "x".repeat(MAX_MESSAGE_TEXT_BYTES + 1),
            "2026-01-01T00:00:00Z",
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn attachment_only_message_allows_empty_normalized_text() {
        let message = message(
            "channels/one",
            "users/owner",
            "",
            Some(provider_payload(
                serde_json::json!({"attachment": "present"}),
            )),
        );
        assert!(message.text().is_empty());
        assert!(message.provider_payload().is_some());
    }

    #[test]
    fn timestamp_is_a_valid_rfc3339_instant() {
        for valid in [
            "2024-02-29T23:59:60Z",
            "2026-01-01t00:00:00.123456789+05:30",
            "2026-01-01T00:00:00-00:00",
        ] {
            assert!(InboundMessage::new(
                ChannelId::new("channel").expect("channel"),
                MessageId::new("message").expect("message"),
                ThreadId::new("thread").expect("thread"),
                SenderId::new("sender").expect("sender"),
                "",
                valid,
                false,
            )
            .is_ok());
        }
        for invalid in [
            "2023-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-01-01T24:00:00Z",
            "2026-01-01T00:00:00",
            "2026-01-01T00:00:00.Z",
            "not-a-timestamp",
        ] {
            assert!(InboundMessage::new(
                ChannelId::new("channel").expect("channel"),
                MessageId::new("message").expect("message"),
                ThreadId::new("thread").expect("thread"),
                SenderId::new("sender").expect("sender"),
                "",
                invalid,
                false,
            )
            .is_err());
        }
    }

    #[test]
    fn provider_payload_complete_envelope_has_an_exact_byte_bound() {
        let schema = "fixture.message.v1";
        let fixed_bytes = 21 + schema.len() + 8; // Envelope plus compact `{"x":""}` object.
        let maximum = ProviderPayload::new(
            schema,
            serde_json::Map::from_iter([(
                "x".to_owned(),
                serde_json::Value::String("x".repeat(MAX_PROVIDER_PAYLOAD_BYTES - fixed_bytes)),
            )]),
        )
        .expect("an exact-bound provider payload is valid");
        assert_eq!(maximum.encoded_bytes(), MAX_PROVIDER_PAYLOAD_BYTES);
        assert_eq!(
            serde_json::to_vec(&serde_json::json!({
                "schema": maximum.schema(),
                "data": maximum.data(),
            }))
            .expect("serialize validated provider envelope")
            .len(),
            maximum.encoded_bytes()
        );

        let oversized = ProviderPayload::new(
            schema,
            serde_json::Map::from_iter([(
                "x".to_owned(),
                serde_json::Value::String("x".repeat(MAX_PROVIDER_PAYLOAD_BYTES - fixed_bytes + 1)),
            )]),
        )
        .expect_err("a provider envelope one byte over the bound is refused");
        assert_eq!(
            oversized.detail(),
            format!(
                "provider payload schema and data contain {} encoded bytes; maximum is {MAX_PROVIDER_PAYLOAD_BYTES}",
                MAX_PROVIDER_PAYLOAD_BYTES + 1
            )
        );
    }

    #[test]
    fn provider_payload_bytes_count_toward_batch_limit() {
        let payload = provider_payload(serde_json::Value::String(
            "x".repeat(MAX_PROVIDER_PAYLOAD_BYTES - 64),
        ));
        let first = message("channels/one", "users/owner", "", Some(payload.clone()));
        let second = message("channels/one", "users/owner", "", Some(payload));
        let result = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            vec![
                CommittableEvent::message_created(first),
                CommittableEvent::message_created(second),
            ],
        );
        assert!(result.is_err());
    }

    #[test]
    fn json_escaping_counts_toward_batch_limit() {
        let events = (0..15)
            .map(|_| {
                CommittableEvent::message_created(message(
                    "channels/one",
                    "users/owner",
                    &"\0".repeat(MAX_MESSAGE_TEXT_BYTES),
                    None,
                ))
            })
            .collect();
        let error = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            events,
        )
        .expect_err("JSON escaping must not expand a valid batch past its encoded budget");
        assert!(error.detail().contains("encoded payload"));
    }

    #[test]
    fn backend_configuration_is_optional_and_bounded() {
        assert!(request().backend_configuration().is_none());
        let empty = BackendConfiguration::new("fixture.config.v1", serde_json::Map::new())
            .expect("explicit empty configuration is valid");
        assert!(empty.data().is_empty());

        let schema = "fixture.config.v1";
        let fixed_bytes = 21 + schema.len() + 8; // envelope plus compact `{"x":""}` object
        let maximum = BackendConfiguration::new(
            schema,
            serde_json::Map::from_iter([(
                "x".to_owned(),
                serde_json::Value::String(
                    "x".repeat(MAX_BACKEND_CONFIGURATION_BYTES - fixed_bytes),
                ),
            )]),
        )
        .expect("exact maximum configuration is valid");
        assert_eq!(maximum.encoded_bytes(), MAX_BACKEND_CONFIGURATION_BYTES);
        let oversized = BackendConfiguration::new(
            schema,
            serde_json::Map::from_iter([(
                "x".to_owned(),
                serde_json::Value::String(
                    "x".repeat(MAX_BACKEND_CONFIGURATION_BYTES - fixed_bytes + 1),
                ),
            )]),
        );
        assert!(oversized.is_err());

        let mut nested = serde_json::json!(0);
        for _ in 0..=MAX_BACKEND_CONFIGURATION_DEPTH {
            nested = serde_json::json!({"nested": nested});
        }
        let excessive_depth = BackendConfiguration::new(
            "fixture.config.v1",
            serde_json::Map::from_iter([("resource".to_owned(), nested)]),
        );
        assert!(excessive_depth.is_err());
    }

    #[test]
    fn request_retains_host_controlled_backend_configuration() {
        let configuration = BackendConfiguration::new(
            "fixture.config.v1",
            serde_json::Map::from_iter([(
                "subscription".to_owned(),
                serde_json::Value::String("subscriptions/one".to_owned()),
            )]),
        )
        .expect("configuration");
        let request = request().with_backend_configuration(configuration.clone());
        assert_eq!(request.backend_configuration(), Some(&configuration));
    }

    #[test]
    fn static_json_high_fanout_is_refused_before_stack_growth() {
        let wide_object = serde_json::Map::from_iter(
            (0..=MAX_BACKEND_CONFIGURATION_BYTES)
                .map(|index| (format!("k{index}"), serde_json::Value::Null)),
        );
        let object_error = BackendConfiguration::new("fixture.config.v1", wide_object)
            .expect_err("wide object must exceed the node budget");
        assert!(object_error.detail().contains("pre-encoding budget"));

        let wide_array = vec![serde_json::Value::Null; MAX_BACKEND_CONFIGURATION_BYTES + 1];
        let array_error = BackendConfiguration::new(
            "fixture.config.v1",
            serde_json::Map::from_iter([(
                "resources".to_owned(),
                serde_json::Value::Array(wide_array),
            )]),
        )
        .expect_err("wide array must exceed the node budget");
        assert!(array_error.detail().contains("pre-encoding budget"));
    }

    #[test]
    fn advertised_full_message_data_is_required() {
        let delivery = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            vec![CommittableEvent::message_created(message(
                "channels/one",
                "users/owner",
                "",
                None,
            ))],
        )
        .expect("batch");
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([SubscriptionItem::Batch(delivery)])),
        };
        let resumed = SubscribeRequest::new(
            vec![ChannelId::new("channels/one").expect("channel")],
            vec![SenderId::new("users/owner").expect("sender")],
            Some(ProviderCursor::new("cursor-before").expect("cursor")),
        )
        .expect("request");
        let mut subscription =
            ChatSubscription::open(&mut backend, &resumed).expect("subscription opens");
        assert_eq!(
            subscription.next_item(),
            Err(SubscriptionError::MissingFullMessageData)
        );
    }

    #[test]
    fn maximum_authority_and_batch_bounds_validate_through_membership_sets() {
        let channels = (0..MAX_SUBSCRIPTION_CHANNELS)
            .map(|index| ChannelId::new(format!("channels/{index:02}")).expect("channel"))
            .collect::<Vec<_>>();
        let senders = (0..MAX_ALLOWED_SENDERS)
            .map(|index| SenderId::new(format!("users/{index:03}")).expect("sender"))
            .collect::<Vec<_>>();
        let request = SubscribeRequest::new(
            channels.clone(),
            senders.clone(),
            Some(ProviderCursor::new("cursor-before").expect("cursor")),
        )
        .expect("maximum authority is valid");
        let events = (0..MAX_BATCH_EVENTS)
            .map(|index| {
                let mut item = message(
                    channels.last().expect("channel").as_str(),
                    senders.last().expect("sender").as_str(),
                    "",
                    Some(provider_payload(serde_json::json!({"index": index}))),
                );
                item.message_id =
                    MessageId::new(format!("messages/{index:03}")).expect("message identifier");
                CommittableEvent::message_created(item)
            })
            .collect::<Vec<_>>();
        let delivery = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            events,
        )
        .expect("maximum batch is valid");
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([SubscriptionItem::Batch(delivery.clone())])),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request).expect("subscription opens");
        assert_eq!(
            subscription.next_item().expect("receive"),
            Some(SubscriptionItem::Batch(delivery))
        );
    }

    #[test]
    fn current_head_requires_checkpoint_or_typed_gap_before_messages() {
        let message = InboundMessage::new(
            ChannelId::new("channel").expect("channel"),
            MessageId::new("message").expect("message"),
            ThreadId::new("thread").expect("thread"),
            SenderId::new("sender").expect("sender"),
            "text",
            "2026-01-01T00:00:00Z",
            false,
        )
        .expect("message");
        let first = DeliveryBatch::new(
            EventSequence::new(1).expect("sequence"),
            ProviderCursor::new("cursor").expect("cursor"),
            DeliveryId::new("delivery").expect("delivery"),
            vec![CommittableEvent::message_created(message)],
        )
        .expect("batch");
        let mut backend = Backend {
            capabilities: capabilities(),
            items: Some(VecDeque::from([SubscriptionItem::Batch(first)])),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        assert_eq!(
            subscription.next_item(),
            Err(SubscriptionError::MissingInitialBoundary)
        );
    }

    #[test]
    fn independent_handle_cancels_blocked_subscription_establishment() {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let cancellation = BlockingCancellation {
            state: Arc::clone(&state),
        };
        let (entered, entered_receiver) = mpsc::sync_channel(1);
        let mut backend = BlockingSubscribeBackend {
            cancellation,
            entered,
        };
        let handle = backend.cancellation();
        let (result_sender, result_receiver) = mpsc::channel();
        let opener = thread::spawn(move || {
            result_sender
                .send(ChatSubscription::open(&mut backend, &request()).map(|_| ()))
                .expect("publish establishment result");
        });
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("subscription establishment blocks");
        handle.cancel().expect("cancellation cleanup is certain");
        let error = result_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked establishment is interrupted")
            .expect_err("cancelled establishment fails");
        assert!(matches!(
            error,
            SubscriptionError::Backend(ref failure) if failure.code() == "fixture_cancelled"
        ));
        opener.join().expect("establishment thread joins");
    }

    #[test]
    fn independent_handle_cancels_blocked_receive() {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let cancellation = BlockingCancellation {
            state: Arc::clone(&state),
        };
        let (entered, entered_receiver) = mpsc::sync_channel(1);
        let mut backend = BlockingReceiveBackend {
            cancellation,
            entered: Some(entered),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        let handle = subscription.cancellation();
        let (result_sender, result_receiver) = mpsc::channel();
        let receiver = thread::spawn(move || {
            result_sender
                .send(subscription.next_item())
                .expect("publish receive result");
        });
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("receive blocks");
        handle.cancel().expect("cancellation cleanup is certain");
        let error = result_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked receive is interrupted")
            .expect_err("cancelled receive fails");
        assert!(matches!(
            error,
            SubscriptionError::Backend(ref failure) if failure.code() == "fixture_cancelled"
        ));
        receiver.join().expect("receive thread joins");
    }

    #[test]
    fn cancellation_is_sticky_across_a_receive_start_race() {
        let state = Arc::new((Mutex::new(false), Condvar::new()));
        let cancellation = BlockingCancellation {
            state: Arc::clone(&state),
        };
        let (entered, entered_receiver) = mpsc::sync_channel(1);
        let mut backend = BlockingReceiveBackend {
            cancellation,
            entered: Some(entered),
        };
        let mut subscription =
            ChatSubscription::open(&mut backend, &request()).expect("subscription opens");
        subscription
            .cancellation()
            .cancel()
            .expect("pre-receive cancellation cleanup is certain");
        let error = subscription
            .next_item()
            .expect_err("a receive admitted after terminal cancellation cannot block");
        entered_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the racing receive was entered and observed sticky cancellation");
        assert!(matches!(
            error,
            SubscriptionError::Backend(ref failure) if failure.code() == "fixture_cancelled"
        ));
    }
}
