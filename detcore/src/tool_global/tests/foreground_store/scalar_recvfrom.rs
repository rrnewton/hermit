//! Uses the existing actual Detcore dispatcher/RPC/store fixture. Its backend
//! range verdict and initial provider/root setup remain controlled premises;
//! this is not a native ptrace entry or an end-to-end network qualification.
use reverie::syscalls::Recvfrom;
use reverie::syscalls::SyscallInfo;

use super::*;

fn scalar_recvfrom(fd: i32, destination: u64, capacity: usize) -> Recvfrom {
    Recvfrom::new()
        .with_fd(fd)
        .with_buf(reverie::syscalls::AddrMut::from_raw(destination as usize))
        .with_len(capacity)
}

#[tokio::test]
async fn guest_v4_recvfrom_replay_keeps_original_capacity_and_does_not_touch_faulting_tail() {
    let mut repeated = Vec::new();
    for _ in 0..2 {
        let (trace, _) = scalar_eof_trace(true, false, false);
        let (f, _) = ReplayIssuerFixture::new_trace(false, trace, true).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(f.root.owner(), f.read)
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let before_turn = f.state.sched.lock().unwrap().turn;
        let before_clock = f.state.global_time.lock().unwrap().as_nanos();
        // The available eight-byte prefix ends at the first page boundary.
        // The rest of the original 102400-byte capacity is not a store request.
        let destination = f.pages.at(4088);
        assert!(
            f.thread
                .memory_metadata
                .lock()
                .unwrap()
                .original_copy_span(f.root.owner(), destination, 102_400)
                .is_err()
        );
        assert!(
            f.thread
                .memory_metadata
                .lock()
                .unwrap()
                .original_copy_span(f.root.owner(), destination, 8)
                .is_ok()
        );
        f.pages.protect_second(libc::PROT_NONE);
        let call = scalar_recvfrom(f.binding.slot.fd, destination, 102_400);
        let original = call.into_parts();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread, f.tid);
        let result = tool.handle_network_io(&mut guest, call.into()).await;
        f.pages.protect_second(libc::PROT_READ | libc::PROT_WRITE);

        // Confirm the original common release before accepting the result.
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_scalar_no_store_rpc(&guest);
        assert_replay_release_responses(&guest, &[vec![]]);
        assert!(guest.thread.original_connect.is_none());
        assert!(guest.thread.original_file_metadata.is_none());
        assert_eq!(result.unwrap(), 8);
        assert!(
            guest.range_calls.lock().unwrap().is_empty(),
            "the backend must never be asked to authenticate a fabricated Read"
        );
        assert_eq!(*guest.recvfrom_range_calls.lock().unwrap(), [original]);
        assert_eq!(original.0, Sysno::recvfrom);
        assert_eq!(original.1.arg2, 102_400);
        assert_eq!(f.pages.bytes(0, 4088), vec![0xa5; 4088]);
        assert_eq!(f.pages.bytes(4088, 8), b"abcdefgh");
        assert_eq!(f.pages.bytes(4096, 4096), vec![0xa5; 4096]);
        assert_eq!(
            *guest.memory_events.lock().unwrap(),
            ["access-check", "native-write"]
        );
        let after = engine
            .lock()
            .unwrap()
            .replay_no_store_fixture_state(f.binding.open_file);
        assert_eq!(after.consumed, 8);
        assert!(after.bytes.is_empty());
        assert_eq!(after.eof_offsets, [8]);
        assert!(!after.peer_closed);
        assert_eq!(after.consume_epoch, 1);
        assert!(after.consumed_eof.is_empty());
        assert_eq!(after.completed, [0, 1, 2, 3]);
        assert_eq!(after.released, [true, true, true, true]);
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
        repeated.push((after, before_turn, before_clock));
        assert!(
            engine.lock().unwrap().finish().is_err(),
            "the released EOF still requires a distinct no-store consumption"
        );
    }
    assert_eq!(repeated[0], repeated[1]);
}

#[tokio::test]
async fn guest_v4_recvfrom_original_range_fault_releases_reader_before_record_or_replay_effects() {
    for record in [false, true] {
        let f = ReplayIssuerFixture::new_mode(record).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(f.root.owner(), f.read)
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let before_turn = f.state.sched.lock().unwrap().turn;
        let before_clock = f.state.global_time.lock().unwrap().as_nanos();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread, f.tid);
        guest.range_verdict = reverie::OriginalReadRangeVerdict::Fault;
        let call = scalar_recvfrom(f.binding.slot.fd, 0xffff_ffff_ffff_f000, 102_400);
        let original = call.into_parts();
        let result = tool.handle_network_io(&mut guest, call.into()).await;

        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(
            scalar_foreground_requests(&guest),
            [
                "publication-acquire",
                "publication-empty-release",
                "ordinary-reader",
                "socket-state",
                "reader-finish",
            ]
        );
        assert!(
            matches!(result, Err(reverie::Error::Errno(Errno::EFAULT))),
            "{result:?}"
        );
        assert!(guest.range_calls.lock().unwrap().is_empty());
        assert_eq!(*guest.recvfrom_range_calls.lock().unwrap(), [original]);
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), before_clock);
    }
}

#[tokio::test]
async fn guest_v4_recvfrom_unsupported_outputs_and_flags_refuse_before_acquisition() {
    for record in [false, true] {
        let f = ReplayIssuerFixture::new_mode(record).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        engine
            .lock()
            .unwrap()
            .finish_fd_read(f.root.owner(), f.read)
            .unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread, f.tid);
        let call = scalar_recvfrom(f.binding.slot.fd, f.pages.at(128), 102_400);
        let (_, raw) = call.into_parts();
        for changed in [
            reverie::syscalls::SyscallArgs {
                arg3: libc::MSG_PEEK as usize,
                ..raw
            },
            reverie::syscalls::SyscallArgs {
                arg3: libc::MSG_DONTWAIT as usize,
                ..raw
            },
            reverie::syscalls::SyscallArgs {
                arg3: libc::MSG_WAITALL as usize,
                ..raw
            },
            reverie::syscalls::SyscallArgs {
                arg3: 1usize << 32,
                ..raw
            },
            reverie::syscalls::SyscallArgs {
                arg4: f.pages.at(1024) as usize,
                ..raw
            },
            reverie::syscalls::SyscallArgs {
                arg5: f.pages.at(2048) as usize,
                ..raw
            },
        ] {
            let result = tool
                .handle_network_io(&mut guest, Recvfrom::from(changed).into())
                .await;
            let reverie::Error::Tool(error) = result.unwrap_err() else {
                panic!("unsupported shape must be an explicit capability refusal");
            };
            assert_eq!(
                error.to_string(),
                "V4 scalar recvfrom requires flags=0 and no source-address outputs"
            );
        }
        assert!(guest.requests.lock().unwrap().is_empty());
        assert!(guest.responses.lock().unwrap().is_empty());
        assert!(guest.range_calls.lock().unwrap().is_empty());
        assert!(guest.recvfrom_range_calls.lock().unwrap().is_empty());
        assert!(guest.memory_events.lock().unwrap().is_empty());
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    }
}

#[tokio::test]
async fn guest_v4_recvfrom_saved_policy_refuses_read_and_changed_full_capacity() {
    use reverie::syscalls::Syscall;
    use reverie::syscalls::SyscallArgs;

    use crate::tool_global::CheckedReadRange;
    use crate::tool_global::ScalarReceive;

    let f = ReplayIssuerFixture::new().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let call = scalar_recvfrom(f.binding.slot.fd, f.pages.at(128), 102_400);
    let original = ScalarReceive::from_syscall(call.into()).unwrap();
    let (_, raw) = original.into_parts();
    let metadata = f
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&f.read)
        .unwrap();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread, f.tid);
    let checked = CheckedReadRange::inspect(&guest, original, &f.read, metadata).unwrap();
    let admitted = f
        .state
        .begin_replay_receive_call(
            f.tid,
            &guest.thread,
            f.read,
            original.destination(),
            original.selected_maximum(),
        )
        .unwrap_or_else(|failure| panic!("admission failed: {:?}", failure.primary()));
    f.state
        .bind_saved_receive_policy(&checked, f.tid, &guest.thread, original, admitted)
        .unwrap();
    let nonblocking = metadata.nonblocking.unwrap();
    let policy = f
        .state
        .saved_receive_policy(f.tid, &guest.thread, original, admitted, nonblocking)
        .unwrap()
        .unwrap();
    assert!(policy.matches_span(512, original.destination()));
    assert!(!policy.matches_span(102_400, original.destination()));
    let mut refusals = Vec::new();
    for (number, arguments) in [
        (Sysno::read, raw),
        (
            Sysno::recvfrom,
            SyscallArgs {
                arg0: raw.arg0 + 1,
                ..raw
            },
        ),
        (
            Sysno::recvfrom,
            SyscallArgs {
                arg1: raw.arg1 + 1,
                ..raw
            },
        ),
        // Both capacities derive the same 512 maximum; original identity must
        // nevertheless distinguish them before any selection or publication.
        (Sysno::recvfrom, SyscallArgs { arg2: 512, ..raw }),
        (
            Sysno::recvfrom,
            SyscallArgs {
                arg2: 102_401,
                ..raw
            },
        ),
    ] {
        let changed = ScalarReceive::from_syscall(Syscall::from_raw(number, arguments)).unwrap();
        refusals.push(f.state.saved_receive_policy(
            f.tid,
            &guest.thread,
            changed,
            admitted,
            nonblocking,
        ));
    }
    let unchanged = f
        .state
        .saved_receive_policy(f.tid, &guest.thread, original, admitted, nonblocking)
        .unwrap()
        .unwrap();
    let same_policy = Arc::ptr_eq(&policy, &unchanged);
    let release = tool
        .foreground_v4_receive_after_probe(
            &mut guest,
            original,
            admitted,
            (
                crate::network_replay::NetworkEngineMode::Replay,
                nonblocking,
            ),
            Err(reverie::Error::Tool(anyhow::anyhow!(
                "controlled stop before selection"
            ))),
            None,
        )
        .await;

    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    assert_eq!(
        runtime.private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_exact_call_release(&guest, admitted.id);
    assert_eq!(
        release.unwrap_err().to_string(),
        "controlled stop before selection"
    );
    assert!(same_policy);
    assert_eq!(refusals.len(), 5);
    for refused in refusals {
        assert_eq!(
            refused.unwrap_err(),
            NetworkRpcError::internal("saved receive policy changed original Read")
        );
    }
    assert!(guest.range_calls.lock().unwrap().is_empty());
    assert_eq!(
        *guest.recvfrom_range_calls.lock().unwrap(),
        [original.into_parts()]
    );
    assert!(guest.memory_events.lock().unwrap().is_empty());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
}
