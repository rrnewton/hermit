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
//! Nothing in Hermit files the request yet (its producers come with A5 steps
//! 3c.1c and 3c.1e), so the run is played by plain processes: a forked
//! process enters a new user and PID namespace, and its first child, PID 1
//! there, stands for Hermit's container init, where the scheduler runs. It
//! forks a guest parent, which forks the refusing child and waits for it; the
//! child forks the sentinel grandchild, then asks the init to file its
//! refusal, which the init hands to the real `GlobalState::receive_rpc`.
//!
//! The mutation it catches: `PolicyRefusal` semantics (the reason recorded
//! and the request answered), under which the init lives on, the sentinel
//! writes its file and the run ends normally.

use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

use reverie::Tool;

use super::*;

/// How long the sentinel grandchild waits before it writes its file, and
/// the refusing child before it exits: far longer than the refusal takes.
const SENTINEL_DELAY: Duration = Duration::from_millis(500);
const CHILD_LIFETIME: Duration = Duration::from_millis(1_000);

/// The refusing child's thread, as the scheduler numbers it.
const CHILD: DetTid = DetTid::from_raw(7);

fn sleep(duration: Duration) {
    let request = libc::timespec {
        tv_sec: duration.as_secs() as libc::time_t,
        tv_nsec: duration.subsec_nanos() as libc::c_long,
    };
    // SAFETY: a valid timespec and no remainder.
    unsafe { libc::nanosleep(&request, std::ptr::null_mut()) };
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
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().as_bytes().iter().all(u8::is_ascii_digit))
        .filter(|entry| {
            std::fs::read_link(entry.path().join("ns/pid"))
                .is_ok_and(|link| link.as_os_str() == namespace)
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn a_stopping_refusal_filed_by_a_child_ends_every_process_of_the_run() {
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
    // through `trigger`.
    let mut report = [0; 2];
    let mut trigger = [0; 2];
    // SAFETY: plain descriptor and process calls. Each forked process uses
    // only what the parent made before the fork, async-signal-safe calls,
    // and, in the init, the coordinator's handling of one request.
    let launcher = unsafe {
        assert_eq!(libc::pipe2(report.as_mut_ptr(), libc::O_CLOEXEC), 0);
        assert_eq!(libc::pipe2(trigger.as_mut_ptr(), libc::O_CLOEXEC), 0);
        fork_running(|| {
            libc::close(report[0]);
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWPID) != 0 {
                return 90;
            }
            let init = fork_running(|| {
                let parent = fork_running(|| {
                    let child = fork_running(|| {
                        fork_running(|| {
                            sleep(SENTINEL_DELAY);
                            let file = libc::open(
                                sentinel_path.as_ptr(),
                                libc::O_CREAT | libc::O_WRONLY,
                                0o600,
                            );
                            if file >= 0 { 0 } else { 92 }
                        });
                        libc::write(trigger[1], [1_u8].as_ptr().cast(), 1);
                        sleep(CHILD_LIFETIME);
                        0
                    });
                    let mut status = 0;
                    libc::waitpid(child, &mut status, 0);
                    0
                });
                let _ = parent;
                // Only the guest processes keep the write end, so a child that
                // never asks gives end of file, not a hang.
                libc::close(trigger[1]);
                let mut byte = 0_u8;
                if libc::read(trigger[0], (&mut byte as *mut u8).cast(), 1) != 1 {
                    return 93;
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                let response =
                    runtime.block_on(state.receive_rpc(Tid::from_raw(CHILD.as_raw()), request));
                // Only reached if the refusal was answered: the run goes on
                // until every guest process has ended.
                let _ = response;
                while libc::waitpid(-1, std::ptr::null_mut(), 0) > 0 {}
                0
            });
            libc::close(trigger[0]);
            libc::close(trigger[1]);
            // The init's namespace, then how it ended.
            let link = format!("/proc/{init}/ns/pid\0");
            let mut namespace = [0_u8; 64];
            let length = libc::readlink(
                link.as_ptr().cast(),
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
        libc::close(report[1]);
        libc::close(trigger[0]);
        libc::close(trigger[1]);
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
    // Past the sentinel's own delay, it never wrote its file.
    sleep(SENTINEL_DELAY * 2);
    assert!(
        !sentinel.exists(),
        "the sentinel grandchild ran on after the refusal"
    );
}
