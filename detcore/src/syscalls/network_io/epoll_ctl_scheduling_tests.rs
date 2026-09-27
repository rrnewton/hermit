//! Actual handler order with explicitly controlled RPC/delegate inputs.
//! This does not issue native authority or claim a kernel syscall executed.
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Never;
use reverie::Tid;
use reverie::TimerSchedule;
use reverie::Tool;
use reverie::syscalls::LocalMemory;
use serde::Deserialize;
use serde::Serialize;

use super::*;
use crate::Config;
use crate::network_replay::NetworkFdPublicationAdmission;
use crate::network_replay::NetworkFdPublicationPermit;
use crate::network_replay::NetworkFdPublicationReply;
use crate::network_replay::NetworkFdPublicationRequest;
use crate::network_replay::NetworkStreamLeaseId;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Kind;
use crate::tool_global::GlobalRequest;
use crate::tool_global::GlobalResponse;
use crate::tool_global::GlobalState;
use crate::tool_global::ResumeStatus;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
enum DelegateMode {
    #[default]
    Success,
    Errno,
    Tool,
    Io,
    MissingReturn,
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Delegate;
#[reverie::tool]
impl Tool for Delegate {
    type GlobalState = GlobalState;
    type ThreadState = (DelegateMode, usize);
    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, Error> {
        assert!(matches!(call, Syscall::EpollCtl(_)));
        guest.thread_state_mut().1 += 1;
        match guest.thread_state().0 {
            DelegateMode::Tool => Err(engine_error("controlled ctl delegate tool failure")),
            DelegateMode::Io => Err(Error::Io(std::io::Error::other(
                "controlled ctl delegate IO failure",
            ))),
            _ => Ok(guest.inject(call).await?),
        }
    }
}
impl RecordOrReplay for Delegate {
    async fn invoke_original_read<G: Guest<Self>>(
        &self,
        _: &mut G,
        _: syscalls::Read,
    ) -> Result<reverie::InjectedReadResult, Error> {
        panic!("ctl must not invoke Read")
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    None,
    Begin,
    Submit,
    Outcome,
    Retire,
}
struct Boundary {
    events: Mutex<Vec<&'static str>>,
    admission: Mutex<Option<Admission>>,
    observed: AtomicBool,
    release_outcome: AtomicBool,
    release_continue: AtomicBool,
    changed: tokio::sync::Notify,
    failure: Failure,
}
impl Boundary {
    fn new(failure: Failure) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            admission: Mutex::new(None),
            observed: AtomicBool::new(false),
            release_outcome: AtomicBool::new(false),
            release_continue: AtomicBool::new(true),
            changed: tokio::sync::Notify::new(),
            failure,
        }
    }
    fn event(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
    async fn until(&self, condition: &AtomicBool) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if condition.load(Ordering::SeqCst) {
                return;
            }
            changed.await;
        }
    }
}
struct CtlGuest<'a> {
    config: &'a Config,
    thread: crate::ThreadState<(DelegateMode, usize)>,
    boundary: Arc<Boundary>,
}
#[reverie::tool]
impl GlobalRPC<GlobalState> for CtlGuest<'_> {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        let owner = NetworkStreamOwner {
            thread: self.thread.dettid,
            mm: self.thread.mm_id,
        };
        if let GlobalRequest::RequestResources(resources, process) = &request.2 {
            assert_eq!(*process, self.thread.detpid.unwrap());
            assert_eq!(resources.tid, owner.thread);
            assert_eq!(resources.resources.len(), 1);
            assert!(resources.fd_read.is_none());
            let operation = self
                .thread
                .original_connect
                .as_ref()
                .unwrap()
                .arguments
                .operation;
            if resources
                .resources
                .contains_key(&ResourceID::BlockingExternalIO(operation))
            {
                self.boundary.event("background");
            } else {
                assert!(
                    resources
                        .resources
                        .contains_key(&ResourceID::BlockedExternalContinue(operation)),
                    "local ctl must never start a network-capture clock"
                );
                self.boundary.event("continue-request");
                self.boundary.until(&self.boundary.release_continue).await;
                self.boundary.event("continue-grant");
            }
            return (None, GlobalResponse::RequestResources(ResumeStatus::Normal));
        }
        let GlobalRequest::Network(request) = request.2 else {
            panic!("unexpected ctl request")
        };
        let fail = |name| Err(NetworkRpcError::internal(name));
        let reply = match request {
            NetworkRequest::FdPublication(NetworkFdPublicationRequest::Acquire { files }) => {
                assert!(self.thread.original_connect.is_none());
                self.boundary.event("publication-acquire");
                Ok(NetworkReply::FdPublication(
                    NetworkFdPublicationReply::Admitted(NetworkFdPublicationAdmission {
                        permit: NetworkFdPublicationPermit {
                            owner,
                            files,
                            lease: NetworkStreamLeaseId::controlled_fixture(90),
                        },
                        acknowledged_sequence: 0,
                        acknowledged_generation: 0,
                        recovery: None,
                    }),
                ))
            }
            NetworkRequest::FdPublication(NetworkFdPublicationRequest::ReleaseEmpty { permit }) => {
                assert_eq!(permit.owner, owner);
                self.boundary.event("publication-release");
                Ok(NetworkReply::FdPublication(
                    NetworkFdPublicationReply::Released,
                ))
            }
            NetworkRequest::NativeBeginOriginalConnect { arguments } => {
                assert_eq!(
                    arguments,
                    self.thread.original_connect.as_ref().unwrap().arguments
                );
                assert_eq!(arguments.kind, Kind::EpollCtl);
                assert!(arguments.binding.is_none());
                assert_eq!(
                    (
                        arguments.fd,
                        arguments.length,
                        arguments.original_count,
                        arguments.address
                    ),
                    (7, libc::EPOLL_CTL_ADD, 8, 0x2000)
                );
                self.boundary.event("begin");
                if self.boundary.failure == Failure::Begin {
                    fail("controlled ctl begin failure")
                } else {
                    let admission = Admission {
                        call: crate::network_replay::NetworkStreamCallId::controlled_fixture(1),
                        arguments,
                    };
                    *self.boundary.admission.lock().unwrap() = Some(admission.clone());
                    Ok(NetworkReply::OriginalConnectAdmission(admission))
                }
            }
            NetworkRequest::NativeSubmitOriginalConnect { admission } => {
                assert_eq!(
                    Some(&admission),
                    self.boundary.admission.lock().unwrap().as_ref()
                );
                assert_eq!(
                    self.thread.original_connect.as_ref().unwrap().admission,
                    Some(admission)
                );
                self.boundary.event("submit");
                if self.boundary.failure == Failure::Submit {
                    fail("controlled ctl submit failure")
                } else {
                    Ok(NetworkReply::Unit)
                }
            }
            NetworkRequest::NativeOriginalConnectOutcome { admission } => {
                let local = self.thread.original_connect.as_ref().unwrap();
                assert!(local.invoked);
                let returned = local
                    .returned
                    .expect("outcome must have a backend result input");
                assert_eq!(local.admission.as_ref(), Some(&admission));
                assert!(!self.boundary.events().contains(&"continue-request"));
                self.boundary.event("outcome-pending");
                self.boundary.observed.store(true, Ordering::SeqCst);
                self.boundary.until(&self.boundary.release_outcome).await;
                self.boundary.event("outcome-returned");
                if self.boundary.failure == Failure::Outcome {
                    fail("controlled ctl outcome failure")
                } else {
                    Ok(NetworkReply::OriginalConnectOutcome(
                        crate::network_runtime::original_connect::Outcome {
                            admission,
                            returned,
                            pin: None,
                            address: None,
                            socket: None,
                            read_copy: None,
                        },
                    ))
                }
            }
            NetworkRequest::NativeRetireOriginalConnect { admission } => {
                assert_eq!(
                    Some(&admission),
                    self.boundary.admission.lock().unwrap().as_ref()
                );
                let expected = if self.config.sequentialize_threads {
                    "continue-grant"
                } else {
                    "outcome-returned"
                };
                assert_eq!(self.boundary.events().last(), Some(&expected));
                self.boundary.event("retire");
                if self.boundary.failure == Failure::Retire {
                    fail("controlled ctl retire failure")
                } else {
                    Ok(NetworkReply::Unit)
                }
            }
            NetworkRequest::NativeOriginalConnectFailed { local, detail } => {
                assert_eq!(Some(&local), self.thread.original_connect.as_ref());
                assert!(
                    detail.contains("controlled ctl")
                        || detail.contains("no real backend completion")
                );
                self.boundary.event("failure-fence");
                Ok(NetworkReply::Unit)
            }
            other => panic!("unexpected ctl request {other:?}"),
        };
        (None, GlobalResponse::Network(reply))
    }
    fn config(&self) -> &Config {
        self.config
    }
}
struct NoStack;
struct NoStackGuard;
impl Drop for NoStackGuard {
    fn drop(&mut self) {}
}
impl Stack for NoStack {
    type StackGuard = NoStackGuard;
    fn size(&self) -> usize {
        panic!("unexpected stack")
    }
    fn capacity(&self) -> usize {
        panic!("unexpected stack")
    }
    fn push<'s, V>(&mut self, _: V) -> Addr<'s, V> {
        panic!("unexpected stack")
    }
    fn reserve<'s, V>(&mut self) -> AddrMut<'s, V> {
        panic!("unexpected stack")
    }
    fn commit(self) -> Result<Self::StackGuard, Errno> {
        panic!("unexpected stack")
    }
}
#[reverie::tool]
impl Guest<Detcore<Delegate>> for CtlGuest<'_> {
    type Memory = LocalMemory;
    type Stack = NoStack;
    fn tid(&self) -> Tid {
        Tid::from_raw(self.thread.dettid.as_raw())
    }
    fn pid(&self) -> Tid {
        Tid::from_raw(self.thread.detpid.unwrap().as_raw())
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn memory(&self) -> Self::Memory {
        panic!("handler must not copy event or reread fd")
    }
    fn thread_state(&self) -> &crate::ThreadState<(DelegateMode, usize)> {
        &self.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<(DelegateMode, usize)> {
        &mut self.thread
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("unexpected regs")
    }
    async fn stack(&mut self) -> NoStack {
        panic!("unexpected stack")
    }
    async fn daemonize(&mut self) {
        panic!("unexpected daemon")
    }
    async fn inject<S: SyscallInfo>(&mut self, syscall: S) -> Result<i64, Errno> {
        let (nr, raw) = syscall.into_parts();
        assert_eq!(nr, syscalls::Sysno::epoll_ctl);
        let mode = self.thread.as_ref().0;
        let local = self.thread.original_connect.as_mut().unwrap();
        assert!(local.invoked && local.returned.is_none());
        assert_eq!(
            local.raw_arguments,
            [raw.arg0, raw.arg1, raw.arg2, raw.arg3, raw.arg4, raw.arg5]
        );
        let returned = if matches!(mode, DelegateMode::Errno) {
            -i64::from(libc::EFAULT)
        } else {
            0
        };
        // Explicit completion INPUT, not a fabricated native certificate.
        if !matches!(mode, DelegateMode::MissingReturn) {
            local.returned = Some(returned);
        }
        self.boundary.event("delegate");
        if returned < 0 {
            Err(Errno::EFAULT)
        } else {
            Ok(returned)
        }
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> Never {
        panic!("unexpected tail inject")
    }
    fn set_timer(&mut self, _: TimerSchedule) -> Result<(), Error> {
        panic!("unexpected timer")
    }
    fn set_timer_precise(&mut self, _: TimerSchedule) -> Result<(), Error> {
        panic!("unexpected timer")
    }
    fn read_clock(&mut self) -> Result<u64, Error> {
        panic!("local ctl must not read clock")
    }
}
fn setup(
    config: &Config,
    mode: DelegateMode,
    failure: Failure,
) -> (
    Detcore<Delegate>,
    CtlGuest<'_>,
    Arc<Boundary>,
    syscalls::EpollCtl,
) {
    let tid = Tid::from_raw(71);
    let tool = Detcore::<Delegate>::new(tid, config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.detpid = Some(thread.dettid);
    thread.stats.syscall_count = 10;
    *thread.as_mut() = (mode, 0);
    *thread.file_metadata.lock().unwrap() =
        crate::tool_local::FileMetadata::empty_network_fixture(thread.dettid);
    let boundary = Arc::new(Boundary::new(failure));
    let call = Syscall::from_raw(
        syscalls::Sysno::epoll_ctl,
        syscalls::SyscallArgs::new(7, libc::EPOLL_CTL_ADD as usize, 8, 0x2000, 0, 0),
    );
    let Syscall::EpollCtl(call) = call else {
        unreachable!()
    };
    (
        tool,
        CtlGuest {
            config,
            thread,
            boundary: boundary.clone(),
        },
        boundary,
        call,
    )
}
fn config(sequential: bool) -> Config {
    let mut config = Config {
        sequentialize_threads: sequential,
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Record;
    config
}
#[tokio::test]
async fn ctl_handler_keeps_pending_outcome_backgrounded_then_pays_exact_continuation() {
    for sequential in [true, false] {
        for mode in [DelegateMode::Success, DelegateMode::Errno] {
            let config = config(sequential);
            let (tool, mut guest, boundary, call) = setup(&config, mode, Failure::None);
            boundary.release_continue.store(false, Ordering::SeqCst);
            let result = {
                let mut pending = Box::pin(tool.network_original_epoll_ctl(&mut guest, call));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                assert!(boundary.observed.load(Ordering::SeqCst));
                assert!(!boundary.events().contains(&"continue-request"));
                assert!(!boundary.events().contains(&"retire"));
                boundary.release_outcome.store(true, Ordering::SeqCst);
                boundary.changed.notify_waiters();
                if sequential {
                    assert!(futures::poll!(pending.as_mut()).is_pending());
                    assert_eq!(boundary.events().last(), Some(&"continue-request"));
                    assert!(!boundary.events().contains(&"retire"));
                    boundary.release_continue.store(true, Ordering::SeqCst);
                    boundary.changed.notify_waiters();
                }
                pending.await
            };
            match mode {
                DelegateMode::Success => assert_eq!(result.unwrap(), 0),
                DelegateMode::Errno => assert!(matches!(result, Err(Error::Errno(Errno::EFAULT)))),
                _ => unreachable!(),
            }
            assert_eq!(guest.thread.as_ref().1, 1);
            assert!(guest.thread.original_connect.is_none());
            let expected = if sequential {
                vec![
                    "publication-acquire",
                    "publication-release",
                    "background",
                    "begin",
                    "submit",
                    "delegate",
                    "outcome-pending",
                    "outcome-returned",
                    "continue-request",
                    "continue-grant",
                    "retire",
                ]
            } else {
                vec![
                    "publication-acquire",
                    "publication-release",
                    "begin",
                    "submit",
                    "delegate",
                    "outcome-pending",
                    "outcome-returned",
                    "retire",
                ]
            };
            assert_eq!(boundary.events(), expected);
        }
    }
}
#[tokio::test]
async fn ctl_handler_returned_failures_pay_continuation_before_retained_failure_fence() {
    for sequential in [true, false] {
        for (mode, failure, invoked, returned) in [
            (DelegateMode::Success, Failure::Begin, false, None),
            (DelegateMode::Success, Failure::Submit, false, None),
            (DelegateMode::Tool, Failure::None, true, None),
            (DelegateMode::Io, Failure::None, true, None),
            (DelegateMode::MissingReturn, Failure::None, true, None),
            (DelegateMode::Success, Failure::Outcome, true, Some(0)),
            (DelegateMode::Success, Failure::Retire, true, Some(0)),
        ] {
            let config = config(sequential);
            let (tool, mut guest, boundary, call) = setup(&config, mode, failure);
            boundary.release_outcome.store(true, Ordering::SeqCst);
            {
                let mut pending = Box::pin(tool.network_original_epoll_ctl(&mut guest, call));
                assert!(
                    futures::poll!(pending.as_mut()).is_pending(),
                    "failed Call must await terminal custody"
                );
                let events = boundary.events();
                assert_eq!(events.last(), Some(&"failure-fence"));
                assert_eq!(
                    events.iter().filter(|e| **e == "continue-request").count(),
                    usize::from(sequential)
                );
                assert_eq!(
                    events.iter().filter(|e| **e == "continue-grant").count(),
                    usize::from(sequential)
                );
                if sequential {
                    assert!(
                        events.iter().position(|e| *e == "continue-grant").unwrap()
                            < events.iter().position(|e| *e == "failure-fence").unwrap()
                    );
                }
                assert_eq!(events.contains(&"retire"), failure == Failure::Retire);
                assert_eq!(
                    events.contains(&"outcome-pending"),
                    matches!(failure, Failure::Outcome | Failure::Retire)
                );
            }
            let local = guest
                .thread
                .original_connect
                .as_ref()
                .expect("cancellation retains Local");
            assert_eq!(local.invoked, invoked);
            assert_eq!(local.returned, returned);
            assert_eq!(local.admission.is_some(), failure != Failure::Begin);
            assert_eq!(guest.thread.as_ref().1, usize::from(invoked));
        }
    }
}
#[tokio::test]
async fn ctl_handler_cancelled_outcome_does_not_post_continue_or_drop_original_evidence() {
    for sequential in [true, false] {
        let config = config(sequential);
        let (tool, mut guest, boundary, call) =
            setup(&config, DelegateMode::Success, Failure::None);
        {
            let mut pending = Box::pin(tool.network_original_epoll_ctl(&mut guest, call));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            assert!(boundary.observed.load(Ordering::SeqCst));
        }
        let local = guest.thread.original_connect.as_ref().unwrap();
        assert!(local.invoked && local.admission.is_some());
        assert_eq!(local.returned, Some(0));
        assert_eq!(boundary.events().last(), Some(&"outcome-pending"));
        assert!(!boundary.events().contains(&"continue-request"));
        assert!(!boundary.events().contains(&"retire"));
        assert!(!boundary.events().contains(&"failure-fence"));
        assert_eq!(guest.thread.as_ref().1, 1);
    }
}
