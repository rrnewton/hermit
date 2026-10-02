//! Actual engine/local publication controls. Provider receipts below are explicit
//! test inputs; these controls do not claim native kernel or flush evidence.
use chrono::TimeZone;

use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::Kind;
use crate::network_replay::original_connect::Local;
use crate::network_replay::*;
use crate::network_runtime::OriginalSelection;

fn install_at(
    engine: &mut NetworkReplayEngine,
    metadata: &mut FileMetadata,
    owner: NetworkStreamOwner,
    fd: i32,
) -> FdSlotBinding {
    metadata
        .add_fd(owner.thread, fd, OFlag::empty(), FdType::Socket, None)
        .unwrap();
    let replacement = *metadata.pending_network_installations.last().unwrap();
    let effect = engine.fd_publication_fixture_effect(owner, replacement);
    let admission = engine
        .acquire_fd_publication(owner, metadata.files_id)
        .unwrap();
    metadata
        .associate_network_installation(replacement.installation_generation, effect)
        .unwrap();
    let batch = metadata.publication_snapshot(&admission).unwrap();
    engine
        .publish_fd_publication(owner, admission.permit, &batch)
        .unwrap();
    metadata.publication_acknowledge(&batch).unwrap();
    engine
        .acknowledge_fd_publication(owner, admission.permit, &batch)
        .unwrap();
    assert_eq!(
        metadata.network_publication.awaiting_global_ack,
        Some(batch.clone())
    );
    metadata.publication_server_acknowledge(&batch).unwrap();
    assert!(metadata.network_publication.awaiting_global_ack.is_none());
    metadata.descriptor_binding(fd).unwrap()
}

fn install(
    engine: &mut NetworkReplayEngine,
    metadata: &mut FileMetadata,
    owner: NetworkStreamOwner,
) -> FdSlotBinding {
    install_at(engine, metadata, owner, 7)
}

pub(crate) fn dispatcher_install(
    engine: &mut NetworkReplayEngine,
    metadata: &mut FileMetadata,
    owner: NetworkStreamOwner,
    fd: i32,
) -> FdSlotBinding {
    install_at(engine, metadata, owner, fd)
}

// This is the existing modeled Alias transaction, with an explicit successful
// result fixture. It does not claim native fd selection or kernel execution.
fn alias(
    engine: &mut NetworkReplayEngine,
    metadata: &mut FileMetadata,
    owner: NetworkStreamOwner,
    source_fd: i32,
    destination: i32,
) -> FdSlotBinding {
    let source = metadata.descriptor_binding(source_fd).unwrap();
    assert_eq!(
        engine.fd_table_fixture_files(owner),
        Some(metadata.files_id)
    );
    assert_eq!(metadata.descriptor_binding(destination), Err(Errno::EBADF));
    assert_eq!(
        metadata
            .with_detfd(source_fd, |fd| fd.open_file_alias_count())
            .unwrap(),
        1
    );
    let before = engine.native_capture_fixture_counts(source.open_file);
    let NetworkFdMutationBegin::Admitted(admission) = engine
        .begin_fd_mutation(
            owner,
            metadata.files_id,
            NetworkFdMutationKind::Alias {
                source_fd,
                source: Some(source),
                kind: NetworkFdInstallKind::Dup2,
                cloexec: false,
                destination: Some(destination),
                replaced: None,
            },
        )
        .unwrap()
    else {
        panic!("existing alias source must be admitted");
    };
    let admission = *admission;
    let permit = admission.publication.permit;
    let captured = metadata.capture_fd(source_fd).unwrap();
    engine.submit_fd_mutation(owner, permit).unwrap();
    engine
        .confirm_fd_mutation_result(owner, permit, Ok(i64::from(destination)))
        .unwrap();
    assert_eq!(
        metadata
            .install_captured_fd(captured, destination, OFlag::empty())
            .unwrap(),
        None
    );
    let replacement = *metadata.pending_network_installations.last().unwrap();
    let effect = engine
        .confirm_fd_installation(owner, permit, replacement)
        .unwrap();
    assert_eq!(effect.kind, NetworkFdInstallKind::Dup2);
    metadata
        .associate_network_installation(replacement.installation_generation, effect)
        .unwrap();
    let batch = metadata
        .publication_snapshot(&admission.publication)
        .unwrap();
    assert_eq!(
        engine
            .publish_fd_publication(owner, permit, &batch)
            .unwrap(),
        batch
    );
    metadata.publication_acknowledge(&batch).unwrap();
    engine
        .acknowledge_fd_publication(owner, permit, &batch)
        .unwrap();
    assert_eq!(
        metadata.network_publication.awaiting_global_ack,
        Some(batch.clone())
    );
    metadata.publication_server_acknowledge(&batch).unwrap();
    assert!(metadata.network_publication.awaiting_global_ack.is_none());
    assert_eq!(metadata.descriptor_binding(source_fd).unwrap(), source);
    let installed = metadata.descriptor_binding(destination).unwrap();
    assert_eq!(installed.open_file, source.open_file);
    assert_ne!(installed.generation, source.generation);
    assert_eq!(
        metadata
            .with_detfd(source_fd, |fd| fd.open_file_alias_count())
            .unwrap(),
        2
    );
    assert_eq!(
        engine.native_capture_fixture_counts(source.open_file),
        before
    );
    installed
}

pub(crate) fn fixture(
    occupied: bool,
) -> (
    NetworkReplayEngine,
    FileMetadata,
    NetworkStreamOwner,
    NetworkStreamOwner,
    Admission,
) {
    fixture_with_alias(occupied, false)
}

pub(crate) fn dispatcher_fixture(
    fd: i32,
) -> (NetworkReplayEngine, FileMetadata, NetworkStreamOwner) {
    let first = DetTid::from_raw(61);
    let owner = NetworkStreamOwner {
        thread: first,
        mm: MmId::initial(first),
    };
    let mut engine = NetworkReplayEngine::record_native_receive(
        chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
    );
    engine.fd_table_fixture_enable();
    engine.fd_publication_fixture_register(owner, None);
    let mut metadata = FileMetadata::empty_network_fixture(first);
    dispatcher_install(&mut engine, &mut metadata, owner, fd);
    (engine, metadata, owner)
}

fn fixture_with_alias(
    occupied: bool,
    retain_alias: bool,
) -> (
    NetworkReplayEngine,
    FileMetadata,
    NetworkStreamOwner,
    NetworkStreamOwner,
    Admission,
) {
    let first = DetTid::from_raw(61);
    let owner = NetworkStreamOwner {
        thread: first,
        mm: MmId::initial(first),
    };
    let peer = NetworkStreamOwner {
        thread: DetTid::from_raw(62),
        mm: owner.mm,
    };
    let mut engine =
        NetworkReplayEngine::record(chrono::Utc.timestamp_opt(1_790_000_000, 0).unwrap());
    engine.fd_table_fixture_enable();
    let files = engine.fd_publication_fixture_register(owner, None);
    assert_eq!(
        engine.fd_publication_fixture_register(peer, Some(owner)),
        files
    );
    let mut metadata = FileMetadata::empty_network_fixture(first);
    if occupied {
        install(&mut engine, &mut metadata, owner);
    }
    if retain_alias {
        assert!(occupied);
        let source = metadata.descriptor_binding(7).unwrap();
        let retained = alias(&mut engine, &mut metadata, owner, 7, 8);
        assert_eq!(retained.open_file, source.open_file);
        assert_eq!(metadata.descriptor_binding(7).unwrap(), source);
    }
    let admission = engine
        .begin_original_connect(
            owner,
            Arguments {
                kind: Kind::Close,
                operation: crate::resources::ExternalOpId::new(first, 10),
                files,
                binding: metadata.descriptor_binding(7).ok(),
                fd: 7,
                address: 0,
                length: 0,
                original_count: 0,
            },
        )
        .unwrap();
    engine
        .original_connect_provider_submitted(owner, &admission)
        .unwrap();
    engine
        .original_call_prepared(owner, &admission, None, 17)
        .unwrap();
    (engine, metadata, owner, peer, admission)
}

fn selection(admission: &Admission) -> OriginalSelection {
    OriginalSelection {
        command: 17,
        call: admission.call.native_command_call(),
        owner_mm: 0,
        provider: 1,
        task: 61,
        task_start: 99,
        table: 5,
        file: if admission.arguments.binding.is_some() {
            admission.arguments.fd as u64
        } else {
            0
        },
        requested_fd: admission.arguments.fd,
        ready: 1,
        user_address: 0,
        fdput_flags: 0,
        address_length: 0,
        original_count: 0,
    }
}

pub(crate) fn selected(owner: NetworkStreamOwner, admission: &Admission) -> OriginalSelection {
    let mut value = selection(admission);
    value.owner_mm = owner.mm.generation();
    value
}

fn local(admission: &Admission, invoked: bool) -> Local {
    Local {
        arguments: admission.arguments.clone(),
        raw_arguments: [7, 0, 0, 0, 0, 0],
        admission: Some(admission.clone()),
        invoked,
        returned: None,
    }
}

#[test]
fn close_publication_allows_peer_reuse_before_final_eintr_without_removing_replacement() {
    for same_file in [false, true] {
        let (mut engine, mut metadata, owner, peer, admission) =
            fixture_with_alias(true, same_file);
        let old = admission.arguments.binding.unwrap();
        let retained = metadata.descriptor_binding(8).ok();
        assert_eq!(
            retained.map(|binding| binding.open_file),
            same_file.then_some(old.open_file)
        );
        let physical = selected(owner, &admission);
        assert!(
            engine
                .acquire_fd_publication(peer, metadata.files_id)
                .is_err()
        );
        assert!(
            engine
                .publish_original_close_selection(owner, &admission, &physical, &mut metadata)
                .is_err()
        );
        engine.original_connect_invoked(owner, &admission).unwrap();
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
        engine
            .publish_original_close_selection(owner, &admission, &physical, &mut metadata)
            .unwrap();
        assert_eq!(metadata.descriptor_binding(7), Err(Errno::EBADF));
        assert_eq!(
            engine.native_capture_fixture_counts(old.open_file),
            (1, 0, 0, 1)
        );
        assert!(engine.finish_original_connect(owner, &admission).is_err());
        assert_eq!(metadata.descriptor_binding(8).ok(), retained);
        let replacement = if same_file {
            assert_eq!(
                metadata
                    .with_detfd(8, |fd| fd.open_file_alias_count())
                    .unwrap(),
                1
            );
            alias(&mut engine, &mut metadata, peer, 8, 7)
        } else {
            install(&mut engine, &mut metadata, peer)
        };
        assert_ne!(replacement.generation, old.generation);
        assert_eq!(replacement.open_file == old.open_file, same_file);
        engine
            .publish_original_close_selection(owner, &admission, &physical, &mut metadata)
            .unwrap();
        assert_eq!(metadata.descriptor_binding(7).unwrap(), replacement);
        let raw = -i64::from(libc::EINTR);
        engine
            .original_connect_returned(owner, &admission, raw)
            .unwrap();
        assert!(
            engine
                .original_connect_provider_retired(owner, &admission, 0)
                .is_err()
        );
        engine
            .original_connect_provider_retired(owner, &admission, raw)
            .unwrap();
        engine
            .original_connect_pin_released(owner, &admission)
            .unwrap();
        engine.finish_original_connect(owner, &admission).unwrap();
        assert_eq!(metadata.descriptor_binding(7).unwrap(), replacement);
        assert_eq!(
            engine.native_capture_fixture_counts(old.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(metadata.descriptor_binding(8).ok(), retained);
        if same_file {
            assert_eq!(
                metadata
                    .with_detfd(8, |fd| fd.open_file_alias_count())
                    .unwrap(),
                2
            );
        }
    }
}

#[test]
fn close_publication_wrong_receipt_or_local_generation_changes_neither_population() {
    for mutation in 0..8 {
        let (mut engine, mut metadata, owner, peer, admission) = fixture(true);
        engine.original_connect_invoked(owner, &admission).unwrap();
        let old = admission.arguments.binding.unwrap();
        let mut raw = selected(owner, &admission);
        match mutation {
            0 => raw.command += 1,
            1 => raw.call += 1,
            2 => raw.owner_mm += 1,
            3 => raw.requested_fd += 1,
            4 => raw.file = 0,
            5 => raw.ready = 0,
            6 => raw.fdput_flags = 1,
            7 => {
                metadata.slot_generations.insert(7, old.generation + 1);
            }
            _ => unreachable!(),
        }
        let before = metadata.descriptor_binding(7).unwrap();
        assert!(
            engine
                .publish_original_close_selection(owner, &admission, &raw, &mut metadata)
                .is_err()
        );
        assert_eq!(metadata.descriptor_binding(7).unwrap(), before);
        assert_eq!(
            engine.native_capture_fixture_counts(old.open_file),
            (1, 1, 1, 1)
        );
        assert!(
            engine
                .acquire_fd_publication(peer, metadata.files_id)
                .is_err()
        );
        assert_eq!(
            engine.original_connect_result(owner, &admission).unwrap(),
            None
        );
    }
}

#[test]
fn close_empty_selection_releases_only_its_permit_and_requires_independent_final_result() {
    let (mut engine, mut metadata, owner, peer, admission) = fixture(false);
    engine.original_connect_invoked(owner, &admission).unwrap();
    engine
        .publish_original_close_selection(
            owner,
            &admission,
            &selected(owner, &admission),
            &mut metadata,
        )
        .unwrap();
    assert!(metadata.network_descriptor_slots().is_empty());
    assert_eq!(
        engine.original_connect_result(owner, &admission).unwrap(),
        None
    );
    assert!(
        engine
            .original_connect_provider_retired(owner, &admission, -i64::from(libc::EBADF))
            .is_err()
    );
    let permit = engine
        .acquire_fd_publication(peer, metadata.files_id)
        .unwrap()
        .permit;
    engine.release_empty_fd_publication(peer, permit).unwrap();
    engine
        .original_connect_returned(owner, &admission, -i64::from(libc::EBADF))
        .unwrap();
    engine
        .original_connect_provider_retired(owner, &admission, -i64::from(libc::EBADF))
        .unwrap();
    engine
        .original_connect_pin_released(owner, &admission)
        .unwrap();
    engine.finish_original_connect(owner, &admission).unwrap();
}

#[test]
fn close_uninvoked_cancellation_preserves_original_slot_until_exact_disarm() {
    let (mut engine, metadata, owner, peer, admission) = fixture(true);
    let old = admission.arguments.binding.unwrap();
    engine
        .original_connect_consumed(owner, &local(&admission, false))
        .unwrap();
    assert!(
        engine
            .acquire_fd_publication(peer, metadata.files_id)
            .is_err()
    );
    assert!(
        engine
            .original_connect_disarmed(owner, &admission, 18)
            .is_err()
    );
    engine
        .original_connect_disarmed(owner, &admission, 17)
        .unwrap();
    assert_eq!(metadata.descriptor_binding(7).unwrap(), old);
    assert_eq!(
        engine.original_connect_result(owner, &admission).unwrap(),
        None
    );
    engine
        .original_connect_cancel_retired(owner, &admission)
        .unwrap();
    engine
        .original_connect_pin_released(owner, &admission)
        .unwrap();
    engine.finish_original_connect(owner, &admission).unwrap();
    assert_eq!(metadata.descriptor_binding(7).unwrap(), old);
}

#[test]
fn close_terminal_without_selection_keeps_shared_table_fenced_until_consuming_owners_are_gone() {
    let (mut engine, metadata, owner, peer, admission) = fixture(true);
    engine.original_connect_invoked(owner, &admission).unwrap();
    let retained = local(&admission, true);
    engine
        .original_connect_final_wait(owner, &retained)
        .unwrap();
    engine.original_connect_consumed(owner, &retained).unwrap();
    engine.retire_fd_table_owner(owner);
    engine.stream_owner_gone(owner);
    assert!(
        engine
            .original_close_terminal_publication_pending(owner, &admission)
            .unwrap()
    );
    assert!(
        engine
            .original_connect_dead_retired(owner, &admission, 17)
            .is_err()
    );
    assert!(
        engine
            .acquire_fd_publication(peer, metadata.files_id)
            .is_err()
    );
    engine.retire_fd_table_owner(peer);
    engine.stream_owner_gone(peer);
    assert!(
        !engine
            .original_close_terminal_publication_pending(owner, &admission)
            .unwrap()
    );
    engine
        .original_connect_dead_retired(owner, &admission, 17)
        .unwrap();
    assert_eq!(
        engine.original_connect_result(owner, &admission).unwrap(),
        None
    );
    engine
        .original_connect_pin_released(owner, &admission)
        .unwrap();
    engine.finish_original_connect(owner, &admission).unwrap();
}

#[test]
fn close_positive_terminal_selection_publishes_once_after_original_owner_consumption() {
    let (mut engine, mut metadata, owner, peer, admission) = fixture(true);
    engine.original_connect_invoked(owner, &admission).unwrap();
    let retained = local(&admission, true);
    engine
        .original_connect_final_wait(owner, &retained)
        .unwrap();
    engine.original_connect_consumed(owner, &retained).unwrap();
    engine.retire_fd_table_owner(owner);
    engine.stream_owner_gone(owner);
    engine
        .publish_original_close_selection(
            owner,
            &admission,
            &selected(owner, &admission),
            &mut metadata,
        )
        .unwrap();
    let replacement = install(&mut engine, &mut metadata, peer);
    engine
        .publish_original_close_selection(
            owner,
            &admission,
            &selected(owner, &admission),
            &mut metadata,
        )
        .unwrap();
    engine
        .original_connect_dead_retired(owner, &admission, 17)
        .unwrap();
    assert_eq!(
        engine.original_connect_result(owner, &admission).unwrap(),
        None
    );
    engine
        .original_connect_pin_released(owner, &admission)
        .unwrap();
    engine.finish_original_connect(owner, &admission).unwrap();
    assert_eq!(metadata.descriptor_binding(7).unwrap(), replacement);
}
