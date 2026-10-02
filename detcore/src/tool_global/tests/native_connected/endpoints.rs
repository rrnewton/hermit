//! Original Connect/Call publisher and real endpoint dispatch controls. The
//! provider and backend-result premises are controlled; localhost connection,
//! same-pin getters and original release are real. Replay uses a Guest whose
//! physical injections panic, after both actual socket owners have been closed.
use reverie::syscalls::AddrMut;
use reverie::syscalls::{self};

use super::*;

async fn query(
    tool: &Detcore,
    guest: &mut OwnedReadGuest<'_>,
    fd: i32,
    peer: bool,
) -> (
    Result<i64, reverie::Error>,
    libc::sockaddr_in,
    libc::socklen_t,
) {
    let mut address = libc::sockaddr_in {
        sin_family: 0x5555,
        sin_port: 0x5555,
        sin_addr: libc::in_addr { s_addr: 0x55555555 },
        sin_zero: [0x55; 8],
    };
    let mut length = std::mem::size_of_val(&address) as libc::socklen_t;
    let address_ptr = AddrMut::from_ptr(&raw mut address).unwrap().cast();
    let length_ptr = AddrMut::from_ptr(&raw mut length);
    let result = if peer {
        tool.handle_getpeername(
            guest,
            syscalls::Getpeername::new()
                .with_fd(fd)
                .with_usockaddr(Some(address_ptr))
                .with_usockaddr_len(length_ptr),
        )
        .await
    } else {
        tool.handle_getsockname(
            guest,
            syscalls::Getsockname::new()
                .with_fd(fd)
                .with_usockaddr(Some(address_ptr))
                .with_usockaddr_len(length_ptr),
        )
        .await
    };
    (result, address, length)
}

fn address_result(
    result: &(
        Result<i64, reverie::Error>,
        libc::sockaddr_in,
        libc::socklen_t,
    ),
    expected: &NetworkAddressV2,
) {
    assert!(matches!(result.0, Ok(0)), "{:?}", result.0);
    assert_eq!(result.2 as usize, std::mem::size_of::<libc::sockaddr_in>());
    assert_eq!(result.1.sin_family, libc::AF_INET as u16);
    assert_eq!(
        NetworkAddressV2::Inet4 {
            address: result.1.sin_addr.s_addr.to_ne_bytes(),
            port: u16::from_be(result.1.sin_port),
        },
        *expected
    );
    assert_eq!(result.1.sin_zero, [0; 8]);
}

async fn round_trip(asynchronous: bool) {
    let mut f = Fixture::new().await;
    let fd = f.client.as_raw_fd();
    let open_file = f.admission.arguments.binding.unwrap().open_file;
    let unpublished = f
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .accepted_endpoint(open_file, false);
    f.connect();
    let returned = if asynchronous {
        -i64::from(libc::EINPROGRESS)
    } else {
        0
    };
    if asynchronous {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
    }
    // The -115 case is an explicit component return, not a claim that the
    // preceding physical blocking Connect itself returned EINPROGRESS.
    f.returned = Some(returned);
    f.completed(ConnectCompletionChange::None).unwrap();
    f.observe(InjectedSyscallEvent::Returned(returned), 0);
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .controlled_connect_retirement_with_return(f.owner(), &f.admission, returned)
        .unwrap();
    f.close_pin();
    f.foreground().await;
    f.state
        .publish_foreground_native_connected(f.tid, &f.thread, &f.admission)
        .unwrap();
    let trace = f.trace();
    let channel = trace.channels[0].id;
    let remote = trace.channels[0].peer_address.clone().unwrap();
    let local = trace.channels[0].local_address.clone().unwrap();
    f.finish();
    let Fixture {
        mut config,
        state,
        tool,
        thread,
        client,
        _listener: listener,
        ..
    } = f;
    drop(client);
    drop(listener);
    assert!(
        unpublished.is_err(),
        "requested peer metadata was not establishment"
    );
    let mut record_guest = owned_read_guest(&config, &state, thread);
    let record_peer = query(&tool, &mut record_guest, fd, true).await;
    let record_local = query(&tool, &mut record_guest, fd, false).await;
    let thread = record_guest.thread;
    address_result(&record_peer, &remote);
    address_result(&record_local, &local);
    trace.validate().unwrap();

    config.network_trace.policy = NetworkPolicy::Replay;
    let mut replay = NetworkReplayEngine::replay_native_receive(trace.clone()).unwrap();
    replay.bind(open_file, channel).unwrap();
    *state.network_engine.as_ref().unwrap().lock().unwrap() = replay;
    let mut guest = owned_read_guest(&config, &state, thread);
    for peer in [false, true] {
        let denied = query(&tool, &mut guest, fd, peer).await;
        assert!(
            denied.0.is_err(),
            "frozen future endpoint leaked before release"
        );
        assert_eq!(denied.1.sin_family, 0x5555);
        assert_eq!(denied.1.sin_zero, [0x55; 8]);
    }
    state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .release_eligible(trace.inputs[0].release.not_before_global_time)
        .unwrap();
    for peer in [false, true] {
        assert!(
            query(&tool, &mut guest, fd, peer).await.0.is_err(),
            "released metadata replaced delivery of the actual Connect outcome"
        );
    }
    let outcome = state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .take_connection_outcome(open_file)
        .unwrap();
    assert_eq!(
        outcome,
        Some(crate::network_replay::ConnectionOutcome::Connect(
            if asynchronous {
                NetworkConnectionResultV2::Error(libc::EINPROGRESS)
            } else {
                NetworkConnectionResultV2::Connected
            }
        ))
    );
    address_result(&query(&tool, &mut guest, fd, true).await, &remote);
    address_result(&query(&tool, &mut guest, fd, false).await, &local);
    assert!(
        guest
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| matches!(
                request,
                GlobalRequest::Network(NetworkRequest::AcceptedEndpoint { .. })
            ))
    );
    state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish()
        .unwrap();
}

#[tokio::test]
async fn outbound_endpoints_follow_successful_original_connect_without_replay_network() {
    round_trip(false).await;
}

#[tokio::test]
async fn outbound_endpoints_preserve_einprogress_and_wait_for_delivered_establishment() {
    round_trip(true).await;
}
