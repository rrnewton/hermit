//! Canonical issuer/parser/worker components. Native Unix bytes are checked by
//! the helper fixture; all provider/TCP geometry remains controlled metadata.
use super::*;
use crate::network_runtime::HelperCopyBinding;
use crate::network_runtime::NetworkRuntimeResources;

async fn fixture(
    version: u64,
    joined: bool,
    cursor: i32,
) -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    NetworkStreamCallId,
    NetworkStreamLeaseId,
    NetworkRuntimeResources,
    Observation,
) {
    fixture_bytes(
        version,
        joined,
        cursor,
        &(0..128).map(|i| i as u8).collect::<Vec<_>>(),
        false,
    )
    .await
}
async fn fixture_bytes(
    version: u64,
    joined: bool,
    cursor: i32,
    bytes: &[u8],
    eof: bool,
) -> (
    NetworkReplayEngine,
    NetworkStreamOwner,
    NetworkStreamCallId,
    NetworkStreamLeaseId,
    NetworkRuntimeResources,
    Observation,
) {
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    engine
        .ensure_channel(
            binding.open_file,
            NetworkChannelBinding {
                transport: NetworkTransportV2::Tcp,
                role: NetworkEndpointRoleV2::OutboundClient,
                peer_address: Some(NetworkAddressV2::Inet4 {
                    address: [192, 0, 2, 1],
                    port: 443,
                }),
                requested_local_constraint: None,
                observed_local_address: None,
                accepted_from: None,
                selected_channel: None,
            },
        )
        .unwrap();
    engine
        .shadow
        .as_mut()
        .unwrap()
        .sockets
        .get_mut(&binding.open_file)
        .unwrap()
        .options
        .peek_offset = Some(cursor);
    let control = engine
        .begin_socket_controls(owner, vec![binding.open_file])
        .unwrap()[0]
        .1;
    let call = engine.begin_stream_call(owner, control).unwrap().id;
    engine
        .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
        .unwrap();
    engine
        .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
        .unwrap();
    let lease = engine
        .begin_shadow_probe(
            owner,
            call,
            LogicalTime::from_nanos(1_790_000_000_000_000_001),
        )
        .unwrap()
        .lease;
    engine
        .submit_stream_physical(owner, lease, NetworkStreamPhysicalEffect::ReadPeekOffset)
        .unwrap();
    engine
        .confirm_stream_physical(
            owner,
            lease,
            NetworkStreamPhysicalResult::PeekOffset(cursor),
        )
        .unwrap();
    if cursor >= 0 {
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: -1 },
            )
            .unwrap();
        engine
            .confirm_stream_physical(owner, lease, NetworkStreamPhysicalResult::Unit)
            .unwrap();
    }
    engine
        .submit_stream_physical(
            owner,
            lease,
            NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
        )
        .unwrap();
    let (runtime, observed, _receipt) = HelperCopyBinding::controlled_joined_peek(
        owner,
        call,
        lease,
        &mut engine,
        bytes,
        version,
        joined,
        eof,
    )
    .await;
    (engine, owner, call, lease, runtime, observed)
}
fn control_observation() -> Observation {
    Observation {
        raw_return: 0,
        errno: None,
        bytes: Vec::new(),
        confirmation: NetworkStreamPhysicalResult::Unit,
        helper_copy: None,
    }
}
fn semantic_state(engine: &NetworkReplayEngine) -> String {
    format!("{:?}/{:?}", engine.channels, engine.mode)
}
fn forbidden_finish(
    engine: &mut NetworkReplayEngine,
    owner: NetworkStreamOwner,
    call: NetworkStreamCallId,
    lease: NetworkStreamLeaseId,
) {
    let before = format!("{engine:?}");
    assert!(engine.begin_record_drain(owner, lease).is_err());
    assert!(engine.finish_record_drain(owner, lease).is_err());
    for disposition in [
        NetworkStreamChunkDisposition::Consumed,
        NetworkStreamChunkDisposition::Peeked,
        NetworkStreamChunkDisposition::CopyFailed,
    ] {
        assert!(
            engine
                .finish_stream_chunk(owner, lease, disposition)
                .is_err()
        );
    }
    assert!(engine.begin_stream_call_release(owner, call).is_err());
    assert!(engine.finish_stream_call_release(owner, call).is_err());
    assert_eq!(format!("{engine:?}"), before);
}

#[tokio::test]
async fn private_peek_opaque_completion_keeps_bytes_private_and_custody_after_pending_retirement() {
    let (mut engine, owner, call, probe, runtime, observed) = fixture(5, true, -1).await;
    let semantic = semantic_state(&engine);
    runtime
        .preflight_native_stream(
            owner,
            probe,
            &NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
            &observed,
        )
        .unwrap();
    engine
        .confirm_retained_stream_physical(owner, probe, &observed)
        .unwrap();
    runtime
        .confirm_native_stream(
            owner,
            probe,
            &NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
            &observed,
        )
        .unwrap();
    let retained = engine
        .private_receive_completion(owner, probe)
        .unwrap()
        .clone();
    assert_eq!(retained, observed.helper_copy.clone().unwrap());
    assert_eq!(engine.stream_calls[&call].native_receive.len(), 1);
    assert!(engine.stream_calls[&call].native_receive[0].joined);
    assert_eq!(semantic_state(&engine), semantic);
    let before = format!("{engine:?}");
    assert!(
        engine
            .complete_shadow_probe(
                owner,
                probe,
                LogicalTime::from_nanos(1_790_000_000_000_000_002),
                observed.bytes.clone(),
                false
            )
            .is_err()
    );
    assert!(
        engine
            .confirm_stream_physical(owner, probe, observed.confirmation.clone())
            .is_err()
    );
    assert!(
        engine
            .confirm_retained_stream_physical(owner, probe, &observed)
            .is_err()
    );
    assert_eq!(format!("{engine:?}"), before);
    let NetworkStreamChunk::Reserved {
        lease,
        selection_len,
        outcome,
    } = engine
        .reserve_private_receive_span(owner, call, probe, 100, 7)
        .unwrap()
    else {
        panic!("private complete bytes must reserve a private span")
    };
    assert_eq!(selection_len, 100);
    assert_eq!(
        outcome,
        NetworkStreamChunkOutcome::Bytes(observed.bytes[7..107].to_vec())
    );
    runtime.finish_native_stream_lease(owner, probe).unwrap();
    drop(observed);
    assert_eq!(
        engine.private_receive_completion(owner, lease).unwrap(),
        &retained
    );
    assert_eq!(
        engine
            .read_stream_chunk_view(owner, lease, 40, 512)
            .unwrap(),
        (47u8..107).collect::<Vec<_>>()
    );
    assert_eq!(
        engine
            .read_stream_chunk_view(owner, lease, 100, 512)
            .unwrap(),
        Vec::<u8>::new()
    );
    assert!(engine.read_stream_chunk_view(owner, lease, 101, 1).is_err());
    assert!(engine.read_stream_chunk_view(owner, lease, 0, 513).is_err());
    assert_eq!(semantic_state(&engine), semantic);
    forbidden_finish(&mut engine, owner, call, lease);
    assert!(engine.check_stream_operations_finished().is_err());
}

#[tokio::test]
async fn private_peek_missing_serialized_foreign_unjoined_or_v4_proof_cannot_mutate_engine() {
    for variant in 0..9 {
        let (mut engine, owner, call, lease, _runtime, actual) =
            fixture(if variant == 7 { 4 } else { 5 }, variant != 8, -1).await;
        let mut observed = actual.clone();
        let mut changed_owner = owner;
        let mut changed_lease = lease;
        match variant {
            0 => observed.helper_copy = None,
            1 => observed = serde_json::from_slice(&serde_json::to_vec(&actual).unwrap()).unwrap(),
            2 => observed = fixture(5, true, -1).await.5,
            3 => changed_owner.mm = owner.mm.for_exec(owner.thread),
            4 => changed_lease = serde_json::from_value(serde_json::json!(999)).unwrap(),
            5 => observed.bytes[0] ^= 1,
            6 => observed.raw_return -= 1,
            7 | 8 => {}
            _ => unreachable!(),
        }
        let before = format!("{engine:?}");
        assert!(
            engine
                .confirm_retained_stream_physical(changed_owner, changed_lease, &observed)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(format!("{engine:?}"), before, "variant {variant}");
        assert!(engine.stream_calls[&call].private_receive.is_none());
        assert!(engine.stream_calls[&call].native_receive.is_empty());
        assert!(engine.shadow_probes[&lease].pending.is_some());
    }
}

#[tokio::test]
async fn private_peek_cursor_restoration_is_exact_and_never_semantic_discharge() {
    let (mut engine, owner, call, lease, _runtime, observed) = fixture(5, true, 6).await;
    engine
        .confirm_retained_stream_physical(owner, lease, &observed)
        .unwrap();
    let semantic = semantic_state(&engine);
    let before = format!("{engine:?}");
    assert!(
        engine
            .reserve_private_receive_span(owner, call, lease, 10, 0)
            .is_err()
    );
    for effect in [
        NetworkStreamPhysicalEffect::SetPeekOffset { value: 7 },
        NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
        NetworkStreamPhysicalEffect::PollState,
        NetworkStreamPhysicalEffect::QueuedBytes,
        NetworkStreamPhysicalEffect::Drain { maximum: 1 },
    ] {
        assert!(
            engine
                .submit_retained_stream_physical(owner, lease, effect)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    assert!(
        engine
            .submit_stream_physical(
                owner,
                lease,
                NetworkStreamPhysicalEffect::SetPeekOffset { value: 6 }
            )
            .is_err()
    );
    engine
        .submit_retained_stream_physical(
            owner,
            lease,
            NetworkStreamPhysicalEffect::SetPeekOffset { value: 6 },
        )
        .unwrap();
    let pending = format!("{engine:?}");
    for variant in 0..3 {
        let mut bad = control_observation();
        match variant {
            0 => bad.raw_return = 1,
            1 => bad.errno = Some(libc::EIO),
            2 => bad.bytes.push(1),
            _ => unreachable!(),
        }
        assert!(
            engine
                .confirm_retained_stream_physical(owner, lease, &bad)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), pending);
    }
    engine
        .confirm_retained_stream_physical(owner, lease, &control_observation())
        .unwrap();
    assert!(engine.shadow_probes[&lease].cursor_restored());
    assert!(engine.stream_calls[&call].helper_copy.is_some());
    let NetworkStreamChunk::Reserved {
        lease: delivery, ..
    } = engine
        .reserve_private_receive_span(owner, call, lease, 10, 0)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(semantic_state(&engine), semantic);
    forbidden_finish(&mut engine, owner, call, delivery);
}

#[tokio::test]
async fn private_peek_changed_physical_cut_or_unknown_predecessor_cannot_select_or_replace_source()
{
    for advance in [false, true] {
        let (mut engine, owner, call, lease, _runtime, observed) = fixture(5, true, -1).await;
        engine
            .confirm_retained_stream_physical(owner, lease, &observed)
            .unwrap();
        let file = engine.stream_calls[&call].open_file.unwrap();
        // Explicit changed-authority premises; never native frontier evidence.
        if advance {
            engine
                .shadow
                .as_mut()
                .unwrap()
                .sockets
                .get_mut(&file)
                .unwrap()
                .native
                .as_mut()
                .unwrap()
                .physical_observed = Cut { bytes: 1, order: 1 };
        } else {
            engine.stream_calls.get_mut(&call).unwrap().native_receive[0].joined = false;
        }
        let before = format!("{engine:?}");
        assert!(
            engine
                .reserve_private_receive_span(owner, call, lease, 10, 0)
                .is_err()
        );
        assert!(
            engine
                .confirm_retained_stream_physical(owner, lease, &observed)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert_eq!(
            engine.private_receive_completion(owner, lease).unwrap(),
            observed.helper_copy.as_ref().unwrap()
        );
    }
}

#[tokio::test]
async fn private_peek_final_wait_keeps_unjoined_call_source_and_views_cannot_complete_it() {
    let (mut engine, owner, call, probe, _runtime, observed) = fixture(5, true, -1).await;
    engine
        .confirm_retained_stream_physical(owner, probe, &observed)
        .unwrap();
    let retained = observed.helper_copy.as_ref().unwrap().clone();
    let NetworkStreamChunk::Reserved { lease, .. } = engine
        .reserve_private_receive_span(owner, call, probe, 12, 0)
        .unwrap()
    else {
        panic!()
    };
    assert!(engine.native_stream_final_wait(owner));
    assert!(
        engine
            .terminal_stream_admission(owner, call)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        engine.stream_calls[&call]
            .private_receive
            .as_ref()
            .unwrap()
            .completion,
        retained
    );
    assert!(engine.check_stream_operations_finished().is_err());
    assert!(
        engine
            .finish_stream_chunk(owner, lease, NetworkStreamChunkDisposition::CopyFailed)
            .is_err()
    );
    assert!(engine.finish_stream_call_release(owner, call).is_err());
}

#[tokio::test]
async fn private_peek_overlap_compares_bytes_without_ordering_or_freezing_layout() {
    let (_a, _owner, _call, _lease, _ar, observed) =
        fixture_bytes(5, true, -1, b"abcdefgh", false).await;
    let source = PrivateSource::checked(observed.helper_copy.as_ref().unwrap()).unwrap();
    let (_b, _, _, _, _br, same) = fixture_bytes(5, true, -1, b"abcdefghmore", false).await;
    let larger = PrivateSource::checked(same.helper_copy.as_ref().unwrap()).unwrap();
    source.check_overlap(&larger).unwrap();
    larger.check_overlap(&source).unwrap();
    assert_eq!(source.cut, larger.cut);
    assert_ne!(source.length, larger.length);
    let (_c, _, _, _, _cr, changed) = fixture_bytes(5, true, -1, b"abcdXfgh", false).await;
    let changed = PrivateSource::checked(changed.helper_copy.as_ref().unwrap()).unwrap();
    assert!(source.check_overlap(&changed).is_err());
    assert!(changed.check_overlap(&source).is_err());
    assert_eq!(source.view(0, 512).unwrap(), b"abcdefgh");
    assert_eq!(larger.view(0, 512).unwrap(), b"abcdefghmore");
}

#[tokio::test]
async fn private_peek_actual_eagain_and_eof_keep_custody_without_empty_selection_or_publication() {
    for eof in [false, true] {
        let (mut engine, owner, call, lease, runtime, observed) =
            fixture_bytes(5, true, -1, b"", eof).await;
        let before = format!("{engine:?}");
        runtime
            .preflight_native_stream(
                owner,
                lease,
                &NetworkStreamPhysicalEffect::Peek { maximum: 1024 },
                &observed,
            )
            .unwrap();
        assert_eq!(observed.raw_return, if eof { 0 } else { -1 });
        assert_eq!(observed.errno, if eof { None } else { Some(libc::EAGAIN) });
        assert!(
            engine
                .confirm_retained_stream_physical(owner, lease, &observed)
                .is_err()
        );
        assert!(
            engine
                .reserve_private_receive_span(owner, call, lease, 1, 0)
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(
            observed
                .helper_copy
                .as_ref()
                .unwrap()
                .joined_worker()
                .is_ok()
        );
        assert!(engine.stream_calls[&call].helper_copy.is_some());
        assert!(engine.stream_calls[&call].private_receive.is_none());
    }
}

#[tokio::test]
async fn private_peek_published_prefix_compares_every_known_fragment_before_any_mutation() {
    for variant in 0..4 {
        let (mut engine, owner, _metadata, binding) =
            original_installation::controlled_receive_origin();
        engine
            .ensure_channel(
                binding.open_file,
                NetworkChannelBinding {
                    transport: NetworkTransportV2::Tcp,
                    role: NetworkEndpointRoleV2::OutboundClient,
                    peer_address: Some(NetworkAddressV2::Inet4 {
                        address: [192, 0, 2, 1],
                        port: 443,
                    }),
                    requested_local_constraint: None,
                    observed_local_address: None,
                    accepted_from: None,
                    selected_channel: None,
                },
            )
            .unwrap();
        let control = engine
            .begin_socket_controls(owner, vec![binding.open_file])
            .unwrap()[0]
            .1;
        let call = engine.begin_stream_call(owner, control).unwrap().id;
        engine
            .confirm_stream_call_pin(owner, call, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        // Existing semantic queue premises use the unchanged old publication
        // transaction. These numeric legacy observations are not native proof.
        for (index, part) in [b"ab", b"cd"].into_iter().enumerate() {
            let at = LogicalTime::from_nanos(1_790_000_000_000_000_001 + 2 * index as u64);
            let probe = engine.begin_shadow_probe(owner, call, at).unwrap();
            engine
                .submit_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::ReadPeekOffset,
                )
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalResult::PeekOffset(-1),
                )
                .unwrap();
            engine
                .submit_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::Peek {
                        maximum: probe.retained_prefix + 1024,
                    },
                )
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalResult::Peeked {
                        count: (index + 1) * 2,
                    },
                )
                .unwrap();
            engine
                .submit_stream_physical(owner, probe.lease, NetworkStreamPhysicalEffect::PollState)
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalResult::PollState {
                        revents: libc::POLLIN,
                    },
                )
                .unwrap();
            engine
                .submit_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalEffect::QueuedBytes,
                )
                .unwrap();
            engine
                .confirm_stream_physical(
                    owner,
                    probe.lease,
                    NetworkStreamPhysicalResult::QueuedBytes {
                        count: (index + 1) * 2,
                    },
                )
                .unwrap();
            engine
                .complete_shadow_probe(owner, probe.lease, at, part.to_vec(), false)
                .unwrap();
        }
        let probe = engine
            .begin_shadow_probe(
                owner,
                call,
                LogicalTime::from_nanos(1_790_000_000_000_000_007),
            )
            .unwrap();
        assert_eq!(probe.retained_prefix, 4);
        engine
            .submit_stream_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::ReadPeekOffset,
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalResult::PeekOffset(-1),
            )
            .unwrap();
        engine
            .submit_stream_physical(
                owner,
                probe.lease,
                NetworkStreamPhysicalEffect::Peek { maximum: 1028 },
            )
            .unwrap();
        let full = if variant == 1 { b"abXdef" } else { b"abcdef" };
        let (_runtime, observed, _receipt) = HelperCopyBinding::controlled_joined_peek_prefix(
            owner,
            call,
            probe.lease,
            &mut engine,
            full,
            5,
            true,
            false,
            4,
        )
        .await;
        assert_eq!(observed.bytes, b"ef");
        assert_eq!(observed.raw_return, 6);
        let channel = engine.bound_channel(binding.open_file).unwrap();
        if variant == 2 {
            engine
                .channels
                .get_mut(&channel)
                .unwrap()
                .inbound
                .pop_front();
        }
        if variant == 3 {
            let InboundOutcome::Stream {
                requires_message_io,
                ..
            } = engine
                .channels
                .get_mut(&channel)
                .unwrap()
                .inbound
                .front_mut()
                .unwrap()
            else {
                panic!()
            };
            *requires_message_io = true;
        }
        let before = format!("{engine:?}");
        let semantic = semantic_state(&engine);
        let result = engine.confirm_retained_stream_physical(owner, probe.lease, &observed);
        if variant == 0 {
            result.unwrap();
            assert_eq!(semantic_state(&engine), semantic);
            assert!(engine.stream_calls[&call].private_receive.is_some());
        } else {
            assert!(result.is_err(), "variant {variant}");
            assert_eq!(format!("{engine:?}"), before);
            assert!(engine.stream_calls[&call].private_receive.is_none());
            assert!(engine.stream_calls[&call].native_receive.is_empty());
        }
    }
}
