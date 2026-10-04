//! Real logical Call/Normal/census and runtime exclusion, with supplied backend
//! store outcomes. This does not authenticate guest memory or qualify native M2.
use reverie::syscalls::Errno;
use reverie::syscalls::NativeUserStoreOutcome as Outcome;
use reverie::syscalls::NativeUserStoreRefusal;

use super::*;
use crate::tool_global::SharedPollStoreAttempt;

async fn one_poll(f: &Fixture, timeout: i32) -> NetworkStreamCall {
    let owner = f.root.owner();
    let read = {
        let mut e = f.engine.lock().unwrap();
        let NetworkFdReadBegin::Admitted(read) = e
            .begin_fd_read(owner, f.root.files(), f.binding.slot.fd)
            .unwrap()
        else {
            panic!("original FD read");
        };
        *read
    };
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(f.root.clone(), &f.engine, None)
        .await
        .unwrap();
    f.runtime
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
                    e.finish_socket_control(
                        owner,
                        read.control.unwrap(),
                        NetworkSocketControlFinish::Unchanged,
                    )
                    .unwrap();
                    let raw = Poll::new()
                        .with_fds(reverie::syscalls::AddrMut::from_raw(0x1000))
                        .with_nfds(1)
                        .with_timeout(timeout)
                        .into_parts();
                    let deadline = (timeout >= 0).then(|| {
                        LogicalTime::from_nanos(f.now.as_nanos() + timeout as u64 * 1_000_000)
                    });
                    let intent = SharedWaitIntent::Poll(Arc::new(
                        OriginalPollIntent::new(
                            raw,
                            vec![(f.binding, libc::POLLIN)],
                            f.now,
                            deadline,
                        )
                        .unwrap(),
                    ));
                    e.attach_shared_wait_call(call.id, f.binding, intent, &grant, admission, f.now)
                        .unwrap();
                    assert!(e.validate_fd_read_grant(owner, &read).is_err());
                    Ok(call)
                })
        })
        .unwrap()
}

#[tokio::test]
async fn shared_poll_output_replay_actual_reservation_full_store_retires_without_consumption() {
    let f = fixture_with_inputs(false, true).await;
    let call = one_poll(&f, 5000).await;
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
        .await
        .unwrap();
    let now = LogicalTime::from_nanos(f.deadline.as_nanos() + 1);
    let mut retained = None;
    let mut weak = None;
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            let count = f.runtime.with_shared_replay_poll_output(
                &prefix,
                lineage,
                &mut e,
                call.id,
                |e, proof| {
                    let SharedPollDecision::Store(plan) = e
                        .plan_shared_replay_poll(call.id, &grant, proof, now)
                        .unwrap()
                    else {
                        panic!("actual nonzero wins after deadline");
                    };
                    e.reserve_shared_replay_poll(plan, &grant, proof, now)
                        .map_err(std::io::Error::other)
                },
                |e, source, interval| {
                    assert_eq!(source.input(), (f.binding.slot.fd, libc::POLLIN, 5000));
                    assert_eq!(source.revents(), libc::POLLIN);
                    assert!(
                        e.begin_stream_call_release(f.root.owner(), call.id)
                            .is_err()
                    );
                    weak = Some(Arc::downgrade(interval));
                    retained = Some(interval.clone());
                    e.with_shared_poll_store_retention(source, &grant, now, |retainer| {
                        retainer.retain(SharedPollStoreAttempt::controlled_with_interval(
                            source.clone(),
                            Outcome::Attempted {
                                raw: Ok(2),
                                postcheck: Ok(()),
                            },
                            interval.clone(),
                        ))
                    })
                    .unwrap()
                    .unwrap();
                    let count = e
                        .complete_shared_replay_poll_store(source, &grant, now)
                        .unwrap();
                    assert!(!e.stream_calls.contains_key(&call.id));
                    assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                    assert_eq!(
                        e.shadow.as_ref().unwrap().sockets[&f.binding.open_file].consume_epoch,
                        0
                    );
                    Ok(count)
                },
            )?;
            assert_eq!(count, 1);
            Ok(())
        })
        .unwrap();
    assert!(weak.as_ref().unwrap().upgrade().is_some());
    drop(retained);
    assert!(weak.unwrap().upgrade().is_none());
}

#[tokio::test]
async fn shared_poll_output_replay_failed_effects_retain_call_interval_and_prevent_retry() {
    for (outcome, duplicate) in [
        (Outcome::Refused(NativeUserStoreRefusal::WriteDenied), false),
        (
            Outcome::Attempted {
                raw: Ok(1),
                postcheck: Ok(()),
            },
            false,
        ),
        (
            Outcome::Attempted {
                raw: Err(Errno::EFAULT),
                postcheck: Ok(()),
            },
            false,
        ),
        (
            Outcome::Attempted {
                raw: Ok(2),
                postcheck: Err(Errno::EBUSY),
            },
            false,
        ),
        (
            Outcome::Attempted {
                raw: Ok(2),
                postcheck: Ok(()),
            },
            true,
        ),
    ] {
        let f = fixture_with_inputs(false, true).await;
        let call = one_poll(&f, 5000).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        let now = LogicalTime::from_nanos(f.now.as_nanos() + 37);
        let mut weak = None;
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime.with_shared_replay_poll_output(
                    &prefix,
                    lineage,
                    &mut e,
                    call.id,
                    |e, proof| {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_replay_poll(call.id, &grant, proof, now)
                            .unwrap()
                        else {
                            panic!("ready");
                        };
                        e.reserve_shared_replay_poll(plan, &grant, proof, now)
                            .map_err(std::io::Error::other)
                    },
                    |e, source, interval| {
                        weak = Some(Arc::downgrade(interval));
                        e.with_shared_poll_store_retention(source, &grant, now, |retainer| {
                            retainer
                                .retain(SharedPollStoreAttempt::controlled_with_interval(
                                    source.clone(),
                                    outcome,
                                    interval.clone(),
                                ))
                                .unwrap();
                            if duplicate {
                                assert!(
                                    retainer
                                        .retain(SharedPollStoreAttempt::controlled_with_interval(
                                            source.clone(),
                                            Outcome::Attempted {
                                                raw: Ok(2),
                                                postcheck: Ok(())
                                            },
                                            interval.clone()
                                        ))
                                        .is_err()
                                );
                            }
                        })
                        .unwrap();
                        assert!(
                            e.complete_shared_replay_poll_store(source, &grant, now)
                                .is_err()
                        );
                        assert!(
                            e.with_shared_poll_store_retention(source, &grant, now, |_| panic!(
                                "no second effect"
                            ))
                            .is_err()
                        );
                        assert!(
                            e.begin_stream_call_release(f.root.owner(), call.id)
                                .is_err()
                        );
                        assert!(e.stream_calls.contains_key(&call.id));
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        assert!(e.finish().is_err());
                        Ok(())
                    },
                )
            })
            .unwrap();
        assert!(weak.unwrap().upgrade().is_some());
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            f.runtime
                .controlled_shared_source_worker_submission(ran.clone())
                .is_err()
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }
}

#[tokio::test]
async fn shared_poll_output_missing_final_scan_and_changed_profile_refuse_before_store() {
    for timeout in [0, 5000, -1] {
        let f = fixture().await;
        let call = one_poll(&f, timeout).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime
                    .with_shared_attempt_prefix(&prefix, &mut e, |e, proof| {
                        if timeout == 5000 {
                            assert!(matches!(
                                e.plan_shared_replay_poll(call.id, &grant, proof, f.now)
                                    .unwrap(),
                                SharedPollDecision::Pending
                            ));
                        }
                        assert!(
                            e.plan_shared_replay_poll(call.id, &grant, proof, f.deadline)
                                .is_err()
                        );
                        assert!(e.stream_calls.contains_key(&call.id));
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        Ok(())
                    })
            })
            .unwrap();
    }
}

#[tokio::test]
async fn shared_poll_output_prewrite_lifetime_control_time_and_attempt_refuse_without_effect() {
    for variant in 0..5 {
        let f = fixture_with_inputs(false, true).await;
        let call = one_poll(&f, 5000).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        let now = LogicalTime::from_nanos(f.now.as_nanos() + 37);
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime.with_shared_replay_poll_output(
                    &prefix,
                    lineage,
                    &mut e,
                    call.id,
                    |e, proof| {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_replay_poll(call.id, &grant, proof, now)
                            .unwrap()
                        else {
                            panic!("ready");
                        };
                        e.reserve_shared_replay_poll(plan, &grant, proof, now)
                            .map_err(std::io::Error::other)
                    },
                    |e, source, _interval| {
                        let checked_at = match variant {
                            0 => {
                                e.release_stream_call_lifetime(
                                    f.root.owner(),
                                    call.id,
                                    call.open_file,
                                )
                                .unwrap();
                                now
                            }
                            1 => {
                                e.channels
                                    .get_mut(&NetworkChannelId(1))
                                    .unwrap()
                                    .local_control_generation += 1;
                                now
                            }
                            2 => {
                                e.shadow
                                    .as_mut()
                                    .unwrap()
                                    .sockets
                                    .get_mut(&call.open_file)
                                    .unwrap()
                                    .options
                                    .receive_low_water += 1;
                                now
                            }
                            3 => LogicalTime::from_nanos(now.as_nanos() + 1),
                            4 => {
                                let Some(SharedAttempt::Wait(wait)) =
                                    &mut e.stream_calls.get_mut(&call.id).unwrap().shared_attempt
                                else {
                                    unreachable!()
                                };
                                let AttemptPhase::Active { ordinal, .. } = &mut wait.phase else {
                                    unreachable!()
                                };
                                *ordinal += 1;
                                now
                            }
                            _ => unreachable!(),
                        };
                        let mut ran = false;
                        assert!(
                            e.with_shared_poll_store_retention(source, &grant, checked_at, |_| {
                                ran = true;
                            })
                            .is_err()
                        );
                        assert!(
                            !ran,
                            "case {variant} must refuse before real write callback"
                        );
                        assert!(
                            e.complete_shared_replay_poll_store(source, &grant, checked_at)
                                .is_err()
                        );
                        assert!(e.stream_calls.contains_key(&call.id));
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        Ok(())
                    },
                )
            })
            .unwrap();
    }
}

#[tokio::test]
async fn shared_poll_replay_released_ready_precedes_later_calls_without_consumption() {
    let mut f = fixture_with_inputs(false, true).await;
    let sampled = LogicalTime::from_nanos(f.now.as_nanos() + 37);
    let mut original_snapshot = None;
    for delay in [1, 2] {
        // The trace input is unchanged; each original Call starts later.
        f.now = LogicalTime::from_nanos(sampled.as_nanos() + delay);
        let call = one_poll(&f, 5000).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                let count = f.runtime.with_shared_replay_poll_output(
                    &prefix,
                    lineage,
                    &mut e,
                    call.id,
                    |e, proof| {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_replay_poll(call.id, &grant, proof, f.now)
                            .expect("released ready snapshot predating Poll must be observed in the current Call")
                        else {
                            panic!("released nonzero readiness must produce output");
                        };
                        e.reserve_shared_replay_poll(plan, &grant, proof, f.now)
                            .map_err(std::io::Error::other)
                    },
                    |e, source, interval| {
                        assert_eq!(source.input(), (f.binding.slot.fd, libc::POLLIN, 5000));
                        assert_eq!(source.revents(), libc::POLLIN);
                        let snapshot = e
                            .shared_poll_snapshot(f.binding.open_file)
                            .unwrap()
                            .unwrap();
                        assert_eq!(snapshot.observed_at, sampled);
                        assert_eq!(snapshot.consumed_prefix, 0);
                        if let Some(original) = original_snapshot {
                            assert_eq!(snapshot, original);
                        } else {
                            original_snapshot = Some(snapshot);
                        }
                        assert!(
                            e.begin_stream_call_release(f.root.owner(), call.id)
                                .is_err()
                        );
                        e.with_shared_poll_store_retention(source, &grant, f.now, |retainer| {
                            retainer.retain(SharedPollStoreAttempt::controlled_with_interval(
                                source.clone(),
                                Outcome::Attempted {
                                    raw: Ok(2),
                                    postcheck: Ok(()),
                                },
                                interval.clone(),
                            ))
                        })
                        .unwrap()
                        .unwrap();
                        let count = e
                            .complete_shared_replay_poll_store(source, &grant, f.now)
                            .unwrap();
                        assert!(!e.stream_calls.contains_key(&call.id));
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        assert_eq!(
                            e.shadow.as_ref().unwrap().sockets[&f.binding.open_file].consume_epoch,
                            0
                        );
                        assert_eq!(
                            e.shared_poll_snapshot(f.binding.open_file).unwrap(),
                            original_snapshot
                        );
                        Ok(count)
                    },
                )?;
                assert_eq!(count, 1);
                Ok(())
            })
            .unwrap();
    }
}

async fn zero_poll_fixture(final_scan: bool) -> Fixture {
    let mut f = fixture_engine_kind_with_trace(false, false, false, false, false, |trace| {
        let epoch = trace.epoch_global_time().unwrap();
        let started = LogicalTime::from_nanos(epoch.as_nanos() + 38);
        let sampled = if final_scan {
            LogicalTime::from_nanos(started.as_nanos() + 5_000_000_000)
        } else {
            LogicalTime::from_nanos(started.as_nanos() - 1)
        };
        // Construct a quiet connected socket before engine construction: there
        // are no data, EOF or error inputs, including beyond the Poll deadline.
        assert!(matches!(
            trace.inputs[0].event,
            NetworkInputKindV2::Connect(_)
        ));
        trace.inputs.truncate(1);
        trace.native_receive_observations.clear();
        assert!(trace.outputs.is_empty());
        trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
            nodes: trace.release_model.nodes()[..2].to_vec(),
        };
        let ordinal = trace.inputs.len() as u64;
        let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
        let prerequisites = trace.entry_frontier(cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: sampled,
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::SharedRawTcpPollState {
                consumed_prefix: 0,
                revents: 0,
                control_generation: 0,
                receive_low_water: 1,
            },
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut trace.release_model
        else {
            unreachable!()
        };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(cut.0),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites,
        });
    })
    .await;
    f.now = LogicalTime::from_nanos(f.now.as_nanos() + 38);
    let e = f.engine.lock().unwrap();
    let queue = &e.channels[&NetworkChannelId(1)];
    assert!(queue.inbound.is_empty());
    assert!(!queue.peer_write_closed && !queue.local_read_shutdown);
    assert_eq!(e.native_trace_fixture().inputs.len(), 2);
    assert_eq!(
        e.shadow.as_ref().unwrap().sockets[&f.binding.open_file]
            .options
            .receive_low_water,
        1
    );
    drop(e);
    f
}

#[tokio::test]
async fn shared_poll_replay_old_zero_is_pending_but_only_final_zero_can_timeout() {
    for final_scan in [false, true] {
        let f = zero_poll_fixture(final_scan).await;
        let old_sample = LogicalTime::from_nanos(f.now.as_nanos() - 1);
        let deadline = LogicalTime::from_nanos(f.now.as_nanos() + 5_000_000_000);
        let trace = f.engine.lock().unwrap().native_trace_fixture();
        let call = one_poll(&f, 5000).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime
                    .with_shared_attempt_prefix(&prefix, &mut e, |e, proof| {
                        assert!(matches!(
                            e.plan_shared_replay_poll(call.id, &grant, proof, f.now)
                                .unwrap(),
                            SharedPollDecision::Pending
                        ));
                        if !final_scan {
                            assert!(matches!(
                                e.plan_shared_replay_poll(call.id, &grant, proof, deadline),
                                Err(NetworkReplayError::FdPublicationProtocol(message))
                                    if message == "Poll timeout lacks an actual final zero scan"
                            ));
                            assert_eq!(
                                e.shared_poll_snapshot(f.binding.open_file)
                                    .unwrap()
                                    .unwrap()
                                    .observed_at,
                                old_sample
                            );
                            assert!(e.stream_calls.contains_key(&call.id));
                            assert!(
                                e.shared_wait(f.root.owner(), call.id)
                                    .unwrap()
                                    .1
                                    .poll_output
                                    .is_none()
                            );
                        }
                        assert_eq!(e.native_trace_fixture(), trace);
                        assert!(e.channels[&NetworkChannelId(1)].inbound.is_empty());
                        assert!(!e.channels[&NetworkChannelId(1)].peer_write_closed);
                        assert!(!e.channels[&NetworkChannelId(1)].local_read_shutdown);
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        Ok(())
                    })
            })
            .unwrap();
        if !final_scan {
            continue;
        }
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                let count = f.runtime.with_shared_replay_poll_output(
                    &prefix,
                    lineage,
                    &mut e,
                    call.id,
                    |e, proof| {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_replay_poll(call.id, &grant, proof, deadline)
                            .unwrap()
                        else {
                            panic!("the released final zero must produce timeout output");
                        };
                        e.reserve_shared_replay_poll(plan, &grant, proof, deadline)
                            .map_err(std::io::Error::other)
                    },
                    |e, source, interval| {
                        assert_eq!(source.revents(), 0);
                        assert_eq!(source.count(), 0);
                        assert_eq!(
                            e.shared_poll_snapshot(f.binding.open_file)
                                .unwrap()
                                .unwrap()
                                .observed_at,
                            deadline
                        );
                        e.with_shared_poll_store_retention(source, &grant, deadline, |retainer| {
                            retainer.retain(SharedPollStoreAttempt::controlled_with_interval(
                                source.clone(),
                                Outcome::Attempted {
                                    raw: Ok(2),
                                    postcheck: Ok(()),
                                },
                                interval.clone(),
                            ))
                        })
                        .unwrap()
                        .unwrap();
                        let count = e
                            .complete_shared_replay_poll_store(source, &grant, deadline)
                            .unwrap();
                        assert!(!e.stream_calls.contains_key(&call.id));
                        assert_eq!(e.native_trace_fixture(), trace);
                        assert!(e.channels[&NetworkChannelId(1)].inbound.is_empty());
                        assert!(!e.channels[&NetworkChannelId(1)].peer_write_closed);
                        assert!(!e.channels[&NetworkChannelId(1)].local_read_shutdown);
                        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                        assert_eq!(
                            e.shadow.as_ref().unwrap().sockets[&f.binding.open_file].consume_epoch,
                            0
                        );
                        Ok(count)
                    },
                )?;
                assert_eq!(count, 0);
                Ok(())
            })
            .unwrap();
    }
}

#[tokio::test]
async fn shared_poll_replay_future_source_and_shifted_reservation_refuse() {
    for future_source in [true, false] {
        let mut f = fixture_with_inputs(false, true).await;
        let sampled = LogicalTime::from_nanos(f.now.as_nanos() + 37);
        if !future_source {
            f.now = LogicalTime::from_nanos(sampled.as_nanos() + 1);
        }
        let call = one_poll(&f, 5000).await;
        let prefix = f
            .runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
            .await
            .unwrap();
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                let trace = e.native_trace_fixture();
                f.runtime.with_shared_attempt_prefix(&prefix, &mut e, |e, proof| {
                    if future_source {
                        let earlier = LogicalTime::from_nanos(sampled.as_nanos() - 1);
                        assert!(matches!(
                            e.plan_shared_replay_poll(call.id, &grant, proof, earlier).unwrap(),
                            SharedPollDecision::Pending
                        ));
                        // Deliberately inconsistent state exercises the upper
                        // bound; ordinary release never makes this sample due.
                        e.release_native_eligible(sampled).unwrap();
                        assert!(matches!(
                            e.plan_shared_replay_poll(call.id, &grant, proof, earlier),
                            Err(NetworkReplayError::FdPublicationProtocol(message))
                                if message == "Poll observation is outside its original call interval"
                        ));
                    } else {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_replay_poll(call.id, &grant, proof, f.now)
                            .unwrap()
                        else {
                            panic!("released ready sample");
                        };
                        let later = LogicalTime::from_nanos(f.now.as_nanos() + 1);
                        assert!(matches!(
                            e.reserve_shared_replay_poll(plan, &grant, proof, later),
                            Err(NetworkReplayError::FdPublicationProtocol(message))
                                if message == "Poll reservation changed selected observation"
                        ));
                    }
                    assert!(e.stream_calls.contains_key(&call.id));
                    assert!(e.shared_wait(f.root.owner(), call.id).unwrap().1.poll_output.is_none());
                    assert_eq!(
                        e.shared_poll_snapshot(f.binding.open_file).unwrap().unwrap().observed_at,
                        sampled
                    );
                    assert_eq!(e.native_trace_fixture(), trace);
                    assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                    Ok(())
                })
            })
            .unwrap();
    }
}
