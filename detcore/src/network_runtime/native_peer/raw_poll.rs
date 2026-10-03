//! One actual nonconsuming poll on the retained original OFD.
use detcore_model::network_trace::NETWORK_TCP_POLL_REQUEST;
use detcore_model::network_trace::valid_tcp_poll_mask;

use super::*;

pub(super) fn observe(fd: BorrowedFd<'_>) -> Observation {
    let mut row = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: NETWORK_TCP_POLL_REQUEST,
        revents: 0,
    };
    let raw = unsafe { libc::poll(&raw mut row, 1, 0) };
    observation(i64::from(raw), Vec::new(), |_| ResultValue::PollState {
        revents: row.revents,
    })
}

/// The requested timeout is below the existing one-second worker bound; host
/// descheduling does not extend that bound or prove worker completion. The
/// caller retains the original OFD until the actual worker is joined.
pub(super) fn valid_wait_request(events: i16, timeout_ns: u64) -> bool {
    valid_tcp_poll_mask(events) && (1..1_000_000_000).contains(&timeout_ns)
}

pub(super) fn wait(fd: BorrowedFd<'_>, events: i16, timeout_ns: u64) -> Observation {
    if !valid_wait_request(events, timeout_ns) {
        // Admission also checks this before creating a physical Pending. This
        // defensive refusal is not a kernel completion or a guest errno.
        return Observation {
            raw_return: -1,
            errno: Some(libc::EINVAL),
            bytes: Vec::new(),
            confirmation: ResultValue::Errno(libc::EINVAL),
            helper_copy: None,
        };
    }
    let mut row = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: timeout_ns as libc::c_long,
    };
    // No signal-mask override, retry, descriptor-flag change or queue access.
    // In particular, do not request all raw-state bits here: POLLOUT would
    // otherwise complete an empty POLLIN wait immediately.
    let raw = unsafe { libc::ppoll(&raw mut row, 1, &timeout, std::ptr::null()) };
    observation(i64::from(raw), Vec::new(), |_| ResultValue::PollWait {
        revents: row.revents,
    })
}

pub(super) fn validate_wait(observed: &Observation, events: i16) -> io::Result<()> {
    let valid = match observed.confirmation {
        ResultValue::PollWait { revents } => {
            valid_tcp_poll_mask(events)
                && valid_tcp_poll_mask(revents)
                && revents & !(events | libc::POLLERR | libc::POLLHUP) == 0
                && observed.raw_return == i64::from(revents != 0)
                && observed.errno.is_none()
        }
        // Host EINTR and every other error remain a failed physical operation;
        // none is an authenticated guest interruption or an elapsed timeout.
        _ => false,
    };
    if !valid || !observed.bytes.is_empty() || observed.helper_copy.is_some() {
        return Err(io::Error::other(
            "native poll wait raw return/mask differs from requested completion",
        ));
    }
    Ok(())
}

pub(super) fn validate(observed: &Observation) -> io::Result<()> {
    let valid = match observed.confirmation {
        ResultValue::PollState { revents } => {
            valid_tcp_poll_mask(revents)
                && observed.raw_return == i64::from(revents != 0)
                && observed.errno.is_none()
        }
        ResultValue::Errno(errno) => {
            (1..=4095).contains(&errno)
                && observed.raw_return == -1
                && observed.errno == Some(errno)
        }
        _ => false,
    };
    if !valid || !observed.bytes.is_empty() || observed.helper_copy.is_some() {
        return Err(io::Error::other(
            "native poll raw return/mask differs from retained result",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll_abort_owner() -> NetworkStreamOwner {
        let thread = crate::types::DetTid::from_raw(7);
        NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        }
    }

    fn with_private_poll_fds(test: fn()) {
        std::thread::spawn(move || {
            // Other parallel tests fork without exec. Isolate before creating
            // these sockets so those children cannot retain our EOF-producing
            // aliases. Clear the copied table too: a bare unshare would instead
            // keep unrelated tests' descriptors alive on this worker.
            let closed = unsafe {
                libc::syscall(
                    libc::SYS_close_range,
                    3u32,
                    u32::MAX,
                    libc::CLOSE_RANGE_UNSHARE,
                )
            };
            assert_eq!(
                closed,
                0,
                "private fixture FD table: {}",
                io::Error::last_os_error()
            );
            test();
        })
        .join()
        .expect("private FD-table fixture worker must complete");
    }

    fn close_aborted_poll(calls: &mut Calls, call: NetworkStreamCallId, peer: OwnedFd) {
        let release = calls.release(poll_abort_owner(), call).unwrap();
        calls.finish_release(poll_abort_owner(), call).unwrap();
        let mut row = libc::pollfd {
            fd: peer.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&raw mut row, 1, 1000) };
        let mut byte = 0u8;
        let eof = unsafe {
            libc::recv(
                peer.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        drop(peer);
        assert_eq!(release.original, None);
        assert_eq!(ready, 1);
        assert_eq!(
            eof, 0,
            "the retained original was closed, not its recycled number"
        );
    }

    #[test]
    fn raw_poll_abort_requires_exact_call_owner_lease_and_closes_retained_original() {
        with_private_poll_fds(|| {
            let owner = poll_abort_owner();
            let call = NetworkStreamCallId::controlled_fixture(101);
            let other_call = NetworkStreamCallId::controlled_fixture(102);
            let missing_call = NetworkStreamCallId::controlled_fixture(103);
            let lease = NetworkStreamLeaseId::controlled_fixture(101);
            let wrong_lease = NetworkStreamLeaseId::controlled_fixture(102);
            let (original, peer) = super::super::tests::pair();
            let (other_original, other_peer) = super::super::tests::pair();
            let mut calls = Calls::default();
            calls.capture(owner, call, original).unwrap();
            calls.capture(owner, other_call, other_original).unwrap();
            calls.bind_lease(owner, call, lease).unwrap();
            let observed = calls.execute(owner, lease, Effect::PollState).unwrap();
            calls
                .confirm(owner, lease, &Effect::PollState, &observed)
                .unwrap();
            let stale = NetworkStreamOwner {
                mm: owner.mm.for_exec(owner.thread),
                ..owner
            };
            let wrong_thread = crate::types::DetTid::from_raw(8);
            let other_owner = NetworkStreamOwner {
                thread: wrong_thread,
                ..owner
            };
            for (bad_owner, bad_call, bad_lease) in [
                (owner, missing_call, lease),
                (owner, other_call, lease),
                (owner, call, wrong_lease),
                (stale, call, lease),
                (other_owner, call, lease),
            ] {
                assert!(
                    calls
                        .abort_poll_lease(bad_owner, bad_call, bad_lease)
                        .is_err()
                );
                let pending = calls.calls[&call].leases[&lease].pending.as_ref().unwrap();
                assert!(pending.confirmed);
                assert_eq!(pending.effect, Effect::PollState);
                assert_eq!(pending.result, Some(observed.clone()));
                assert_eq!(calls.calls.len(), 2);
            }
            calls.abort_poll_lease(owner, call, lease).unwrap();
            assert!(calls.calls[&call].leases.is_empty());
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            close_aborted_poll(&mut calls, call, peer);
            close_aborted_poll(&mut calls, other_call, other_peer);
            calls.settled().unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
        });
    }

    #[test]
    fn raw_poll_abort_retains_unsubmitted_running_and_unconfirmed_scans() {
        with_private_poll_fds(|| {
            let owner = poll_abort_owner();
            let call = NetworkStreamCallId::controlled_fixture(104);
            let lease = NetworkStreamLeaseId::controlled_fixture(104);
            let (original, peer) = super::super::tests::pair();
            let mut calls = Calls::default();
            calls.capture(owner, call, original).unwrap();
            calls.bind_lease(owner, call, lease).unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            assert!(calls.calls[&call].leases[&lease].pending.is_none());

            let work = calls.prepare(owner, lease, Effect::PollState).unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            assert!(calls.prepare_release(owner, call).is_err());
            let observed = work.perform();
            // Physical execution alone, without the retained completion, is not an
            // abort receipt. The actual result is kept locally until retain below.
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            assert!(
                calls.calls[&call].leases[&lease]
                    .pending
                    .as_ref()
                    .unwrap()
                    .result
                    .is_none()
            );
            calls
                .retain(owner, lease, &Effect::PollState, observed.clone())
                .unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            assert!(
                !calls.calls[&call].leases[&lease]
                    .pending
                    .as_ref()
                    .unwrap()
                    .confirmed
            );
            calls
                .confirm(owner, lease, &Effect::PollState, &observed)
                .unwrap();

            // Model a surviving worker's exact Arc reference without inventing a
            // second file handle; confirmed completion still cannot retire it.
            let retained_worker_pin = calls.calls[&call].original.as_ref().unwrap().clone();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            drop(retained_worker_pin);
            calls.abort_poll_lease(owner, call, lease).unwrap();
            close_aborted_poll(&mut calls, call, peer);
            calls.settled().unwrap();
        });
    }

    #[test]
    fn raw_poll_abort_never_launders_prepared_or_completed_wait_as_initial_scan() {
        with_private_poll_fds(|| {
            let owner = poll_abort_owner();
            let call = NetworkStreamCallId::controlled_fixture(105);
            let lease = NetworkStreamLeaseId::controlled_fixture(105);
            let (original, peer) = super::super::tests::pair();
            let mut calls = Calls::default();
            calls.capture(owner, call, original).unwrap();
            calls.bind_lease(owner, call, lease).unwrap();
            let initial = calls.execute(owner, lease, Effect::PollState).unwrap();
            calls
                .confirm(owner, lease, &Effect::PollState, &initial)
                .unwrap();
            let effect = Effect::PollWait {
                events: libc::POLLIN,
                timeout_ns: 1_000_000,
            };
            let work = calls.prepare(owner, lease, effect.clone()).unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            let observed = work.perform();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            calls
                .retain(owner, lease, &effect, observed.clone())
                .unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            calls.confirm(owner, lease, &effect, &observed).unwrap();
            assert!(calls.abort_poll_lease(owner, call, lease).is_err());
            let pending = calls.calls[&call].leases[&lease].pending.as_ref().unwrap();
            assert!(pending.confirmed);
            assert_eq!(pending.effect, effect);
            assert_eq!(pending.result, Some(observed.clone()));
            // The ordinary acknowledged-effect retirement remains available; the
            // initial-scan abort must not be a replacement for this completion.
            calls.finish_lease(owner, lease).unwrap();
            close_aborted_poll(&mut calls, call, peer);
            calls.settled().unwrap();
            assert_eq!(observed.confirmation, ResultValue::PollWait { revents: 0 });
        });
    }

    #[test]
    fn raw_poll_wait_wakes_on_retained_tcp_after_worker_start_and_numeric_replacement() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (original, peer) = super::super::tests::pair();
        let slot_owner = original.try_clone().unwrap();
        let slot = slot_owner.as_raw_fd();
        // Both numeric slots remain owned during allocation and replacement;
        // no close/allocation gap can steal a parallel test's descriptor.
        let (replacement, replacement_peer) = super::super::tests::pair();
        let flags_before = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) };
        let (started_tx, started_rx) = mpsc::sync_channel(0);
        let worker = std::thread::spawn(move || {
            let started = started_tx.send(()).is_ok();
            let observed = wait(original.as_fd(), libc::POLLIN, 750_000_000);
            let flags_after = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_GETFL) };
            (started, observed, flags_after, original)
        });
        // This orders the producer after worker start, not after a fabricated
        // kernel-blocked witness. Readiness may arrive just before ppoll entry.
        let started = started_rx.recv_timeout(Duration::from_secs(1));
        drop(started_rx);
        let replaced = unsafe { libc::dup3(replacement.as_raw_fd(), slot, libc::O_CLOEXEC) };
        drop(replacement);
        let replacement_sent = unsafe {
            libc::send(
                replacement_peer.as_raw_fd(),
                b"new".as_ptr().cast(),
                3,
                libc::MSG_NOSIGNAL,
            )
        };
        let original_sent = unsafe {
            libc::send(
                peer.as_raw_fd(),
                b"old".as_ptr().cast(),
                3,
                libc::MSG_NOSIGNAL,
            )
        };
        // Do not assert a producer/worker result before the worker is joined.
        let joined = worker.join();
        let (worker_started, observed, flags_after, original) = joined.unwrap();
        let mut old_bytes = [0u8; 3];
        let old_read = unsafe {
            libc::recv(
                original.as_raw_fd(),
                old_bytes.as_mut_ptr().cast(),
                old_bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        let mut new_bytes = [0u8; 3];
        let new_read = unsafe {
            libc::recv(
                slot_owner.as_raw_fd(),
                new_bytes.as_mut_ptr().cast(),
                new_bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        drop((original, slot_owner, peer, replacement_peer));

        assert!(started.is_ok());
        assert!(worker_started);
        assert_eq!(replaced, slot);
        assert_eq!((original_sent, replacement_sent), (3, 3));
        assert!(flags_before >= 0);
        assert_eq!(flags_after, flags_before);
        validate_wait(&observed, libc::POLLIN).unwrap();
        assert_eq!(observed.raw_return, 1);
        assert_eq!(
            observed.confirmation,
            ResultValue::PollWait {
                revents: libc::POLLIN
            }
        );
        assert_eq!((old_read, new_read), (3, 3));
        assert_eq!(&old_bytes, b"old");
        assert_eq!(&new_bytes, b"new");
    }

    #[test]
    fn raw_poll_wait_finite_timeout_does_not_add_writable_or_consume_data() {
        let (original, peer) = super::super::tests::pair();
        let empty = wait(original.as_fd(), libc::POLLIN, 1_000_000);
        let sent = unsafe {
            libc::send(
                peer.as_raw_fd(),
                b"abc".as_ptr().cast(),
                3,
                libc::MSG_NOSIGNAL,
            )
        };
        let ordinary = wait(original.as_fd(), libc::POLLRDNORM, 750_000_000);
        // Ordinary queued bytes do not satisfy a request for urgent data.
        let no_urgent = wait(original.as_fd(), libc::POLLPRI, 1_000_000);
        let mut bytes = [0u8; 3];
        let received = unsafe {
            libc::recv(
                original.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        drop((original, peer));

        validate_wait(&empty, libc::POLLIN).unwrap();
        assert_eq!(empty.raw_return, 0);
        assert_eq!(empty.confirmation, ResultValue::PollWait { revents: 0 });
        assert_eq!(sent, 3);
        validate_wait(&ordinary, libc::POLLRDNORM).unwrap();
        assert_eq!(ordinary.raw_return, 1);
        assert_eq!(
            ordinary.confirmation,
            ResultValue::PollWait {
                revents: libc::POLLRDNORM
            }
        );
        validate_wait(&no_urgent, libc::POLLPRI).unwrap();
        assert_eq!(no_urgent.raw_return, 0);
        assert_eq!(no_urgent.confirmation, ResultValue::PollWait { revents: 0 });
        assert_eq!(received, 3);
        assert_eq!(&bytes, b"abc");
    }

    #[test]
    fn raw_poll_wait_distinguishes_urgent_eof_and_requested_masks() {
        let (original, peer) = super::super::tests::pair();
        let writable = wait(original.as_fd(), libc::POLLWRNORM, 750_000_000);
        let sent = unsafe {
            libc::send(
                peer.as_raw_fd(),
                b"!".as_ptr().cast(),
                1,
                libc::MSG_NOSIGNAL | libc::MSG_OOB,
            )
        };
        let urgent = wait(original.as_fd(), libc::POLLPRI, 750_000_000);
        let mut byte = [0u8; 1];
        let received = unsafe {
            libc::recv(
                original.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_DONTWAIT | libc::MSG_OOB,
            )
        };
        let cleared = wait(original.as_fd(), libc::POLLPRI, 1_000_000);
        let shut = unsafe { libc::shutdown(peer.as_raw_fd(), libc::SHUT_WR) };
        let eof = wait(original.as_fd(), libc::POLLRDHUP, 750_000_000);
        let eof_read = unsafe {
            libc::recv(
                original.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_DONTWAIT,
            )
        };
        drop((original, peer));

        validate_wait(&writable, libc::POLLWRNORM).unwrap();
        assert_eq!(
            writable.confirmation,
            ResultValue::PollWait {
                revents: libc::POLLWRNORM
            }
        );
        assert_eq!(sent, 1);
        validate_wait(&urgent, libc::POLLPRI).unwrap();
        assert_eq!(
            urgent.confirmation,
            ResultValue::PollWait {
                revents: libc::POLLPRI
            }
        );
        assert_eq!(received, 1);
        assert_eq!(byte, [b'!']);
        validate_wait(&cleared, libc::POLLPRI).unwrap();
        assert_eq!(cleared.raw_return, 0);
        assert_eq!(shut, 0);
        validate_wait(&eof, libc::POLLRDHUP).unwrap();
        assert_eq!(
            eof.confirmation,
            ResultValue::PollWait {
                revents: libc::POLLRDHUP
            }
        );
        assert_eq!(eof_read, 0);
    }

    #[test]
    fn raw_poll_wait_refuses_unbounded_requests_and_nonmatching_completions() {
        for events in [0, NETWORK_TCP_POLL_REQUEST, libc::POLLERR | libc::POLLHUP] {
            for timeout_ns in [1, 999_999_999] {
                assert!(valid_wait_request(events, timeout_ns));
                super::super::validate(&Effect::PollWait { events, timeout_ns }).unwrap();
            }
        }
        for (events, timeout_ns) in [
            (libc::POLLIN, 0),
            (libc::POLLIN, 1_000_000_000),
            (libc::POLLIN, u64::MAX),
            (libc::POLLNVAL, 1),
            (i16::MIN, 1),
        ] {
            assert!(!valid_wait_request(events, timeout_ns));
            assert!(super::super::validate(&Effect::PollWait { events, timeout_ns }).is_err());
        }
        let valid = Observation {
            raw_return: 0,
            errno: None,
            bytes: vec![],
            confirmation: ResultValue::PollWait { revents: 0 },
            helper_copy: None,
        };
        validate_wait(&valid, libc::POLLIN).unwrap();
        for case in 0..9 {
            let mut bad = valid.clone();
            match case {
                0 => bad.raw_return = 1,
                1 => bad.errno = Some(libc::EINTR),
                2 => bad.bytes.push(1),
                3 => bad.confirmation = ResultValue::PollState { revents: 0 },
                4 => {
                    bad.raw_return = 1;
                    bad.confirmation = ResultValue::PollWait {
                        revents: libc::POLLNVAL,
                    };
                }
                5 => {
                    bad.raw_return = 1;
                    bad.confirmation = ResultValue::PollWait {
                        revents: libc::POLLOUT,
                    };
                }
                6 => {
                    bad.raw_return = 1;
                    bad.confirmation = ResultValue::PollWait { revents: i16::MIN };
                }
                7 => {
                    bad.raw_return = -1;
                    bad.errno = Some(libc::EINTR);
                    bad.confirmation = ResultValue::Errno(libc::EINTR);
                }
                _ => {
                    bad.raw_return = -1;
                    bad.errno = Some(libc::EBADF);
                    bad.confirmation = ResultValue::Errno(libc::EBADF);
                }
            }
            assert!(validate_wait(&bad, libc::POLLIN).is_err(), "case {case}");
        }
        let mut terminal = valid;
        terminal.raw_return = 1;
        terminal.confirmation = ResultValue::PollWait {
            revents: libc::POLLERR | libc::POLLHUP,
        };
        validate_wait(&terminal, 0).unwrap();
        terminal.raw_return = 2;
        assert!(validate_wait(&terminal, 0).is_err());
    }

    #[test]
    fn raw_poll_retained_tcp_observes_aliases_eof_and_never_consumes_payload() {
        let (original, peer) = super::super::tests::pair();
        let before = observe(original.as_fd());
        validate(&before).unwrap();
        let ResultValue::PollState { revents } = before.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLOUT | libc::POLLWRNORM),
            libc::POLLOUT | libc::POLLWRNORM
        );
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM | libc::POLLPRI),
            0
        );
        assert_eq!(
            unsafe {
                libc::send(
                    peer.as_raw_fd(),
                    b"abc".as_ptr().cast(),
                    3,
                    libc::MSG_NOSIGNAL,
                )
            },
            3
        );
        let mut wait = libc::pollfd {
            fd: original.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let data = observe(original.as_fd());
        validate(&data).unwrap();
        let ResultValue::PollState { revents } = data.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM),
            libc::POLLIN | libc::POLLRDNORM
        );
        let mut bytes = [0u8; 3];
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    3,
                    libc::MSG_DONTWAIT,
                )
            },
            3
        );
        assert_eq!(&bytes, b"abc");
        assert_eq!(
            unsafe {
                libc::send(
                    peer.as_raw_fd(),
                    b"!".as_ptr().cast(),
                    1,
                    libc::MSG_NOSIGNAL | libc::MSG_OOB,
                )
            },
            1
        );
        wait.events = libc::POLLPRI;
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let urgent = observe(original.as_fd());
        validate(&urgent).unwrap();
        let ResultValue::PollState { revents } = urgent.confirmation else {
            panic!("poll state")
        };
        assert_eq!(revents & libc::POLLPRI, libc::POLLPRI);
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    1,
                    libc::MSG_DONTWAIT | libc::MSG_OOB,
                )
            },
            1
        );
        assert_eq!(bytes[0], b'!');
        let cleared = observe(original.as_fd());
        validate(&cleared).unwrap();
        let ResultValue::PollState { revents } = cleared.confirmation else {
            panic!("poll state")
        };
        assert_eq!(revents & libc::POLLPRI, 0);
        assert_eq!(
            unsafe { libc::shutdown(peer.as_raw_fd(), libc::SHUT_WR) },
            0
        );
        wait.events = libc::POLLIN;
        assert_eq!(unsafe { libc::poll(&raw mut wait, 1, 1000) }, 1);
        let eof = observe(original.as_fd());
        validate(&eof).unwrap();
        let ResultValue::PollState { revents } = eof.confirmation else {
            panic!("poll state")
        };
        assert_eq!(
            revents & (libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP),
            libc::POLLIN | libc::POLLRDNORM | libc::POLLRDHUP
        );
        assert_eq!(
            unsafe {
                libc::recv(
                    original.as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    3,
                    libc::MSG_DONTWAIT,
                )
            },
            0
        );
    }

    #[test]
    fn raw_poll_confirmation_refuses_count_errno_bytes_unknown_mask_and_nval() {
        let valid = Observation {
            raw_return: 0,
            errno: None,
            bytes: vec![],
            confirmation: ResultValue::PollState { revents: 0 },
            helper_copy: None,
        };
        validate(&valid).unwrap();
        for index in 0..6 {
            let mut bad = valid.clone();
            match index {
                0 => bad.raw_return = 1,
                1 => bad.errno = Some(libc::EINTR),
                2 => bad.bytes.push(7),
                3 => {
                    bad.confirmation = ResultValue::PollState {
                        revents: libc::POLLNVAL,
                    }
                }
                4 => bad.confirmation = ResultValue::PollState { revents: i16::MIN },
                _ => {
                    bad.confirmation = ResultValue::PollState {
                        revents: libc::POLLOUT,
                    }
                }
            }
            assert!(validate(&bad).is_err(), "case {index}");
        }
        let mut terminal = valid;
        terminal.raw_return = 1;
        terminal.confirmation = ResultValue::PollState {
            revents: libc::POLLERR | libc::POLLHUP | libc::POLLPRI,
        };
        validate(&terminal).unwrap();
        terminal.raw_return = 2;
        assert!(validate(&terminal).is_err());
    }
}
