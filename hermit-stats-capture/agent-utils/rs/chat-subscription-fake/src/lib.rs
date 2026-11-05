//! Deterministic static and process-plugin conformance backend.
//!
//! The fake performs no network or filesystem I/O. Tests supply a finite ordered script, inspect
//! receive/acknowledgement counts, and run the same consumer assertions against a directly linked
//! backend and the framed process adapter.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex, MutexGuard};

use chat_subscription::{
    BackendCapabilities, BackendConfiguration, BackendFailure, CancellationError, ChannelId,
    ChatSubscriptionBackend, ChatSubscriptionCancellation, ChatSubscriptionDriver,
    CommittableEvent, DeliveryBatch, DeliveryId, EventKind, EventSequence, Heartbeat,
    InboundMessage, MessageId, ProviderCursor, ProviderPayload, ReconciliationGap, ReplaySupport,
    SenderId, SubscribeRequest, SubscriptionItem, ThreadId,
};

struct FakeCancellation;

impl ChatSubscriptionCancellation for FakeCancellation {
    fn cancel(&self) -> Result<(), CancellationError> {
        Ok(())
    }
}

fn fake_cancellation() -> Arc<dyn ChatSubscriptionCancellation> {
    Arc::new(FakeCancellation)
}

/// Observable fake hot-loop activity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FakeObservation {
    /// Number of provider receive calls.
    pub receive_calls: usize,
    /// Delivery IDs acknowledged in order.
    pub acknowledged_delivery_ids: Vec<String>,
    /// Subscription authorities opened by the backend.
    pub subscriptions: Vec<FakeSubscriptionAuthority>,
}

/// One authority record observed by the fake at subscription start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeSubscriptionAuthority {
    /// Exact channel/space allowlist.
    pub channel_ids: Vec<String>,
    /// Exact authenticated sender allowlist.
    pub allowed_senders: Vec<String>,
    /// Inclusive durable replay boundary.
    pub resume_from: Option<String>,
    /// Exact host-controlled backend configuration.
    pub backend_configuration: Option<BackendConfiguration>,
}

#[derive(Clone)]
struct SharedObservation(Arc<Mutex<FakeObservation>>);

impl SharedObservation {
    fn lock(&self) -> MutexGuard<'_, FakeObservation> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A finite deterministic backend that implements the production object-safe traits.
pub struct FakeBackend {
    capabilities: BackendCapabilities,
    script: Option<VecDeque<SubscriptionItem>>,
    observation: SharedObservation,
    required_configuration: Option<BackendConfiguration>,
}

impl FakeBackend {
    /// Construct a single-use backend with an explicit ordered script.
    #[must_use]
    pub fn new(capabilities: BackendCapabilities, script: Vec<SubscriptionItem>) -> Self {
        Self {
            capabilities,
            script: Some(script.into()),
            observation: SharedObservation(Arc::new(Mutex::new(FakeObservation::default()))),
            required_configuration: None,
        }
    }

    /// Snapshot observable calls without sharing mutable fake internals.
    #[must_use]
    pub fn observation(&self) -> FakeObservation {
        self.observation.lock().clone()
    }

    /// Build the standard message/checkpoint/gap/heartbeat conformance script.
    ///
    /// # Panics
    ///
    /// Panics only if compile-time fixture constants violate public domain bounds.
    #[must_use]
    pub fn conformance(backend_name: &str) -> Self {
        let capabilities = BackendCapabilities::new(
            backend_name,
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
        .expect("fixture capabilities are valid");
        let provider_payload = ProviderPayload::new(
            "fake.message.v1",
            serde_json::Map::from_iter([(
                "provider_kind".to_owned(),
                serde_json::Value::String("fixture".to_owned()),
            )]),
        )
        .expect("fixture provider payload is valid");
        let message = InboundMessage::new(
            ChannelId::new("channels/conformance").expect("fixture channel"),
            MessageId::new("messages/one").expect("fixture message"),
            ThreadId::new("threads/one").expect("fixture thread"),
            SenderId::new("users/owner").expect("fixture sender"),
            "first message",
            "2026-01-01T00:00:00Z",
            true,
        )
        .expect("fixture message is valid")
        .with_provider_payload(provider_payload);
        let script = vec![
            SubscriptionItem::Batch(fixture_delivery(
                1,
                CommittableEvent::message_created(message),
            )),
            SubscriptionItem::Batch(fixture_delivery(2, CommittableEvent::Checkpoint)),
            SubscriptionItem::Batch(fixture_delivery(
                3,
                CommittableEvent::Gap(
                    ReconciliationGap::new(Some("fixture replay gap".to_owned()))
                        .expect("fixture gap is valid"),
                ),
            )),
            SubscriptionItem::Heartbeat(Heartbeat::new(
                EventSequence::new(4).expect("fixture sequence is nonzero"),
            )),
        ];
        let mut backend = Self::new(capabilities, script);
        backend.required_configuration = Some(
            BackendConfiguration::new(
                "fixture.config.v1",
                serde_json::Map::from_iter([(
                    "subscription".to_owned(),
                    serde_json::Value::String("subscriptions/conformance".to_owned()),
                )]),
            )
            .expect("fixture backend configuration is valid"),
        );
        backend
    }
}

fn fixture_delivery(sequence: u64, event: CommittableEvent) -> DeliveryBatch {
    DeliveryBatch::new(
        EventSequence::new(sequence).expect("fixture sequence is nonzero"),
        ProviderCursor::new(format!("cursor-{sequence}")).expect("fixture cursor is valid"),
        DeliveryId::new(format!("delivery-{sequence}")).expect("fixture delivery id is valid"),
        vec![event],
    )
    .expect("fixture delivery batch is bounded")
}

impl ChatSubscriptionBackend for FakeBackend {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        fake_cancellation()
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.capabilities.clone()
    }

    fn subscribe(
        &mut self,
        request: &SubscribeRequest,
    ) -> Result<Box<dyn ChatSubscriptionDriver>, BackendFailure> {
        let script = self.script.take().ok_or_else(|| {
            BackendFailure::new(
                "fake_already_subscribed",
                "the deterministic fake supports one subscription",
                false,
            )
            .expect("constant failure is valid")
        })?;
        if self.required_configuration.as_ref() != request.backend_configuration() {
            return Err(BackendFailure::new(
                "fake_configuration_mismatch",
                "the conformance backend did not receive its exact host configuration",
                false,
            )
            .expect("constant failure is valid"));
        }
        self.observation
            .lock()
            .subscriptions
            .push(FakeSubscriptionAuthority {
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
                resume_from: request
                    .resume_from()
                    .map(|cursor| cursor.as_str().to_owned()),
                backend_configuration: request.backend_configuration().cloned(),
            });
        Ok(Box::new(FakeDriver {
            script,
            pending: None,
            observation: self.observation.clone(),
        }))
    }
}

struct FakeDriver {
    script: VecDeque<SubscriptionItem>,
    pending: Option<DeliveryId>,
    observation: SharedObservation,
}

impl ChatSubscriptionDriver for FakeDriver {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        fake_cancellation()
    }

    fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
        if self.pending.is_some() {
            return Err(BackendFailure::new(
                "fake_backpressure_violation",
                "fake driver received next_item before acknowledgement",
                false,
            )
            .expect("constant failure is valid"));
        }
        self.observation.lock().receive_calls += 1;
        let item = self.script.pop_front();
        if let Some(SubscriptionItem::Batch(delivery)) = &item {
            self.pending = Some(delivery.delivery_id().clone());
        }
        Ok(item)
    }

    fn acknowledge(&mut self, delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
        let pending = self.pending.as_ref().ok_or_else(|| {
            BackendFailure::new(
                "fake_no_pending_delivery",
                "fake driver has no delivery to acknowledge",
                false,
            )
            .expect("constant failure is valid")
        })?;
        if pending != delivery_id {
            return Err(BackendFailure::new(
                "fake_delivery_id_mismatch",
                "fake acknowledgement did not match the pending delivery id",
                false,
            )
            .expect("constant failure is valid"));
        }
        self.observation
            .lock()
            .acknowledged_delivery_ids
            .push(delivery_id.as_str().to_owned());
        self.pending = None;
        Ok(())
    }
}
