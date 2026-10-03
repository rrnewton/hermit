// Native PIDFDs and actual custody/exec-binding consumers. Delivery of the
// leader initial-EXEC callback and the consumed logical exec receipt are
// controlled premises here; the unchanged official cell covers composition.
use super::*;
use crate::network_replay::NetworkStreamOwner;
use crate::scheduler::Scheduler;
use crate::types::DetTid;
use crate::types::ExecFilesReceipt;
use crate::types::FilesId;
use crate::types::FilesIdAllocator;
use crate::types::MmId;

fn root_runtime(
    control: Arc<AdmissionControl>,
) -> (
    NetworkRuntimeOwner,
    NetworkRuntimeResources,
    Scheduler,
    NetworkStreamOwner,
) {
    // A leader is required by bind_initial_exec, unlike the existing admission
    // tests that also cover enrolling a nonleader test-harness thread.
    let thread = DetTid::from_raw(std::process::id() as i32);
    let owner = NetworkStreamOwner {
        thread,
        mm: MmId::initial(thread),
    };
    let (custody, runtime) = unsafe {
        NetworkRuntimeResources::from_authenticated_guard(
            control,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
    };
    let mut scheduler = Scheduler::new(&crate::Config::default());
    register(&runtime, &mut scheduler, owner);
    (custody, runtime, scheduler, owner)
}

fn register(
    runtime: &NetworkRuntimeResources,
    scheduler: &mut Scheduler,
    owner: NetworkStreamOwner,
) {
    let pin = scheduler
        .register_stopped_ptrace_thread(owner, owner.thread.as_raw(), owner.thread.as_raw())
        .unwrap();
    runtime.register_ptrace_task(pin).unwrap();
}

fn receipt(owner: NetworkStreamOwner) -> ExecFilesReceipt {
    ExecFilesReceipt {
        caller: owner.thread,
        process: owner.thread,
        mm: owner.mm,
        old_files: FilesId::initial(owner.thread),
        new_files: FilesIdAllocator::default().allocate_exec(owner.thread),
    }
}

fn after_exec(owner: NetworkStreamOwner) -> NetworkStreamOwner {
    NetworkStreamOwner {
        mm: owner.mm.for_exec(owner.thread),
        ..owner
    }
}

#[test]
fn enrolled_root_survives_exact_initial_exec_without_reenrollment_or_new_observer() {
    let control = Arc::new(AdmissionControl::default());
    let (_custody, runtime, mut scheduler, before) = root_runtime(control.clone());
    runtime.register_guard_initial(before).unwrap();
    let deadline = {
        let retained = runtime.shared.guard.lock().unwrap();
        let guard = retained.as_ref().unwrap();
        guard.deadline
    };
    let after = after_exec(before);
    register(&runtime, &mut scheduler, after);
    // An MM advance and even the same actual PIDFD are insufficient.
    assert!(runtime.register_guard_initial(after).is_err());
    runtime.bind_initial_exec(after, receipt(before)).unwrap();
    runtime.register_guard_initial(after).unwrap();
    runtime.register_guard_initial(after).unwrap();
    assert!(runtime.register_guard_initial(before).is_err());
    assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
    {
        let retained = runtime.shared.guard.lock().unwrap();
        let guard = retained.as_ref().unwrap();
        assert_eq!(guard.deadline, deadline);
        assert_eq!(guard.initial.as_ref().unwrap().owner, before);
        assert!(guard.initial.as_ref().unwrap().result.is_ok());
        assert!(guard.observer.as_ref().unwrap().1.is_ok());
    }
}

#[test]
fn guard_exec_refuses_missing_foreign_wrong_step_and_second_exec_receipts() {
    let control = Arc::new(AdmissionControl::default());
    let (_custody, runtime, mut scheduler, before) = root_runtime(control.clone());
    runtime.register_guard_initial(before).unwrap();
    let after = after_exec(before);
    register(&runtime, &mut scheduler, after);
    let exact = receipt(before);
    let foreign = DetTid::from_raw(before.thread.as_raw() + 1);
    for changed in [
        ExecFilesReceipt {
            caller: foreign,
            ..exact
        },
        ExecFilesReceipt {
            process: foreign,
            ..exact
        },
        ExecFilesReceipt {
            mm: after.mm,
            ..exact
        },
        ExecFilesReceipt {
            new_files: exact.old_files,
            ..exact
        },
    ] {
        assert!(runtime.bind_initial_exec(after, changed).is_err());
        assert!(runtime.register_guard_initial(after).is_err());
    }
    runtime.bind_initial_exec(after, exact).unwrap();
    runtime.register_guard_initial(after).unwrap();
    let second = after_exec(after);
    register(&runtime, &mut scheduler, second);
    runtime.bind_initial_exec(second, receipt(after)).unwrap();
    assert!(runtime.register_guard_initial(second).is_err());
    assert!(runtime.register_guard_initial(after).is_err());
    assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
}

#[test]
fn guard_exec_keeps_original_enrollment_and_observer_failures_sticky() {
    for fail_register in [true, false] {
        let control = Arc::new(AdmissionControl {
            fail_register,
            fail_observer: !fail_register,
            ..Default::default()
        });
        let (_custody, runtime, mut scheduler, before) = root_runtime(control.clone());
        let failure = runtime
            .register_guard_initial(before)
            .unwrap_err()
            .to_string();
        let after = after_exec(before);
        register(&runtime, &mut scheduler, after);
        runtime.bind_initial_exec(after, receipt(before)).unwrap();
        for _ in 0..2 {
            assert_eq!(
                runtime
                    .register_guard_initial(after)
                    .unwrap_err()
                    .to_string(),
                failure
            );
        }
        let expected = if fail_register {
            vec!["register"]
        } else {
            vec!["register", "observer"]
        };
        assert_eq!(*control.calls.lock().unwrap(), expected);
    }
}

#[test]
fn guard_exec_refuses_a_different_native_pidfd_despite_equal_claimed_numbers() {
    let control = Arc::new(AdmissionControl::default());
    let (_custody, runtime, mut scheduler, before) = root_runtime(control.clone());
    runtime.register_guard_initial(before).unwrap();
    let after = after_exec(before);
    register(&runtime, &mut scheduler, after);
    let (ready, other_tid) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        ready
            .send(unsafe { libc::syscall(libc::SYS_gettid) as i32 })
            .unwrap();
        let _ = wait.recv();
    });
    let other = other_tid
        .recv_timeout(std::time::Duration::from_secs(1))
        .unwrap();
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, other, libc::O_EXCL) };
    assert!(raw >= 0);
    let other_pin = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    {
        let mut physical = runtime.shared.physical.lock().unwrap();
        physical.forget(after);
        // Deliberately malformed private custody fixture: a valid *different*
        // PIDFD paired with identical claimed process/thread/MM fields.
        physical
            .register(
                after,
                before.thread.as_raw(),
                before.thread.as_raw(),
                || Ok(other_pin),
            )
            .unwrap();
    }
    runtime.bind_initial_exec(after, receipt(before)).unwrap();
    assert!(runtime.register_guard_initial(after).is_err());
    assert_eq!(*control.calls.lock().unwrap(), ["register", "observer"]);
    release.send(()).unwrap();
    worker.join().unwrap();
}
