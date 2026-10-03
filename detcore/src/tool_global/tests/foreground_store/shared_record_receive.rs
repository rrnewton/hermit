//! Actual Global Record admission, native workers, TCP Peek/cursor/Poll/Drain,
//! and process_vm_writev. Original installation/copy5 geometry and the backend
//! held original/restored frame are explicitly controlled premises. No BPF,
//! native Reverie callback, timer or whole-M2 qualification is claimed.
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use detcore_model::network_trace::NetworkInputKindV2;
use detcore_model::network_trace::NetworkNativeCopyDispositionV4;
use reverie::syscalls::FollowedStore;
use reverie::syscalls::NativeUserReadRefusal;
use reverie::syscalls::NativeUserStoreOutcome;
use reverie::syscalls::NativeUserStoreRefusal;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::shared_waits::ControlledRecordReceiveFixture;
use crate::tool_global::ScalarReceive;
use crate::tool_global::SharedReceiveInvocation;
use crate::tool_global::SharedRecordReceiveEffect;

pub(super) struct ControlledRecordStore {
    raw: (Sysno, reverie::syscalls::SyscallArgs),
    restored: bool,
    tid: i32,
    maximum: usize,
    post_error: bool,
    valid: AtomicBool,
    used: AtomicBool,
    writes: AtomicUsize,
    checks: AtomicUsize,
}
impl ControlledRecordStore {
    fn new(
        f: &ReplayIssuerFixture,
        original: ScalarReceive,
        restored: bool,
        maximum: usize,
        post_error: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            raw: original.into_parts(),
            restored,
            tid: f.tid.as_raw(),
            maximum,
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
        assert_eq!(
            original.into_parts(),
            self.raw,
            "exact original scalar tuple"
        );
        assert_eq!(
            restored, self.restored,
            "explicit controlled backend context"
        );
        Ok(action(&mut Writer(self)))
    }
}
struct Writer<'a>(&'a ControlledRecordStore);
impl FollowedStore for Writer<'_> {
    fn validate_context(&self) -> Result<(), NativeUserStoreRefusal> {
        self.0.checks.fetch_add(1, Ordering::SeqCst);
        if !self.0.valid.load(Ordering::SeqCst) || self.0.used.load(Ordering::SeqCst) {
            return Err(NativeUserStoreRefusal::Evidence(
                NativeUserReadRefusal::TargetState(Errno::EBUSY),
            ));
        }
        Ok(())
    }
    fn store(&mut self, bytes: &[u8]) -> NativeUserStoreOutcome {
        if let Err(error) = self.validate_context() {
            return NativeUserStoreOutcome::Refused(error);
        }
        assert!(!self.0.used.swap(true, Ordering::SeqCst));
        assert!(bytes.len() <= self.0.raw.1.arg2);
        let length = bytes.len().min(self.0.maximum);
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
struct Fixture {
    f: ReplayIssuerFixture,
    peer: std::net::TcpStream,
    retained: std::net::TcpStream,
    hooks: Arc<ControlledRecordReceiveFixture>,
    _birth: Box<dyn std::any::Any>,
}
async fn fixture(nonblocking: bool) -> Fixture {
    let epoch = NetworkReplayEngine::controlled_replay_two_row_trace().epoch;
    let mut config = Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch,
        network_record_profile: Some(crate::config::NetworkRecordProfile::SharedMmV1),
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Record;
    // Select the production shared Record policy before census, FD publication,
    // or effects. The engine is never replaced with a post-effect fixture.
    let mut state = GlobalState::initialize(&config, false);
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let tid = Tid::from_raw(raw);
    let birth = ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, claim| {
        state
            .sched
            .lock()
            .unwrap()
            .controlled_foreground_store_grant(root);
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        assert!(engine.uses_shared_mm_attempts());
        assert_eq!(
            engine.mode(),
            crate::network_replay::NetworkEngineMode::Record
        );
        engine.fd_table_fixture_enable();
        engine
            .register_initial_census(root.association(), claim, root.owner().thread)
            .unwrap();
    })
    .await;
    let root = birth.parent.clone();
    let owner = root.owner();
    state
        .sched
        .lock()
        .unwrap()
        .controlled_shared_birth_census(&root, &birth.child, &birth._birth);
    let metadata = birth.metadata.clone();
    let memory = birth.memory.clone();
    let (runtime, retained_birth) = birth.into_runtime_and_retention();
    state.network_runtime = Some(runtime);
    let tool: Detcore = Detcore::new(tid, &config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.dettid = owner.thread;
    thread.mm_id = owner.mm;
    thread.detpid = Some(root.logical_process());
    thread.file_metadata = metadata;
    thread.memory_metadata = memory;
    state
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    tool.on_thread_state_ready(tid, &state, &thread).unwrap();
    state.global_time.lock().unwrap().update_global_time(
        owner.thread,
        thread.thread_logical_time.as_nanos(),
        thread.thread_logical_time.inherited_nanos(),
    );
    let pages = Pages::new();
    let mut guest = owned_read_guest(&config, &state, thread);
    let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Socket).await;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (held, _) = listener.accept().unwrap();
    let retained = held.try_clone().unwrap();
    let lowat = 3i32;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                held.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVLOWAT,
                (&lowat as *const i32).cast(),
                std::mem::size_of::<i32>() as _,
            )
        },
        0
    );
    let timeout = libc::timeval {
        tv_sec: 5,
        tv_usec: 0,
    };
    assert_eq!(
        unsafe {
            libc::setsockopt(
                held.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeout as *const libc::timeval).cast(),
                std::mem::size_of::<libc::timeval>() as _,
            )
        },
        0
    );
    let mut cursor = 0i32;
    let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                held.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEEK_OFF,
                (&mut cursor as *mut i32).cast(),
                &mut size,
            )
        },
        0
    );
    {
        let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
        engine.controlled_connect_socket_premise(binding.open_file);
        let identity =
            crate::network_runtime::original_installation::FileIdentity::controlled_fixture(3, 109);
        guest
            .thread
            .file_metadata
            .lock()
            .unwrap()
            .bind_native_installation(binding, identity)
            .unwrap();
        // The supplied installation premise must be identical in the actual
        // metadata and the engine's private native socket origin. A public
        // Connect profile alone deliberately carries no native provenance.
        engine.controlled_shared_record_receive_installation(binding, identity);
        // Supplied original option/Connect premises only. No Joined capability,
        // current receive source or successful output is constructed here.
        let control = engine
            .begin_socket_controls(owner, vec![binding.open_file])
            .unwrap()[0]
            .1;
        engine
            .submit_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalEffect::SetSocketOption {
                    option: crate::network_replay::NetworkStreamSocketOption::ReceiveTimeout {
                        seconds: 5,
                        microseconds: 0,
                    },
                },
            )
            .unwrap();
        engine
            .confirm_stream_physical(
                owner,
                control,
                NetworkStreamPhysicalResult::SocketOption { result: Ok(()) },
            )
            .unwrap();
        engine
            .finish_socket_control(
                owner,
                control,
                crate::network_replay::NetworkSocketControlFinish::Unchanged,
            )
            .unwrap();
        let now = state.global_time.lock().unwrap().as_nanos();
        engine.controlled_shared_record_receive_initial(binding, now, cursor);
    }
    guest
        .thread
        .with_detfd(binding.slot.fd, |fd| fd.set_nonblocking(nonblocking))
        .unwrap();
    let hooks = state
        .network_runtime
        .as_ref()
        .unwrap()
        .install_controlled_record_receive(held.into());
    grant_owned_read_foreground(&state, &mut guest).await;
    let reply = super::super::super::network_request(
        &mut guest,
        NetworkRequest::BeginOrdinaryFdRead {
            files: binding.slot.files,
            fd: binding.slot.fd,
        },
    )
    .await
    .unwrap();
    let NetworkReply::FdRead(crate::network_replay::NetworkFdReadBegin::Admitted(read)) = reply
    else {
        panic!("actual FD reader refused")
    };
    assert_eq!(read.binding, Some(binding));
    assert!(read.control.is_some());
    let thread = guest.thread;
    Fixture {
        f: ReplayIssuerFixture {
            config,
            state,
            thread,
            root,
            pages,
            tid,
            binding,
            read: *read,
        },
        peer,
        retained,
        hooks,
        _birth: retained_birth,
    }
}
fn original(f: &ReplayIssuerFixture, recvfrom: bool) -> ScalarReceive {
    if !recvfrom {
        return scalar_read(f.binding.slot.fd, f.pages.at(128), 8).into();
    }
    ScalarReceive::from_syscall(Syscall::from_raw(
        Sysno::recvfrom,
        reverie::syscalls::SyscallArgs::new(
            f.binding.slot.fd as usize,
            f.pages.at(128) as usize,
            8,
            0,
            0,
            0,
        ),
    ))
    .unwrap()
}
fn queued(socket: &std::net::TcpStream) -> i32 {
    let mut n = 0i32;
    assert_eq!(
        unsafe { libc::ioctl(socket.as_raw_fd(), libc::FIONREAD, &mut n) },
        0
    );
    n
}
fn wait_bytes(socket: &std::net::TcpStream, expected: i32) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while queued(socket) != expected {
        assert!(
            std::time::Instant::now() < end,
            "actual TCP payload did not arrive"
        );
        std::thread::yield_now();
    }
}
fn canaries(f: &ReplayIssuerFixture, copied: &[u8]) {
    let mut expected = vec![0xa5; 8192];
    expected[128..128 + copied.len()].copy_from_slice(copied);
    assert_eq!(
        f.pages.bytes(0, 8192),
        expected,
        "all output and surrounding bytes"
    );
}
async fn begin(
    f: &ReplayIssuerFixture,
    guest: &ScalarForegroundGuest<'_>,
    original: ScalarReceive,
) -> SharedReceiveInvocation {
    let expected = guest
        .thread_state()
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&f.read)
        .unwrap();
    f.state
        .begin_shared_record_receive(guest, original, f.read.clone(), expected)
        .await
        .unwrap()
}
fn consumed(f: &ReplayIssuerFixture) -> u64 {
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .controlled_shared_record_receive_consumed(f.binding.open_file)
}
fn retained(f: &ReplayIssuerFixture, invocation: &SharedReceiveInvocation) {
    let mut e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        e.begin_stream_call_release(f.root.owner(), invocation.call())
            .is_err(),
        "exact original Call debt prevents ordinary release"
    );
    assert_ne!(e.native_capture_fixture_counts(f.binding.open_file).0, 0);
    assert!(e.finish().is_err());
}
async fn close(
    f: &ReplayIssuerFixture,
    guest: &mut ScalarForegroundGuest<'_>,
    call: NetworkStreamCallId,
) {
    let response = super::super::super::network_request(
        guest,
        NetworkRequest::NativeReleaseStreamCall { call },
    )
    .await
    .unwrap();
    assert_eq!(response, NetworkReply::Unit);
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

#[tokio::test]
async fn shared_record_receive_global_real_scalar_store_drain_publish_and_pin_close() {
    for recvfrom in [false, true] {
        for restored in [false, true] {
            let mut q = fixture(false).await;
            q.peer.write_all(b"abcdefgh").unwrap();
            wait_bytes(&q.retained, 8);
            let f = &q.f;
            let original = original(f, recvfrom);
            let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
            let writer = ControlledRecordStore::new(f, original, restored, 8, false);
            guest.shared_record_store = Some(writer.clone());
            let now = f.state.global_time.lock().unwrap().as_nanos();
            let turn = f.state.sched.lock().unwrap().turn;
            let invocation = begin(f, &guest, original).await;
            assert_eq!(invocation.policy().target(), 3);
            assert_eq!(invocation.policy().started(), now);
            assert_eq!(
                invocation.policy().deadline(),
                Some(LogicalTime::from_nanos(now.as_nanos() + 5_000_000_000))
            );
            assert!(!invocation.policy().nonblocking());
            if recvfrom {
                assert!(guest.range_calls.lock().unwrap().is_empty());
                assert_eq!(
                    *guest.recvfrom_range_calls.lock().unwrap(),
                    vec![original.into_parts(); 3],
                    "pre-prefix, pre-transfer and post-join exact Recvfrom observations"
                );
            } else {
                assert!(guest.recvfrom_range_calls.lock().unwrap().is_empty());
                assert_eq!(
                    *guest.range_calls.lock().unwrap(),
                    vec![(f.binding.slot.fd, f.pages.at(128), 8); 3],
                    "pre-prefix, pre-transfer and post-join exact Read observations"
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
            let prepared = f
                .state
                .prepare_shared_record_receive(f.tid, guest.thread_state(), &invocation)
                .await
                .unwrap();
            assert!(!prepared.pending());
            assert_eq!(q.hooks.order(), ["capture", "peek"]);
            assert_eq!(queued(&q.retained), 8, "Peek/cursor/Poll must not consume");
            let effect = f
                .state
                .store_shared_record_receive(&guest, &invocation, prepared, restored)
                .unwrap();
            let SharedRecordReceiveEffect::Stored(stored) = effect else {
                panic!("bytes need actual store")
            };
            canaries(f, b"abcdefgh");
            assert_eq!(writer.writes.load(Ordering::SeqCst), 1);
            assert!(writer.checks.load(Ordering::SeqCst) >= 2);
            assert_eq!(consumed(f), 0);
            assert_eq!(queued(&q.retained), 8);
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
            assert_eq!(
                f.state
                    .drain_shared_record_receive(f.tid, guest.thread_state(), &invocation, stored)
                    .await
                    .unwrap(),
                8
            );
            assert_eq!(q.hooks.order(), ["capture", "peek", "drain"]);
            assert_eq!(queued(&q.retained), 0);
            assert_eq!(consumed(f), 8);
            let trace = f
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_trace_fixture();
            trace.validate().unwrap();
            assert_eq!(trace.inputs.len(), before.inputs.len() + 1);
            assert_eq!(
                trace.inputs.last().unwrap().event,
                NetworkInputKindV2::StreamBytes {
                    stream_offset: 0,
                    bytes: b"abcdefgh".to_vec()
                }
            );
            assert_eq!(trace.native_receive_observations.len(), 1);
            let observation = &trace.native_receive_observations[0];
            assert_eq!((observation.stream_offset, observation.length), (0, 8));
            assert!(
                observation
                    .fragments
                    .iter()
                    .all(|f| f.disposition == NetworkNativeCopyDispositionV4::Consume)
            );
            assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), now);
            assert_eq!(f.state.sched.lock().unwrap().turn, turn);
            close(f, &mut guest, invocation.call()).await;
            canaries(f, b"abcdefgh");
        }
    }
}

#[tokio::test]
async fn shared_record_receive_global_real_partial_or_failed_postcheck_retains_effect_and_interval()
{
    for (maximum, post_error) in [(2, false), (8, true)] {
        let mut q = fixture(false).await;
        q.peer.write_all(b"abcdefgh").unwrap();
        wait_bytes(&q.retained, 8);
        let f = &q.f;
        let original = original(f, false);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let writer = ControlledRecordStore::new(f, original, false, maximum, post_error);
        guest.shared_record_store = Some(writer.clone());
        let invocation = begin(f, &guest, original).await;
        let before = f
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture();
        let prepared = f
            .state
            .prepare_shared_record_receive(f.tid, guest.thread_state(), &invocation)
            .await
            .unwrap();
        let error = f
            .state
            .store_shared_record_receive(&guest, &invocation, prepared, false)
            .err()
            .expect("actual nonfull/postfailed effect refused");
        assert!(
            error.to_string().contains("full"),
            "specific full-store predicate: {error}"
        );
        assert_eq!(writer.writes.load(Ordering::SeqCst), 1);
        canaries(f, &b"abcdefgh"[..maximum]);
        assert_eq!(queued(&q.retained), 8);
        assert_eq!(consumed(f), 0);
        assert_eq!(q.hooks.order(), ["capture", "peek"]);
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
        retained(f, &invocation);
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
        assert!(
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .join_shared_foreground_prefix(
                    f.root.clone(),
                    f.state.network_engine.as_ref().unwrap(),
                    Some(invocation.call())
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn shared_record_receive_global_changed_epoch_before_store_or_drain_cannot_commit() {
    for after_store in [false, true] {
        let mut q = fixture(false).await;
        q.peer.write_all(b"abcdefgh").unwrap();
        wait_bytes(&q.retained, 8);
        let f = &q.f;
        let original = original(f, false);
        let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
        let writer = ControlledRecordStore::new(f, original, false, 8, false);
        guest.shared_record_store = Some(writer.clone());
        let invocation = begin(f, &guest, original).await;
        let before = f
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture();
        let prepared = f
            .state
            .prepare_shared_record_receive(f.tid, guest.thread_state(), &invocation)
            .await
            .unwrap();
        if after_store {
            let SharedRecordReceiveEffect::Stored(stored) = f
                .state
                .store_shared_record_receive(&guest, &invocation, prepared, false)
                .unwrap()
            else {
                panic!("Stored")
            };
            f.state
                .sched
                .lock()
                .unwrap()
                .controlled_shared_foreground_grant(&f.root);
            let error = f
                .state
                .drain_shared_record_receive(f.tid, guest.thread_state(), &invocation, stored)
                .await
                .unwrap_err();
            assert!(
                error.to_string().contains("epoch"),
                "exact post-store epoch refusal: {error}"
            );
            canaries(f, b"abcdefgh");
        } else {
            f.state
                .sched
                .lock()
                .unwrap()
                .controlled_shared_foreground_grant(&f.root);
            let error = f
                .state
                .store_shared_record_receive(&guest, &invocation, prepared, false)
                .err()
                .expect("stale original grant");
            assert!(
                error.to_string().contains("Normal"),
                "exact pre-store grant refusal: {error}"
            );
            canaries(f, &[]);
        }
        assert_eq!(
            writer.writes.load(Ordering::SeqCst),
            usize::from(after_store)
        );
        assert_eq!(queued(&q.retained), 8);
        assert_eq!(consumed(f), 0);
        assert_eq!(q.hooks.order(), ["capture", "peek"]);
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
        retained(f, &invocation);
    }
}

#[tokio::test]
async fn shared_record_receive_global_real_eof_or_nonblocking_empty_uses_checked_no_store() {
    use crate::network_replay::shared_waits::SharedNoStoreResult;
    for eof in [false, true] {
        for stale in [false, true] {
            let q = fixture(!eof).await;
            let f = &q.f;
            if eof {
                q.peer.shutdown(std::net::Shutdown::Write).unwrap();
                let mut row = libc::pollfd {
                    fd: q.retained.as_raw_fd(),
                    events: libc::POLLIN | libc::POLLRDHUP,
                    revents: 0,
                };
                assert_eq!(unsafe { libc::poll(&mut row, 1, 1000) }, 1);
                assert_ne!(row.revents & libc::POLLRDHUP, 0);
            }
            let original = original(f, false);
            let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
            let writer = ControlledRecordStore::new(f, original, false, 8, false);
            guest.shared_record_store = Some(writer.clone());
            let invocation = begin(f, &guest, original).await;
            let before = f
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_trace_fixture();
            let prepared = f
                .state
                .prepare_shared_record_receive(f.tid, guest.thread_state(), &invocation)
                .await
                .unwrap();
            assert!(
                !prepared.pending(),
                "EOF/nonblocking empty is completed source"
            );
            if stale {
                writer.valid.store(false, Ordering::SeqCst);
            }
            let result = f
                .state
                .store_shared_record_receive(&guest, &invocation, prepared, false);
            assert_eq!(writer.writes.load(Ordering::SeqCst), 0);
            assert!(writer.checks.load(Ordering::SeqCst) > 0);
            assert_eq!(q.hooks.order(), ["capture", "peek"]);
            canaries(f, &[]);
            assert_eq!(queued(&q.retained), 0);
            assert_eq!(consumed(f), 0);
            if stale {
                let error = result.err().expect("stale backend context");
                assert!(error.to_string().contains("context"));
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
                retained(f, &invocation);
            } else {
                let SharedRecordReceiveEffect::NoStore(actual) = result.unwrap() else {
                    panic!("NoStore")
                };
                assert_eq!(
                    actual,
                    if eof {
                        SharedNoStoreResult::Eof
                    } else {
                        SharedNoStoreResult::WouldBlock { timed_out: false }
                    }
                );
                let trace = f
                    .state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .native_trace_fixture();
                trace.validate().unwrap();
                assert!(
                    trace.native_receive_observations.is_empty(),
                    "NoStore never invents Consume"
                );
                if eof {
                    assert_eq!(trace.inputs.len(), before.inputs.len() + 1);
                    assert!(matches!(
                        trace.inputs.last().unwrap().event,
                        NetworkInputKindV2::PeerShutdown {
                            stream_offset: 0,
                            direction: detcore_model::network_trace::NetworkShutdownV2::Write
                        }
                    ));
                } else {
                    assert_eq!(
                        trace, before,
                        "actual empty EAGAIN appends no invented network event"
                    );
                }
                close(f, &mut guest, invocation.call()).await;
            }
        }
    }
}

#[tokio::test]
async fn shared_record_receive_global_dropped_stored_token_does_not_erase_drain_debt() {
    let mut q = fixture(false).await;
    q.peer.write_all(b"abcdefgh").unwrap();
    wait_bytes(&q.retained, 8);
    let f = &q.f;
    let original = original(f, false);
    let mut guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let writer = ControlledRecordStore::new(f, original, false, 8, false);
    guest.shared_record_store = Some(writer);
    let invocation = begin(f, &guest, original).await;
    let before = f
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    let prepared = f
        .state
        .prepare_shared_record_receive(f.tid, guest.thread_state(), &invocation)
        .await
        .unwrap();
    let effect = f
        .state
        .store_shared_record_receive(&guest, &invocation, prepared, false)
        .unwrap();
    assert!(matches!(&effect, SharedRecordReceiveEffect::Stored(_)));
    drop(effect);
    canaries(f, b"abcdefgh");
    assert_eq!(queued(&q.retained), 8);
    assert_eq!(consumed(f), 0);
    assert_eq!(q.hooks.order(), ["capture", "peek"]);
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
    retained(f, &invocation);
}

#[tokio::test]
async fn shared_record_receive_global_canceled_capture_keeps_actual_worker_and_original_call() {
    let q = fixture(false).await;
    let f = &q.f;
    let (started, release) = q.hooks.pause_capture();
    let guest = scalar_foreground_guest(&f.state, &f.config, f.thread.clone(), f.tid);
    let expected = guest
        .thread_state()
        .file_metadata
        .lock()
        .unwrap()
        .observe_fd_read(&f.read)
        .unwrap();
    let mut pending = Box::pin(f.state.begin_shared_record_receive(
        &guest,
        original(f, false),
        f.read.clone(),
        expected,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::select! {
            observed = started => observed.unwrap(),
            _ = pending.as_mut() => panic!("Global capture returned before the actual worker gate"),
        }
    })
    .await
    .unwrap();
    drop(pending);
    {
        let mut e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        assert_ne!(e.native_capture_fixture_counts(f.binding.open_file).0, 0);
        assert!(e.finish_fd_read(f.root.owner(), f.read.clone()).is_err());
        assert!(e.finish().is_err());
    }
    release.send(()).unwrap();
    f.state
        .network_runtime
        .as_ref()
        .unwrap()
        .controlled_join_retry_workers()
        .await;
    let e = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert_ne!(
        e.native_capture_fixture_counts(f.binding.open_file).0,
        0,
        "actual worker join cannot invent lost Global capture completion"
    );
    assert!(e.finish().is_err());
    assert_eq!(q.hooks.order(), ["capture"]);
    canaries(f, &[]);
}
