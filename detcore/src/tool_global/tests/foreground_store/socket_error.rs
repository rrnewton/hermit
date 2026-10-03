use detcore_model::network_trace::*;

use super::*;
use crate::network_replay::NetworkSocketControlFinish;

async fn request(
    guest: &mut ScalarForegroundGuest<'_>,
    request: NetworkRequest,
) -> Result<NetworkReply, NetworkRpcError> {
    super::super::super::network_request(guest, request).await
}

async fn begin(guest: &mut ScalarForegroundGuest<'_>, file: OpenFileId) -> NetworkStreamLeaseId {
    let NetworkReply::SocketControl(control) = request(
        guest,
        NetworkRequest::BeginSocketControl { open_file: file },
    )
    .await
    .unwrap() else {
        panic!("missing socket control")
    };
    control.lease
}

// The scalar physical completion is a controlled premise; the global RPC,
// current scheduler grant, retained root and journal/control transitions are real.
async fn recorded_errors(errors: &[i32]) -> NetworkTraceV4 {
    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let now = f.state.global_time.lock().unwrap().as_nanos();
    let turn = f.state.sched.lock().unwrap().turn;
    {
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.finish_fd_read(owner, f.read.clone()).unwrap();
        engine
            .controlled_poll_connected_channel(f.binding.open_file, now)
            .unwrap();
    }
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    for errno in errors {
        let lease = begin(&mut guest, f.binding.open_file).await;
        assert_eq!(
            request(
                &mut guest,
                NetworkRequest::SubmitStreamPhysical {
                    lease,
                    effect: NetworkStreamPhysicalEffect::ReadSocketError,
                }
            )
            .await
            .unwrap(),
            NetworkReply::Unit
        );
        for disposition in [
            NetworkSocketControlFinish::Unchanged,
            NetworkSocketControlFinish::ErrorTaken,
        ] {
            assert!(
                request(
                    &mut guest,
                    NetworkRequest::FinishSocketControl { lease, disposition }
                )
                .await
                .is_err()
            );
        }
        let before = f
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture();
        assert!(
            request(
                &mut guest,
                NetworkRequest::ConfirmStreamPhysical {
                    lease,
                    result: crate::network_replay::NetworkStreamPhysicalResult::SocketError(-1),
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_trace_fixture(),
            before
        );
        assert!(
            request(
                &mut guest,
                NetworkRequest::SubmitStreamPhysical {
                    lease,
                    effect: NetworkStreamPhysicalEffect::ReadSocketError,
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            request(
                &mut guest,
                NetworkRequest::ConfirmStreamPhysical {
                    lease,
                    result: crate::network_replay::NetworkStreamPhysicalResult::SocketError(*errno),
                }
            )
            .await
            .unwrap(),
            NetworkReply::Unit
        );
        assert!(
            request(
                &mut guest,
                NetworkRequest::ConfirmStreamPhysical {
                    lease,
                    result: crate::network_replay::NetworkStreamPhysicalResult::SocketError(*errno),
                }
            )
            .await
            .is_err()
        );
        assert_eq!(
            request(
                &mut guest,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::ErrorTaken,
                }
            )
            .await
            .unwrap(),
            NetworkReply::Unit
        );
    }
    let engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);
    assert_eq!(f.state.sched.lock().unwrap().turn, turn);
    assert_eq!(
        engine.native_capture_fixture_counts(f.binding.open_file),
        (0, 0, 0, 0)
    );
    let trace = engine.native_trace_fixture();
    trace.validate().unwrap();
    trace
}

#[tokio::test]
async fn socket_error_zero_nonzero_and_repeated_reads_are_distinct_consumptions() {
    let expected = [libc::ECONNREFUSED, 0, libc::EPIPE, 0];
    let trace = recorded_errors(&expected).await;
    assert_eq!(
        trace
            .inputs
            .iter()
            .filter_map(|row| match row.event {
                NetworkInputKindV2::SocketErrorRead {
                    consumed_prefix: 0,
                    errno,
                } => Some(errno),
                _ => None,
            })
            .collect::<Vec<_>>(),
        expected
    );
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace.clone(), false).await;
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let now = f.state.global_time.lock().unwrap().as_nanos();
    let turn = f.state.sched.lock().unwrap().turn;
    for (index, errno) in expected.into_iter().enumerate() {
        let lease = begin(&mut guest, f.binding.open_file).await;
        assert_eq!(
            request(&mut guest, NetworkRequest::TakeSocketError { lease })
                .await
                .unwrap(),
            NetworkReply::SocketError(errno)
        );
        // Consuming this input fulfills its progress node and only now admits
        // the next recorded control, even though every release has the same time.
        assert_eq!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .next_release_time()
                .unwrap(),
            (index + 1 < expected.len())
                .then(|| trace.inputs[index + 2].release.not_before_global_time)
        );
        assert!(
            request(&mut guest, NetworkRequest::TakeSocketError { lease })
                .await
                .is_err()
        );
        assert!(
            request(
                &mut guest,
                NetworkRequest::FinishSocketControl {
                    lease,
                    disposition: NetworkSocketControlFinish::Unchanged,
                }
            )
            .await
            .is_err()
        );
        request(
            &mut guest,
            NetworkRequest::FinishSocketControl {
                lease,
                disposition: NetworkSocketControlFinish::ErrorTaken,
            },
        )
        .await
        .unwrap();
    }
    let lease = begin(&mut guest, f.binding.open_file).await;
    assert!(
        request(&mut guest, NetworkRequest::TakeSocketError { lease })
            .await
            .is_err()
    );
    request(
        &mut guest,
        NetworkRequest::FinishSocketControl {
            lease,
            disposition: NetworkSocketControlFinish::Unchanged,
        },
    )
    .await
    .unwrap();
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);
    assert_eq!(f.state.sched.lock().unwrap().turn, turn);
    assert_eq!(engine.native_trace_fixture(), trace);
    // Controlled backend task-consumption premise closes the fixture's table
    // owner before the unchanged complete-trace finalizer runs.
    engine.retire_fd_table_owner(f.root.owner());
    engine.stream_owner_gone(f.root.owner());
    engine.finish().unwrap();
}

#[tokio::test]
async fn socket_error_user_copy_order_preserves_linux_consumption() {
    // Linux 295ad0959f344afbc813d7962007d36db1dd8e8b net/core/sock.c:
    // signed optlen read precedes SO_ERROR, value copy precedes length copy.
    let trace = recorded_errors(&[libc::ECONNRESET, 0]).await;
    for (capacity, readonly_length, bad_value, bad_length, consumed) in [
        (4, false, false, false, true),
        (1, false, false, false, true),
        (3, false, false, false, true),
        (0, false, true, false, true),
        (4, true, false, false, true),
        (4, false, true, false, true),
        (-1, false, false, false, false),
        (4, false, false, true, false),
    ] {
        let (f, _) = ReplayIssuerFixture::new_trace(false, trace.clone(), false).await;
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .finish_fd_read(f.root.owner(), f.read.clone())
            .unwrap();
        unsafe { std::ptr::write_unaligned(f.pages.at(4096) as *mut i32, capacity) };
        if readonly_length {
            f.pages.protect_second(libc::PROT_READ);
        }
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        guest.socket_error_access = true;
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let call = reverie::syscalls::Getsockopt::new()
            .with_fd(f.binding.slot.fd)
            .with_level(libc::SOL_SOCKET)
            .with_optname(libc::SO_ERROR)
            .with_optval(if bad_value {
                None
            } else {
                reverie::syscalls::AddrMut::from_raw(f.pages.at(0) as usize)
            })
            .with_optlen(if bad_length {
                None
            } else {
                reverie::syscalls::AddrMut::from_raw(f.pages.at(4096) as usize)
            });
        let result = tool.try_shadow_getsockopt(&mut guest, call).await;
        if capacity < 0 {
            assert!(matches!(result, Err(reverie::Error::Errno(Errno::EINVAL))));
        } else if readonly_length || bad_length || (bad_value && capacity != 0) {
            assert!(matches!(result, Err(reverie::Error::Errno(Errno::EFAULT))));
        } else {
            assert!(matches!(result, Ok(Some(0))), "{result:?}");
        }
        if !bad_value && !bad_length && capacity > 0 {
            let count = (capacity as usize).min(4);
            assert_eq!(
                f.pages.bytes(0, count),
                libc::ECONNRESET.to_ne_bytes()[..count]
            );
            assert_eq!(f.pages.bytes(count, 4 - count), vec![0xa5; 4 - count]);
        } else {
            assert_eq!(f.pages.bytes(0, 4), [0xa5; 4]);
        }
        let lease = begin(&mut guest, f.binding.open_file).await;
        assert_eq!(
            request(&mut guest, NetworkRequest::TakeSocketError { lease })
                .await
                .unwrap(),
            NetworkReply::SocketError(if consumed { 0 } else { libc::ECONNRESET })
        );
        request(
            &mut guest,
            NetworkRequest::FinishSocketControl {
                lease,
                disposition: NetworkSocketControlFinish::ErrorTaken,
            },
        )
        .await
        .unwrap();
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
    }
}

#[tokio::test]
async fn socket_error_replay_refuses_a_future_consumed_byte_cut() {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    let ordinal = trace.inputs.len() as u64;
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: trace.epoch_global_time().unwrap(),
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: NetworkInputKindV2::SocketErrorRead {
            consumed_prefix: 1,
            errno: 0,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model;
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0 + 1),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel: trace.channels[0].id,
            milestone: NetworkProgressV4::SocketErrorConsumed {
                input_ordinal: ordinal,
            },
        },
        prerequisites: vec![NetworkReleaseNodeIdV4(cut.0)],
    });
    trace.validate().unwrap();
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace.clone(), true).await;
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let lease = begin(&mut guest, f.binding.open_file).await;
    let error = request(&mut guest, NetworkRequest::TakeSocketError { lease })
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("consumed-byte cut"));
    request(
        &mut guest,
        NetworkRequest::FinishSocketControl {
            lease,
            disposition: NetworkSocketControlFinish::Unchanged,
        },
    )
    .await
    .unwrap();
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(engine.native_trace_fixture(), trace);
    assert_eq!(
        engine
            .controlled_replay_delivery_state(f.binding.open_file)
            .0,
        0
    );
    engine.retire_fd_table_owner(f.root.owner());
    engine.stream_owner_gone(f.root.owner());
    assert!(matches!(
        engine.finish(),
        Err(NetworkReplayError::UnconsumedTrace)
    ));
}

#[tokio::test]
async fn socket_error_record_control_keeps_matching_submit_confirm_finish() {
    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let now = f.state.global_time.lock().unwrap().as_nanos();
    {
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.finish_fd_read(owner, f.read.clone()).unwrap();
        engine
            .controlled_poll_connected_channel(f.binding.open_file, now)
            .unwrap();
    }
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let reply = super::super::super::network_request(
        &mut guest,
        NetworkRequest::BeginSocketControl {
            open_file: f.binding.open_file,
        },
    )
    .await
    .unwrap();
    let NetworkReply::SocketControl(control) = reply else {
        panic!("missing control");
    };
    for request in [
        NetworkRequest::SubmitStreamPhysical {
            lease: control.lease,
            effect: NetworkStreamPhysicalEffect::ReadSocketError,
        },
        NetworkRequest::ConfirmStreamPhysical {
            lease: control.lease,
            result: crate::network_replay::NetworkStreamPhysicalResult::SocketError(0),
        },
        NetworkRequest::FinishSocketControl {
            lease: control.lease,
            disposition: NetworkSocketControlFinish::ErrorTaken,
        },
    ] {
        assert_eq!(
            super::super::super::network_request(&mut guest, request)
                .await
                .unwrap(),
            NetworkReply::Unit
        );
    }
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
}
