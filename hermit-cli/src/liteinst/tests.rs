mod session_review {
    use std::io;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use std::time::Instant;

    use hermit::liteinst;
    use hermit::liteinst_bootstrap::EffectiveFilter;
    use reverie_liteinst::retained_guest_log as logs;

    use crate as hermit;

    #[derive(Default)]
    struct Destination {
        flushes: Arc<AtomicUsize>,
        progress: logs::DestinationProgress,
    }

    impl Write for Destination {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.progress.acknowledged_data_bytes += bytes.len() as u64;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl logs::CaptureDestination for Destination {
        fn progress(&self) -> logs::DestinationProgress {
            self.progress
        }
    }

    fn prepare() -> (liteinst::Session, liteinst::LogInput, logs::HostProducer) {
        let options = logs::CaptureOptions {
            limits: logs::CaptureLimits {
                producers: 4,
                slots_per_producer: 4,
                max_record_bytes: 1024,
                host_pending_bytes: 8192,
                guest_pending_bytes: 8192,
                pending_records: 16,
                diagnostic_bytes: 8192,
            },
            timeouts: logs::CaptureTimeouts {
                startup: Duration::from_secs(2),
                blocked_publication: Duration::from_secs(2),
                final_drain: Duration::from_millis(20),
            },
        };
        let filter = EffectiveFilter::from_directives_lossy(
            "detcore=info",
            tracing::metadata::LevelFilter::INFO,
        );
        liteinst::prepare(options, Destination::default(), &filter).unwrap()
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    fn refused_run(input: liteinst::LogInput) -> liteinst::PendingRun<hermit::Output> {
        hermit::run_with_output_backend_timeout_and_log(
            hermit::Command::new("/review-must-not-launch"),
            hermit::DetConfig::default(),
            false,
            &None,
            None,
            input.with_runtime_path("/review-must-not-have-a-runtime.so"),
        )
    }

    #[test]
    fn mismatched_pending_must_not_close_another_sessions_host_domain() {
        let (mut first, first_input, first_host) = prepare();
        let (mut second, second_input, second_host) = prepare();
        second.retain_record_status(|| None).unwrap();
        let pending = refused_run(first_input);
        let refused = second.finish(pending, deadline());
        assert!(refused.result.is_err());
        assert!(
            refused
                .evidence
                .primary
                .unwrap()
                .to_string()
                .contains("another capture")
        );
        let unrelated_cleanup = second_host.write_record(b"unrelated session cleanup\n");
        drop(second_input);
        drop(first_host);
        first.finish_evidence(deadline());
        assert!(
            unrelated_cleanup.is_ok(),
            "wrong-session rejection closed the unrelated host domain: {unrelated_cleanup:?}"
        );
    }

    #[test]
    fn mismatched_pending_must_report_identity_error_not_foreign_run_error() {
        let (mut first, first_input, _first_host) = prepare();
        let (mut second, second_input, _second_host) = prepare();
        second.retain_record_status(|| None).unwrap();
        let refused = second.finish(refused_run(first_input), deadline());
        let error = refused.result.unwrap_err();
        drop(second_input);
        first.finish_evidence(deadline());
        assert!(
            error.to_string().contains("another capture"),
            "returned a different session's primary error: {error}"
        );
    }

    #[test]
    fn finalization_must_seal_record_status_registration() {
        let (mut session, input, _host) = prepare();
        let report = session.finish_evidence(deadline());
        assert!(!report.capture.qualifies());
        let registration = session.retain_record_status(|| None);
        drop(input);
        assert!(
            registration.is_err(),
            "observer registration succeeded after capture finalization"
        );
    }

    #[test]
    fn observer_panic_remains_failure_on_repeated_finalization() {
        let (mut session, input, _host) = prepare();
        session
            .retain_record_status(|| panic!("review observer panic"))
            .unwrap();
        drop(input);
        for _iteration in 0..2 {
            let report = session.finish_evidence(deadline());
            assert!(!report.record_status_retained);
            assert!(
                report
                    .primary
                    .unwrap()
                    .to_string()
                    .contains("observer panicked")
            );
            assert!(!report.capture.qualifies());
        }
    }

    #[test]
    fn prelaunch_record_failure_wins_over_artifact_lookup_and_retains_typed_status() {
        let (mut session, input, host) = prepare();
        let cause = Arc::new(io::Error::other("review formatting failure"));
        let retained = cause.clone();
        session
            .retain_record_status(move || Some(retained.clone()))
            .unwrap();
        let pending = refused_run(input);
        host.write_record(b"failure cleanup\n").unwrap();
        let result = session.finish(pending, deadline());
        assert!(
            result
                .result
                .unwrap_err()
                .to_string()
                .contains("review formatting failure")
        );
        assert!(result.evidence.runtime.is_none());
        assert!(matches!(
            result.evidence.stdio,
            liteinst::StdioEvidence::Unavailable
        ));
        assert!(
            result
                .evidence
                .record_failure
                .unwrap()
                .downcast_ref::<io::Error>()
                .is_some()
        );
    }
}

use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;

mod owned_exit_status {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::ExitStatusExt;

    use reverie_liteinst::run_evidence::FailureEvidence;
    use reverie_liteinst::run_evidence::StreamEvidence;

    use super::*;

    fn completed_run(status: i32, mode: StdioMode) -> RunEvidence {
        let stream = StreamEvidence {
            state: match mode {
                StdioMode::Captured => StreamState::Eof,
                StdioMode::Inherited => StreamState::Inherited,
            },
            chunks: Vec::new(),
        };
        RunEvidence {
            mode,
            polled: true,
            worker_submitted: true,
            caller_cancelled: false,
            completion: RunCompletion::Succeeded,
            spawned: true,
            pid: Some(std::process::id()),
            wait_status: Some(std::process::ExitStatus::from_raw(status)),
            reaped: true,
            wait_error: None,
            first_error: None,
            cleanup_issue: None,
            stdout: stream.clone(),
            stderr: stream,
        }
    }

    fn guest(input: &mut LogInput, finish: bool, fail: bool) {
        let socket = input
            .sink
            .as_mut()
            .unwrap()
            .take_prepared_endpoint()
            .unwrap()
            .unwrap();
        let buffer = logs::ordered::Buffer::receive(socket.as_raw_fd()).unwrap();
        let mut writer = buffer.activate(1, i64::from(std::process::id())).unwrap();
        if finish {
            writer
                .finish(|_, _| {
                    std::thread::yield_now();
                    Ok(())
                })
                .unwrap();
        }
        if fail {
            buffer.fail_guest();
        }
        drop(socket);
        input.shared.handle.root_reaped();
        input.shared.handle.run_state(logs::RunState::Succeeded);
    }

    async fn cleanup(
        forced: Result<bool, Arc<Error>>,
    ) -> (Result<(), Arc<Error>>, Vec<&'static str>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let result = complete_exit_cleanup(
            calls.clone(),
            forced,
            async |calls| calls.lock().unwrap().push("cancel"),
            async |calls| calls.lock().unwrap().push("cleanup"),
        )
        .await;
        let observed = calls.lock().unwrap().clone();
        (result, observed)
    }

    #[tokio::test]
    async fn completed_statuses_preserve_wait_values_and_cleanup_once() {
        let (mut session, mut input, _host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        guest(&mut input, true, false);
        assert_eq!(
            input.shared.handle.guest_finished().await.phase,
            logs::Phase::Complete
        );
        let capture = input.shared.handle.capture_snapshot().unwrap();
        assert!(!capture.host.closed);
        assert!(!capture.qualifies());
        for mode in [StdioMode::Captured, StdioMode::Inherited] {
            for raw in [0, 7 << 8, 125 << 8, 130 << 8, 255 << 8, libc::SIGINT] {
                let status = crate::ExitStatus::from(std::process::ExitStatus::from_raw(raw));
                let run = completed_run(raw, mode);
                let retained = run.wait_status.unwrap();
                let evidence = verify_owned_exit(status, run, &capture).map_err(Arc::new);
                let forced = exit_requires_forced_shutdown(true, status, evidence);
                assert_eq!(forced.as_ref().unwrap(), &(raw == libc::SIGINT));
                let (result, calls) = cleanup(forced).await;
                assert!(result.is_ok());
                assert_eq!(
                    calls,
                    if raw == libc::SIGINT {
                        vec!["cancel", "cleanup"]
                    } else {
                        vec!["cleanup"]
                    }
                );
                assert_eq!(crate::ExitStatus::from(retained), status);
            }
        }
        drop(input);
        session.finish_evidence(deadline());
    }

    #[tokio::test]
    async fn failures_always_cancel_then_cleanup_and_legacy_is_unchanged() {
        let failure = Arc::new(anyhow::anyhow!("retained backend failure"));
        for code in [0, 7, 125, 130, 255] {
            let status = crate::ExitStatus::Exited(code);
            let (result, calls) = cleanup(exit_requires_forced_shutdown(
                true,
                status,
                Err(failure.clone()),
            ))
            .await;
            assert!(Arc::ptr_eq(&result.unwrap_err(), &failure));
            assert_eq!(calls, ["cancel", "cleanup"]);
            let forced =
                exit_requires_forced_shutdown(false, status, Err(failure.clone())).unwrap();
            assert_eq!(forced, matches!(code, 125 | 130));
            let (result, calls) = cleanup(Ok(forced)).await;
            assert!(result.is_ok());
            assert_eq!(
                calls,
                if forced {
                    vec!["cancel", "cleanup"]
                } else {
                    vec!["cleanup"]
                }
            );
        }
    }

    #[tokio::test]
    async fn incomplete_wait_and_capture_mutations_never_select_normal_cleanup() {
        let (mut session, mut input, _host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        guest(&mut input, true, false);
        input.shared.handle.guest_finished().await;
        let capture = input.shared.handle.capture_snapshot().unwrap();
        let failure = || FailureEvidence {
            display: "failure".into(),
            debug: "failure".into(),
        };
        for mode in [StdioMode::Captured, StdioMode::Inherited] {
            for mutation in 0..15 {
                let mut run = completed_run(125 << 8, mode);
                match mutation {
                    0 => run.polled = false,
                    1 => run.worker_submitted = false,
                    2 => run.spawned = false,
                    3 => run.caller_cancelled = true,
                    4 => run.completion = RunCompletion::Interrupted,
                    5 => run.pid = None,
                    6 => run.wait_status = None,
                    7 => run.wait_status = Some(std::process::ExitStatus::from_raw(libc::SIGINT)),
                    8 => run.reaped = false,
                    9 => run.wait_error = Some(failure()),
                    10 => run.first_error = Some(failure()),
                    11 => {
                        run.cleanup_issue = Some(logs::Issue {
                            kind: IssueKind::Cleanup,
                            message: "failure".into(),
                        })
                    }
                    12 => run.stdout.state = StreamState::Reading,
                    13 => run.stderr.state = StreamState::Interrupted,
                    14 => run.pid = Some(0),
                    _ => unreachable!(),
                }
                let evidence = verify_owned_exit(crate::ExitStatus::Exited(125), run, &capture)
                    .map_err(Arc::new);
                assert!(evidence.is_err(), "run mutation {mutation}");
                let (result, calls) = cleanup(exit_requires_forced_shutdown(
                    true,
                    crate::ExitStatus::Exited(125),
                    evidence,
                ))
                .await;
                assert!(result.is_err());
                assert_eq!(calls, ["cancel", "cleanup"]);
            }
        }
        for mutation in 0..13 {
            let mut changed = capture.clone();
            match mutation {
                0 => changed.guest.phase = logs::Phase::Incomplete,
                1 => changed.guest.run = logs::RunState::Cancelled,
                2 => changed.guest.root_reaped = false,
                3 => changed.guest.peer_closed = false,
                4 => changed.guest.issues.push(logs::Issue {
                    kind: IssueKind::Child,
                    message: "failure".into(),
                }),
                5 => changed.guest_admission.closed = false,
                6 => changed.guest_admission.entrants = 1,
                7 => changed.host.closed = true,
                8 => changed.late_host_writes = 1,
                9 => changed.omitted_issues = 1,
                10 => changed.error = Some("capture failed".into()),
                11 => changed.publication.progress.output_ceiling = true,
                12 => changed.publication.progress.marker_failed = true,
                _ => unreachable!(),
            }
            let evidence = verify_owned_exit(
                crate::ExitStatus::Exited(125),
                completed_run(125 << 8, StdioMode::Inherited),
                &changed,
            )
            .map_err(Arc::new);
            assert!(evidence.is_err(), "capture mutation {mutation}");
            let (result, calls) = cleanup(exit_requires_forced_shutdown(
                true,
                crate::ExitStatus::Exited(125),
                evidence,
            ))
            .await;
            assert!(result.is_err());
            assert_eq!(calls, ["cancel", "cleanup"]);
        }
        drop(input);
        session.finish_evidence(deadline());
    }

    #[tokio::test]
    async fn failures_before_and_after_finish_keep_host_cleanup_records() {
        for finish in [false, true] {
            let destination = Destination::default();
            let bytes = destination.bytes.clone();
            let (mut session, mut input, host) =
                prepare(options(), destination, &filter()).unwrap();
            guest(&mut input, finish, true);
            assert_eq!(
                input.shared.handle.guest_finished().await.phase,
                logs::Phase::Incomplete
            );
            let capture = input.shared.handle.capture_snapshot().unwrap();
            let evidence = verify_owned_exit(
                crate::ExitStatus::Exited(125),
                completed_run(125 << 8, StdioMode::Inherited),
                &capture,
            )
            .map_err(Arc::new);
            assert!(evidence.is_err());
            let (result, calls) = cleanup(exit_requires_forced_shutdown(
                true,
                crate::ExitStatus::Exited(125),
                evidence,
            ))
            .await;
            assert!(result.is_err());
            assert_eq!(calls, ["cancel", "cleanup"]);
            host.write_record(b"host cleanup after guest failure\n")
                .unwrap();
            drop(input);
            let final_report = session.finish_evidence(deadline());
            assert!(!final_report.capture.qualifies());
            assert_eq!(
                &*bytes.lock().unwrap(),
                b"host cleanup after guest failure\n"
            );
        }
    }

    #[tokio::test]
    async fn late_publication_failure_does_not_become_successful_capture() {
        let (mut session, mut input, host) = prepare(
            options(),
            Destination {
                fail_flush: true,
                ..Default::default()
            },
            &filter(),
        )
        .unwrap();
        guest(&mut input, true, false);
        input.shared.handle.guest_finished().await;
        let capture = input.shared.handle.capture_snapshot().unwrap();
        verify_owned_exit(
            crate::ExitStatus::Exited(130),
            completed_run(130 << 8, StdioMode::Inherited),
            &capture,
        )
        .unwrap();
        let (result, calls) = cleanup(Ok(false)).await;
        assert!(result.is_ok());
        assert_eq!(calls, ["cleanup"]);
        host.write_record(b"late cleanup\n").unwrap();
        drop(input);
        let final_report = session.finish_evidence(deadline());
        assert!(final_report.capture.publication.error.is_some());
        assert!(!final_report.capture.qualifies());
        assert!(
            verify_owned_exit(
                crate::ExitStatus::Exited(130),
                completed_run(130 << 8, StdioMode::Inherited),
                &final_report.capture
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn busy_observation_yields_without_weakening_poison_or_cancellation() {
        let mut attempts = 0;
        let run = orderly_run_snapshot(
            || {
                attempts += 1;
                if attempts <= 17 {
                    Err(SnapshotUnavailable::Busy)
                } else {
                    Ok(completed_run(125 << 8, StdioMode::Inherited))
                }
            },
            || false,
        )
        .await
        .unwrap();
        assert_eq!(attempts, 18);
        assert_eq!(run.wait_status.unwrap().code(), Some(125));
        assert!(
            orderly_run_snapshot(|| Err(SnapshotUnavailable::Poisoned), || false)
                .await
                .is_err()
        );
        let attempts = std::cell::Cell::new(0);
        assert!(
            orderly_run_snapshot(
                || {
                    attempts.set(attempts.get() + 1);
                    Err(SnapshotUnavailable::Busy)
                },
                || attempts.get() == 3
            )
            .await
            .is_err()
        );
        assert_eq!(attempts.get(), 3);
    }

    #[tokio::test]
    async fn missing_or_failed_session_evidence_retains_first_error_and_cleanup() {
        let (mut session, input, _host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        let missing = owned_exit_evidence(&input.shared, crate::ExitStatus::Exited(0))
            .await
            .unwrap_err();
        assert!(missing.to_string().contains("observer"));
        let repeated = owned_exit_evidence(&input.shared, crate::ExitStatus::Exited(125))
            .await
            .unwrap_err();
        assert!(Arc::ptr_eq(&missing, &repeated));
        let (result, calls) = cleanup(Err(repeated)).await;
        assert!(Arc::ptr_eq(&missing, &result.unwrap_err()));
        assert_eq!(calls, ["cancel", "cleanup"]);
        drop(input);
        session.finish_evidence(deadline());
    }
}

#[derive(Default)]
struct Destination {
    bytes: Arc<Mutex<Vec<u8>>>,
    progress: logs::DestinationProgress,
    fail_flush: bool,
    record_failure_on_flush: Option<Arc<AtomicBool>>,
}

impl Write for Destination {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        self.progress.acknowledged_data_bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if let Some(failure) = &self.record_failure_on_flush {
            failure.store(true, Ordering::Release);
        }
        if self.fail_flush {
            Err(io::Error::other("destination flush failed"))
        } else {
            Ok(())
        }
    }
}
impl CaptureDestination for Destination {
    fn progress(&self) -> logs::DestinationProgress {
        self.progress
    }
}

fn options() -> CaptureOptions {
    CaptureOptions {
        limits: logs::CaptureLimits {
            producers: 4,
            slots_per_producer: 4,
            max_record_bytes: 1024,
            host_pending_bytes: 8192,
            guest_pending_bytes: 8192,
            pending_records: 16,
            diagnostic_bytes: 8192,
        },
        timeouts: logs::CaptureTimeouts {
            startup: Duration::from_secs(2),
            blocked_publication: Duration::from_secs(2),
            final_drain: Duration::from_millis(20),
        },
    }
}

fn filter() -> EffectiveFilter {
    EffectiveFilter::from_directives_lossy(
        "detcore[work{task=1.0}]=info",
        tracing::metadata::LevelFilter::INFO,
    )
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

#[test]
fn prepared_run_observer_is_retained_before_polling_in_both_stdio_modes() {
    fn check<F>(session: &mut Session, observer: RunObserver, future: F, mode: StdioMode) {
        assert!(session.run_observer().is_none());
        session
            .shared
            .retain_run_observer(observer.clone())
            .unwrap();
        assert!(session.shared.retain_run_observer(observer).is_err());
        let retained = session.run_observer().unwrap();
        let before = retained.try_snapshot().unwrap();
        assert!(!before.polled && !before.spawned && !before.reaped);
        assert_eq!(before.mode, mode);
        let expected = if mode == StdioMode::Captured {
            StreamState::NotStarted
        } else {
            StreamState::Inherited
        };
        assert_eq!(before.stdout.state, expected);
        assert_eq!(before.stderr.state, expected);
        drop(future);
        let evidence = session.finish_evidence(deadline());
        let after = evidence.run.unwrap().unwrap();
        assert!(after.caller_cancelled);
        assert!(!after.polled && !after.worker_submitted && !after.spawned && !after.reaped);
        assert_eq!(after.completion, RunCompletion::Interrupted);
        assert_eq!(after.stdout.state, expected);
        assert_eq!(after.stderr.state, expected);
        assert!(after.stdout.bytes().is_empty() && after.stderr.bytes().is_empty());
        assert!(matches!(evidence.stdio, StdioEvidence::Unavailable));
    }
    for captured in [false, true] {
        let (mut session, mut input, _host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        session.retain_record_status(|| None).unwrap();
        let command = Command::new("/must-not-launch");
        let sink = input.sink.take().unwrap();
        if captured {
            let (observer, future) =
                LiteinstBackend::prepare_with_output_and_preload_data_and_log_sink::<()>(
                    command,
                    (),
                    "/missing-runtime",
                    Vec::new(),
                    sink,
                );
            check(&mut session, observer, future, StdioMode::Captured);
        } else {
            let (observer, future) =
                LiteinstBackend::prepare_with_inherited_stdio_and_preload_data_and_log_sink::<()>(
                    command,
                    (),
                    "/missing-runtime",
                    Vec::new(),
                    sink,
                );
            check(&mut session, observer, future, StdioMode::Inherited);
        }
    }
}

#[test]
fn finalization_refuses_late_run_observer_registration() {
    let (mut session, mut input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    let (observer, future) = LiteinstBackend::prepare_with_output_and_preload_data_and_log_sink::<()>(
        Command::new("/must-not-launch"),
        (),
        "/missing-runtime",
        Vec::new(),
        input.sink.take().unwrap(),
    );
    let primary = session.finish_evidence(deadline()).primary.unwrap();
    assert!(session.shared.retain_run_observer(observer).is_err());
    assert!(session.run_observer().is_none());
    assert!(Arc::ptr_eq(
        &session.finish_evidence(deadline()).primary.unwrap(),
        &primary
    ));
    drop(future);
}

#[test]
fn returned_preparation_error_preserves_not_started_evidence_and_primary() {
    let (mut session, mut input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let (observer, future) = LiteinstBackend::prepare_with_output_and_preload_data_and_log_sink::<()>(
        Command::new("/must-not-launch"),
        (),
        "/missing-runtime",
        Vec::new(),
        input.sink.take().unwrap(),
    );
    session.shared.retain_run_observer(observer).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime.block_on(future).unwrap_err();
    let (_, primary) = retain_backend_error(&session.shared, error, true);
    drop(runtime);
    session
        .shared
        .fail(anyhow::anyhow!("later cleanup failure"));
    let finalized = session.finish(
        PendingRun::<()> {
            result: Err(primary.clone()),
            shared: session.shared.clone(),
        },
        deadline(),
    );
    assert!(Arc::ptr_eq(&finalized.result.unwrap_err(), &primary));
    assert!(
        primary
            .downcast_ref::<reverie_liteinst::LoggedRunError>()
            .is_some()
    );
    let snapshot = finalized.evidence.run.unwrap().unwrap();
    assert!(snapshot.polled && !snapshot.worker_submitted && !snapshot.spawned && !snapshot.reaped);
    assert!(snapshot.wait_status.is_none());
    assert!(matches!(snapshot.completion, RunCompletion::Failed(_)));
    assert!(snapshot.first_error.is_some());
    assert_eq!(snapshot.stdout.state, StreamState::NotStarted);
    assert_eq!(snapshot.stderr.state, StreamState::NotStarted);
}

#[test]
fn unavailable_run_snapshots_never_qualify_completion() {
    assert!(!run_completed(&None));
    assert!(!run_completed(&Some(Err(SnapshotUnavailable::Busy))));
    assert!(!run_completed(&Some(Err(SnapshotUnavailable::Poisoned))));
}

#[test]
fn cleanup_issue_never_qualifies_completed_run_evidence() {
    use std::os::unix::process::ExitStatusExt;

    use reverie_liteinst::run_evidence::StreamEvidence;

    for mode in [StdioMode::Captured, StdioMode::Inherited] {
        let stream = StreamEvidence {
            state: match mode {
                StdioMode::Captured => StreamState::Eof,
                StdioMode::Inherited => StreamState::Inherited,
            },
            chunks: Vec::new(),
        };
        let mut snapshot = RunEvidence {
            mode,
            polled: true,
            worker_submitted: true,
            caller_cancelled: false,
            completion: RunCompletion::Succeeded,
            spawned: true,
            pid: Some(42),
            wait_status: Some(std::process::ExitStatus::from_raw(0)),
            reaped: true,
            wait_error: None,
            first_error: None,
            cleanup_issue: None,
            stdout: stream.clone(),
            stderr: stream,
        };
        assert!(run_completed(&Some(Ok(snapshot.clone()))));
        snapshot.cleanup_issue = Some(logs::Issue {
            kind: IssueKind::Cleanup,
            message: "modeled retained owner cleanup failure".into(),
        });
        assert!(!run_completed(&Some(Ok(snapshot))));
    }
}

#[test]
fn actual_observer_prefix_survives_h_deadline_and_runtime_teardown() {
    let (mut session, mut input, host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let (stdin, input_lifetime) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "printf 'out\\000prefix'; printf 'err\\377prefix' >&2; read ignored",
        ])
        .stdin(stdin);
    let (observer, future) = LiteinstBackend::prepare_with_output_and_preload_data_and_log_sink::<()>(
        command,
        (),
        "/dev/null",
        Vec::new(),
        input.sink.take().unwrap(),
    );
    session.shared.retain_run_observer(observer).unwrap();
    let retained = session.run_observer().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (before, error) =
        runtime.block_on(async {
            let mut future = Box::pin(future);
            let before = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    result = &mut future => panic!("ordinary blocked child returned: {result:?}"),
                    _ = tokio::task::yield_now() => {},
                }
                if let Ok(snapshot) = retained.try_snapshot()
                    && snapshot.stdout.bytes() == b"out\0prefix"
                    && snapshot.stderr.bytes().ends_with(b"err\xffprefix")
                {
                    break snapshot;
                }
            }
        }).await.unwrap();
            let error = crate::with_run_deadline(Some(Duration::from_millis(10)), async {
                future.await.map_err(Error::new)
            })
            .await
            .unwrap_err();
            (before, error)
        });
    assert!(
        error.downcast_ref::<crate::GuestTimedOut>().is_some(),
        "expected caller deadline, got: {error:?}"
    );
    let primary = session.shared.fail(error);
    drop(runtime);
    let after = session.run_observer().unwrap().try_snapshot().unwrap();
    assert_eq!(before.stdout.state, StreamState::Reading);
    assert_eq!(before.stderr.state, StreamState::Reading);
    assert_eq!(before.stderr.bytes(), b"ERROR: ld.so: object '/dev/null' from LD_PRELOAD cannot be preloaded (file too short): ignored.\nerr\xffprefix");
    assert_eq!(after.stdout.bytes(), before.stdout.bytes());
    assert_eq!(after.stderr.bytes(), before.stderr.bytes());
    assert_eq!(after.stdout.state, StreamState::Interrupted);
    assert_eq!(after.stderr.state, StreamState::Interrupted);
    assert_eq!(after.completion, RunCompletion::Interrupted);
    assert!(after.caller_cancelled && after.worker_submitted && after.spawned);
    assert!(!after.reaped && after.wait_status.is_none());
    host.write_record(b"cleanup after observed prefix\n")
        .unwrap();
    let finalized = session.finish(
        PendingRun::<()> {
            result: Err(primary.clone()),
            shared: session.shared.clone(),
        },
        deadline(),
    );
    assert!(Arc::ptr_eq(&finalized.result.unwrap_err(), &primary));
    let final_snapshot = finalized.evidence.run.unwrap().unwrap();
    assert_eq!(final_snapshot.stdout.bytes(), before.stdout.bytes());
    assert_eq!(final_snapshot.stderr.bytes(), before.stderr.bytes());
    assert!(!final_snapshot.reaped);
    let limit = deadline();
    loop {
        let result = unsafe {
            libc::waitpid(
                after.pid.unwrap() as i32,
                std::ptr::null_mut(),
                libc::WNOHANG,
            )
        };
        if result > 0
            || (result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
        {
            break;
        }
        assert!(
            Instant::now() < limit,
            "ordinary test child was not killed on runtime drop"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!retained.try_snapshot().unwrap().reaped);
    drop(input_lifetime);
}

#[test]
fn wrong_session_rejection_preserves_token_and_both_lifecycles() {
    let (mut first, first_input, first_host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    let (mut second, second_input, second_host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    let pending = crate::run_with_output_backend_timeout_and_log(
        Command::new("/review-must-not-launch"),
        DetConfig::default(),
        false,
        &None,
        None,
        first_input.with_runtime_path("/review-must-not-have-a-runtime.so"),
    );
    let primary = first.shared.state.lock().unwrap().primary.clone().unwrap();
    let rejected = second.finish(pending, deadline());
    assert!(
        rejected
            .result
            .unwrap_err()
            .to_string()
            .contains("another capture")
    );
    assert!(!rejected.evidence.finalization_started);
    assert_eq!(rejected.evidence.stage, Stage::Prepared);
    assert!(second.shared.state.lock().unwrap().primary.is_none());
    assert!(!first.shared.state.lock().unwrap().finalization_started);
    second.retain_record_status(|| None).unwrap();
    second_host.write_record(b"unrelated cleanup").unwrap();
    first_host.write_record(b"original cleanup").unwrap();
    let recovered = first.finish(rejected.rejected_pending.unwrap(), deadline());
    assert!(Arc::ptr_eq(&recovered.result.unwrap_err(), &primary));
    assert!(Arc::ptr_eq(&recovered.evidence.primary.unwrap(), &primary));
    assert!(recovered.rejected_pending.is_none());
    assert!(recovered.evidence.finalization_started);
    drop(second_input);
    second.finish_evidence(deadline());
}

#[test]
fn finalization_refuses_both_launches_with_unused_input_and_keeps_primary() {
    for captured in [false, true] {
        let (mut session, input, _host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        let before = session.finish_evidence(deadline());
        let primary = before.primary.unwrap();
        assert!(before.finalization_started);
        assert_eq!(before.stage, Stage::Prepared);
        assert!(session.retain_record_status(|| None).is_err());
        let input = input.with_runtime_path("/review-must-not-have-a-runtime.so");
        let pending = if captured {
            crate::run_with_output_backend_timeout_and_log(
                Command::new("/review-must-not-launch"),
                DetConfig::default(),
                false,
                &None,
                None,
                input,
            )
            .map(|_| ())
        } else {
            crate::run_with_backend_timeout_and_log(
                Command::new("/review-must-not-launch"),
                DetConfig::default(),
                false,
                &None,
                None,
                input,
            )
            .map(|_| ())
        };
        let after = session.finish(pending, deadline());
        assert!(Arc::ptr_eq(&after.result.unwrap_err(), &primary));
        assert!(Arc::ptr_eq(&after.evidence.primary.unwrap(), &primary));
        assert!(after.evidence.finalization_started);
        assert_eq!(after.evidence.stage, Stage::Prepared);
        assert!(after.evidence.runtime.is_none());
        assert!(after.evidence.guest.is_none());
        assert!(matches!(after.evidence.stdio, StdioEvidence::Unavailable));
        assert!(!after.evidence.capture.guest.root_reaped);
        assert!(session.retain_record_status(|| None).is_err());
    }
}

#[test]
fn finalized_registered_session_refuses_launch_before_observer_or_artifact_lookup() {
    let (mut session, input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    session
        .retain_record_status(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            None
        })
        .unwrap();
    let before = session.finish_evidence(deadline());
    assert!(before.primary.is_none());
    assert!(before.finalization_started);
    let before_calls = calls.load(Ordering::SeqCst);
    let pending = crate::run_with_output_backend_timeout_and_log(
        Command::new("/review-must-not-launch"),
        DetConfig::default(),
        false,
        &None,
        None,
        input.with_runtime_path("/review-must-not-have-a-runtime.so"),
    );
    assert_eq!(calls.load(Ordering::SeqCst), before_calls);
    assert!(
        pending
            .result
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("no longer prepared")
    );
    let finalized = session.finish(pending, deadline());
    assert!(finalized.result.is_err());
    assert!(finalized.evidence.runtime.is_none());
    assert_eq!(finalized.evidence.stage, Stage::Prepared);
}

#[test]
fn dropping_owner_seals_launch_with_unused_input() {
    let (mut session, input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let shared = input.shared.clone();
    drop(session);
    assert!(shared.state.lock().unwrap().finalization_started);
    let pending = crate::run_with_output_backend_timeout_and_log(
        Command::new("/review-must-not-launch"),
        DetConfig::default(),
        false,
        &None,
        None,
        input.with_runtime_path("/review-must-not-have-a-runtime.so"),
    );
    assert!(
        pending
            .result
            .unwrap_err()
            .to_string()
            .contains("no longer prepared")
    );
    let state = shared.state.lock().unwrap();
    assert_eq!(state.stage, Stage::Prepared);
    assert!(state.runtime.is_none());
    assert!(state.guest.is_none());
}

#[test]
fn real_capture_survives_unused_input_and_runtime_drop() {
    let output = Destination::default();
    let bytes = output.bytes.clone();
    let (mut session, input, host) = prepare(options(), output, &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    host.write_record(b"before runtime\n").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        session.handle().ready().await.unwrap();
        drop(input);
    });
    drop(runtime);
    host.write_record(b"after runtime\n").unwrap();
    let evidence = session.finish_evidence(deadline());
    assert_eq!(evidence.stage, Stage::Interrupted);
    assert!(!evidence.capture.qualifies());
    assert_eq!(&*bytes.lock().unwrap(), b"before runtime\nafter runtime\n");
    assert_eq!(
        evidence.capture.publication.stability,
        logs::ArtifactStability::Stable
    );
}

#[test]
fn both_explicit_wrappers_refuse_missing_record_status_before_guest_launch() {
    for captured in [false, true] {
        let (mut session, input, host) =
            prepare(options(), Destination::default(), &filter()).unwrap();
        if captured {
            let pending = crate::run_with_output_backend_timeout_and_log(
                Command::new("/bin/true"),
                DetConfig::default(),
                false,
                &None,
                None,
                input,
            );
            host.write_record(b"cleanup").unwrap();
            let finalized = session.finish(pending, deadline());
            assert!(
                finalized
                    .result
                    .unwrap_err()
                    .to_string()
                    .contains("RecordStatus")
            );
            assert!(!finalized.evidence.capture.guest.root_reaped);
        } else {
            let pending = crate::run_with_backend_timeout_and_log(
                Command::new("/bin/true"),
                DetConfig::default(),
                false,
                &None,
                None,
                input,
            );
            host.write_record(b"cleanup").unwrap();
            let finalized = session.finish(pending, deadline());
            assert!(
                finalized
                    .result
                    .unwrap_err()
                    .to_string()
                    .contains("RecordStatus")
            );
            assert!(!finalized.evidence.capture.guest.root_reaped);
        }
    }
}

fn assert_public_session_dispatch_refuses_invalid_provenance(captured: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(crate::liteinst_artifact::RUNTIME_NAME);
    std::fs::write(&path, b"not a Detcore DSO").unwrap();
    let mut provenance = path.as_os_str().to_os_string();
    provenance.push(".provenance.json");
    std::fs::create_dir(provenance).unwrap();

    let (mut session, input, host) = prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let input = input.with_runtime_path(&path);
    let pending = if captured {
        crate::run_with_output_backend_timeout_and_log(
            Command::new("/bin/true"),
            DetConfig::default(),
            false,
            &None,
            None,
            input,
        )
        .map(|_| ())
    } else {
        crate::run_with_backend_timeout_and_log(
            Command::new("/bin/true"),
            DetConfig::default(),
            false,
            &None,
            None,
            input,
        )
        .map(|_| ())
    };
    host.write_record(b"artifact refusal cleanup").unwrap();
    let finalized = session.finish(pending, deadline());
    let error = finalized.result.unwrap_err();
    assert_eq!(error.to_string(), "provenance must be regular file");
    assert_eq!(
        error.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::InvalidData)
    );
    assert!(error.downcast_ref::<crate::BackendUnavailable>().is_none());
    assert!(finalized.evidence.runtime.is_none());
    assert!(!finalized.evidence.capture.guest.root_reaped);
    assert!(matches!(
        finalized.evidence.stdio,
        StdioEvidence::Unavailable
    ));
}

#[test]
fn public_session_dispatch_without_output_reaches_provenance_refusal() {
    assert_public_session_dispatch_refuses_invalid_provenance(false);
}

#[test]
fn public_session_dispatch_with_output_reaches_provenance_refusal() {
    assert_public_session_dispatch_refuses_invalid_provenance(true);
}

#[test]
fn primary_error_is_retained_despite_record_and_destination_failure() {
    let (mut session, input, host) = prepare(
        options(),
        Destination {
            fail_flush: true,
            ..Default::default()
        },
        &filter(),
    )
    .unwrap();
    session
        .retain_record_status(|| Some(Arc::new(io::Error::other("formatting failed"))))
        .unwrap();
    let primary = session
        .shared
        .fail(io::Error::new(io::ErrorKind::TimedOut, "original deadline").into());
    drop(input);
    host.write_record(b"timeout cleanup").unwrap();
    let pending = PendingRun::<()> {
        result: Err(primary.clone()),
        shared: session.shared.clone(),
    };
    let finalized = session.finish(pending, deadline());
    assert!(Arc::ptr_eq(&finalized.result.unwrap_err(), &primary));
    assert_eq!(
        finalized
            .evidence
            .primary
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap()
            .kind(),
        io::ErrorKind::TimedOut
    );
    assert!(finalized.evidence.record_failure.is_some());
    assert!(finalized.evidence.capture.publication.error.is_some());
    assert!(matches!(
        finalized.evidence.stdio,
        StdioEvidence::Unavailable
    ));
}

#[test]
fn nominal_output_cannot_escape_with_incomplete_guest_evidence() {
    let (mut session, input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    drop(input);
    session.shared.state.lock().unwrap().stage = Stage::Returned;
    let pending = PendingRun {
        result: Ok(()),
        shared: session.shared.clone(),
    };
    let result = session.finish(pending, deadline());
    assert!(result.result.is_err());
    assert!(
        !result.evidence.capture.guest.peer_closed || !result.evidence.capture.guest.root_reaped
    );
}

#[test]
fn record_status_is_retained_and_checked_after_destination_finalization() {
    let failure = Arc::new(AtomicBool::new(false));
    let (mut session, input, _host) = prepare(
        options(),
        Destination {
            record_failure_on_flush: Some(failure.clone()),
            ..Default::default()
        },
        &filter(),
    )
    .unwrap();
    session
        .retain_record_status(move || {
            failure
                .load(Ordering::Acquire)
                .then(|| Arc::new(io::Error::other("late format failure")) as RecordFailure)
        })
        .unwrap();
    assert!(session.retain_record_status(|| None).is_err());
    drop(input);
    let evidence = session.finish_evidence(deadline());
    assert!(
        evidence
            .record_failure
            .unwrap()
            .to_string()
            .contains("late format")
    );
    assert!(evidence.primary.is_some());
}

#[test]
fn bootstrap_uses_actual_config_fingerprint_and_preserved_filter() {
    let (mut session, input, _host) =
        prepare(options(), Destination::default(), &filter()).unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&input.payload).unwrap();
    assert_eq!(
        payload["config_wire_fingerprint"],
        detcore::config_wire_fingerprint()
    );
    assert!(payload["log_filter"].as_str().unwrap().contains("task=1.0"));
    crate::liteinst_bootstrap::decode(&input.payload, &detcore::config_wire_fingerprint()).unwrap();
    drop(input);
    session.finish_evidence(deadline());
}

#[test]
fn retained_backend_error_keeps_binary_partial_stdio_before_any_await() {
    let (mut session, input, host) = prepare(options(), Destination::default(), &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let error = reverie_liteinst::LoggedRunError {
        cause: io::Error::other("original backend failure").into(),
        logs: session.handle(),
        rpc: None,
        stdout: b"out\0tail".to_vec(),
        stderr: b"err\xfftail".to_vec(),
    };
    let (_, cause) = retain_backend_error(&session.shared, error, true);
    session.shared.fail(
        crate::GuestTimedOut {
            limit: Duration::from_millis(1),
        }
        .into(),
    );
    drop(input);
    host.write_record(b"cleanup after backend failure").unwrap();
    let pending = PendingRun::<()> {
        result: Err(cause.clone()),
        shared: session.shared.clone(),
    };
    let result = session.finish(pending, deadline());
    assert!(Arc::ptr_eq(&result.result.unwrap_err(), &cause));
    assert!(
        cause
            .downcast_ref::<reverie_liteinst::LoggedRunError>()
            .is_some()
    );
    match result.evidence.stdio {
        StdioEvidence::Partial { stdout, stderr } => {
            assert_eq!(stdout, b"out\0tail");
            assert_eq!(stderr, b"err\xfftail");
        }
        other => panic!("partial bytes were lost: {other:?}"),
    }
}

#[test]
fn host_only_deadline_drops_input_but_keeps_cleanup_and_primary_error() {
    struct Cleanup(HostProducer);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            self.0.write_record(b"timeout cleanup\n").unwrap();
        }
    }
    assert!(std::env::var_os("HERMIT_INTERNAL_RUN_TIMEOUT_STALL_UNWIND").is_none());
    let output = Destination::default();
    let bytes = output.bytes.clone();
    let (mut session, input, host) = prepare(options(), output, &filter()).unwrap();
    session.retain_record_status(|| None).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result: Result<(), Error> = runtime.block_on(crate::with_run_deadline(
        Some(Duration::from_millis(10)),
        async move {
            let _cleanup = Cleanup(host);
            let _input = input;
            std::future::pending().await
        },
    ));
    drop(runtime);
    let cause = session.shared.fail(result.unwrap_err());
    assert!(cause.downcast_ref::<crate::GuestTimedOut>().is_some());
    let pending = PendingRun::<()> {
        result: Err(cause.clone()),
        shared: session.shared.clone(),
    };
    let finalized = session.finish(pending, deadline());
    assert!(Arc::ptr_eq(&finalized.result.unwrap_err(), &cause));
    assert_eq!(&*bytes.lock().unwrap(), b"timeout cleanup\n");
    assert!(matches!(
        finalized.evidence.stdio,
        StdioEvidence::Unavailable
    ));
    assert!(!finalized.evidence.capture.qualifies());
}
