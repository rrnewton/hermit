//! Join a narrow local probe to the current original owner and real backend stops.
use std::sync::Arc;

use reverie::GlobalTool;
use reverie::InjectedSyscallEvent;
use reverie::Tid;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::Sysno;

use super::GlobalState;
use crate::network_replay::NetworkFdReadAdmission;
use crate::network_replay::NetworkStreamOwner;
use crate::network_runtime::guard::NetworkGuardProbeKind;
use crate::network_runtime::guard_probe::Invocation;
use crate::network_runtime::guard_probe::Local;
use crate::tool_local::ThreadState;
use crate::types::DetPid;

impl GlobalState {
    pub(crate) fn retain_local_pair_probe<T>(
        &self,
        state: &ThreadState<T>,
        read: &NetworkFdReadAdmission,
        nr: Sysno,
        args: SyscallArgs,
        kind: NetworkGuardProbeKind,
    ) -> Result<Option<Arc<Local>>, reverie::Error> {
        self.validate_local_pair_poll(state, read)?;
        if state.local_guard_probe.is_some()
            || !matches!(
                (kind, nr),
                (NetworkGuardProbeKind::Poll, Sysno::poll)
                    | (NetworkGuardProbeKind::Ppoll, Sysno::ppoll)
            )
            || args.arg0 == 0
            || args.arg1 != 1
            || (kind == NetworkGuardProbeKind::Poll && args.arg2 != 0)
            || (kind == NetworkGuardProbeKind::Ppoll && args.arg2 == 0)
            || (kind == NetworkGuardProbeKind::Ppoll
                && (args.arg3 != 0 || args.arg4 != 0 || args.arg5 != 0))
        {
            return Err(reverie::Error::Tool(anyhow::anyhow!(
                "local Unix probe changed finite shape/custody"
            )));
        }
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        let runtime = self.network_runtime.as_ref().ok_or_else(|| {
            reverie::Error::Tool(anyhow::anyhow!("local Unix probe runtime absent"))
        })?;
        let root = runtime.foreground_root(owner)?;
        if !root.matches_memory(&state.memory_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
        {
            return Err(reverie::Error::Tool(anyhow::anyhow!(
                "local Unix probe changed current MM"
            )));
        }
        Ok(runtime.retain_guard_probe(
            owner,
            root,
            read.clone(),
            Invocation {
                nr,
                args,
                kind,
                syscall_count: state.stats.syscall_count,
            },
        )?)
    }

    /// Recheck the actual retained grant without holding any scheduler/table
    /// lock across independent keeper control I/O.
    pub(crate) fn validate_retained_local_probe<T>(
        &self,
        state: &ThreadState<T>,
        local: &Local,
    ) -> Result<(), reverie::Error> {
        let owner = NetworkStreamOwner {
            thread: state.dettid,
            mm: state.mm_id,
        };
        if owner != local.owner
            || state.stats.syscall_count != local.syscall_count
            || !local.root.matches_memory(&state.memory_metadata)
            || self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm)
            || !self.network_runtime.as_ref().is_some_and(|runtime| {
                runtime
                    .foreground_root(owner)
                    .is_ok_and(|root| Arc::ptr_eq(&root, &local.root))
            })
        {
            return Err(reverie::Error::Tool(anyhow::anyhow!(
                "local Unix probe changed retained owner/MM/root"
            )));
        }
        self.validate_local_pair_poll(state, &local.read)
    }

    pub(crate) fn observe_local_guard_probe<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &ThreadState<T>,
        nr: Sysno,
        args: SyscallArgs,
        event: InjectedSyscallEvent,
    ) {
        let Some(local) = &state.local_guard_probe else {
            return;
        };
        let result = (|| {
            if tid.as_raw() != local.root.thread() || process.as_raw() != local.root.process() {
                return Err(reverie::Error::Tool(anyhow::anyhow!(
                    "local Unix probe changed backend task"
                )));
            }
            self.validate_retained_local_probe(state, local)?;
            Ok::<_, reverie::Error>(local.observe(nr, args, event)?)
        })();
        if let Err(error) = result {
            local.reject(&error.to_string());
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase: "local Unix probe failed actual backend boundary authentication",
            });
        }
    }

    pub(crate) fn observe_local_guard_probe_terminal<T>(
        &self,
        tid: Tid,
        process: DetPid,
        state: &ThreadState<T>,
    ) {
        let Some(local) = &state.local_guard_probe else {
            return;
        };
        if tid.as_raw() != local.root.thread()
            || process.as_raw() != local.root.process()
            || local.owner
                != (NetworkStreamOwner {
                    thread: state.dettid,
                    mm: state.mm_id,
                })
        {
            self.report_backend_failure(reverie::BackendFailure {
                pid: Tid::from_raw(process.as_raw()),
                tid,
                phase: "local Unix probe terminal changed actual owner",
            });
            return;
        }
        local.terminal();
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::fd::BorrowedFd;
    use std::time::Duration;
    use std::time::Instant;

    use detcore_model::network_trace::NetworkPolicy;
    use detcore_model::network_trace::NetworkTrace;
    use reverie::Tool;

    use super::*;
    use crate::Detcore;
    use crate::config::Config;
    use crate::network_replay::NetworkFdReadBegin;
    use crate::network_replay::NetworkReplayEngine;
    use crate::network_runtime::guard::NetworkGuardControl;
    use crate::network_runtime::guard::NetworkGuardControllerAbort;
    use crate::network_runtime::guard::NetworkGuardOutcome;
    use crate::network_runtime::guard::NetworkGuardProbeCompletion;
    use crate::network_runtime::guard::NetworkGuardProbeId;

    #[derive(Debug)]
    struct Control {
        observations: std::sync::atomic::AtomicU64,
    }
    impl NetworkGuardControl for Control {
        unsafe fn register_stopped_initial(&self, _: BorrowedFd<'_>, _: Instant) -> io::Result<()> {
            Ok(())
        }
        fn observation(&self) -> NetworkGuardOutcome {
            NetworkGuardOutcome::Running
        }
        fn start_observer(&self, _: NetworkGuardControllerAbort) -> io::Result<()> {
            Ok(())
        }
        unsafe fn arm_stopped_probe(
            &self,
            kind: NetworkGuardProbeKind,
            _: Instant,
        ) -> io::Result<NetworkGuardProbeId> {
            Ok(NetworkGuardProbeId {
                incarnation: 23,
                initial_sequence: 3,
                sequence: 5,
                kind,
            })
        }
        unsafe fn submit_entered_probe(
            &self,
            _: NetworkGuardProbeId,
            _: Instant,
        ) -> io::Result<()> {
            Ok(())
        }
        unsafe fn complete_returned_probe(
            &self,
            probe: NetworkGuardProbeId,
            raw: i64,
            _: Instant,
        ) -> io::Result<NetworkGuardProbeCompletion> {
            Ok(NetworkGuardProbeCompletion {
                probe,
                raw,
                observations: self.observations.load(std::sync::atomic::Ordering::SeqCst),
            })
        }
        fn retire_completed_probe(
            &self,
            _: NetworkGuardProbeCompletion,
            _: Instant,
        ) -> io::Result<()> {
            Ok(())
        }
    }
    struct Fixture {
        state: GlobalState,
        tool: Detcore,
        thread: ThreadState<()>,
        read: NetworkFdReadAdmission,
        args: SyscallArgs,
        nr: Sysno,
        tid: Tid,
        control: Arc<Control>,
    }
    impl Fixture {
        fn new(replay: bool) -> Self {
            Self::construct(replay, 7, true)
        }
        fn with_fd(replay: bool, fd: i32) -> Self {
            Self::construct(replay, fd, replay)
        }
        fn construct(replay: bool, fd: i32, guarded: bool) -> Self {
            // Root census, selected Normal grant, installation effect and native
            // keeper replies are explicit component premises. PIDFD comparisons,
            // reader publication and the actual Tool callbacks execute here.
            // This is not a native BPF or ptrace qualification.
            let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            let tid = Tid::from_raw(raw);
            let (runtime, root, metadata, memory, claim) =
                crate::network_runtime::controlled_foreground_runtime(raw);
            let owner = root.owner();
            let control = Arc::new(Control {
                observations: std::sync::atomic::AtomicU64::new(1),
            });
            if guarded {
                unsafe {
                    runtime
                        .attach_authenticated_guard(
                            control.clone(),
                            Instant::now() + Duration::from_secs(1),
                        )
                        .unwrap();
                }
                runtime.register_guard_initial(owner).unwrap();
            }
            let mut cfg = Config {
                sequentialize_threads: true,
                epoch_explicit: true,
                ..Config::default()
            };
            let mut engine = NetworkReplayEngine::record_native_receive(cfg.epoch);
            cfg.network_trace.policy = if replay {
                NetworkPolicy::Replay
            } else {
                NetworkPolicy::Record
            };
            if replay {
                let trace = engine.native_trace_fixture();
                let mut bytes = Vec::new();
                NetworkTrace::V4(trace.clone())
                    .write_framed(&mut bytes)
                    .unwrap();
                cfg.network_trace_input = Some(bytes);
                engine = NetworkReplayEngine::replay_native_receive(trace).unwrap();
            }
            let mut state = GlobalState::initialize(&cfg, false);
            state.network_runtime = Some(runtime);
            engine.fd_table_fixture_enable();
            engine
                .register_initial_census(root.association(), &claim, owner.thread)
                .unwrap();
            *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
            state
                .registered_exec_mms
                .lock()
                .unwrap()
                .insert(owner.thread, owner.mm);
            state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(&root);
            let tool: Detcore = Detcore::new(tid, &cfg);
            let mut thread = tool.init_thread_state(tid, None);
            thread.dettid = owner.thread;
            thread.detpid = Some(owner.thread);
            thread.mm_id = owner.mm;
            thread.file_metadata = metadata.clone();
            thread.memory_metadata = memory;
            tool.on_thread_state_ready(tid, &state, &thread).unwrap();
            // First bind the actual initial census, then publish the later
            // local-pair allocation. Reversing this order is correctly refused.
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            {
                let mut metadata = metadata.lock().unwrap();
                metadata
                    .add_local_socket_pair_fd(owner.thread, fd, nix::fcntl::OFlag::O_RDWR, None)
                    .unwrap();
                let replacement = *metadata.pending_network_installations().last().unwrap();
                let effect = engine.fd_publication_fixture_effect(owner, replacement);
                metadata
                    .associate_network_installation(replacement.installation_generation, effect)
                    .unwrap();
                let admission = engine.acquire_fd_publication(owner, root.files()).unwrap();
                let batch = metadata.publication_snapshot(&admission).unwrap();
                engine
                    .publish_fd_publication(owner, admission.permit, &batch)
                    .unwrap();
                metadata.publication_acknowledge(&batch).unwrap();
                engine
                    .acknowledge_fd_publication(owner, admission.permit, &batch)
                    .unwrap();
                metadata.publication_server_acknowledge(&batch).unwrap();
            }
            let NetworkFdReadBegin::Admitted(read) =
                engine.begin_fd_read(owner, root.files(), fd).unwrap()
            else {
                panic!("reader admitted")
            };
            drop(engine);
            Self {
                state,
                tool,
                thread,
                read: *read,
                args: SyscallArgs::new(0x23400, 1, 0, 0, 0, 0),
                nr: Sysno::poll,
                tid,
                control,
            }
        }
        fn stage(&mut self) -> Arc<Local> {
            let local = self
                .state
                .retain_local_pair_probe(
                    &self.thread,
                    &self.read,
                    Sysno::poll,
                    self.args,
                    NetworkGuardProbeKind::Poll,
                )
                .unwrap()
                .unwrap();
            self.thread.local_guard_probe = Some(local.clone());
            local.arm().unwrap();
            local
        }
        fn event(&mut self, event: InjectedSyscallEvent) {
            self.tool.on_injected_syscall_observed(
                self.tid,
                &self.state,
                &mut self.thread,
                self.nr,
                self.args,
                event,
            );
        }
        fn time_and_turn(&self) -> (crate::types::LogicalTime, String) {
            (
                self.state.global_time.lock().unwrap().as_nanos(),
                format!("{:?}", self.state.sched.lock().unwrap().next_turns),
            )
        }
    }
    #[tokio::test]
    async fn local_guard_probe_actual_tool_callbacks_preserve_turn_in_record_and_replay() {
        for replay in [false, true] {
            let mut f = Fixture::new(replay);
            let before = f.time_and_turn();
            let local = f.stage();
            assert!(local.returned().is_err());
            for event in [
                InjectedSyscallEvent::Prepared,
                InjectedSyscallEvent::Entered,
                InjectedSyscallEvent::Returned(0),
            ] {
                f.event(event);
            }
            assert_eq!(local.returned().unwrap(), Some(0));
            f.state
                .validate_retained_local_probe(&f.thread, &local)
                .unwrap();
            local.retire().unwrap();
            assert!(local.is_retired());
            assert!(!f.state.sched.lock().unwrap().backend_failed());
            assert_eq!(f.time_and_turn(), before);
        }
    }
    #[tokio::test]
    async fn local_guard_probe_actual_join_refuses_changed_task_mm_root_and_reader() {
        for mutation in 0..8 {
            let mut f = Fixture::new(true);
            let local = f.stage();
            match mutation {
                0 => f.tid = Tid::from_raw(f.tid.as_raw() + 1),
                1 => f.thread.mm_id = f.thread.mm_id.for_exec(f.thread.dettid),
                2 => f.state.registered_exec_mms.lock().unwrap().clear(),
                3 => f
                    .state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .revoke_foreground_lineage(),
                4 => f.args.arg0 += 8,
                5 => {
                    let metadata = f.thread.file_metadata.lock().unwrap().clone();
                    f.thread.file_metadata = Arc::new(std::sync::Mutex::new(metadata));
                }
                6 => {
                    f.state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .finish_fd_read(local.owner, f.read.clone())
                        .unwrap();
                }
                7 => f.thread.stats.syscall_count += 1,
                _ => unreachable!(),
            }
            f.event(InjectedSyscallEvent::Prepared);
            assert!(
                f.state.sched.lock().unwrap().backend_failed(),
                "mutation {mutation}"
            );
            assert!(local.returned().is_err());
            assert!(!local.is_retired());
        }
    }

    struct Scratch {
        bytes: Box<[u128; 32]>,
        size: usize,
    }
    struct ScratchGuard {
        _bytes: Box<[u128; 32]>,
    }
    impl Drop for ScratchGuard {
        fn drop(&mut self) {}
    }
    impl reverie::Stack for Scratch {
        type StackGuard = ScratchGuard;
        fn size(&self) -> usize {
            self.size
        }
        fn capacity(&self) -> usize {
            std::mem::size_of_val(self.bytes.as_ref())
        }
        fn push<'s, V>(&mut self, value: V) -> reverie::syscalls::Addr<'s, V> {
            let address = self.reserve::<V>().as_raw();
            unsafe {
                std::ptr::write(address as *mut V, value);
            }
            reverie::syscalls::Addr::from_raw(address).unwrap()
        }
        fn reserve<'s, V>(&mut self) -> reverie::syscalls::AddrMut<'s, V> {
            self.size = self.size.next_multiple_of(std::mem::align_of::<V>());
            let start = self.size;
            self.size += std::mem::size_of::<V>();
            assert!(self.size <= self.capacity());
            reverie::syscalls::AddrMut::from_raw(self.bytes.as_ptr() as usize + start).unwrap()
        }
        fn commit(self) -> Result<ScratchGuard, reverie::Errno> {
            Ok(ScratchGuard { _bytes: self.bytes })
        }
    }
    struct ProbeGuest {
        fixture: Fixture,
        requests: std::sync::Mutex<Vec<String>>,
        output: usize,
        original: i16,
        scratch: bool,
        native_raw: Option<i64>,
        injections: usize,
    }
    #[reverie::tool]
    impl reverie::GlobalRPC<GlobalState> for ProbeGuest {
        async fn send_rpc(
            &self,
            request: <GlobalState as GlobalTool>::Request,
        ) -> <GlobalState as GlobalTool>::Response {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{:?}", request.2));
            self.fixture
                .state
                .receive_rpc(self.fixture.tid, request)
                .await
        }
        fn config(&self) -> &Config {
            &self.fixture.state.cfg
        }
    }
    #[reverie::tool]
    impl reverie::Guest<Detcore> for ProbeGuest {
        type Memory = reverie::syscalls::LocalMemory;
        type Stack = Scratch;
        fn tid(&self) -> Tid {
            self.fixture.tid
        }
        fn pid(&self) -> Tid {
            self.fixture.tid
        }
        fn ppid(&self) -> Option<Tid> {
            None
        }
        fn local_global_state(&self) -> Option<&GlobalState> {
            Some(&self.fixture.state)
        }
        fn memory(&self) -> Self::Memory {
            reverie::syscalls::LocalMemory::new()
        }
        fn thread_state(&self) -> &ThreadState<()> {
            &self.fixture.thread
        }
        fn thread_state_mut(&mut self) -> &mut ThreadState<()> {
            &mut self.fixture.thread
        }
        async fn regs(&mut self) -> libc::user_regs_struct {
            panic!("unexpected registers")
        }
        async fn stack(&mut self) -> Scratch {
            Scratch {
                bytes: Box::new([0; 32]),
                size: 0,
            }
        }
        async fn daemonize(&mut self) {
            panic!("unexpected daemon")
        }
        async fn inject<S: reverie::syscalls::SyscallInfo>(
            &mut self,
            call: S,
        ) -> Result<i64, reverie::Errno> {
            let (nr, args) = call.into_parts();
            assert_eq!(
                nr,
                if self.scratch {
                    Sysno::ppoll
                } else {
                    Sysno::poll
                }
            );
            if self.scratch {
                assert_ne!(
                    args.arg0, self.output,
                    "shadow probe writes only private scratch"
                );
            } else {
                assert_eq!(
                    args.arg0, self.output,
                    "ordinary Poll preserves original kernel copyout"
                );
            }
            self.fixture.args = args;
            self.fixture.nr = nr;
            self.fixture.event(InjectedSyscallEvent::Prepared);
            self.fixture.event(InjectedSyscallEvent::Entered);
            assert_eq!(
                unsafe { std::ptr::read_unaligned((self.output + 6) as *const i16) },
                self.original
            );
            self.injections += 1;
            let raw = unsafe {
                libc::syscall(
                    nr as libc::c_long,
                    args.arg0,
                    args.arg1,
                    args.arg2,
                    args.arg3,
                    args.arg4,
                    args.arg5,
                )
            };
            let result = if raw < 0 {
                Err(reverie::Errno::last())
            } else {
                Ok(raw)
            };
            let raw = result.unwrap_or_else(|error| -i64::from(error.into_raw()));
            self.native_raw = Some(raw);
            self.fixture.event(InjectedSyscallEvent::Returned(raw));
            if self.scratch {
                assert_eq!(
                    unsafe { std::ptr::read_unaligned((self.output + 6) as *const i16) },
                    self.original
                );
            }
            result
        }
        async fn tail_inject<S: reverie::syscalls::SyscallInfo>(&mut self, _: S) -> reverie::Never {
            panic!("unexpected tail")
        }
        fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("unexpected timer")
        }
        fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
            panic!("unexpected timer")
        }
        fn read_clock(&mut self) -> Result<u64, reverie::Error> {
            panic!("probe must not read host clock")
        }
    }
    #[tokio::test]
    async fn local_guard_probe_production_helper_native_poll_and_pre_exposure_gate() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        for replay in [false, true] {
            for scratch in [false, true] {
                for (ready, observations) in [(false, 1), (true, 1), (true, 0), (true, 2)] {
                    if !replay && observations != 1 {
                        continue;
                    }
                    let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
                    if ready {
                        writer.write_all(b"x").unwrap();
                    }
                    let fixture = Fixture::with_fd(replay, reader.as_raw_fd());
                    fixture
                        .control
                        .observations
                        .store(observations, std::sync::atomic::Ordering::SeqCst);
                    fixture
                        .state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .finish_fd_read(fixture.read.publication.permit.owner, fixture.read.clone())
                        .unwrap();
                    let mut row = libc::pollfd {
                        fd: reader.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0x5555,
                    };
                    let output = std::ptr::from_mut(&mut row) as usize;
                    let tool: Detcore = Detcore::new(fixture.tid, &fixture.state.cfg);
                    let mut guest = ProbeGuest {
                        fixture,
                        requests: std::sync::Mutex::default(),
                        output,
                        original: row.revents,
                        scratch,
                        native_raw: None,
                        injections: 0,
                    };
                    let before = guest.fixture.time_and_turn();
                    let result = if scratch {
                        tool.controlled_shadow_local_pair_probe(&mut guest, row)
                            .await
                            .map(|observed| {
                                (Some(i64::from(observed.revents != 0)), observed.revents)
                            })
                    } else {
                        tool.try_local_pair_poll(
                            &mut guest,
                            reverie::syscalls::Poll::new()
                                .with_fds(reverie::syscalls::AddrMut::from_raw(output))
                                .with_nfds(1)
                                .with_timeout(0),
                        )
                        .await
                        .map(|raw| (raw, row.revents))
                    };
                    assert_eq!(guest.injections, 1);
                    if observations == 1 {
                        assert_eq!(
                            result.unwrap(),
                            (Some(i64::from(ready)), if ready { libc::POLLIN } else { 0 })
                        );
                        assert_eq!(
                            row.revents,
                            if scratch {
                                0x5555
                            } else if ready {
                                libc::POLLIN
                            } else {
                                0
                            }
                        );
                        assert!(guest.fixture.thread.local_guard_probe.is_none());
                        assert!(
                            guest
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|request| request.contains("FinishFdRead"))
                        );
                    } else {
                        assert!(result.is_err());
                        // Native original Poll already writes its own result;
                        // shadow Ppoll must never expose unverified scratch.
                        assert_eq!(row.revents, if scratch { 0x5555 } else { libc::POLLIN });
                        assert!(guest.fixture.thread.local_guard_probe.is_some());
                        assert!(
                            !guest
                                .requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|request| request.contains("FinishFdRead"))
                        );
                    }
                    assert_eq!(guest.native_raw, Some(i64::from(ready)));
                    assert_eq!(row.fd, reader.as_raw_fd());
                    assert_eq!(row.events, libc::POLLIN);
                    assert_eq!(guest.fixture.time_and_turn(), before);
                }
            }
        }
    }

    struct Pages {
        address: usize,
        length: usize,
    }
    impl Drop for Pages {
        fn drop(&mut self) {
            assert_eq!(
                unsafe { libc::munmap(self.address as *mut _, self.length) },
                0
            );
        }
    }
    #[tokio::test]
    async fn local_guard_probe_original_copyout_keeps_native_readonly_and_cross_page_faults() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        for replay in [false, true] {
            for crossing in [false, true] {
                let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
                let allocation = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        page * 2,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                        -1,
                        0,
                    )
                };
                assert_ne!(allocation, libc::MAP_FAILED);
                let pages = Pages {
                    address: allocation as usize,
                    length: page * 2,
                };
                let output = pages.address + if crossing { page - 7 } else { 64 };
                let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
                writer.write_all(b"x").unwrap();
                let initial = libc::pollfd {
                    fd: reader.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0x5555,
                };
                unsafe {
                    std::ptr::write_unaligned(output as *mut libc::pollfd, initial);
                }
                let protected = if crossing {
                    (pages.address + page) as *mut libc::c_void
                } else {
                    allocation
                };
                assert_eq!(
                    unsafe { libc::mprotect(protected, page, libc::PROT_READ) },
                    0
                );
                // Ground the actual errno/atomic short output against Linux,
                // including a two-byte revents store crossing into an RO page.
                assert_eq!(unsafe { libc::poll(output as *mut libc::pollfd, 1, 0) }, -1);
                assert_eq!(reverie::Errno::last(), reverie::Errno::EFAULT);
                assert_eq!(
                    unsafe { std::ptr::read_unaligned((output + 6) as *const i16) },
                    initial.revents
                );
                let fixture = Fixture::with_fd(replay, reader.as_raw_fd());
                fixture
                    .state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .finish_fd_read(fixture.read.publication.permit.owner, fixture.read.clone())
                    .unwrap();
                let tool: Detcore = Detcore::new(fixture.tid, &fixture.state.cfg);
                let mut guest = ProbeGuest {
                    fixture,
                    requests: std::sync::Mutex::default(),
                    output,
                    original: initial.revents,
                    scratch: false,
                    native_raw: None,
                    injections: 0,
                };
                let result = tool
                    .try_local_pair_poll(
                        &mut guest,
                        reverie::syscalls::Poll::new()
                            .with_fds(reverie::syscalls::AddrMut::from_raw(output))
                            .with_nfds(1)
                            .with_timeout(0),
                    )
                    .await;
                assert_eq!(guest.injections, 1);
                assert_eq!(guest.native_raw, Some(-i64::from(libc::EFAULT)));
                if replay {
                    // Finite guard contract deliberately retains unsupported
                    // native faults. It does not invent/forward a successful
                    // EFAULT result or retire a potentially partial copyout.
                    assert!(result.is_err());
                    assert!(!matches!(result, Err(reverie::Error::Errno(_))));
                    assert!(guest.fixture.thread.local_guard_probe.is_some());
                    assert!(
                        !guest
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|request| request.contains("FinishFdRead"))
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(reverie::Error::Errno(reverie::Errno::EFAULT))
                    ));
                    assert!(guest.fixture.thread.local_guard_probe.is_none());
                    assert!(
                        guest
                            .requests
                            .lock()
                            .unwrap()
                            .iter()
                            .any(|request| request.contains("FinishFdRead"))
                    );
                }
                let observed = unsafe { std::ptr::read_unaligned(output as *const libc::pollfd) };
                assert_eq!(
                    (observed.fd, observed.events, observed.revents),
                    (initial.fd, initial.events, initial.revents)
                );
            }
        }
    }
}
