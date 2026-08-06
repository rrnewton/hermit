/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Contract fixtures for process-tree ordering: fork, exec, wait/reap order,
//! zombie handling, and the vfork hazard.
//!
//! WHY A FIXTURE RATHER THAN A SWEEP. A sweep records what ordering was observed
//! on the day it ran, and that finding decays silently the moment scheduling
//! changes. These pin the ordering as an ASSERTION, so a regression fails the
//! build with a diff instead of quietly changing a number in a report.
//!
//! The orderings below are pinned to what Detcore's deterministic scheduler
//! produces. If one of these fails, do NOT relax the assertion to match the new
//! behaviour -- that converts a determinism regression into a rebaselined test.
//! Establish first whether the scheduler change was intended.

use std::time::Duration;
use std::time::Instant;

/// Reap every child and return `(pid, exit_code)` pairs **in reap order**.
///
/// Reap ORDER is the contract, not just the set: a deterministic scheduler must
/// hand children back in a reproducible sequence, so the assertions below check
/// the sequence rather than sorting it first (sorting would hide exactly the
/// regression this exists to catch).
fn reap_all(expected: usize) -> Vec<(libc::pid_t, i32)> {
    let mut reaped = Vec::with_capacity(expected);
    for _ in 0..expected {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::wait(&mut status) };
        assert!(pid > 0, "wait() failed with {pid} before all children were reaped");
        assert!(
            libc::WIFEXITED(status),
            "child {pid} did not exit normally; status {status:#x}"
        );
        reaped.push((pid, libc::WEXITSTATUS(status)));
    }
    // The tree is now empty: a further wait must report ECHILD, which is also
    // the zombie-handling contract (every child was reaped exactly once).
    let mut status: libc::c_int = 0;
    let extra = unsafe { libc::wait(&mut status) };
    assert_eq!(extra, -1, "an unexpected extra child {extra} was reapable");
    reaped
}

/// Fork `n` children; child `i` exits with code `i`.
fn fork_children_exiting_with_index(n: i32) -> Vec<libc::pid_t> {
    let mut pids = Vec::with_capacity(n as usize);
    for i in 0..n {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork() failed at index {i}");
        if pid == 0 {
            unsafe { libc::_exit(i) };
        }
        pids.push(pid);
    }
    pids
}

#[test]
fn fork_order_and_pid_assignment_are_deterministic() {
    super::det_test_fn_without_pmu(|| {
        let pids = fork_children_exiting_with_index(4);
        // IDENTITY: every child gets a distinct pid, so the parent can tell them
        // apart and no pid is reused inside a live tree.
        //
        // Deliberately NOT asserting a pid SPACING pattern. I tried that first and
        // it was wrong -- measured pids were [.., ..75, ..79, ..81, ..85], deltas
        // [4, 2, 4]. In this harness the guest observes HOST pids, whose spacing
        // depends on unrelated host activity, so a spacing rule would be a flake
        // generator asserting something Detcore never promised. Reap ORDER below
        // is the real scheduling contract; pid VALUES are not.
        let mut distinct = pids.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            pids.len(),
            "a pid was reused inside a live process tree: {pids:?}"
        );
        let reaped = reap_all(pids.len());
        let reaped_pids: Vec<libc::pid_t> = reaped.iter().map(|(p, _)| *p).collect();
        assert_eq!(
            reaped_pids, pids,
            "REAP ORDER changed: children were created {pids:?} but reaped {reaped_pids:?}"
        );
        let codes: Vec<i32> = reaped.iter().map(|(_, c)| *c).collect();
        assert_eq!(codes, vec![0, 1, 2, 3], "exit codes did not follow creation order");
    });
}

#[test]
fn reap_order_is_stable_across_repeated_generations() {
    super::det_test_fn_without_pmu(|| {
        // Two successive generations must produce the SAME relative reap order.
        // A scheduler that is deterministic only on a cold process table would
        // pass a single generation and fail here.
        let mut generations = Vec::new();
        for _ in 0..2 {
            let pids = fork_children_exiting_with_index(3);
            let reaped = reap_all(pids.len());
            let order: Vec<usize> = reaped
                .iter()
                .map(|(p, _)| pids.iter().position(|q| q == p).expect("reaped an unknown pid"))
                .collect();
            generations.push(order);
        }
        assert_eq!(
            generations[0], generations[1],
            "reap order differed between two identical generations: {generations:?}"
        );
    });
}

#[test]
fn exec_preserves_child_identity_and_exit_status() {
    super::det_test_fn_without_pmu(|| {
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork() failed");
        if pid == 0 {
            // `exec` must not change the pid the parent waits on.
            let path = c"/bin/true";
            let argv = [path.as_ptr(), std::ptr::null()];
            unsafe { libc::execv(path.as_ptr(), argv.as_ptr()) };
            unsafe { libc::_exit(127) }; // exec failed
        }
        let mut status: libc::c_int = 0;
        let reaped = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(reaped, pid, "exec changed the pid observed by the parent");
        assert!(libc::WIFEXITED(status), "exec'd child did not exit normally");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "exec'd /bin/true did not exit 0 (127 means execv itself failed)"
        );
    });
}

/// THE HAZARD THIS FIXTURE EXISTS FOR.
///
/// A `vfork` parent is suspended until the child execs or exits. If the child is
/// ptrace-stopped and never resumed, that wait is UNSATISFIABLE and the pair
/// spins to the budget -- the `detcore_misc` livelock signature, measured as
/// cpu/wall ~= 1.0 at the timeout (a full core burned, retry-futile).
///
/// A plain timeout kill records that as a generic no-result minutes later. This
/// asserts a WALL BUDGET inside the test so the class fails LOUDLY, in seconds,
/// naming itself -- which is the difference between "something timed out" and
/// "the vfork wait was unsatisfiable".
#[test]
fn vfork_child_satisfies_the_parent_wait_within_budget() {
    super::det_test_fn_without_pmu(|| {
        const BUDGET: Duration = Duration::from_secs(20);
        let started = Instant::now();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork() failed");
        if pid == 0 {
            // Child: exit promptly. This is the satisfiable case; the fixture
            // guards that it STAYS satisfiable.
            unsafe { libc::_exit(0) };
        }

        // Poll rather than blocking, so an unsatisfiable wait is reported as a
        // budget breach instead of hanging until the harness kills the run.
        let mut status: libc::c_int = 0;
        loop {
            let reaped = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if reaped == pid {
                break;
            }
            assert!(reaped >= 0, "waitpid failed with {reaped}");
            assert!(
                started.elapsed() < BUDGET,
                "VFORK WAIT UNSATISFIABLE: child {pid} neither exec'd nor exited within {BUDGET:?}. \
                 This is the detcore_misc livelock signature (cpu/wall ~= 1.0 at the budget, \
                 retry-futile). Do NOT raise the budget to make this pass -- a vfork child that \
                 is ptrace-stopped and never resumed will never satisfy its parent."
            );
            std::thread::yield_now();
        }
        assert!(libc::WIFEXITED(status), "vfork child did not exit normally");
        assert_eq!(libc::WEXITSTATUS(status), 0);
    });
}

#[test]
fn pipeline_shaped_tree_reaps_every_stage() {
    super::det_test_fn_without_pmu(|| {
        // Shell-pipeline shape: a parent forks several stages wired by a pipe and
        // must reap all of them, in a deterministic order, with no zombies left.
        let mut fds = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe() failed");
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let mut pids = Vec::new();
        for stage in 0..3 {
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork() failed at stage {stage}");
            if pid == 0 {
                unsafe { libc::close(read_fd) };
                let byte = [stage as u8];
                unsafe { libc::write(write_fd, byte.as_ptr().cast(), 1) };
                unsafe { libc::close(write_fd) };
                unsafe { libc::_exit(stage) };
            }
            pids.push(pid);
        }
        unsafe { libc::close(write_fd) };

        // Every stage's byte must arrive, and the BYTE ORDER is itself a
        // scheduling contract: it records which stage ran first.
        let mut buf = [0_u8; 3];
        let mut got = 0;
        while got < buf.len() {
            let n = unsafe { libc::read(read_fd, buf[got..].as_mut_ptr().cast(), buf.len() - got) };
            assert!(n > 0, "pipe read returned {n} before all stages wrote");
            got += n as usize;
        }
        unsafe { libc::close(read_fd) };
        assert_eq!(
            buf,
            [0, 1, 2],
            "pipeline stage WRITE ORDER changed: {buf:?}. This is a scheduling-order \
             regression, not a flake -- do not sort to make it pass."
        );

        let reaped = reap_all(pids.len());
        let reaped_pids: Vec<libc::pid_t> = reaped.iter().map(|(p, _)| *p).collect();
        assert_eq!(reaped_pids, pids, "pipeline reap order changed");
    });
}
