use super::*;

#[test]
fn shared_constructor_never_upgrades_a_legacy_trace_or_issuer() {
    let epoch = crate::config::Config::default().epoch;
    let legacy = NetworkReplayEngine::record_native_receive(epoch);
    assert!(!legacy.uses_shared_mm_attempts());
    legacy.require_sole_initial_release_policy().unwrap();
    let legacy = legacy.into_native_recorded_trace().unwrap();
    let shared = NetworkReplayEngine::record_shared_mm_attempts(epoch);
    assert!(shared.uses_shared_mm_attempts());
    assert!(shared.require_sole_initial_release_policy().is_err());
    let shared = shared.into_native_recorded_trace().unwrap();
    assert!(NetworkReplayEngine::replay_native_receive(shared.clone()).is_err());
    assert!(NetworkReplayEngine::replay_shared_mm_attempts(legacy.clone()).is_err());
    assert!(
        !NetworkReplayEngine::replay_native_receive(legacy)
            .unwrap()
            .uses_shared_mm_attempts()
    );
    assert!(
        NetworkReplayEngine::replay_shared_mm_attempts(shared)
            .unwrap()
            .uses_shared_mm_attempts()
    );
}

#[tokio::test]
async fn shared_source_claim_transfers_reader_and_excludes_generic_consumers_until_exact_finish() {
    // Provider/census and immutable bytes are controlled premises. The test
    // executes real FD publication/ACK, current Normal selection, runtime
    // interval reservation and existing Call lifetime; it does not read guest
    // memory or certify a backend worker/physical peer stop.
    let mut trace = crate::network_replay::replay_connect::fixture(LogicalTime::ZERO, false)
        .engine
        .native_trace_fixture();
    let mut nodes = trace.release_model.nodes().to_vec();
    trace.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes: b"abc".to_vec(),
        },
    });
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
    trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes };
    trace.validate().unwrap();
    let key = trace.fresh_stream_profiles[0].key;
    let mut engine = NetworkReplayEngine::replay_shared_mm_attempts(trace).unwrap();
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (runtime, root, metadata, _memory, claim) =
        crate::network_runtime::controlled_foreground_runtime(tid);
    let owner = root.owner();
    let mut scheduler = crate::scheduler::Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
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
    engine.release_eligible(LogicalTime::ZERO).unwrap();
    assert_eq!(
        engine.take_connection_outcome(binding.open_file).unwrap(),
        Some(ConnectionOutcome::Connect(
            NetworkConnectionResultV2::Connected
        ))
    );
    let NetworkFdReadBegin::Admitted(read) = engine
        .begin_fd_read(owner, root.files(), binding.slot.fd)
        .unwrap()
    else {
        panic!("acknowledged descriptor")
    };
    let read = *read;
    let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
    let (call, interval) = runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
            // Rejected preparation consumes neither the read nor an interval.
            assert!(
                runtime
                    .prepare_shared_replay_source(&prefix, lineage, |admission| {
                        engine
                            .begin_shared_replay_transmit(
                                read.clone(),
                                &grant,
                                &prefix,
                                admission,
                                4,
                            )
                            .map_err(|e| std::io::Error::other(e.to_string()))
                    })
                    .is_err()
            );
            engine.validate_fd_read(owner, &read).unwrap();
            runtime.prepare_shared_replay_source(&prefix, lineage, |admission| {
                engine
                    .begin_shared_replay_transmit(read.clone(), &grant, &prefix, admission, 3)
                    .map_err(|e| std::io::Error::other(e.to_string()))
            })
        })
        .unwrap();
    assert!(engine.validate_fd_read(owner, &read).is_err());
    assert!(engine.transmit_stream(binding.open_file, b"abc").is_err());
    assert!(engine.begin_stream_call_release(owner, call.id).is_err());
    assert!(engine.check_stream_operations_finished().is_err());
    runtime
        .with_shared_foreground_lineage(owner, |lineage| {
            let grant = scheduler.shared_mm_foreground_observation(owner, lineage)?;
            runtime.with_source_interval(&interval, || {
                assert!(
                    engine
                        .complete_shared_replay_transmit(call.id, &grant, &prefix, b"bad")
                        .is_err()
                );
                assert_eq!(engine.replay_transmit_offset(binding.open_file).unwrap(), 0);
                assert!(engine.stream_calls.contains_key(&call.id));
                assert_eq!(
                    engine
                        .complete_shared_replay_transmit(call.id, &grant, &prefix, b"abc")
                        .unwrap(),
                    StreamTransmitOutcome::Accepted(3)
                );
                assert!(
                    engine
                        .complete_shared_replay_transmit(call.id, &grant, &prefix, b"abc")
                        .is_err()
                );
                Ok(())
            })
        })
        .unwrap();
    assert!(!engine.stream_calls.contains_key(&call.id));
    assert_eq!(engine.replay_transmit_offset(binding.open_file).unwrap(), 3);
}
