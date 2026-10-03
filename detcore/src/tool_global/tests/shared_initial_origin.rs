//! Actual existing terminal callbacks and cleanup; controlled provider/birth
//! fixture and prior Connect-origin premise. No native network qualification.
use reverie::Tool;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

struct Fixture {
    state: GlobalState,
    parent: Arc<ForegroundRoot>,
    child: Arc<ForegroundRoot>,
    initial: Arc<NativeTaskProjection>,
    child_projection: Arc<NativeTaskProjection>,
    parent_state: crate::ThreadState<()>,
    child_state: crate::ThreadState<()>,
    _retained: Box<dyn std::any::Any>,
}
impl Fixture {
    async fn new() -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let mut cfg = Config {
            epoch_explicit: true,
            epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
            ..Config::default()
        };
        cfg.network_trace.policy = NetworkPolicy::Record;
        let mut state = GlobalState::initialize(&cfg, false);
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record_shared_mm_attempts(cfg.epoch);
        let mut initial = None;
        let birth = ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, _| {
            let mut scheduler = state.sched.lock().unwrap();
            scheduler.controlled_foreground_store_grant(root);
            let projection = scheduler.shared_initial_projection(root).unwrap();
            state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .controlled_shared_initial_origin(root.clone(), projection.clone());
            initial = Some(projection);
        })
        .await;
        state.sched.lock().unwrap().controlled_shared_birth_census(
            &birth.parent,
            &birth.child,
            &birth._birth,
        );
        let parent = birth.parent.clone();
        let child = birth.child.clone();
        let child_projection = state
            .sched
            .lock()
            .unwrap()
            .shared_terminal_projection(child.owner(), parent.logical_process())
            .unwrap()
            .unwrap();
        let tool: Detcore = Detcore::new(Tid::from_raw(raw), &cfg);
        let make = |root: &Arc<ForegroundRoot>| {
            let mut thread =
                tool.init_thread_state(Tid::from_raw(root.owner().thread.as_raw()), None);
            thread.dettid = root.owner().thread;
            thread.mm_id = root.owner().mm;
            thread.detpid = Some(parent.logical_process());
            thread.thread_start_entered = true;
            thread.file_metadata = birth.metadata.clone();
            thread.memory_metadata = birth.memory.clone();
            thread
        };
        let parent_state = make(&parent);
        let child_state = make(&child);
        let (runtime, retained) = birth.into_runtime_and_retention();
        state.network_runtime = Some(runtime);
        Self {
            state,
            parent,
            child,
            initial: initial.unwrap(),
            child_projection,
            parent_state,
            child_state,
            _retained: retained,
        }
    }
    fn cleanup(&self, root: &Arc<ForegroundRoot>) {
        self.state.recv_network_owner_gone(root.owner());
        self.state.sched.lock().unwrap().logically_kill_thread(
            &root.owner().thread,
            &self.parent.logical_process(),
            root.owner().mm,
        );
        self.state
            .sched
            .lock()
            .unwrap()
            .controlled_drain_shared_terminal_removals();
    }
    fn settle(&self, root: &Arc<ForegroundRoot>, thread: &crate::ThreadState<()>) {
        self.state.settle_no_seq_terminal(
            Tid::from_raw(root.owner().thread.as_raw()),
            self.parent.logical_process(),
            thread,
        );
    }
    fn observers(&self, root: &Arc<ForegroundRoot>, thread: &crate::ThreadState<()>) {
        let tid = Tid::from_raw(root.owner().thread.as_raw());
        self.state
            .observe_original_connect_terminal(tid, self.parent.logical_process(), thread);
        self.state
            .observe_native_stream_terminal(tid, self.parent.logical_process(), thread);
    }
    fn finish_child(&self) {
        self.settle(&self.child, &self.child_state);
        self.observers(&self.child, &self.child_state);
        self.cleanup(&self.child);
        assert!(
            self.child_projection
                .completed_final_wait(&self.parent)
                .is_some()
        );
    }
}

#[tokio::test]
async fn shared_initial_final_wait_requires_both_boundaries_and_preserves_history() {
    for cleanup_first in [false, true] {
        let f = Fixture::new().await;
        f.finish_child();
        let history = f.state.sched.lock().unwrap().thread_tree.size();
        let time = f.state.global_time.lock().unwrap().as_nanos();
        if cleanup_first {
            f.cleanup(&f.parent);
        }
        assert!(!f.initial.completed_initial_final_wait(&f.parent));
        f.settle(&f.parent, &f.parent_state);
        assert!(
            !f.initial.completed_initial_final_wait(&f.parent),
            "settle cannot invent observers"
        );
        f.observers(&f.parent, &f.parent_state);
        assert!(!f.state.sched.lock().unwrap().backend_failed());
        assert!(f.initial.completed_initial_final_wait(&f.parent));
        assert!(
            f.child_projection.completed_final_wait(&f.parent).is_none(),
            "dead parent cannot authorize another child operation"
        );
        assert!(
            f.child_projection
                .completed_child_for_initial_finalization(&f.parent)
                .is_some()
        );
        if !cleanup_first {
            f.cleanup(&f.parent);
        }
        f.settle(&f.parent, &f.parent_state);
        f.observers(&f.parent, &f.parent_state);
        assert!(!f.state.sched.lock().unwrap().backend_failed());
        assert_eq!(f.state.sched.lock().unwrap().thread_tree.size(), history);
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), time);
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .controlled_shared_initial_finalization()
                .is_ok()
        );
    }
}

#[tokio::test]
async fn shared_initial_final_wait_refuses_live_or_unobserved_child_and_changed_history() {
    for fault in 0..3 {
        let f = Fixture::new().await;
        match fault {
            0 => {} // actual child still live
            1 => {
                f.settle(&f.child, &f.child_state);
                f.cleanup(&f.child);
            }
            2 => {
                f.finish_child();
                // Explicit corrupted-history premise must not become absence proof.
                f.state
                    .sched
                    .lock()
                    .unwrap()
                    .controlled_remove_shared_initial_history(f.child.owner().thread);
            }
            _ => unreachable!(),
        }
        f.settle(&f.parent, &f.parent_state);
        assert!(f.state.sched.lock().unwrap().backend_failed());
        assert!(!f.initial.completed_initial_final_wait(&f.parent));
        assert!(
            f.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .controlled_shared_initial_finalization()
                .is_err()
        );
    }
}

// The existing native_connected fixture is copied here because this off-tree
// checkpoint may not edit or expose the legacy test module. Only its constructor
// selects the new release policy; selected old provider/Call/return/custody
// assertions remain literal. These controlled provider messages are not a
// native BPF qualification. The localhost socket effects are real when run.
mod connect_publication {
    use detcore_model::network_trace::NetworkConnectionResultV2;
    use detcore_model::network_trace::NetworkEndpointRoleV2;
    use detcore_model::network_trace::NetworkEstablishmentV4;
    use detcore_model::network_trace::NetworkInputKindV2;
    use detcore_model::network_trace::NetworkProgressV4;
    use detcore_model::network_trace::NetworkReleaseNodeKindV4;
    use detcore_model::network_trace::NetworkTraceV4;
    use detcore_model::network_trace::NetworkTransportV2;
    use reverie::InjectedSyscallEvent;
    use reverie::Tool;
    use reverie::syscalls::SyscallArgs;
    use reverie::syscalls::Sysno;

    use super::*;
    use crate::network_replay::original_connect::Admission;
    use crate::network_replay::original_connect::Arguments;
    use crate::network_replay::original_connect::Kind;
    use crate::network_replay::original_connect::Local;
    use crate::network_runtime::NativeCaptureRecovery;
    use crate::network_runtime::native_peer::native_connected_tests::ConnectCompletionChange;

    struct EntryFixture {
        config: Config,
        state: GlobalState,
        tool: Detcore,
        thread: crate::ThreadState<()>,
        root: Arc<crate::network_runtime::ForegroundRoot>,
        tid: Tid,
        client: OwnedFd,
        listener: std::net::TcpListener,
        address: Box<libc::sockaddr_in>,
        arguments: Arguments,
        read: crate::network_replay::NetworkFdReadAdmission,
    }
    impl EntryFixture {
        async fn new() -> Self {
            let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
            let tid = Tid::from_raw(raw);
            let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let socket =
                unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            assert!(socket >= 0, "{}", std::io::Error::last_os_error());
            let client = unsafe { OwnedFd::from_raw_fd(socket) };
            let address = Box::new(libc::sockaddr_in {
                sin_family: libc::AF_INET as u16,
                sin_port: port.to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes([127, 0, 0, 1]),
                },
                sin_zero: [0; 8],
            });
            let (runtime, root, metadata, memory, claim) =
                crate::network_runtime::controlled_foreground_runtime(raw);
            let owner = root.owner();
            let mut config = Config {
                sequentialize_threads: true,
                epoch_explicit: true,
                epoch: chrono::DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
                ..Config::default()
            };
            config.network_trace.policy = NetworkPolicy::Record;
            let mut state = GlobalState::initialize(&config, false);
            state.network_runtime = Some(runtime);
            let tool: Detcore = Detcore::new(tid, &config);
            let mut thread = tool.init_thread_state(tid, None);
            thread.dettid = owner.thread;
            thread.mm_id = owner.mm;
            thread.detpid = Some(owner.thread);
            thread.stats.syscall_count = 17;
            thread.file_metadata = metadata;
            thread.memory_metadata = memory;
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
            {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                *engine = NetworkReplayEngine::record_shared_mm_attempts(config.epoch);
                engine.fd_table_fixture_enable();
                engine
                    .register_initial_census(root.association(), &claim, owner.thread)
                    .unwrap();
            }
            tool.on_thread_state_ready(tid, &state, &thread).unwrap();
            state.global_time.lock().unwrap().update_global_time(
                owner.thread,
                thread.thread_logical_time.as_nanos(),
                thread.thread_logical_time.inherited_nanos(),
            );
            let mut guest = owned_read_guest(&config, &state, thread);
            guest
                .thread
                .add_fd(
                    socket,
                    nix::fcntl::OFlag::empty(),
                    crate::fd::FdType::Socket,
                    None,
                )
                .unwrap();
            let binding = guest.thread.descriptor_binding(socket).unwrap();
            {
                let mut metadata = guest.thread.file_metadata.lock().unwrap();
                let replacement = metadata.pending_network_installations()[0];
                let effect = state
                    .network_engine
                    .as_ref()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .fd_publication_fixture_effect(owner, replacement);
                metadata
                    .associate_network_installation(replacement.installation_generation, effect)
                    .unwrap();
                metadata
                .bind_native_installation(
                    binding,
                    crate::network_runtime::original_installation::FileIdentity::controlled_fixture(
                        3, 109,
                    ),
                )
                .unwrap();
            }
            tool.publish_network_fd_installations(&mut guest)
                .await
                .unwrap();
            {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine.controlled_connect_socket_premise(binding.open_file);
                engine
                    .ensure_channel(
                        binding.open_file,
                        NetworkChannelBinding {
                            transport: NetworkTransportV2::Tcp,
                            role: NetworkEndpointRoleV2::OutboundClient,
                            peer_address: Some(NetworkAddressV2::Inet4 {
                                address: [127, 0, 0, 1],
                                port,
                            }),
                            requested_local_constraint: None,
                            observed_local_address: None,
                            accepted_from: None,
                            selected_channel: None,
                        },
                    )
                    .unwrap();
            }
            let request = selected_external_request(&guest.thread, socket);
            let read = {
                let mut pending = std::pin::pin!(crate::tool_global::fd_read_resource_request(
                    &mut guest,
                    request.clone()
                ));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                let selected = state.sched.lock().unwrap().select_test_turn().unwrap();
                finish_external_component_grant(
                    &state,
                    selected,
                    owner,
                    request.fd_read.unwrap().operation,
                )
                .await;
                let ResourceReply::ReadGrant { read, .. } = pending.await else {
                    panic!("exact FD reader grant absent")
                };
                *read
            };
            assert_eq!(read.binding, Some(binding));
            let arguments = Arguments {
                kind: Kind::Connect,
                operation: request.fd_read.unwrap().operation,
                files: binding.slot.files,
                binding: Some(binding),
                fd: socket,
                address: (&*address as *const libc::sockaddr_in) as u64,
                length: std::mem::size_of::<libc::sockaddr_in>() as i32,
                original_count: 0,
            };
            let thread = guest.thread;
            Self {
                config,
                state,
                tool,
                thread,
                root,
                tid,
                client,
                listener,
                address,
                arguments,
                read,
            }
        }
    }

    struct Fixture {
        config: Config,
        state: GlobalState,
        tool: Detcore,
        thread: crate::ThreadState<()>,
        root: Arc<crate::network_runtime::ForegroundRoot>,
        admission: Admission,
        publication: NativeCaptureRecovery,
        tid: Tid,
        client: OwnedFd,
        _listener: std::net::TcpListener,
        address: Box<libc::sockaddr_in>,
        returned: Option<i64>,
    }
    impl Fixture {
        async fn new() -> Self {
            let EntryFixture {
                config,
                state,
                tool,
                thread,
                root,
                tid,
                client,
                listener,
                address,
                arguments,
                read,
                ..
            } = EntryFixture::new().await;
            let owner = root.owner();
            let socket = arguments.fd;
            let (admission, task) = state
                .begin_original_native_entry_from_read(owner, arguments.clone(), read)
                .await
                .unwrap();
            drop(task); // The controlled capture below owns its separate real pin.
            let publication = NativeCaptureRecovery::new(
                state.network_engine.as_ref().unwrap().clone(),
                state.network_stream_changed.clone(),
                |_| {},
            );
            let pin = state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_capture(
                    owner,
                    &admission,
                    client.try_clone().unwrap(),
                    publication.clone(),
                )
                .unwrap();
            {
                let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
                engine
                    .original_connect_provider_submitted(owner, &admission)
                    .unwrap();
                engine
                    .original_connect_prepared(owner, &admission, pin, 103)
                    .unwrap();
                engine.original_connect_invoked(owner, &admission).unwrap();
            }
            let mut thread = thread;
            thread.original_connect = Some(Local {
                arguments: arguments.clone(),
                raw_arguments: [
                    socket as usize,
                    arguments.address as usize,
                    arguments.length as usize,
                    0,
                    0,
                    0,
                ],
                admission: Some(admission.clone()),
                invoked: true,
                returned: None,
            });
            let mut result = Self {
                config,
                state,
                tool,
                thread,
                root,
                admission,
                publication,
                tid,
                client,
                _listener: listener,
                address,
                returned: None,
            };
            result.observe(InjectedSyscallEvent::Prepared, 0);
            let selected = result
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_selection(
                    owner,
                    &result.admission,
                    result.root.native_identity(),
                )
                .unwrap();
            result
                .state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .original_connect_selected(
                    owner,
                    &result.admission,
                    selected.command,
                    (
                        selected.provider,
                        selected.task,
                        selected.task_start,
                        selected.table,
                        selected.file,
                    ),
                )
                .unwrap();
            result
        }
        fn owner(&self) -> NetworkStreamOwner {
            self.root.owner()
        }
        fn trace(&self) -> NetworkTraceV4 {
            self.state
                .network_engine
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .native_trace_fixture()
        }
        fn snapshot(&self) -> (String, String, u64, LogicalTime) {
            (
                format!(
                    "{:?}",
                    self.state.network_engine.as_ref().unwrap().lock().unwrap()
                ),
                self.state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .controlled_connect_custody(),
                self.state.sched.lock().unwrap().turn,
                self.state.global_time.lock().unwrap().as_nanos(),
            )
        }
        fn assert_refused(&self) {
            let before = self.snapshot();
            assert!(
                self.state
                    .publish_foreground_native_connected(self.tid, &self.thread, &self.admission)
                    .is_err()
            );
            assert_eq!(
                self.snapshot(),
                before,
                "refusal mutated Call, engine, scheduler or time"
            );
        }
        fn connect(&mut self) {
            assert!(self.returned.is_none());
            let raw = unsafe {
                libc::connect(
                    self.client.as_raw_fd(),
                    (&*self.address as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            };
            assert_eq!(
                raw,
                0,
                "actual loopback connect: {}",
                std::io::Error::last_os_error()
            );
            self.returned = Some(i64::from(raw));
        }
        fn observe(&mut self, event: InjectedSyscallEvent, changed: u8) {
            let args = self.thread.original_connect.as_ref().unwrap().raw_arguments;
            self.state.observe_original_connect(
                Tid::from_raw(self.tid.as_raw() + i32::from(changed == 1)),
                self.owner().thread,
                &mut self.thread,
                Sysno::connect,
                SyscallArgs::new(
                    args[0],
                    args[1] + usize::from(changed == 2),
                    args[2],
                    args[3],
                    args[4],
                    args[5],
                ),
                event,
            );
        }
        fn returned(&mut self) {
            self.observe(InjectedSyscallEvent::Returned(self.returned.unwrap()), 0);
            assert!(!self.state.sched.lock().unwrap().backend_failed());
            assert_eq!(
                self.thread.original_connect.as_ref().unwrap().returned,
                Some(0)
            );
        }
        fn completed(&self, change: ConnectCompletionChange) -> std::io::Result<()> {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    (&*self.address as *const libc::sockaddr_in).cast::<u8>(),
                    std::mem::size_of::<libc::sockaddr_in>(),
                )
            };
            self.state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_complete(
                    self.owner(),
                    &self.admission,
                    bytes,
                    self.returned.unwrap(),
                    change,
                )
        }
        async fn foreground(&mut self) {
            let thread = std::mem::replace(
                &mut self.thread,
                self.tool.init_thread_state(self.tid, None),
            );
            let mut guest = owned_read_guest(&self.config, &self.state, thread);
            {
                let mut pending =
                    std::pin::pin!(self.tool.finish_original_openat_wait(
                        &mut guest,
                        self.admission.arguments.operation
                    ));
                assert!(futures::poll!(pending.as_mut()).is_pending());
                assert!(
                    self.state
                        .sched
                        .lock()
                        .unwrap()
                        .harvest_external_io_for_test()
                        .is_ok()
                );
                let selected = self.state.sched.lock().unwrap().select_test_turn().unwrap();
                crate::scheduler::finish_selected_turn(
                    self.state.sched.clone(),
                    self.state.global_time.clone(),
                    selected.0,
                    selected.1,
                    selected.2,
                )
                .await
                .unwrap();
                pending.await;
            }
            self.thread = guest.thread;
            self.state
                .sched
                .lock()
                .unwrap()
                .foreground_native_observation(self.owner(), &self.root)
                .unwrap();
        }
        fn retire_provider(&self) -> std::io::Result<()> {
            self.state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_retirement(self.owner(), &self.admission)
        }
        fn close_pin(&self) {
            self.state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_close(self.owner(), &self.admission)
                .unwrap();
            assert!(
                unsafe { libc::fcntl(self.client.as_raw_fd(), libc::F_GETFD) } >= 0,
                "runtime duplicate close must not close actual guest source"
            );
        }
        async fn ready() -> Self {
            let mut f = Self::new().await;
            f.connect();
            f.completed(ConnectCompletionChange::None).unwrap();
            f.returned();
            f.retire_provider().unwrap();
            f.close_pin();
            f.foreground().await;
            f
        }
        fn finish(&self) {
            self.state
                .network_runtime
                .as_ref()
                .unwrap()
                .retire_original_connect(self.owner(), &self.admission, &self.publication)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn shared_initial_connected_public_entry_joins_real_return_retained_call_and_publishes_once()
     {
        let f = Fixture::ready().await;
        let before = f.snapshot();
        assert!(f.trace().inputs.is_empty());
        f.state
            .publish_foreground_native_connected(f.tid, &f.thread, &f.admission)
            .unwrap();
        let trace = f.trace();
        trace.validate().unwrap();
        assert_eq!(trace.inputs.len(), 1);
        assert_eq!(
            trace.inputs[0].event,
            NetworkInputKindV2::Connect(NetworkConnectionResultV2::Connected)
        );
        assert_eq!(trace.release_model.nodes().len(), 2);
        assert_eq!(
            trace.release_model.nodes()[0].kind,
            NetworkReleaseNodeKindV4::Input { input_ordinal: 0 }
        );
        assert_eq!(
            trace.release_model.nodes()[1].kind,
            NetworkReleaseNodeKindV4::Progress {
                channel: trace.inputs[0].channel,
                milestone: NetworkProgressV4::Established {
                    source: NetworkEstablishmentV4::ConnectedInput { input_ordinal: 0 }
                },
            }
        );
        assert!(trace.inputs[0].release.prerequisites.is_empty());
        assert_eq!(trace.inputs[0].release.receive_entry_cut.0, 0);
        let after = f.snapshot();
        assert_eq!(
            before.1, after.1,
            "publication only borrows actual runtime custody"
        );
        assert_eq!((before.2, before.3), (after.2, after.3));
        f.assert_refused();
        assert_eq!(f.trace(), trace);
        f.finish();
    }

    #[tokio::test]
    async fn shared_initial_connected_provider_and_backend_return_are_independent_required_joins() {
        for provider_only in [false, true] {
            let mut f = Fixture::new().await;
            f.connect();
            if provider_only {
                f.completed(ConnectCompletionChange::None).unwrap();
            } else {
                f.returned();
            }
            assert!(f.retire_provider().is_err());
            if provider_only {
                assert_eq!(
                    f.state
                        .network_engine
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .original_connect_result(f.owner(), &f.admission)
                        .unwrap(),
                    None
                );
            }
            f.foreground().await;
            f.assert_refused();
            assert!(f.trace().inputs.is_empty());
        }
    }

    #[tokio::test]
    async fn shared_initial_connected_wrong_or_duplicate_provider_completion_retains_original() {
        for change in [
            ConnectCompletionChange::Command,
            ConnectCompletionChange::File,
            ConnectCompletionChange::MissingSecurity,
        ] {
            let mut f = Fixture::new().await;
            f.connect();
            let before = f.snapshot();
            assert!(f.completed(change).is_err(), "{change:?}");
            assert_eq!(f.snapshot(), before);
            f.completed(ConnectCompletionChange::None).unwrap();
            let before = f.snapshot();
            assert!(f.completed(ConnectCompletionChange::None).is_err());
            assert_eq!(f.snapshot(), before);
            f.returned();
            f.retire_provider().unwrap();
            f.close_pin();
            f.foreground().await;
            f.state
                .publish_foreground_native_connected(f.tid, &f.thread, &f.admission)
                .unwrap();
            f.trace().validate().unwrap();
            f.finish();
        }
    }

    #[tokio::test]
    async fn shared_initial_connected_missing_copy_security_failure_or_changed_sockaddr_cannot_publish()
     {
        for change in [
            ConnectCompletionChange::MissingCopy,
            ConnectCompletionChange::SecurityError,
            ConnectCompletionChange::Peer,
            ConnectCompletionChange::Family,
        ] {
            let mut f = Fixture::new().await;
            f.connect();
            f.completed(change).unwrap();
            f.returned();
            f.retire_provider().unwrap();
            f.close_pin();
            f.foreground().await;
            let diagnostic = f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .controlled_connect_diagnostic(f.owner(), &f.admission);
            f.assert_refused();
            assert!(f.trace().inputs.is_empty());
            assert_eq!(
                f.state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .controlled_connect_diagnostic(f.owner(), &f.admission),
                diagnostic
            );
        }
    }

    #[tokio::test]
    async fn shared_initial_connected_publication_waits_for_provider_retirement_and_actual_pin_close()
     {
        let mut f = Fixture::new().await;
        f.connect();
        f.completed(ConnectCompletionChange::None).unwrap();
        f.returned();
        f.foreground().await;
        f.assert_refused();
        f.retire_provider().unwrap();
        f.assert_refused();
        f.close_pin();
        f.state
            .publish_foreground_native_connected(f.tid, &f.thread, &f.admission)
            .unwrap();
        f.trace().validate().unwrap();
        f.finish();
    }

    #[tokio::test]
    async fn shared_initial_connected_foreign_local_state_or_missing_call_cannot_reconstruct_publication()
     {
        for changed in 0..6 {
            let mut f = Fixture::ready().await;
            let original = f.trace();
            match changed {
                0 => f.thread.mm_id = f.thread.mm_id.for_exec(f.thread.dettid),
                1 => {
                    let copied = f.thread.file_metadata.lock().unwrap().clone();
                    f.thread.file_metadata = Arc::new(Mutex::new(copied));
                }
                2 => {
                    let copied = f.thread.memory_metadata.lock().unwrap().clone();
                    f.thread.memory_metadata = Arc::new(Mutex::new(copied));
                }
                3 => {
                    f.state.registered_exec_mms.lock().unwrap().clear();
                }
                4 => f
                    .state
                    .network_runtime
                    .as_ref()
                    .unwrap()
                    .revoke_foreground_lineage(),
                5 => {
                    let diagnostic = f
                        .state
                        .network_runtime
                        .as_ref()
                        .unwrap()
                        .controlled_connect_diagnostic(f.owner(), &f.admission);
                    let decoded: serde_json::Value = serde_json::from_slice(&diagnostic).unwrap();
                    assert!(!decoded.is_null());
                    let admission: Admission =
                        serde_json::from_slice(&serde_json::to_vec(&f.admission).unwrap()).unwrap();
                    assert_eq!(admission, f.admission);
                    f.state
                        .network_runtime
                        .as_ref()
                        .unwrap()
                        .controlled_connect_forget_closed(f.owner(), &f.admission)
                        .unwrap();
                    f.admission = admission;
                }
                _ => unreachable!(),
            }
            f.assert_refused();
            assert_eq!(f.trace(), original);
        }
    }
}
