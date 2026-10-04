//! Production Close classification and real FD-reader RPCs. Socket and
//! current-turn premises are controlled; no provider/native completion claim.
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Guest;
use reverie::Stack;
use reverie::syscalls::Addr;
use reverie::syscalls::AddrMut;
use reverie::syscalls::Close;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkFdReadBegin;

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
    reply: ReadReply,
    requests: Mutex<Vec<GlobalRequest>>,
    responses: Mutex<Vec<GlobalResponse>>,
    admitted: Mutex<Option<NetworkFdReadAdmission>>,
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
}

struct Fixture {
    global: GlobalState,
    config: Config,
    thread: crate::ThreadState<()>,
    tid: Tid,
    original: Close,
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
    let original = Close::new().with_fd(f.read.fd);
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
        reply,
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(Vec::new()),
        admitted: Mutex::new(None),
    }
}

#[tokio::test]
async fn foreground_close_unissued_birth_releases_actual_reader_without_staging() {
    let f = fixture();
    let mut g = guest(&f, ReadReply::Original);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let time = f.global.global_time.lock().unwrap().as_nanos();
    let turn = f.global.sched.lock().unwrap().turn;
    let trace = f
        .global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    assert_eq!(
        tool.controlled_foreground_close(&mut g, f.original)
            .await
            .unwrap(),
        None
    );
    let read = g.admitted.lock().unwrap().clone().unwrap();
    assert!(g.thread.original_connect.is_none());
    let requests = g.requests.lock().unwrap();
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
    assert!(!requests.iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect { .. })
    )));
    assert!(matches!(
        g.responses.lock().unwrap().last(),
        Some(GlobalResponse::Network(Ok(NetworkReply::Unit)))
    ));
    let engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        engine
            .validate_fd_read_grant(read.publication.permit.owner, &read)
            .is_err()
    );
    assert_eq!(engine.native_trace_fixture(), trace);
    assert_eq!(f.global.global_time.lock().unwrap().as_nanos(), time);
    assert_eq!(f.global.sched.lock().unwrap().turn, turn);
}

#[tokio::test]
async fn foreground_close_bad_reader_is_error_not_generic_fallback() {
    for reply in [
        ReadReply::UnknownLease,
        ReadReply::WrongFd,
        ReadReply::WrongOwner,
        ReadReply::AlreadyReleased,
    ] {
        let f = fixture();
        let mut g = guest(&f, reply);
        let tool: Detcore = Detcore::new(f.tid, &f.config);
        let error = tool
            .controlled_foreground_close(&mut g, f.original)
            .await
            .unwrap_err();
        let reverie::Error::Tool(error) = error else {
            panic!("{reply:?}: expected Tool reader refusal");
        };
        assert_eq!(
            error.root_cause().to_string(),
            NetworkReplayError::FdPublicationProtocol(
                "reader is not the exact retained table/owner admission".into()
            )
            .to_string()
        );
        assert!(g.thread.original_connect.is_none());
        assert!(!g.requests.lock().unwrap().iter().any(|r| matches!(
            r,
            GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect { .. })
        )));
    }
}

// Controlled provider/query/getter facts enter through Plan::complete and the
// real original Socket publication transaction. No FiniteCloseBirth constructor.
fn born_fixture() -> Fixture {
    use crate::network_replay::NetworkFdMutationBegin;
    use crate::network_replay::NetworkFdMutationKind;
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::Kind;
    use crate::network_replay::original_installation::FreshStreamEnrollment;
    use crate::network_runtime::original_installation::Source;
    use crate::network_runtime::original_installation::installation_fixture;
    let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
    let tid = Tid::from_raw(raw);
    let (runtime, root, metadata, memory, claim) =
        crate::network_runtime::controlled_foreground_runtime(raw);
    let owner = root.owner();
    let template = crate::network_replay::replay_connect::fixture(LogicalTime::ZERO, false)
        .engine
        .native_trace_fixture();
    let profile = template.fresh_stream_profiles[0].clone();
    let mut config = Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch: template.epoch,
        network_record_profile: Some(crate::config::NetworkRecordProfile::SharedMmV1),
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Record;
    let mut global = GlobalState::initialize(&config, false);
    let tool: Detcore = Detcore::new(tid, &config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.dettid = owner.thread;
    thread.detpid = Some(root.logical_process());
    thread.mm_id = owner.mm;
    thread.file_metadata = metadata.clone();
    thread.memory_metadata = memory;
    global
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    global
        .sched
        .lock()
        .unwrap()
        .controlled_foreground_store_grant(&root);
    let mut engine = NetworkReplayEngine::record_shared_mm_attempts(template.epoch);
    engine.fd_table_fixture_enable();
    engine
        .register_initial_census(root.association(), &claim, owner.thread)
        .unwrap();
    engine
        .associate_fd_metadata(owner, &metadata, &metadata.lock().unwrap())
        .unwrap();
    let NetworkFdMutationBegin::Admitted(mutation) = engine
        .begin_fd_mutation(owner, root.files(), NetworkFdMutationKind::Socket)
        .unwrap()
    else {
        panic!("real Socket mutation");
    };
    engine
        .submit_fd_mutation(owner, mutation.publication.permit)
        .unwrap();
    let socket = engine
        .begin_original_socket(
            owner,
            Arguments {
                kind: Kind::Socket,
                operation: ExternalOpId::new(owner.thread, 1),
                files: root.files(),
                binding: None,
                fd: libc::AF_INET,
                address: libc::SOCK_STREAM as u64,
                length: libc::IPPROTO_TCP,
                original_count: 0,
            },
            (*mutation).clone(),
        )
        .unwrap();
    global.network_engine = Some(Arc::new(Mutex::new(engine)));
    global.network_runtime = Some(runtime);
    let target = global
        .network_runtime
        .as_ref()
        .unwrap()
        .prepare_native_capture_task(owner)
        .unwrap();
    let authority = global
        .original_socket_birth_authority(owner, &socket, &target)
        .unwrap()
        .unwrap();
    let policy = crate::network_runtime::socket_birth_policy::controlled_birth_receipt(
        authority, owner, &socket,
    )
    .unwrap();
    let mut observed_metadata = metadata.lock().unwrap();
    let mut engine = global.network_engine.as_ref().unwrap().lock().unwrap();
    engine
        .original_connect_provider_submitted(owner, &socket)
        .unwrap();
    engine
        .original_call_prepared(owner, &socket, None, 71)
        .unwrap();
    engine.original_connect_invoked(owner, &socket).unwrap();
    engine
        .original_connect_selected(owner, &socket, 71, (7, 31, 101, 13, 19))
        .unwrap();
    engine.original_connect_returned(owner, &socket, 5).unwrap();
    engine
        .original_connect_provider_retired(owner, &socket, 5)
        .unwrap();
    engine
        .original_connect_pin_released(owner, &socket)
        .unwrap();
    let receipt = installation_fixture(
        owner,
        metadata.clone(),
        mutation.publication.permit,
        Source::Socket(socket.call),
        71,
        5,
        false,
    );
    let receipt =
        crate::network_runtime::original_installation::installation_with_controlled_birth(
            receipt, policy,
        )
        .unwrap();
    engine
        .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(5))
        .unwrap();
    engine
        .publish_original_installation(
            owner,
            &mutation.publication,
            &receipt,
            (&metadata, &mut observed_metadata),
            (
                nix::fcntl::OFlag::O_RDWR,
                None,
                Some(FreshStreamEnrollment {
                    key: profile.key,
                    namespace: crate::network_replay::NetworkStreamNamespace {
                        device: 1,
                        inode: 1,
                    },
                    observed_profile: Some(profile),
                }),
            ),
            LogicalTime::ZERO,
        )
        .unwrap();
    engine
        .original_socket_publication_finished(owner, &socket, mutation.publication.permit)
        .unwrap();
    engine.finish_original_connect(owner, &socket).unwrap();
    assert!(root.is_sole_initial_root(owner));
    assert!(engine.native_trace_fixture().channels.is_empty());
    drop(engine);
    drop(observed_metadata);
    Fixture {
        global,
        config,
        thread,
        tid,
        original: Close::new().with_fd(5),
    }
}

fn owner(f: &Fixture) -> crate::network_replay::NetworkStreamOwner {
    crate::network_replay::NetworkStreamOwner {
        thread: f.thread.dettid,
        mm: f.thread.mm_id,
    }
}
fn arm_alarm(f: &Fixture) -> LogicalTime {
    let now = f.global.global_time.lock().unwrap().as_nanos();
    f.global.sched.lock().unwrap().register_alarm(
        f.thread.detpid.unwrap(),
        f.thread.dettid,
        now,
        LogicalTime::from_secs(8),
        LogicalTime::ZERO,
        nix::sys::signal::Signal::SIGALRM,
    );
    now
}
fn check_normal_clock(f: &Fixture, now: LogicalTime) {
    let mut scheduler = f.global.sched.lock().unwrap();
    let (maintenance, external, signals) = scheduler.controlled_original_close_clock_probe(
        owner(f),
        ExternalOpId::new(f.thread.dettid, f.thread.stats.syscall_count),
        &f.global.global_time,
    );
    assert!(maintenance);
    assert!(!external);
    assert_eq!(signals, 0);
    assert_eq!(f.global.global_time.lock().unwrap().as_nanos(), now);
    assert_eq!(
        scheduler.alarm_remaining(f.thread.detpid.unwrap(), now),
        LogicalTime::from_secs(8)
    );
}

#[tokio::test]
async fn foreground_close_full_caller_unconnected_birth_keeps_normal_alarm_and_actual_abort() {
    let f = born_fixture();
    let mut g = guest(&f, ReadReply::Original);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let time = arm_alarm(&f);
    let before = f
        .global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .native_trace_fixture();
    let error = tool
        .controlled_foreground_close(&mut g, f.original)
        .await
        .unwrap_err();
    let reverie::Error::Tool(error) = error else {
        panic!("known pre-provider stop");
    };
    assert_eq!(
        error.root_cause().to_string(),
        "accepted runtime has no endpoint"
    );
    let local = g
        .thread
        .original_connect
        .as_ref()
        .expect("retained original intent");
    let admission = local
        .admission
        .as_ref()
        .expect("actual reader transferred before preparation");
    assert!(!local.invoked);
    assert_eq!(local.returned, None);
    assert_eq!(
        local.arguments.kind,
        crate::network_replay::original_connect::Kind::Close
    );
    assert_eq!(local.raw_arguments, [5, 0, 0, 0, 0, 0]);
    let requests = g.requests.lock().unwrap();
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
    assert!(!requests.iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::FinishFdRead { .. })
    )));
    assert!(!requests.iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::NativeSubmitOriginalConnect { .. })
    )));
    let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    // The existing runtime setup branch positively resolves a known unsubmitted
    // preparation; absence is checked only after that actual abort returned.
    assert!(
        matches!(engine.foreground_close_origin(owner(&f), admission), Err(NetworkReplayError::UnknownStreamCall(call)) if call == admission.call)
    );
    let NetworkFdReadBegin::Admitted(read) = engine
        .begin_fd_read(owner(&f), admission.arguments.files, 5)
        .unwrap()
    else {
        panic!("abort released its exact exclusion");
    };
    engine.finish_fd_read(owner(&f), *read).unwrap();
    assert_eq!(engine.native_trace_fixture(), before);
    drop(engine);
    check_normal_clock(&f, time);
}

async fn begin_at_barrier(
    f: &Fixture,
) -> crate::tool_global::foreground_close::PreparedForegroundClose {
    let files = f.thread.file_metadata.lock().unwrap().files_id;
    let read = {
        let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
        let NetworkFdReadBegin::Admitted(read) = engine.begin_fd_read(owner(f), files, 5).unwrap()
        else {
            panic!("real read");
        };
        *read
    };
    assert!(
        f.global
            .foreground_close_candidate(f.tid, &f.thread, &read)
            .unwrap()
    );
    let arguments = crate::network_replay::original_connect::Arguments {
        kind: crate::network_replay::original_connect::Kind::Close,
        operation: ExternalOpId::new(f.thread.dettid, f.thread.stats.syscall_count),
        files: read.publication.permit.files,
        binding: read.binding,
        fd: 5,
        address: 0,
        length: 0,
        original_count: 0,
    };
    f.global
        .begin_foreground_original_close(f.tid, &f.thread, read, arguments, [5, 0, 0, 0, 0, 0])
        .await
        .unwrap()
}

#[tokio::test]
async fn foreground_close_barrier_revalidates_tuple_and_actual_epoch() {
    let f = born_fixture();
    let time = arm_alarm(&f);
    let prepared = begin_at_barrier(&f).await;
    let admission = prepared.origin.admission();
    f.global
        .validate_foreground_close_callback(f.tid, &f.thread, admission, [5, 0, 0, 0, 0, 0])
        .unwrap();
    for index in 0..6 {
        let mut wrong = [5, 0, 0, 0, 0, 0];
        wrong[index] += 1;
        let error = f
            .global
            .validate_foreground_close_callback(f.tid, &f.thread, admission, wrong)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            NetworkReplayError::FdPublicationProtocol(
                "finite Close changed retained Normal/entry/Call".into()
            )
            .to_string()
        );
    }
    assert_eq!(
        f.global
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_connect_cancellation(owner(&f), admission)
            .unwrap(),
        (false, false)
    );
    check_normal_clock(&f, time);
    let root = f
        .global
        .network_runtime
        .as_ref()
        .unwrap()
        .foreground_root(owner(&f))
        .unwrap();
    f.global
        .sched
        .lock()
        .unwrap()
        .controlled_shared_foreground_grant(&root);
    assert!(
        f.global
            .validate_foreground_close_callback(f.tid, &f.thread, admission, [5, 0, 0, 0, 0, 0])
            .is_err()
    );
}

#[tokio::test]
async fn foreground_close_consumed_before_submission_keeps_exact_call_debt() {
    let f = born_fixture();
    let prepared = begin_at_barrier(&f).await;
    let admission = prepared.origin.admission().clone();
    let local = crate::network_replay::original_connect::Local {
        arguments: admission.arguments.clone(),
        raw_arguments: [5, 0, 0, 0, 0, 0],
        admission: Some(admission.clone()),
        invoked: false,
        returned: None,
    };
    f.global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .original_connect_consumed(owner(&f), &local)
        .unwrap();
    assert!(
        f.global
            .validate_foreground_close_callback(f.tid, &f.thread, &admission, local.raw_arguments)
            .is_err()
    );
    let engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine
            .original_connect_cancellation(owner(&f), &admission)
            .unwrap(),
        (true, true)
    );
    assert_eq!(
        engine
            .original_connect_result(owner(&f), &admission)
            .unwrap(),
        None
    );
    assert!(
        engine
            .foreground_close_origin(owner(&f), &admission)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn foreground_close_foreign_birth_root_is_not_equal_numeric_owner() {
    let f = born_fixture();
    let other = born_fixture();
    assert_eq!(owner(&f), owner(&other));
    let other_root = other
        .global
        .network_runtime
        .as_ref()
        .unwrap()
        .foreground_root(owner(&other))
        .unwrap();
    let files = f.thread.file_metadata.lock().unwrap().files_id;
    let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    let NetworkFdReadBegin::Admitted(read) = engine.begin_fd_read(owner(&f), files, 5).unwrap()
    else {
        panic!("actual read");
    };
    assert_eq!(
        engine
            .validate_foreground_close_birth_root(owner(&f), &read, &other_root)
            .unwrap_err()
            .to_string(),
        NetworkReplayError::FdPublicationProtocol(
            "finite Close changed original birth root".into()
        )
        .to_string()
    );
    engine.finish_fd_read(owner(&f), *read).unwrap();
}

// Real descriptor metadata and mutation publication; the successful dup result
// is an explicit controlled premise, not a native syscall or extra file ref.
fn install_alias(f: &mut Fixture, newfd: i32) {
    use crate::network_replay::NetworkFdInstallKind;
    use crate::network_replay::NetworkFdMutationBegin;
    use crate::network_replay::NetworkFdMutationKind;
    let owner = owner(f);
    let source = f.thread.descriptor_binding(5).unwrap();
    let mutation = {
        let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
        let NetworkFdMutationBegin::Admitted(mutation) = engine
            .begin_fd_mutation(
                owner,
                source.slot.files,
                NetworkFdMutationKind::Alias {
                    source_fd: 5,
                    source: Some(source),
                    kind: NetworkFdInstallKind::Dup,
                    cloexec: false,
                    destination: None,
                    replaced: None,
                },
            )
            .unwrap()
        else {
            panic!("actual alias admission");
        };
        engine
            .submit_fd_mutation(owner, mutation.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(i64::from(newfd)))
            .unwrap();
        mutation
    };
    assert_eq!(
        f.thread
            .dup_fd(5, newfd, nix::fcntl::OFlag::empty())
            .unwrap(),
        None
    );
    let mut metadata = f.thread.file_metadata.lock().unwrap();
    let change = *metadata.pending_network_installations().last().unwrap();
    assert_eq!(change.after.unwrap().binding.open_file, source.open_file);
    let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    let effect = engine
        .confirm_fd_installation(owner, mutation.publication.permit, change)
        .unwrap();
    metadata
        .associate_network_installation(change.installation_generation, effect)
        .unwrap();
    let batch = metadata
        .publication_snapshot(&mutation.publication)
        .unwrap();
    engine
        .publish_fd_publication(owner, mutation.publication.permit, &batch)
        .unwrap();
    metadata.publication_acknowledge(&batch).unwrap();
    engine
        .acknowledge_fd_publication(owner, mutation.publication.permit, &batch)
        .unwrap();
    metadata.publication_server_acknowledge(&batch).unwrap();
}

#[tokio::test]
async fn foreground_close_alias_uses_same_birth_and_exact_selected_reader() {
    let mut f = born_fixture();
    install_alias(&mut f, 9);
    f.original = Close::new().with_fd(9);
    let mut g = guest(&f, ReadReply::Original);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let error = tool
        .controlled_foreground_close(&mut g, f.original)
        .await
        .unwrap_err();
    let reverie::Error::Tool(error) = error else {
        panic!("actual pre-provider abort");
    };
    assert_eq!(
        error.root_cause().to_string(),
        "accepted runtime has no endpoint"
    );
    let read = g.admitted.lock().unwrap().clone().unwrap();
    assert_eq!(read.fd, 9);
    assert_eq!(read.binding, Some(g.thread.descriptor_binding(9).unwrap()));
    assert_eq!(
        read.binding.unwrap().open_file,
        g.thread.descriptor_binding(5).unwrap().open_file
    );
    let local = g.thread.original_connect.as_ref().unwrap();
    assert_eq!(local.arguments.fd, 9);
    assert_eq!(local.raw_arguments, [9, 0, 0, 0, 0, 0]);
    assert!(local.admission.is_some());
    assert!(!local.invoked);
    assert!(!g.requests.lock().unwrap().iter().any(|r| matches!(
        r,
        GlobalRequest::Network(NetworkRequest::FinishFdRead { .. })
    )));
}

// Existing birth fixture uses /dev/null PIDFD stand-ins and controlled provider
// responses. Terminal delivery below calls the real Tool consumer, but does not
// execute a kernel wait4 or establish a native child-exit result.
struct SharedFixture {
    f: Fixture,
    child: Arc<crate::network_runtime::ForegroundRoot>,
    child_state: crate::ThreadState<()>,
    projection: Arc<crate::network_runtime::native_birth_outcome::NativeTaskProjection>,
    _retained: Box<dyn std::any::Any>,
}
impl SharedFixture {
    async fn new() -> Self {
        use crate::network_replay::NetworkFdMutationBegin;
        use crate::network_replay::NetworkFdMutationKind;
        use crate::network_replay::original_connect::Arguments;
        use crate::network_replay::original_connect::Kind;
        use crate::network_replay::original_installation::FreshStreamEnrollment;
        use crate::network_runtime::original_installation::Source;
        use crate::network_runtime::original_installation::installation_fixture;
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let template = crate::network_replay::replay_connect::fixture(LogicalTime::ZERO, false)
            .engine
            .native_trace_fixture();
        let profile = template.fresh_stream_profiles[0].clone();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: template.epoch,
            network_record_profile: Some(crate::config::NetworkRecordProfile::SharedMmV1),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Record;
        let mut global = GlobalState::initialize(&config, false);
        let mut engine = NetworkReplayEngine::record_shared_mm_attempts(template.epoch);
        engine.fd_table_fixture_enable();
        let birth = crate::network_runtime::ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, claim| {
            let mut scheduler = global.sched.lock().unwrap();
            scheduler.controlled_foreground_store_grant(root);
            let owner = root.owner();
            let metadata = root.metadata().unwrap();
            let mut metadata_guard = metadata.lock().unwrap();
            engine.register_initial_census(root.association(), claim, owner.thread).unwrap();
            engine.associate_fd_metadata(owner, &metadata, &metadata_guard).unwrap();
            let NetworkFdMutationBegin::Admitted(mutation) = engine.begin_fd_mutation(owner, root.files(), NetworkFdMutationKind::Socket).unwrap() else { panic!("actual Socket mutation"); };
            engine.submit_fd_mutation(owner, mutation.publication.permit).unwrap();
            let socket = engine.begin_original_socket(owner, Arguments { kind: Kind::Socket,
                operation: ExternalOpId::new(owner.thread, 1), files: root.files(), binding: None,
                fd: libc::AF_INET, address: libc::SOCK_STREAM as u64, length: libc::IPPROTO_TCP, original_count: 0 }, (*mutation).clone()).unwrap();
            // Same production issuer as Global uses; current Normal and the
            // exact initial root are borrowed before the controlled child birth.
            let grant = scheduler.foreground_native_observation(owner, root).unwrap();
            let authority = crate::network_runtime::socket_birth_policy::SocketBirthAuthority::from_original(root.clone(), &grant, &socket).unwrap();
            let policy = crate::network_runtime::socket_birth_policy::controlled_birth_receipt(authority, owner, &socket).unwrap();
            engine.original_connect_provider_submitted(owner, &socket).unwrap();
            engine.original_call_prepared(owner, &socket, None, 71).unwrap();
            engine.original_connect_invoked(owner, &socket).unwrap();
            engine.original_connect_selected(owner, &socket, 71, (7, 31, 101, 13, 19)).unwrap();
            engine.original_connect_returned(owner, &socket, 5).unwrap();
            engine.original_connect_provider_retired(owner, &socket, 5).unwrap();
            engine.original_connect_pin_released(owner, &socket).unwrap();
            let receipt = installation_fixture(owner, metadata.clone(), mutation.publication.permit, Source::Socket(socket.call), 71, 5, false);
            let receipt = crate::network_runtime::original_installation::installation_with_controlled_birth(receipt, policy).unwrap();
            engine.confirm_fd_mutation_result(owner, mutation.publication.permit, Ok(5)).unwrap();
            engine.publish_original_installation(owner, &mutation.publication, &receipt,
                (&metadata, &mut metadata_guard), (nix::fcntl::OFlag::O_RDWR, None, Some(FreshStreamEnrollment {
                    key: profile.key, namespace: crate::network_replay::NetworkStreamNamespace { device: 1, inode: 1 }, observed_profile: Some(profile) })), LogicalTime::ZERO).unwrap();
            engine.original_socket_publication_finished(owner, &socket, mutation.publication.permit).unwrap();
            engine.finish_original_connect(owner, &socket).unwrap();
        }).await;
        global.sched.lock().unwrap().controlled_shared_birth_census(
            &birth.parent,
            &birth.child,
            &birth._birth,
        );
        let flags = birth._birth.flags();
        let NetworkFdMutationBegin::Admitted(clone) = engine
            .begin_fd_mutation(
                birth.parent.owner(),
                birth.parent.files(),
                NetworkFdMutationKind::Clone { flags },
            )
            .unwrap()
        else {
            panic!("actual clone mutation");
        };
        engine
            .submit_fd_mutation(birth.parent.owner(), clone.publication.permit)
            .unwrap();
        engine
            .confirm_fd_mutation_result(
                birth.parent.owner(),
                clone.publication.permit,
                Ok(i64::from(birth.child.owner().thread.as_raw())),
            )
            .unwrap();
        engine
            .register_cloned_fd_table(
                birth.parent.owner(),
                birth.child.owner(),
                birth.parent.logical_process(),
                flags,
            )
            .unwrap();
        let tool: Detcore = Detcore::new(tid, &config);
        let make = |root: &Arc<crate::network_runtime::ForegroundRoot>| {
            let mut state =
                tool.init_thread_state(Tid::from_raw(root.owner().thread.as_raw()), None);
            state.dettid = root.owner().thread;
            state.detpid = Some(birth.parent.logical_process());
            state.mm_id = root.owner().mm;
            state.thread_start_entered = true;
            state.file_metadata = birth.metadata.clone();
            state.memory_metadata = birth.memory.clone();
            state
        };
        let thread = make(&birth.parent);
        let child_state = make(&birth.child);
        let child = birth.child.clone();
        let projection = global
            .sched
            .lock()
            .unwrap()
            .shared_terminal_projection(child.owner(), birth.parent.logical_process())
            .unwrap()
            .unwrap();
        for root in [&birth.parent, &birth.child] {
            global
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(root.owner().thread, root.owner().mm);
            engine
                .associate_fd_metadata(
                    root.owner(),
                    &birth.metadata,
                    &birth.metadata.lock().unwrap(),
                )
                .unwrap();
        }
        let (runtime, retained) = birth.into_runtime_and_retention();
        global.network_runtime = Some(runtime);
        global.network_engine = Some(Arc::new(Mutex::new(engine)));
        let mut f = Fixture {
            global,
            config,
            thread,
            tid,
            original: Close::new().with_fd(5),
        };
        install_alias(&mut f, 9);
        Self {
            f,
            child,
            child_state,
            projection,
            _retained: retained,
        }
    }
    fn finish_child(&mut self) {
        let tool: Detcore = Detcore::new(self.f.tid, &self.f.config);
        tool.on_backend_thread_terminal(
            Tid::from_raw(self.child.owner().thread.as_raw()),
            &self.f.global,
            &mut self.child_state,
            reverie::ExitStatus::Exited(0),
        );
        assert!(!self.f.global.sched.lock().unwrap().backend_failed());
        self.f.global.recv_network_owner_gone(self.child.owner());
        let mut scheduler = self.f.global.sched.lock().unwrap();
        scheduler.logically_kill_thread(
            &self.child.owner().thread,
            &self.f.thread.detpid.unwrap(),
            self.child.owner().mm,
        );
        scheduler.controlled_drain_shared_terminal_removals();
    }
}

#[tokio::test]
async fn foreground_close_live_child_alias_and_parent_are_known_generic() {
    for child_selected in [false, true] {
        let shared = SharedFixture::new().await;
        let f = &shared.f;
        let mut g = guest(f, ReadReply::Original);
        if child_selected {
            f.global
                .sched
                .lock()
                .unwrap()
                .controlled_shared_child_grant(&shared.child);
            g.thread = shared.child_state.clone();
            g.tid = Tid::from_raw(shared.child.owner().thread.as_raw());
        }
        let tool: Detcore = Detcore::new(g.tid, &f.config);
        let time = f.global.global_time.lock().unwrap().as_nanos();
        let history = f.global.sched.lock().unwrap().thread_tree.size();
        assert_eq!(
            tool.controlled_foreground_close(&mut g, Close::new().with_fd(9))
                .await
                .unwrap(),
            None
        );
        assert!(g.thread.original_connect.is_none());
        let read = g.admitted.lock().unwrap().clone().unwrap();
        assert_eq!(read.fd, 9);
        assert_eq!(g.requests.lock().unwrap().iter().filter(|r| matches!(r, GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) if admission == &read)).count(), 1);
        assert!(matches!(
            g.responses.lock().unwrap().last(),
            Some(GlobalResponse::Network(Ok(NetworkReply::Unit)))
        ));
        assert_eq!(f.global.global_time.lock().unwrap().as_nanos(), time);
        assert_eq!(f.global.sched.lock().unwrap().thread_tree.size(), history);
    }
}

#[tokio::test]
async fn foreground_close_after_final_wait_callback_and_cleanup_preserves_history() {
    let mut shared = SharedFixture::new().await;
    let history = shared.f.global.sched.lock().unwrap().thread_tree.size();
    shared.finish_child();
    let f = &shared.f;
    let parent = f
        .global
        .network_runtime
        .as_ref()
        .unwrap()
        .foreground_root(owner(f))
        .unwrap();
    assert!(!parent.is_sole_initial_root(owner(f)));
    assert!(shared.projection.completed_final_wait(&parent).is_some());
    assert_eq!(f.global.sched.lock().unwrap().thread_tree.size(), history);
    let time = arm_alarm(f);
    let prepared = begin_at_barrier(f).await;
    f.global
        .validate_foreground_close_callback(
            f.tid,
            &f.thread,
            prepared.origin.admission(),
            [5, 0, 0, 0, 0, 0],
        )
        .unwrap();
    check_normal_clock(f, time);
    assert_eq!(f.global.sched.lock().unwrap().thread_tree.size(), history);
}

#[tokio::test]
async fn foreground_close_option_attempt_precedes_fault_and_never_rearms_aliases() {
    let mut f = born_fixture();
    install_alias(&mut f, 9);
    let tool: Detcore = Detcore::new(f.tid, &f.config);
    let mut g = guest(&f, ReadReply::Original);
    let unsafe_option = reverie::syscalls::Setsockopt::new()
        .with_fd(9)
        .with_level(libc::SOL_SOCKET)
        .with_optname(libc::SO_RCVBUF)
        .with_optval(None)
        .with_optlen(4);
    let error = tool
        .try_shadow_setsockopt(&mut g, unsafe_option)
        .await
        .unwrap_err();
    assert!(matches!(error, reverie::Error::Errno(Errno::EFAULT)));
    assert!(g.requests.lock().unwrap().iter().any(|r| matches!(r,
        GlobalRequest::Network(NetworkRequest::ObserveFiniteCloseOptionAttempt { level, option, .. })
            if *level == libc::SOL_SOCKET && *option == libc::SO_RCVBUF)));
    let safe_option = reverie::syscalls::Setsockopt::new()
        .with_fd(5)
        .with_level(libc::SOL_SOCKET)
        .with_optname(libc::SO_SNDTIMEO)
        .with_optval(None)
        .with_optlen(16);
    let error = tool
        .try_shadow_setsockopt(&mut g, safe_option)
        .await
        .unwrap_err();
    assert!(matches!(error, reverie::Error::Errno(Errno::EFAULT)));
    assert!(g.requests.lock().unwrap().iter().any(|r| matches!(r,
        GlobalRequest::Network(NetworkRequest::ObserveFiniteCloseOptionAttempt { level, option, .. })
            if *level == libc::SOL_SOCKET && *option == libc::SO_SNDTIMEO)));
    assert!(matches!(
        g.responses.lock().unwrap().last(),
        Some(GlobalResponse::Network(Ok(NetworkReply::Unit)))
    ));
    for fd in [5, 9] {
        let mut g = guest(&f, ReadReply::Original);
        assert_eq!(
            tool.controlled_foreground_close(&mut g, Close::new().with_fd(fd))
                .await
                .unwrap(),
            None
        );
        assert!(g.thread.original_connect.is_none());
        let read = g.admitted.lock().unwrap().clone().unwrap();
        assert_eq!(g.requests.lock().unwrap().iter().filter(|r| matches!(r,
            GlobalRequest::Network(NetworkRequest::FinishFdRead { admission }) if admission == &read)).count(), 1);
    }
}

#[tokio::test]
async fn foreground_close_submitted_consumption_keeps_unreturned_call_and_normal_clock() {
    let f = born_fixture();
    let time = arm_alarm(&f);
    let prepared = begin_at_barrier(&f).await;
    let admission = prepared.origin.admission().clone();
    {
        let mut engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
        // Explicit controlled provider READY is distinct from effect entry or
        // return; the real engine and Global submit consumers still validate it.
        engine
            .original_connect_provider_submitted(owner(&f), &admission)
            .unwrap();
        engine
            .original_call_prepared(owner(&f), &admission, None, 72)
            .unwrap();
    }
    assert!(
        f.global
            .submit_foreground_original_close(owner(&f), &admission)
            .unwrap()
    );
    f.global
        .validate_foreground_close_callback(f.tid, &f.thread, &admission, [5, 0, 0, 0, 0, 0])
        .unwrap();
    check_normal_clock(&f, time);
    let local = crate::network_replay::original_connect::Local {
        arguments: admission.arguments.clone(),
        raw_arguments: [5, 0, 0, 0, 0, 0],
        admission: Some(admission.clone()),
        invoked: true,
        returned: None,
    };
    f.global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .original_connect_consumed(owner(&f), &local)
        .unwrap();
    assert!(
        f.global
            .validate_foreground_close_callback(f.tid, &f.thread, &admission, local.raw_arguments)
            .is_err()
    );
    let engine = f.global.network_engine.as_ref().unwrap().lock().unwrap();
    assert_eq!(
        engine
            .original_connect_cancellation(owner(&f), &admission)
            .unwrap(),
        (false, true)
    );
    assert_eq!(
        engine
            .original_connect_result(owner(&f), &admission)
            .unwrap(),
        None
    );
    assert!(
        engine
            .foreground_close_origin(owner(&f), &admission)
            .unwrap()
            .is_some()
    );
}
