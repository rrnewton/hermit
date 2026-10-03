//! Actual Tool terminal callback and Global owner cleanup, with the existing
//! typed provider/birth fixture. PIDFD stand-ins and delivery of final wait are
//! controlled premises; this is not native multithreaded qualification.
use reverie::ExitStatus;
use reverie::Tool;

use super::*;
use crate::network_runtime::ForegroundRoot;
use crate::network_runtime::native_birth_outcome::NativeTaskProjection;

struct Fixture {
    state: GlobalState,
    tool: Detcore,
    thread: crate::ThreadState<()>,
    parent: Arc<ForegroundRoot>,
    child: Arc<ForegroundRoot>,
    projection: Arc<NativeTaskProjection>,
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
        cfg.network_trace.policy = detcore_model::network_trace::NetworkPolicy::Record;
        let mut state = GlobalState::initialize(&cfg, false);
        *state.network_engine.as_ref().unwrap().lock().unwrap() =
            NetworkReplayEngine::record_shared_mm_attempts(cfg.epoch);
        let birth = ForegroundRoot::controlled_shared_birth_after_close_setup(raw, |root, _| {
            state
                .sched
                .lock()
                .unwrap()
                .controlled_foreground_store_grant(root);
        })
        .await;
        state.sched.lock().unwrap().controlled_shared_birth_census(
            &birth.parent,
            &birth.child,
            &birth._birth,
        );
        let parent = birth.parent.clone();
        let child = birth.child.clone();
        let projection = state
            .sched
            .lock()
            .unwrap()
            .shared_terminal_projection(child.owner(), parent.logical_process())
            .unwrap()
            .unwrap();
        let tool = Detcore::new(Tid::from_raw(raw), &cfg);
        let mut thread = tool.init_thread_state(Tid::from_raw(child.owner().thread.as_raw()), None);
        thread.dettid = child.owner().thread;
        thread.mm_id = child.owner().mm;
        thread.detpid = Some(parent.logical_process());
        thread.thread_start_entered = true;
        thread.file_metadata = birth.metadata.clone();
        thread.memory_metadata = birth.memory.clone();
        let (runtime, retained) = birth.into_runtime_and_retention();
        state.network_runtime = Some(runtime);
        Self {
            state,
            tool,
            thread,
            parent,
            child,
            projection,
            _retained: retained,
        }
    }
    fn admitted(&self) -> bool {
        let scheduler = self.state.sched.lock().unwrap();
        self.state
            .network_runtime
            .as_ref()
            .unwrap()
            .with_shared_foreground_lineage(self.parent.owner(), |lineage| {
                scheduler
                    .shared_mm_foreground_observation(self.parent.owner(), lineage)
                    .map(|_| ())
            })
            .is_ok()
    }
    fn cleanup(&self) {
        let owner = self.child.owner();
        self.state.recv_network_owner_gone(owner);
        self.state.sched.lock().unwrap().logically_kill_thread(
            &owner.thread,
            &self.parent.logical_process(),
            owner.mm,
        );
    }
    fn terminal(&mut self) {
        self.tool.on_backend_thread_terminal(
            Tid::from_raw(self.child.owner().thread.as_raw()),
            &self.state,
            &mut self.thread,
            ExitStatus::Exited(0),
        );
    }
    fn drain(&self) {
        self.state
            .sched
            .lock()
            .unwrap()
            .controlled_drain_shared_terminal_removals();
    }
}

#[tokio::test]
async fn shared_terminal_actual_callback_and_cleanup_both_orders_preserve_history() {
    for cleanup_first in [false, true] {
        let mut f = Fixture::new().await;
        assert!(f.admitted());
        let history = f.state.sched.lock().unwrap().thread_tree.size();
        let clock = f.state.global_time.lock().unwrap().as_nanos();
        if cleanup_first {
            f.cleanup();
            assert!(!f.admitted(), "logical cleanup is not final wait");
            f.drain();
            assert!(
                !f.admitted(),
                "scheduler removal cannot issue physical death"
            );
        }
        f.terminal();
        assert!(!f.state.sched.lock().unwrap().backend_failed());
        assert!(f.projection.completed_final_wait(&f.parent).is_some());
        assert!(!f.child.is_current(f.child.owner()));
        assert!(f.parent.is_current(f.parent.owner()));
        if !cleanup_first {
            assert!(
                !f.admitted(),
                "registered dead child still requires consuming cleanup"
            );
            f.cleanup();
            assert!(!f.admitted(), "deferred removal must drain normally");
            f.drain();
        }
        assert!(f.admitted());
        assert_eq!(f.state.sched.lock().unwrap().thread_tree.size(), history);
        assert_eq!(f.state.global_time.lock().unwrap().as_nanos(), clock);
        assert!(!f.parent.is_sole_initial_root(f.parent.owner()));
        f.terminal(); // exact repeat after physical owner release is idempotent
        assert!(!f.state.sched.lock().unwrap().backend_failed());
        assert!(f.admitted());
    }
}

#[tokio::test]
async fn shared_terminal_actual_wrong_task_mm_or_process_never_publishes() {
    for fault in 0..3 {
        let mut f = Fixture::new().await;
        f.cleanup();
        f.drain();
        let owner = f.child.owner();
        if fault == 1 {
            f.thread.mm_id = owner.mm.for_exec(owner.thread);
        }
        if fault == 2 {
            f.tool = Detcore::new(Tid::from_raw(owner.thread.as_raw() + 100), &f.state.cfg);
        }
        f.tool.on_backend_thread_terminal(
            Tid::from_raw(owner.thread.as_raw() + i32::from(fault == 0)),
            &f.state,
            &mut f.thread,
            ExitStatus::Exited(0),
        );
        assert!(f.state.sched.lock().unwrap().backend_failed());
        assert!(f.projection.completed_final_wait(&f.parent).is_none());
        assert!(!f.admitted());
    }
}

#[tokio::test]
async fn shared_terminal_parent_death_or_prior_lineage_failure_cannot_admit_child() {
    for lose_parent in [false, true] {
        let mut f = Fixture::new().await;
        if lose_parent {
            f.state.recv_network_owner_gone(f.parent.owner());
        } else {
            f.state
                .network_runtime
                .as_ref()
                .unwrap()
                .revoke_foreground_lineage();
        }
        f.cleanup();
        f.drain();
        f.terminal();
        assert!(!f.admitted());
        assert!(f.projection.completed_final_wait(&f.parent).is_none());
        assert!(!f.parent.is_sole_initial_root(f.parent.owner()));
    }
}

#[tokio::test]
async fn shared_terminal_observers_complete_even_without_local_original_slots() {
    let f = Fixture::new().await;
    let owner = f.child.owner();
    assert!(f.thread.original_connect.is_none());
    f.state.settle_no_seq_terminal(
        Tid::from_raw(owner.thread.as_raw()),
        f.parent.logical_process(),
        &f.thread,
    );
    assert!(
        f.projection.completed_final_wait(&f.parent).is_none(),
        "settle alone omits both terminal observers"
    );
    f.cleanup();
    f.drain();
    assert!(!f.admitted());
    f.state.observe_original_connect_terminal(
        Tid::from_raw(owner.thread.as_raw()),
        f.parent.logical_process(),
        &f.thread,
    );
    assert!(f.projection.completed_final_wait(&f.parent).is_none());
    f.state.observe_native_stream_terminal(
        Tid::from_raw(owner.thread.as_raw()),
        f.parent.logical_process(),
        &f.thread,
    );
    assert!(f.projection.completed_final_wait(&f.parent).is_some());
    assert!(f.admitted());
}

#[tokio::test]
async fn shared_terminal_actual_callback_does_not_settle_unknown_child_capture() {
    let mut f = Fixture::new().await;
    let owner = f.child.owner();
    let call = {
        let mut engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
        // Controlled descriptor premise, actual engine ownership transition.
        // No native pin acquisition has completed: this is a retained unknown
        // capture, not invented positive physical evidence.
        let file = OpenFileId::new_socket(owner.thread, 700);
        let control = engine.begin_socket_controls(owner, vec![file]).unwrap()[0].1;
        let call = engine.begin_stream_call(owner, control).unwrap();
        assert!(call.physical_pin_required);
        engine
            .finish_socket_control(owner, control, NetworkSocketControlFinish::Unchanged)
            .unwrap();
        assert_eq!(
            engine.terminal_stream_admission(owner, call.id).unwrap(),
            None
        );
        call
    };
    f.cleanup();
    f.drain();
    f.terminal();
    assert!(
        f.projection.completed_final_wait(&f.parent).is_some(),
        "physical death and semantic debt are distinct"
    );
    assert!(
        f.state.sched.lock().unwrap().backend_failed(),
        "existing terminal observer must retain its failure"
    );
    assert!(!f.admitted());
    let engine = f.state.network_engine.as_ref().unwrap().lock().unwrap();
    assert!(
        matches!(engine.terminal_stream_admission(owner, call.id),
        Err(crate::network_replay::NetworkReplayError::StreamCallPhaseMismatch(id)) if id == call.id),
        "the original unknown Call must still exist in its original phase"
    );
    assert!(
        engine.finish().is_err(),
        "terminal projection cannot forgive capture debt"
    );
}
