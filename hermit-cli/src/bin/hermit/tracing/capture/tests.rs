use std::sync::Mutex;

use super::*;

#[derive(Clone, Default)]
struct Bytes(Arc<Mutex<Vec<u8>>>);

impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn destination_preserves_original_prefix_and_marker_and_keeps_draining() {
    for limit in [0, 1, 3, 8, 100] {
        let bytes = Bytes::default();
        let reference = Bytes::default();
        let mut destination = Destination::new(bytes.clone(), limit);
        let mut original = BoundedWriter::new(reference.clone(), limit);
        for record in [b"a\0b".as_slice(), b"four", b"last"] {
            destination.write_all(record).unwrap();
            original.write_all(record).unwrap();
        }
        destination.flush().unwrap();
        assert_eq!(*bytes.0.lock().unwrap(), *reference.0.lock().unwrap());
        let progress = destination.progress();
        assert_eq!(
            progress.acknowledged_data_bytes,
            if limit == 0 { 11 } else { limit.min(11) }
        );
        assert_eq!(
            progress.discarded_bytes,
            11 - progress.acknowledged_data_bytes
        );
        assert_eq!(progress.output_ceiling, limit != 0 && limit < 11);
        assert_eq!(progress.marker_complete, progress.output_ceiling);
        assert!(!progress.marker_failed);
    }
}

#[test]
fn failed_partial_marker_is_not_complete() {
    struct Fails {
        left: usize,
    }
    impl Write for Fails {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::Error::other("destination failure"));
            }
            let count = self.left.min(bytes.len());
            self.left -= count;
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut destination = Destination::new(Fails { left: 5 }, 3);
    assert!(destination.write_all(b"long-record").is_err());
    let progress = destination.progress();
    assert_eq!(progress.acknowledged_data_bytes, 3);
    assert_eq!(progress.marker_bytes, 2);
    assert!(progress.marker_failed);
    assert!(!progress.marker_complete);
}

#[test]
fn process_global_capture_two_independent_children() {
    for mode in ["cancel", "cancel", "timeout", "ceiling"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tracing::capture::tests::process_global_capture_child",
                "--test-threads=1",
            ])
            .env("HERMIT_TEST_COMMON_CAPTURE_CHILD", mode)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn process_global_capture_child() {
    let Ok(mode) = std::env::var("HERMIT_TEST_COMMON_CAPTURE_CHILD") else {
        return;
    };
    let bytes = Bytes::default();
    let capture = Capture::install(
        bytes.clone(),
        if mode == "ceiling" { 3 } else { 0 },
        EffectiveFilter::from_directives_lossy(
            "common_capture=info",
            ::tracing::metadata::LevelFilter::OFF,
        ),
    )
    .unwrap();
    ::tracing::info!(target: "common_capture", "before-global-tool");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::spawn(async {
            ::tracing::info!(target: "common_capture", "tokio-task-one");
        })
        .await
        .unwrap();
        tokio::spawn(async {
            ::tracing::info!(target: "common_capture", "tokio-task-two");
        })
        .await
        .unwrap();
        tokio::task::spawn_blocking(|| {
            ::tracing::info!(target: "common_capture", "blocking-task");
        })
        .await
        .unwrap();
    });
    drop(runtime);
    std::thread::spawn(|| {
        ::tracing::info!(target: "common_capture", "standard-thread");
    })
    .join()
    .unwrap();
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            ::tracing::info!(target: "common_capture", "cleanup-drop");
        }
    }
    drop(Cleanup);
    let error = capture
        .run::<()>(|input| {
            drop(input);
            ::tracing::info!(target: "common_capture", "host-after-guest-cancel");
            if mode == "timeout" {
                Err(hermit::GuestTimedOut {
                    limit: Duration::from_secs(7),
                }
                .into())
            } else {
                Err(io::Error::new(io::ErrorKind::Interrupted, "primary-before-launch").into())
            }
        })
        .unwrap_err();
    let retained = error.downcast_ref::<CaptureError>().unwrap();
    if mode == "timeout" {
        assert_eq!(
            retained
                .primary
                .downcast_ref::<hermit::GuestTimedOut>()
                .unwrap()
                .limit,
            Duration::from_secs(7)
        );
    } else {
        assert_eq!(
            retained.primary.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Interrupted
        );
    }
    assert!(!retained.evidence.capture.qualifies());
    assert!(retained.evidence.record_status_retained);
    assert!(retained.evidence.record_failure.is_none());
    assert!(retained.evidence.run.is_none());
    let text = String::from_utf8(bytes.0.lock().unwrap().clone()).unwrap();
    if mode == "timeout" {
        assert_eq!(
            hermit::SerializableError::from(error).kind(),
            hermit::FailureKind::RunTimeout
        );
    }
    if mode == "ceiling" {
        assert_eq!(&text.as_bytes()[3..], TRUNCATION_MARKER);
        return;
    }
    let messages: Vec<_> = text
        .lines()
        .map(|line| line.split("common_capture: ").nth(1).unwrap())
        .collect();
    assert_eq!(
        messages,
        [
            "before-global-tool",
            "tokio-task-one",
            "tokio-task-two",
            "blocking-task",
            "standard-thread",
            "cleanup-drop",
            "host-after-guest-cancel"
        ]
    );
}
