//! Controlled census/descriptor premises; actual selected grant, reader/Call,
//! prefix borrow and entry marker. No native Connect/provider result is forged.
use super::*;
use crate::network_replay::original_connect::Arguments;
use crate::network_replay::original_connect::Kind;
use crate::resources::ExternalOpId;
use crate::scheduler::Scheduler;

#[tokio::test]
async fn shared_initial_entry_is_selected_original_connect_and_one_use() {
    for fault in 0..4 {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let (runtime, root, _metadata, _memory, claim) =
            crate::network_runtime::controlled_foreground_runtime(raw);
        let owner = root.owner();
        let mut scheduler = Scheduler::new(&crate::config::Config::default());
        scheduler.controlled_foreground_store_grant(&root);
        let initial = scheduler.shared_initial_projection(&root).unwrap();
        let operation = ExternalOpId::new(owner.thread, 10);
        scheduler.controlled_selected_network_capture(owner, operation);
        let grant = scheduler
            .native_capture_entry_observation(owner, operation, &root)
            .unwrap();
        let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
        let epoch = chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let mut engine = NetworkReplayEngine::record_shared_mm_attempts(epoch);
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), &claim, owner.thread)
            .unwrap();
        let NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(owner, root.files(), 9999).unwrap()
        else {
            panic!("fresh controlled table needs no recovery");
        };
        let read = engine
            .bind_fd_read_external_grant(owner, *read, operation)
            .unwrap();
        let admission = engine
            .begin_original_external_from_read(
                owner,
                Arguments {
                    kind: Kind::Connect,
                    operation,
                    files: root.files(),
                    binding: read.binding,
                    fd: 9999,
                    address: 0x2000,
                    length: 16,
                    original_count: 0,
                },
                read,
            )
            .unwrap();
        assert!(
            engine
                .begin_native_entry_stamp(owner, admission.call)
                .is_err(),
            "legacy issuer must still refuse new policy"
        );
        let mut attempt = engine
            .begin_shared_initial_entry_stamp(owner, admission.call)
            .unwrap();
        let retained = attempt.retain_unsubmitted_recovery(&prefix).unwrap();
        if fault == 1 {
            engine
                .stream_calls
                .get_mut(&admission.call)
                .unwrap()
                .abandoned = true;
        }
        if fault == 2 {
            runtime.revoke_foreground_lineage();
        }
        let now = if fault == 3 {
            LogicalTime::from_nanos(1)
        } else {
            LogicalTime::from_nanos(1_790_000_000_000_000_000)
        };
        let result = runtime.with_foreground_prefix(&prefix, |borrow| {
            engine
                .stamp_shared_initial_connect_entry(attempt, borrow, &grant, initial.clone(), now)
                .map_err(std::io::Error::other)
        });
        assert_eq!(result.is_ok(), fault == 0);
        assert!(
            engine
                .begin_shared_initial_entry_stamp(owner, admission.call)
                .is_err()
        );
        assert!(
            engine.stream_calls.contains_key(&admission.call),
            "refusal cannot erase original custody"
        );
        if fault == 0 {
            let entry = engine.stream_calls[&admission.call]
                .native_entry
                .as_ref()
                .unwrap();
            assert_eq!(entry.kind, EntryKind::SharedInitialConnect { operation });
            assert_eq!(entry.release.receive_entry_cut.0, 0);
            assert!(entry.release.prerequisites.is_empty());
            assert!(!entry.used);
            let (saved, projection) = engine.shared_initial_origin().unwrap();
            assert!(Arc::ptr_eq(&saved, &root));
            assert!(Arc::ptr_eq(&projection, &initial));
            assert!(
                engine.check_shared_initial_finalization().is_err(),
                "live origin is not final wait"
            );
        } else {
            assert!(engine.shared_initial_origin().is_none());
            assert!(engine.stream_calls[&admission.call].native_entry.is_none());
        }
        drop(retained);
    }
}

#[tokio::test]
async fn shared_replay_origin_requires_actual_initial_grant_and_is_one_use() {
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let (runtime, root, _metadata, _memory, _) =
        crate::network_runtime::controlled_foreground_runtime(raw);
    let mut scheduler = Scheduler::new(&crate::config::Config::default());
    scheduler.controlled_foreground_store_grant(&root);
    let initial = scheduler.shared_initial_projection(&root).unwrap();
    let operation = ExternalOpId::new(root.owner().thread, 10);
    scheduler.controlled_selected_network_capture(root.owner(), operation);
    let grant = scheduler
        .native_capture_entry_observation(root.owner(), operation, &root)
        .unwrap();
    let record = NetworkReplayEngine::record_shared_mm_attempts(
        chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
    );
    let EngineState::Native(native) = record.mode else {
        unreachable!();
    };
    let mut replay = NetworkReplayEngine::replay_shared_mm_attempts(native.trace).unwrap();
    replay
        .bind_shared_initial_replay_origin(root.clone(), initial.clone(), &grant)
        .unwrap();
    assert!(
        replay.stream_calls.is_empty(),
        "origin cannot fabricate a logical Connect Call"
    );
    assert!(
        replay
            .bind_shared_initial_replay_origin(root.clone(), initial, &grant)
            .is_err()
    );
    assert!(replay.check_shared_initial_finalization().is_err());
    runtime.revoke_foreground_lineage();
    assert!(replay.check_native_retirement().is_err());
}
