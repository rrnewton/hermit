//! These component premises use a real retained initial census, Normal grant,
//! FD publication/ACK and runtime worker owner. They do not certify peer stops,
//! original guest memory, native receive completion or shared Record output.
use std::sync::Mutex;

use reverie::syscalls::Poll;
use reverie::syscalls::Read;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::NetworkRuntimeResources;
use crate::scheduler::Scheduler;

struct Fixture {
    runtime: NetworkRuntimeResources,
    root: Arc<ForegroundRoot>,
    _metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    _memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    engine: Arc<Mutex<NetworkReplayEngine>>,
    parent: Option<Arc<ForegroundRoot>>,
    _birth: Option<Box<dyn std::any::Any>>,
    scheduler: Scheduler,
    binding: crate::types::FdSlotBinding,
    now: LogicalTime,
    deadline: LogicalTime,
}
async fn fixture() -> Fixture {
    fixture_with_child(false).await
}
async fn fixture_with_child(with_child: bool) -> Fixture {
    fixture_with_inputs(with_child, false).await
}
async fn fixture_with_inputs(with_child: bool, ready_poll: bool) -> Fixture {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let now = trace.epoch_global_time().unwrap();
    // Real validated immutable trace: bytes are unavailable at this first
    // observation, and become eligible later; no queue is edited by the test.
    for input in &mut trace.inputs {
        if !matches!(input.event, NetworkInputKindV2::Connect(_)) {
            input.release.not_before_global_time = LogicalTime::from_nanos(now.as_nanos() + 37);
        }
    }
    trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
        nodes: trace.release_model.nodes().to_vec(),
    };
    if with_child {
        trace.outputs.push(NetworkOutputEventV2 {
            channel: NetworkChannelId(1),
            event: NetworkOutputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: b"abc".to_vec(),
            },
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut trace.release_model
        else {
            unreachable!()
        };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(nodes.len() as u64),
            kind: NetworkReleaseNodeKindV4::Progress {
                channel: NetworkChannelId(1),
                milestone: NetworkProgressV4::StreamPrefix {
                    exclusive_offset: 3,
                },
            },
            prerequisites: vec![NetworkReleaseNodeIdV4(1)],
        });
    }
    if ready_poll {
        let ordinal = trace.inputs.len() as u64;
        let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
        let prerequisites = trace.entry_frontier(cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: NetworkChannelId(1),
            release: NetworkReleaseV4 {
                not_before_global_time: LogicalTime::from_nanos(now.as_nanos() + 37),
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents: libc::POLLIN,
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
    }
    trace.validate().unwrap();
    let key = trace.fresh_stream_profiles[0].key;
    let mut engine = NetworkReplayEngine::replay_shared_mm_attempts(trace).unwrap();
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let mut scheduler = Scheduler::new(&crate::config::Config::default());
    engine.fd_table_fixture_enable();
    let (runtime, root, metadata, memory, parent, retained_birth) = if with_child {
        let birth =
            ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, claim| {
                scheduler.controlled_foreground_store_grant(root);
                engine
                    .register_initial_census(root.association(), claim, root.owner().thread)
                    .unwrap();
                let metadata = root.metadata().unwrap();
                engine
                    .associate_fd_metadata(root.owner(), &metadata, &metadata.lock().unwrap())
                    .unwrap();
            })
            .await;
        scheduler.controlled_shared_birth_census(&birth.parent, &birth.child, &birth._birth);
        let flags = birth._birth.flags();
        let NetworkFdMutationBegin::Admitted(admission) = engine
            .begin_fd_mutation(
                birth.parent.owner(),
                birth.parent.files(),
                NetworkFdMutationKind::Clone { flags },
            )
            .unwrap()
        else {
            panic!("controlled actual clone preparation");
        };
        let permit = admission.publication.permit;
        engine
            .submit_fd_mutation(birth.parent.owner(), permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(
                birth.parent.owner(),
                permit,
                Ok(i64::from(birth.child.owner().thread.as_raw())),
            )
            .unwrap();
        engine
            .register_cloned_fd_table(
                birth.parent.owner(),
                birth.child.owner(),
                birth.parent.logical_process(),
                flags,
            )
            .unwrap();
        let root = birth.child.clone();
        let parent = birth.parent.clone();
        let metadata = birth.metadata.clone();
        let memory = birth.memory.clone();
        let (runtime, retained) = birth.into_runtime_and_retention();
        scheduler.controlled_shared_child_grant(&root);
        (
            runtime,
            root,
            metadata,
            memory,
            Some(parent),
            Some(retained),
        )
    } else {
        let (runtime, root, metadata, memory, claim) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        scheduler.controlled_foreground_store_grant(&root);
        engine
            .register_initial_census(root.association(), &claim, root.owner().thread)
            .unwrap();
        (runtime, root, metadata, memory, None, None)
    };
    let owner = root.owner();
    engine
        .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
        .unwrap();
    let (mut candidate, replacement) = metadata
        .lock()
        .unwrap()
        .prepare_original_installation(owner.thread, 5, nix::fcntl::OFlag::O_RDWR, None)
        .unwrap();
    let binding = replacement.after.unwrap().binding;
    let publication = engine.acquire_fd_publication(owner, root.files()).unwrap();
    let effect = engine.fd_publication_fixture_effect(owner, replacement);
    candidate
        .associate_network_installation(replacement.installation_generation, effect)
        .unwrap();
    let batch = candidate.publication_snapshot(&publication).unwrap();
    *metadata.lock().unwrap() = candidate;
    engine
        .publish_fd_publication(owner, publication.permit, &batch)
        .unwrap();
    metadata
        .lock()
        .unwrap()
        .publication_acknowledge(&batch)
        .unwrap();
    engine
        .acknowledge_fd_publication(owner, publication.permit, &batch)
        .unwrap();
    metadata
        .lock()
        .unwrap()
        .publication_server_acknowledge(&batch)
        .unwrap();
    engine
        .register_stream_socket(
            binding.open_file,
            key,
            NetworkStreamNamespace {
                device: 1,
                inode: 1,
            },
            None,
        )
        .unwrap();
    engine.bind(binding.open_file, NetworkChannelId(1)).unwrap();
    engine.release_eligible(now).unwrap();
    assert!(
        engine
            .take_connection_outcome(binding.open_file)
            .unwrap()
            .is_some()
    );
    Fixture {
        runtime,
        root,
        _metadata: metadata,
        _memory: memory,
        engine: Arc::new(Mutex::new(engine)),
        parent,
        _birth: retained_birth,
        scheduler,
        binding,
        now,
        deadline: LogicalTime::from_nanos(now.as_nanos() + 5_000_000_000),
    }
}
impl Fixture {
    async fn begin(&self, poll: bool) -> NetworkStreamCall {
        let owner = self.root.owner();
        let read = {
            let mut engine = self.engine.lock().unwrap();
            let NetworkFdReadBegin::Admitted(read) = engine
                .begin_fd_read(owner, self.root.files(), self.binding.slot.fd)
                .unwrap()
            else {
                panic!("original published FD");
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
                        engine
                            .finish_socket_control(
                                owner,
                                read.control.unwrap(),
                                NetworkSocketControlFinish::Unchanged,
                            )
                            .unwrap();
                        let intent = if poll {
                            let raw = Poll::new()
                                .with_fds(reverie::syscalls::AddrMut::from_raw(0x1000))
                                .with_nfds(2)
                                .with_timeout(5000)
                                .into_parts();
                            SharedWaitIntent::Poll(Arc::new(
                                OriginalPollIntent::new(
                                    raw,
                                    vec![
                                        (self.binding, libc::POLLIN),
                                        (self.binding, libc::POLLOUT),
                                    ],
                                    self.now,
                                    Some(self.deadline),
                                )
                                .unwrap(),
                            ))
                        } else {
                            let raw = Read::new()
                                .with_fd(self.binding.slot.fd)
                                .with_buf(reverie::syscalls::AddrMut::from_ptr(
                                    0x2000usize as *mut u8,
                                ))
                                .with_len(8)
                                .into_parts();
                            SharedWaitIntent::Receive(
                                crate::tool_global::SavedReceivePolicy::controlled_shared(
                                    (owner, call.id, self.binding.open_file),
                                    self.root.clone(),
                                    raw,
                                    (self.now, Some(self.deadline)),
                                    (false, 3),
                                ),
                            )
                        };
                        engine
                            .attach_shared_wait_call(
                                call.id,
                                self.binding,
                                intent,
                                &grant,
                                admission,
                                self.now,
                            )
                            .unwrap();
                        assert!(
                            engine.validate_fd_read(owner, &read).is_err(),
                            "custody actually transferred"
                        );
                        Ok(call)
                    },
                )
            })
            .unwrap()
    }
    async fn suspend(&self, call: NetworkStreamCallId) {
        let owner = self.root.owner();
        let completed = self
            .runtime
            .with_shared_foreground_lineage(owner, |lineage| {
                let grant = self
                    .scheduler
                    .shared_mm_foreground_observation(owner, lineage)?;
                Ok(self
                    .engine
                    .lock()
                    .unwrap()
                    .complete_shared_replay_wait_observation(call, &grant, self.now)
                    .unwrap())
            })
            .unwrap();
        let prefix = self
            .runtime
            .join_shared_foreground_prefix(self.root.clone(), &self.engine, Some(call))
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
                            .suspend_shared_wait(completed, &grant, admission)
                            .map_err(std::io::Error::other)
                    },
                )
            })
            .unwrap();
    }
}

#[tokio::test]
async fn shared_wait_retains_original_call_target_deadline_and_requires_fresh_grant() {
    let mut f = fixture().await;
    let owner = f.root.owner();
    let call = f.begin(false).await;
    let before = f.engine.lock().unwrap().channels[&NetworkChannelId(1)].inbound_consumed;
    assert!(
        f.engine
            .lock()
            .unwrap()
            .call_wait_binding(
                owner,
                call.id,
                NetworkWaitKind::ReadableAtLeast(3),
                Some(f.deadline)
            )
            .is_err()
    );
    f.suspend(call.id).await;
    {
        let mut engine = f.engine.lock().unwrap();
        assert_eq!(
            engine.channels[&NetworkChannelId(1)].inbound_consumed,
            before
        );
        assert!(engine.begin_stream_call_release(owner, call.id).is_err());
        assert!(engine.stream_call_open_file(owner, call.id).is_err());
        assert!(
            engine.finish().is_err(),
            "suspension does not complete a Call"
        );
        for (kind, deadline) in [
            (NetworkWaitKind::ReadableAtLeast(1), Some(f.deadline)),
            (NetworkWaitKind::Any, Some(f.deadline)),
            (NetworkWaitKind::ReadableAtLeast(3), None),
            (
                NetworkWaitKind::ReadableAtLeast(3),
                Some(LogicalTime::from_nanos(f.deadline.as_nanos() + 1)),
            ),
        ] {
            assert!(
                engine
                    .call_wait_binding(owner, call.id, kind, deadline)
                    .is_err()
            );
        }
        assert_eq!(
            engine
                .call_wait_binding(
                    owner,
                    call.id,
                    NetworkWaitKind::ReadableAtLeast(3),
                    Some(f.deadline)
                )
                .unwrap()
                .open_file,
            call.open_file
        );
    }
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
        .await
        .unwrap();
    f.runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(owner, lineage)?;
            let mut engine = f.engine.lock().unwrap();
            f.runtime
                .with_shared_attempt_prefix(&prefix, &mut engine, |engine, admission| {
                    assert!(
                        engine
                            .resume_shared_wait(call.id, &grant, admission, f.now)
                            .is_err(),
                        "old Normal epoch cannot restart"
                    );
                    Ok(())
                })
        })
        .unwrap();
    f.scheduler.controlled_shared_foreground_grant(&f.root);
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(f.root.clone(), &f.engine, Some(call.id))
        .await
        .unwrap();
    f.runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(owner, lineage)?;
            let mut engine = f.engine.lock().unwrap();
            f.runtime
                .with_shared_attempt_prefix(&prefix, &mut engine, |engine, admission| {
                    assert_eq!(
                        engine
                            .resume_shared_wait(call.id, &grant, admission, f.now)
                            .unwrap(),
                        1
                    );
                    assert!(
                        engine
                            .resume_shared_wait(call.id, &grant, admission, f.now)
                            .is_err()
                    );
                    assert_eq!(
                        engine.stream_calls[&call.id].open_file,
                        Some(f.binding.open_file)
                    );
                    engine
                        .release_eligible(LogicalTime::from_nanos(f.now.as_nanos() + 37))
                        .unwrap();
                    assert!(
                        engine
                            .complete_shared_replay_wait_observation(
                                call.id,
                                &grant,
                                LogicalTime::from_nanos(f.now.as_nanos() + 37)
                            )
                            .is_err(),
                        "ready saved target must not be reported as pending"
                    );
                    assert_eq!(
                        engine.channels[&NetworkChannelId(1)].inbound_consumed,
                        before
                    );
                    Ok(())
                })
        })
        .unwrap();
}

#[tokio::test]
async fn shared_poll_wait_requires_all_original_interests_and_never_invents_zero() {
    let f = fixture().await;
    let owner = f.root.owner();
    let call = f.begin(true).await;
    f.suspend(call.id).await;
    let engine = f.engine.lock().unwrap();
    let interests = vec![
        (call.id, NetworkWaitKind::PollReadable),
        (call.id, NetworkWaitKind::Writable),
    ];
    engine
        .validate_call_wait_set(owner, &interests, Some(f.deadline))
        .unwrap();
    assert!(
        engine
            .validate_call_wait_set(owner, &interests[..1], Some(f.deadline))
            .is_err()
    );
    assert!(
        engine
            .validate_call_wait_set(owner, &[], Some(f.deadline))
            .is_err()
    );
    assert!(
        engine
            .validate_call_wait_set(owner, &[(call.id, NetworkWaitKind::Any)], Some(f.deadline))
            .is_err()
    );
    assert_eq!(
        engine
            .call_wait_binding(
                owner,
                call.id,
                NetworkWaitKind::PollReadable,
                Some(f.deadline)
            )
            .unwrap()
            .observed_ready,
        Some(false),
        "absence is Pending, not a published zero"
    );
    assert!(
        engine
            .native_trace_fixture()
            .inputs
            .iter()
            .all(|i| !matches!(i.event, NetworkInputKindV2::RawTcpPollState { .. }))
    );
}

#[tokio::test]
async fn shared_wait_runtime_refuses_unknown_native_call() {
    let f = fixture().await;
    let call = f.begin(false).await;
    f.suspend(call.id).await;
    // Existing failed-capture fixture installs an actual row in the native
    // ledger. It cannot be made inert by the engine's Suspended annotation.
    f.runtime
        .controlled_shared_unknown_capture(f.root.owner(), call.id);
    assert!(
        f.runtime
            .join_shared_foreground_prefix(f.root.clone(), &f.engine, None)
            .await
            .is_err()
    );
    assert!(f.engine.lock().unwrap().finish().is_err());
}

#[tokio::test]
async fn shared_wait_child_suspension_allows_exact_parent_transmit_and_preserves_wait() {
    let mut f = fixture_with_child(true).await;
    let call = f.begin(false).await;
    let parent = f.parent.clone().unwrap();
    let child = f.root.owner();
    assert!(!parent.is_sole_initial_root(parent.owner()));
    assert!(!f.root.is_sole_initial_root(child));
    assert!(
        f.runtime
            .join_shared_foreground_prefix(parent.clone(), &f.engine, None)
            .await
            .is_err(),
        "an active child is not an idle peer"
    );
    f.suspend(call.id).await;
    let interests = f
        .engine
        .lock()
        .unwrap()
        .shared_wait_interests(child, call.id)
        .unwrap()
        .into_iter()
        .map(|kind| (call.id, kind))
        .collect();
    f.scheduler
        .controlled_park_shared_wait(child, interests, Some(f.deadline), f.engine.clone());
    f.scheduler.controlled_shared_foreground_grant(&parent);
    let read = {
        let mut engine = f.engine.lock().unwrap();
        let NetworkFdReadBegin::Admitted(read) = engine
            .begin_fd_read(parent.owner(), parent.files(), f.binding.slot.fd)
            .unwrap()
        else {
            panic!("same shared original FD");
        };
        *read
    };
    let prefix = f
        .runtime
        .join_shared_foreground_prefix(parent.clone(), &f.engine, None)
        .await
        .unwrap();
    let before = f.engine.lock().unwrap().channels[&NetworkChannelId(1)].inbound_consumed;
    let (transmit, interval) = f
        .runtime
        .with_shared_foreground_lineage(parent.owner(), |lineage| {
            assert_eq!(lineage.members().count(), 2);
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(parent.owner(), lineage)?;
            let mut engine = f.engine.lock().unwrap();
            f.runtime.prepare_shared_replay_source(
                &prefix,
                lineage,
                &mut engine,
                |engine, admission| {
                    engine
                        .begin_shared_replay_transmit(read.clone(), &grant, &prefix, admission, 3)
                        .map_err(std::io::Error::other)
                },
            )
        })
        .unwrap();
    assert!(
        f.runtime
            .with_source_interval(&interval, || Ok(()))
            .is_err(),
        "legacy issuer cannot consume shared interval"
    );
    assert!(
        f.runtime
            .join_shared_foreground_prefix(parent.clone(), &f.engine, None)
            .await
            .is_err(),
        "active source interval and original TX prevent another attempt"
    );
    f.runtime
        .with_shared_foreground_lineage(parent.owner(), |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(parent.owner(), lineage)?;
            let mut engine = f.engine.lock().unwrap();
            f.runtime
                .with_shared_source_interval(&interval, &mut engine, transmit.id, |engine| {
                    assert!(
                        engine
                            .complete_shared_replay_transmit(transmit.id, &grant, &prefix, b"bad")
                            .is_err()
                    );
                    assert_eq!(
                        engine.replay_transmit_offset(f.binding.open_file).unwrap(),
                        0
                    );
                    assert_eq!(
                        engine
                            .complete_shared_replay_transmit(transmit.id, &grant, &prefix, b"abc")
                            .unwrap(),
                        StreamTransmitOutcome::Accepted(3)
                    );
                    assert!(engine.stream_calls.contains_key(&call.id));
                    assert_eq!(
                        engine.channels[&NetworkChannelId(1)].inbound_consumed,
                        before
                    );
                    assert_eq!(
                        engine
                            .call_wait_binding(
                                child,
                                call.id,
                                NetworkWaitKind::ReadableAtLeast(3),
                                Some(f.deadline)
                            )
                            .unwrap()
                            .open_file,
                        f.binding.open_file
                    );
                    assert!(
                        engine.finish().is_err(),
                        "parent result does not retire child wait"
                    );
                    Ok(())
                })
        })
        .unwrap();
    let keepalive = interval.keepalive();
    drop(interval);
    assert!(
        f.runtime
            .join_shared_foreground_prefix(parent.clone(), &f.engine, None)
            .await
            .is_err(),
        "actual source worker keepalive cannot be dropped by callback completion"
    );
    drop(keepalive);
    f.runtime
        .join_shared_foreground_prefix(parent, &f.engine, None)
        .await
        .unwrap();
}

async fn due_input_is_not_pending(poll: bool) {
    let f = fixture_with_inputs(false, poll).await;
    let call = f.begin(poll).await;
    let owner = f.root.owner();
    let due = LogicalTime::from_nanos(f.now.as_nanos() + 37);
    f.runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = f
                .scheduler
                .shared_mm_foreground_observation(owner, lineage)?;
            let mut engine = f.engine.lock().unwrap();
            assert_eq!(
                engine
                    .stream_queue_status(call.open_file)
                    .unwrap()
                    .queued_bytes,
                0
            );
            assert_eq!(engine.shared_poll_sample(call.open_file).unwrap(), None);
            let trace = engine.native_trace_fixture();
            let before = engine.channels[&NetworkChannelId(1)].inbound_consumed;
            // Deliberately no release_eligible(due) call in this fixture. The
            // production pending issuer must establish the current frontier.
            assert!(
                engine
                    .complete_shared_replay_wait_observation(call.id, &grant, due)
                    .is_err()
            );
            assert_eq!(
                engine.channels[&NetworkChannelId(1)].inbound_consumed,
                before
            );
            assert_eq!(engine.native_trace_fixture(), trace);
            assert!(
                engine
                    .stream_queue_status(call.open_file)
                    .unwrap()
                    .queued_bytes
                    >= 3
            );
            if poll {
                assert_eq!(
                    engine.shared_poll_sample(call.open_file).unwrap(),
                    Some((due, libc::POLLIN))
                );
            }
            let (_, wait) = engine.shared_wait(owner, call.id).unwrap();
            assert!(matches!(
                wait.phase,
                AttemptPhase::Active {
                    completion: None,
                    ..
                }
            ));
            assert!(engine.shared_wait_interests(owner, call.id).is_err());
            assert!(engine.begin_stream_call_release(owner, call.id).is_err());
            assert!(engine.finish().is_err());
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn shared_wait_due_receive_target_cannot_be_stamped_pending() {
    due_input_is_not_pending(false).await;
}

#[tokio::test]
async fn shared_wait_due_ready_poll_cannot_be_stamped_pending() {
    due_input_is_not_pending(true).await;
}
