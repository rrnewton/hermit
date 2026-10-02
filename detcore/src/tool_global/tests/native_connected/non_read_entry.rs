//! Real Tool -> GlobalState -> engine entry dispatch, not a replacement model.
//! The existing fixture supplies controlled provider/admission observations;
//! loopback connects and held-FD closes are physical. No ptrace/BPF claim.
use super::*;

fn observe_tool(f: &mut Fixture, event: InjectedSyscallEvent, wrong: u8) {
    let raw = f.thread.original_connect.as_ref().unwrap().raw_arguments;
    let tid = Tid::from_raw(f.tid.as_raw() + i32::from(wrong == 1));
    f.tool.on_injected_syscall_observed(
        tid,
        &f.state,
        &mut f.thread,
        Sysno::connect,
        SyscallArgs::new(
            raw[0],
            raw[1] + usize::from(wrong == 2),
            raw[2],
            raw[3],
            raw[4],
            raw[5],
        ),
        event,
    );
}

fn assert_no_result(f: &Fixture) {
    assert_eq!(f.thread.original_connect.as_ref().unwrap().returned, None);
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_connect_result(f.owner(), &f.admission)
            .unwrap(),
        None,
    );
    assert!(f.trace().inputs.is_empty());
}

#[tokio::test]
async fn non_read_entry_precedes_real_connect_but_cannot_publish_its_result() {
    // Fixture::new invokes the existing actual Prepared consumer and retains
    // its original native Call/pin. The new event goes through the Tool hook.
    let mut f = Fixture::new().await;
    let before = f.snapshot();
    let trace = f.trace();
    observe_tool(&mut f, InjectedSyscallEvent::Entered, 0);
    assert!(!f.state.sched.lock().unwrap().backend_failed());
    assert_no_result(&f);
    let entered = f.snapshot();
    assert_eq!(
        (entered.1, entered.2, entered.3),
        (before.1, before.2, before.3),
        "entry changed runtime custody, scheduler turn or logical time"
    );
    assert_eq!(f.trace(), trace);
    assert!(
        f.retire_provider().is_err(),
        "entry is not provider retirement"
    );
    f.assert_refused();

    f.connect(); // actual original loopback effect, after the entry observation
    f.completed(ConnectCompletionChange::None).unwrap();
    let raw = f.returned.unwrap();
    assert_eq!(raw, 0);
    observe_tool(&mut f, InjectedSyscallEvent::Returned(raw), 0);
    assert!(!f.state.sched.lock().unwrap().backend_failed());
    assert_eq!(
        f.thread.original_connect.as_ref().unwrap().returned,
        Some(raw)
    );
    f.retire_provider().unwrap();
    f.close_pin();
    f.foreground().await;
    f.state
        .publish_foreground_native_connected(f.tid, &f.thread, &f.admission)
        .unwrap();
    assert_eq!(f.trace().inputs.len(), 1);
    f.trace().validate().unwrap();
    f.finish();
}

#[tokio::test]
async fn non_read_entry_refuses_wrong_task_arguments_admission_and_phase() {
    for wrong in 1..=7 {
        let mut f = Fixture::new().await;
        match wrong {
            3 => f.thread.original_connect.as_mut().unwrap().admission = None,
            4 => {
                f.thread
                    .original_connect
                    .as_mut()
                    .unwrap()
                    .admission
                    .as_mut()
                    .unwrap()
                    .arguments
                    .address += 1
            }
            5 => f.thread.original_connect.as_mut().unwrap().invoked = false,
            6 => f.thread.stats.syscall_count += 1,
            7 => {
                let other = f
                    .admission
                    .call
                    .native_command_call()
                    .checked_add(1)
                    .unwrap();
                f.thread
                    .original_connect
                    .as_mut()
                    .unwrap()
                    .admission
                    .as_mut()
                    .unwrap()
                    .call = crate::network_replay::NetworkStreamCallId::controlled_fixture(other);
            }
            _ => {}
        }
        let engine_before = f.snapshot().0;
        observe_tool(&mut f, InjectedSyscallEvent::Entered, wrong);
        assert!(
            f.state.sched.lock().unwrap().backend_failed(),
            "wrong case {wrong}"
        );
        assert_eq!(
            f.snapshot().0,
            engine_before,
            "wrong entry changed original Call"
        );
        assert_no_result(&f);
        f.assert_refused();
    }
}

#[tokio::test]
async fn non_read_duplicate_entry_refuses_without_a_result_or_second_transition() {
    let mut f = Fixture::new().await;
    observe_tool(&mut f, InjectedSyscallEvent::Entered, 0);
    assert!(!f.state.sched.lock().unwrap().backend_failed());
    let engine_before = f.snapshot().0;
    observe_tool(&mut f, InjectedSyscallEvent::Entered, 0);
    assert!(f.state.sched.lock().unwrap().backend_failed());
    assert_eq!(f.snapshot().0, engine_before);
    assert_no_result(&f);
    f.assert_refused();
}

#[tokio::test]
async fn non_read_entry_after_real_return_refuses_without_rewriting_that_return() {
    let mut f = Fixture::new().await;
    f.connect();
    f.completed(ConnectCompletionChange::None).unwrap();
    f.returned(); // unchanged backend may report a result without optional ENTRY
    let engine_before = f.snapshot().0;
    observe_tool(&mut f, InjectedSyscallEvent::Entered, 0);
    assert!(f.state.sched.lock().unwrap().backend_failed());
    assert_eq!(f.snapshot().0, engine_before);
    assert_eq!(
        f.thread.original_connect.as_ref().unwrap().returned,
        Some(0)
    );
    assert_eq!(
        f.state
            .network_engine
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .original_connect_result(f.owner(), &f.admission)
            .unwrap(),
        Some(0),
    );
    f.retire_provider().unwrap();
    f.close_pin();
    f.assert_refused();
}

#[tokio::test]
async fn non_read_signal_observation_does_not_borrow_read_cancellation_authority() {
    let mut f = Fixture::new().await;
    let engine_before = f.snapshot().0;
    observe_tool(&mut f, InjectedSyscallEvent::InterruptedBeforeEntry, 0);
    assert!(f.state.sched.lock().unwrap().backend_failed());
    assert_eq!(f.snapshot().0, engine_before);
    assert!(f.thread.original_connect.as_ref().unwrap().invoked);
    assert_no_result(&f);
    f.assert_refused();
}
