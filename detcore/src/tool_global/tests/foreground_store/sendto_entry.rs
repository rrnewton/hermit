//! Actual Tool entry and existing local RPCs; table/root/trace premises are
//! controlled. No native Sendto, source worker or provider success is claimed.
use detcore_model::network_trace::*;
use reverie::Error;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Sendto;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;

use super::*;

fn send(fd: i32, address: u64, length: usize, flags: i32) -> Sendto {
    Sendto::new()
        .with_fd(fd)
        .with_buf(AddrMut::from_raw(address as usize))
        .with_size(length)
        .with_flags(flags as u32)
}

fn output_trace(errno: Option<i32>) -> NetworkTraceV4 {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let channel = trace.channels[0].id;
    trace.outputs.push(NetworkOutputEventV2 {
        channel,
        event: match errno {
            Some(errno) => NetworkOutputKindV2::SocketError {
                stream_offset: 0,
                errno,
            },
            None => NetworkOutputKindV2::StreamBytes {
                stream_offset: 0,
                bytes: b"abc".to_vec(),
            },
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(nodes.len() as u64),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel,
            milestone: if errno.is_some() {
                NetworkProgressV4::OutputError { output_ordinal: 0 }
            } else {
                NetworkProgressV4::StreamPrefix {
                    exclusive_offset: 3,
                }
            },
        },
        prerequisites: vec![NetworkReleaseNodeIdV4(1)],
    });
    trace.validate().unwrap();
    trace
}

async fn fixture(record: bool, errno: Option<i32>) -> ReplayIssuerFixture {
    let (mut f, _) = ReplayIssuerFixture::new_trace(record, output_trace(errno), true).await;
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    // This component has no PMU or physical register backend. Ordinary syscall
    // count/time accounting remains enabled and is asserted below.
    f.config.max_timeslice = None;
    f.config.syscall_clobbers_virtualized_by_backend = true;
    f.thread.end_of_timeslice = None;
    f.thread.max_timeslice_end = None;
    f.thread
        .with_detfd(f.binding.slot.fd, |fd| fd.set_nonblocking(true))
        .unwrap();
    f
}

fn assert_replay_reader_released(f: &ReplayIssuerFixture, guest: &OwnedReadGuest<'_>) {
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    let requests = guest.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(
                request,
                GlobalRequest::Network(NetworkRequest::BeginFdRead { .. })
            ))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| matches!(
                request,
                GlobalRequest::Network(NetworkRequest::FinishFdRead { .. })
            ))
            .count(),
        1
    );
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
}

// These replies are controlled backend premises. The real Tool callback,
// source preparation/revalidation, worker join and engine consumption run;
// this does not establish native SourceStop acquisition or ptrace custody.
async fn controlled_source_callback(case: usize) {
    use reverie::syscalls::NativeUserReadError;
    use reverie::syscalls::NativeUserReadRefusal;

    for flags in [libc::MSG_NOSIGNAL, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT] {
        let mut f = fixture(false, None).await;
        let engine = f.state.network_engine.as_ref().unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let before_turn = f.state.sched.lock().unwrap().turn;
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
        let mut guest = owned_read_guest(&f.config, &f.state, thread);
        guest.expose_local_global = true;
        guest.forbid_ordinary_memory = true;
        guest.native_source_reply = Some(match case {
            0 => Ok(b"abc".to_vec()),
            1 => Err(NativeUserReadError::Refused(
                NativeUserReadRefusal::UnsupportedBackend,
            )),
            2 => Ok(b"bad".to_vec()),
            _ => unreachable!(),
        });
        let before_count = guest.thread.stats.syscall_count;
        let before_time = guest.thread.thread_logical_time.as_nanos();
        // Deliberately inaccessible numeric source. The selected three bytes
        // end at a page boundary; the unselected fourth byte must not enter
        // the source API or cause a cross-page range refusal.
        let result = tool
            .handle_syscall_event(&mut guest, send(f.binding.slot.fd, 4093, 4, flags).into())
            .await;
        assert_replay_reader_released(&f, &guest);
        assert_eq!(guest.native_source_calls, [(4093, 3)]);
        assert_eq!(
            *guest.native_source_events.lock().unwrap(),
            ["submitted", "worker", "joined", "retired"]
        );
        assert!(guest.native_source_reply.is_none());
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
        assert_eq!(guest.thread.stats.syscall_count, before_count + 1);
        assert_eq!(
            guest.thread.thread_logical_time.as_nanos(),
            before_time + LogicalTime::from_nanos(crate::syscall_time::cost_ns(Sysno::sendto))
        );
        assert!(
            !guest
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| matches!(
                    request,
                    GlobalRequest::Network(NetworkRequest::TransmitStream { .. })
                ))
        );
        if case == 0 {
            assert_eq!(result.unwrap(), 3);
            assert!(
                engine
                    .lock()
                    .unwrap()
                    .transmit_stream_read_limit(f.binding.open_file, 4)
                    .is_err()
            );
        } else {
            let Error::Tool(error) = result.unwrap_err() else {
                panic!("controlled source failure must remain a Tool error")
            };
            if case == 1 {
                assert_eq!(
                    error.to_string(),
                    "shared network engine refused operation: V4 Sendto Replay positive prefix requires source-stop/MM-bound read custody"
                );
            } else {
                let refusal = error
                    .downcast_ref::<crate::network_failure::NetworkPolicyRefusal>()
                    .expect("mismatched bytes must preserve the typed Replay refusal");
                assert_eq!(
                    refusal.reason(),
                    crate::network_failure::NetworkRefusalReason::OutboundMismatch
                );
            }
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .transmit_stream_read_limit(f.binding.open_file, 4)
                    .unwrap(),
                3
            );
        }
    }
}

#[tokio::test]
async fn sendto_tool_replay_joined_source_commits_only_selected_partial_prefix() {
    controlled_source_callback(0).await;
}

#[tokio::test]
async fn sendto_tool_replay_joined_source_refusal_releases_without_consumption() {
    controlled_source_callback(1).await;
}

#[tokio::test]
async fn sendto_tool_replay_joined_source_mismatch_releases_without_consumption() {
    controlled_source_callback(2).await;
}

#[tokio::test]
async fn sendto_tool_replay_zero_prefix_errno_never_reads_payload() {
    for flags in [libc::MSG_NOSIGNAL, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT] {
        for errno in [libc::EAGAIN, libc::EFAULT] {
            let mut f = fixture(false, Some(errno)).await;
            let engine = f.state.network_engine.as_ref().unwrap();
            let before_trace = engine.lock().unwrap().native_trace_fixture();
            let before_turn = f.state.sched.lock().unwrap().turn;
            let tool: Detcore = Detcore::new(f.tid, &f.config);
            let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
            let mut guest = owned_read_guest(&f.config, &f.state, thread);
            guest.expose_local_global = true;
            let before_time = guest.thread.thread_logical_time.as_nanos();
            let before_count = guest.thread.stats.syscall_count;
            // A selected errno owns no source bytes. Even an inaccessible
            // payload must not replace the retained errno with a memory fault.
            let result = tool
                .handle_syscall_event(&mut guest, send(f.binding.slot.fd, 1, 512, flags).into())
                .await;
            assert_replay_reader_released(&f, &guest);
            assert!(
                matches!(result, Err(Error::Errno(e)) if e.into_raw() == errno),
                "{result:?}"
            );
            let requests = guest.requests.lock().unwrap();
            let outputs: Vec<_> = requests
                .iter()
                .filter_map(|request| match request {
                    GlobalRequest::Network(NetworkRequest::TransmitStream { open_file, bytes }) => {
                        Some((*open_file, bytes.clone()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(outputs, [(f.binding.open_file, vec![])]);
            assert!(
                engine
                    .lock()
                    .unwrap()
                    .transmit_stream_read_limit(f.binding.open_file, 512)
                    .is_err()
            );
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
            assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
            assert_eq!(guest.thread.stats.syscall_count, before_count + 1);
            assert_eq!(
                guest.thread.thread_logical_time.as_nanos(),
                before_time + LogicalTime::from_nanos(crate::syscall_time::cost_ns(Sysno::sendto))
            );
        }
    }
}

#[tokio::test]
async fn sendto_tool_replay_positive_prefix_refuses_without_source_custody() {
    for flags in [libc::MSG_NOSIGNAL, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT] {
        for address in [b"abc!".as_ptr() as u64, b"bad!".as_ptr() as u64, 1] {
            let mut f = fixture(false, None).await;
            let engine = f.state.network_engine.as_ref().unwrap();
            let before_trace = engine.lock().unwrap().native_trace_fixture();
            let before_turn = f.state.sched.lock().unwrap().turn;
            let tool: Detcore = Detcore::new(f.tid, &f.config);
            let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
            let mut guest = owned_read_guest(&f.config, &f.state, thread);
            guest.expose_local_global = true;
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .transmit_stream_read_limit(f.binding.open_file, 4)
                    .unwrap(),
                3
            );
            let result = tool
                .handle_syscall_event(
                    &mut guest,
                    send(f.binding.slot.fd, address, 4, flags).into(),
                )
                .await;
            assert_replay_reader_released(&f, &guest);
            let Error::Tool(error) = result.unwrap_err() else {
                panic!("custody refusal must be a Tool error")
            };
            assert_eq!(
                error.to_string(),
                "shared network engine refused operation: V4 Sendto Replay positive prefix requires source-stop/MM-bound read custody"
            );
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .transmit_stream_read_limit(f.binding.open_file, 4)
                    .unwrap(),
                3
            );
            assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
            assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
            assert!(
                !guest
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|request| matches!(
                        request,
                        GlobalRequest::Network(NetworkRequest::TransmitStream { .. })
                    ))
            );
        }
    }
}

#[tokio::test]
async fn sendto_tool_record_reaches_authentication_without_native_submission() {
    for flags in [libc::MSG_NOSIGNAL, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT] {
        let mut f = fixture(true, None).await;
        // Remove actual runtime authority, not the entry classification. The
        // real original-send admission must refuse before creating any Call.
        let runtime = f.state.network_runtime.take().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        let engine = f.state.network_engine.as_ref().unwrap();
        let before_trace = engine.lock().unwrap().native_trace_fixture();
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
        let mut guest = owned_read_guest(&f.config, &f.state, thread);
        guest.expose_local_global = true;
        let call = send(f.binding.slot.fd, f.pages.at(0), 3, flags);
        let mut callback = Box::pin(tool.handle_syscall_event(&mut guest, call.into()));
        let returned = tokio::select! {
            result = &mut callback => Some(result),
            () = f.state.wait_for_backend_failure() => None,
        };
        drop(callback);
        assert!(
            returned.is_none(),
            "the existing retained failure owner must contain admission refusal: {returned:?}"
        );
        let requests = guest.requests.lock().unwrap();
        let entries: Vec<_> = requests
            .iter()
            .filter_map(|request| match request {
                GlobalRequest::Network(NetworkRequest::NativeBeginOriginalConnect {
                    arguments,
                }) => Some(arguments),
                _ => None,
            })
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].kind,
            crate::network_replay::original_connect::Kind::Sendto
        );
        assert_eq!(entries[0].original_count, 3);
        assert!(requests.iter().any(|request| matches!(request,
            GlobalRequest::Network(NetworkRequest::NativeOriginalConnectFailed { detail, .. })
                if detail == "shared network engine refused operation: Sendto runtime absent")));
        assert!(!requests.iter().any(|request| matches!(
            request,
            GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect { .. })
        )));
        let local = guest.thread.original_connect.as_ref().unwrap();
        assert!(local.admission.is_none() && !local.invoked && local.returned.is_none());
        assert!(f.state.sched.lock().unwrap().backend_failed());
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 0, 0, 0)
        );
        assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
        assert_eq!(
            runtime.private_publication_runtime_fixture_state(),
            before_runtime
        );
        // This is cancellation of a component callback with no submitted
        // native operation. It is not an original-task final-wait receipt.
    }
}

#[tokio::test]
async fn sendto_tool_replay_missing_local_authority_never_falls_back() {
    let mut f = fixture(false, Some(libc::EAGAIN)).await;
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
    let mut guest = owned_read_guest(&f.config, &f.state, thread);
    assert!(!guest.expose_local_global);
    let result = tool
        .handle_syscall_event(
            &mut guest,
            send(f.binding.slot.fd, f.pages.at(0), 3, libc::MSG_NOSIGNAL).into(),
        )
        .await;
    let Error::Tool(error) = result.unwrap_err() else {
        panic!("missing authority must refuse")
    };
    assert_eq!(
        error.to_string(),
        "shared network engine refused operation: tracked Sendto Replay requires actual local V4 authority"
    );
    assert!(guest.requests.lock().unwrap().is_empty());
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .transmit_stream_read_limit(f.binding.open_file, 3)
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn sendto_tool_unsupported_shapes_remain_at_entry_gate() {
    for record in [false, true] {
        let mut f = fixture(record, None).await;
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
        let mut guest = owned_read_guest(&f.config, &f.state, thread);
        guest.expose_local_global = true;
        let fd = f.binding.slot.fd;
        let address = f.pages.at(0);
        let good = send(fd, address, 3, libc::MSG_NOSIGNAL);
        let (_, args) = Syscall::from(good).into_parts();
        let mut calls = vec![
            send(fd, address, 0, libc::MSG_NOSIGNAL).into(),
            send(fd, address, 513, libc::MSG_NOSIGNAL).into(),
            send(fd, 0, 3, libc::MSG_NOSIGNAL).into(),
            send(fd, address, 3, 0).into(),
            send(fd, address, 3, libc::MSG_NOSIGNAL | libc::MSG_MORE).into(),
            Syscall::from_raw(
                Sysno::sendto,
                SyscallArgs::new(args.arg0, args.arg1, args.arg2, args.arg3, 1, 0),
            ),
            Syscall::from_raw(
                Sysno::sendto,
                SyscallArgs::new(args.arg0, args.arg1, args.arg2, args.arg3, 0, 1),
            ),
        ];
        for number in [Sysno::writev, Sysno::sendmsg, Sysno::sendmmsg] {
            calls.push(Syscall::from_raw(
                number,
                SyscallArgs::new(fd as usize, 0, 0, 0, 0, 0),
            ));
        }
        let before_count = guest.thread.stats.syscall_count;
        for call in calls {
            let result = tool.handle_syscall_event(&mut guest, call).await;
            let Error::Tool(error) = result.unwrap_err() else {
                panic!("unsupported shape must refuse")
            };
            assert_eq!(
                error.to_string(),
                format!(
                    "authenticated Record route has no original effect join for {}",
                    call.number()
                )
            );
        }
        assert!(guest.requests.lock().unwrap().is_empty());
        assert_eq!(guest.thread.stats.syscall_count, before_count);
        assert!(guest.thread.original_connect.is_none());
    }
}
