/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `GlobalRequest::StoppingRefusal` (A5 design D49) ends the whole run where
//! the scheduler is the init of the guest's PID namespace: the coordinator
//! exits with `HERMIT_POLICY_REFUSAL_EXIT`, and the kernel then kills every
//! process of the namespace, so a parent waiting for the refusing child, and
//! a grandchild that would write a file later, never run on.
//!
//! Its producers (A5 step 3c.1c's restart checkpoint and kernel-run refusals,
//! and step 3c.1e's) file it only under the in-guest delivery gate, which no
//! run sets yet, so the run is played by plain processes: a forked
//! process enters a new user and PID namespace, and its first child, PID 1
//! there, stands for Hermit's container init, where the scheduler runs. It
//! forks a guest parent, which forks the refusing child and waits for it; the
//! child forks the sentinel grandchild, then asks the init to file its
//! refusal, which the init hands to the real `GlobalState::receive_rpc`.
//!
//! Ordering is by events, not delays: the sentinel writes its file only if
//! the init, after `receive_rpc` returned, tells it to go on (one byte on a
//! pipe whose write end only the init holds). An init that exits inside the
//! call never sends it, so the sentinel only ever sees end of file, or is
//! killed first. The mutation it catches: `PolicyRefusal` semantics (the
//! reason recorded and the request answered), under which the init goes on,
//! the sentinel writes its file and the run ends normally.
//!
//! The forking test runs in a process of its own (`in_own_process`). A
//! forked child of a multithreaded process may only do what is safe after a
//! fork, and the init here runs the coordinator's handling of a request,
//! which takes locks. In the library suite, another test's thread can hold
//! such a lock when the init is forked, and the init then waits for it for
//! ever: the whole suite hung on that futex.

use std::os::unix::ffi::OsStrExt;

use reverie::Tool;

use super::*;

/// The refusing child's thread, as the scheduler numbers it.
const CHILD: DetTid = DetTid::from_raw(7);

/// Whether this process runs the test `name` of this module by itself.
///
/// In the library suite this re-executes the test binary for exactly that
/// test and requires that it passed, within two minutes, and returns false.
/// In the process it starts, where the only other thread is libtest's main
/// thread waiting for the test, it returns true and the caller runs the test.
fn in_own_process(name: &str) -> bool {
    const CHILD_TEST: &str = "HERMIT_STOPPING_REFUSAL_TEST";
    let test = format!("tool_global::tests::stopping_refusal_tests::{name}");
    if std::env::var(CHILD_TEST).as_deref() == Ok(test.as_str()) {
        return true;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, &test)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("launch the test in a process of its own");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed; 0 failed;"),
        "test {test} in its own process did not pass exactly once within two minutes: {}\n{stdout}\n{stderr}",
        output.status
    );
    false
}

/// `/proc/PID/ns/pid` as a NUL-terminated path in `buffer`, without
/// allocating (for a forked process).
fn ns_pid_path(pid: libc::pid_t, buffer: &mut [u8; 64]) -> *const libc::c_char {
    let mut digits = [0_u8; 20];
    let mut n = pid as u64;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let mut at = 0;
    for byte in b"/proc/" {
        buffer[at] = *byte;
        at += 1;
    }
    for i in (0..count).rev() {
        buffer[at] = digits[i];
        at += 1;
    }
    for byte in b"/ns/pid\0" {
        buffer[at] = *byte;
        at += 1;
    }
    buffer.as_ptr().cast()
}

/// Forks; the child runs `body` and exits with its status.
///
/// # Safety
///
/// As `fork`: the child must only do what is safe after a fork.
unsafe fn fork_running(body: impl FnOnce() -> i32) -> libc::pid_t {
    match unsafe { libc::fork() } {
        -1 => panic!("fork: {}", std::io::Error::last_os_error()),
        0 => {
            let status = body();
            unsafe { libc::_exit(status) }
        }
        child => child,
    }
}

/// The PID namespace of `pid`, as procfs names it (`pid:[INODE]`).
fn pid_namespace(pid: libc::pid_t) -> Option<std::ffi::OsString> {
    std::fs::read_link(format!("/proc/{pid}/ns/pid"))
        .ok()
        .map(std::path::PathBuf::into_os_string)
}

/// Every process this test can see in the PID namespace `namespace`.
fn members(namespace: &std::ffi::OsStr) -> Vec<String> {
    let mut found = Vec::new();
    crate::util::find_in_directory(std::path::Path::new("/proc"), |name| {
        if name.as_bytes().iter().all(u8::is_ascii_digit)
            && std::fs::read_link(std::path::Path::new("/proc").join(name).join("ns/pid"))
                .is_ok_and(|link| link.as_os_str() == namespace)
        {
            found.push(name.to_string_lossy().into_owned());
        }
        None::<()>
    })
    .unwrap();
    found
}

#[test]
fn a_stopping_refusal_filed_by_a_child_ends_every_process_of_the_run() {
    if !in_own_process("a_stopping_refusal_filed_by_a_child_ends_every_process_of_the_run") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("sentinel");
    let sentinel_path = std::ffi::CString::new(sentinel.as_os_str().as_bytes()).unwrap();
    // Everything that allocates is made before the first fork.
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: true,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let state = GlobalState::initialize(&config, false);
    let tool = <Detcore as Tool>::new(Tid::from_raw(CHILD.as_raw()), &config);
    let child_state = tool.init_thread_state(Tid::from_raw(CHILD.as_raw()), None);
    let request = (
        child_state.thread_logical_time.clone(),
        child_state.mm_id,
        GlobalRequest::StoppingRefusal(
            "hermit refused to continue the run (stopping-refusal-test): the refusing child \
             filed it"
                .to_owned(),
        ),
    );
    // The init reports its namespace and then its own wait status to the
    // test through `report`; the child asks the init to file its refusal
    // through `trigger`; the init tells the sentinel to go on through
    // `proceed`, whose write end only the init keeps, and only once the
    // request was answered.
    let mut report = [0; 2];
    let mut trigger = [0; 2];
    let mut proceed = [0; 2];
    // The forks below are safe only with no other thread that could hold a
    // lock: here libtest's main thread, waiting for this one, is the only one.
    let mut threads = 0;
    crate::util::find_in_directory(std::path::Path::new("/proc/self/task"), |_| {
        threads += 1;
        None::<()>
    })
    .unwrap();
    assert!(threads <= 2, "{threads} threads at the first fork");
    // SAFETY: plain descriptor and process calls. Each forked process uses
    // only what the parent made before the fork and async-signal-safe calls;
    // the init also runs the coordinator's handling of one request, polled
    // without a runtime.
    let launcher = unsafe {
        assert_eq!(libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::pipe2(trigger.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::pipe2(proceed.as_mut_ptr(), libc::O_CLOEXEC), 0);
        fork_running(|| {
            libc::close(report[0]);
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) != 0 {
                return 90;
            }
            let init = fork_running(|| {
                fork_running(|| {
                    // The guest parent, and below it every guest process,
                    // drop the go-on pipe's write end: only the init keeps it.
                    libc::close(proceed[1]);
                    let child = fork_running(|| {
                        let sentinel_process = fork_running(|| {
                            let mut byte = 0_u8;
                            if libc::read(proceed[0], (&mut byte as *mut u8).cast(), 1) == 1 {
                                // The run went on past the refusal.
                                libc::open(
                                    sentinel_path.as_ptr(),
                                    libc::O_CREAT | libc::O_WRONLY,
                                    0o600,
                                );
                            }
                            0
                        });
                        libc::write(trigger[1], [1_u8].as_ptr().cast(), 1);
                        let mut status = 0;
                        libc::waitpid(sentinel_process, &mut status, 0);
                        0
                    });
                    let mut status = 0;
                    libc::waitpid(child, &mut status, 0);
                    0
                });
                // Only the guest processes keep the trigger's write end, so a
                // child that never asks gives end of file, not a hang.
                libc::close(trigger[1]);
                libc::close(proceed[0]);
                let mut byte = 0_u8;
                if libc::read(trigger[0], (&mut byte as *mut u8).cast(), 1) != 1 {
                    return 93;
                }
                let mut call =
                    std::pin::pin!(state.receive_rpc(Tid::from_raw(CHILD.as_raw()), request));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                while std::future::Future::poll(call.as_mut(), &mut context).is_pending() {}
                // Only reached if the refusal was answered: the run goes on,
                // and the sentinel is told so.
                libc::write(proceed[1], [1_u8].as_ptr().cast(), 1);
                libc::close(proceed[1]);
                while libc::waitpid(-1, std::ptr::null_mut(), 0) > 0 {}
                0
            });
            for fd in [trigger[0], trigger[1], proceed[0], proceed[1]] {
                libc::close(fd);
            }
            // The init's namespace, then how it ended.
            let mut link = [0_u8; 64];
            let mut namespace = [0_u8; 64];
            let length = libc::readlink(
                ns_pid_path(init, &mut link),
                namespace.as_mut_ptr().cast(),
                namespace.len(),
            );
            if length <= 0 {
                return 91;
            }
            libc::write(report[1], namespace.as_ptr().cast(), length as usize);
            libc::write(report[1], b"\n".as_ptr().cast(), 1);
            let mut status = 0;
            libc::waitpid(init, &mut status, 0);
            libc::write(report[1], status.to_ne_bytes().as_ptr().cast(), 4);
            0
        })
    };
    // SAFETY: the parent's copies of the pipe ends.
    unsafe {
        for fd in [report[1], trigger[0], trigger[1], proceed[0], proceed[1]] {
            libc::close(fd);
        }
    }
    let mut status = 0;
    // SAFETY: waits for the launcher this test forked.
    assert_eq!(unsafe { libc::waitpid(launcher, &mut status, 0) }, launcher);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the launcher failed: wait status {status:#x} (90: no user and PID namespace)"
    );
    let mut bytes = Vec::new();
    // SAFETY: the read end of `report`, whose writer has exited.
    unsafe {
        let mut buffer = [0_u8; 128];
        loop {
            let n = libc::read(report[0], buffer.as_mut_ptr().cast(), buffer.len());
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n as usize]);
        }
        libc::close(report[0]);
    }
    let newline = bytes.iter().position(|&b| b == b'\n').expect("namespace");
    let namespace = std::ffi::OsStr::from_bytes(&bytes[..newline]).to_owned();
    let init_status = i32::from_ne_bytes(bytes[newline + 1..newline + 5].try_into().unwrap());
    assert!(
        libc::WIFEXITED(init_status)
            && libc::WEXITSTATUS(init_status) == detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
        "the run did not end with the refusal status: wait status {init_status:#x} \
         (sentinel written: {})",
        sentinel.exists()
    );
    assert_ne!(
        Some(namespace.clone()),
        pid_namespace(std::process::id() as i32)
    );
    assert_eq!(
        members(&namespace),
        Vec::<String>::new(),
        "a guest process outlived the refusal"
    );
    // The sentinel writes only if told to go on, which an init that exited
    // inside the call never did; every guest process is gone by now.
    assert!(
        !sentinel.exists(),
        "the sentinel grandchild ran on after the refusal"
    );
}

/// IR-16: with the gate off a filed `StoppingRefusal` is refused the run-wide
/// exit. Through the real `GlobalState::receive_rpc`, from a registered
/// thread (so the handler's bookkeeping after its `match` runs on the
/// answer): the reply is `PolicyRefusal`, the reason is recorded as the
/// run's policy refusal (which ends the run with `HERMIT_POLICY_REFUSAL_EXIT`
/// once it completes), and the process goes on: this test returns. Mutation
/// "the gate check removed": the coordinator exits inside the call, so the
/// test process ends with `HERMIT_POLICY_REFUSAL_EXIT` and the test fails.
#[tokio::test]
async fn with_the_gate_off_a_stopping_refusal_is_recorded_and_answered_without_the_exit() {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: false,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    let state = GlobalState::initialize(&config, false);
    let detpid = DetPid::from_raw(CHILD.as_raw());
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(CHILD, CHILD, true);
    install_test_registration(&state, CHILD, Ivar::new());
    let reason = "hermit refused to continue the run (stopping-refusal-test): filed with the \
                  gate off"
        .to_owned();
    let (_, reply) = state
        .receive_rpc(
            Tid::from_raw(CHILD.as_raw()),
            (
                DetTime::new(&config),
                MmId::initial(detpid),
                GlobalRequest::StoppingRefusal(reason.clone()),
            ),
        )
        .await;
    assert_eq!(reply, GlobalResponse::PolicyRefusal(()));
    assert_eq!(state.policy_refusal(), Some(reason));
}

/// A self-sentinel `pidfd_send_signal` refusal filed with the coordinator is
/// answered as a policy refusal, recorded as the run's policy refusal (what
/// the in-guest launcher reads), and kept as its type for the DBT launcher,
/// which takes it once. The first filed refusal is the one kept.
#[tokio::test]
async fn a_filed_pidfd_self_refusal_is_recorded_as_its_type_and_as_a_policy_refusal() {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: false,
        ..Config::default()
    };
    let state = GlobalState::initialize(&config, false);
    let detpid = DetPid::from_raw(CHILD.as_raw());
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(CHILD, CHILD, true);
    install_test_registration(&state, CHILD, Ivar::new());
    let first = crate::PidfdSelfQualificationRefusal { sentinel: -10001 };
    for sentinel in [-10001, -10000] {
        let (_, reply) = state
            .receive_rpc(
                Tid::from_raw(CHILD.as_raw()),
                (
                    DetTime::new(&config),
                    MmId::initial(detpid),
                    GlobalRequest::PidfdSelfQualificationRefusal(
                        crate::PidfdSelfQualificationRefusal { sentinel },
                    ),
                ),
            )
            .await;
        assert_eq!(reply, GlobalResponse::PolicyRefusal(()));
    }
    assert_eq!(state.policy_refusal(), Some(first.to_string()));
    assert_eq!(state.take_pidfd_self_qualification_refusal(), Some(first));
    assert_eq!(state.take_pidfd_self_qualification_refusal(), None);
}

/// How a request filed with a coordinator that runs as PID 1 of a new PID
/// namespace ended ([`in_namespace_init`]).
struct InitOutcome {
    /// The init's wait status.
    status: i32,
    /// Whether `receive_rpc` returned `PolicyRefusal` (`Some(true)`), another
    /// reply (`Some(false)`), or never returned (`None`).
    answered_policy_refusal: Option<bool>,
    /// The processes of the namespace still alive after the init ended.
    survivors: Vec<String>,
}

/// Runs `request` through the real `GlobalState::receive_rpc` in the init of
/// a new user and PID namespace, as Hermit's container init runs the
/// scheduler. The init forks a guest that waits for ever, initializes the
/// global state from `config` itself, as PID 1, registers the refusing
/// thread, and files the request. A coordinator that exits inside the call
/// takes the guest with it (`zap_pid_ns_processes`); one that answers kills
/// the guest and exits 0. Call it from a test that runs in a process of its
/// own (`in_own_process`).
fn in_namespace_init(config: Config, request: GlobalRequest) -> InitOutcome {
    let mut report = [0; 2];
    let mut threads = 0;
    crate::util::find_in_directory(std::path::Path::new("/proc/self/task"), |_| {
        threads += 1;
        None::<()>
    })
    .unwrap();
    assert!(threads <= 2, "{threads} threads at the first fork");
    // SAFETY: plain descriptor and process calls. The forked init builds
    // and uses the global state while the only other thread of this process,
    // libtest's main thread, waits for this test, holding no lock it takes.
    let launcher = unsafe {
        assert_eq!(libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC), 0);
        fork_running(|| {
            libc::close(report[0]);
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) != 0 {
                return 90;
            }
            let init = fork_running(|| {
                let guest = fork_running(|| {
                    loop {
                        libc::pause();
                    }
                });
                assert_eq!(libc::getpid(), 1);
                let state = GlobalState::initialize(&config, false);
                let detpid = DetPid::from_raw(CHILD.as_raw());
                state
                    .sched
                    .lock()
                    .unwrap()
                    .thread_tree
                    .add_child(CHILD, CHILD, true);
                install_test_registration(&state, CHILD, Ivar::new());
                let mut call = std::pin::pin!(state.receive_rpc(
                    Tid::from_raw(CHILD.as_raw()),
                    (DetTime::new(&config), MmId::initial(detpid), request),
                ));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                let reply = loop {
                    if let std::task::Poll::Ready((_, reply)) =
                        std::future::Future::poll(call.as_mut(), &mut context)
                    {
                        break reply;
                    }
                };
                let tag = if reply == GlobalResponse::PolicyRefusal(()) {
                    b'P'
                } else {
                    b'O'
                };
                libc::write(report[1], [tag].as_ptr().cast(), 1);
                libc::kill(guest, libc::SIGKILL);
                libc::waitpid(guest, std::ptr::null_mut(), 0);
                0
            });
            // The init's namespace, then how it ended.
            let mut link = [0_u8; 64];
            let mut namespace = [0_u8; 64];
            let length = libc::readlink(
                ns_pid_path(init, &mut link),
                namespace.as_mut_ptr().cast(),
                namespace.len(),
            );
            if length <= 0 {
                return 91;
            }
            let mut status = 0;
            libc::waitpid(init, &mut status, 0);
            libc::write(report[1], b"\n".as_ptr().cast(), 1);
            libc::write(report[1], namespace.as_ptr().cast(), length as usize);
            libc::write(report[1], b"\n".as_ptr().cast(), 1);
            libc::write(report[1], status.to_ne_bytes().as_ptr().cast(), 4);
            0
        })
    };
    // SAFETY: the parent's copy of the write end.
    unsafe { libc::close(report[1]) };
    let mut status = 0;
    // SAFETY: waits for the launcher this test forked.
    assert_eq!(unsafe { libc::waitpid(launcher, &mut status, 0) }, launcher);
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "the launcher failed: wait status {status:#x} (90: no user and PID namespace)"
    );
    let mut bytes = Vec::new();
    // SAFETY: the read end of `report`, whose writers have exited.
    unsafe {
        let mut buffer = [0_u8; 128];
        loop {
            let n = libc::read(report[0], buffer.as_mut_ptr().cast(), buffer.len());
            if n <= 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..n as usize]);
        }
        libc::close(report[0]);
    }
    // [tag] "\n" namespace "\n" status
    let first = bytes.iter().position(|&b| b == b'\n').expect("report");
    let answered_policy_refusal = match &bytes[..first] {
        [] => None,
        [tag] => Some(*tag == b'P'),
        other => panic!("a malformed report: {other:?}"),
    };
    let rest = &bytes[first + 1..];
    let second = rest.iter().position(|&b| b == b'\n').expect("namespace");
    let namespace = std::ffi::OsStr::from_bytes(&rest[..second]).to_owned();
    let status = i32::from_ne_bytes(rest[second + 1..second + 5].try_into().unwrap());
    assert_ne!(
        Some(namespace.clone()),
        pid_namespace(std::process::id() as i32)
    );
    InitOutcome {
        status,
        answered_policy_refusal,
        survivors: members(&namespace),
    }
}

/// A gate-off configuration on the default in-guest route (`route`: whether
/// Hermit's launch marked it, `Config::in_guest_refuses_stop_sends`).
fn stop_send_config(route: bool) -> Config {
    let mut config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        in_guest_signal_delivery: false,
        in_guest_refuses_stop_sends: route,
        ..Config::default()
    };
    config.backend = reverie::BackendCapabilities::LITEINST_IN_GUEST;
    config
}

fn stop_send_refusal() -> GlobalRequest {
    GlobalRequest::StopSendRefusal(
        "HERMIT_STOP_SIGNAL_SEND_REFUSED signum=19 (SIGSTOP) call=kill sender=7 target=8: \
         filed by the stop-send test"
            .to_owned(),
    )
}

/// https://github.com/rrnewton/hermit/issues/5430: on the default in-guest
/// route, with the delivery gate off, a coordinator that is PID 1 of the
/// guests' namespace, and initialized its global state there, exits on a
/// filed `StopSendRefusal` with `HERMIT_POLICY_REFUSAL_EXIT`, never answers
/// it, and leaves no guest process alive. Mutation: the request routed
/// through the generic gate-off arm (recorded and answered, no exit).
#[test]
fn an_owning_coordinator_ends_the_run_on_a_stop_send_refusal() {
    if !in_own_process("an_owning_coordinator_ends_the_run_on_a_stop_send_refusal") {
        return;
    }
    let outcome = in_namespace_init(stop_send_config(true), stop_send_refusal());
    assert!(
        libc::WIFEXITED(outcome.status)
            && libc::WEXITSTATUS(outcome.status) == detcore_model::HERMIT_POLICY_REFUSAL_EXIT,
        "the coordinator did not exit with the refusal status: wait status {:#x}, answered {:?}",
        outcome.status,
        outcome.answered_policy_refusal
    );
    assert_eq!(outcome.answered_policy_refusal, None);
    assert_eq!(
        outcome.survivors,
        Vec::<String>::new(),
        "a guest process outlived the refusal"
    );
}

/// The run-wide exit belongs to the stop-send refusal alone: a coordinator
/// that owns the guests' namespace (PID 1, route marked, gate off) still
/// records and answers an ordinary `StoppingRefusal`, as IR-16 rules
/// (https://github.com/rrnewton/hermit/issues/5430). Mutation: the generic
/// arm given the ownership exit.
#[test]
fn an_owning_coordinator_answers_an_ordinary_stopping_refusal_without_the_exit() {
    if !in_own_process(
        "an_owning_coordinator_answers_an_ordinary_stopping_refusal_without_the_exit",
    ) {
        return;
    }
    let outcome = in_namespace_init(
        stop_send_config(true),
        GlobalRequest::StoppingRefusal(
            "hermit refused to continue the run (stopping-refusal-test): an ordinary refusal"
                .to_owned(),
        ),
    );
    assert!(
        libc::WIFEXITED(outcome.status) && libc::WEXITSTATUS(outcome.status) == 0,
        "wait status {:#x}",
        outcome.status
    );
    assert_eq!(outcome.answered_policy_refusal, Some(true));
}

/// A coordinator that is PID 1 but whose run Hermit did not mark as the
/// default in-guest route (a ptrace launch, for one) does not own the
/// guests for this refusal: a filed `StopSendRefusal` is recorded and
/// answered, without the exit (https://github.com/rrnewton/hermit/issues/5430).
/// Mutation: ownership taken from PID 1 alone.
#[test]
fn a_pid_1_coordinator_off_the_route_answers_a_stop_send_refusal_without_the_exit() {
    if !in_own_process(
        "a_pid_1_coordinator_off_the_route_answers_a_stop_send_refusal_without_the_exit",
    ) {
        return;
    }
    let outcome = in_namespace_init(stop_send_config(false), stop_send_refusal());
    assert!(
        libc::WIFEXITED(outcome.status) && libc::WEXITSTATUS(outcome.status) == 0,
        "wait status {:#x}",
        outcome.status
    );
    assert_eq!(outcome.answered_policy_refusal, Some(true));
}

/// A coordinator on the default in-guest route that is not PID 1 (here a
/// test process; `hermit run --no-namespace` is another) does not own the
/// guests: a filed `StopSendRefusal` is recorded as the run's policy refusal,
/// which ends the run with `HERMIT_POLICY_REFUSAL_EXIT` once it completes,
/// and answered, and the process goes on: this test returns
/// (https://github.com/rrnewton/hermit/issues/5430). Mutation: the exit taken
/// without ownership (this test process ends with the refusal status).
#[tokio::test]
async fn a_coordinator_that_is_not_pid_1_records_and_answers_a_stop_send_refusal() {
    assert_ne!(std::process::id(), 1);
    let config = stop_send_config(true);
    let state = GlobalState::initialize(&config, false);
    let detpid = DetPid::from_raw(CHILD.as_raw());
    state
        .sched
        .lock()
        .unwrap()
        .thread_tree
        .add_child(CHILD, CHILD, true);
    install_test_registration(&state, CHILD, Ivar::new());
    let request = stop_send_refusal();
    let GlobalRequest::StopSendRefusal(reason) = request.clone() else {
        unreachable!()
    };
    let (_, reply) = state
        .receive_rpc(
            Tid::from_raw(CHILD.as_raw()),
            (DetTime::new(&config), MmId::initial(detpid), request),
        )
        .await;
    assert_eq!(reply, GlobalResponse::PolicyRefusal(()));
    assert_eq!(state.policy_refusal(), Some(reason));
}
