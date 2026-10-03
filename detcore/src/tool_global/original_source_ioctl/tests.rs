//! Provider rows and backend ENTRY are controlled premises. The actual Tool
//! callback, root issuer, Normal grant and metadata admission execute here.
use std::sync::Arc;
use std::sync::Mutex;

use detcore_model::network_trace::NetworkPolicy;
use detcore_model::network_trace::NetworkTrace;
use reverie::Tid;
use reverie::Tool;
use reverie::syscalls::SyscallArgs;

use super::*;
use crate::Detcore;
use crate::config::Config;
use crate::fd::DetFd;
use crate::fd::FdType;
use crate::network_replay::NetworkReplayEngine;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::original_installation::FileIdentity;
use crate::tool_local::FileMetadata;
use crate::types::OpenFileId;

// Independent controlled ABI9 census values; no native observation is claimed.
const NULL_DISPATCH: u64 = 1;
const BTRFS_DISPATCH: u64 = 2;

struct Fixture {
    tool: Detcore,
    state: GlobalState,
    root: Arc<ForegroundRoot>,
    metadata: Arc<Mutex<FileMetadata>>,
    // Keep the exact state-ready memory association alive through classification.
    _memory: Arc<Mutex<crate::memory::MemoryMetadata>>,
    tid: Tid,
}

impl Fixture {
    fn new(dispatch: u64, replay: bool) -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Tid::from_raw(raw);
        let (runtime, root, metadata, memory, claim) =
            ForegroundRoot::controlled_source_ioctl_runtime(raw, dispatch);
        let owner = root.owner();
        let mut config = Config {
            sequentialize_threads: true,
            epoch_explicit: true,
            ..Config::default()
        };
        let record = NetworkReplayEngine::record_native_receive(config.epoch);
        let trace = record.native_trace_fixture();
        config.network_trace.policy = if replay {
            NetworkPolicy::Replay
        } else {
            NetworkPolicy::Record
        };
        if replay {
            let mut bytes = Vec::new();
            NetworkTrace::V4(trace.clone())
                .write_framed(&mut bytes)
                .unwrap();
            config.network_trace_input = Some(bytes);
        }
        let mut state = GlobalState::initialize(&config, false);
        state.network_runtime = Some(runtime);
        let tool: Detcore = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.mm_id = owner.mm;
        thread.detpid = Some(owner.thread);
        thread.file_metadata = metadata.clone();
        thread.memory_metadata = memory.clone();
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
            *engine = if replay {
                NetworkReplayEngine::replay_native_receive(trace).unwrap()
            } else {
                record
            };
            engine.fd_table_fixture_enable();
            engine
                .register_initial_census(root.association(), &claim, owner.thread)
                .unwrap();
        }
        tool.on_thread_state_ready(tid, &state, &thread).unwrap();
        Self {
            tool,
            state,
            root,
            metadata,
            _memory: memory,
            tid,
        }
    }

    fn entry(&self, fd: usize, request: usize) -> OriginalIoctlEntry {
        // Component premise only: no actual stopped native ioctl is claimed.
        unsafe {
            OriginalIoctlEntry::from_original_backend_entry(
                self.tid,
                SyscallArgs::new(fd, request, 0x1234, 0x55, 0x66, 0x77),
            )
        }
    }

    fn classify(&self, entry: &OriginalIoctlEntry) -> Option<OriginalIoctlEffect> {
        self.tool.classify_original_source_ioctl(&self.state, entry)
    }

    fn time_and_turn(&self) -> (crate::types::LogicalTime, String) {
        let sched = self.state.sched.lock().unwrap();
        (
            self.state.global_time.lock().unwrap().as_nanos(),
            format!("{:?}", sched.next_turns),
        )
    }
}

#[tokio::test]
async fn original_source_ioctl_joins_both_dispatchers_and_initial_aliases_without_a_turn() {
    for dispatch in [NULL_DISPATCH, BTRFS_DISPATCH] {
        for replay in [false, true] {
            let f = Fixture::new(dispatch, replay);
            let before = f.time_and_turn();
            for fd in [1, 3] {
                for request in [0x5401, 0x5413] {
                    let entry = f.entry(fd, request);
                    let effect = f.classify(&entry).expect("complete same-OFD original join");
                    assert!(entry.accepts(&effect));
                    assert!(
                        !f.entry(fd, request).accepts(&effect),
                        "equal later attempt differs"
                    );
                }
            }
            assert!(f.classify(&f.entry(2, 0x5401)).is_none());
            assert!(f.classify(&f.entry(1, 0x541b)).is_none());
            assert_eq!(f.time_and_turn(), before);
        }
    }
}

#[tokio::test]
async fn original_source_ioctl_refuses_unknown_dispatch_and_changed_original_custody() {
    for code in [0, 3] {
        let f = Fixture::new(code, false);
        assert!(f.classify(&f.entry(1, 0x5401)).is_none());
    }
    for mutation in 0..12 {
        let mut f = Fixture::new(NULL_DISPATCH, false);
        let owner = f.root.owner();
        let entry = f.entry(1, 0x5401);
        assert!(f.classify(&entry).is_some());
        match mutation {
            0 => f.state.cfg.sequentialize_threads = false,
            1 => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .remove(&owner.thread);
            }
            2 => {
                f.state
                    .registered_exec_mms
                    .lock()
                    .unwrap()
                    .insert(owner.thread, owner.mm.for_exec(owner.thread));
            }
            3 => f
                .state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage(),
            4 => {
                f.state
                    .sched
                    .lock()
                    .unwrap()
                    .next_turns
                    .get_mut(&owner.thread)
                    .unwrap()
                    .req = crate::ivar::Ivar::new();
            }
            5 => {
                f.state
                    .sched
                    .lock()
                    .unwrap()
                    .next_turns
                    .get_mut(&owner.thread)
                    .unwrap()
                    .req =
                    crate::ivar::Ivar::full(Ok(crate::resources::Resources::new(owner.thread)));
            }
            6 => {
                f.metadata.lock().unwrap().files_id = crate::types::FilesId::initial(owner.thread);
            }
            7 => {
                f.metadata.lock().unwrap().file_handles.remove(&1);
            }
            8 => {
                let mut metadata = f.metadata.lock().unwrap();
                let previous = metadata.file_handles[&1].clone();
                let replacement = DetFd::new(
                    1,
                    nix::fcntl::OFlag::O_WRONLY,
                    FdType::Regular,
                    OpenFileId::new(owner.thread, 999),
                )
                .with_stat(previous.stat().unwrap())
                .with_resource(previous.resource());
                assert!(replacement.bind_native_file(FileIdentity::controlled_fixture(3, 38)));
                metadata.file_handles.insert(1, replacement);
            }
            9 => {
                *f.state.network_engine.as_ref().unwrap().lock().unwrap() =
                    NetworkReplayEngine::record_shadow(f.state.cfg.epoch);
            }
            10 => {
                *f.state.network_engine.as_ref().unwrap().lock().unwrap() =
                    NetworkReplayEngine::record_native_receive(f.state.cfg.epoch);
            }
            11 => {
                let copied = f.metadata.lock().unwrap().clone();
                f.metadata = Arc::new(Mutex::new(copied));
            }
            _ => unreachable!(),
        }
        let before = f.time_and_turn();
        assert!(f.classify(&entry).is_none(), "mutation {mutation}");
        assert_eq!(f.time_and_turn(), before, "mutation {mutation}");
    }
}

#[tokio::test]
async fn original_source_ioctl_rejects_foreign_backend_task_and_tool_process() {
    let f = Fixture::new(NULL_DISPATCH, false);
    let entry = f.entry(1, 0x5401);
    let foreign = unsafe {
        OriginalIoctlEntry::from_original_backend_entry(
            Tid::from_raw(f.tid.as_raw() + 1),
            entry.args(),
        )
    };
    assert!(f.classify(&foreign).is_none());
    let other: Detcore = Detcore::new(Tid::from_raw(f.tid.as_raw() + 1), &f.state.cfg);
    assert!(
        other
            .classify_original_source_ioctl(&f.state, &entry)
            .is_none()
    );
    assert!(f.classify(&entry).is_some());
}
