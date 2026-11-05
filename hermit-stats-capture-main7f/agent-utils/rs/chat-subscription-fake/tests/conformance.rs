//! Shared conformance suite for direct and out-of-process fake backends.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chat_subscription::{
    BackendConfiguration, BackendFailure, ChannelId, ChatSubscription, ChatSubscriptionBackend,
    CommittableEvent, EventKind, ProviderCursor, SenderId, SubscribeRequest, SubscriptionError,
    SubscriptionItem,
};
use chat_subscription_fake::{FakeBackend, FakeSubscriptionAuthority};
use chat_subscription_plugin::process::{ProcessPhaseTimeouts, ProcessPluginChild};

fn request() -> SubscribeRequest {
    SubscribeRequest::new(
        vec![ChannelId::new("channels/conformance").expect("fixture channel")],
        vec![SenderId::new("users/owner").expect("fixture sender")],
        Some(ProviderCursor::new("durable-before").expect("fixture cursor")),
    )
    .expect("fixture authority")
    .with_backend_configuration(
        BackendConfiguration::new(
            "fixture.config.v1",
            serde_json::Map::from_iter([(
                "subscription".to_owned(),
                serde_json::Value::String("subscriptions/conformance".to_owned()),
            )]),
        )
        .expect("fixture configuration"),
    )
}

fn maximum_authority_request() -> SubscribeRequest {
    let channels = (0..chat_subscription::MAX_SUBSCRIPTION_CHANNELS)
        .map(|index| {
            ChannelId::new(format!(
                "{index:02}{}",
                "\0".repeat(chat_subscription::MAX_RESOURCE_ID_BYTES - 2)
            ))
            .expect("maximum channel")
        })
        .collect();
    let senders = (0..chat_subscription::MAX_ALLOWED_SENDERS)
        .map(|index| {
            SenderId::new(format!(
                "{index:03}{}",
                "\0".repeat(chat_subscription::MAX_SENDER_ID_BYTES - 3)
            ))
            .expect("maximum sender")
        })
        .collect();
    SubscribeRequest::new(
        channels,
        senders,
        Some(ProviderCursor::new("durable-before").expect("fixture cursor")),
    )
    .expect("maximum authority remains valid")
}

fn exercise(backend: &mut dyn ChatSubscriptionBackend, expected_name: &str) {
    let capabilities = backend.capabilities();
    assert_eq!(capabilities.backend_name(), expected_name);
    assert!(capabilities.full_message_data());
    assert_eq!(capabilities.max_uncommitted().get(), 1);
    assert!(capabilities.supports(EventKind::Gap));

    let mut subscription =
        ChatSubscription::open(backend, &request()).expect("conformance subscription opens");
    let first = match subscription.next_item().expect("first item") {
        Some(SubscriptionItem::Batch(delivery)) => delivery,
        other => panic!("expected first delivery, found {other:?}"),
    };
    assert!(matches!(
        first.events(),
        [CommittableEvent::MessageCreated(message)]
            if message.message_id().as_str() == "messages/one"
    ));
    assert_eq!(first.cursor().as_str(), "cursor-1");
    assert_eq!(first.delivery_id().as_str(), "delivery-1");
    assert_eq!(
        subscription.next_item(),
        Err(SubscriptionError::OutstandingDelivery {
            sequence: first.sequence()
        })
    );
    subscription
        .commit_durable(&first)
        .expect("first durable commit is acknowledged");

    let second = match subscription.next_item().expect("second item") {
        Some(SubscriptionItem::Batch(delivery)) => delivery,
        other => panic!("expected checkpoint delivery, found {other:?}"),
    };
    assert_eq!(second.sequence().get(), 2);
    assert!(matches!(second.events(), [CommittableEvent::Checkpoint]));
    assert_eq!(
        subscription.commit_durable(&first),
        Err(SubscriptionError::CommitMismatch {
            expected: second.sequence(),
            observed: first.sequence(),
        })
    );
    subscription
        .commit_durable(&second)
        .expect("checkpoint commits");

    let gap = match subscription.next_item().expect("gap item") {
        Some(SubscriptionItem::Batch(delivery)) => delivery,
        other => panic!("expected gap delivery, found {other:?}"),
    };
    assert!(gap
        .events()
        .iter()
        .any(CommittableEvent::requires_reconciliation));
    subscription.commit_durable(&gap).expect("gap commits");
    assert!(matches!(
        subscription.next_item().expect("heartbeat item"),
        Some(SubscriptionItem::Heartbeat(heartbeat)) if heartbeat.sequence().get() == 4
    ));
    assert_eq!(subscription.next_item().expect("clean end"), None);
    assert_eq!(subscription.next_item().expect("stable clean end"), None);
}

#[test]
fn statically_linked_backend_obeys_conformance_contract() {
    let mut backend = FakeBackend::conformance("fake-static");
    exercise(&mut backend, "fake-static");
    let observation = backend.observation();
    assert_eq!(observation.receive_calls, 5);
    assert_eq!(
        observation.acknowledged_delivery_ids,
        ["delivery-1", "delivery-2", "delivery-3"]
    );
    assert_eq!(
        observation.subscriptions,
        [FakeSubscriptionAuthority {
            channel_ids: vec!["channels/conformance".to_owned()],
            allowed_senders: vec!["users/owner".to_owned()],
            resume_from: Some("durable-before".to_owned()),
            backend_configuration: request().backend_configuration().cloned(),
        }]
    );
}

fn timeouts() -> ProcessPhaseTimeouts {
    ProcessPhaseTimeouts::new(
        Duration::from_millis(500),
        Duration::from_millis(200),
        Duration::from_millis(200),
        Duration::from_millis(200),
        Duration::from_millis(200),
    )
    .expect("fixture deadlines are valid")
}

fn spawn(stall: Option<&str>) -> ProcessPluginChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-subscription-fake-plugin"));
    if let Some(mode) = stall {
        command.env("CHAT_SUBSCRIPTION_FAKE_STALL", mode);
    }
    ProcessPluginChild::spawn(command).expect("fake plugin starts")
}

fn spawn_blocking_close(entered_marker: &PathBuf, close_marker: &PathBuf) -> ProcessPluginChild {
    spawn_blocking_close_with_delay(entered_marker, close_marker, None)
}

fn spawn_blocking_close_with_delay(
    entered_marker: &PathBuf,
    close_marker: &PathBuf,
    cancellation_delay_millis: Option<u64>,
) -> ProcessPluginChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-subscription-fake-plugin"));
    command
        .env("CHAT_SUBSCRIPTION_FAKE_STALL", "blocking-next-graceful")
        .env("CHAT_SUBSCRIPTION_FAKE_ENTERED_MARKER", entered_marker)
        .env("CHAT_SUBSCRIPTION_FAKE_CLOSE_MARKER", close_marker);
    if let Some(delay) = cancellation_delay_millis {
        command.env(
            "CHAT_SUBSCRIPTION_FAKE_CANCEL_DELAY_MILLIS",
            delay.to_string(),
        );
    }
    ProcessPluginChild::spawn(command).expect("blocking fake plugin starts")
}

fn spawn_delayed_start_close(start_marker: &PathBuf) -> ProcessPluginChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chat-subscription-fake-plugin"));
    command
        .env("CHAT_SUBSCRIPTION_FAKE_STALL", "start-delay-close-clean")
        .env("CHAT_SUBSCRIPTION_FAKE_START_MARKER", start_marker)
        .env("CHAT_SUBSCRIPTION_FAKE_START_DELAY_MILLIS", "100");
    ProcessPluginChild::spawn(command).expect("delayed-Start fake plugin starts")
}

fn write_json_frame(writer: &mut impl Write, value: &serde_json::Value) {
    let payload = serde_json::to_vec(value).expect("serialize protocol frame");
    writer
        .write_all(
            &u32::try_from(payload.len())
                .expect("fixture frame length fits u32")
                .to_be_bytes(),
        )
        .expect("write protocol frame length");
    writer
        .write_all(&payload)
        .expect("write protocol frame payload");
    writer.flush().expect("flush protocol frame");
}

fn read_json_frame(reader: &mut impl Read) -> serde_json::Value {
    let mut length = [0_u8; 4];
    reader
        .read_exact(&mut length)
        .expect("read protocol frame length");
    let mut payload = vec![0_u8; u32::from_be_bytes(length) as usize];
    reader
        .read_exact(&mut payload)
        .expect("read protocol frame payload");
    serde_json::from_slice(&payload).expect("decode protocol frame")
}

fn assert_reaped(process_id: u32) {
    assert!(
        !PathBuf::from(format!("/proc/{process_id}")).exists(),
        "plugin leader {process_id} was not reaped"
    );
}

fn assert_bounded(started: Instant) {
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "bounded process operation took {:?}",
        started.elapsed()
    );
}

fn backend_failure_code(error: SubscriptionError) -> String {
    backend_failure(error).code().to_owned()
}

fn backend_failure(error: SubscriptionError) -> BackendFailure {
    match error {
        SubscriptionError::Backend(error) => error,
        other => panic!("expected backend failure, found {other:?}"),
    }
}

#[test]
fn out_of_process_plugin_obeys_same_conformance_contract() {
    let child = spawn(None);
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("plugin handshake");
    exercise(&mut backend, "fake-process-plugin");
    drop(backend);
    let status = cancellation.cancel().expect("plugin is joined and reaped");
    assert!(status.success(), "fake plugin exited with {status}");
    assert_reaped(process_id);
}

#[test]
fn process_plugin_exits_after_end_while_host_input_remains_open() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chat-subscription-fake-plugin"))
        .env("CHAT_SUBSCRIPTION_FAKE_STALL", "serve-natural-end")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn natural-End process plugin");
    let mut writer = child.stdin.take().expect("take plugin input");
    let mut reader = child.stdout.take().expect("take plugin output");
    write_json_frame(
        &mut writer,
        &serde_json::json!({"type": "hello", "min_version": 1, "max_version": 1}),
    );
    assert_eq!(read_json_frame(&mut reader)["type"], "hello");
    write_json_frame(
        &mut writer,
        &serde_json::json!({
            "type": "start",
            "channel_ids": ["channels/conformance"],
            "allowed_senders": ["users/owner"],
            "max_uncommitted": 1,
            "resume_from": "cursor-before",
            "backend_configuration": null
        }),
    );
    assert_eq!(read_json_frame(&mut reader)["type"], "subscribed");
    assert_eq!(read_json_frame(&mut reader)["type"], "end");

    let (status_sender, status_receiver) = mpsc::channel();
    let waiter = thread::spawn(move || {
        status_sender
            .send(child.wait().expect("wait for natural-End plugin"))
            .expect("publish natural-End status");
    });
    let status = status_receiver
        .recv_timeout(Duration::from_millis(500))
        .expect("plugin exits without host stdin EOF");
    assert!(status.success(), "natural-End plugin exited with {status}");
    drop(writer);
    waiter.join().expect("process waiter joins");
}

#[test]
fn stalled_hello_is_bounded_and_reaped() {
    let child = spawn(Some("hello"));
    let process_id = child.id();
    let started = Instant::now();
    let error = match child.connect(timeouts()) {
        Ok(_) => panic!("a plugin that never answers Hello must time out"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "plugin_hello_timeout");
    assert_bounded(started);
    assert_reaped(process_id);
}

#[test]
fn stalled_start_is_bounded_and_reaped() {
    let child = spawn(Some("start"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let started = Instant::now();
    let error = ChatSubscription::open(&mut backend, &maximum_authority_request())
        .err()
        .expect("a plugin that never reads the maximum Start frame must time out");
    assert_eq!(backend_failure_code(error), "plugin_start_timeout");
    assert_bounded(started);
    assert!(!cancellation
        .cancel()
        .expect("stalled plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn stalled_commit_is_bounded_and_reaped() {
    let child = spawn(Some("commit"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let delivery = match subscription.next_item().expect("fixture item") {
        Some(SubscriptionItem::Batch(delivery)) => delivery,
        other => panic!("expected a delivery, found {other:?}"),
    };
    let started = Instant::now();
    let error = subscription
        .commit_durable(&delivery)
        .expect_err("a plugin that never confirms Commit must time out");
    assert_eq!(backend_failure_code(error), "commit_outcome_unknown");
    assert_bounded(started);
    drop(subscription);
    assert!(!cancellation
        .cancel()
        .expect("stalled plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn protocol_end_with_nonzero_exit_is_failure_and_reaped() {
    let child = spawn(Some("end-exit-7"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let started = Instant::now();
    let error = subscription
        .next_item()
        .expect_err("End followed by exit 7 is not clean termination");
    let error = backend_failure(error);
    assert_eq!(error.code(), "plugin_unclean_end");
    assert!(error.detail().contains("exit status: 7"), "{error}");
    assert_bounded(started);
    assert!(!cancellation
        .cancel()
        .expect("nonzero plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn protocol_end_requiring_forced_kill_is_failure_and_reaped() {
    let child = spawn(Some("end-stall"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let started = Instant::now();
    let error = subscription
        .next_item()
        .expect_err("End from a plugin that stays alive is not clean termination");
    let error = backend_failure(error);
    assert_eq!(error.code(), "plugin_unclean_end");
    assert!(error.detail().contains("signal: 9"), "{error}");
    assert_bounded(started);
    assert!(!cancellation
        .cancel()
        .expect("forced plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn stalled_close_is_bounded_and_reaped() {
    let child = spawn(Some("close"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let started = Instant::now();
    let error = subscription
        .close()
        .expect_err("a plugin that ignores Close must be forcibly stopped");
    assert_eq!(backend_failure_code(error), "plugin_forced_shutdown");
    assert_bounded(started);
    assert!(!cancellation
        .cancel()
        .expect("stalled plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn stalled_close_before_start_is_bounded_and_reaped() {
    let child = spawn(Some("close-before-start"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let started = Instant::now();
    let error = backend
        .close()
        .expect_err("a negotiated plugin that ignores Close must be forcibly stopped");
    assert_eq!(error.code(), "plugin_forced_shutdown");
    assert_bounded(started);
    assert!(!cancellation
        .cancel()
        .expect("stalled plugin is reaped")
        .success());
    assert_reaped(process_id);
}

#[test]
fn cancellation_before_start_sends_semantic_close_and_reaps() {
    let child = spawn(Some("close-before-start-clean"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");

    let status = cancellation
        .cancel()
        .expect("cancellation sends Close before Start");
    assert!(status.success(), "pre-Start Close exited with {status}");
    let error = match backend.subscribe(&request()) {
        Err(error) => error,
        Ok(_) => panic!("retired command port unexpectedly admitted Start"),
    };
    assert_eq!(error.code(), "plugin_start_cancelled");
    assert!(error.retryable());
    assert_reaped(process_id);
}

#[test]
fn cancellation_racing_start_queues_close_after_start_and_reaps() {
    let root = std::env::temp_dir().join(format!(
        "chat-subscription-start-cancel-{}",
        std::process::id()
    ));
    fs::create_dir(&root).expect("create Start-cancellation fixture directory");
    let start_marker = root.join("start-entered");
    let child = spawn_delayed_start_close(&start_marker);
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let (start_result, start_result_receiver) = mpsc::channel();
    let starter = thread::spawn(move || {
        let result = backend.subscribe(&request()).map(drop);
        start_result.send(result).expect("publish Start result");
    });

    let marker_deadline = Instant::now() + Duration::from_secs(1);
    while !start_marker.exists() {
        assert!(
            Instant::now() < marker_deadline,
            "plugin did not admit delayed Start"
        );
        thread::yield_now();
    }

    let cancel_handle = cancellation.clone();
    let cancel = thread::spawn(move || cancel_handle.cancel());
    let transaction_deadline = Instant::now() + Duration::from_secs(1);
    while !cancellation.graceful_shutdown_in_progress() {
        assert!(
            Instant::now() < transaction_deadline,
            "cancellation did not publish its graceful transaction"
        );
        thread::yield_now();
    }

    let status = cancel
        .join()
        .expect("cancellation thread joins")
        .expect("Start-racing Close completes");
    assert!(status.success(), "Start-racing Close exited with {status}");
    start_result_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("Start caller finishes")
        .expect("Start completes before queued Close");
    starter.join().expect("Start thread joins");
    assert_reaped(process_id);
    fs::remove_dir_all(root).expect("remove Start-cancellation fixture directory");
}

#[test]
fn independent_cancellation_interrupts_blocking_next_item_and_joins_worker() {
    let child = spawn(Some("close"));
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let (result_sender, result_receiver) = mpsc::channel();
    let receiver_thread = thread::spawn(move || {
        let result = subscription.next_item();
        let _ = result_sender.send(result);
    });
    thread::sleep(Duration::from_millis(50));
    let started = Instant::now();
    let error = cancellation
        .cancel()
        .expect_err("a plugin that stalls after Close requires forced fallback");
    assert!(error.to_string().contains("forced shutdown"), "{error}");
    let result = result_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("blocked next_item returns after cancellation");
    assert!(result.is_err());
    receiver_thread.join().expect("receiver thread joins");
    assert_bounded(started);
    assert_reaped(process_id);
}

#[test]
fn graceful_cancellation_closes_blocked_next_before_clean_process_exit() {
    let root = std::env::temp_dir().join(format!(
        "chat-subscription-graceful-close-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("create graceful-close fixture directory");
    let entered_marker = root.join("entered");
    let close_marker = root.join("closed");
    let child = spawn_blocking_close(&entered_marker, &close_marker);
    let process_id = child.id();
    let (mut backend, cancellation) = child.connect(timeouts()).expect("Hello succeeds");
    let mut subscription =
        ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let (result_sender, result_receiver) = mpsc::channel();
    let receiver_thread = thread::spawn(move || {
        let result = subscription.next_item();
        let _ = result_sender.send(result);
    });
    let entered_deadline = Instant::now() + Duration::from_secs(1);
    while !entered_marker.exists() {
        assert!(
            Instant::now() < entered_deadline,
            "plugin did not enter blocked Next"
        );
        thread::yield_now();
    }
    let host_receive_deadline = Instant::now() + Duration::from_secs(1);
    while !cancellation.receive_in_progress() {
        assert!(
            Instant::now() < host_receive_deadline,
            "host framed worker did not enter blocked Next"
        );
        thread::yield_now();
    }

    let started = Instant::now();
    let status = cancellation
        .cancel()
        .expect("framed Close reaches semantic cleanup and exact process exit");
    assert!(status.success(), "graceful plugin exited with {status}");
    let result = result_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("blocked host Next returns after post-Close EOF")
        .expect_err("graceful cancellation is not a natural stream End");
    let failure = backend_failure(result);
    assert_eq!(failure.code(), "plugin_receive_cancelled", "{failure:?}");
    receiver_thread.join().expect("receiver thread joins");
    assert_eq!(
        fs::read(&close_marker).expect("semantic close marker"),
        b"closed"
    );
    assert_bounded(started);
    assert_reaped(process_id);
    fs::remove_dir_all(root).expect("remove graceful-close fixture directory");
}

#[test]
fn graceful_close_admitted_before_host_next_retains_full_close_cleanup_budget() {
    let root = std::env::temp_dir().join(format!(
        "chat-subscription-queued-graceful-close-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).expect("create queued graceful-close fixture directory");
    let entered_marker = root.join("entered");
    let close_marker = root.join("closed");
    let child = spawn_blocking_close_with_delay(&entered_marker, &close_marker, Some(1_000));
    let process_id = child.id();
    // The 1 s cleanup outlasts the grace, and any other phase deadline plus the grace, so only
    // the Close budget lets it finish. That budget must cover the cleanup plus whatever
    // scheduling stall delays the plugin's exit, so it leaves more than a second to spare.
    let selected_timeouts = ProcessPhaseTimeouts::new(
        Duration::from_millis(500),
        Duration::from_millis(500),
        Duration::from_millis(500),
        Duration::from_millis(2_000),
        Duration::from_millis(200),
    )
    .expect("queued Close deadlines");
    let (mut backend, cancellation) = child.connect(selected_timeouts).expect("Hello succeeds");
    let _subscription = ChatSubscription::open(&mut backend, &request()).expect("Start succeeds");
    let entered_deadline = Instant::now() + Duration::from_secs(1);
    while !entered_marker.exists() {
        assert!(
            Instant::now() < entered_deadline,
            "plugin did not enter provider Next"
        );
        thread::yield_now();
    }
    assert!(
        !cancellation.receive_in_progress(),
        "host Next must remain idle so Close uses the queued control path"
    );

    let started = Instant::now();
    let status = cancellation
        .cancel()
        .expect("queued Close retains its full semantic cleanup deadline");
    assert!(
        status.success(),
        "queued graceful plugin exited with {status}"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1_000),
        "fixture did not exercise semantic Close cleanup"
    );
    assert_eq!(
        fs::read(&close_marker).expect("semantic close marker"),
        b"closed"
    );
    assert_bounded(started);
    assert_reaped(process_id);
    fs::remove_dir_all(root).expect("remove queued graceful-close fixture directory");
}
