//! Controlled initial provider/Connect facts. Actual FD publication, selected
//! grant, reader transfer and runtime prefix borrowing; no native TX claim.
use std::sync::Mutex;

use super::*;

pub(crate) struct Fixture {
    pub engine: Mutex<NetworkReplayEngine>,
    pub runtime: crate::network_runtime::NetworkRuntimeResources,
    pub root: Arc<crate::network_runtime::ForegroundRoot>,
    pub metadata: Arc<Mutex<crate::tool_local::FileMetadata>>,
    pub memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    pub scheduler: crate::scheduler::Scheduler,
    pub read: NetworkFdReadAdmission,
    pub arguments: Arguments,
    pub raw: [usize; 6],
    pub now: LogicalTime,
}
pub(crate) fn fixture() -> Fixture {
    let trace = crate::network_replay::replay_connect::fixture(LogicalTime::ZERO, false)
        .engine
        .native_trace_fixture();
    let now = trace.epoch_global_time().unwrap();
    let (runtime, root, metadata, memory, claim) =
        crate::network_runtime::controlled_foreground_runtime(unsafe {
            libc::syscall(libc::SYS_gettid)
        } as i32);
    let owner = root.owner();
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let projection = scheduler.shared_initial_projection(&root).unwrap();
    let mut engine = NetworkReplayEngine::record_shared_mm_attempts(trace.epoch);
    engine.fd_table_fixture_enable();
    engine
        .register_initial_census(root.association(), &claim, owner.thread)
        .unwrap();
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
    let profile = trace.fresh_stream_profiles[0].clone();
    engine
        .register_stream_socket_profile(
            binding.open_file,
            profile.key,
            NetworkStreamNamespace {
                device: 1,
                inode: 1,
            },
            Some(profile),
        )
        .unwrap();
    let definition = &trace.channels[0];
    engine
        .ensure_channel(
            binding.open_file,
            NetworkChannelBinding {
                transport: definition.transport,
                role: definition.role,
                peer_address: definition.peer_address.clone(),
                requested_local_constraint: None,
                observed_local_address: definition.local_address.clone(),
                accepted_from: definition.accepted_from,
                selected_channel: None,
            },
        )
        .unwrap();
    engine.retain_native_fresh_send(trace.fresh_stream_profiles[0].key);
    // Explicit controlled earlier actual setter and completed Connect. This
    // fixture does not issue provider/worker success or an original Sendto.
    engine
        .shadow
        .as_mut()
        .unwrap()
        .sockets
        .get_mut(&binding.open_file)
        .unwrap()
        .send_timeout = Some(ReceiveTimeoutV3::FiniteTicks(5000));
    let EngineState::Native(native) = &mut engine.mode else {
        unreachable!()
    };
    native.trace.inputs = trace.inputs;
    native.trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
        nodes: trace.release_model.nodes().to_vec(),
    };
    engine.controlled_shared_initial_origin(root.clone(), projection);
    engine.native_trace_fixture().validate().unwrap();
    let NetworkFdReadBegin::Admitted(read) = engine.begin_fd_read(owner, root.files(), 5).unwrap()
    else {
        panic!("published exact slot")
    };
    let raw = [5, 0x1000, 8, libc::MSG_NOSIGNAL as usize, 0, 0];
    let arguments = Arguments {
        kind: Kind::BlockingSendto {
            timeout_ticks: 5000,
        },
        operation: crate::resources::ExternalOpId::new(owner.thread, 10),
        files: root.files(),
        binding: read.binding,
        fd: 5,
        address: 0x1000,
        length: libc::MSG_NOSIGNAL,
        original_count: 8,
    };
    Fixture {
        engine: Mutex::new(engine),
        runtime,
        root,
        metadata,
        memory,
        scheduler,
        read: *read,
        arguments,
        raw,
        now,
    }
}

#[tokio::test]
async fn shared_send_uses_selected_reader_and_refuses_changed_tuple_before_transfer() {
    let f = fixture();
    let owner = f.root.owner();
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
            let before = format!("{e:?}");
            assert!(
                e.begin_original_connect(owner, f.arguments.clone())
                    .is_err(),
                "generic Begin cannot mint op25"
            );
            for index in 0..6 {
                let mut raw = f.raw;
                raw[index] ^= 1;
                assert!(
                    f.runtime
                        .with_shared_attempt_prefix(&prefix, &mut e, |e, a| e
                            .begin_shared_record_send(
                                SharedRecordSendEntry {
                                    read: f.read.clone(),
                                    arguments: f.arguments.clone(),
                                    raw,
                                },
                                &grant,
                                &prefix,
                                a,
                                f.now
                            )
                            .map_err(std::io::Error::other))
                        .is_err()
                );
                e.validate_fd_read_grant(owner, &f.read).unwrap();
                assert_eq!(format!("{e:?}"), before);
            }
            let origin = f
                .runtime
                .with_shared_attempt_prefix(&prefix, &mut e, |e, a| {
                    e.begin_shared_record_send(
                        SharedRecordSendEntry {
                            read: f.read.clone(),
                            arguments: f.arguments.clone(),
                            raw: f.raw,
                        },
                        &grant,
                        &prefix,
                        a,
                        f.now,
                    )
                    .map_err(std::io::Error::other)
                })?;
            assert!(e.validate_fd_read_grant(owner, &f.read).is_err());
            assert!(e.finish_fd_read(owner, f.read.clone()).is_err());
            e.validate_shared_record_send(&origin, &grant, f.raw, f.now)
                .unwrap();
            assert!(
                e.original_shared_native_sent(owner, origin.admission())
                    .is_err()
            );
            assert!(
                e.finish_original_connect(owner, origin.admission())
                    .is_err()
            );
            assert!(
                f.runtime.check_shared_send_prepared(&origin).is_err(),
                "logical entry cannot invent preparation join"
            );
            assert!(e.finish().is_err());
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn shared_send_current_timeout_remains_original_after_transfer() {
    let f = fixture();
    let owner = f.root.owner();
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
            let origin = f
                .runtime
                .with_shared_attempt_prefix(&prefix, &mut e, |e, a| {
                    e.begin_shared_record_send(
                        SharedRecordSendEntry {
                            read: f.read.clone(),
                            arguments: f.arguments.clone(),
                            raw: f.raw,
                        },
                        &grant,
                        &prefix,
                        a,
                        f.now,
                    )
                    .map_err(std::io::Error::other)
                })?;
            let trace = e.native_trace_fixture();
            let binding = f.read.binding.unwrap();
            e.shadow
                .as_mut()
                .unwrap()
                .sockets
                .get_mut(&binding.open_file)
                .unwrap()
                .send_timeout = Some(ReceiveTimeoutV3::FiniteTicks(4999));
            assert!(
                e.validate_shared_record_send(&origin, &grant, f.raw, f.now)
                    .is_err()
            );
            assert_eq!(e.native_trace_fixture(), trace);
            assert!(e.stream_calls.contains_key(&origin.admission.call));
            assert!(
                f.runtime
                    .with_shared_send_completion(&origin, &mut e, |_, _| Ok(()))
                    .is_err(),
                "no synthetic joined result"
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn shared_send_closed_kind_preserves_v1_and_rejects_unbounded_or_changed_flags() {
    assert_eq!(Kind::Sendto.provider_operation(), 24);
    assert!(Kind::Sendto.valid_operands(0x1000, 0x4040, 8));
    for ticks in [0, i64::MAX as u64, u64::MAX] {
        assert!(
            !Kind::BlockingSendto {
                timeout_ticks: ticks
            }
            .valid_operands(0x1000, libc::MSG_NOSIGNAL, 8)
        );
    }
    let kind = Kind::BlockingSendto {
        timeout_ticks: 5000,
    };
    assert_eq!(kind.provider_operation(), 25);
    assert!(kind.valid_operands(0x1000, libc::MSG_NOSIGNAL, 8));
    for (address, flags, count) in [
        (0, libc::MSG_NOSIGNAL, 8),
        (0x1000, 0x4040, 8),
        (0x1000, libc::MSG_NOSIGNAL, 0),
        (0x1000, libc::MSG_NOSIGNAL, 513),
    ] {
        assert!(!kind.valid_operands(address, flags, count));
    }
}
