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
