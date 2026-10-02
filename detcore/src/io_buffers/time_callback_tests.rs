mod time_callback_tests {
    use detcore_model::network_trace::NetworkPolicy;

    use super::*;

    const SECONDS: i64 = 1_790_123_456;
    const FRACTION: u64 = 123_456_789;

    fn fixture(authenticated_record: bool) -> (Detcore, EventGuest) {
        let (mut tool, mut guest) = event_guest(FdType::Regular, None);
        guest.config.virtualize_time = true;
        guest.config.detlog_io_buffers = false;
        if authenticated_record {
            guest.config.network_trace.policy = NetworkPolicy::Record;
            // Controlled table-admission premise, using the same activation
            // fixture as other callback tests. The real Tool entry gate and
            // unchanged time handler below are not bypassed.
            *guest.thread.file_metadata.lock().unwrap() =
                crate::tool_local::FileMetadata::empty_network_fixture(guest.thread.dettid);
        }
        guest.clock_result = Some(crate::types::LogicalTime::from_nanos(
            SECONDS as u64 * 1_000_000_000 + FRACTION,
        ));
        tool.cfg = guest.config.clone();
        (tool, guest)
    }

    async fn check_time_callbacks(policy: Option<NetworkPolicy>) {
        for address in [None, Some(FIRST_DEST), Some(0x4000)] {
            let (mut tool, mut guest) = fixture(policy.is_some());
            if let Some(policy) = policy {
                guest.config.network_trace.policy = policy;
                tool.cfg = guest.config.clone();
            }
            assert_eq!(tool.network_fd_tracking_active(&guest), policy.is_some());
            let before = guest.memory.0.lock().unwrap().clone();
            let call =
                reverie::syscalls::Time::new().with_tloc(address.and_then(AddrMut::from_raw));
            let result = tool.handle_syscall_event(&mut guest, call.into()).await;
            if address == Some(0x4000) {
                assert!(
                    matches!(result, Err(Error::Errno(Errno::EFAULT))),
                    "unwritable tloc must retain EFAULT, got {result:?}"
                );
                assert_eq!(*guest.memory.0.lock().unwrap(), before);
            } else {
                assert!(
                    matches!(result, Ok(SECONDS)),
                    "time({address:?}) must reach the deterministic callback, got {result:?}"
                );
                let mut expected = before;
                if let Some(address) = address {
                    expected[address..address + 8].copy_from_slice(&SECONDS.to_ne_bytes());
                }
                assert_eq!(*guest.memory.0.lock().unwrap(), expected);
            }
            assert_eq!(*guest.clock_observations.lock().unwrap(), 1);
            assert_eq!(guest.thread.stats.syscall_count, 1);
            assert!(guest.injected_iovecs.is_empty());
            assert_eq!(guest.injected_zero_reads, 0);
            assert_eq!(*guest.releases.lock().unwrap(), 0);
            assert!(guest.thread.original_connect.is_none());
            assert!(
                guest
                    .thread
                    .file_metadata
                    .lock()
                    .unwrap()
                    .pending_network_installations()
                    .is_empty()
            );
            // EventGuest rejects every attempted native time injection and
            // unexpected RPC. This is a Tool-callback component test, not a
            // native provider/bootstrap or full clock-determinism claim.
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_record_time_callback_null_output_and_fault() {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            check_time_callbacks(Some(NetworkPolicy::Record)),
        )
        .await
        .expect("time callback exceeded existing component bound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ordinary_time_callback_keeps_null_output_and_fault() {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            check_time_callbacks(None),
        )
        .await
        .expect("time callback exceeded existing component bound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_replay_time_callback_null_output_and_fault() {
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            check_time_callbacks(Some(NetworkPolicy::Replay)),
        )
        .await
        .expect("time callback exceeded existing component bound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_record_time_without_virtualization_never_falls_back() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for (policy, panic_on_unsupported) in [
                (NetworkPolicy::Record, true),
                (NetworkPolicy::Record, false),
                (NetworkPolicy::Replay, true),
                (NetworkPolicy::Replay, false),
            ] {
                for address in [None, Some(FIRST_DEST), Some(0x4000)] {
                    let (mut tool, mut guest) = fixture(true);
                    guest.config.network_trace.policy = policy;
                    guest.config.virtualize_time = false;
                    guest.config.panic_on_unsupported_syscalls = panic_on_unsupported;
                    tool.cfg = guest.config.clone();
                    let before = guest.memory.0.lock().unwrap().clone();
                    let call = reverie::syscalls::Time::new()
                        .with_tloc(address.and_then(AddrMut::from_raw));
                    let error = tool
                        .handle_syscall_event(&mut guest, call.into())
                        .await
                        .unwrap_err();
                    let Error::Tool(error) = error else {
                        panic!("nonvirtualized time must retain the authenticated-route refusal");
                    };
                    assert_eq!(
                        error.to_string(),
                        "authenticated Record route has no original effect join for time"
                    );
                    assert_eq!(*guest.memory.0.lock().unwrap(), before);
                    assert_eq!(*guest.clock_observations.lock().unwrap(), 0);
                    assert_eq!(guest.thread.stats.syscall_count, 0);
                    assert!(guest.injected_iovecs.is_empty());
                    assert_eq!(guest.injected_zero_reads, 0);
                }
            }
        })
        .await
        .expect("nonvirtualized callback exceeded existing component bound");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_record_time_neighbor_clock_settime_stays_refused() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let (tool, mut guest) = fixture(true);
            let before = guest.memory.0.lock().unwrap().clone();
            let call = Syscall::from_raw(
                reverie::syscalls::Sysno::clock_settime,
                reverie::syscalls::SyscallArgs::new(0, FIRST_DEST, 0, 0, 0, 0),
            );
            let error = tool
                .handle_syscall_event(&mut guest, call)
                .await
                .unwrap_err();
            let Error::Tool(error) = error else {
                panic!("unjoined host clock mutation became a guest result");
            };
            assert_eq!(
                error.to_string(),
                "authenticated Record route has no original effect join for clock_settime"
            );
            assert_eq!(*guest.memory.0.lock().unwrap(), before);
            assert_eq!(*guest.clock_observations.lock().unwrap(), 0);
            assert_eq!(guest.thread.stats.syscall_count, 0);
            assert!(guest.injected_iovecs.is_empty());
            assert_eq!(guest.injected_zero_reads, 0);
        })
        .await
        .expect("negative callback exceeded existing component bound");
    }
}
