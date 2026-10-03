//! Real TCP/worker/cursor/readiness paths; original file installation and copy5
//! rows are controlled premises. No native provider or guest backend claim.
use std::io::Write;
use std::os::fd::AsRawFd;

use super::*;
use crate::network_replay::shared_waits::PreparedSharedEffect;
use crate::network_replay::shared_waits::SharedProbeProgress;
use crate::network_replay::shared_waits::SharedRecordProbe;
use crate::network_runtime::shared_waits::JoinedSharedEffect;

async fn captured() -> (
    Fixture,
    NetworkStreamCall,
    std::net::TcpStream,
    std::net::TcpStream,
) {
    let f = record_fixture().await;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (held, _) = listener.accept().unwrap();
    let duplicate = held.try_clone().unwrap();
    let lowat: i32 = 3;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                held.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVLOWAT,
                (&lowat as *const i32).cast(),
                std::mem::size_of::<i32>() as _,
            )
        },
        0
    );
    let mut cursor = 0i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                held.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&mut cursor as *mut i32).cast(),
                &mut size,
            )
        },
        0
    );
    {
        let mut e = f.engine.lock().unwrap();
        let channel = NetworkReplayEngine::controlled_replay_two_row_trace().channels[0].clone();
        e.record_channel(channel).unwrap();
        e.bind(f.binding.open_file, NetworkChannelId(1)).unwrap();
        let socket = e
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&f.binding.open_file)
            .unwrap();
        // Controlled original profile now matches this actual retained TCP OFD.
        socket.options.peek_offset = Some(cursor);
        socket.options.receive_low_water = 3;
    }
    let (call, submission) = f.prepare_capture().await;
    let joined = f
        .runtime
        .controlled_shared_capture_with(submission, f.recovery(), move || Ok(held.into()))
        .await
        .unwrap();
    assert_eq!(f.finish_capture(&joined).unwrap().id, call.id);
    (f, call, peer, duplicate)
}
impl Fixture {
    async fn record_probe(&self, call: NetworkStreamCallId) -> Arc<SharedRecordProbe> {
        let prefix = self
            .runtime
            .join_shared_foreground_prefix(self.root.clone(), &self.engine, Some(call))
            .await
            .unwrap();
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                let mut e = self.engine.lock().unwrap();
                let origin =
                    self.runtime
                        .with_shared_attempt_prefix(&prefix, &mut e, |e, admission| {
                            e.begin_shared_record_probe(call, &grant, admission, self.now)
                                .map_err(std::io::Error::other)
                        })?;
                self.runtime.bind_shared_record_probe(&origin, &e)?;
                Ok(origin)
            })
            .unwrap()
    }
    fn prepare_effect(
        &self,
        origin: &Arc<SharedRecordProbe>,
        effect: NetworkStreamPhysicalEffect,
    ) -> PreparedSharedEffect {
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                self.engine
                    .lock()
                    .unwrap()
                    .prepare_shared_record_effect(origin, &grant, effect, self.now)
                    .map_err(std::io::Error::other)
            })
            .unwrap()
    }
    fn confirm_effect(
        &self,
        actual: &Arc<JoinedSharedEffect>,
    ) -> std::io::Result<SharedProbeProgress> {
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                let mut e = self.engine.lock().unwrap();
                self.runtime
                    .with_shared_record_effect(actual, &mut e, |e, proof| {
                        e.confirm_shared_record_effect(proof, &grant, self.now)
                            .map_err(std::io::Error::other)
                    })
            })
    }
    async fn effect(
        &self,
        origin: &Arc<SharedRecordProbe>,
        effect: NetworkStreamPhysicalEffect,
    ) -> SharedProbeProgress {
        let prepared = self.prepare_effect(origin, effect);
        let actual = self
            .runtime
            .controlled_shared_record_effect(prepared)
            .await
            .unwrap();
        let next = self.confirm_effect(&actual).unwrap();
        assert!(
            self.confirm_effect(&actual).is_err(),
            "one original result cannot complete twice"
        );
        next
    }
    fn complete_pending(
        &self,
        origin: &Arc<SharedRecordProbe>,
    ) -> std::io::Result<CompletedSharedAttempt> {
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                let mut e = self.engine.lock().unwrap();
                self.runtime
                    .with_shared_record_pending(origin, &mut e, |e, proof| {
                        e.complete_shared_record_pending(proof, &grant, self.now)
                            .map_err(std::io::Error::other)
                    })
            })
    }
    async fn suspend_record(&self, call: NetworkStreamCallId, completed: CompletedSharedAttempt) {
        let prefix = self
            .runtime
            .join_shared_foreground_prefix(self.root.clone(), &self.engine, Some(call))
            .await
            .unwrap();
        self.runtime
            .with_shared_foreground_lineage(self.root.owner(), |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(self.root.owner(), lineage)?;
                let mut e = self.engine.lock().unwrap();
                self.runtime
                    .with_shared_attempt_prefix(&prefix, &mut e, |e, admission| {
                        e.suspend_shared_wait(completed, &grant, admission)
                            .map_err(std::io::Error::other)
                    })
            })
            .unwrap();
    }
}
fn wait_bytes(socket: &std::net::TcpStream, expected: i32) {
    let bound = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let mut queued = 0i32;
        assert_eq!(
            unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut queued) },
            0
        );
        if queued == expected {
            break;
        }
        assert!(
            std::time::Instant::now() < bound,
            "real test payload did not arrive"
        );
        std::thread::yield_now();
    }
}
fn trace_and_queue(f: &Fixture) -> (usize, usize, u64) {
    let e = f.engine.lock().unwrap();
    let EngineState::Native(n) = &e.mode else {
        unreachable!()
    };
    (
        n.trace().inputs.len(),
        n.trace().release_model.nodes().len(),
        e.channels[&NetworkChannelId(1)].inbound_consumed,
    )
}
async fn peek(f: &Fixture, origin: &Arc<SharedRecordProbe>) {
    let mut next = f
        .effect(origin, NetworkStreamPhysicalEffect::ReadPeekOffset)
        .await;
    if let SharedProbeProgress::Need(NetworkStreamPhysicalEffect::SetPeekOffset { value }) = next {
        next = f
            .effect(origin, NetworkStreamPhysicalEffect::SetPeekOffset { value })
            .await;
    }
    let SharedProbeProgress::Need(NetworkStreamPhysicalEffect::Peek { maximum }) = next else {
        panic!("{next:?}")
    };
    next = f
        .effect(origin, NetworkStreamPhysicalEffect::Peek { maximum })
        .await;
    if let SharedProbeProgress::Need(NetworkStreamPhysicalEffect::SetPeekOffset { value }) = next {
        next = f
            .effect(origin, NetworkStreamPhysicalEffect::SetPeekOffset { value })
            .await;
    }
    assert_eq!(
        next,
        SharedProbeProgress::Need(NetworkStreamPhysicalEffect::PollState)
    );
}
#[tokio::test]
async fn shared_record_probe_short_prefix_actual_scan_and_join_suspend_without_consuming() {
    let (f, call, mut peer, retained) = captured().await;
    peer.write_all(b"ab").unwrap();
    wait_bytes(&retained, 2);
    let before = trace_and_queue(&f);
    let original_turn = f.scheduler.turn;
    let origin = f.record_probe(call.id).await;
    peek(&f, &origin).await;
    assert!(
        f.complete_pending(&origin).is_err(),
        "a short Peek alone is not a Pending witness"
    );
    assert_eq!(
        f.effect(&origin, NetworkStreamPhysicalEffect::PollState)
            .await,
        SharedProbeProgress::PendingCandidate
    );
    let completed = f.complete_pending(&origin).unwrap();
    assert!(f.complete_pending(&origin).is_err());
    f.suspend_record(call.id, completed).await;
    let mut e = f.engine.lock().unwrap();
    assert!(
        e.call_wait_binding(
            f.root.owner(),
            call.id,
            NetworkWaitKind::ReadableAtLeast(3),
            Some(f.deadline)
        )
        .is_ok()
    );
    assert!(
        e.call_wait_binding(
            f.root.owner(),
            call.id,
            NetworkWaitKind::ReadableAtLeast(1),
            Some(f.deadline)
        )
        .is_err()
    );
    assert!(
        e.call_wait_binding(
            f.root.owner(),
            call.id,
            NetworkWaitKind::ReadableAtLeast(3),
            Some(f.now)
        )
        .is_err()
    );
    assert_eq!(e.stream_calls[&call.id].native_receive.len(), 1);
    assert!(e.stream_calls[&call.id].native_receive[0].joined);
    assert!(
        e.begin_stream_call_release(f.root.owner(), call.id)
            .is_err()
    );
    drop(e);
    assert_eq!(trace_and_queue(&f), before);
    assert_eq!(f.scheduler.turn, original_turn);
    wait_bytes(&retained, 2);
}
#[tokio::test]
async fn shared_record_probe_short_prefix_followed_by_fin_or_new_ready_bytes_cannot_suspend() {
    for fin in [false, true] {
        let (f, call, mut peer, retained) = captured().await;
        peer.write_all(b"ab").unwrap();
        wait_bytes(&retained, 2);
        let before = trace_and_queue(&f);
        let origin = f.record_probe(call.id).await;
        peek(&f, &origin).await;
        if fin {
            peer.shutdown(std::net::Shutdown::Write).unwrap();
        } else {
            peer.write_all(b"c").unwrap();
            wait_bytes(&retained, 3);
        }
        let mut ready = libc::pollfd {
            fd: retained.as_raw_fd(),
            events: libc::POLLIN | libc::POLLRDHUP,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut ready, 1, 1000) }, 1);
        assert!(matches!(
            f.effect(&origin, NetworkStreamPhysicalEffect::PollState)
                .await,
            SharedProbeProgress::Refused(_)
        ));
        assert!(f.complete_pending(&origin).is_err());
        let e = f.engine.lock().unwrap();
        assert!(e.shadow_probes.contains_key(&origin.lease()));
        assert!(e.stream_calls[&call.id].helper_copy.is_some());
        assert!(e.finish().is_err());
        drop(e);
        assert_eq!(trace_and_queue(&f), before);
    }
}
#[tokio::test]
async fn shared_record_probe_unsubmitted_and_stale_grant_keep_exact_effect_debt() {
    let (mut f, call, _peer, _retained) = captured().await;
    let origin = f.record_probe(call.id).await;
    let prepared = f.prepare_effect(&origin, NetworkStreamPhysicalEffect::ReadPeekOffset);
    assert!(f.complete_pending(&origin).is_err());
    let actual = f
        .runtime
        .controlled_shared_record_effect(prepared)
        .await
        .unwrap();
    f.scheduler
        .controlled_shared_foreground_grant(f.parent.as_ref().unwrap());
    f.scheduler.controlled_shared_foreground_grant(&f.root);
    assert!(f.confirm_effect(&actual).is_err());
    assert!(f.complete_pending(&origin).is_err());
    let e = f.engine.lock().unwrap();
    assert!(e.shadow_probes[&origin.lease()].pending.is_some());
    assert!(e.finish().is_err());
}

#[tokio::test]
async fn shared_record_probe_dropped_submission_never_mints_native_completion() {
    let (f, call, _peer, _retained) = captured().await;
    let before = trace_and_queue(&f);
    let origin = f.record_probe(call.id).await;
    let prepared = f.prepare_effect(&origin, NetworkStreamPhysicalEffect::ReadPeekOffset);
    drop(prepared);
    assert!(f.complete_pending(&origin).is_err());
    let e = f.engine.lock().unwrap();
    assert!(e.shared_call_census(Some(call.id)).is_err());
    assert!(e.shadow_probes[&origin.lease()].pending.is_some());
    assert!(e.finish().is_err());
    drop(e);
    assert_eq!(trace_and_queue(&f), before);
}

#[tokio::test]
async fn shared_record_probe_actual_empty_eagain_is_distinct_from_eof() {
    for eof in [false, true] {
        let (f, call, peer, retained) = captured().await;
        if eof {
            peer.shutdown(std::net::Shutdown::Write).unwrap();
            let mut row = libc::pollfd {
                fd: retained.as_raw_fd(),
                events: libc::POLLIN | libc::POLLRDHUP,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut row, 1, 1000) }, 1);
        }
        let before = trace_and_queue(&f);
        let origin = f.record_probe(call.id).await;
        peek(&f, &origin).await;
        let observed = f
            .effect(&origin, NetworkStreamPhysicalEffect::PollState)
            .await;
        if eof {
            assert_eq!(observed, SharedProbeProgress::EligibleSource);
            assert!(
                f.complete_pending(&origin).is_err(),
                "authenticated EOF is not pending"
            );
        } else {
            assert_eq!(observed, SharedProbeProgress::PendingCandidate);
            let completed = f.complete_pending(&origin).unwrap();
            f.suspend_record(call.id, completed).await;
        }
        assert_eq!(
            trace_and_queue(&f),
            before,
            "source observation never delivers EOF or bytes"
        );
    }
}

#[tokio::test]
async fn shared_record_probe_real_cursor_restore_and_pre_effect_controls_survive() {
    let (f, call, mut peer, retained) = captured().await;
    let cursor = 1i32;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                retained.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&cursor as *const i32).cast(),
                std::mem::size_of::<i32>() as _,
            )
        },
        0
    );
    // Controlled source profile contains this actual pre-effect value. This is
    // not a claim that a bare integer authenticates original guest setsockopt.
    f.engine
        .lock()
        .unwrap()
        .shadow
        .as_mut()
        .unwrap()
        .sockets
        .get_mut(&f.binding.open_file)
        .unwrap()
        .options
        .peek_offset = Some(cursor);
    peer.write_all(b"ab").unwrap();
    wait_bytes(&retained, 2);
    let origin = f.record_probe(call.id).await;
    peek(&f, &origin).await;
    let mut actual = 0i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                retained.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&mut actual as *mut i32).cast(),
                &mut size,
            )
        },
        0
    );
    assert_eq!(
        actual, cursor,
        "actual shared cursor restored before the final scan"
    );
    assert_eq!(
        f.effect(&origin, NetworkStreamPhysicalEffect::PollState)
            .await,
        SharedProbeProgress::PendingCandidate
    );
    let source = f
        .runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let e = f.engine.lock().unwrap();
            let source = e
                .shared_record_poll_source(&origin, &grant, f.now)
                .map_err(std::io::Error::other)?;
            e.validate_shared_record_poll_source(&source, &grant, f.now)
                .map_err(std::io::Error::other)?;
            Ok(source)
        })
        .unwrap();
    let late = f
        .runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            f.engine
                .lock()
                .unwrap()
                .shared_record_poll_source(
                    &origin,
                    &grant,
                    LogicalTime::from_nanos(f.now.as_nanos() + 1),
                )
                .map_err(std::io::Error::other)
        });
    assert!(
        late.is_err(),
        "a past native scan cannot acquire a fresh observation time"
    );
    assert_eq!(source.low_water(), 3);
    assert_eq!(source.control_generation(), 0);
    assert_eq!(
        source.revents() & (libc::POLLIN | libc::POLLRDNORM | libc::POLLHUP | libc::POLLRDHUP),
        0
    );
    f.engine
        .lock()
        .unwrap()
        .channels
        .get_mut(&NetworkChannelId(1))
        .unwrap()
        .local_control_generation += 1;
    let rejected = f
        .runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            f.engine
                .lock()
                .unwrap()
                .validate_shared_record_poll_source(&source, &grant, f.now)
                .map_err(std::io::Error::other)
        });
    assert!(
        rejected.is_err(),
        "late application cannot relabel a past mask with new controls"
    );
    assert!(f.complete_pending(&origin).is_err());
}

#[tokio::test]
async fn shared_record_probe_changed_original_cut_or_policy_owner_refuses_before_effect() {
    for changed in 0..3 {
        let (f, call, _peer, _retained) = captured().await;
        let origin = f.record_probe(call.id).await;
        {
            let mut e = f.engine.lock().unwrap();
            match changed {
                0 => {
                    e.channels
                        .get_mut(&NetworkChannelId(1))
                        .unwrap()
                        .inbound_consumed += 1
                }
                1 => {
                    e.shadow
                        .as_mut()
                        .unwrap()
                        .sockets
                        .get_mut(&f.binding.open_file)
                        .unwrap()
                        .native
                        .as_mut()
                        .unwrap()
                        .physical_observed =
                        crate::network_replay::native_receive::Cut { bytes: 1, order: 1 }
                }
                2 => {
                    let state = e.stream_calls.get_mut(&call.id).unwrap();
                    let old = state.receive_policy.as_ref().unwrap();
                    let replacement = crate::tool_global::SavedReceivePolicy::controlled_shared(
                        (f.root.owner(), call.id, f.binding.open_file),
                        f.root.clone(),
                        old.raw(),
                        (old.started(), old.deadline()),
                        (old.nonblocking(), old.target()),
                    );
                    state.receive_policy = Some(replacement.clone());
                    let Some(SharedAttempt::Wait(wait)) = &mut state.shared_attempt else {
                        unreachable!()
                    };
                    wait.intent = SharedWaitIntent::Receive(replacement);
                }
                _ => unreachable!(),
            }
            assert!(e.shared_record_probe_progress(&origin, f.now).is_err());
            assert!(e.shadow_probes[&origin.lease()].pending.is_none());
        }
        assert!(f.complete_pending(&origin).is_err());
        assert!(f.engine.lock().unwrap().finish().is_err());
    }
}

#[path = "record_poll_tests.rs"]
mod record_poll_tests;
