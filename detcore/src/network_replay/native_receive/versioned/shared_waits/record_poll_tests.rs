//! Actual TCP full-state scans and worker joins. Original installation/Connect
//! remain explicit controlled premises; this does not execute a guest Poll.
use super::*;
use crate::network_replay::shared_waits::SharedRecordPollPublication;
use crate::network_replay::shared_waits::SharedRecordPollSource;

async fn poll_captured(
    prior_poll: bool,
) -> (
    Fixture,
    NetworkStreamCall,
    std::net::TcpStream,
    std::net::TcpStream,
) {
    let f = record_fixture().await;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (held, _) = listener.accept().unwrap();
    let retained = held.try_clone().unwrap();
    {
        let mut e = f.engine.lock().unwrap();
        let definition = NetworkReplayEngine::controlled_replay_two_row_trace().channels[0].clone();
        let channel = e
            .ensure_channel(
                f.binding.open_file,
                NetworkChannelBinding {
                    transport: definition.transport,
                    role: definition.role,
                    peer_address: definition.peer_address,
                    requested_local_constraint: None,
                    observed_local_address: definition.local_address,
                    accepted_from: definition.accepted_from,
                    selected_channel: None,
                },
            )
            .unwrap();
        assert_eq!(channel, NetworkChannelId(1));
        let key = e.shadow.as_ref().unwrap().sockets[&f.binding.open_file].key;
        e.retain_native_fresh_send(key);
        // Controlled successful original Connect precedes this Call admission;
        // no late replacement or relabeling of the Call's pre-effect frontier.
        let EngineState::Native(n) = &mut e.mode else {
            unreachable!()
        };
        n.trace.inputs.push(NetworkInputEventV4 {
            ordinal: 0,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: f.now,
                receive_entry_cut: NetworkReceiveEntryCutV4(0),
                prerequisites: vec![],
            },
            event: NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected),
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut n.trace.release_model
        else {
            unreachable!()
        };
        nodes.extend([
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(0),
                kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 0 },
                prerequisites: vec![],
            },
            NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(1),
                kind: NetworkReleaseNodeKindV4::Progress {
                    channel: NetworkChannelId(1),
                    milestone: NetworkProgressV4::Established {
                        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
                    },
                },
                prerequisites: vec![NetworkReleaseNodeIdV4(0)],
            },
        ]);
        if prior_poll {
            // A valid prior snapshot at the same semantic cut/time. The new
            // publisher must preserve the existing duplicate-source refusal.
            n.trace.inputs.push(NetworkInputEventV4 {
                ordinal: 1,
                channel: NetworkChannelId(1),
                release: NetworkReleaseV4 {
                    not_before_global_time: f.now,
                    receive_entry_cut: NetworkReceiveEntryCutV4(2),
                    prerequisites: vec![NetworkReleaseNodeIdV4(1)],
                },
                event: NetworkInputKindV2::SharedRawTcpPollState {
                    consumed_prefix: 0,
                    revents: 0,
                    control_generation: 0,
                    receive_low_water: 1,
                },
            });
            nodes.push(NetworkReleaseNodeV4 {
                id: NetworkReleaseNodeIdV4(2),
                kind: NetworkReleaseNodeKindV4::Input { input_ordinal: 1 },
                prerequisites: vec![NetworkReleaseNodeIdV4(1)],
            });
        }
        e.native_trace_fixture().validate().unwrap();
    }
    let owner = f.root.owner();
    let read = {
        let mut e = f.engine.lock().unwrap();
        let NetworkFdReadBegin::Admitted(read) = e
            .begin_fd_read(owner, f.root.files(), f.binding.slot.fd)
            .unwrap()
        else {
            panic!("original selected descriptor");
        };
        *read
    };
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(f.root.clone(), &f.engine, None)
        .await
        .unwrap();
    let (call, submission) = f
        .runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(owner, lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime
                .with_shared_attempt_prefix(&prefix, &mut e, |e, admission| {
                    e.preflight_shared_wait_begin(&read, &grant, admission)
                        .unwrap();
                    let call = e
                        .begin_native_stream_call_from_read(owner, read.clone())
                        .unwrap();
                    let raw = Poll::new()
                        .with_fds(reverie::syscalls::AddrMut::from_raw(0x1000))
                        .with_nfds(1)
                        .with_timeout(5000)
                        .into_parts();
                    let intent = SharedWaitIntent::Poll(Arc::new(
                        OriginalPollIntent::new(
                            raw,
                            vec![(f.binding, libc::POLLIN)],
                            f.now,
                            Some(f.deadline),
                        )
                        .unwrap(),
                    ));
                    e.attach_shared_wait_call(call.id, f.binding, intent, &grant, admission, f.now)
                        .unwrap();
                    assert!(e.validate_fd_read_grant(owner, &read).is_err());
                    let submission = e
                        .prepare_shared_record_capture(call.id, identity(), &grant, admission)
                        .unwrap();
                    Ok((call, submission))
                })
        })
        .unwrap();
    let joined = f
        .runtime
        .controlled_shared_capture_with(submission, f.recovery(), move || Ok(held.into()))
        .await
        .unwrap();
    assert_eq!(f.finish_capture(&joined).unwrap().id, call.id);
    (f, call, peer, retained)
}

fn source(f: &Fixture, origin: &Arc<SharedRecordProbe>) -> SharedRecordPollSource {
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            f.engine
                .lock()
                .unwrap()
                .shared_record_poll_source(origin, &grant, f.now)
                .map_err(std::io::Error::other)
        })
        .unwrap()
}
fn publish(
    f: &Fixture,
    origin: &Arc<SharedRecordProbe>,
    source: SharedRecordPollSource,
    now: LogicalTime,
) -> std::io::Result<Arc<SharedRecordPollPublication>> {
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime
                .with_shared_record_poll_publication(origin, &mut e, |e, proof| {
                    e.publish_shared_record_poll(source, proof, &grant, now)
                        .map_err(std::io::Error::other)
                })
        })
}
async fn scan(f: &Fixture, origin: &Arc<SharedRecordProbe>) -> SharedProbeProgress {
    let prepared = f.prepare_effect(origin, NetworkStreamPhysicalEffect::PollState);
    // The production direct worker path: no helper-copy fixture or fake ACK.
    let actual = f
        .runtime
        .execute_shared_record_effect(prepared)
        .await
        .unwrap();
    f.confirm_effect(&actual).unwrap()
}

#[tokio::test]
async fn shared_record_poll_publishes_exact_row_once_before_pending_settlement() {
    let (mut f, call, _peer, _retained) = poll_captured(false).await;
    let origin = f.record_probe(call.id).await;
    assert_eq!(
        scan(&f, &origin).await,
        SharedProbeProgress::PendingCandidate
    );
    let before = trace_and_queue(&f);
    assert!(
        f.complete_pending(&origin).is_err(),
        "actual scan alone is not trace publication"
    );
    let actual = source(&f, &origin);
    let mask = actual.revents();
    let receipt = publish(&f, &origin, actual, f.now).unwrap();
    assert_eq!(receipt.input_ordinal(), 1);
    assert!(Arc::ptr_eq(receipt.source().origin(), &origin));
    assert!(publish(&f, &origin, source(&f, &origin), f.now).is_err());
    let trace = f.engine.lock().unwrap().native_trace_fixture();
    let expected = NetworkInputEventV4 {
        ordinal: 1,
        channel: NetworkChannelId(1),
        release: NetworkReleaseV4 {
            not_before_global_time: f.now,
            receive_entry_cut: NetworkReceiveEntryCutV4(2),
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        },
        event: NetworkInputKindV2::SharedRawTcpPollState {
            consumed_prefix: 0,
            revents: mask,
            control_generation: 0,
            receive_low_water: 1,
        },
    };
    assert_eq!(trace.inputs[1], expected);
    assert_eq!(trace.inputs.len(), before.0 + 1);
    assert_eq!(trace.release_model.nodes().len(), before.1 + 1);
    let mut encoded = Vec::new();
    trace.write_framed(&mut encoded).unwrap();
    assert_eq!(
        NetworkTraceV4::read_framed(std::io::Cursor::new(encoded)).unwrap(),
        trace
    );
    let completed = f.complete_pending(&origin).unwrap();
    assert!(f.complete_pending(&origin).is_err());
    f.suspend_record(call.id, completed).await;
    {
        let mut e = f.engine.lock().unwrap();
        let owner = f.root.owner();
        let binding = || (NetworkWaitKind::PollReadable, Some(f.deadline));
        assert!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .is_ok()
        );
        assert_eq!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .unwrap()
                .observed_ready,
            Some(false)
        );
        for (kind, deadline) in [
            (NetworkWaitKind::Writable, Some(f.deadline)),
            (NetworkWaitKind::Any, Some(f.deadline)),
            (NetworkWaitKind::PollReadable, None),
            (
                NetworkWaitKind::PollReadable,
                Some(LogicalTime::from_nanos(f.deadline.as_nanos() + 1)),
            ),
        ] {
            assert!(e.call_wait_binding(owner, call.id, kind, deadline).is_err());
        }
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        let row = native.trace.inputs.pop().unwrap();
        assert!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .is_err()
        );
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        native.trace.inputs.push(row.clone());
        let NetworkInputKindV2::SharedRawTcpPollState {
            control_generation, ..
        } = &mut native.trace.inputs[1].event
        else {
            unreachable!()
        };
        *control_generation += 1;
        assert!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .is_err()
        );
        let EngineState::Native(native) = &mut e.mode else {
            unreachable!()
        };
        native.trace.inputs[1] = row;
        let Some(SharedAttempt::Wait(wait)) =
            &mut e.stream_calls.get_mut(&call.id).unwrap().shared_attempt
        else {
            unreachable!()
        };
        let history = std::mem::take(&mut wait.record_history);
        assert!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .is_err()
        );
        let Some(SharedAttempt::Wait(wait)) =
            &mut e.stream_calls.get_mut(&call.id).unwrap().shared_attempt
        else {
            unreachable!()
        };
        wait.record_history = history;
        e.stream_calls.get_mut(&call.id).unwrap().no_store_completed = true;
        assert!(
            e.call_wait_binding(owner, call.id, binding().0, binding().1)
                .is_err()
        );
        e.stream_calls.get_mut(&call.id).unwrap().no_store_completed = false;
        assert!(e.stream_calls[&call.id].physical_pin_required);
        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, before.2);
        assert_eq!(e.native_trace_fixture(), trace);
        // Generic modeled readiness is deliberately true. It is neither the
        // actual saved Poll scan nor authority to wake this pending attempt.
        e.channels
            .get_mut(&NetworkChannelId(1))
            .unwrap()
            .explicit_readiness
            .error = true;
        e.channels
            .get_mut(&NetworkChannelId(1))
            .unwrap()
            .refresh_readiness();
        assert!(e.poll_readable(f.binding.open_file).unwrap());
    }
    f.scheduler.controlled_pending_record_poll(
        f.root.owner(),
        call.id,
        f.deadline,
        f.engine.clone(),
    );
}

#[tokio::test]
async fn shared_record_poll_ready_publication_keeps_pin_and_never_issues_pending() {
    let (f, call, mut peer, retained) = poll_captured(false).await;
    peer.write_all(b"abc").unwrap();
    wait_bytes(&retained, 3);
    let origin = f.record_probe(call.id).await;
    assert_eq!(scan(&f, &origin).await, SharedProbeProgress::EligibleSource);
    let receipt = publish(&f, &origin, source(&f, &origin), f.now).unwrap();
    assert_ne!(receipt.source().revents() & libc::POLLIN, 0);
    assert!(f.complete_pending(&origin).is_err());
    let mut e = f.engine.lock().unwrap();
    // Actual native borrower still succeeds: publication did not delete its
    // confirmed final lease, close the retained pin, or claim an output store.
    f.runtime
        .with_shared_record_poll_publication(&origin, &mut e, |_, _| Ok(()))
        .unwrap();
    assert!(e.stream_calls[&call.id].physical_pin_required);
    assert_eq!(e.stream_calls[&call.id].phase, StreamCallPhase::Active);
    assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
}

#[tokio::test]
async fn shared_record_poll_late_time_or_changed_controls_cannot_publish() {
    for change in 0..3 {
        let (f, call, _peer, _retained) = poll_captured(false).await;
        let origin = f.record_probe(call.id).await;
        assert_eq!(
            scan(&f, &origin).await,
            SharedProbeProgress::PendingCandidate
        );
        let actual = source(&f, &origin);
        let before = f.engine.lock().unwrap().native_trace_fixture();
        let mut now = f.now;
        match change {
            0 => now = LogicalTime::from_nanos(now.as_nanos() + 1),
            1 => {
                f.engine
                    .lock()
                    .unwrap()
                    .channels
                    .get_mut(&NetworkChannelId(1))
                    .unwrap()
                    .local_control_generation += 1
            }
            2 => {
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
                    .receive_low_water = 3
            }
            _ => unreachable!(),
        }
        assert!(publish(&f, &origin, actual, now).is_err());
        assert_eq!(f.engine.lock().unwrap().native_trace_fixture(), before);
        assert!(f.complete_pending(&origin).is_err());
        assert!(f.engine.lock().unwrap().stream_calls[&call.id].physical_pin_required);
    }
}

#[tokio::test]
async fn shared_record_poll_foreign_source_cannot_spend_the_selected_receipt() {
    let (f, call, _peer, _retained) = poll_captured(false).await;
    let origin = f.record_probe(call.id).await;
    assert_eq!(
        scan(&f, &origin).await,
        SharedProbeProgress::PendingCandidate
    );
    let (foreign, other, _other_peer, _other_retained) = poll_captured(false).await;
    let other_origin = foreign.record_probe(other.id).await;
    assert_eq!(
        scan(&foreign, &other_origin).await,
        SharedProbeProgress::PendingCandidate
    );
    let before = trace_and_queue(&f);
    assert!(publish(&f, &origin, source(&foreign, &other_origin), f.now).is_err());
    assert_eq!(trace_and_queue(&f), before);
    assert!(f.complete_pending(&origin).is_err());
    publish(&f, &origin, source(&f, &origin), f.now).unwrap();
    assert!(f.complete_pending(&origin).is_ok());
    assert!(foreign.complete_pending(&other_origin).is_err());
}

#[tokio::test]
async fn shared_record_poll_invalid_duplicate_candidate_retains_trace_and_native_debt() {
    let (f, call, _peer, _retained) = poll_captured(true).await;
    let origin = f.record_probe(call.id).await;
    assert_eq!(
        scan(&f, &origin).await,
        SharedProbeProgress::PendingCandidate
    );
    let before = f.engine.lock().unwrap().native_trace_fixture();
    let failure = publish(&f, &origin, source(&f, &origin), f.now).unwrap_err();
    assert!(
        failure.to_string().contains("InvalidNativeObservation"),
        "{failure}"
    );
    assert_eq!(f.engine.lock().unwrap().native_trace_fixture(), before);
    assert!(f.complete_pending(&origin).is_err());
    let mut e = f.engine.lock().unwrap();
    f.runtime
        .with_shared_record_poll_publication(&origin, &mut e, |_, _| Ok(()))
        .unwrap();
    assert!(e.stream_calls[&call.id].physical_pin_required);
}

#[path = "poll_record_output_tests.rs"]
mod poll_record_output_tests;
