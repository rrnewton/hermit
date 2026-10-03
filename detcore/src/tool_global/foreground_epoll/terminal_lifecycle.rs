//! Actual Tool final/consuming callbacks, scheduler and physical task retirement.
//! Census/channel rows and delivery of the backend final-wait event are controlled
//! premises; socket close is real. The official curl cell supplies native backend
//! and provider composition. https://github.com/rrnewton/hermit/issues/3612
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;

use detcore_model::network_trace::NetworkPolicy;
use detcore_model::network_trace::NetworkTraceV4;
use reverie::ExitStatus;
use reverie::GlobalRPC;
use reverie::GlobalTool;
use reverie::Pid;
use reverie::Tool;

use super::*;
use crate::Detcore;
use crate::config::Config;
use crate::network_runtime::ForegroundRoot;
use crate::tool_global::GlobalState;

struct Fixture {
    state: GlobalState,
    tool: Detcore,
    thread: crate::ThreadState<()>,
    root: Arc<ForegroundRoot>,
    prefix: crate::network_runtime::JoinedNativePrefix,
    trace: NetworkTraceV4,
    _peer: OwnedFd,
}
impl Fixture {
    async fn new() -> Self {
        let raw = unsafe { libc::syscall(libc::SYS_gettid) } as i32;
        let tid = Pid::from_raw(raw);
        let ((runtime, root, metadata, memory, _), peer) =
            ForegroundRoot::controlled_terminal_runtime(raw);
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
        let runtime = state.network_runtime.as_ref().unwrap();
        let tool = Detcore::new(tid, &config);
        let mut thread = tool.init_thread_state(tid, None);
        thread.dettid = owner.thread;
        thread.detpid = Some(owner.thread);
        thread.mm_id = owner.mm;
        thread.thread_start_entered = true;
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
        state.global_time.lock().unwrap().update_global_time(
            owner.thread,
            thread.thread_logical_time.as_nanos(),
            thread.thread_logical_time.inherited_nanos(),
        );
        let prefix = runtime.join_foreground_prefix(root.clone()).await.unwrap();
        let (mut engine, file, socket, _socket_peer) =
            NetworkReplayEngine::controlled_terminal_recording(owner);
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        {
            let scheduler = state.sched.lock().unwrap();
            let grant = scheduler
                .foreground_native_observation(owner, &root)
                .unwrap();
            runtime
                .with_foreground_prefix(&prefix, |admission| {
                    engine
                        .submit_native_descriptor_close(control, admission, &grant)
                        .map_err(std::io::Error::other)
                })
                .unwrap();
        }
        let raw = socket.into_raw_fd();
        assert_eq!(unsafe { libc::close(raw) }, 0);
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EBADF)
        );
        engine
            .confirm_descriptor_effect(owner, control, Ok(()))
            .unwrap();
        engine
            .finish_socket_control(
                owner,
                control,
                NetworkSocketControlFinish::Closed { last_alias: true },
            )
            .unwrap();
        let trace = engine.native_trace_fixture();
        trace.validate().unwrap();
        *state.network_engine.as_ref().unwrap().lock().unwrap() = engine;
        assert!(thread.original_connect.is_none());
        assert!(thread.pending_no_seq_birth.is_none());
        assert!(!thread.native_birth_required);
        assert!(root.is_sole_initial_root(owner));
        Self {
            state,
            tool,
            thread,
            root,
            prefix,
            trace,
            _peer: peer,
        }
    }
    fn terminal(&mut self, tid: Pid) {
        self.tool.on_backend_thread_terminal(
            tid,
            &self.state,
            &mut self.thread,
            ExitStatus::Exited(0),
        );
    }
    fn assert_live_authority_closed(&self) {
        let owner = self.root.owner();
        assert!(!self.root.is_current(owner));
        assert!(self.root.metadata().is_err());
        assert!(self.root.memory().is_err());
        let runtime = self.state.network_runtime.as_ref().unwrap();
        assert!(runtime.foreground_root(owner).is_err());
        assert!(runtime.validate_foreground_prefix(&self.prefix).is_err());
        assert!(
            self.state
                .sched
                .lock()
                .unwrap()
                .foreground_native_observation(owner, &self.root)
                .is_err()
        );
    }
    fn into_trace(mut self) -> Result<NetworkTraceV4, String> {
        use std::os::fd::AsRawFd;
        let mut output = tempfile::tempfile().unwrap();
        self.state.cfg.network_trace_output_fd = Some(output.as_raw_fd());
        if let Err(error) = self.state.finalize_network_trace() {
            assert_eq!(
                output.metadata().unwrap().len(),
                0,
                "failed trace was published"
            );
            return Err(format!("{error:#}"));
        }
        std::io::Seek::rewind(&mut output).unwrap();
        match detcore_model::network_trace::NetworkTrace::read_framed(&mut output).unwrap() {
            detcore_model::network_trace::NetworkTrace::V4(trace) => Ok(trace),
            other => panic!("changed native recording version: {other:?}"),
        }
    }
}
struct ExitRpc<'a> {
    state: &'a GlobalState,
    sender: Pid,
}
#[reverie::tool]
impl GlobalRPC<GlobalState> for ExitRpc<'_> {
    async fn send_rpc(
        &self,
        request: <GlobalState as GlobalTool>::Request,
    ) -> <GlobalState as GlobalTool>::Response {
        self.state.receive_rpc(self.sender, request).await
    }
    fn config(&self) -> &Config {
        &self.state.cfg
    }
}
#[tokio::test]
async fn v4_actual_terminal_and_consuming_exit_preserve_retained_root_history() {
    let mut f = Fixture::new().await;
    let owner = f.root.owner();
    let tid = Pid::from_raw(owner.thread.as_raw());
    f.terminal(tid);
    // Every live admission is closed synchronously, before any await or Tool
    // continuation; this is not a terminal status relabeled as source authority.
    f.assert_live_authority_closed();
    assert!(!f.state.sched.lock().unwrap().backend_failed());
    assert!(f.root.has_sole_initial_root_history());
    let thread = std::mem::replace(&mut f.thread, f.tool.init_thread_state(tid, None));
    f.tool
        .on_exit_thread(
            tid,
            &ExitRpc {
                state: &f.state,
                sender: tid,
            },
            thread,
            ExitStatus::Exited(0),
        )
        .await
        .unwrap();
    assert!(
        f.state
            .sched
            .lock()
            .unwrap()
            .deregistration_was_accounted(owner.thread)
    );
    assert!(
        !f.state
            .registered_exec_mms
            .lock()
            .unwrap()
            .contains_key(&owner.thread)
    );
    assert!(f.root.has_sole_initial_root_history());
    f.assert_live_authority_closed();
    let expected = f.trace.clone();
    assert_eq!(f.into_trace().unwrap(), expected);
}
#[tokio::test]
async fn v4_terminal_mismatched_owner_or_mm_still_revokes_history_and_fails() {
    for wrong_mm in [false, true] {
        let mut f = Fixture::new().await;
        let owner = f.root.owner();
        let tid = Pid::from_raw(owner.thread.as_raw() + i32::from(!wrong_mm));
        if wrong_mm {
            f.thread.mm_id = owner.mm.for_exec(owner.thread);
        }
        f.terminal(tid);
        f.assert_live_authority_closed();
        assert!(f.state.sched.lock().unwrap().backend_failed());
        assert!(!f.root.has_sole_initial_root_history());
        assert!(
            f.into_trace()
                .unwrap_err()
                .to_string()
                .contains("sole-initial-root policy")
        );
    }
}
#[tokio::test]
async fn v4_terminal_does_not_restore_lost_history_or_skip_close_after_backend_failure() {
    for history_lost in [false, true] {
        let mut f = Fixture::new().await;
        let tid = Pid::from_raw(f.root.owner().thread.as_raw());
        if history_lost {
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage();
        } else {
            f.state.report_backend_failure(reverie::BackendFailure {
                pid: tid,
                tid,
                phase: "prior terminal fixture failure",
            });
        }
        f.terminal(tid);
        f.assert_live_authority_closed();
        assert_eq!(f.root.has_sole_initial_root_history(), !history_lost);
        if history_lost {
            assert!(
                f.into_trace()
                    .unwrap_err()
                    .to_string()
                    .contains("sole-initial-root policy")
            );
        } else {
            assert!(
                f.state.sched.lock().unwrap().backend_failed(),
                "prior run failure was cleared"
            );
        }
    }
}
