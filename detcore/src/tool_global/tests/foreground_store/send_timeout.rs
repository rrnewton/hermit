//! Actual Detcore caller and Global socket-control transaction. Initial root,
//! fresh socket/profile and native setter completion are explicit controlled
//! premises. Guest copies use actual process_vm_{readv,writev}; no backend or
//! native M2 execution is claimed. Replay forbids every native injection.
use detcore_model::network_trace::NetworkReleaseModelV4;
use detcore_model::network_trace::ReceiveTimeoutV3;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::SyscallInfo;

use super::*;

struct OptionMemory(i32);
impl MemoryAccess for OptionMemory {
    fn read_vectored(&self, remote: &[IoSlice], local: &mut [IoSliceMut]) -> Result<usize, Errno> {
        Errno::result(unsafe {
            libc::process_vm_readv(
                self.0,
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        })
        .map(|n| n as usize)
    }
    fn write_vectored(
        &mut self,
        local: &[IoSlice],
        remote: &mut [IoSliceMut],
    ) -> Result<usize, Errno> {
        Errno::result(unsafe {
            libc::process_vm_writev(
                self.0,
                local.as_ptr().cast(),
                local.len() as _,
                remote.as_ptr().cast(),
                remote.len() as _,
                0,
            )
        })
        .map(|n| n as usize)
    }
}
struct OptionStack {
    bytes: Box<[u8; 16]>,
    pushed: bool,
}
struct OptionStackGuard {
    _bytes: Box<[u8; 16]>,
}
impl Drop for OptionStackGuard {
    fn drop(&mut self) {}
}
impl Stack for OptionStack {
    type StackGuard = OptionStackGuard;
    fn size(&self) -> usize {
        if self.pushed { 16 } else { 0 }
    }
    fn capacity(&self) -> usize {
        16
    }
    fn push<'s, V>(&mut self, value: V) -> Addr<'s, V> {
        assert!(!self.pushed);
        assert_eq!(std::mem::size_of::<V>(), 16);
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&value as *const V).cast::<u8>(),
                self.bytes.as_mut_ptr(),
                16,
            )
        };
        self.pushed = true;
        Addr::from_raw(self.bytes.as_ptr() as usize).unwrap()
    }
    fn reserve<'s, V>(&mut self) -> AddrMut<'s, V> {
        panic!("timeout setter must push its retained snapshot")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        assert!(self.pushed);
        Ok(OptionStackGuard { _bytes: self.bytes })
    }
}
struct SetterPremise {
    original: reverie::syscalls::SyscallArgs,
    bytes: [u8; 16],
    result: Result<i64, Errno>,
}
// Borrow only the Sync runtime/config inputs. The original fixture owns the
// mappings for this entire view; no raw-pointer Sync implementation is added.
struct FixtureView<'a> {
    state: &'a GlobalState,
    config: &'a Config,
    tid: Tid,
    binding: crate::types::FdSlotBinding,
    pages: PageView,
}
struct PageView(u64);
impl PageView {
    fn at(&self, offset: usize) -> u64 {
        self.0 + offset as u64
    }
    fn bytes(&self, offset: usize, count: usize) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(self.at(offset) as *const u8, count).to_vec() }
    }
}
struct OptionGuest<'a> {
    f: FixtureView<'a>,
    thread: crate::ThreadState<()>,
    requests: Mutex<Vec<GlobalRequest>>,
    responses: Mutex<Vec<GlobalResponse>>,
    next: Option<SetterPremise>,
    injections: usize,
}
#[reverie::tool]
impl GlobalRPC<GlobalState> for OptionGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.requests.lock().unwrap().push(message.2.clone());
        let response = self.f.state.receive_rpc(self.f.tid, message).await;
        self.responses.lock().unwrap().push(response.1.clone());
        response
    }
    fn config(&self) -> &Config {
        self.f.config
    }
}
#[reverie::tool]
impl Guest<Detcore> for OptionGuest<'_> {
    type Memory = OptionMemory;
    type Stack = OptionStack;
    fn tid(&self) -> Tid {
        self.f.tid
    }
    fn pid(&self) -> Tid {
        self.f.tid
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn local_global_state(&self) -> Option<&GlobalState> {
        Some(self.f.state)
    }
    fn memory(&self) -> Self::Memory {
        OptionMemory(self.f.tid.as_raw())
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("unexpected registers")
    }
    async fn stack(&mut self) -> Self::Stack {
        assert_eq!(self.f.config.network_trace.policy, NetworkPolicy::Record);
        assert!(self.next.is_some());
        OptionStack {
            bytes: Box::new([0; 16]),
            pushed: false,
        }
    }
    async fn inject<S: SyscallInfo>(&mut self, call: S) -> Result<i64, Errno> {
        assert_eq!(
            self.f.config.network_trace.policy,
            NetworkPolicy::Record,
            "Replay used a native socket"
        );
        let premise = self.next.take().expect("unrequested native operation");
        let (nr, args) = call.into_parts();
        assert_eq!(nr, Sysno::setsockopt);
        assert_eq!(
            (args.arg0, args.arg1, args.arg2, args.arg4, args.arg5),
            (
                premise.original.arg0,
                premise.original.arg1,
                premise.original.arg2,
                premise.original.arg4,
                premise.original.arg5
            )
        );
        assert_ne!(
            args.arg3, premise.original.arg3,
            "original user memory was reread instead of injecting the retained snapshot"
        );
        let mut bytes = [0; 16];
        self.memory()
            .read_exact_with_user_access(Addr::from_raw(args.arg3).unwrap(), &mut bytes)
            .unwrap();
        assert_eq!(bytes, premise.bytes);
        self.injections += 1;
        premise.result
    }
    async fn daemonize(&mut self) {
        panic!("unexpected daemonization")
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("unexpected tail injection")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("unexpected timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("unexpected timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("unexpected clock")
    }
}
async fn fixture(record: bool, shared: bool) -> ReplayIssuerFixture {
    fixture_inner(record, shared, true).await
}
async fn fixture_inner(record: bool, shared: bool, fresh: bool) -> ReplayIssuerFixture {
    let mut trace = NetworkReplayEngine::controlled_replay_two_row_trace();
    if shared {
        trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 {
            nodes: trace.release_model.nodes().to_vec(),
        };
    }
    trace.validate().unwrap();
    let (f, _) = ReplayIssuerFixture::new_trace_profile(record, trace, false, shared, fresh).await;
    let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    engine
        .finish_fd_read(f.root.owner(), f.read.clone())
        .unwrap();
    assert_eq!(engine.uses_shared_mm_attempts(), shared);
    assert!(
        !engine.accepted_mode(),
        "V4 must not impersonate accepted V3"
    );
    assert_eq!(
        engine
            .stream_socket_state(f.binding.open_file)
            .unwrap()
            .unwrap()
            .send_timeout,
        fresh.then_some(ReceiveTimeoutV3::Infinite)
    );
    drop(engine);
    f
}
fn guest(f: &ReplayIssuerFixture) -> OptionGuest<'_> {
    OptionGuest {
        f: FixtureView {
            state: &f.state,
            config: &f.config,
            tid: f.tid,
            binding: f.binding,
            pages: PageView(f.pages.at(0)),
        },
        thread: f.thread.clone(),
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(Vec::new()),
        next: None,
        injections: 0,
    }
}
fn state(f: &ReplayIssuerFixture) -> crate::network_replay::NetworkStreamSocketState {
    f.state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .stream_socket_state(f.binding.open_file)
        .unwrap()
        .unwrap()
}
fn setter(f: &FixtureView<'_>, seconds: i64, microseconds: i64) -> reverie::syscalls::Setsockopt {
    let bytes = [seconds.to_ne_bytes(), microseconds.to_ne_bytes()].concat();
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), f.pages.at(256) as *mut u8, 16) };
    reverie::syscalls::Setsockopt::new()
        .with_fd(f.binding.slot.fd)
        .with_level(libc::SOL_SOCKET)
        .with_optname(libc::SO_SNDTIMEO)
        .with_optlen(16)
        .with_optval(Addr::from_raw(f.pages.at(256) as usize))
}
async fn set(
    g: &mut OptionGuest<'_>,
    seconds: i64,
    microseconds: i64,
    result: Result<i64, Errno>,
) -> Result<Option<i64>, reverie::Error> {
    let call = setter(&g.f, seconds, microseconds);
    if g.f.config.network_trace.policy == NetworkPolicy::Record {
        assert!(
            g.next
                .replace(SetterPremise {
                    original: call.into_parts().1,
                    bytes: g.f.pages.bytes(256, 16).try_into().unwrap(),
                    result
                })
                .is_none()
        );
    }
    let tool: Detcore = Detcore::new(g.f.tid, g.f.config);
    tool.try_shadow_setsockopt(g, call).await
}
fn getter(f: &FixtureView<'_>, capacity: i32) -> reverie::syscalls::Getsockopt {
    unsafe { std::ptr::write_unaligned(f.pages.at(4096) as *mut i32, capacity) };
    reverie::syscalls::Getsockopt::new()
        .with_fd(f.binding.slot.fd)
        .with_level(libc::SOL_SOCKET)
        .with_optname(libc::SO_SNDTIMEO)
        .with_optval(AddrMut::from_raw(f.pages.at(0) as usize))
        .with_optlen(AddrMut::from_raw(f.pages.at(4096) as usize))
}
async fn assert_get(g: &mut OptionGuest<'_>, expected: (i64, i64)) {
    let tool: Detcore = Detcore::new(g.f.tid, g.f.config);
    let call = getter(&g.f, 24);
    assert!(matches!(
        tool.try_shadow_getsockopt(g, call).await,
        Ok(Some(0))
    ));
    assert_eq!(
        g.f.pages.bytes(0, 16),
        [expected.0.to_ne_bytes(), expected.1.to_ne_bytes()].concat()
    );
    assert_eq!(g.f.pages.bytes(16, 8), [0xa5; 8]);
    assert_eq!(g.f.pages.bytes(4096, 4), 16_i32.to_ne_bytes());
}
async fn round_trip(record: bool) {
    let f = fixture(record, true).await;
    let mut g = guest(&f);
    let before = state(&f);
    let original_trace = f
        .state
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    let time = f.state.global_time.lock().unwrap().as_nanos();
    let turn = f.state.sched.lock().unwrap().turn;
    assert!(matches!(set(&mut g, 5, 0, Ok(0)).await, Ok(Some(0))));
    let after = state(&f);
    assert_eq!(
        after.send_timeout,
        Some(ReceiveTimeoutV3::FiniteTicks(5000))
    );
    assert_eq!(after.option_generation, before.option_generation + 1);
    assert_eq!(
        after.options, before.options,
        "SNDTIMEO must not change receive options"
    );
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .shared_record_send_timeout(f.binding.open_file)
            .unwrap(),
        5000
    );
    assert_get(&mut g, (5, 0)).await;
    assert_eq!(g.injections, usize::from(record));
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture(),
        original_trace,
        "a later guest setter must not relabel the immutable fresh-timeout trace"
    );
    let requests = g.requests.lock().unwrap();
    let submitted = requests
        .iter()
        .filter(|r| {
            matches!(
                r,
                GlobalRequest::Network(NetworkRequest::SubmitStreamPhysical {
                    effect: NetworkStreamPhysicalEffect::SetSocketOption {
                        option: crate::network_replay::NetworkStreamSocketOption::SendTimeout {
                            seconds: 5,
                            microseconds: 0
                        }
                    },
                    ..
                })
            )
        })
        .count();
    assert_eq!(submitted, 1);
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(
                r,
                GlobalRequest::Network(NetworkRequest::ConfirmStreamPhysical {
                    result: crate::network_replay::NetworkStreamPhysicalResult::SocketOption {
                        result: Ok(())
                    },
                    ..
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(
                r,
                GlobalRequest::Network(NetworkRequest::FinishSocketControl { .. })
            ))
            .count(),
        2
    );
    assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), time);
    assert_eq!(f.state.sched.lock().unwrap().turn, turn);
}
#[tokio::test]
async fn shared_record_send_timeout_caller_saves_finite_value() {
    round_trip(true).await;
}
#[tokio::test]
async fn shared_replay_send_timeout_caller_never_uses_native_socket() {
    round_trip(false).await;
}

#[tokio::test]
async fn shared_send_timeout_normalization_preserves_errors_and_finite_guard() {
    for record in [true, false] {
        let f = fixture(record, true).await;
        let mut g = guest(&f);
        assert!(matches!(set(&mut g, 5, 0, Ok(0)).await, Ok(Some(0))));
        for microseconds in [-1, 1_000_000] {
            let before = state(&f);
            assert!(matches!(
                set(&mut g, 1, microseconds, Err(Errno::EDOM)).await,
                Err(reverie::Error::Errno(Errno::EDOM))
            ));
            assert_eq!(state(&f), before);
        }
        for (seconds, expected) in [
            (0, ReceiveTimeoutV3::Infinite),
            (-1, ReceiveTimeoutV3::FiniteTicks(0)),
            (i64::MAX, ReceiveTimeoutV3::Infinite),
        ] {
            assert!(matches!(set(&mut g, seconds, 0, Ok(0)).await, Ok(Some(0))));
            assert_eq!(state(&f).send_timeout, Some(expected));
            assert!(
                f.state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .shared_record_send_timeout(f.binding.open_file)
                    .is_err()
            );
            assert_get(&mut g, (0, 0)).await;
        }
        assert_eq!(g.injections, if record { 6 } else { 0 });
    }
}

#[tokio::test]
async fn shared_send_timeout_usercopy_order_and_truncation() {
    for record in [true, false] {
        let f = fixture(record, true).await;
        let mut g = guest(&f);
        assert!(matches!(set(&mut g, 5, 0, Ok(0)).await, Ok(Some(0))));
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        for (length, null, errno) in [
            (0, true, Errno::EINVAL),
            (3, true, Errno::EINVAL),
            (u32::MAX, true, Errno::EINVAL),
            (4, true, Errno::EFAULT),
            (4, false, Errno::EINVAL),
            (16, true, Errno::EFAULT),
        ] {
            let before = state(&f);
            let call = setter(&g.f, 7, 0).with_optlen(length);
            let call = if null { call.with_optval(None) } else { call };
            assert!(
                matches!(tool.try_shadow_setsockopt(&mut g, call).await, Err(reverie::Error::Errno(e)) if e == errno)
            );
            assert_eq!(state(&f), before);
        }
        let call = getter(&g.f, 8);
        assert!(matches!(
            tool.try_shadow_getsockopt(&mut g, call).await,
            Ok(Some(0))
        ));
        assert_eq!(f.pages.bytes(0, 8), 5_i64.to_ne_bytes());
        assert_eq!(f.pages.bytes(8, 16), [0xa5; 16]);
        assert_eq!(f.pages.bytes(4096, 4), 8_i32.to_ne_bytes());
        let call = getter(&g.f, -1);
        assert!(matches!(
            tool.try_shadow_getsockopt(&mut g, call).await,
            Err(reverie::Error::Errno(Errno::EINVAL))
        ));
        let call = getter(&g.f, 16).with_optlen(None);
        assert!(matches!(
            tool.try_shadow_getsockopt(&mut g, call).await,
            Err(reverie::Error::Errno(Errno::EFAULT))
        ));
        let call = getter(&g.f, 16);
        f.pages.protect_second(libc::PROT_READ);
        assert!(matches!(
            tool.try_shadow_getsockopt(&mut g, call).await,
            Err(reverie::Error::Errno(Errno::EFAULT))
        ));
        assert_eq!(
            f.pages.bytes(0, 16),
            [5_i64.to_ne_bytes(), 0_i64.to_ne_bytes()].concat(),
            "value copy precedes faulting length copy"
        );
        assert_eq!(
            state(&f).send_timeout,
            Some(ReceiveTimeoutV3::FiniteTicks(5000))
        );
        assert_eq!(g.injections, usize::from(record));
    }
}

#[tokio::test]
async fn legacy_sole_send_timeout_route_still_falls_back() {
    for record in [true, false] {
        let f = fixture(record, false).await;
        let mut g = guest(&f);
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let before = state(&f);
        let call = setter(&g.f, 5, 0);
        assert!(matches!(
            tool.try_shadow_setsockopt(&mut g, call).await,
            Ok(None)
        ));
        let call = getter(&g.f, 16);
        assert!(matches!(
            tool.try_shadow_getsockopt(&mut g, call).await,
            Ok(None)
        ));
        assert_eq!(state(&f), before);
        assert_eq!(g.injections, 0);
        assert!(
            g.requests
                .lock()
                .unwrap()
                .iter()
                .all(|r| matches!(r, GlobalRequest::Network(NetworkRequest::AcceptedMode)))
        );
    }
}

#[tokio::test]
async fn shared_send_timeout_unexpected_native_result_retains_debt() {
    let f = fixture(true, true).await;
    let mut g = guest(&f);
    let before = state(&f);
    let failure = set(&mut g, 5, 0, Ok(1)).await.unwrap_err();
    let reverie::Error::Tool(failure) = failure else {
        panic!("unexpected native result must remain a Tool failure");
    };
    assert_eq!(
        failure.root_cause().to_string(),
        "shared network engine refused operation: setsockopt returned unexpected 1"
    );
    assert!(
        failure
            .to_string()
            .starts_with("secondary network cleanup failure:")
    );
    assert_eq!(state(&f), before);
    let requests = g.requests.lock().unwrap();
    assert!(requests.iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::SubmitStreamPhysical { .. })
    )));
    assert!(!requests.iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::ConfirmStreamPhysical { .. })
    )));
    let lease = requests
        .iter()
        .find_map(|r| match r {
            GlobalRequest::Network(NetworkRequest::FinishSocketControl { lease, .. }) => {
                Some(*lease)
            }
            _ => None,
        })
        .unwrap();
    drop(requests);
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .finish_socket_control(
                f.root.owner(),
                lease,
                crate::network_replay::NetworkSocketControlFinish::Unchanged
            )
            .is_err()
    );
}

#[tokio::test]
async fn shared_send_timeout_profile_alone_cannot_supply_fresh_authority() {
    let f = fixture_inner(true, true, false).await;
    let mut g = guest(&f);
    let before = state(&f);
    let error = set(&mut g, 5, 0, Ok(0)).await.unwrap_err();
    let reverie::Error::Tool(error) = error else {
        panic!("missing fresh authority must be a protocol refusal");
    };
    assert_eq!(
        error.root_cause().to_string(),
        NetworkReplayError::WrongMode.to_string()
    );
    let leases: Vec<_> = g
        .responses
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| match r {
            GlobalResponse::Network(Ok(NetworkReply::SocketControl(control))) => {
                Some(control.lease)
            }
            _ => None,
        })
        .collect();
    assert_eq!(leases.len(), 1);
    let submissions: Vec<_> = g
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|r| match r {
            GlobalRequest::Network(NetworkRequest::SubmitStreamPhysical { lease, effect }) => {
                Some((*lease, effect.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        submissions,
        vec![(
            leases[0],
            NetworkStreamPhysicalEffect::SetSocketOption {
                option: crate::network_replay::NetworkStreamSocketOption::SendTimeout {
                    seconds: 5,
                    microseconds: 0
                },
            }
        )]
    );
    assert_eq!(g.injections, 0);
    assert_eq!(state(&f), before);
    assert_eq!(before.send_timeout, None);
    assert!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .shared_record_send_timeout(f.binding.open_file)
            .is_err()
    );
    assert!(!g.requests.lock().unwrap().iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::ConfirmStreamPhysical { .. })
    )));
}
