//! Actual Global transactions and real process_vm_writev effects. The backend
//! original/restored frame and physical hold are explicit controlled premises;
//! these controls do not qualify Reverie's native callback or provider.
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use reverie::syscalls::FollowedStore;
use reverie::syscalls::NativeUserReadRefusal;
use reverie::syscalls::NativeUserStoreOutcome;
use reverie::syscalls::NativeUserStoreRefusal;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::tool_global::SharedReceivePreparation;

pub(super) struct ControlledStore {
    raw: (Sysno, reverie::syscalls::SyscallArgs),
    restored: bool,
    tid: i32,
    maximum_write: usize,
    post_error: bool,
    valid: AtomicBool,
    used: AtomicBool,
    writes: AtomicUsize,
    checks: AtomicUsize,
}
impl ControlledStore {
    fn new(
        f: &ReplayIssuerFixture,
        capacity: usize,
        restored: bool,
        maximum_write: usize,
        post_error: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            raw: scalar_read(f.binding.slot.fd, f.pages.at(128), capacity).into_parts(),
            restored,
            tid: f.tid.as_raw(),
            maximum_write,
            post_error,
            valid: AtomicBool::new(true),
            used: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
        })
    }
    pub(super) fn with<R>(
        &self,
        original: Syscall,
        restored: bool,
        action: impl FnOnce(&mut dyn FollowedStore) -> R,
    ) -> Result<R, NativeUserStoreRefusal> {
        assert_eq!(original.into_parts(), self.raw, "controlled original tuple");
        assert_eq!(restored, self.restored, "controlled backend context class");
        Ok(action(&mut BorrowedStore(self)))
    }
}
struct BorrowedStore<'a>(&'a ControlledStore);
fn state(error: Errno) -> NativeUserStoreRefusal {
    NativeUserStoreRefusal::Evidence(NativeUserReadRefusal::TargetState(error))
}
impl FollowedStore for BorrowedStore<'_> {
    fn validate_context(&self) -> Result<(), NativeUserStoreRefusal> {
        self.0.checks.fetch_add(1, Ordering::SeqCst);
        if !self.0.valid.load(Ordering::SeqCst) || self.0.used.load(Ordering::SeqCst) {
            return Err(state(Errno::EBUSY));
        }
        Ok(())
    }
    fn store(&mut self, bytes: &[u8]) -> NativeUserStoreOutcome {
        if let Err(error) = self.validate_context() {
            return NativeUserStoreOutcome::Refused(error);
        }
        assert!(!self.0.used.swap(true, Ordering::SeqCst));
        assert!(bytes.len() <= self.0.raw.1.arg2);
        let length = bytes.len().min(self.0.maximum_write);
        let local = libc::iovec {
            iov_base: bytes.as_ptr().cast_mut().cast(),
            iov_len: length,
        };
        let remote = libc::iovec {
            iov_base: self.0.raw.1.arg1 as *mut libc::c_void,
            iov_len: length,
        };
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

fn configure(f: &mut ReplayIssuerFixture, nonblocking: bool) {
    finite_replay_fixture_timeout(f, 5, 0);
    let mut e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    let control = e
        .begin_socket_controls(f.root.owner(), vec![f.binding.open_file])
        .unwrap()[0]
        .1;
    for option in [
        crate::network_replay::NetworkStreamSocketOption::ReceiveBuffer(131072),
        crate::network_replay::NetworkStreamSocketOption::ReceiveLowWater(3),
    ] {
        e.submit_stream_physical(
            f.root.owner(),
            control,
            NetworkStreamPhysicalEffect::SetSocketOption { option },
        )
        .unwrap();
        e.confirm_stream_physical(
            f.root.owner(),
            control,
            NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
        )
        .unwrap();
    }
    e.finish_socket_control(
        f.root.owner(),
        control,
        crate::network_replay::NetworkSocketControlFinish::Unchanged,
    )
    .unwrap();
    f.thread
        .with_detfd(f.binding.slot.fd, |fd| fd.set_nonblocking(nonblocking))
        .unwrap();
    let crate::network_replay::NetworkFdReadBegin::Admitted(read) = e
        .begin_fd_read(f.root.owner(), f.root.files(), f.binding.slot.fd)
        .unwrap()
    else {
        panic!("configured reader")
    };
    f.read = *read;
}
async fn begin(
    f: &ReplayIssuerFixture,
    guest: &ScalarForegroundGuest<'_>,
    capacity: usize,
) -> crate::tool_global::SharedReceiveInvocation {
    let expected = f
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&f.read)
        .unwrap();
    f.state
        .begin_shared_replay_receive(
            guest,
            scalar_read(f.binding.slot.fd, f.pages.at(128), capacity).into(),
            f.read.clone(),
            expected,
        )
        .await
        .unwrap()
}
async fn store(
    f: &ReplayIssuerFixture,
    guest: &ScalarForegroundGuest<'_>,
    invocation: &crate::tool_global::SharedReceiveInvocation,
    restored: bool,
) -> Result<usize, NetworkRpcError> {
    let prepared = f
        .state
        .prepare_shared_replay_receive(f.tid, guest.thread_state(), invocation)
        .await
        .unwrap();
    let SharedReceivePreparation::Store(prepared) = prepared else {
        panic!("selected bytes")
    };
    f.state
        .store_shared_replay_receive(guest, invocation, prepared, restored)
}
fn consumed(f: &ReplayIssuerFixture) -> u64 {
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .controlled_replay_delivery_state(f.binding.open_file)
        .0
}

#[tokio::test]
async fn shared_receive_global_saved_policy_and_actual_full_write_on_same_turn() {
    for restored in [false, true] {
        let mut fixture = super::shared_source::selected().await;
        let f = &mut fixture.f;
        configure(f, false);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let writer = ControlledStore::new(f, 3, restored, 3, false);
        guest.shared_store = Some(writer.clone());
        let time = f.state.global_time.lock().unwrap().as_nanos();
        let turn = f.state.sched.lock().unwrap().turn;
        let invocation = begin(f, &guest, 3).await;
        assert_eq!(invocation.policy().target(), 3);
        assert_eq!(invocation.policy().started(), time);
        assert_eq!(
            invocation.policy().deadline(),
            Some(LogicalTime::from_nanos(time.as_nanos() + 5_000_000_000))
        );
        assert!(!invocation.policy().nonblocking());
        assert_eq!(guest.range_calls.lock().unwrap().len(), 2);
        assert_eq!(store(f, &guest, &invocation, restored).await.unwrap(), 3);
        assert_eq!(f.pages.bytes(128, 3), b"abc");
        assert_eq!(f.pages.bytes(0, 128), vec![0xa5; 128]);
        assert_eq!(f.pages.bytes(131, 8192 - 131), vec![0xa5; 8192 - 131]);
        assert_eq!(writer.writes.load(Ordering::SeqCst), 1);
        assert_eq!(consumed(f), 3);
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
    }
}

#[tokio::test]
async fn shared_receive_global_actual_partial_and_failed_postcheck_retain_custody() {
    for (maximum, post_error) in [(2, false), (3, true)] {
        let mut fixture = super::shared_source::selected().await;
        let f = &mut fixture.f;
        configure(f, false);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let writer = ControlledStore::new(f, 3, false, maximum, post_error);
        guest.shared_store = Some(writer.clone());
        let invocation = begin(f, &guest, 3).await;
        assert!(store(f, &guest, &invocation, false).await.is_err());
        assert_eq!(writer.writes.load(Ordering::SeqCst), 1);
        assert_eq!(f.pages.bytes(128, maximum), b"abc"[..maximum]);
        assert_eq!(
            f.pages.bytes(128 + maximum, 8192 - 128 - maximum),
            vec![0xa5; 8192 - 128 - maximum]
        );
        assert_eq!(consumed(f), 0);
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .finish()
                .is_err()
        );
        assert!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .join_shared_foreground_prefix(
                    f.root.clone(),
                    f.state.network_engine.as_ref().unwrap(),
                    None
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn shared_receive_global_no_store_checks_context_inside_commit() {
    for valid in [false, true] {
        let mut fixture = super::shared_source::selected().await;
        let f = &mut fixture.f;
        configure(f, true);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let writer = ControlledStore::new(f, 8, false, 8, false);
        guest.shared_store = Some(writer);
        let invocation = begin(f, &guest, 8).await;
        assert_eq!(store(f, &guest, &invocation, false).await.unwrap(), 8);
        let crate::network_replay::NetworkFdReadBegin::Admitted(read) = f
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .begin_fd_read(f.root.owner(), f.root.files(), f.binding.slot.fd)
            .unwrap()
        else {
            panic!("next original reader")
        };
        f.read = *read;
        let writer = ControlledStore::new(f, 3, false, 3, false);
        writer.valid.store(valid, Ordering::SeqCst);
        guest.shared_store = Some(writer.clone());
        let invocation = begin(f, &guest, 3).await;
        let SharedReceivePreparation::NoStore(prepared) = f
            .state
            .prepare_shared_replay_receive(f.tid, guest.thread_state(), &invocation)
            .await
            .unwrap()
        else {
            panic!("empty nonblocking result")
        };
        let result =
            f.state
                .complete_shared_replay_receive_no_store(&guest, &invocation, *prepared, false);
        if valid {
            assert_eq!(
                result.unwrap(),
                crate::network_replay::shared_waits::SharedNoStoreResult::WouldBlock {
                    timed_out: false
                }
            );
        } else {
            assert!(result.is_err());
        }
        assert_eq!(writer.checks.load(Ordering::SeqCst), 1);
        assert_eq!(writer.writes.load(Ordering::SeqCst), 0);
        assert!(!writer.used.load(Ordering::SeqCst));
        assert_eq!(consumed(f), 8);
        assert_eq!(f.pages.bytes(128, 8), b"abcdefgh");
        let joined = f
            .state
            .network_runtime
            .as_ref()
            .unwrap()
            .join_shared_foreground_prefix(
                f.root.clone(),
                f.state.network_engine.as_ref().unwrap(),
                None,
            )
            .await;
        assert_eq!(joined.is_ok(), valid);
    }
}

#[tokio::test]
async fn shared_receive_global_range_fault_preserves_original_reader() {
    let mut fixture = super::shared_source::selected().await;
    let f = &mut fixture.f;
    configure(f, false);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    guest.range_verdict = reverie::OriginalReadRangeVerdict::Fault;
    let expected = f
        .thread
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&f.read)
        .unwrap();
    assert!(
        f.state
            .begin_shared_replay_receive(
                &guest,
                scalar_read(f.binding.slot.fd, f.pages.at(128), 3).into(),
                f.read.clone(),
                expected
            )
            .await
            .is_err()
    );
    assert_eq!(guest.range_calls.lock().unwrap().len(), 1);
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    engine
        .validate_fd_read_grant(f.root.owner(), &f.read)
        .unwrap();
    engine
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    assert_eq!(f.pages.bytes(0, 8192), vec![0xa5; 8192]);
}
