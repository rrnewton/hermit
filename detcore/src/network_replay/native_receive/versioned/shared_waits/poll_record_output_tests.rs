//! Actual TCP PollState, original worker joins and native pin close. Only the
//! original guest installation/backend two-byte store are controlled premises.
use reverie::syscalls::Errno;
use reverie::syscalls::NativeUserStoreOutcome as Outcome;

use super::*;
use crate::network_replay::shared_waits::SharedPollDecision;
use crate::network_replay::shared_waits::SharedPollSource;
use crate::tool_global::SharedPollStoreAttempt;

#[tokio::test]
async fn shared_poll_output_record_full_store_keeps_pin_until_actual_close() {
    for ready in [false, true] {
        let (mut f, call, mut peer, _held) = poll_captured(false).await;
        // An actual final scan at the original absolute deadline. Ready wins;
        // an actual zero is eligible only because this scan is final.
        f.now = f.deadline;
        if ready {
            peer.write_all(b"x").unwrap();
        }
        let origin = f.record_probe(call.id).await;
        assert_eq!(scan(&f, &origin).await, SharedProbeProgress::EligibleSource);
        let publication = publish(&f, &origin, source(&f, &origin), f.now).unwrap();
        let before = trace_and_queue(&f);
        let mut local_interval = None;
        let mut actual_source: Option<Arc<SharedPollSource>> = None;
        f.runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                let count = f.runtime.with_shared_record_poll_output(
                    &publication,
                    &mut e,
                    |e, proof| {
                        let SharedPollDecision::Store(plan) = e
                            .plan_shared_record_poll(&publication, &grant, proof, f.now)
                            .unwrap()
                        else {
                            panic!("actual final full scan");
                        };
                        e.reserve_shared_record_poll(plan, &grant, proof, f.now)
                            .map_err(std::io::Error::other)
                    },
                    |e, proof, selected, interval| {
                        local_interval = Some(interval.clone());
                        actual_source = Some(selected.clone());
                        assert!(
                            e.begin_stream_call_release(f.root.owner(), call.id)
                                .is_err()
                        );
                        e.with_shared_poll_store_retention(selected, &grant, f.now, |retainer| {
                            retainer.retain(SharedPollStoreAttempt::controlled_with_interval(
                                selected.clone(),
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
                            .complete_shared_record_poll_store(selected, &grant, proof, f.now)
                            .unwrap();
                        assert!(
                            e.stream_calls.contains_key(&call.id),
                            "logical and actual native pin survive output"
                        );
                        assert!(e.shared_record_poll_output_committed(selected));
                        assert!(
                            e.stream_call_open_file(f.root.owner(), call.id).is_err(),
                            "successful Poll never reopens generic stream I/O"
                        );
                        assert!(
                            e.complete_shared_record_poll_store(selected, &grant, proof, f.now)
                                .is_err()
                        );
                        Ok(count)
                    },
                )?;
                assert_eq!(count, i64::from(ready));
                Ok(())
            })
            .unwrap();
        assert_eq!(trace_and_queue(&f), before);
        let weak = Arc::downgrade(local_interval.as_ref().unwrap());
        assert!(weak.upgrade().is_some());
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            f.runtime
                .controlled_shared_source_worker_submission(ran.clone())
                .is_err()
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        drop(local_interval);
        assert!(
            weak.upgrade().is_none(),
            "full success releases only interval, not pin"
        );
        f.engine
            .lock()
            .unwrap()
            .begin_stream_call_release(f.root.owner(), call.id)
            .unwrap();
        f.runtime
            .release_native_stream(f.root.owner(), call.id)
            .await
            .unwrap();
        let completion = f
            .engine
            .lock()
            .unwrap()
            .complete_stream_call_release(f.root.owner(), call.id)
            .unwrap();
        completion.into_result().unwrap();
        f.runtime
            .finish_native_stream_release(f.root.owner(), call.id)
            .unwrap();
        assert!(!f.engine.lock().unwrap().stream_calls.contains_key(&call.id));
        assert!(actual_source.unwrap().record_publication().is_some());
    }
}

#[tokio::test]
async fn shared_poll_output_record_partial_or_postcheck_keeps_actual_lease_and_interval() {
    for outcome in [
        Outcome::Attempted {
            raw: Ok(1),
            postcheck: Ok(()),
        },
        Outcome::Attempted {
            raw: Ok(2),
            postcheck: Err(Errno::EBUSY),
        },
    ] {
        let (f, call, mut peer, _held) = poll_captured(false).await;
        peer.write_all(b"x").unwrap();
        let origin = f.record_probe(call.id).await;
        assert_eq!(scan(&f, &origin).await, SharedProbeProgress::EligibleSource);
        let publication = publish(&f, &origin, source(&f, &origin), f.now).unwrap();
        let before = trace_and_queue(&f);
        let mut weak = None;
        let result: std::io::Result<()> =
            f.runtime
                .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                    let grant = f
                        .scheduler
                        .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                    let mut e = f.engine.lock().unwrap();
                    f.runtime.with_shared_record_poll_output(
                        &publication,
                        &mut e,
                        |e, proof| {
                            let SharedPollDecision::Store(plan) = e
                                .plan_shared_record_poll(&publication, &grant, proof, f.now)
                                .unwrap()
                            else {
                                panic!("ready");
                            };
                            e.reserve_shared_record_poll(plan, &grant, proof, f.now)
                                .map_err(std::io::Error::other)
                        },
                        |e, proof, selected, interval| {
                            weak = Some(Arc::downgrade(interval));
                            e.with_shared_poll_store_retention(
                                selected,
                                &grant,
                                f.now,
                                |retainer| {
                                    retainer.retain(
                                        SharedPollStoreAttempt::controlled_with_interval(
                                            selected.clone(),
                                            outcome,
                                            interval.clone(),
                                        ),
                                    )
                                },
                            )
                            .unwrap()
                            .unwrap();
                            assert!(
                                e.complete_shared_record_poll_store(selected, &grant, proof, f.now)
                                    .is_err()
                            );
                            assert!(
                                e.begin_stream_call_release(f.root.owner(), call.id)
                                    .is_err()
                            );
                            assert!(e.shadow_probes.contains_key(&origin.lease()));
                            assert!(e.socket_controls.contains_key(&f.binding.open_file));
                            assert!(!e.shared_record_poll_output_committed(selected));
                            // Even an erroneous successful closure cannot authorize
                            // native lease retirement without exact engine completion.
                            Ok(())
                        },
                    )
                });
        assert!(result.is_err());
        assert!(weak.unwrap().upgrade().is_some());
        assert_eq!(trace_and_queue(&f), before);
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            f.runtime
                .controlled_shared_source_worker_submission(ran.clone())
                .is_err()
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            f.runtime
                .release_native_stream(f.root.owner(), call.id)
                .await
                .is_err()
        );
        assert!(f.engine.lock().unwrap().finish().is_err());
    }
}

#[tokio::test]
async fn shared_poll_output_record_old_zero_cannot_become_timeout_or_reuse_publication() {
    let (f, call, _peer, _held) = poll_captured(false).await;
    let origin = f.record_probe(call.id).await;
    assert_eq!(
        scan(&f, &origin).await,
        SharedProbeProgress::PendingCandidate
    );
    let publication = publish(&f, &origin, source(&f, &origin), f.now).unwrap();
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime
                .with_shared_record_poll_publication(&origin, &mut e, |e, proof| {
                    assert!(matches!(
                        e.plan_shared_record_poll(&publication, &grant, proof, f.now)
                            .unwrap(),
                        SharedPollDecision::Pending
                    ));
                    assert!(
                        e.plan_shared_record_poll(&publication, &grant, proof, f.deadline)
                            .is_err()
                    );
                    assert!(
                        e.begin_stream_call_release(f.root.owner(), call.id)
                            .is_err()
                    );
                    assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
                    Ok(())
                })
        })
        .unwrap();
}
