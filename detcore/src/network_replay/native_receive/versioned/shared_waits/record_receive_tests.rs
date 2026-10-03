//! Real TCP Peek/Consume, native Pending and actual worker joins. The original
//! installation, provider copy5 geometry and backend writer context are supplied
//! premises. Store effects below are real local writes with explicit outcomes;
//! these controls do not qualify original guest PVM or native BPF composition.
use reverie::syscalls::NativeUserStoreOutcome;

use super::*;
use crate::network_replay::shared_waits::PreparedSharedRecordReceivePublication;
use crate::network_replay::shared_waits::SharedRecordReceivePlan;
use crate::network_replay::shared_waits::SharedRecordReceiveSource;
use crate::network_replay::shared_waits::SharedRecordStored;
use crate::network_runtime::shared_waits::JoinedSharedDrain;
use crate::tool_global::SharedRecordStoreAttempt;

async fn receive_captured() -> (
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
    let lowat = 3i32;
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
    f.engine
        .lock()
        .unwrap()
        .controlled_shared_record_receive_initial(f.binding, f.now, cursor);
    let (call, submission) = f.prepare_capture().await;
    let joined = f
        .runtime
        .controlled_shared_capture_with(submission, f.recovery(), move || Ok(held.into()))
        .await
        .unwrap();
    assert_eq!(f.finish_capture(&joined).unwrap().id, call.id);
    (f, call, peer, retained)
}
async fn eligible(f: &Fixture, call: NetworkStreamCallId) -> Arc<SharedRecordProbe> {
    let origin = f.record_probe(call).await;
    peek(f, &origin).await;
    assert_eq!(
        f.effect(&origin, NetworkStreamPhysicalEffect::PollState)
            .await,
        SharedProbeProgress::EligibleSource
    );
    origin
}
fn plan(f: &Fixture, origin: &Arc<SharedRecordProbe>) -> std::io::Result<SharedRecordReceivePlan> {
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime
                .with_shared_record_receive_source(origin, &mut e, |e, proof| {
                    e.plan_shared_record_receive(origin, &grant, proof, f.now)
                        .map_err(std::io::Error::other)
                })
        })
}
struct StoredOutput {
    source: Arc<SharedRecordReceiveSource>,
    stored: Option<SharedRecordStored>,
    interval: std::sync::Weak<crate::network_runtime::NativeSourceInterval>,
    bytes: [u8; 10],
}
fn store(
    f: &Fixture,
    origin: &Arc<SharedRecordProbe>,
    count: usize,
    postcheck: bool,
) -> StoredOutput {
    let SharedRecordReceivePlan::Bytes(plan) = plan(f, origin).unwrap() else {
        panic!("expected canonical bytes")
    };
    let mut bytes = [0xa5u8; 10];
    let mut captured = None;
    let result = f
        .runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime.with_shared_record_receive_output(
                origin,
                &mut e,
                |e, proof| {
                    e.reserve_shared_record_receive(plan, &grant, proof, f.now)
                        .map_err(std::io::Error::other)
                },
                |e, _proof, source, interval| {
                    let n = count.min(source.len());
                    let weak = Arc::downgrade(interval);
                    e.with_shared_record_store_retention(source, &grant, f.now, |retainer| {
                        bytes[1..1 + n].copy_from_slice(&source.bytes()[..n]);
                        retainer.retain(SharedRecordStoreAttempt::controlled_with_interval(
                            source.clone(),
                            NativeUserStoreOutcome::Attempted {
                                raw: Ok(n),
                                postcheck: if postcheck {
                                    Ok(())
                                } else {
                                    Err(reverie::syscalls::Errno::EBUSY)
                                },
                            },
                            interval.clone(),
                        ))
                    })
                    .map_err(std::io::Error::other)?
                    .map_err(std::io::Error::other)?;
                    let stored = e.complete_shared_record_store(source, &grant, f.now);
                    assert_eq!(stored.is_ok(), n == source.len() && postcheck);
                    assert!(
                        weak.upgrade().is_some(),
                        "local callback retains interval after full store"
                    );
                    assert!(
                        e.begin_stream_call_release(f.root.owner(), source.call())
                            .is_err(),
                        "guest store is not actual Consume or native close"
                    );
                    captured = Some((source.clone(), stored.ok(), weak));
                    if n == source.len() && postcheck {
                        Ok(())
                    } else {
                        Err(std::io::Error::other("actual failed store retained"))
                    }
                },
            )
        });
    let (source, stored, interval) = captured.unwrap();
    assert_eq!(result.is_ok(), stored.is_some());
    assert_eq!(bytes[0], 0xa5);
    assert_eq!(bytes[9], 0xa5);
    StoredOutput {
        source,
        stored,
        interval,
        bytes,
    }
}
fn confirm(
    f: &Fixture,
    joined: &Arc<JoinedSharedDrain>,
) -> std::io::Result<Arc<PreparedSharedRecordReceivePublication>> {
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            let mut e = f.engine.lock().unwrap();
            f.runtime
                .with_shared_record_drain(joined, &mut e, |e, proof| {
                    e.confirm_shared_record_drain(proof, &grant, f.now)
                        .map_err(std::io::Error::other)
                })
        })
}
fn publish(
    f: &Fixture,
    prepared: &Arc<PreparedSharedRecordReceivePublication>,
) -> std::io::Result<usize> {
    f.runtime
        .with_shared_foreground_lineage(f.root.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(f.root.owner(), lineage)?;
            f.runtime.publish_shared_record_receive(
                prepared,
                &mut f.engine.lock().unwrap(),
                &grant,
                f.now,
            )
        })
}

#[tokio::test]
async fn shared_record_receive_full_store_real_drain_publishes_exact_contiguous_source_once() {
    let (f, call, mut peer, retained) = receive_captured().await;
    peer.write_all(b"abcdefgh").unwrap();
    wait_bytes(&retained, 8);
    let origin = eligible(&f, call.id).await;
    let before = trace_and_queue(&f);
    let output = store(&f, &origin, 8, true);
    assert_eq!(&output.bytes[1..9], b"abcdefgh");
    assert!(
        output.interval.upgrade().is_none(),
        "callback ended before worker admission"
    );
    assert_ne!(
        output.source.lease(),
        origin.lease(),
        "Consume owns a distinct successor lease"
    );
    assert_eq!(trace_and_queue(&f), before);
    wait_bytes(&retained, 8);
    let joined = f
        .runtime
        .controlled_shared_record_drain(output.stored.unwrap(), false)
        .await
        .unwrap();
    wait_bytes(&retained, 0);
    assert_eq!(joined.observed().bytes, b"abcdefgh");
    assert!(
        joined
            .observed()
            .helper_copy
            .as_ref()
            .unwrap()
            .binding()
            .succeeds(output.source.predecessor())
    );
    let prepared = confirm(&f, &joined).unwrap();
    assert!(confirm(&f, &joined).is_err());
    assert_eq!(
        trace_and_queue(&f),
        before,
        "physical confirmation is not trace publication"
    );
    assert_eq!(publish(&f, &prepared).unwrap(), 8);
    assert!(publish(&f, &prepared).is_err());
    {
        let e = f.engine.lock().unwrap();
        let trace = e.native_trace_fixture();
        trace.validate().unwrap();
        assert_eq!(trace.inputs.len(), 2);
        assert_eq!(
            trace.inputs[1].event,
            NetworkInputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: b"abcdefgh".to_vec()
            }
        );
        assert_eq!(trace.inputs[1].release.not_before_global_time, f.now);
        assert_eq!(
            trace.inputs[1].release.receive_entry_cut,
            NetworkReceiveEntryCutV4(2)
        );
        assert_eq!(
            trace.inputs[1].release.prerequisites,
            vec![NetworkReleaseNodeIdV4(1)]
        );
        assert_eq!(trace.native_receive_observations.len(), 1);
        let actual = &trace.native_receive_observations[0];
        assert_eq!((actual.stream_offset, actual.length), (0, 8));
        assert!(actual.fragments.iter().all(|u| u.disposition
            == detcore_model::network_trace::NetworkNativeCopyDispositionV4::Consume));
        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 8);
        assert!(!e.stream_operations.contains_key(&output.source.lease()));
        assert!(e.stream_calls[&call.id].helper_copy.is_none());
    }
    f.engine
        .lock()
        .unwrap()
        .begin_stream_call_release(f.root.owner(), call.id)
        .unwrap();
    f.runtime
        .release_native_stream(f.root.owner(), call.id)
        .await
        .unwrap();
    f.engine
        .lock()
        .unwrap()
        .complete_stream_call_release(f.root.owner(), call.id)
        .unwrap()
        .into_result()
        .unwrap();
    f.runtime
        .finish_native_stream_release(f.root.owner(), call.id)
        .unwrap();
    assert!(!f.engine.lock().unwrap().stream_calls.contains_key(&call.id));
}

#[tokio::test]
async fn shared_record_receive_partial_and_failed_postcheck_keep_interval_and_source_debt() {
    for (count, postcheck) in [(2, true), (8, false)] {
        let (f, call, mut peer, retained) = receive_captured().await;
        peer.write_all(b"abcdefgh").unwrap();
        wait_bytes(&retained, 8);
        let origin = eligible(&f, call.id).await;
        let before = trace_and_queue(&f);
        let output = store(&f, &origin, count, postcheck);
        assert!(output.stored.is_none());
        assert!(output.interval.upgrade().is_some());
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        assert!(
            f.runtime
                .controlled_shared_source_worker_submission(ran.clone())
                .is_err()
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(trace_and_queue(&f), before);
        wait_bytes(&retained, 8);
        {
            let mut e = f.engine.lock().unwrap();
            assert!(
                e.begin_stream_call_release(f.root.owner(), call.id)
                    .is_err()
            );
            assert!(e.finish().is_err());
            let Some(SharedAttempt::Wait(wait)) = &e.stream_calls[&call.id].shared_attempt else {
                unreachable!()
            };
            assert!(wait.record_receive.is_some());
            assert!(e.stream_operations.contains_key(&output.source.lease()));
            drop(e);
        }
        assert!(
            f.runtime
                .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
                .await
                .is_err(),
            "failed retained interval cannot become a settled prefix"
        );
    }
}

#[tokio::test]
async fn shared_record_receive_changed_actual_drain_retains_physical_history_without_trace_commit()
{
    for bad_geometry in [false, true] {
        let (f, call, mut peer, retained) = receive_captured().await;
        peer.write_all(b"abcdefgh").unwrap();
        wait_bytes(&retained, 8);
        let origin = eligible(&f, call.id).await;
        let before = trace_and_queue(&f);
        let output = store(&f, &origin, 8, true);
        if !bad_geometry {
            // Controlled external interference: the actual later Consume reads
            // different bytes. Neither old Peek nor full store can hide that.
            let mut removed = [0u8; 8];
            assert_eq!(
                unsafe {
                    libc::recv(
                        retained.as_raw_fd(),
                        removed.as_mut_ptr().cast(),
                        8,
                        libc::MSG_DONTWAIT,
                    )
                },
                8
            );
            peer.write_all(b"ABCDEFGH").unwrap();
            wait_bytes(&retained, 8);
        }
        let joined = f
            .runtime
            .controlled_shared_record_drain(output.stored.unwrap(), bad_geometry)
            .await
            .unwrap();
        assert!(confirm(&f, &joined).is_err());
        assert!(
            confirm(&f, &joined).is_err(),
            "failed real result cannot be consumed again"
        );
        assert_eq!(trace_and_queue(&f), before);
        let mut e = f.engine.lock().unwrap();
        let physical = e.shadow.as_ref().unwrap().sockets[&f.binding.open_file]
            .native
            .as_ref()
            .unwrap()
            .physical_observed;
        if bad_geometry {
            assert_eq!(physical.bytes, 0);
        } else {
            assert_eq!(
                physical.bytes, 8,
                "valid physical history survives semantic byte mismatch"
            );
        }
        assert!(
            e.begin_stream_call_release(f.root.owner(), call.id)
                .is_err()
        );
        assert!(e.finish().is_err());
    }
}

#[tokio::test]
async fn shared_record_receive_candidate_refusal_is_atomic_after_actual_drain() {
    let (f, call, mut peer, retained) = receive_captured().await;
    peer.write_all(b"abcdefgh").unwrap();
    wait_bytes(&retained, 8);
    let origin = eligible(&f, call.id).await;
    let output = store(&f, &origin, 8, true);
    let joined = f
        .runtime
        .controlled_shared_record_drain(output.stored.unwrap(), false)
        .await
        .unwrap();
    let prepared = confirm(&f, &joined).unwrap();
    // Invalid actual projection input, rather than a fabricated trace-success
    // flag: candidate validation must run before any live trace append.
    f.engine
        .lock()
        .unwrap()
        .shadow
        .as_mut()
        .unwrap()
        .profiles
        .clear();
    let before = f.engine.lock().unwrap().native_trace_fixture();
    assert!(publish(&f, &prepared).is_err());
    let mut e = f.engine.lock().unwrap();
    assert_eq!(e.native_trace_fixture(), before);
    assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
    assert!(e.stream_operations.contains_key(&output.source.lease()));
    assert!(
        e.begin_stream_call_release(f.root.owner(), call.id)
            .is_err()
    );
}

#[tokio::test]
async fn shared_record_receive_eof_has_no_consume_and_late_early_eagain_is_not_timeout() {
    for eof in [true, false] {
        let (mut f, call, peer, retained) = receive_captured().await;
        if eof {
            peer.shutdown(std::net::Shutdown::Write).unwrap();
            let mut row = libc::pollfd {
                fd: retained.as_raw_fd(),
                events: libc::POLLIN | libc::POLLRDHUP,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut row, 1, 1000) }, 1);
            assert_ne!(
                row.revents & libc::POLLRDHUP,
                0,
                "actual peer FIN arrived before the observed Peek"
            );
        }
        let origin = f.record_probe(call.id).await;
        peek(&f, &origin).await;
        let progress = f
            .effect(&origin, NetworkStreamPhysicalEffect::PollState)
            .await;
        if eof {
            assert_eq!(progress, SharedProbeProgress::EligibleSource);
        } else {
            assert_eq!(progress, SharedProbeProgress::PendingCandidate);
            f.now = f.deadline;
        }
        let before = trace_and_queue(&f);
        if !eof {
            assert!(
                plan(&f, &origin).is_err(),
                "late clock cannot certify a fresh deadline observation"
            );
            assert_eq!(trace_and_queue(&f), before);
            continue;
        }
        let SharedRecordReceivePlan::NoStore(plan) = plan(&f, &origin).unwrap() else {
            panic!("actual EOF is NoStore")
        };
        let result = f
            .runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime
                    .with_shared_record_no_store(&plan, &mut e, |e, proof| {
                        // Supplied original held-backend context premise, no guest write.
                        e.commit_shared_record_no_store(&plan, proof, &grant, f.now)
                            .map_err(std::io::Error::other)
                    })
            })
            .unwrap();
        assert_eq!(result, SharedNoStoreResult::Eof);
        let mut e = f.engine.lock().unwrap();
        let trace = e.native_trace_fixture();
        trace.validate().unwrap();
        assert_eq!(
            trace.inputs.last().unwrap().event,
            NetworkInputKindV2::PeerShutdown {
                stream_offset: 0,
                direction: NetworkShutdownV2::Write
            }
        );
        assert!(trace.native_receive_observations.is_empty());
        assert_eq!(e.channels[&NetworkChannelId(1)].inbound_consumed, 0);
        assert!(e.stream_calls[&call.id].native_receive.is_empty());
        e.begin_stream_call_release(f.root.owner(), call.id)
            .unwrap();
    }
}

#[tokio::test]
async fn shared_record_receive_changed_control_or_lifetime_refuses_before_delivery_and_store() {
    for changed_control in [false, true] {
        let (f, call, mut peer, retained) = receive_captured().await;
        peer.write_all(b"abcdefgh").unwrap();
        wait_bytes(&retained, 8);
        let origin = eligible(&f, call.id).await;
        let SharedRecordReceivePlan::Bytes(selected) = plan(&f, &origin).unwrap() else {
            panic!("Bytes")
        };
        let before = trace_and_queue(&f);
        {
            let mut e = f.engine.lock().unwrap();
            if changed_control {
                e.channels
                    .get_mut(&NetworkChannelId(1))
                    .unwrap()
                    .local_control_generation += 1;
            } else {
                e.release_stream_call_lifetime(f.root.owner(), call.id, call.open_file)
                    .unwrap();
            }
        }
        let mut ran = false;
        let result = f
            .runtime
            .with_shared_foreground_lineage(f.root.owner(), |lineage| {
                let grant = f
                    .scheduler
                    .shared_mm_foreground_observation(f.root.owner(), lineage)?;
                let mut e = f.engine.lock().unwrap();
                f.runtime.with_shared_record_receive_output(
                    &origin,
                    &mut e,
                    |e, proof| {
                        e.reserve_shared_record_receive(selected, &grant, proof, f.now)
                            .map_err(std::io::Error::other)
                    },
                    |_e, _proof, _source, _interval| {
                        ran = true;
                        Ok(())
                    },
                )
            });
        assert!(result.is_err());
        assert!(!ran);
        assert_eq!(trace_and_queue(&f), before);
        wait_bytes(&retained, 8);
        let e = f.engine.lock().unwrap();
        assert!(!e.stream_delivery.contains_key(&f.binding.open_file));
        assert!(e.stream_calls.contains_key(&call.id));
    }
}

#[tokio::test]
async fn shared_record_receive_census_recognizes_only_its_exact_physical_delivery() {
    let (f, call, mut peer, retained) = receive_captured().await;
    peer.write_all(b"abcdefgh").unwrap();
    wait_bytes(&retained, 8);
    let origin = eligible(&f, call.id).await;
    let output = store(&f, &origin, 8, true);
    let before = trace_and_queue(&f);
    {
        let mut e = f.engine.lock().unwrap();
        e.shared_record_receive_peers(&output.source).unwrap();
        assert!(
            e.shared_call_census(Some(call.id)).is_err(),
            "generic census keeps its old physical-debt refusal"
        );
        let duplicate = e.shadow_deliveries[&output.source.lease()].clone();
        let foreign = e.allocate_stream_lease().unwrap();
        assert!(e.shadow_deliveries.insert(foreign, duplicate).is_none());
        assert!(
            e.shared_record_receive_peers(&output.source).is_err(),
            "extra physical row cannot disappear from census"
        );
        e.shadow_deliveries.remove(&foreign).unwrap();
        let original = e.stream_operations[&output.source.lease()].open_file;
        e.stream_operations
            .get_mut(&output.source.lease())
            .unwrap()
            .open_file = OpenFileId::new_socket(f.root.owner().thread, 999);
        assert!(
            e.shared_record_receive_peers(&output.source).is_err(),
            "same lease cannot name a foreign OFD"
        );
        e.stream_operations
            .get_mut(&output.source.lease())
            .unwrap()
            .open_file = original;
        e.shared_record_receive_peers(&output.source).unwrap();
    }
    assert_eq!(trace_and_queue(&f), before);
    wait_bytes(&retained, 8);
    let joined = f
        .runtime
        .controlled_shared_record_drain(output.stored.unwrap(), false)
        .await
        .unwrap();
    let prepared = confirm(&f, &joined).unwrap();
    assert_eq!(publish(&f, &prepared).unwrap(), 8);
}

impl NetworkReplayEngine {
    /// Supplied original Connect/channel/profile premise, never a native receipt.
    /// Must run before Call capture or any Record receive entry.
    pub(crate) fn controlled_shared_record_receive_initial(
        &mut self,
        binding: crate::types::FdSlotBinding,
        now: LogicalTime,
        cursor: i32,
    ) {
        let e = self;

        let definition = NetworkReplayEngine::controlled_replay_two_row_trace().channels[0].clone();
        let channel = e
            .ensure_channel(
                binding.open_file,
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
        let socket = e
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&binding.open_file)
            .unwrap();
        socket.options.peek_offset = Some(cursor);
        socket.options.receive_low_water = 3;
        let key = socket.key;
        e.retain_native_fresh_send(key);
        // Explicit valid original Connect premise BEFORE the Call's entry cut.
        let EngineState::Native(n) = &mut e.mode else {
            unreachable!()
        };
        n.trace.inputs.push(NetworkInputEventV4 {
            ordinal: 0,
            channel,
            release: NetworkReleaseV4 {
                not_before_global_time: now,
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
                    channel,
                    milestone: NetworkProgressV4::Established {
                        source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 },
                    },
                },
                prerequisites: vec![NetworkReleaseNodeIdV4(0)],
            },
        ]);
        e.native_trace_fixture().validate().unwrap();
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Explicit original-installation premise for the Global receive fixture.
    /// This does not issue a provider receipt, pin outcome, worker join or copy.
    pub(crate) fn controlled_shared_record_receive_installation(
        &mut self,
        binding: crate::types::FdSlotBinding,
        identity: crate::network_runtime::original_installation::FileIdentity,
    ) {
        assert!(self.uses_shared_mm_attempts());
        assert_eq!(self.mode(), NetworkEngineMode::Record);
        assert!(
            self.stream_calls.is_empty(),
            "installation precedes any Call"
        );
        assert!(
            self.socket_controls.is_empty(),
            "installation precedes control"
        );
        let socket = self
            .shadow
            .as_mut()
            .unwrap()
            .sockets
            .get_mut(&binding.open_file)
            .unwrap();
        assert!(
            socket.native.is_none(),
            "controlled installation cannot replace an existing native origin"
        );
        socket.native = Some(crate::network_replay::native_receive::NativeReceive {
            identity,
            binding,
            birth: NetworkStreamCallId::controlled_fixture(9999),
            physical_observed: crate::network_replay::native_receive::Cut::ZERO,
        });
    }
}

#[cfg(test)]
impl NetworkReplayEngine {
    /// Observe the same semantic consumed counter as the Replay control, without
    /// requesting Replay-only completion nodes from a Record engine.
    pub(crate) fn controlled_shared_record_receive_consumed(&self, file: OpenFileId) -> u64 {
        assert!(self.uses_shared_mm_attempts());
        assert_eq!(self.mode(), NetworkEngineMode::Record);
        let channel = self.bound_channel(file).unwrap();
        self.channels[&channel].inbound_consumed
    }
}
