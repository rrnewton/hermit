/* SPDX-License-Identifier: BSD-3-Clause */
// Offline supervisor components. The Python children below exercise actual
// execute_role_mutants/execute_stage/OwnedChild, not the C coverage predicates.
// The explicitly modeled closure tests supply no kernel/provider evidence.
use super::*;
use super::driver_ftrace_process::LIMITS;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Case {
    path: PathBuf,
    name: String,
}
impl Case {
    fn new() -> Self {
        let name = format!("role-batch-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let path = root.join(&name);
        fs::create_dir(&path).unwrap();
        fs::write(path.join("stdout"), b"retained stdout\n").unwrap();
        fs::write(path.join("stderr"), b"retained stderr\n").unwrap();
        Self { path, name }
    }

    fn save(&self, results: &[(u32, Value)], observation: &Result<BTreeMap<u32, Value>>) {
        fs::write(self.path.join("result.json"), serde_json::to_vec_pretty(&json!({
            "component":"actual role batch supervision, not original C or native provider",
            "receipts":results,
            "rendezvous":observation.as_ref().ok(),
            "rendezvous_error":observation.as_ref().err().map(|e| format!("{e:#}")),
        })).unwrap()).unwrap();
    }

    fn run_live(&self, last: u32, failure: u32, bulk: bool) -> (Vec<(u32, Value)>, Result<BTreeMap<u32, Value>>) {
        // Abstract local socket avoids depending on the evidence directory's
        // length. It is a rendezvous only, never a child wait/signal authority.
        let address = SocketAddr::from_abstract_name(self.name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let executable = self.path.join("role-control");
        let script = format!(r#"#!/usr/bin/python3
import faulthandler, json, os, socket
faulthandler.disable()
role = int(os.environ["AP_FTRACE_MUTATE_ROLE"])
peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
peer.connect(b"\0" + {name}.encode())
peer.sendall(json.dumps([role, os.getpid(), os.getpgrp(), os.getsid(0)]).encode() + b"\n")
if peer.recv(1) != b"!":
    os._exit(91)
if {bulk}:
    data = b"x" * (1024 * 1024 + 1)
    while data:
        written = os.write(1, data)
        assert written > 0
        data = data[written:]
else:
    os.write(1, ("role:%d\n" % role).encode())
    os.write(2, ("role:%d\n" % role).encode())
if role == {failure}:
    os._exit(7)
os.abort()
"#, name=serde_json::to_string(&self.name).unwrap(), bulk=if bulk { "True" } else { "False" });
        fs::write(&executable, script).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = self.path.join("stdout");
        let stderr = self.path.join("stderr");
        let started = Instant::now();
        let deadline = started + Duration::from_secs(1);
        let (results, observation) = std::thread::scope(|scope| {
            let worker = scope.spawn(|| execute_role_mutants(&executable, 1..=last, started, &stdout, &stderr));
            // No child is released until all four have reported while blocked.
            // On error, dropping listener/connections releases every accepted
            // or queued peer before joining the unchanged bounded supervisors.
            let observation = rendezvous(listener, deadline);
            let results = worker.join().unwrap();
            (results, observation)
        });
        self.save(&results, &observation);
        (results, observation)
    }
}

fn rendezvous(listener: UnixListener, deadline: Instant) -> Result<BTreeMap<u32, Value>> {
    let mut peers = BTreeMap::<u32, (UnixStream, Value)>::new();
    for _ in 0..4 {
        let remaining = deadline.checked_duration_since(Instant::now())
            .context("four live roles did not rendezvous within the original one-second component bound")?;
        let mut pollfd = libc::pollfd { fd:listener.as_raw_fd(), events:libc::POLLIN, revents:0 };
        let ready = unsafe { libc::poll(&mut pollfd, 1, remaining.as_millis().max(1) as i32) };
        ensure!(ready == 1 && pollfd.revents == libc::POLLIN && Instant::now() < deadline,
            "four simultaneously blocked roles required; readiness={ready}, events={}", pollfd.revents);
        let (stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(deadline.checked_duration_since(Instant::now()).context("rendezvous deadline")?))?;
        stream.set_write_timeout(Some(deadline.checked_duration_since(Instant::now()).context("rendezvous deadline")?))?;
        let mut reader = BufReader::new(stream);
        let mut raw = Vec::new();
        // Each header is small and peer-controlled. No unbounded read_to_end.
        reader.by_ref().take(256).read_until(b'\n', &mut raw)?;
        ensure!(raw.last() == Some(&b'\n'), "bounded role header missing newline");
        let header: Value = serde_json::from_slice(&raw)?;
        let role = header[0].as_u64().context("role")? as u32;
        ensure!((1..=4).contains(&role) && header.as_array().is_some_and(|a| a.len() == 4), "unexpected role header");
        ensure!(header[1].as_u64().is_some_and(|pid| pid > 0) &&
            header[1] == header[2] && header[1] == header[3], "each actual child needs its own session/group");
        ensure!(peers.insert(role, (reader.into_inner(), header)).is_none(), "duplicate role environment");
    }
    ensure!(peers.keys().copied().collect::<Vec<_>>() == [1, 2, 3, 4], "missing simultaneous role");
    let pids: std::collections::BTreeSet<_> = peers.values().map(|(_, h)| h[1].as_u64().unwrap()).collect();
    ensure!(pids.len() == 4, "four distinct live children required");
    // Reverse release does not imply kernel terminal/completion order. Returned
    // receipts must nevertheless remain canonical; the model below forces
    // reverse worker completion separately.
    for (stream, _) in peers.values_mut().rev() {
        ensure!(Instant::now() < deadline, "original rendezvous deadline");
        stream.write_all(b"!")?;
    }
    Ok(peers.into_iter().map(|(role, (_, header))| (role, header)).collect())
}

fn assert_reaped(raw: &Value) {
    assert_eq!(raw["cleanup_attempted"], true);
    assert_eq!(raw["cleanup_complete"], true);
    assert_eq!(raw["cleanup_within_bound"], true);
    assert_eq!(raw["cleanup_errors"], json!([]));
    assert_eq!(raw["cleanup_reaped_descendants"], json!([]));
    assert_eq!(raw["naturally_reaped_descendants"], json!([]));
    assert_eq!(raw["final_group_absent"], true);
    assert_eq!(raw["unreaped_child_retained_until_receipt"], false);
    let pid = u32::try_from(raw["pid"].as_u64().unwrap()).unwrap();
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
}

#[test]
fn actual_roles_overlap_preserve_receipts_and_isolate_environment() {
    let case = Case::new();
    let parent_env = std::env::var_os("AP_FTRACE_MUTATE_ROLE");
    let (results, observation) = case.run_live(4, 0, false);
    let observed = observation.unwrap(); // Assert only after all supervisors join.
    assert_eq!(ROLE_MUTANT_CONCURRENCY, 4);
    assert_eq!(results.iter().map(|(role, _)| *role).collect::<Vec<_>>(), [1, 2, 3, 4]);
    for (role, receipt) in &results {
        assert_eq!(receipt["passed"], true);
        assert_eq!(receipt["expected_refusal"], true);
        let raw = &receipt["raw"];
        assert_eq!(raw["pid"], observed[role][1]);
        assert_eq!(raw["signal"], libc::SIGABRT);
        assert_eq!(raw["passed"], false);
        assert_eq!(raw["timed_out"], false);
        assert_eq!(raw["log_overflow"], false);
        assert_eq!(raw["primary_error"], Value::Null);
        assert_eq!(raw["terminal_bounds_error"], Value::Null);
        assert_eq!(raw["natural_terminal_group"], true);
        assert_reaped(raw);
    }
    for (file, prefix) in [("stdout", "retained stdout"), ("stderr", "retained stderr")] {
        let bytes = fs::read_to_string(case.path.join(file)).unwrap();
        let mut lines = bytes.lines();
        assert_eq!(lines.next(), Some(prefix));
        let mut roles = lines.collect::<Vec<_>>();
        roles.sort_unstable();
        assert_eq!(roles, ["role:1", "role:2", "role:3", "role:4"]);
    }
    assert_eq!(std::env::var_os("AP_FTRACE_MUTATE_ROLE"), parent_env);
}

#[test]
fn actual_role_failure_joins_entire_batch_and_blocks_later_roles() {
    let case = Case::new();
    let (results, observation) = case.run_live(8, 2, false);
    let observed = observation.unwrap();
    assert_eq!(results.len(), 4, "roles 5..=8 must never be admitted");
    let mut stages = Vec::new();
    let mut overall = json!({"passed":true});
    append_role_receipts(&mut stages, &mut overall, results);
    assert_eq!(stages.len(), 4);
    assert_eq!(overall["passed"], false);
    assert_eq!(overall["raw"]["raw_status"], 7, "later success cannot erase role 2 failure");
    for (index, stage) in stages.iter().enumerate() {
        let role = index as u32 + 1;
        assert_eq!(stage["name"], format!("test:ftrace-mutant-{role}"));
        let receipt = &stage["receipt"];
        assert_eq!(receipt["raw"]["pid"], observed[&role][1]);
        assert_eq!(receipt["passed"], role != 2);
        if role == 2 {
            assert_eq!(receipt["raw"]["raw_status"], 7);
            assert_eq!(receipt["raw"]["signal"], Value::Null);
        } else {
            assert_eq!(receipt["raw"]["signal"], libc::SIGABRT);
        }
        assert_reaped(&receipt["raw"]);
    }
}

#[test]
fn actual_role_log_growth_uses_one_shared_aggregate_budget() {
    let case = Case::new();
    let (results, observation) = case.run_live(8, 0, true);
    observation.unwrap();
    assert_eq!(results.len(), 4, "overflow blocks roles 5..=8");
    let bytes = fs::metadata(case.path.join("stdout")).unwrap().len()
        + fs::metadata(case.path.join("stderr")).unwrap().len();
    assert!(bytes > LIMITS.logs, "four individually sub-limit producers must cross the aggregate cap");
    assert!(results.iter().any(|(_, r)| r["raw"]["log_overflow"] == true));
    let mut stages = Vec::new();
    let mut overall = json!({"passed":true});
    append_role_receipts(&mut stages, &mut overall, results);
    assert_eq!(overall["passed"], false);
    assert_eq!(stages.len(), 4);
    for stage in stages {
        let receipt = &stage["receipt"];
        if receipt["raw"]["log_overflow"] == true {
            assert_eq!(receipt["passed"], false, "SIGABRT with overflow never qualifies");
        }
        assert_reaped(&receipt["raw"]);
    }
}

fn assert_shared_admission_refuses(wall: bool) {
    let case = Case::new();
    let started = if wall {
        Instant::now().checked_sub(LIMITS.wall).unwrap()
    } else {
        File::options().write(true).open(case.path.join("stdout")).unwrap()
            .set_len(LIMITS.logs + 1).unwrap();
        Instant::now()
    };
    // An invalid executable would report spawn failure if aggregate admission
    // were bypassed: each receipt must specifically retain the bound refusal.
    let results = execute_role_mutants(&case.path.join("never-spawn"), 1..=8,
        started, &case.path.join("stdout"), &case.path.join("stderr"));
    assert_eq!(results.len(), 4);
    for (index, (role, receipt)) in results.iter().enumerate() {
        assert_eq!(*role, index as u32 + 1);
        assert_eq!(receipt["passed"], false);
        assert_eq!(receipt["raw"]["pid"], Value::Null);
        assert_eq!(receipt["raw"]["cleanup_attempted"], false);
        assert_eq!(receipt["raw"]["cleanup_complete"], Value::Null);
        assert!(receipt["raw"]["primary_error"].as_str().unwrap()
            .contains("aggregate wall/log bound before next stage"));
    }
}

#[test]
fn actual_role_admission_keeps_original_action_start() {
    assert_shared_admission_refuses(true);
}

#[test]
fn actual_role_admission_keeps_original_aggregate_logs() {
    assert_shared_admission_refuses(false);
}

// These modeled closure receipts test orchestration only. No process or
// terminal/cleanup observation is manufactured or used as child authority.
fn modeled_receipt(role: u32, fail: u32) -> Value {
    json!({"passed":role != fail, "modeled":true,
        "opaque_payload":{"role":role,"bytes":[0,255,role],"error":if role == fail {Some("original failure")} else {None}}})
}

fn modeled_batches(fail: u32, panic_role: u32) -> Vec<(u32, Value)> {
    let deadline = Instant::now() + Duration::from_secs(1);
    let active = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let finished = AtomicUsize::new(0);
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let last = if fail == 0 && panic_role == 0 { 17 } else { 4 };
    let results = std::thread::scope(|scope| {
        let worker = scope.spawn(|| role_batches(1..=17, |role| {
            assert!(finished.load(Ordering::SeqCst) >= ((role - 1) / 4 * 4) as usize,
                "next batch must wait for every earlier worker");
            let live = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(live, Ordering::SeqCst);
            assert!(live <= 4);
            let (release_tx, release_rx) = mpsc::channel();
            ready_tx.send((role, release_tx)).unwrap();
            release_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            active.fetch_sub(1, Ordering::SeqCst);
            finished.fetch_add(1, Ordering::SeqCst);
            done_tx.send(role).unwrap();
            assert_ne!(role, panic_role, "modeled worker panic after rendezvous");
            modeled_receipt(role, fail)
        }));
        for first in (1..=last).step_by(4) {
            let end = (first + 3).min(last);
            let mut held = BTreeMap::new();
            for _ in first..=end {
                let (role, release) = ready_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
                assert!((first..=end).contains(&role));
                assert!(held.insert(role, release).is_none());
            }
            assert_eq!(active.load(Ordering::SeqCst), (end - first + 1) as usize);
            assert!(matches!(ready_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            // Force reverse operation completion while the helper must return
            // canonical role order and retain the complete opaque Values.
            for (role, release) in held.into_iter().rev() {
                release.send(()).unwrap();
                assert_eq!(done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap(), role);
            }
        }
        worker.join().unwrap()
    });
    assert_eq!(ROLE_MUTANT_CONCURRENCY, 4);
    assert_eq!(peak.load(Ordering::SeqCst), 4);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(finished.load(Ordering::SeqCst), last as usize);
    assert_eq!(results.len(), last as usize);
    assert!(matches!(ready_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    results
}

#[test]
fn modeled_batches_bound_overlap_and_retain_all_seventeen_canonical_values() {
    let results = modeled_batches(0, 0);
    for (index, (role, receipt)) in results.into_iter().enumerate() {
        assert_eq!(role, index as u32 + 1);
        assert_eq!(receipt, modeled_receipt(role, 0));
    }
}

#[test]
fn modeled_failure_stays_sticky_and_joins_the_full_started_batch() {
    let results = modeled_batches(2, 0);
    let mut stages = Vec::new();
    let mut overall = json!({"passed":true});
    append_role_receipts(&mut stages, &mut overall, results);
    assert_eq!(overall, modeled_receipt(2, 2));
    assert_eq!(stages.len(), 4);
    for (index, stage) in stages.into_iter().enumerate() {
        let role = index as u32 + 1;
        assert_eq!(stage, json!({"name":format!("test:ftrace-mutant-{role}"),"receipt":modeled_receipt(role, 2)}));
    }
}

#[test]
fn modeled_panic_cannot_detach_peers_or_certify_cleanup() {
    let results = modeled_batches(0, 2);
    for (role, receipt) in results {
        if role == 2 {
            assert_eq!(receipt["passed"], false);
            assert_eq!(receipt["raw"], Value::Null);
            assert!(receipt["worker_error"].as_str().unwrap().contains("modeled worker panic"));
        } else {
            assert_eq!(receipt, modeled_receipt(role, 0));
        }
    }
}
