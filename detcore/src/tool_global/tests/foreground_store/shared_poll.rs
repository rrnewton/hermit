//! Real Global/Normal/FD transactions and process_vm input/output on a mapped
//! original row. The backend original context, complete hold, true capture join
//! and typed peer join are explicit supplied premises, not native R evidence.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use detcore_model::network_trace::*;
use reverie::syscalls::AddrMut;
use reverie::syscalls::FollowedPollStore;
use reverie::syscalls::NativeUserReadError;
use reverie::syscalls::NativeUserReadRefusal;
use reverie::syscalls::NativeUserStoreOutcome;
use reverie::syscalls::NativeUserStoreRefusal;
use reverie::syscalls::OriginalPollInput;
use reverie::syscalls::Poll;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::*;

pub(super) struct ControlledPoll {
    raw: (Sysno, reverie::syscalls::SyscallArgs),
    tid: i32,
    input: Mutex<Option<OriginalPollInput>>,
    maximum_write: usize,
    post_error: bool,
    valid: AtomicBool,
    used: AtomicBool,
    captures: AtomicUsize,
    joins: AtomicUsize,
    writes: AtomicUsize,
    events: Mutex<Vec<&'static str>>,
}
impl ControlledPoll {
    fn new(
        f: &ReplayIssuerFixture,
        timeout: i32,
        maximum_write: usize,
        post_error: bool,
    ) -> Arc<Self> {
        let row = libc::pollfd {
            fd: f.binding.slot.fd,
            events: libc::POLLIN,
            revents: 0xa5a5_u16 as i16,
        };
        unsafe { std::ptr::write(f.pages.at(128) as *mut libc::pollfd, row) };
        let raw = Poll::new()
            .with_fds(AddrMut::from_raw(f.pages.at(128) as usize))
            .with_nfds(1)
            .with_timeout(timeout)
            .into_parts();
        Arc::new(Self {
            raw,
            tid: f.tid.as_raw(),
            input: Mutex::new(None),
            maximum_write,
            post_error,
            valid: AtomicBool::new(true),
            used: AtomicBool::new(false),
            captures: AtomicUsize::new(0),
            joins: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            events: Mutex::new(Vec::new()),
        })
    }
    fn original(&self) -> Syscall {
        Syscall::from_raw(self.raw.0, self.raw.1)
    }
    fn read_row(&self) -> Result<OriginalPollInput, Errno> {
        let mut row = std::mem::MaybeUninit::<libc::pollfd>::zeroed();
        let local = libc::iovec {
            iov_base: row.as_mut_ptr().cast(),
            iov_len: std::mem::size_of::<libc::pollfd>(),
        };
        let remote = libc::iovec {
            iov_base: self.raw.1.arg0 as *mut libc::c_void,
            iov_len: local.iov_len,
        };
        let length =
            Errno::result(unsafe { libc::process_vm_readv(self.tid, &local, 1, &remote, 1, 0) })?
                as usize;
        if length != local.iov_len {
            return Err(Errno::EFAULT);
        }
        let row = unsafe { row.assume_init() };
        Ok(OriginalPollInput {
            fd: row.fd,
            events: row.events,
            timeout_millis: self.raw.1.arg2 as i32,
        })
    }
    pub(super) fn capture(
        &self,
        original: Syscall,
        retention: Box<dyn Send + Sync>,
    ) -> Result<OriginalPollInput, NativeUserReadError> {
        assert_eq!(original.into_parts(), self.raw);
        assert_eq!(self.captures.fetch_add(1, Ordering::SeqCst), 0);
        assert!(!self.used.load(Ordering::SeqCst));
        assert_eq!(self.joins.load(Ordering::SeqCst), 1);
        assert_eq!(
            self.events.lock().unwrap().last(),
            Some(&"initial-peer-join")
        );
        self.events.lock().unwrap().push("capture-read");
        let input = self
            .read_row()
            .map_err(|e| NativeUserReadError::Refused(NativeUserReadRefusal::TargetState(e)))?;
        assert!(self.input.lock().unwrap().replace(input).is_none());
        // The fixture supplies backend stopped-cohort/true-join authority.
        // The actual retention remains owned until after the real input read.
        drop(retention);
        Ok(input)
    }
    pub(super) fn join(&self, original: Syscall) -> Result<(), reverie::Error> {
        assert_eq!(original.into_parts(), self.raw);
        assert!(self.valid.load(Ordering::SeqCst));
        assert!(!self.used.load(Ordering::SeqCst));
        let prior = self.joins.fetch_add(1, Ordering::SeqCst);
        let event = if self.input.lock().unwrap().is_none() {
            assert_eq!(self.captures.load(Ordering::SeqCst), 0);
            assert_eq!(prior, 0, "initial join is before the only capture");
            "initial-peer-join"
        } else {
            assert_eq!(self.captures.load(Ordering::SeqCst), 1);
            assert!(prior >= 1, "output join retains the initial join history");
            "output-peer-join"
        };
        self.events.lock().unwrap().push(event);
        Ok(())
    }
    pub(super) fn with<R>(
        &self,
        original: Syscall,
        action: impl FnOnce(&mut dyn FollowedPollStore) -> R,
    ) -> Result<R, NativeUserStoreRefusal> {
        assert_eq!(original.into_parts(), self.raw);
        assert!(
            self.joins.load(Ordering::SeqCst) >= 2,
            "initial and output typed peer joins precede output"
        );
        self.events.lock().unwrap().push("held-writer");
        Ok(action(&mut BorrowedPoll(self)))
    }
}
fn refusal(errno: Errno) -> NativeUserStoreRefusal {
    NativeUserStoreRefusal::Evidence(NativeUserReadRefusal::TargetState(errno))
}
struct BorrowedPoll<'a>(&'a ControlledPoll);
impl FollowedPollStore for BorrowedPoll<'_> {
    fn input(&self) -> OriginalPollInput {
        self.0.input.lock().unwrap().expect("actual captured input")
    }
    fn validate_context(&self) -> Result<(), NativeUserStoreRefusal> {
        self.0.events.lock().unwrap().push("context-check");
        if !self.0.valid.load(Ordering::SeqCst) || self.0.used.load(Ordering::SeqCst) {
            return Err(refusal(Errno::EBUSY));
        }
        if self.0.read_row().map_err(refusal)? != self.input() {
            return Err(refusal(Errno::ESTALE));
        }
        Ok(())
    }
    fn store_revents(&mut self, revents: i16) -> NativeUserStoreOutcome {
        if let Err(error) = self.validate_context() {
            return NativeUserStoreOutcome::Refused(error);
        }
        assert!(!self.0.used.swap(true, Ordering::SeqCst));
        let bytes = revents.to_ne_bytes();
        let length = self.0.maximum_write.min(bytes.len());
        let local = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: length,
        };
        let remote = libc::iovec {
            iov_base: (self.0.raw.1.arg0 + std::mem::offset_of!(libc::pollfd, revents))
                as *mut libc::c_void,
            iov_len: length,
        };
        self.0.events.lock().unwrap().push("actual-write");
        self.0.writes.fetch_add(1, Ordering::SeqCst);
        let raw =
            Errno::result(unsafe { libc::process_vm_writev(self.0.tid, &local, 1, &remote, 1, 0) })
                .map(|n| n as usize);
        NativeUserStoreOutcome::Attempted {
            raw,
            postcheck: if self.0.post_error {
                Err(Errno::EBUSY)
            } else {
                Ok(())
            },
        }
    }
}

fn poll_trace(trace: &mut NetworkTraceV4, samples: &[(u64, i16)]) {
    // Build a different valid immutable trace before any engine/census/effect.
    // The old selected() transform remains identity and every old test intact.
    trace.inputs.truncate(1);
    trace.outputs.clear();
    trace.native_receive_observations.clear();
    let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } = &mut trace.release_model
    else {
        panic!("shared constructor premise");
    };
    nodes.truncate(2);
    let base = trace.epoch_global_time().unwrap();
    for &(nanos, revents) in samples {
        let ordinal = trace.inputs.len() as u64;
        let cut = NetworkReceiveEntryCutV4(trace.release_model.nodes().len() as u64);
        let prerequisites = trace.entry_frontier(cut).unwrap();
        trace.inputs.push(NetworkInputEventV4 {
            ordinal,
            channel: trace.channels[0].id,
            release: NetworkReleaseV4 {
                not_before_global_time: LogicalTime::from_nanos(base.as_nanos() + nanos),
                receive_entry_cut: cut,
                prerequisites: prerequisites.clone(),
            },
            event: NetworkInputKindV2::SharedRawTcpPollState {
                consumed_prefix: 0,
                revents,
                control_generation: 0,
                receive_low_water: 1,
            },
        });
        let NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes } =
            &mut trace.release_model
        else {
            unreachable!();
        };
        nodes.push(NetworkReleaseNodeV4 {
            id: NetworkReleaseNodeIdV4(cut.0),
            kind: NetworkReleaseNodeKindV4::Input {
                input_ordinal: ordinal,
            },
            prerequisites,
        });
    }
    trace.validate().unwrap();
}

async fn begin(
    f: &ReplayIssuerFixture,
    guest: &mut ScalarForegroundGuest<'_>,
    backend: &ControlledPoll,
) -> crate::tool_global::SharedPollInvocation {
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    let prepared = f
        .state
        .prepare_shared_poll_input(guest, backend.original())
        .await
        .unwrap();
    assert_eq!(prepared.original().into_parts(), backend.raw);
    guest
        .join_followed_observation_timers(prepared.original())
        .await
        .unwrap();
    let copied = guest
        .capture_original_followed_poll(prepared.original(), prepared.retention())
        .await
        .unwrap();
    assert_eq!(copied.fd, f.binding.slot.fd);
    assert_eq!(copied.events, libc::POLLIN);
    assert_eq!(backend.captures.load(Ordering::SeqCst), 1);
    let captured = f
        .state
        .finish_shared_poll_input(guest, prepared, copied)
        .unwrap();
    let reply = super::super::super::network_request(
        guest,
        NetworkRequest::BeginOrdinaryFdRead {
            files: f.binding.slot.files,
            fd: captured.input().fd,
        },
    )
    .await
    .unwrap();
    let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
    else {
        panic!("actual captured FD read refused");
    };
    let expected = guest
        .thread_state()
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&read)
        .unwrap();
    let saved_read = (*read).clone();
    let invocation = f
        .state
        .begin_shared_poll_call(guest, captured, *read, expected)
        .await
        .unwrap();
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .validate_fd_read_grant(f.root.owner(), &saved_read)
            .is_err()
    );
    invocation
}
fn unchanged_consumption(f: &ReplayIssuerFixture) {
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .controlled_replay_delivery_state(f.binding.open_file)
            .0,
        0
    );
}
fn assert_retained(f: &ReplayIssuerFixture, call: NetworkStreamCallId) {
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine.native_capture_fixture_counts(f.binding.open_file).0,
        1
    );
    assert!(
        engine
            .begin_stream_call_release(f.root.owner(), call)
            .is_err()
    );
    assert!(engine.finish().is_err());
}
fn assert_canaries(f: &ReplayIssuerFixture, before: &[u8], revents: i16, length: usize) {
    let offset = 128 + std::mem::offset_of!(libc::pollfd, revents);
    let mut expected = before.to_vec();
    expected[offset..offset + length].copy_from_slice(&revents.to_ne_bytes()[..length]);
    assert_eq!(f.pages.bytes(0, 8192), expected);
}

#[tokio::test]
async fn shared_poll_global_captures_original_row_and_stores_exact_zero_or_ready_without_turn() {
    for (mask, timeout, count) in [(0, 0, 0), (libc::POLLIN, 5000, 1)] {
        let fixture =
            shared_source::selected_with_transform(|trace| poll_trace(trace, &[(0, mask)])).await;
        let f = &fixture.f;
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let backend = ControlledPoll::new(f, timeout, 2, false);
        guest.shared_poll = Some(backend.clone());
        let before = f.pages.bytes(0, 8192);
        let time = f.state.global_time.lock().unwrap().as_nanos();
        let turn = f.state.sched.lock().unwrap().turn;
        let invocation = begin(f, &mut guest, &backend).await;
        assert_eq!(
            invocation.deadline(),
            LogicalTime::from_nanos(time.as_nanos() + timeout as u64 * 1_000_000)
        );
        // revents is output-only; changing its old value cannot alter input.
        unsafe { (*(f.pages.at(128) as *mut libc::pollfd)).revents = 0x1234 };
        let prepared = f
            .state
            .prepare_shared_poll_attempt(&guest, &invocation)
            .await
            .unwrap();
        guest
            .join_followed_observation_timers(invocation.original())
            .await
            .unwrap();
        assert_eq!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .unwrap(),
            Some(count)
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        assert_canaries(f, &before, mask, 2);
        unchanged_consumption(f);
        assert_eq!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_capture_fixture_counts(f.binding.open_file)
                .0,
            0
        );
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), time);
        assert_eq!(f.state.sched.lock().unwrap().turn, turn);
        f.state
            .network_runtime
            .as_ref()
            .unwrap()
            .join_shared_foreground_prefix(
                f.root.clone(),
                f.state.network_engine.as_ref().unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .is_err()
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        let events = backend.events.lock().unwrap();
        assert!(backend.joins.load(Ordering::SeqCst) >= 2);
        let position = |name| events.iter().position(|e| *e == name).unwrap();
        assert_eq!(events[0], "initial-peer-join");
        assert!(position("initial-peer-join") < position("capture-read"));
        assert!(position("capture-read") < position("output-peer-join"));
        assert!(position("output-peer-join") < position("actual-write"));
    }
}

#[tokio::test]
async fn shared_poll_global_partial_or_failed_postcheck_retains_real_effect_and_interval() {
    for (maximum, post_error) in [(1, false), (2, true)] {
        let fixture =
            shared_source::selected_with_transform(|trace| poll_trace(trace, &[(0, libc::POLLIN)]))
                .await;
        let f = &fixture.f;
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let backend = ControlledPoll::new(f, 5000, maximum, post_error);
        guest.shared_poll = Some(backend.clone());
        let before = f.pages.bytes(0, 8192);
        let invocation = begin(f, &mut guest, &backend).await;
        let prepared = f
            .state
            .prepare_shared_poll_attempt(&guest, &invocation)
            .await
            .unwrap();
        guest
            .join_followed_observation_timers(invocation.original())
            .await
            .unwrap();
        assert!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .is_err()
        );
        drop(prepared);
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        assert_canaries(f, &before, libc::POLLIN, maximum);
        unchanged_consumption(f);
        assert_retained(f, invocation.call());
        assert!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .join_shared_foreground_prefix(
                    f.root.clone(),
                    f.state.network_engine.as_ref().unwrap(),
                    None,
                )
                .await
                .is_err()
        );
        let ran = Arc::new(AtomicBool::new(false));
        assert!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_shared_source_worker_submission(ran.clone())
                .is_err()
        );
        assert!(!ran.load(Ordering::SeqCst));
        assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn shared_poll_global_changed_row_grant_or_physical_owner_refuses_before_store() {
    for variant in 0..4 {
        let fixture =
            shared_source::selected_with_transform(|trace| poll_trace(trace, &[(0, libc::POLLIN)]))
                .await;
        let f = &fixture.f;
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let backend = ControlledPoll::new(f, 5000, 2, false);
        guest.shared_poll = Some(backend.clone());
        let invocation = begin(f, &mut guest, &backend).await;
        let prepared = f
            .state
            .prepare_shared_poll_attempt(&guest, &invocation)
            .await
            .unwrap();
        guest
            .join_followed_observation_timers(invocation.original())
            .await
            .unwrap();
        match variant {
            0 => unsafe { (*(f.pages.at(128) as *mut libc::pollfd)).fd += 1 },
            1 => unsafe { (*(f.pages.at(128) as *mut libc::pollfd)).events = libc::POLLOUT },
            2 => f
                .state
                .sched
                .lock()
                .unwrap()
                .controlled_shared_foreground_grant(&f.root),
            3 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .forget_task(f.root.owner()),
            _ => unreachable!(),
        }
        let before = f.pages.bytes(0, 8192);
        assert!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .is_err(),
            "variant {variant}"
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.pages.bytes(0, 8192), before);
        unchanged_consumption(f);
        assert_retained(f, invocation.call());
    }
}

#[tokio::test]
async fn shared_poll_global_pending_does_not_write_and_timeout_requires_actual_final_scan() {
    for final_scan in [false, true] {
        let samples = if final_scan {
            vec![(0, 0), (5_000_000_000, 0)]
        } else {
            vec![(0, 0)]
        };
        let fixture =
            shared_source::selected_with_transform(|trace| poll_trace(trace, &samples)).await;
        let f = &fixture.f;
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let backend = ControlledPoll::new(f, 5000, 2, false);
        guest.shared_poll = Some(backend.clone());
        let before = f.pages.bytes(0, 8192);
        let invocation = begin(f, &mut guest, &backend).await;
        let prepared = f
            .state
            .prepare_shared_poll_attempt(&guest, &invocation)
            .await
            .unwrap();
        guest
            .join_followed_observation_timers(invocation.original())
            .await
            .unwrap();
        assert_eq!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .unwrap(),
            None
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.pages.bytes(0, 8192), before);
        // Controlled logical boundary, not a physical timer or scheduler wake
        // qualification: test the actual Global selection at the saved deadline.
        f.state
            .global_time
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_secs(5));
        assert_eq!(
            f.state.global_time.lock().unwrap().as_nanos(),
            invocation.deadline()
        );
        let at_deadline = f
            .state
            .store_shared_poll_attempt(&guest, &invocation, &prepared);
        if final_scan {
            assert_eq!(at_deadline.unwrap(), Some(0));
            assert_canaries(f, &before, 0, 2);
            assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
        } else {
            assert!(at_deadline.is_err());
            assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
            assert_eq!(f.pages.bytes(0, 8192), before);
            assert_retained(f, invocation.call());
        }
        unchanged_consumption(f);
    }
}

#[tokio::test]
async fn shared_poll_global_real_deadline_wait_issues_fresh_normal_before_equal_and_after() {
    for elapsed in [0, 5_000_000_000, 5_000_000_001] {
        let fixture =
            shared_source::selected_with_transform(|trace| poll_trace(trace, &[(0, 0)])).await;
        let f = &fixture.f;
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let backend = ControlledPoll::new(f, 5000, 2, false);
        guest.shared_poll = Some(backend.clone());
        let before = f.pages.bytes(0, 8192);
        let invocation = begin(f, &mut guest, &backend).await;
        let prepared = f
            .state
            .prepare_shared_poll_attempt(&guest, &invocation)
            .await
            .unwrap();
        guest
            .join_followed_observation_timers(invocation.original())
            .await
            .unwrap();
        assert_eq!(
            f.state
                .store_shared_poll_attempt(&guest, &invocation, &prepared)
                .unwrap(),
            None
        );
        let interests = f
            .state
            .suspend_prepared_shared_poll(&guest, &invocation, prepared)
            .await
            .unwrap();
        assert_eq!(
            interests,
            vec![crate::resources::NetworkWaitKind::PollReadable]
        );
        // Controlled initial clock position; the actual production scheduler
        // decides whether to park, fast-forward its timer, and issue Normal.
        f.state
            .global_time
            .lock()
            .unwrap()
            .add_extra_time(std::time::Duration::from_nanos(elapsed));
        let (parked, _) = f
            .state
            .sched
            .lock()
            .unwrap()
            .controlled_shared_poll_deadline_continuation(
                f.root.owner(),
                invocation.call(),
                invocation.deadline(),
                f.state.network_engine.as_ref().unwrap().clone(),
                &f.state.global_time,
            );
        assert_eq!(parked, elapsed < 5_000_000_000);
        assert_eq!(
            f.state.global_time.lock().unwrap().as_nanos(),
            LogicalTime::from_nanos(
                invocation.deadline().as_nanos() + elapsed.saturating_sub(5_000_000_000)
            )
        );
        f.state
            .resume_shared_poll(f.tid, &f.thread, &invocation)
            .await
            .unwrap();
        assert!(
            f.state
                .resume_shared_poll(f.tid, &f.thread, &invocation)
                .await
                .is_err()
        );
        assert_eq!(backend.writes.load(Ordering::SeqCst), 0);
        assert_eq!(f.pages.bytes(0, 8192), before);
        unchanged_consumption(f);
        assert_retained(f, invocation.call());
    }
}
