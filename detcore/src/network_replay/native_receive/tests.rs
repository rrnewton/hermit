//! Controlled provider records exercise the actual installation ACK, original
//! Call admission and private custody parser. No native execution is claimed.
use std::sync::Arc;

use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::FileMetadataObservation;
use crate::network_replay::original_connect::Kind;
use crate::network_runtime::OriginalSelection;
use crate::network_runtime::original_read_copy::ReadCopyCustody;
use crate::resources::ExternalOpId;
use crate::types::DetTid;
use crate::types::MmId;

struct Reader {
    owner: NetworkStreamOwner,
    admission: Admission,
    selected: OriginalSelection,
    custody: Arc<ReadCopyCustody>,
}
fn reader(
    engine: &mut NetworkReplayEngine,
    parent: NetworkStreamOwner,
    binding: FdSlotBinding,
    thread: i32,
) -> Reader {
    let thread = DetTid::from_raw(thread);
    let owner = if thread == parent.thread {
        parent
    } else {
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        assert_eq!(
            engine.fd_publication_fixture_register(owner, Some(parent)),
            binding.slot.files
        );
        owner
    };
    let admission = engine
        .begin_original_connect(
            owner,
            Arguments {
                kind: Kind::Read,
                operation: ExternalOpId::new(owner.thread, 500),
                files: binding.slot.files,
                binding: Some(binding),
                fd: binding.slot.fd,
                address: 0x2000,
                length: 0,
                original_count: 64,
            },
        )
        .unwrap();
    let command = 100 + admission.call.native_command_call();
    engine
        .original_connect_provider_submitted(owner, &admission)
        .unwrap();
    engine
        .original_call_prepared(owner, &admission, None, command)
        .unwrap();
    engine.original_connect_invoked(owner, &admission).unwrap();
    engine
        .original_read_metadata_prepared(
            owner,
            &admission,
            &FileMetadataObservation {
                admission: admission.clone(),
                logical_nonblocking: Some(false),
                status_flags: Some(libc::O_RDWR),
            },
        )
        .unwrap();
    engine.original_read_entered(owner, &admission).unwrap();
    let selected = OriginalSelection {
        provider: 7,
        command,
        call: admission.call.native_command_call(),
        owner_mm: owner.mm.generation(),
        task: thread.as_raw() as u64,
        task_start: 101,
        table: 13,
        file: 19,
        ready: 1,
        requested_fd: binding.slot.fd,
        user_address: 0x2000,
        address_length: 0,
        original_count: 64,
        fdput_flags: 1,
    };
    engine
        .original_connect_selected(
            owner,
            &admission,
            command,
            (
                selected.provider,
                selected.task,
                selected.task_start,
                selected.table,
                selected.file,
            ),
        )
        .unwrap();
    let custody = Arc::new(ReadCopyCustody::default());
    assert_eq!(
        engine
            .bind_original_read_copy(owner, &admission, command, &custody)
            .unwrap(),
        0
    );
    Reader {
        owner,
        admission,
        selected,
        custody,
    }
}
type NativeAttemptSpec = (u64, u64, u64, u64, Vec<u8>, i64);

fn receipt(reader: &Reader, specs: &[NativeAttemptSpec]) -> Vec<NativeAttempt> {
    reader
        .custody
        .controlled_append_v5(
            reader.owner,
            reader.admission.call,
            reader.selected.clone(),
            specs,
        )
        .unwrap();
    reader
        .custody
        .completed_since(0)
        .unwrap()
        .unwrap()
        .native_attempts()
        .to_vec()
}
fn frontier(engine: &NetworkReplayEngine, binding: FdSlotBinding) -> Cut {
    engine.shadow.as_ref().unwrap().sockets[&binding.open_file]
        .native
        .as_ref()
        .unwrap()
        .physical_observed
}
fn observe(
    engine: &mut NetworkReplayEngine,
    reader: &Reader,
    attempts: &[NativeAttempt],
) -> Result<(), NetworkReplayError> {
    engine.retain_native_receive_attempts(reader.owner, reader.admission.call, attempts)
}

#[test]
fn native_receive_origin_survives_actual_ack_but_public_profile_has_no_authority() {
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    assert!(engine.fd_publication_history.is_empty());
    assert!(engine.fd_installations.is_empty());
    assert_eq!(frontier(&engine, binding), Cut::ZERO);
    let public = engine
        .stream_socket_state(binding.open_file)
        .unwrap()
        .unwrap();
    let decoded: NetworkStreamSocketState =
        serde_json::from_slice(&serde_json::to_vec(&public).unwrap()).unwrap();
    assert_eq!(public, decoded);
    assert!(Socket::new(decoded).native.is_none());
    let a = reader(&mut engine, owner, binding, 31);
    let attempts = receipt(&a, &[(0, 0, 3, 6, b"abc".to_vec(), 0)]);
    let before = format!("{engine:?}");
    let mut changed_owner = owner;
    changed_owner.mm = owner.mm.for_exec(owner.thread);
    assert!(
        engine
            .retain_native_receive_attempts(changed_owner, a.admission.call, &attempts)
            .is_err()
    );
    assert_eq!(format!("{engine:?}"), before);
    observe(&mut engine, &a, &attempts).unwrap();
    assert_eq!(frontier(&engine, binding), Cut { bytes: 3, order: 1 });
    assert_eq!(
        engine
            .stream_socket_state(binding.open_file)
            .unwrap()
            .unwrap()
            .consume_epoch,
        0
    );
    assert!(engine.channels.is_empty());
    assert!(engine.finish_original_connect(owner, &a.admission).is_err());
}

#[test]
fn native_receive_interleaved_a_b_a_waits_for_exact_predecessors_in_every_delivery_order() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let (mut engine, owner, _metadata, binding) =
            original_installation::controlled_receive_origin();
        let a = reader(&mut engine, owner, binding, 31);
        let b = reader(&mut engine, owner, binding, 32);
        let ar = receipt(
            &a,
            &[
                (0, 0, 3, 8, b"abc".to_vec(), 0),
                (5, 2, 3, 3, b"fgh".to_vec(), 0),
            ],
        );
        let br = receipt(&b, &[(3, 1, 2, 5, b"de".to_vec(), 0)]);
        // Per-Call receipt extraction cannot skip A1 to deliver A2 alone. The
        // A2-first case carries A1's known raw handle but tests global ordering
        // by retaining A1 as pending below, not inventing local history.
        let mut delivered = [false; 3];
        for which in order {
            delivered[which] = true;
            // Existing Driver supplies a contiguous local prefix; a host reply
            // containing A2 necessarily retains A1 as well.
            if which == 2 {
                delivered[0] = true;
            }
            match which {
                0 => observe(&mut engine, &a, &ar[..1]).unwrap(),
                1 => observe(&mut engine, &b, &br).unwrap(),
                2 => observe(&mut engine, &a, &ar).unwrap(),
                _ => unreachable!(),
            }
            let expected = if delivered[0] && delivered[1] && delivered[2] {
                Cut { bytes: 8, order: 3 }
            } else if delivered[0] && delivered[1] {
                Cut { bytes: 5, order: 2 }
            } else if delivered[0] {
                Cut { bytes: 3, order: 1 }
            } else {
                Cut::ZERO
            };
            assert_eq!(
                frontier(&engine, binding),
                expected,
                "order {order:?}, event {which}"
            );
            assert!(engine.channels.is_empty());
        }
        assert_eq!(
            engine.stream_calls[&a.admission.call].native_receive.len(),
            2
        );
        assert_eq!(
            engine.stream_calls[&b.admission.call].native_receive.len(),
            1
        );
    }
}

#[test]
fn native_receive_stale_new_command_or_duplicate_successor_cannot_advance_or_replace() {
    for first_pending in [false, true] {
        let (mut engine, owner, _metadata, binding) =
            original_installation::controlled_receive_origin();
        let a = reader(&mut engine, owner, binding, 31);
        let b = reader(&mut engine, owner, binding, 32);
        let ar = receipt(
            &a,
            &[(
                if first_pending { 3 } else { 0 },
                if first_pending { 1 } else { 0 },
                3,
                3,
                b"abc".to_vec(),
                0,
            )],
        );
        let br = receipt(
            &b,
            &[(
                if first_pending { 3 } else { 0 },
                if first_pending { 1 } else { 0 },
                3,
                3,
                b"xyz".to_vec(),
                0,
            )],
        );
        observe(&mut engine, &a, &ar).unwrap();
        let before = format!("{engine:?}");
        assert!(observe(&mut engine, &b, &br).is_err());
        assert_eq!(format!("{engine:?}"), before);
        observe(&mut engine, &a, &ar).unwrap();
        assert_eq!(format!("{engine:?}"), before);
        if first_pending {
            let c = reader(&mut engine, owner, binding, 33);
            let cr = receipt(&c, &[(0, 0, 3, 6, b"pre".to_vec(), 0)]);
            observe(&mut engine, &c, &cr).unwrap();
            assert_eq!(frontier(&engine, binding), Cut { bytes: 6, order: 2 });
            assert!(
                engine.stream_calls[&b.admission.call]
                    .native_receive
                    .is_empty()
            );
        }
    }
}

#[test]
fn native_receive_fault_layouts_share_cut_without_ordering_or_semantic_consumption() {
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    let a = reader(&mut engine, owner, binding, 31);
    let b = reader(&mut engine, owner, binding, 32);
    let ar = receipt(&a, &[(0, 0, 32, 32, vec![b'a'; 16], -14)]);
    let br = receipt(&b, &[(0, 0, 64, 64, vec![b'a'; 32], -14)]);
    observe(&mut engine, &b, &br).unwrap();
    observe(&mut engine, &a, &ar).unwrap();
    assert_eq!(frontier(&engine, binding), Cut::ZERO);
    assert_eq!(
        engine.stream_calls[&a.admission.call].native_receive[0]
            .receipt
            .unit()
            .observation
            .unwrap()
            .begin
            .available,
        32
    );
    assert_eq!(
        engine.stream_calls[&b.admission.call].native_receive[0]
            .receipt
            .unit()
            .observation
            .unwrap()
            .begin
            .available,
        64
    );
    assert!(engine.channels.is_empty());
    assert!(engine.shadow.as_ref().unwrap().units.is_empty());
}

#[test]
fn native_receive_late_cut_without_live_custody_refuses_instead_of_recreating_history() {
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    let a = reader(&mut engine, owner, binding, 31);
    let b = reader(&mut engine, owner, binding, 32);
    let ar = receipt(
        &a,
        &[
            (0, 0, 3, 8, b"abc".to_vec(), 0),
            (3, 1, 5, 5, b"defgh".to_vec(), 0),
        ],
    );
    observe(&mut engine, &a, &ar).unwrap();
    // Production cannot retire this unresolved Read today. Model the future
    // removal explicitly to test the bounded live-custody policy, not to claim
    // terminal cleanup/semantic discharge occurred.
    let removed = engine.stream_calls.remove(&a.admission.call).unwrap();
    let br = receipt(&b, &[(3, 1, 3, 5, b"d".to_vec(), -14)]);
    let before = format!("{engine:?}");
    assert!(observe(&mut engine, &b, &br).is_err());
    assert_eq!(format!("{engine:?}"), before);
    engine.stream_calls.insert(a.admission.call, removed);
    observe(&mut engine, &b, &br).unwrap();
    assert_eq!(frontier(&engine, binding), Cut { bytes: 8, order: 2 });
}

#[test]
fn native_receive_numeric_same_fields_other_custody_missing_origin_and_changed_file_refuse() {
    for mutation in 0..4 {
        let (mut engine, owner, _metadata, binding) =
            original_installation::controlled_receive_origin();
        let a = reader(&mut engine, owner, binding, 31);
        let records = receipt(&a, &[(0, 0, 3, 6, b"abc".to_vec(), 0)]);
        let replacement = Arc::new(ReadCopyCustody::default());
        let input = if mutation == 0 {
            replacement
                .controlled_append_v5(
                    a.owner,
                    a.admission.call,
                    a.selected.clone(),
                    &[(0, 0, 3, 6, b"abc".to_vec(), 0)],
                )
                .unwrap();
            replacement
                .completed_since(0)
                .unwrap()
                .unwrap()
                .native_attempts()
                .to_vec()
        } else {
            records
        };
        match mutation {
            1 => {
                engine
                    .shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&binding.open_file)
                    .unwrap()
                    .native = None
            }
            2 => {
                engine
                    .shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&binding.open_file)
                    .unwrap()
                    .native
                    .as_mut()
                    .unwrap()
                    .identity = FileIdentity::controlled_fixture(8, 19)
            }
            3 => {
                engine
                    .shadow
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut(&binding.open_file)
                    .unwrap()
                    .native
                    .as_mut()
                    .unwrap()
                    .identity = FileIdentity::controlled_fixture(7, 20)
            }
            _ => {}
        }
        let before = format!("{engine:?}");
        assert!(
            observe(&mut engine, &a, &input).is_err(),
            "mutation {mutation}"
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(
            engine.stream_calls[&a.admission.call]
                .native_receive
                .is_empty()
        );
    }
}

#[test]
fn native_receive_valid_prefix_before_refused_suffix_remains_physical_only() {
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    let a = reader(&mut engine, owner, binding, 31);
    let records = a
        .custody
        .controlled_append_v5(
            a.owner,
            a.admission.call,
            a.selected.clone(),
            &[(0, 0, 3, 6, b"abc".to_vec(), 0)],
        )
        .unwrap();
    let mut malformed = records[0].clone();
    malformed.sequence = 4;
    malformed.attempt = 2;
    malformed.kind = 99;
    assert!(
        a.custody
            .append(&a.selected, 3, vec![malformed], None)
            .is_err()
    );
    let delta = a.custody.completed_since(0).unwrap().unwrap();
    assert_eq!(
        engine
            .retain_original_read_copy(a.owner, &a.admission, delta)
            .unwrap(),
        1
    );
    assert_eq!(frontier(&engine, binding), Cut { bytes: 3, order: 1 });
    assert!(a.custody.require_no_unjoined_receipts(1).is_err());
    assert!(
        engine
            .finish_original_connect(a.owner, &a.admission)
            .is_err()
    );
    assert!(engine.channels.is_empty());
    assert!(engine.shadow.as_ref().unwrap().units.is_empty());
}

#[test]
fn native_receive_engine_join_does_not_wait_for_canonical_custody_mutex() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (mut engine, owner, _metadata, binding) =
        original_installation::controlled_receive_origin();
    let a = reader(&mut engine, owner, binding, 31);
    let attempts = receipt(&a, &[(0, 0, 3, 6, b"abc".to_vec(), 0)]);
    let delta = a.custody.completed_since(0).unwrap().unwrap();
    let (held, held_rx) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let raw_owner = std::thread::spawn(move || {
        delta
            .with_unit(0, |_, _| {
                held.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(1)).unwrap();
            })
            .unwrap()
    });
    held_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let (done, done_rx) = mpsc::channel();
    let joiner = std::thread::spawn(move || {
        let result = observe(&mut engine, &a, &attempts);
        done.send(result.is_ok()).unwrap();
        (engine, result)
    });
    let completed_before_release = done_rx.recv_timeout(Duration::from_secs(1));
    let _ = release.send(());
    raw_owner.join().unwrap();
    let (engine, result) = joiner.join().unwrap();
    assert!(completed_before_release.unwrap());
    result.unwrap();
    assert_eq!(frontier(&engine, binding), Cut { bytes: 3, order: 1 });
}
