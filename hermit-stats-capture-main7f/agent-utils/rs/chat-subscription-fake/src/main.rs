//! Standalone conformance plugin used by process-protocol tests and host smoke checks.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chat_subscription::{
    BackendCapabilities, BackendFailure, CancellationError, ChatSubscriptionBackend,
    ChatSubscriptionCancellation, ChatSubscriptionDriver, DeliveryId, EventKind, ReplaySupport,
    SubscribeRequest, SubscriptionItem,
};
use chat_subscription_fake::FakeBackend;

fn main() {
    if let Ok(mode) = std::env::var("CHAT_SUBSCRIPTION_FAKE_STALL") {
        if mode == "blocking-next-graceful" {
            run_blocking_close_fixture();
            return;
        }
        if mode == "serve-natural-end" {
            let capabilities = BackendCapabilities::new(
                "natural-end-process-fixture",
                ReplaySupport::Cursor,
                false,
                NonZeroU16::new(1).expect("one is nonzero"),
                vec![EventKind::Heartbeat],
            )
            .expect("natural-End capabilities");
            let mut backend = FakeBackend::new(capabilities, Vec::new());
            if let Err(error) = chat_subscription_plugin::serve_stdio(&mut backend) {
                eprintln!("natural-End fake chat plugin failed: {error}");
                std::process::exit(1);
            }
            return;
        }
        if let Err(error) = run_stalled_fixture(&mode) {
            eprintln!("stalled fake chat plugin failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let mut backend = FakeBackend::conformance("fake-process-plugin");
    if let Err(error) = chat_subscription_plugin::serve_stdio(&mut backend) {
        eprintln!("fake chat plugin failed: {error}");
        std::process::exit(1);
    }
}

#[derive(Default)]
struct BlockingState {
    cancelled: bool,
}

struct BlockingBackend {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
    entered_marker: PathBuf,
    close_marker: PathBuf,
    cancellation_delay: Duration,
}

#[derive(Clone)]
struct BlockingCancellation {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
    delay: Duration,
}

struct BlockingDriver {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
    entered_marker: PathBuf,
    close_marker: PathBuf,
    cancellation_delay: Duration,
}

impl ChatSubscriptionCancellation for BlockingCancellation {
    fn cancel(&self) -> Result<(), CancellationError> {
        std::thread::sleep(self.delay);
        let (state, changed) = &*self.state;
        state.lock().expect("blocking state").cancelled = true;
        changed.notify_all();
        Ok(())
    }
}

impl ChatSubscriptionBackend for BlockingBackend {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(BlockingCancellation {
            state: Arc::clone(&self.state),
            delay: self.cancellation_delay,
        })
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities::new(
            "blocking-close-process-fixture",
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
        Ok(Box::new(BlockingDriver {
            state: Arc::clone(&self.state),
            entered_marker: self.entered_marker.clone(),
            close_marker: self.close_marker.clone(),
            cancellation_delay: self.cancellation_delay,
        }))
    }
}

impl ChatSubscriptionDriver for BlockingDriver {
    fn cancellation(&self) -> Arc<dyn ChatSubscriptionCancellation> {
        Arc::new(BlockingCancellation {
            state: Arc::clone(&self.state),
            delay: self.cancellation_delay,
        })
    }

    fn next_item(&mut self) -> Result<Option<SubscriptionItem>, BackendFailure> {
        std::fs::write(&self.entered_marker, b"entered").expect("record blocked receive admission");
        let (state, changed) = &*self.state;
        let mut state = state.lock().expect("blocking state");
        while !state.cancelled {
            state = changed.wait(state).expect("blocking receive wait");
        }
        Err(BackendFailure::new(
            "fixture_cancelled",
            "blocking fixture receive was cancelled",
            true,
        )
        .expect("fixture cancellation failure"))
    }

    fn acknowledge(&mut self, _delivery_id: &DeliveryId) -> Result<(), BackendFailure> {
        Ok(())
    }

    fn close(&mut self) -> Result<(), BackendFailure> {
        assert!(
            self.state.0.lock().expect("blocking state").cancelled,
            "semantic close must follow cancellation"
        );
        std::fs::write(&self.close_marker, b"closed").expect("record semantic close");
        Ok(())
    }
}

fn run_blocking_close_fixture() {
    let entered_marker = PathBuf::from(
        std::env::var_os("CHAT_SUBSCRIPTION_FAKE_ENTERED_MARKER")
            .expect("blocking fixture entered marker"),
    );
    let close_marker = PathBuf::from(
        std::env::var_os("CHAT_SUBSCRIPTION_FAKE_CLOSE_MARKER")
            .expect("blocking fixture close marker"),
    );
    let mut backend = BlockingBackend {
        state: Arc::new((Mutex::new(BlockingState::default()), Condvar::new())),
        entered_marker,
        close_marker,
        cancellation_delay: std::env::var("CHAT_SUBSCRIPTION_FAKE_CANCEL_DELAY_MILLIS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(Duration::ZERO, Duration::from_millis),
    };
    if let Err(error) = chat_subscription_plugin::serve_stdio(&mut backend) {
        eprintln!("blocking fake chat plugin failed: {error}");
        std::process::exit(1);
    }
}

fn read_frame(reader: &mut impl Read) -> io::Result<serde_json::Value> {
    let mut header = [0_u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > chat_subscription_plugin::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fixture received an invalid frame length",
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(io::Error::other)
}

fn write_frame(writer: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    let payload = serde_json::to_vec(value).map_err(io::Error::other)?;
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "fixture frame is too large"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

fn expect_type(value: &serde_json::Value, expected: &str) -> io::Result<()> {
    if value.get("type").and_then(serde_json::Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("fixture expected {expected}, found {value}"),
        ))
    }
}

fn hello(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    expect_type(&read_frame(reader)?, "hello")?;
    write_frame(
        writer,
        &serde_json::json!({
            "type": "hello",
            "version": 1,
            "capabilities": {
                "backend_name": "stall-fixture",
                "replay": "cursor",
                "full_message_data": false,
                "max_uncommitted": 1,
                "event_kinds": ["checkpoint", "heartbeat"]
            }
        }),
    )
}

fn start(reader: &mut impl Read, writer: &mut impl Write) -> io::Result<()> {
    expect_type(&read_frame(reader)?, "start")?;
    write_frame(writer, &serde_json::json!({"type": "subscribed"}))
}

fn stall() {
    std::thread::sleep(Duration::from_secs(60));
}

fn run_stalled_fixture(mode: &str) -> io::Result<()> {
    let input = io::stdin();
    let output = io::stdout();
    let mut reader = input.lock();
    let mut writer = output.lock();
    match mode {
        "hello" => stall(),
        "start" => {
            hello(&mut reader, &mut writer)?;
            stall();
        }
        "commit" => {
            hello(&mut reader, &mut writer)?;
            start(&mut reader, &mut writer)?;
            write_frame(
                &mut writer,
                &serde_json::json!({
                    "type": "item",
                    "item": {
                        "kind": "batch",
                        "sequence": 1,
                        "provider_cursor": "cursor-one",
                        "delivery_id": "delivery-one",
                        "events": [{"kind": "checkpoint"}]
                    }
                }),
            )?;
            expect_type(&read_frame(&mut reader)?, "commit")?;
            stall();
        }
        "end-exit-7" => {
            hello(&mut reader, &mut writer)?;
            start(&mut reader, &mut writer)?;
            write_frame(&mut writer, &serde_json::json!({"type": "end"}))?;
            std::process::exit(7);
        }
        "end-stall" => {
            hello(&mut reader, &mut writer)?;
            start(&mut reader, &mut writer)?;
            write_frame(&mut writer, &serde_json::json!({"type": "end"}))?;
            stall();
        }
        "close-before-start" => {
            hello(&mut reader, &mut writer)?;
            expect_type(&read_frame(&mut reader)?, "close")?;
            stall();
        }
        "close-before-start-clean" => {
            hello(&mut reader, &mut writer)?;
            expect_type(&read_frame(&mut reader)?, "close")?;
        }
        "start-delay-close-clean" => {
            hello(&mut reader, &mut writer)?;
            expect_type(&read_frame(&mut reader)?, "start")?;
            let marker = PathBuf::from(
                std::env::var_os("CHAT_SUBSCRIPTION_FAKE_START_MARKER")
                    .expect("delayed Start marker"),
            );
            std::fs::write(marker, b"start-entered").expect("record delayed Start admission");
            let delay = std::env::var("CHAT_SUBSCRIPTION_FAKE_START_DELAY_MILLIS")
                .expect("delayed Start duration")
                .parse::<u64>()
                .expect("delayed Start duration is an integer");
            std::thread::sleep(Duration::from_millis(delay));
            write_frame(&mut writer, &serde_json::json!({"type": "subscribed"}))?;
            expect_type(&read_frame(&mut reader)?, "close")?;
        }
        "close" => {
            hello(&mut reader, &mut writer)?;
            start(&mut reader, &mut writer)?;
            expect_type(&read_frame(&mut reader)?, "close")?;
            stall();
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown stall mode {other}"),
            ));
        }
    }
    Ok(())
}
