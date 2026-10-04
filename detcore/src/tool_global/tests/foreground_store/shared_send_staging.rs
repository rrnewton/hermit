//! Actual shared Sendto caller and Global publication/reader RPCs. Initial
//! root, socket profile, finite timeout and completed Connect are controlled
//! premises. The peer-join callback checks custody and deliberately refuses;
//! no native peer join, provider submission or transmitted bytes are claimed.
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Sendto;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::network_replay::NetworkFdPublicationRequest;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkFdReadBegin;
use crate::network_replay::original_connect::Kind;

const CONTROLLED_STOP: &str = "controlled stop after original shared Sendto staging";

struct NoMemory;
impl MemoryAccess for NoMemory {
    fn read_vectored(&self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
        panic!("staging must not read source memory")
    }
    fn write_vectored(&mut self, _: &[IoSlice], _: &mut [IoSliceMut]) -> Result<usize, Errno> {
        panic!("staging must not write guest memory")
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
        0
    }
    fn capacity(&self) -> usize {
        0
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

#[derive(Clone, Copy, Debug)]
enum ReadReply {
    Original,
    UnknownLease,
    WrongFd,
    WrongOwner,
    AlreadyReleased,
}
struct StagingGuest<'a> {
    global: &'a GlobalState,
    config: &'a Config,
    thread: crate::ThreadState<()>,
    tid: Tid,
    original: Sendto,
    reply: ReadReply,
    requests: Mutex<Vec<GlobalRequest>>,
    responses: Mutex<Vec<GlobalResponse>>,
    admitted: Mutex<Option<NetworkFdReadAdmission>>,
    joined: usize,
}
#[reverie::tool]
impl GlobalRPC<GlobalState> for StagingGuest<'_> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.requests.lock().unwrap().push(message.2.clone());
        let mut response = self.global.receive_rpc(self.tid, message).await;
        if let GlobalResponse::Network(Ok(NetworkReply::FdRead(NetworkFdReadBegin::Admitted(
            read,
        )))) = &mut response.1
        {
            assert!(
                self.admitted
                    .lock()
                    .unwrap()
                    .replace((**read).clone())
                    .is_none()
            );
            match self.reply {
                ReadReply::Original => {}
                ReadReply::UnknownLease => {
                    read.publication.permit.lease =
                        crate::network_replay::NetworkStreamLeaseId::controlled_fixture(u64::MAX)
                }
                ReadReply::WrongFd => read.fd += 1,
                ReadReply::WrongOwner => {
                    read.publication.permit.owner.mm = read
                        .publication
                        .permit
                        .owner
                        .mm
                        .for_exec(read.publication.permit.owner.thread)
                }
                ReadReply::AlreadyReleased => self
                    .global
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .finish_fd_read(read.publication.permit.owner, (**read).clone())
                    .unwrap(),
            }
        }
        self.responses.lock().unwrap().push(response.1.clone());
        response
    }
    fn config(&self) -> &Config {
        self.config
    }
}
#[reverie::tool]
impl Guest<Detcore> for StagingGuest<'_> {
    type Memory = NoMemory;
    type Stack = NoStack;
    fn tid(&self) -> Tid {
        self.tid
    }
    fn pid(&self) -> Tid {
        self.tid
    }
    fn ppid(&self) -> Option<Tid> {
        None
    }
    fn local_global_state(&self) -> Option<&GlobalState> {
        Some(self.global)
    }
    fn memory(&self) -> Self::Memory {
        NoMemory
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
        panic!("unexpected stack")
    }
    async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, Errno> {
        panic!("unexpected native submission")
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
    async fn join_followed_observation_timers(
        &mut self,
        original: Syscall,
    ) -> Result<(), reverie::Error> {
        assert_eq!(original.into_parts(), self.original.into_parts());
        let read = self.admitted.lock().unwrap().clone().unwrap();
        let engine = self.global.network_engine.as_ref().unwrap().lock().unwrap();
        engine
            .validate_fd_read_grant(read.publication.permit.owner, &read)
            .unwrap();
        assert_eq!(
            engine
                .shared_record_send_timeout(read.binding.unwrap().open_file)
                .unwrap(),
            5000
        );
        let local = self
            .thread
            .original_connect
            .as_ref()
            .expect("Local precedes peer join");
        let args = self.original.into_parts().1;
        assert_eq!(
            local.raw_arguments,
            [
                args.arg0, args.arg1, args.arg2, args.arg3, args.arg4, args.arg5
            ]
        );
        assert_eq!(
            local.arguments.kind,
            Kind::BlockingSendto {
                timeout_ticks: 5000
            }
        );
        assert_eq!(local.arguments.files, read.publication.permit.files);
        assert_eq!(local.arguments.binding, read.binding);
        assert_eq!(local.arguments.fd, read.fd);
        assert_eq!(local.arguments.address, args.arg1 as u64);
        assert_eq!(local.arguments.length, libc::MSG_NOSIGNAL);
        assert_eq!(local.arguments.original_count, 8);
        assert_eq!(
            local.arguments.operation,
            ExternalOpId::new(self.thread.dettid, self.thread.stats.syscall_count)
        );
        assert!(local.admission.is_none() && !local.invoked && local.returned.is_none());
        assert!(read.control.is_some());
        self.joined += 1;
        Err(reverie::Error::Tool(anyhow::anyhow!(CONTROLLED_STOP)))
    }
}

struct Fixture {
    global: GlobalState,
    config: Config,
    thread: crate::ThreadState<()>,
    tid: Tid,
    original: Sendto,
}
fn fixture() -> Fixture {
    let f = crate::network_replay::shared_send::tests::fixture();
    let owner = f.root.owner();
    let tid = Tid::from_raw(f.root.thread());
    let mut config = Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch: f.engine.lock().unwrap().native_trace_fixture().epoch,
        network_record_profile: Some(crate::config::NetworkRecordProfile::SharedMmV1),
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Record;
    let mut global = GlobalState::initialize(&config, false);
    let tool: Detcore = Detcore::new(tid, &config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.dettid = owner.thread;
    thread.detpid = Some(f.root.logical_process());
    thread.mm_id = owner.mm;
    thread.file_metadata = f.metadata;
    thread.memory_metadata = f.memory;

    f.engine
        .lock()
        .unwrap()
        .finish_fd_read(owner, f.read.clone())
        .unwrap();
    global.network_runtime = Some(f.runtime);
    global.network_engine = Some(Arc::new(f.engine));
    global.sched = Arc::new(Mutex::new(f.scheduler));
    global
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    let original = Sendto::new()
        .with_fd(f.read.fd)
        .with_buf(AddrMut::from_raw(0x1000))
        .with_size(8)
        .with_flags(libc::MSG_NOSIGNAL as u32);
    Fixture {
        global,
        config,
        thread,
        tid,
        original,
    }
}
fn guest(f: &Fixture, reply: ReadReply) -> StagingGuest<'_> {
    StagingGuest {
        global: &f.global,
        config: &f.config,
        thread: f.thread.clone(),
        tid: f.tid,
        original: f.original,
        reply,
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(Vec::new()),
        admitted: Mutex::new(None),
        joined: 0,
    }
}
fn acquire_count(requests: &[GlobalRequest]) -> usize {
    requests
        .iter()
        .filter(|r| {
            matches!(
                r,
                GlobalRequest::Network(NetworkRequest::FdPublication(
                    NetworkFdPublicationRequest::Acquire { .. }
                ))
            )
        })
        .count()
}

#[tokio::test]
async fn shared_send_staging_full_caller_keeps_exact_reader_without_reacquire() {
    let f = fixture();
    let mut g = guest(&f, ReadReply::Original);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let before = f
        .global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    let turn = f.global.sched.lock().unwrap().turn;
    let time = f.global.global_time.lock().unwrap().as_nanos();
    let result = tool
        .controlled_shared_send_staging(&mut g, f.original)
        .await;
    let read = g
        .admitted
        .lock()
        .unwrap()
        .clone()
        .expect("actual Global reader response");
    let reverie::Error::Tool(error) = result.unwrap_err() else {
        panic!("controlled stop must remain Tool")
    };
    let message = error.root_cause().to_string();
    let old_busy = format!(
        "network replay mismatch: {:?}",
        NetworkReplayError::StreamOperationBusy(read.publication.permit.lease)
    );
    assert!(
        message == CONTROLLED_STOP || message == old_busy,
        "unexpected earlier refusal: {message}"
    );
    assert_eq!(
        message, CONTROLLED_STOP,
        "old route's exact retained table lease is {:?}",
        read.publication.permit.lease
    );
    assert_eq!(g.joined, 1);
    assert!(
        matches!(
            g.responses.lock().unwrap().last(),
            Some(GlobalResponse::Network(Ok(NetworkReply::Unit)))
        ),
        "actual FinishFdRead cleanup must succeed"
    );
    let requests = g.requests.lock().unwrap();
    assert_eq!(
        acquire_count(&requests),
        1,
        "only the pre-reader publication acquires the table"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| matches!(
                r,
                GlobalRequest::Network(NetworkRequest::BeginFdRead { .. })
            ))
            .count(),
        1
    );
    assert_eq!(requests.iter().filter(|r| matches!(r, GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) if admission == &read)).count(), 1);
    let engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        engine
            .validate_fd_read_grant(read.publication.permit.owner, &read)
            .is_err(),
        "controlled pre-transfer stop releases only its logical reader"
    );
    assert_eq!(engine.native_trace_fixture(), before);
    assert_eq!(f.global.sched.lock().unwrap().turn, turn);
    assert_eq!(f.global.global_time.lock().unwrap().as_nanos(), time);
}

#[tokio::test]
async fn shared_send_staging_rejects_unknown_stale_and_wrong_reader_before_local() {
    for reply in [
        ReadReply::UnknownLease,
        ReadReply::WrongFd,
        ReadReply::WrongOwner,
        ReadReply::AlreadyReleased,
    ] {
        let f = fixture();
        let mut g = guest(&f, reply);
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let before = f
            .global
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture();
        let result = tool
            .controlled_shared_send_staging(&mut g, f.original)
            .await;
        let reverie::Error::Tool(error) = result.unwrap_err() else {
            panic!("{reply:?} must remain a Tool refusal")
        };
        let expected = NetworkReplayError::FdPublicationProtocol(
            "reader is not the exact retained table/owner admission".into(),
        )
        .to_string();
        assert_eq!(error.root_cause().to_string(), expected, "{reply:?}");
        assert!(
            matches!(g.responses.lock().unwrap().last(), Some(GlobalResponse::Network(Err(NetworkRpcError::Internal(message)))) if message == &expected),
            "wrong reader cleanup must refuse the exact token"
        );
        assert_eq!(g.joined, 0);
        assert!(g.thread.original_connect.is_none());
        assert_eq!(acquire_count(&g.requests.lock().unwrap()), 1);
        let read = g.admitted.lock().unwrap().clone().unwrap();
        let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
        if matches!(reply, ReadReply::AlreadyReleased) {
            assert!(
                engine
                    .validate_fd_read_grant(read.publication.permit.owner, &read)
                    .is_err()
            );
        } else {
            engine
                .validate_fd_read_grant(read.publication.permit.owner, &read)
                .unwrap();
            engine
                .finish_fd_read(read.publication.permit.owner, read)
                .unwrap();
        }
        assert_eq!(engine.native_trace_fixture(), before);
    }
}

#[tokio::test]
async fn shared_send_staging_ordinary_path_still_publishes_before_local() {
    let f = fixture();
    let mut g = guest(&f, ReadReply::Original);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let arguments = tool
        .controlled_ordinary_send_staging(&mut g, f.original)
        .await
        .unwrap();
    assert_eq!(arguments.kind, Kind::Sendto);
    assert_eq!(acquire_count(&g.requests.lock().unwrap()), 1);
    assert!(g.admitted.lock().unwrap().is_none());
    assert_eq!(g.joined, 0);
    assert_eq!(
        g.thread.original_connect.as_ref().unwrap().arguments,
        arguments
    );
}
