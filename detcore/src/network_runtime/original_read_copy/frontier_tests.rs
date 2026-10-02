//! Additive copy5 grammar controls. Controller's separate controls qualify the
//! private loader/prepare/selection capability; these do not mint that authority.
use super::*;

fn selection(maximum: u64) -> OriginalSelection {
    OriginalSelection {
        provider: 3,
        command: 7,
        call: 11,
        task: 13,
        task_start: 17,
        table: 19,
        file: 23,
        ready: 1,
        fdput_flags: 1,
        original_count: maximum,
        owner_mm: 0,
        user_address: 0,
        requested_fd: 0,
        address_length: 0,
    }
}
fn words(sequence: u64, attempt: u64, offset: u64, kind: u32, fields: &[u64]) -> Record {
    let mut bytes = vec![0; RECORD_BYTES];
    for (word, value) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(fields) {
        word.copy_from_slice(&value.to_le_bytes());
    }
    Record {
        provider: 3,
        command: 7,
        call: 11,
        task: 13,
        task_start: 17,
        sequence,
        attempt,
        offset,
        length: (fields.len() * 8) as u32,
        kind,
        bytes,
    }
}
#[expect(
    clippy::too_many_arguments,
    reason = "Grammar tests vary each wire field independently, including invalid combinations"
)]
fn attempt(
    records: &mut Vec<Record>,
    attempt: u64,
    offset: u64,
    before: u64,
    order: u64,
    requested: u64,
    available: u64,
    payload: &[u8],
    returned: i64,
    disposition: u64,
) {
    let start = before + if disposition == OBSERVE { offset } else { 0 };
    let begin = [
        23,
        before,
        start,
        order,
        offset,
        requested,
        available,
        0,
        available,
        0,
        42,
        1,
        disposition,
    ];
    records.push(words(
        records.len() as u64 + 1,
        attempt,
        offset,
        frontier::BEGIN,
        &begin,
    ));
    if !payload.is_empty() {
        let mut data = words(records.len() as u64 + 1, attempt, offset, DATA, &[]);
        data.length = payload.len() as u32;
        data.bytes[..payload.len()].copy_from_slice(payload);
        records.push(data);
    }
    let consume = returned == 0 && disposition == CONSUME;
    let end = [
        23,
        order + u64::from(consume),
        offset,
        requested,
        payload.len() as u64,
        returned as u64,
        42,
        1,
        disposition,
        before,
        before + if consume { payload.len() as u64 } else { 0 },
    ];
    records.push(words(
        records.len() as u64 + 1,
        attempt,
        offset,
        frontier::FINISH,
        &end,
    ));
}
fn change(record: &mut Record, field: usize, value: u64) {
    record.bytes[field * 8..field * 8 + 8].copy_from_slice(&value.to_le_bytes());
}
fn parse(records: &[Record], maximum: u64, disposition: u64) -> io::Result<Prefix> {
    let authority = if disposition == OBSERVE {
        CopyAuthority::from_shape(22, 0x42).unwrap()
    } else {
        CopyAuthority::default()
    };
    let mut selected = selection(maximum);
    if disposition == OBSERVE {
        selected.address_length = 0x42;
        selected.fdput_flags = 0;
    }
    validate_prefix(&selected, records, false, authority, frontier::VERSION)
}
#[test]
fn copy5_interleaved_consume_joins_current_file_bytes_without_call_cursor() {
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 6, b"ab", 0, CONSUME);
    // Another authenticated consumer has removed [2,4). This Call resumes at
    // local output2, native frontier4. Its actual bytes are [4,6), not [6,8).
    attempt(&mut records, 2, 2, 4, 2, 2, 2, b"ef", 0, CONSUME);
    let prefix = parse(&records, 6, CONSUME).unwrap();
    assert_eq!(prefix.cursor, 4);
    assert_eq!(prefix.order, 3);
    assert_eq!(prefix.units.len(), 2);
    let second = prefix.units[1].observation.unwrap();
    assert_eq!(
        (second.begin.before, second.begin.start, second.after),
        (4, 4, 6)
    );
    assert_eq!(
        (
            second.begin_record,
            prefix.units[1].first,
            prefix.units[1].end
        ),
        (3, 4, 5)
    );
    let mut incorrect = records.clone();
    change(&mut incorrect[3], 2, 6);
    assert!(parse(&incorrect, 6, CONSUME).is_err());
    // A byte jump without a successful native-unit jump is not a valid join.
    let mut incorrect = records.clone();
    change(&mut incorrect[3], 3, 1);
    assert!(parse(&incorrect, 6, CONSUME).is_err());
}
#[test]
fn copy5_requested_extent_stored_bytes_and_fault_commit_remain_distinct() {
    let mut small = Vec::new();
    attempt(&mut small, 1, 0, 0, 0, 32, 64, &[b'a'; 32], 0, OBSERVE);
    let first = parse(&small, 32, OBSERVE).unwrap().units[0]
        .observation
        .unwrap();
    assert_eq!(
        (first.begin.requested, first.begin.available, first.after),
        (32, 64, 0)
    );
    let mut failed = Vec::new();
    attempt(&mut failed, 1, 0, 0, 0, 64, 64, &[b'a'; 32], -14, CONSUME);
    let prefix = parse(&failed, 64, CONSUME).unwrap();
    assert_eq!(prefix.copied, 32);
    assert_eq!(prefix.cursor, 0);
    assert_eq!(prefix.order, 0);
    assert_eq!(prefix.units[0].observation.unwrap().after, 0);
    let mut retry = Vec::new();
    attempt(&mut retry, 1, 0, 0, 0, 64, 64, &[b'a'; 64], 0, CONSUME);
    assert_eq!(
        parse(&retry, 64, CONSUME).unwrap().units[0]
            .observation
            .unwrap()
            .after,
        64
    );
    let mut prior = Vec::new();
    attempt(&mut prior, 1, 0, 0, 0, 2, 2, b"ab", 0, CONSUME);
    attempt(&mut prior, 2, 2, 2, 1, 4, 4, b"c", -14, CONSUME);
    let prefix = parse(&prior, 6, CONSUME).unwrap();
    assert_eq!((prefix.cursor, prefix.copied, prefix.order), (2, 3, 1));
    assert_eq!(prefix.units[1].observation.unwrap().after, 2);
}
#[test]
fn copy5_peek_traversal_is_locked_and_never_advances_native_bytes() {
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 4, 2, 2, 2, b"ab", 0, OBSERVE);
    attempt(&mut records, 2, 2, 4, 2, 2, 4, b"cd", 0, OBSERVE);
    let prefix = parse(&records, 4, OBSERVE).unwrap();
    assert_eq!((prefix.cursor, prefix.order), (4, 2));
    let second = prefix.units[1].observation.unwrap();
    assert_eq!(
        (second.begin.before, second.begin.start, second.after),
        (4, 6, 4)
    );
    let mut changed = records.clone();
    change(&mut changed[3], 1, 6);
    change(&mut changed[3], 2, 8);
    change(&mut changed[3], 3, 3);
    assert!(parse(&changed, 4, OBSERVE).is_err());
    // Unix object-relative position0 does not reset stable stream coordinates.
    let mut unix = Vec::new();
    attempt(&mut unix, 1, 0, 64, 8, 2, 4, b"xy", 0, CONSUME);
    change(&mut unix[0], 10, 0);
    change(&mut unix[0], 11, 2);
    change(&mut unix[2], 6, 0);
    change(&mut unix[2], 7, 2);
    let unit = parse(&unix, 2, CONSUME).unwrap().units[0]
        .observation
        .unwrap();
    assert_eq!(
        (unit.begin.position, unit.begin.start, unit.after),
        (0, 64, 66)
    );
}
#[test]
fn copy5_each_layout_or_frontier_mismatch_refuses_without_losing_prior_unit() {
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
    attempt(&mut records, 2, 2, 2, 1, 2, 4, b"cd", 0, CONSUME);
    let mutations = [
        (0, 0),
        (0, 24),
        (1, 1),
        (2, 3),
        (3, 0),
        (4, 1),
        (5, 0),
        (5, 5),
        (6, 1),
        (7, 5),
        (8, 3),
        (8, u64::from(u32::MAX) + 1),
        (9, 5),
        (10, u64::from(u32::MAX) + 1),
        (11, 3),
        (12, OBSERVE),
    ];
    for (field, value) in mutations {
        let mut changed = records.clone();
        change(&mut changed[3], field, value);
        let mut prefix = Prefix::default();
        assert!(
            prefix
                .advance(&selection(4), &changed, CopyAuthority::default(), 5)
                .is_err(),
            "Begin field {field}"
        );
        assert_eq!(prefix.units.len(), 1);
        assert_eq!(prefix.units[0].native.copied, 2);
    }
    for field in 0..11 {
        let mut changed = records.clone();
        let old = u64::from_le_bytes(
            changed[5].bytes[field * 8..field * 8 + 8]
                .try_into()
                .unwrap(),
        );
        change(&mut changed[5], field, old.wrapping_add(1));
        let mut prefix = Prefix::default();
        assert!(
            prefix
                .advance(&selection(4), &changed, CopyAuthority::default(), 5)
                .is_err(),
            "End field {field}"
        );
        assert_eq!(prefix.units.len(), 1);
        assert_eq!(prefix.units[0].observation.unwrap().after, 2);
        assert_eq!(changed.len(), 6); // caller's canonical raw store remains complete
    }
}
#[test]
fn copy4_and_copy5_grammars_do_not_negotiate_from_frames_or_late_manifest() {
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
    assert!(validate_prefix(&selection(4), &records, false, CopyAuthority::default(), 4).is_err());
    assert!(validate_prefix(&selection(4), &records, false, CopyAuthority::default(), 6).is_err());
    let mut legacy_end = records[2].clone();
    legacy_end.kind = UNIT;
    legacy_end.length = 72;
    legacy_end.bytes[72..].fill(0);
    let mut mixed = records.clone();
    mixed[2] = legacy_end;
    assert!(parse(&mixed, 4, CONSUME).is_err());
    for count in 0..=records.len() {
        let mut prefix = Prefix::default();
        prefix
            .advance(
                &selection(4),
                &records[..count],
                CopyAuthority::default(),
                5,
            )
            .unwrap();
        assert_eq!(prefix.processed, count);
        assert_eq!(
            prefix.require_closed(count).is_ok(),
            count == 0 || count == 3
        );
    }
    let mut raw = crate::network_runtime::accepted_provider_ffi::OriginalEffect::default();
    raw.command.operation = 11;
    raw.command.command = 7;
    raw.command.returned = 2;
    raw.command.phase = 1;
    raw.command.identity.provider = 3;
    raw.command.task = 13;
    raw.command.start_boottime = 17;
    raw.command.original_count = 4;
    raw.original.returned = 2;
    raw.original.complete = 1;
    let mut effect: OriginalEffect = raw.into();
    effect.original.selection = selection(4);
    let manifest = Manifest {
        provider: 3,
        command: 7,
        call: 11,
        task: 13,
        task_start: 17,
        present: 1,
        returned: 2,
        summary: Summary {
            version: 5,
            initial_count: 4,
            attempts: 1,
            records: 3,
            copied: 2,
            final_count: 2,
            protocol_returned: 2,
            protocol_complete: 1,
        },
    };
    effect.read_copy = Some(manifest);
    assert!(manifest.validate(&effect).is_err());
    assert!(decode(&effect, records.clone()).is_err());
    manifest.validate_for_version(&effect, 5).unwrap();
    assert!(manifest.validate_for_version(&effect, 6).is_err());
    assert_eq!(
        validate_records_for_version(&effect, &records, 5).unwrap(),
        manifest
    );
    let mut wrong = effect.clone();
    wrong.read_copy.as_mut().unwrap().summary.version = 4;
    assert!(validate_records_for_version(&wrong, &records, 5).is_err());
    let mut absent_begin = records[1..].to_vec();
    for (i, record) in absent_begin.iter_mut().enumerate() {
        record.sequence = i as u64 + 1;
    }
    assert!(parse(&absent_begin, 4, CONSUME).is_err());
    // A larger geometry does not permit more DATA than the requested attempt.
    let mut too_much = records.clone();
    too_much[1].length = 3;
    too_much[1].bytes[2] = b'c';
    assert!(parse(&too_much, 4, CONSUME).is_err());
}

fn owner() -> crate::network_replay::NetworkStreamOwner {
    let thread = crate::types::DetTid::from_raw(13);
    crate::network_replay::NetworkStreamOwner {
        thread,
        mm: crate::types::MmId::initial(thread),
    }
}
fn issued(selected: &OriginalSelection, version: u64) -> CopyWireAuthority {
    use crate::network_runtime::ProviderWireFormat;
    use crate::network_runtime::copy_wire_authority::controlled_copy_authority;
    let wire = match version {
        4 => ProviderWireFormat::Abi7Copy4,
        5 => ProviderWireFormat::Abi8Copy5,
        _ => panic!("fixture version"),
    };
    controlled_copy_authority(wire, owner(), 11, selected.clone()).unwrap()
}
fn completed_effect(
    selected: OriginalSelection,
    returned: i32,
    present: bool,
    summary: Summary,
) -> OriginalEffect {
    let mut raw = crate::network_runtime::accepted_provider_ffi::OriginalEffect::default();
    raw.command.operation = 11;
    raw.command.command = selected.command;
    raw.command.returned = returned;
    raw.command.phase = 1;
    raw.command.identity.provider = selected.provider;
    raw.command.task = selected.task;
    raw.command.start_boottime = selected.task_start;
    raw.command.original_count = selected.original_count;
    raw.original.returned = returned;
    raw.original.complete = 1;
    let mut effect: OriginalEffect = raw.into();
    effect.read_copy = Some(Manifest {
        provider: selected.provider,
        command: selected.command,
        call: selected.call,
        task: selected.task,
        task_start: selected.task_start,
        present: u64::from(present),
        returned: i64::from(returned),
        summary,
    });
    effect.original.selection = selected;
    effect
}
#[test]
fn copy5_incremental_parser_uses_actual_prepared_selection_before_first_delta() {
    let selected = selection(4);
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
    let mut legacy = StreamingPrefix::for_authority(issued(&selected, 4)).unwrap();
    assert!(legacy.advance(&selected, &records[..1], None).is_err());
    assert!(legacy.prefix.units.is_empty());
    assert!(legacy.refused);
    // No EXIT or manifest has arrived: the retained Controller issuer alone
    // chooses this grammar, and Begin itself supplies no completed delta.
    let mut current = StreamingPrefix::for_authority(issued(&selected, 5)).unwrap();
    for n in 1..=3 {
        current.advance(&selected, &records[..n], None).unwrap();
        assert_eq!(current.prefix.units.len(), usize::from(n == 3));
    }
    assert_eq!(current.prefix.units[0].observation.unwrap().after, 2);
    let mut changed = selected.clone();
    changed.file += 1;
    assert!(current.advance(&changed, &records, None).is_err());
    assert!(current.advance(&selected, &records, None).is_err());
    assert_eq!(current.prefix.units.len(), 1);
}
#[test]
fn prepared_no_file_read_requires_actual_empty_exit_and_cannot_gain_file_authority() {
    for version in [4, 5] {
        let mut selected = selection(4);
        selected.file = 0;
        selected.fdput_flags = 0;
        let effect = completed_effect(selected.clone(), -libc::EBADF, false, Summary::default());
        let mut current = StreamingPrefix::for_authority(issued(&selected, version)).unwrap();
        current
            .advance(&selected, &[], Some(End::OriginalExit { protocol: false }))
            .unwrap();
        let capture = current.collect(&effect, Vec::new()).unwrap();
        assert!(capture.records.is_empty() && capture.units.is_empty());
        assert_eq!(capture.manifest.present, 0);
        assert_eq!(capture.manifest.returned, -i64::from(libc::EBADF));
        // An empty buffer without the actual EXIT remains insufficient. The
        // attempted replacement cannot recover from this refusal.
        let mut no_exit = StreamingPrefix::for_authority(issued(&selected, version)).unwrap();
        no_exit.advance(&selected, &[], None).unwrap();
        assert!(no_exit.collect(&effect, Vec::new()).is_err());
        assert!(
            no_exit
                .advance(&selected, &[], Some(End::OriginalExit { protocol: false }))
                .is_err()
        );
        let mut raw = Vec::new();
        attempt(&mut raw, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
        let mut nonempty = StreamingPrefix::for_authority(issued(&selected, version)).unwrap();
        assert!(nonempty.advance(&selected, &raw, None).is_err());
        assert!(nonempty.prefix.units.is_empty());
        let mut changed = selected.clone();
        changed.file = 23;
        let mut changed_file = StreamingPrefix::for_authority(issued(&selected, version)).unwrap();
        assert!(changed_file.advance(&changed, &[], None).is_err());
        assert!(
            changed_file
                .advance(&selected, &[], Some(End::OriginalExit { protocol: false }))
                .is_err()
        );
    }
}
fn prepared_custody(selected: &OriginalSelection) -> std::sync::Arc<ReadCopyCustody> {
    use crate::network_runtime::ProviderWireFormat;
    use crate::network_runtime::copy_wire_authority::controlled_copy_authority_with_preparation;
    let custody = std::sync::Arc::new(ReadCopyCustody::default());
    let token = controlled_copy_authority_with_preparation(
        ProviderWireFormat::Abi8Copy5,
        owner(),
        11,
        selected.clone(),
        |prepared| {
            custody.retain_preparation(prepared)?;
            custody.take_preparation()
        },
    )
    .unwrap();
    custody.prepare(token).unwrap();
    let call = serde_json::from_value(serde_json::json!(selected.call)).unwrap();
    custody.bind(owner(), call, selected.command).unwrap();
    custody
}
#[test]
fn copy5_custody_retains_begin_data_end_and_prior_delta_after_malformed_suffix() {
    let selected = selection(4);
    let custody = prepared_custody(&selected);
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
    custody.append(&selected, 0, records.clone(), None).unwrap();
    let delta = custody.completed_since(0).unwrap().unwrap();
    delta
        .with_unit(0, |unit, raw| {
            assert_eq!(raw, records.as_slice());
            assert_eq!(raw[0].kind, frontier::BEGIN);
            assert_eq!(raw[2].kind, frontier::FINISH);
            assert_eq!(unit.observation.unwrap().after, 2);
        })
        .unwrap();
    let mut bad = Vec::new();
    attempt(&mut bad, 2, 2, 2, 1, 2, 4, b"cd", 0, CONSUME);
    for (index, record) in bad.iter_mut().enumerate() {
        record.sequence = 4 + index as u64;
    }
    change(&mut bad[2], 10, 5); // would falsely advance by3 after actual stores2
    assert!(custody.append(&selected, 3, bad.clone(), None).is_err());
    assert_eq!(custody.len().unwrap(), 6);
    assert!(custody.completed_since(1).unwrap().is_none());
    delta
        .with_unit(0, |_, raw| assert_eq!(raw, records.as_slice()))
        .unwrap();
    assert!(custody.require_no_unjoined_receipts(1).is_err());
    assert!(
        custody
            .append(&selected, 6, Vec::new(), Some(End::ThreadTerminal))
            .is_err()
    );
    assert_eq!(custody.len().unwrap(), 6);
}
#[test]
fn copy5_custody_cannot_replace_raw_first_missing_bound_authority_even_for_empty_prefix() {
    use crate::network_runtime::ProviderWireFormat;
    use crate::network_runtime::copy_wire_authority::controlled_copy_authority_with_preparation;
    for raw_count in [0, 1] {
        let selected = selection(4);
        let custody = ReadCopyCustody::default();
        let mut records = Vec::new();
        attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
        let token = controlled_copy_authority_with_preparation(
            ProviderWireFormat::Abi8Copy5,
            owner(),
            11,
            selected.clone(),
            |prepared| {
                custody.retain_preparation(prepared)?;
                let prepared = custody.take_preparation()?;
                assert!(
                    custody
                        .append(&selected, 0, records[..raw_count].to_vec(), None)
                        .is_err()
                );
                assert_eq!(custody.len()?, raw_count);
                Ok(prepared)
            },
        )
        .unwrap();
        // The actual subsequent Controller bind is valid; prior raw custody
        // refusal is still sticky and cannot be erased by installing it.
        assert!(custody.prepare(token).is_err());
        assert_eq!(custody.len().unwrap(), raw_count);
        assert!(custody.require_no_unjoined_receipts(0).is_err());
    }
}
#[test]
fn copy5_canonical_collection_keeps_exact_raw_arc_and_pending_semantic_obligation() {
    let selected = selection(4);
    let custody = prepared_custody(&selected);
    let mut records = Vec::new();
    attempt(&mut records, 1, 0, 0, 0, 2, 4, b"ab", 0, CONSUME);
    custody
        .append(
            &selected,
            0,
            records.clone(),
            Some(End::OriginalExit { protocol: true }),
        )
        .unwrap();
    let effect = completed_effect(
        selected,
        2,
        true,
        Summary {
            version: 5,
            initial_count: 4,
            attempts: 1,
            records: 3,
            copied: 2,
            final_count: 2,
            protocol_returned: 2,
            protocol_complete: 1,
        },
    );
    let first = custody.collect(&effect).unwrap();
    let again = custody.collect(&effect).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &again));
    assert_eq!(first.records, records);
    assert_eq!(first.committed, b"ab");
    assert_eq!(first.units[0].observation.unwrap().after, 2);
    assert!(custody.require_no_unjoined_receipts(1).is_err());
}
