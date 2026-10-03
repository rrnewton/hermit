//! Capture only: the provider's original installation and pidfd_getfd result
//! are controlled premises. Real shared census/grant/Call/worker join are used.
use std::os::fd::OwnedFd;

use super::*;
use crate::network_replay::shared_waits::SharedCaptureSubmission;
use crate::network_runtime::shared_waits::JoinedSharedCapture;

async fn record_fixture() -> Fixture {
    fixture_engine_kind(true, false, false, false, true).await
}
fn identity() -> crate::network_runtime::original_installation::FileIdentity {
    crate::network_runtime::original_installation::FileIdentity::controlled_fixture(7, 19)
}
fn socket_pin() -> OwnedFd {
    std::os::unix::net::UnixStream::pair().unwrap().0.into()
}
impl Fixture {
    fn recovery(&self) -> crate::network_runtime::NativeCaptureRecovery {
        crate::network_runtime::NativeCaptureRecovery::new(
            self.engine.clone(),
            Arc::new(tokio::sync::Notify::new()),
            |_| {},
        )
    }
    async fn prepare_capture(&self) -> (NetworkStreamCall, SharedCaptureSubmission) {
        let owner = self.root.owner();
        let read = {
            let mut e = self.engine.lock().unwrap();
            let NetworkFdReadBegin::Admitted(read) = e
                .begin_fd_read(owner, self.root.files(), self.binding.slot.fd)
                .unwrap()
            else {
                panic!("selected original descriptor")
            };
            *read
        };
        let prefix = self
            .runtime
            .join_shared_foreground_prefix(self.root.clone(), &self.engine, None)
            .await
            .unwrap();
        self.runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(owner, lineage)?;
                let mut engine = self.engine.lock().unwrap();
                self.runtime.with_shared_attempt_prefix(
                    &prefix,
                    &mut engine,
                    |engine, admission| {
                        engine
                            .preflight_shared_wait_begin(&read, &grant, admission)
                            .unwrap();
                        let call = engine
                            .begin_native_stream_call_from_read(owner, read.clone())
                            .unwrap();
                        assert!(call.physical_pin_required);
                        let policy = crate::tool_global::SavedReceivePolicy::controlled_shared(
                            (owner, call.id, self.binding.open_file),
                            self.root.clone(),
                            Read::new()
                                .with_fd(self.binding.slot.fd)
                                .with_buf(reverie::syscalls::AddrMut::from_ptr(
                                    0x2000usize as *mut u8,
                                ))
                                .with_len(8)
                                .into_parts(),
                            (self.now, Some(self.deadline)),
                            (false, 3),
                        );
                        engine
                            .attach_shared_wait_call(
                                call.id,
                                self.binding,
                                SharedWaitIntent::Receive(policy),
                                &grant,
                                admission,
                                self.now,
                            )
                            .unwrap();
                        assert!(engine.validate_fd_read_grant(owner, &read).is_err());
                        assert!(
                            engine
                                .confirm_stream_call_pin(
                                    owner,
                                    call.id,
                                    NetworkStreamPinOutcome::Acquired
                                )
                                .is_err(),
                            "raw enum is not capture evidence"
                        );
                        let token = engine
                            .prepare_shared_record_capture(call.id, identity(), &grant, admission)
                            .unwrap();
                        assert!(
                            engine
                                .prepare_shared_record_capture(
                                    call.id,
                                    identity(),
                                    &grant,
                                    admission
                                )
                                .is_err()
                        );
                        Ok((call, token))
                    },
                )
            })
            .unwrap()
    }
    fn finish_capture(&self, joined: &JoinedSharedCapture) -> std::io::Result<NetworkStreamCall> {
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                let mut engine = self.engine.lock().unwrap();
                self.runtime.with_shared_capture_completion(
                    joined,
                    &mut engine,
                    |engine, confirmed| {
                        engine
                            .complete_shared_record_capture(confirmed, &grant)
                            .map_err(std::io::Error::other)
                    },
                )
            })
    }
}

#[tokio::test]
async fn shared_capture_actual_worker_join_settles_only_original_entry() {
    let f = record_fixture().await;
    assert!(f.parent.is_some());
    let (call, submission) = f.prepare_capture().await;
    let before = {
        let e = f.engine.lock().unwrap();
        let Some(SharedAttempt::Wait(wait)) = &e.stream_calls[&call.id].shared_attempt else {
            unreachable!()
        };
        assert!(e.stream_calls[&call.id].native_entry.is_none());
        format!("{:?}", wait.phase)
    };
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked = ran.clone();
    let joined = f
        .runtime
        .controlled_shared_capture_with(submission, f.recovery(), move || {
            marked.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(socket_pin())
        })
        .await
        .unwrap();
    assert!(ran.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(joined.outcome(), NetworkStreamPinOutcome::Acquired);
    assert_eq!(f.finish_capture(&joined).unwrap().id, call.id);
    assert!(f.finish_capture(&joined).is_err());
    let e = f.engine.lock().unwrap();
    let state = &e.stream_calls[&call.id];
    assert_eq!(state.phase, StreamCallPhase::Active);
    assert!(state.capture_control.is_none() && state.capture_publication.is_none());
    let Some(SharedAttempt::Wait(wait)) = &state.shared_attempt else {
        unreachable!()
    };
    assert_eq!(format!("{:?}", wait.phase), before);
    assert_eq!(e.shared_call_census(Some(call.id)).unwrap().rows.len(), 1);
    assert!(
        e.finish().is_err(),
        "capture alone does not finish the original receive"
    );
}

#[tokio::test]
async fn shared_capture_changed_grant_and_control_refuse_before_settlement() {
    for control in [false, true] {
        let mut f = record_fixture().await;
        let (call, submission) = f.prepare_capture().await;
        let joined = f
            .runtime
            .controlled_shared_capture_with(submission, f.recovery(), || Ok(socket_pin()))
            .await
            .unwrap();
        if control {
            let mut e = f.engine.lock().unwrap();
            let lease = e.stream_calls[&call.id].capture_control.unwrap();
            e.submit_descriptor_effect(
                f.root.owner(),
                lease,
                NetworkDescriptorEffect::CloseDescriptor,
            )
            .unwrap();
        } else {
            // Grants are per task. Advance the queued parent, then the
            // child's next actual request so the original child epoch is stale.
            let before = f
                .scheduler
                .ordinary_fd_observation(f.root.owner())
                .unwrap()
                .epoch();
            f.scheduler
                .controlled_shared_foreground_grant(f.parent.as_ref().unwrap());
            f.scheduler.controlled_shared_foreground_grant(&f.root);
            assert_ne!(
                f.scheduler
                    .ordinary_fd_observation(f.root.owner())
                    .unwrap()
                    .epoch(),
                before
            );
        }
        assert!(f.finish_capture(&joined).is_err());
        let e = f.engine.lock().unwrap();
        let state = &e.stream_calls[&call.id];
        assert_eq!(state.phase, StreamCallPhase::PinAcquireSubmitted);
        assert!(state.capture_control.is_some() && state.capture_publication.is_some());
        assert!(e.finish().is_err());
    }
}

#[tokio::test]
async fn shared_capture_canceled_wait_retains_actual_worker_and_unknown_pin() {
    let f = record_fixture().await;
    let (call, submission) = f.prepare_capture().await;
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let mut pending = Box::pin(f.runtime.controlled_shared_capture_with(
        submission,
        f.recovery(),
        move || {
            started.send(()).unwrap();
            released
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            Ok(socket_pin())
        },
    ));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    observed.await.unwrap();
    drop(pending);
    release.send(()).unwrap();
    // Actual terminal join retains completion but cannot synthesize this lost
    // callback's JoinedSharedCapture or a successful semantic receive.
    f.runtime.controlled_join_retry_workers().await;
    let e = f.engine.lock().unwrap();
    assert_eq!(
        e.stream_calls[&call.id].phase,
        StreamCallPhase::PinAcquireSubmitted
    );
    assert!(e.finish().is_err());
}

#[tokio::test]
async fn shared_capture_actual_failed_acquisition_cannot_confirm_success() {
    let f = record_fixture().await;
    let (call, submission) = f.prepare_capture().await;
    let joined = f
        .runtime
        .controlled_shared_capture_with(submission, f.recovery(), || {
            Err(std::io::Error::from_raw_os_error(libc::EBADF))
        })
        .await
        .unwrap();
    assert_eq!(
        joined.outcome(),
        NetworkStreamPinOutcome::Failed(libc::EBADF)
    );
    assert!(f.finish_capture(&joined).is_err());
    let mut e = f.engine.lock().unwrap();
    assert!(
        e.confirm_stream_call_pin(f.root.owner(), call.id, NetworkStreamPinOutcome::Acquired)
            .is_err()
    );
    assert_eq!(
        e.stream_calls[&call.id].phase,
        StreamCallPhase::PinAcquireSubmitted
    );
    assert!(e.finish().is_err());
}

#[path = "record_probe_tests.rs"]
mod record_probe_tests;
