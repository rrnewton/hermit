//! Actual local Global joins over explicitly controlled initial census/profile
//! premises. No test below supplies a native op25 result or worker certificate.
use reverie::Tool;

use super::*;

#[tokio::test]
async fn shared_send_global_requires_current_task_mm_metadata_and_selected_reader() {
    let f = crate::network_replay::shared_send::tests::fixture();
    let owner = f.root.owner();
    let tid = Tid::from_raw(f.root.thread());
    let mut config = Config {
        sequentialize_threads: true,
        epoch_explicit: true,
        epoch: f.engine.lock().unwrap().native_trace_fixture().epoch,
        ..Config::default()
    };
    config.network_trace.policy = NetworkPolicy::Record;
    let mut global = GlobalState::initialize(&config, false);
    let tool: crate::Detcore = crate::Detcore::new(tid, &config);
    let mut thread = tool.init_thread_state(tid, None);
    thread.dettid = owner.thread;
    thread.detpid = Some(f.root.logical_process());
    thread.mm_id = owner.mm;
    thread.file_metadata = f.metadata;
    thread.memory_metadata = f.memory;
    global.network_runtime = Some(f.runtime);
    global.network_engine = Some(Arc::new(f.engine));
    global.sched = Arc::new(Mutex::new(f.scheduler));
    global
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    assert_eq!(
        global.shared_send_timeout(tid, &thread, &f.read).unwrap(),
        5000
    );
    assert!(
        global
            .shared_send_timeout(Tid::from_raw(tid.as_raw() + 1), &thread, &f.read)
            .is_err()
    );
    global
        .registered_exec_mms
        .lock()
        .unwrap()
        .remove(&owner.thread);
    assert!(global.shared_send_timeout(tid, &thread, &f.read).is_err());
    global
        .registered_exec_mms
        .lock()
        .unwrap()
        .insert(owner.thread, owner.mm);
    let real_metadata = thread.file_metadata.clone();
    thread.file_metadata = Arc::new(Mutex::new(
        crate::tool_local::FileMetadata::empty_network_fixture(owner.thread),
    ));
    assert!(global.shared_send_timeout(tid, &thread, &f.read).is_err());
    thread.file_metadata = real_metadata;
    assert_eq!(
        global.shared_send_timeout(tid, &thread, &f.read).unwrap(),
        5000
    );
    let mut wrong_read = f.read.clone();
    wrong_read.fd += 1;
    assert!(
        global
            .shared_send_timeout(tid, &thread, &wrong_read)
            .is_err()
    );
    global
        .network_engine
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .validate_fd_read_grant(owner, &f.read)
        .unwrap();
}
