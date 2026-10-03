//! Real local reader/Normal issuer controls. Root/provider premises are the
//! existing controlled fixture, not ptrace or an end-to-end poll qualification.
use detcore_model::network_trace::*;
use reverie::Error;

use super::*;

#[tokio::test]
async fn raw_poll_record_global_wait_timeout_publishes_pair_without_new_turn() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::time::Duration;

    use crate::network_replay::NetworkSocketControlFinish;
    use crate::network_replay::NetworkStreamPhysicalEffect;
    use crate::network_replay::NetworkStreamPhysicalResult;
    use crate::network_replay::NetworkStreamPinOutcome;

    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let now = f.state.global_time.lock().unwrap().as_nanos();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_thread_time = f.thread.thread_logical_time.as_nanos();
    // The prior Connect and provider/root identity remain explicit controlled
    // premises. The existing runtime really joins its original prefix, retains
    // this TCP pin, executes poll/ppoll and confirms every worker completion.
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let (call, before_turn, before_committed, before_epoch, before_req, before_resp) = {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let turn = &scheduler.next_turns[&owner.thread];
        assert!(turn.req.try_read().is_none());
        assert!(turn.resp.try_read().is_none());
        let mut e = engine.lock().unwrap();
        e.controlled_poll_connected_channel(f.binding.open_file, now)
            .unwrap();
        let call = e
            .begin_native_stream_call_from_read(owner, f.read.clone())
            .unwrap();
        let attempt = e.begin_native_entry_stamp(owner, call.id).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |borrow| {
                e.stamp_native_receive_entry(attempt, borrow, &grant, now)
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        (
            call,
            scheduler.turn,
            scheduler.committed_time,
            grant.epoch(),
            turn.req.clone(),
            turn.resp.clone(),
        )
    };
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let pin = TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(1))
        .unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    drop(listener);
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    runtime
        .controlled_capture_poll_pin(owner, call.id, pin.into())
        .unwrap();
    let probe = {
        let mut e = engine.lock().unwrap();
        e.confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        e.finish_socket_control(
            owner,
            f.read.control.unwrap(),
            NetworkSocketControlFinish::Unchanged,
        )
        .unwrap();
        e.begin_shadow_probe(owner, call.id, now).unwrap()
    };
    runtime
        .bind_native_stream_lease(owner, call.id, probe.lease)
        .unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let (initial_clock, initial_response) = f
        .state
        .receive_rpc(
            f.tid,
            (
                f.thread.thread_logical_time.clone(),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
                    lease: probe.lease,
                    effect: NetworkStreamPhysicalEffect::PollState,
                }),
            ),
        )
        .await;
    let initial = match initial_response {
        GlobalResponse::Network(Ok(NetworkReply::NativeStreamObservation(observed))) => {
            Ok(observed)
        }
        other => Err(format!("initial PollState RPC: {other:?}")),
    };
    let after_initial_trace = engine.lock().unwrap().native_trace_fixture();
    let after_initial_clock = f.state.global_time.lock().unwrap().as_nanos();
    let deadline = now + LogicalTime::from_nanos(5_000_000);
    // Keep the peer live and never write/shutdown it: requested POLLIN must
    // reach a real finite kernel timeout, not fabricated EOF or POLLOUT.
    let waited = if initial.is_ok() {
        f.state
            .wait_foreground_poll(
                f.tid,
                &f.thread,
                call.id,
                Some(probe.lease),
                (libc::POLLIN, deadline),
                &[],
            )
            .await
            .map_err(|error| format!("{error:?}"))
    } else {
        Err("initial physical observation failed".to_owned())
    };
    let published = engine.lock().unwrap().native_trace_fixture();
    let after_wait_clock = f.state.global_time.lock().unwrap().as_nanos();
    let duplicate = if waited.is_ok() {
        Some(
            f.state
                .wait_foreground_poll(
                    f.tid,
                    &f.thread,
                    call.id,
                    Some(probe.lease),
                    (libc::POLLIN, deadline),
                    &[],
                )
                .await,
        )
    } else {
        None
    };
    let after_duplicate_trace = engine.lock().unwrap().native_trace_fixture();
    let after_duplicate_clock = f.state.global_time.lock().unwrap().as_nanos();

    // Do not panic on the result before the original physical owners close.
    // A pre-wait-refusal mutant can abort its known untouched probe and take the
    // same release RPC; an unknown/unacknowledged effect is still a failure.
    let aborted = if waited.is_err() {
        f.state.abort_foreground_poll(owner, call.id, probe.lease)
    } else {
        Ok(())
    };
    let lease_closed = runtime.finish_native_stream_lease(owner, probe.lease);
    let (release_clock, released) = f
        .state
        .receive_rpc(
            f.tid,
            (
                f.thread.thread_logical_time.clone(),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeReleaseStreamCall { call: call.id }),
            ),
        )
        .await;
    let joined = runtime.join_foreground_prefix(f.root.clone()).await;
    let eof = peer.read(&mut [0u8; 1]);
    drop(peer);

    assert!(aborted.is_ok(), "known probe cleanup: {aborted:?}");
    assert!(
        lease_closed.is_ok(),
        "physical lease cleanup: {lease_closed:?}"
    );
    assert_eq!(release_clock, None);
    assert_eq!(released, GlobalResponse::Network(Ok(NetworkReply::Unit)));
    assert!(joined.is_ok(), "original worker join: {joined:?}");
    assert_eq!(eof.unwrap(), 0, "original TCP pin really closed");
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

    let expected = libc::POLLOUT | libc::POLLWRNORM;
    assert!(
        matches!(&waited, Ok(raw) if *raw == expected),
        "coupled Record wait: {waited:?}"
    );
    let initial = initial.unwrap();
    assert_eq!(initial_clock, None);
    assert_eq!(initial.raw_return, 1);
    assert_eq!(initial.errno, None);
    assert_eq!(
        initial.confirmation,
        NetworkStreamPhysicalResult::PollState { revents: expected }
    );
    assert!(initial.bytes.is_empty());
    assert!(initial.helper_copy.is_none());
    assert_eq!(after_initial_trace, before_trace);
    assert_eq!(after_initial_clock, now);
    assert!(duplicate.is_some_and(|result| result.is_err()));
    assert_eq!(after_duplicate_trace, published);
    assert_eq!(after_duplicate_clock, after_wait_clock);
    assert_eq!(
        f.state.global_time.lock().unwrap().as_nanos(),
        after_wait_clock
    );
    assert_eq!(published.inputs.len(), before_trace.inputs.len() + 2);
    assert_eq!(
        published.release_model.nodes().len(),
        before_trace.release_model.nodes().len() + 2
    );
    let rows = &published.inputs[before_trace.inputs.len()..];
    let cut = NetworkReceiveEntryCutV4(before_trace.release_model.nodes().len() as u64);
    let frontier = before_trace.entry_frontier(cut).unwrap();
    for row in rows {
        assert_eq!(row.channel, before_trace.channels[0].id);
        assert_eq!(row.release.receive_entry_cut, cut);
        assert_eq!(row.release.prerequisites, frontier);
        assert_eq!(
            row.event,
            NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents: expected
            }
        );
    }
    assert_eq!(rows[0].release.not_before_global_time, now);
    assert_eq!(rows[1].release.not_before_global_time, after_wait_clock);
    assert!(after_wait_clock > now);
    assert!(after_wait_clock >= deadline);
    assert!(after_wait_clock.as_nanos() - now.as_nanos() < 1_000_000_000);
    published.validate().unwrap();
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), published);
    assert_eq!(f.thread.thread_logical_time.as_nanos(), before_thread_time);
    let scheduler = f.state.sched.lock().unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let turn = &scheduler.next_turns[&owner.thread];
    assert_eq!(scheduler.turn, before_turn);
    assert_eq!(scheduler.committed_time, before_committed);
    assert_eq!(grant.epoch(), before_epoch);
    assert_eq!(
        grant.resume(),
        crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
    );
    assert_eq!(turn.req, before_req);
    assert_eq!(turn.resp, before_resp);
    assert!(turn.req.try_read().is_none());
    assert!(turn.resp.try_read().is_none());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_record_long_wait_refusal_closes_known_probe_without_trace_or_clock() {
    use std::io::Read;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::time::Duration;

    use crate::network_replay::NetworkSocketControlFinish;
    use crate::network_replay::NetworkStreamPhysicalEffect;
    use crate::network_replay::NetworkStreamPhysicalResult;
    use crate::network_replay::NetworkStreamPinOutcome;

    let f = ReplayIssuerFixture::new_record().await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let engine = f.state.network_engine.as_ref().unwrap();
    let now = f.state.global_time.lock().unwrap().as_nanos();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let thread_time = f.thread.thread_logical_time.as_nanos();
    let prefix = runtime
        .join_foreground_prefix(f.root.clone())
        .await
        .unwrap();
    let (call, turn_number, committed, epoch, request, response) = {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let turn = &scheduler.next_turns[&owner.thread];
        let mut e = engine.lock().unwrap();
        e.controlled_poll_connected_channel(f.binding.open_file, now)
            .unwrap();
        let call = e
            .begin_native_stream_call_from_read(owner, f.read.clone())
            .unwrap();
        let attempt = e.begin_native_entry_stamp(owner, call.id).unwrap();
        runtime
            .with_foreground_prefix(&prefix, |borrow| {
                e.stamp_native_receive_entry(attempt, borrow, &grant, now)
                    .map_err(std::io::Error::other)
            })
            .unwrap();
        (
            call,
            scheduler.turn,
            scheduler.committed_time,
            grant.epoch(),
            turn.req.clone(),
            turn.resp.clone(),
        )
    };
    // Same controlled connection/pin premise as the positive coupled body;
    // the initial observation and original physical release are real workers.
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let pin = TcpStream::connect_timeout(&listener.local_addr().unwrap(), Duration::from_secs(1))
        .unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    drop(listener);
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    runtime
        .controlled_capture_poll_pin(owner, call.id, pin.into())
        .unwrap();
    let probe = {
        let mut e = engine.lock().unwrap();
        e.confirm_stream_call_pin(owner, call.id, NetworkStreamPinOutcome::Acquired)
            .unwrap();
        e.finish_socket_control(
            owner,
            f.read.control.unwrap(),
            NetworkSocketControlFinish::Unchanged,
        )
        .unwrap();
        e.begin_shadow_probe(owner, call.id, now).unwrap()
    };
    runtime
        .bind_native_stream_lease(owner, call.id, probe.lease)
        .unwrap();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let (initial_clock, initial) = f
        .state
        .receive_rpc(
            f.tid,
            (
                f.thread.thread_logical_time.clone(),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeStreamEffect {
                    lease: probe.lease,
                    effect: NetworkStreamPhysicalEffect::PollState,
                }),
            ),
        )
        .await;
    let refused = f
        .state
        .wait_foreground_poll(
            f.tid,
            &f.thread,
            call.id,
            Some(probe.lease),
            (libc::POLLIN, now + LogicalTime::from_nanos(1_000_000_000)),
            &[],
        )
        .await;
    // Preserve the primary result while the real production cleanup transaction
    // validates the engine and retained runtime completion under one guard.
    let aborted = f.state.abort_foreground_poll(owner, call.id, probe.lease);
    let (release_clock, released) = f
        .state
        .receive_rpc(
            f.tid,
            (
                f.thread.thread_logical_time.clone(),
                owner.mm,
                GlobalRequest::Network(NetworkRequest::NativeReleaseStreamCall { call: call.id }),
            ),
        )
        .await;
    let joined = runtime.join_foreground_prefix(f.root.clone()).await;
    let eof = peer.read(&mut [0u8; 1]);
    drop(peer);

    assert!(aborted.is_ok(), "known initial poll cleanup: {aborted:?}");
    assert_eq!(release_clock, None);
    assert_eq!(released, GlobalResponse::Network(Ok(NetworkReply::Unit)));
    assert!(joined.is_ok(), "original worker join: {joined:?}");
    assert_eq!(
        eof.unwrap(),
        0,
        "refusal still closes the actual original pin"
    );
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
    assert_eq!(initial_clock, None);
    let GlobalResponse::Network(Ok(NetworkReply::NativeStreamObservation(initial))) = initial
    else {
        panic!("initial PollState did not complete: {initial:?}");
    };
    assert_eq!(initial.raw_return, 1);
    assert_eq!(initial.errno, None);
    assert_eq!(
        initial.confirmation,
        NetworkStreamPhysicalResult::PollState {
            revents: libc::POLLOUT | libc::POLLWRNORM,
        }
    );
    assert!(initial.bytes.is_empty());
    assert!(initial.helper_copy.is_none());
    assert_eq!(
        refused,
        Err(NetworkRpcError::internal(
            "finite poll wait must be below one second"
        ))
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);
    assert_eq!(f.thread.thread_logical_time.as_nanos(), thread_time);
    let scheduler = f.state.sched.lock().unwrap();
    let grant = scheduler
        .foreground_native_observation(owner, &f.root)
        .unwrap();
    let turn = &scheduler.next_turns[&owner.thread];
    assert_eq!(scheduler.turn, turn_number);
    assert_eq!(scheduler.committed_time, committed);
    assert_eq!(grant.epoch(), epoch);
    assert_eq!(
        grant.resume(),
        crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
    );
    assert_eq!(turn.req, request);
    assert_eq!(turn.resp, response);
    assert!(turn.req.try_read().is_none());
    assert!(turn.resp.try_read().is_none());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

fn finite_replay_trace(final_mask: i16, release_ms: u64) -> NetworkTraceV4 {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    // Reuse the existing socket/profile and successful connection premises,
    // but do not preload payload that would contradict the initial empty poll.
    trace.inputs.truncate(1);
    trace.native_receive_observations.clear();
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.truncate(2);
    let cut = NetworkReceiveEntryCutV4(2);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    let now = trace.epoch_global_time().unwrap();
    for (time, revents) in [
        (now, libc::POLLOUT),
        (
            now + LogicalTime::from_nanos(release_ms * 1_000_000),
            final_mask,
        ),
    ] {
        let ordinal = trace.inputs.len() as u64;
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: trace.channels[0].id,
            release: NetworkReleaseV4 {
                not_before_global_time: time,
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::RawTcpPollState {
                consumed_prefix: 0,
                revents,
            },
        });
        let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } =
            &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(nodes.len() as u64),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites: prerequisites.clone(),
        });
    }
    trace.validate().unwrap();
    trace
}

fn finite_poll_rows(fd: i32) -> [libc::pollfd; 6] {
    [
        (fd, libc::POLLIN),
        (fd, libc::POLLRDNORM),
        (fd, libc::POLLIN | libc::POLLPRI),
        (fd, libc::POLLPRI),
        (fd, 0),
        (-1, libc::POLLIN),
    ]
    .map(|(fd, events)| libc::pollfd {
        fd,
        events,
        revents: -1,
    })
}

async fn finite_poll_callback(
    tool: &Detcore,
    guest: &mut OwnedReadGuest<'_>,
    rows: &mut [libc::pollfd],
    timeout_ms: i32,
) -> Result<i64, Error> {
    let call = reverie::syscalls::Poll::new()
        .with_fds(reverie::syscalls::AddrMut::from_ptr(
            rows.as_mut_ptr().cast(),
        ))
        .with_nfds(rows.len() as libc::nfds_t)
        .with_timeout(timeout_ms);
    // Actual Poll dispatcher/local GlobalState, not an engine-result mock.
    // This does not execute the outer ptrace Tool or claim native M1 coverage.
    tool.handle_network_io(guest, call.into()).await
}

fn assert_finite_poll_closed(f: &ReplayIssuerFixture, guest: &OwnedReadGuest<'_>) {
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
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    assert!(!guest.retired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        !guest
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| matches!(
                request,
                GlobalRequest::RequestResources(..) | GlobalRequest::ParkedRequest(..)
            ))
    );
}

async fn finite_replay_case(
    final_mask: i16,
    release_ms: u64,
    timeout_ms: i32,
    expected: Result<i64, &str>,
    expected_advance_ms: u64,
) {
    let trace = finite_replay_trace(final_mask, release_ms);
    let now = trace.epoch_global_time().unwrap();
    let (mut f, _) = ReplayIssuerFixture::new_trace(false, trace.clone(), true).await;
    let owner = f.root.owner();
    let engine = f.state.network_engine.as_ref().unwrap();
    engine
        .lock()
        .unwrap()
        .finish_fd_read(owner, f.read.clone())
        .unwrap();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let thread = std::mem::replace(&mut f.thread, tool.init_thread_state(f.tid, None));
    let mut guest = owned_read_guest(&f.config, &f.state, thread);
    guest.expose_local_global = true;
    let before_thread_time = guest.thread.thread_logical_time.as_nanos();
    let before_runtime = f
        .state
        .network_runtime
        .as_ref()
        .unwrap()
        .private_publication_runtime_fixture_state();
    let (before_turn, before_committed, before_epoch, before_request, before_response) = {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let turn = &scheduler.next_turns[&owner.thread];
        assert!(turn.req.try_read().is_none());
        assert!(turn.resp.try_read().is_none());
        (
            scheduler.turn,
            scheduler.committed_time,
            grant.epoch(),
            turn.req.clone(),
            turn.resp.clone(),
        )
    };
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (0, vec![], vec![0, 1, 2])
    );
    // Extra observations must not consume the future row or advance time.
    let mut initial_rows = finite_poll_rows(f.binding.slot.fd);
    let initial = finite_poll_callback(&tool, &mut guest, &mut initial_rows, 0).await;
    assert_finite_poll_closed(&f, &guest);
    assert!(matches!(initial, Ok(0)), "initial zero scan: {initial:?}");
    assert_eq!(initial_rows.map(|row| row.revents), [0; 6]);
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);

    let mut rows = finite_poll_rows(f.binding.slot.fd);
    let input_rows = rows.map(|row| (row.fd, row.events, row.revents));
    let result = finite_poll_callback(&tool, &mut guest, &mut rows, timeout_ms).await;
    // Check the physical/logical owners before interpreting a success/error.
    assert_finite_poll_closed(&f, &guest);
    assert_eq!(
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    match expected {
        Ok(count) => {
            assert!(
                matches!(result, Ok(actual) if actual == count),
                "Poll result: {result:?}"
            );
            let expected_masks = if count == 3 {
                [libc::POLLIN, libc::POLLRDNORM, libc::POLLIN, 0, 0, 0]
            } else {
                assert_eq!(count, 0);
                [0; 6]
            };
            assert_eq!(rows.map(|row| row.revents), expected_masks);
            assert_eq!(
                rows.map(|row| (row.fd, row.events)),
                input_rows.map(|(fd, events, _)| (fd, events))
            );
            let before_repeat = engine
                .lock()
                .unwrap()
                .controlled_replay_delivery_state(f.binding.open_file);
            let mut repeated_rows = finite_poll_rows(f.binding.slot.fd);
            let repeated = finite_poll_callback(&tool, &mut guest, &mut repeated_rows, 0).await;
            assert_finite_poll_closed(&f, &guest);
            assert!(
                matches!(repeated, Ok(actual) if actual == count),
                "repeated Poll: {repeated:?}"
            );
            assert_eq!(repeated_rows.map(|row| row.revents), expected_masks);
            assert_eq!(
                engine
                    .lock()
                    .unwrap()
                    .controlled_replay_delivery_state(f.binding.open_file),
                before_repeat
            );
        }
        Err(message) => {
            let Error::Tool(error) = result.unwrap_err() else {
                panic!("unsupported coverage/profile must be a Tool refusal")
            };
            assert_eq!(
                format!("{error:#}"),
                format!("shared network engine refused operation: {message}")
            );
            assert_eq!(
                rows.map(|row| (row.fd, row.events, row.revents)),
                input_rows
            );
        }
    }
    assert_eq!(
        f.state.global_time.lock().unwrap().as_nanos(),
        now + LogicalTime::from_nanos(expected_advance_ms * 1_000_000)
    );
    assert_eq!(
        guest.thread.thread_logical_time.as_nanos(),
        before_thread_time
    );
    {
        let scheduler = f.state.sched.lock().unwrap();
        let grant = scheduler
            .foreground_native_observation(owner, &f.root)
            .unwrap();
        let turn = &scheduler.next_turns[&owner.thread];
        assert_eq!(scheduler.turn, before_turn);
        assert_eq!(scheduler.committed_time, before_committed);
        assert_eq!(grant.epoch(), before_epoch);
        assert_eq!(
            grant.resume(),
            crate::scheduler::ordinary_fd::OrdinaryFdResume::Normal
        );
        assert_eq!(turn.req, before_request);
        assert_eq!(turn.resp, before_response);
        assert!(turn.req.try_read().is_none());
        assert!(turn.resp.try_read().is_none());
    }
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        (
            0,
            vec![],
            if expected_advance_ms == 0 {
                vec![0, 1, 2]
            } else {
                vec![0, 1, 2, 3]
            }
        )
    );
    assert_eq!(
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .private_publication_runtime_fixture_state(),
        before_runtime
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), trace);
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_finite_replay_readiness_keeps_normal_grant_and_reuses_state() {
    finite_replay_case(
        libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT,
        198,
        198,
        Ok(3),
        198,
    )
    .await;
}

#[tokio::test]
async fn raw_poll_finite_replay_timeout_requires_actual_final_empty_observation() {
    for release_ms in [198, 199] {
        finite_replay_case(libc::POLLOUT, release_ms, 198, Ok(0), release_ms).await;
    }
}

#[tokio::test]
async fn raw_poll_finite_replay_early_empty_never_invents_deadline_timeout() {
    finite_replay_case(
        libc::POLLOUT,
        197,
        198,
        Err("finite Replay poll lacks a final raw observation"),
        197,
    )
    .await;
}

#[tokio::test]
async fn raw_poll_finite_replay_refuses_indefinite_and_one_second_profiles() {
    for (timeout_ms, message) in [
        (-1, "V4 poll blocking wait is not implemented"),
        (1000, "finite poll wait must be below one second"),
        (1001, "finite poll wait must be below one second"),
    ] {
        finite_replay_case(
            libc::POLLIN | libc::POLLRDNORM | libc::POLLOUT,
            198,
            timeout_ms,
            Err(message),
            0,
        )
        .await;
    }
}

#[tokio::test]
async fn raw_poll_guest_dispatch_preserves_duplicate_alias_masks_and_closes_call() {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let now = trace.epoch_global_time().unwrap();
    let ordinal = trace.inputs.len() as u64;
    let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: NetworkInputKindV2::RawTcpPollState {
            consumed_prefix: 0,
            revents: libc::POLLIN
                | libc::POLLRDNORM
                | libc::POLLOUT
                | libc::POLLWRNORM
                | libc::POLLPRI
                | libc::POLLRDHUP,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace, true).await;
    let engine = f.state.network_engine.as_ref().unwrap();
    // Release only the setup reader. The actual dispatcher must acquire and
    // close its own readers and Call; no issuer shortcut is used below.
    engine
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read)
        .unwrap();
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut guest = owned_read_guest(&f.config, &f.state, f.thread);
    guest.expose_local_global = true;
    let fd = f.binding.slot.fd;
    let alias = fd + 1;
    // Controlled successful dup2 premise, not a fresh socket creation. The
    // existing mutation validates the source and publishes an Alias effect.
    let crate::network_replay::NetworkFdMutationBegin::Admitted(admission) = engine
        .lock()
        .unwrap()
        .begin_fd_mutation(
            f.root.owner(),
            f.binding.slot.files,
            crate::network_replay::NetworkFdMutationKind::Alias {
                source_fd: fd,
                source: Some(f.binding),
                kind: crate::network_replay::NetworkFdInstallKind::Dup2,
                cloexec: false,
                destination: Some(alias),
                replaced: None,
            },
        )
        .unwrap()
    else {
        panic!("alias fixture requires its actual mutation admission")
    };
    {
        let mut engine = engine.lock().unwrap();
        engine
            .submit_fd_mutation(f.root.owner(), admission.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(
                f.root.owner(),
                admission.publication.permit,
                Ok(i64::from(alias)),
            )
            .unwrap();
    }
    assert_eq!(
        guest
            .thread
            .dup_fd(fd, alias, nix::fcntl::OFlag::empty())
            .unwrap(),
        None
    );
    tool.complete_network_fd_installation(&mut guest, Some(&admission), alias)
        .await
        .unwrap();
    assert_eq!(
        guest.thread.descriptor_binding(alias).unwrap().open_file,
        f.binding.open_file
    );
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_delivery = engine
        .lock()
        .unwrap()
        .controlled_replay_delivery_state(f.binding.open_file);
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let before_turn = f.state.sched.lock().unwrap().turn;
    let inputs = [
        (fd, libc::POLLIN),
        (fd, libc::POLLPRI),
        (alias, libc::POLLRDNORM | libc::POLLWRNORM),
        (fd, libc::POLLRDHUP),
        (alias, 0),
        (-1, libc::POLLIN),
    ];
    let mut rows = inputs.map(|(fd, events)| libc::pollfd {
        fd,
        events,
        revents: -1,
    });
    let call = reverie::syscalls::Poll::new()
        .with_fds(reverie::syscalls::AddrMut::from_ptr(
            rows.as_mut_ptr().cast(),
        ))
        .with_nfds(rows.len() as libc::nfds_t)
        .with_timeout(0);
    // OwnedReadGuest permits typed local memory but panics on every physical
    // injection, timer and stack use. Removing native_poll_single_scan's
    // dispatch hits the unchanged exceptional-mask refusal, not this result.
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
    assert!(matches!(result, Ok(4)), "actual Poll handler: {result:?}");
    assert_eq!(
        rows.map(|row| (row.fd, row.events, row.revents)),
        [
            (fd, libc::POLLIN, libc::POLLIN),
            (fd, libc::POLLPRI, libc::POLLPRI),
            (
                alias,
                libc::POLLRDNORM | libc::POLLWRNORM,
                libc::POLLRDNORM | libc::POLLWRNORM
            ),
            (fd, libc::POLLRDHUP, libc::POLLRDHUP),
            (alias, 0, 0),
            (-1, libc::POLLIN, 0),
        ]
    );
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file),
        before_delivery
    );
    assert_eq!(engine.lock().unwrap().native_trace_fixture(), before_trace);
    assert_eq!(f.state.sched.lock().unwrap().turn, before_turn);
    assert!(guest.thread.original_connect.is_none());
    assert!(guest.thread.original_file_metadata.is_none());
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_replay_issuer_uses_actual_reader_without_physical_capture() {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    let now = trace.epoch_global_time().unwrap();
    let ordinal = trace.inputs.len() as u64;
    let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
    let prerequisites = trace.entry_frontier(cut).unwrap();
    trace.inputs.push(NetworkInputEventV4 {
        ordinal,
        channel: trace.channels[0].id,
        release: NetworkReleaseV4 {
            not_before_global_time: now,
            receive_entry_cut: cut,
            prerequisites: prerequisites.clone(),
        },
        event: NetworkInputKindV2::RawTcpPollState {
            consumed_prefix: 0,
            revents: libc::POLLPRI | libc::POLLRDHUP,
        },
    });
    let NetworkReleaseModelV4::SoleInitialRootProgramOrderV1 { nodes } = &mut trace.release_model else { panic!("legacy fixture changed its release policy"); };
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(cut.0),
        kind: NetworkReleaseNodeKindV4::Input {
            input_ordinal: ordinal,
        },
        prerequisites,
    });
    let (f, _) = ReplayIssuerFixture::new_trace(false, trace, true).await;
    let owner = f.root.owner();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let engine = f.state.network_engine.as_ref().unwrap();
    let call = f
        .state
        .begin_replay_poll_call(f.tid, &f.thread, f.read.clone())
        .unwrap();
    assert!(!call.physical_pin_required);
    assert_eq!(call.open_file, f.binding.open_file);
    assert_eq!(
        engine
            .lock()
            .unwrap()
            .native_capture_fixture_counts(f.binding.open_file),
        (1, 0, 0, 1)
    );
    for _ in 0..2 {
        assert_eq!(
            f.state
                .finish_foreground_poll(f.tid, &f.thread, call.id, None)
                .unwrap(),
            libc::POLLPRI | libc::POLLRDHUP
        );
    }
    let before = format!("{:?}", engine.lock().unwrap());
    assert!(
        f.state
            .begin_replay_poll_call(f.tid, &f.thread, f.read.clone())
            .is_err()
    );
    assert!(
        f.state
            .finish_foreground_poll(Tid::from_raw(f.tid.as_raw() + 1), &f.thread, call.id, None)
            .is_err()
    );
    assert_eq!(format!("{:?}", engine.lock().unwrap()), before);
    engine
        .lock()
        .unwrap()
        .begin_stream_call_release(owner, call.id)
        .unwrap();
    engine
        .lock()
        .unwrap()
        .finish_stream_call_release(owner, call.id)
        .unwrap();
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
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}

#[tokio::test]
async fn raw_poll_issuer_refuses_foreign_task_mm_metadata_and_reader() {
    for variant in 0..8 {
        let mut f = ReplayIssuerFixture::new().await;
        let owner = f.root.owner();
        let mut tid = f.tid;
        let mut read = f.read.clone();
        match variant {
            0 => tid = Tid::from_raw(f.tid.as_raw() + 1),
            1 => f.thread.mm_id = f.thread.mm_id.for_exec(owner.thread),
            2 => {
                let copied = f.thread.file_metadata.lock().unwrap().clone();
                f.thread.file_metadata = Arc::new(Mutex::new(copied));
            }
            3 => {
                f.thread.memory_metadata =
                    Arc::new(Mutex::new(crate::memory::MemoryMetadata::default()))
            }
            4 => read.binding = None,
            5 => read.control = None,
            6 => f.state.cfg.sequentialize_threads = false,
            7 => f.state.sched = Arc::new(Mutex::new(crate::scheduler::Scheduler::new(&f.config))),
            _ => unreachable!(),
        }
        let engine = f.state.network_engine.as_ref().unwrap();
        let before = format!("{:?}", engine.lock().unwrap());
        let runtime = f.state.network_runtime.as_ref().unwrap();
        let before_runtime = runtime.private_publication_runtime_fixture_state();
        assert!(
            f.state
                .begin_replay_poll_call(tid, &f.thread, read)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(
            format!("{:?}", engine.lock().unwrap()),
            before,
            "variant {variant}"
        );
        assert_eq!(
            engine
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file),
            (0, 1, 1, 0)
        );
        engine
            .lock()
            .unwrap()
            .finish_fd_read(owner, f.read)
            .unwrap();
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
        assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
    }
}

#[tokio::test]
async fn raw_poll_record_entry_failure_releases_reader_before_any_capture() {
    let f = ReplayIssuerFixture::new_record().await;
    let engine = f.state.network_engine.as_ref().unwrap();
    let runtime = f.state.network_runtime.as_ref().unwrap();
    let before_runtime = runtime.private_publication_runtime_fixture_state();
    let before_trace = engine.lock().unwrap().native_trace_fixture();
    let mut older = f.config.clone();
    older.epoch -= chrono::Duration::seconds(1);
    *f.state.global_time.lock().unwrap() = crate::types::GlobalTime::new(&older);
    let failure = f
        .state
        .begin_native_poll_call(f.tid, &f.thread, f.read.clone())
        .await
        .unwrap_err();
    let NetworkRpcError::Internal(message) = failure.primary().clone() else {
        panic!("entry refusal")
    };
    assert_eq!(
        message,
        NetworkReplayError::from(NetworkTraceValidationError::ReleaseBeforeEpoch).to_string()
    );
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
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}
