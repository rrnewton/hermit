//! Real ptrace Sendto/source worker + production H caller/Global/adapter.
//! Initial H census and packaged provider replies are explicit controlled
//! premises; this is not a loaded-provider or multithreaded M2 qualification.
use detcore_model::network_trace::NetworkOutputEventV2;
use detcore_model::network_trace::NetworkOutputKindV2;
use detcore_model::network_trace::NetworkProgressV4;
use detcore_model::network_trace::NetworkReleaseModelV4;
use detcore_model::network_trace::NetworkReleaseNodeIdV4;
use detcore_model::network_trace::NetworkReleaseNodeKindV4;
use detcore_model::network_trace::NetworkReleaseNodeV4;
use detcore_model::network_trace::NetworkTraceV4;
use reverie::Tool;
use reverie::syscalls::ExecutableSourceArmer;
use reverie::syscalls::NativeUserReadError;
use reverie::syscalls::SyscallInfo;

use super::*;
use crate::network_replay::ConnectionOutcome;
use crate::network_replay::NetworkStreamNamespace;
use crate::network_runtime::executable_capture::positive_fixture;

#[repr(align(4096))]
struct Page([u8; 8]);
static SOURCE: Page = Page(*b"next\ndon");

struct Fixture {
    config: Config,
    state: GlobalState,
    tool: Detcore,
    thread: crate::ThreadState<()>,
    binding: crate::types::FdSlotBinding,
    service: positive_fixture::Service,
}
fn trace(mutant: bool) -> NetworkTraceV4 {
    let mut trace = crate::network_replay::replay_connect::fixture(LogicalTime::ZERO, false)
        .engine
        .native_trace_fixture();
    let mut bytes = SOURCE.0.to_vec();
    if mutant {
        bytes[0] ^= 1;
    }
    trace.outputs.push(NetworkOutputEventV2 {
        channel: NetworkChannelId(1),
        event: NetworkOutputKindV2::StreamBytes {
            stream_offset: 0,
            bytes,
        },
    });
    let mut nodes = trace.release_model.nodes().to_vec();
    nodes.push(NetworkReleaseNodeV4 {
        id: NetworkReleaseNodeIdV4(nodes.len() as u64),
        kind: NetworkReleaseNodeKindV4::Progress {
            channel: NetworkChannelId(1),
            milestone: NetworkProgressV4::StreamPrefix {
                exclusive_offset: SOURCE.0.len() as u64,
            },
        },
        prerequisites: vec![NetworkReleaseNodeIdV4(1)],
    });
    trace.release_model = NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { nodes };
    trace.validate().unwrap();
    trace
}
impl Fixture {
    async fn new(tid: Tid, mutant: bool) -> Self {
        let trace = trace(mutant);
        let mut bytes = Vec::new();
        detcore_model::network_trace::NetworkTrace::V4(trace.clone())
            .write_framed(&mut bytes)
            .unwrap();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            epoch: trace.epoch,
            network_trace_input: Some(bytes),
            ..Config::default()
        };
        config.network_trace.policy = NetworkPolicy::Replay;
        let positive_fixture::Fixture {
            runtime,
            root,
            metadata,
            memory,
            claim,
            service,
        } = positive_fixture::fixture(tid.as_raw());
        let owner = root.owner();
        let mut state = GlobalState::initialize(&config, false);
        state.network_runtime = Some(runtime);
        let tool = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.detpid = Some(owner.thread);
        thread.mm_id = owner.mm;
        thread.stats.syscall_count = 383;
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
            assert!(engine.uses_shared_mm_attempts());
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
        let binding = publish_owned_read_fd(&tool, &mut guest, crate::fd::FdType::Socket).await;
        assert_eq!(binding.slot.fd, 7);
        tool.initialize_network_fd_tracking(&mut guest)
            .await
            .unwrap();
        {
            let mut engine = state.network_engine.as_ref().unwrap().lock().unwrap();
            engine
                .register_stream_socket(
                    binding.open_file,
                    trace.fresh_stream_profiles[0].key,
                    NetworkStreamNamespace {
                        device: 7,
                        inode: 11,
                    },
                    None,
                )
                .unwrap();
            engine.bind(binding.open_file, NetworkChannelId(1)).unwrap();
            engine.release_eligible(LogicalTime::ZERO).unwrap();
            assert_eq!(
                engine.take_connection_outcome(binding.open_file).unwrap(),
                Some(ConnectionOutcome::Connect(
                    detcore_model::network_trace::NetworkConnectionResultV2::Connected
                ))
            );
        }
        grant_owned_read_foreground(&state, &mut guest).await;
        let thread = guest.thread;
        Self {
            config,
            state,
            tool,
            thread,
            binding,
            service,
        }
    }
}

struct ActualGuest<'a, G> {
    actual: &'a mut G,
    local: OwnedReadGuest<'a>,
    joins: usize,
    sources: usize,
    captured_original_span: (usize, usize),
}
#[reverie::tool]
impl<G: Guest<NativeTool>> GlobalRPC<GlobalState> for ActualGuest<'_, G> {
    async fn send_rpc(
        &self,
        message: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.local.send_rpc(message).await
    }
    fn config(&self) -> &Config {
        self.local.config
    }
}
#[reverie::tool]
impl<G: Guest<NativeTool>> Guest<Detcore> for ActualGuest<'_, G> {
    type Memory = OwnedReadMemory;
    type Stack = ExternalRegistrationStack;
    fn tid(&self) -> Tid {
        self.actual.tid()
    }
    fn pid(&self) -> Tid {
        self.actual.pid()
    }
    fn ppid(&self) -> Option<Tid> {
        self.actual.ppid()
    }
    fn local_global_state(&self) -> Option<&GlobalState> {
        Some(self.local.global)
    }
    fn memory(&self) -> Self::Memory {
        OwnedReadMemory { forbid: true }
    }
    fn thread_state(&self) -> &crate::ThreadState<()> {
        &self.local.thread
    }
    fn thread_state_mut(&mut self) -> &mut crate::ThreadState<()> {
        &mut self.local.thread
    }
    async fn join_followed_observation_timers(
        &mut self,
        original: reverie::syscalls::Syscall,
    ) -> Result<(), reverie::Error> {
        self.actual
            .join_followed_observation_timers(original)
            .await?;
        self.joins += 1;
        Ok(())
    }
    async fn stage_followed_executable_source(
        &mut self,
        address: usize,
        length: usize,
        retention: Box<dyn Send + Sync>,
        armer: Box<dyn ExecutableSourceArmer>,
    ) -> Result<Vec<u8>, NativeUserReadError> {
        assert_eq!((address, length), self.captured_original_span);
        // Actual R SourceJobs registration, native GET/PKRU, worker memory read,
        // stopped-cohort validation and true JoinHandle completion. No supplied
        // result or replacement bytes; the production caller receives this Vec.
        let result = self
            .actual
            .stage_followed_executable_source(address, length, retention, armer)
            .await;
        if let Ok(bytes) = &result {
            assert_eq!(bytes.as_slice(), &SOURCE.0);
            self.sources += 1;
        }
        result
    }
    async fn read_native_source(
        &mut self,
        _: usize,
        _: usize,
        _: Box<dyn Send + Sync>,
    ) -> Result<Vec<u8>, NativeUserReadError> {
        panic!("legacy source fallback")
    }
    async fn stage_followed_source(
        &mut self,
        _: usize,
        _: usize,
        _: Box<dyn Send + Sync>,
    ) -> Result<Vec<u8>, NativeUserReadError> {
        panic!("anonymous source fallback")
    }
    async fn regs(&mut self) -> libc::user_regs_struct {
        panic!("ordinary regs")
    }
    async fn stack(&mut self) -> Self::Stack {
        panic!("ordinary stack")
    }
    async fn daemonize(&mut self) {
        panic!("daemonize")
    }
    async fn inject<S: SyscallInfo>(&mut self, _: S) -> Result<i64, reverie::Errno> {
        panic!("native network injection")
    }
    async fn tail_inject<S: SyscallInfo>(&mut self, _: S) -> reverie::Never {
        panic!("tail injection")
    }
    async fn cancel_current_thread(&mut self) -> reverie::Never {
        panic!("canceled selected caller")
    }
    async fn retire_current_thread(&mut self) -> reverie::Never {
        panic!("retired selected caller")
    }
    fn set_timer(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("timer")
    }
    fn set_timer_precise(&mut self, _: reverie::TimerSchedule) -> Result<(), reverie::Error> {
        panic!("timer")
    }
    fn read_clock(&mut self) -> Result<u64, reverie::Error> {
        panic!("clock")
    }
}

#[derive(Debug)]
struct Evidence {
    mutant: bool,
    joined: usize,
    source: usize,
    offset: u64,
    error: Option<String>,
}
#[derive(Default)]
struct NativeGlobal {
    evidence: Mutex<Option<Evidence>>,
}
#[reverie::global_tool]
impl GlobalTool for NativeGlobal {
    type Config = bool;
    type Request = ();
    type Response = ();
    async fn receive_rpc(&self, _: Tid, _: ()) {}
}
#[derive(Default)]
struct NativeTool;
#[reverie::tool]
impl Tool for NativeTool {
    type GlobalState = NativeGlobal;
    type ThreadState = ();
    fn subscriptions(_: &bool) -> reverie::Subscription {
        [
            reverie::syscalls::Sysno::sendto,
            reverie::syscalls::Sysno::exit,
            reverie::syscalls::Sysno::exit_group,
        ]
        .into_iter()
        .collect()
    }
    async fn handle_syscall_event<G: Guest<Self>>(
        &self,
        actual: &mut G,
        call: reverie::syscalls::Syscall,
    ) -> Result<i64, reverie::Error> {
        let reverie::syscalls::Syscall::Sendto(send) = call else {
            return Ok(actual.inject(call).await?);
        };
        assert_eq!(send.fd(), 7);
        assert_eq!(send.size(), SOURCE.0.len());
        assert_eq!(send.flags(), libc::MSG_NOSIGNAL as u32);
        assert!(send.buf().is_some() && send.addr().is_none());
        let mutant = *actual.config();
        let q = Fixture::new(actual.tid(), mutant).await;
        let mut local = owned_read_guest(&q.config, &q.state, q.thread);
        local.expose_local_global = true;
        local.forbid_ordinary_memory = true;
        let before = q.state.sched.lock().unwrap().turn;
        let original_trace = q
            .state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .native_trace_fixture();
        let mut guest = ActualGuest {
            actual,
            local,
            joins: 0,
            sources: 0,
            captured_original_span: (send.buf().unwrap().as_raw(), send.size()),
        };
        let result = q
            .tool
            .executable_source_fixture_dispatch(&mut guest, send)
            .await;
        let joined = guest.joins;
        let source = guest.sources;
        assert_eq!(
            (joined, source),
            (1, 1),
            "real source join must precede either compare result"
        );
        assert_eq!(
            q.state.sched.lock().unwrap().turn,
            before,
            "source read adds no scheduler request"
        );
        let error = result.as_ref().err().map(ToString::to_string);
        let mut engine = q.state.network_engine.as_ref().unwrap().lock().unwrap();
        let offset = engine.replay_transmit_offset(q.binding.open_file).unwrap();
        assert_eq!(engine.native_trace_fixture(), original_trace);
        if mutant {
            assert!(
                result.is_err(),
                "different expected trace must not replace actual source"
            );
            let refusal = match result.as_ref().unwrap_err() {
                reverie::Error::Tool(error) => {
                    error.downcast_ref::<crate::network_failure::NetworkPolicyRefusal>()
                }
                _ => None,
            };
            assert_eq!(
                refusal.map(|refusal| refusal.reason()),
                Some(crate::network_failure::NetworkRefusalReason::OutboundMismatch),
                "must reach the typed actual byte comparator: {result:?}"
            );
            assert_eq!(offset, 0);
            assert_ne!(
                engine.native_capture_fixture_counts(q.binding.open_file).0,
                0,
                "failed comparison retains the original Call"
            );
            assert!(engine.finish().is_err());
        } else {
            assert_eq!(result.as_ref().unwrap(), &(SOURCE.0.len() as i64));
            assert_eq!(offset, SOURCE.0.len() as u64);
            assert_eq!(
                engine.native_capture_fixture_counts(q.binding.open_file),
                (0, 0, 0, 0)
            );
        }
        drop(engine);
        q.service.joined();
        let evidence = Evidence {
            mutant,
            joined,
            source,
            offset,
            error,
        };
        let previous = guest
            .actual
            .local_global_state()
            .unwrap()
            .evidence
            .lock()
            .unwrap()
            .replace(evidence);
        assert!(previous.is_none(), "one genuine original Sendto");
        result
    }
}

async fn run(mutant: bool) {
    // Actual exec establishes the backend's original image/MM/cohort. A
    // fork-only closure cannot supply this authority. The guest's address is
    // observed only from its stopped Sendto, never copied from this process.
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("executable-source-probe");
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tests/c/network_replay_executable_source_probe.c");
    let compiler_log = directory.path().join("compiler.log");
    let compiler_output = std::fs::File::create(&compiler_log).unwrap();
    let mut compile = tokio::process::Command::new("cc");
    compile
        .kill_on_drop(true)
        .args([
            "-O2",
            "-g",
            "-nostdlib",
            "-static",
            "-no-pie",
            "-fno-stack-protector",
            "-Wl,--build-id=none",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .stdout(compiler_output.try_clone().unwrap())
        .stderr(compiler_output);
    let mut compiler = compile.spawn().unwrap();
    let built =
        match tokio::time::timeout(std::time::Duration::from_secs(20), compiler.wait()).await {
            Ok(status) => status.unwrap(),
            Err(_) => {
                compiler
                    .kill()
                    .await
                    .expect("reap bounded fixture compiler");
                panic!("standalone fixture compilation exceeded its separate 20-second bound");
            }
        };
    let diagnostics = std::fs::read_to_string(&compiler_log).unwrap();
    assert!(built.success(), "fixture compiler {built}: {diagnostics}");
    use sha2::Digest;
    eprintln!(
        "executable source fixture: compiler={built}, source_sha256={:x}, elf_sha256={:x}",
        sha2::Sha256::digest(std::fs::read(&source).unwrap()),
        sha2::Sha256::digest(std::fs::read(&executable).unwrap())
    );
    let command = reverie::process::Command::new(&executable);
    let tracer = reverie_ptrace::TracerBuilder::<NativeTool>::new(command)
        .config(mutant)
        .spawn()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), tracer.wait_completion())
        .await
        .expect("original bounded native completion");
    let reverie_ptrace::ToolRunOutcome::Complete(completed) = outcome else {
        panic!("genuine original native workers did not complete");
    };
    assert!(
        completed.global_state.evidence.lock().unwrap().is_some(),
        "native caller produced no evidence; actual backend result: {:?}",
        completed.result,
    );
    let evidence = completed
        .global_state
        .evidence
        .lock()
        .unwrap()
        .take()
        .unwrap();
    assert_eq!(evidence.mutant, mutant);
    assert_eq!((evidence.joined, evidence.source), (1, 1));
    if mutant {
        assert!(completed.result.is_err());
        assert_eq!(evidence.offset, 0);
        assert!(evidence.error.is_some());
    } else {
        assert_eq!(
            completed.result.unwrap(),
            reverie::process::ExitStatus::Exited(0)
        );
        assert_eq!(evidence.offset, SOURCE.0.len() as u64);
        assert!(evidence.error.is_none());
    }
}
#[tokio::test]
async fn executable_source_native_actual_adapter_global_caller_consumes_readonly_static_source() {
    run(false).await;
}
#[tokio::test]
async fn executable_source_native_actual_source_rejects_mutated_expected_trace_after_ack_and_join()
{
    run(true).await;
}
