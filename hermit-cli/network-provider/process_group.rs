/* SPDX-License-Identifier: BSD-3-Clause */
//! Shared compiler/control child supervision. Numeric process-group signals
//! require the exact unreaped child; final group readback never authorizes one.
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use serde_json::Value;
use serde_json::json;

/// Establish ownership before spawning a compiler or control. Nested wrappers
/// may exit without waiting for their own children. Reaping below is restricted
/// to exact terminal children in this invocation's still-reserved process group;
/// it never consumes a wait result from a different group or another parent.
pub(super) fn own_descendants() -> Result<()> {
    let result = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("own compiler descendants");
    }
    Ok(())
}

pub(super) fn absent(pid: u32) -> Result<bool> {
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return Ok(false);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(true)
    } else {
        Err(error.into())
    }
}

fn kill(child: &std::process::Child) -> Result<()> {
    // A reaped child cannot authorize a numeric process-group signal. Validate
    // the retained wait owner here, including Drop and every error path.
    exited_without_reap(child)?;
    let pid = child.id();
    if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

// The explicit supervisor records both the primary error and cleanup evidence.
// Drop is only a panic/unwind backstop, never a successful cleanup receipt.
pub(super) struct OwnedChild {
    pub(super) child: std::process::Child,
    pub(super) cleanup_attempted: bool,
    pub(super) reaped: bool,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.cleanup_attempted && !self.reaped {
            let until = Instant::now() + Duration::from_secs(2);
            let result = kill(&self.child);
            let mut terminal = None;
            let mut descendants = Vec::new();
            if result.is_ok() {
                while Instant::now() < until {
                    let reaped = reap_terminal_descendants(&self.child, until, &mut descendants);
                    match exited_without_reap(&self.child) {
                        Ok(true)
                            if reaped.is_err()
                                || group_members(self.child.id())
                                    .is_ok_and(|members| members == [self.child.id()]) =>
                        {
                            terminal = Some(self.child.wait());
                            self.reaped = terminal.as_ref().is_some_and(|status| status.is_ok());
                            break;
                        }
                        Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                        Err(error) => {
                            terminal = Some(Err(std::io::Error::other(format!("{error:#}"))));
                            break;
                        }
                    }
                }
            }
            let mut group_absent = absent(self.child.id());
            while matches!(group_absent, Ok(false)) && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
                group_absent = absent(self.child.id());
            }
            // Diagnostic only. Panic cleanup cannot turn a failed action into
            // success, and no group signal is issued after the final wait.
            eprintln!(
                "package supervisor unwound: pid={}, kill={result:?}, terminal={terminal:?}, descendants={descendants:?}, group_absent={group_absent:?}, successful_receipt=false",
                self.child.id()
            );
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub(super) wall: Duration,
    pub(super) logs: u64,
    pub(super) cleanup: Duration,
}

pub(super) fn bounds(
    started: Instant,
    stdout: &Path,
    stderr: &Path,
    limits: Limits,
) -> Result<(bool, bool)> {
    let bytes = fs::metadata(stdout)?
        .len()
        .checked_add(fs::metadata(stderr)?.len())
        .context("log size overflow")?;
    Ok((started.elapsed() >= limits.wall, bytes > limits.logs))
}

// Observe, but do not reap, the exact owned child. Its unreaped PID reserves
// the numeric process-group identity through the last possible group signal.
pub(super) fn exited_without_reap(child: &std::process::Child) -> Result<bool> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).context("owned child waitid");
    }
    Ok(unsafe { info.si_pid() } == child.id() as i32)
}

// A task may disappear between proc-directory enumeration, opening stat, and
// its read callback. ENOENT is the pathname case; ESRCH is the already-open proc
// file losing its task. This classification is only for untrusted census
// candidates, never an authenticated child's wait or retained terminal read.
fn vanished_census_task(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

// Linux comm is an opaque byte string and may contain spaces, parentheses,
// newlines, or non-UTF-8 bytes. Only the fields after its final delimiter are
// textual. Malformed numeric fields still fail closed; this parser grants no
// wait or signal authority.
fn stat_fields(raw: &[u8]) -> Result<&str> {
    let end = raw
        .windows(2)
        .rposition(|pair| pair == b") ")
        .context("invalid process stat")?;
    std::str::from_utf8(&raw[end + 2..]).context("invalid process stat fields")
}

fn stat_group(raw: &[u8]) -> Result<u32> {
    Ok(stat_fields(raw)?
        .split_whitespace()
        .nth(2)
        .context("missing process group")?
        .parse::<u32>()?)
}

fn census_group(raw: &[u8]) -> Result<Option<u32>> {
    // do_task_stat leaves precisely ppid=0, pgid=-1, sid=-1 when
    // lock_task_sighand loses to removal of its sampled task. Non-leader exec
    // can transfer that task's PID and group membership to a live replacement
    // before clearing the old task's sighand. None is therefore an INCONCLUSIVE
    // snapshot, never proof that the current candidate PID has no group.
    // Authenticated terminal-child reads still use the strict positive parser.
    let mut fields = stat_fields(raw)?.split_whitespace();
    let _state = fields.next(); // Sampled before sighand locking, not authority.
    if (fields.next(), fields.next(), fields.next()) == (Some("0"), Some("-1"), Some("-1")) {
        return Ok(None);
    }
    stat_group(raw).map(Some)
}

fn resolve_census_group(
    pid: u32,
    mut read_current: impl FnMut() -> std::io::Result<Vec<u8>>,
) -> Result<Option<u32>> {
    // At most one follow-up path lookup. An old-task sentinel does not bind
    // the PID's current task after exec takeover. A repeated ambiguity fails
    // closed under the caller's original bounds; no new deadline or sleep.
    for _ in 0..2 {
        let raw = match read_current() {
            Ok(raw) => raw,
            Err(error) if vanished_census_task(&error) => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read census candidate {pid} stat"));
            }
        };
        if let Some(group) = census_group(&raw).with_context(|| {
            format!(
                "parse census candidate {pid} stat (first 1024 bytes): {:?}",
                &raw[..raw.len().min(1024)]
            )
        })? {
            return Ok(Some(group));
        }
    }
    anyhow::bail!("census candidate {pid} still has an inconclusive removed-task snapshot")
}

fn terminal_stat_identity(raw: &[u8], parent: u32, group: u32) -> Result<(&str, u64)> {
    let fields: Vec<_> = stat_fields(raw)?.split_whitespace().collect();
    ensure!(fields.len() > 19, "short descendant stat");
    ensure!(
        fields[1].parse::<u32>()? == parent && fields[2].parse::<u32>()? == group,
        "terminal descendant ownership/group changed"
    );
    Ok((fields[0], fields[19].parse::<u64>()?))
}

fn group_members(group: u32) -> Result<Vec<u32>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir("/proc").context("enumerate process census")? {
        let entry = entry.context("read process census directory entry")?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        candidates.push((pid, entry.path().join("stat")));
    }
    scan_census(&candidates, group, |pid, path| {
        resolve_census_group(pid, || fs::read(path))
    })
}

// Each call still performs a fresh, complete census. Split only the independent
// proc reads, never waits or signals. There is no cached membership and no
// snapshot shared with another checkpoint or supervisor. Every admitted worker
// is joined, including after an error; partial membership can never be returned.
fn scan_census(
    candidates: &[(u32, std::path::PathBuf)],
    group: u32,
    read: impl Fn(u32, &Path) -> Result<Option<u32>> + Sync,
) -> Result<Vec<u32>> {
    const WORKERS: usize = 2;
    let results = std::thread::scope(|scope| {
        let mut workers = Vec::new();
        let mut admission_error = None;
        for chunk in candidates.chunks(candidates.len().div_ceil(WORKERS).max(1)) {
            let read = &read;
            match std::thread::Builder::new().spawn_scoped(scope, move || {
                let mut members = Vec::new();
                for (pid, path) in chunk {
                    if read(*pid, path)? == Some(group) {
                        members.push(*pid);
                    }
                }
                Ok(members)
            }) {
                Ok(worker) => workers.push(worker),
                Err(error) => {
                    admission_error =
                        Some(anyhow::Error::new(error).context("spawn census worker"));
                    break;
                }
            }
        }
        let mut results: Vec<Result<Vec<u32>>> = workers
            .into_iter()
            .map(|worker| {
                worker.join().unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<String>()
                        .map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic");
                    Err(anyhow::anyhow!("process census worker panic: {message}"))
                })
            })
            .collect();
        if let Some(error) = admission_error {
            results.push(Err(error));
        }
        results
    });
    let mut members = Vec::new();
    for result in results {
        members.extend(result?);
    }
    members.sort_unstable();
    Ok(members)
}

// A group census is only a candidate list, never wait authority. P_PID WNOWAIT
// authenticates each exact adopted child while the unreaped leader reserves the
// group number. The terminal child reserves its own PID through its final wait.
// Every caller exclusively owns its Child/group; no waiter uses waitpid(-1).
fn reap_terminal_descendants(
    leader: &std::process::Child,
    deadline: Instant,
    receipts: &mut Vec<Value>,
) -> Result<()> {
    if Instant::now() >= deadline {
        return Ok(());
    }
    exited_without_reap(leader)?;
    for pid in group_members(leader.id())? {
        if Instant::now() >= deadline {
            break;
        }
        if pid == leader.id() {
            continue;
        }
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let observed = unsafe {
            libc::waitid(
                libc::P_PID,
                pid,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if observed != 0 {
            let error = std::io::Error::last_os_error();
            // A descendant with a living intermediate parent is not ours yet.
            if error.raw_os_error() == Some(libc::ECHILD) {
                continue;
            }
            return Err(error).context("observe exact descendant");
        }
        if unsafe { info.si_pid() } != pid as i32 {
            continue; // This exact child is still live. Never reap or relabel it.
        }
        let raw = fs::read(format!("/proc/{pid}/stat"))
            .with_context(|| format!("read authenticated terminal descendant {pid} stat"))?;
        let (state, birth) = terminal_stat_identity(&raw, std::process::id(), leader.id())?;
        let mut status = 0;
        let reaped = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        if reaped < 0 {
            return Err(std::io::Error::last_os_error()).context("reap exact terminal descendant");
        }
        ensure!(
            reaped == pid as i32,
            "terminal descendant lost wait ownership"
        );
        receipts.push(json!({
            "pid":pid, "birth":birth, "parent":std::process::id(),
            "group":leader.id(), "state_at_wnowait":state,
            "waitid_code":info.si_code, "waitid_status":unsafe { info.si_status() },
            "raw_wait_status":status
        }));
    }
    Ok(())
}

pub(super) fn only_terminal_leader(group: u32, members: &[u32], terminal: bool) -> bool {
    terminal && members == [group]
}

pub(super) fn supervise(
    child: OwnedChild,
    started: Instant,
    stdout: &Path,
    stderr: &Path,
    limits: Limits,
) -> Value {
    supervise_with_pre_kill_census(child, started, stdout, stderr, limits, group_members)
}

// Only the pre-kill observation is parameterized for its negative control.
// Production always uses the actual census above; every wait, signal, reap,
// cleanup observation and bound below remains the real owned operation.
fn supervise_with_pre_kill_census(
    mut child: OwnedChild,
    started: Instant,
    stdout: &Path,
    stderr: &Path,
    limits: Limits,
    pre_kill_census: impl FnOnce(u32) -> Result<Vec<u32>>,
) -> Value {
    let pid = child.child.id();
    let mut status = None;
    let mut observed_terminal = false;
    let mut timed_out = false;
    let mut log_overflow = false;
    let primary = (|| -> Result<()> {
        loop {
            observed_terminal = exited_without_reap(&child.child)?;
            // This check is deliberately AFTER observation, including terminal
            // results. A fast exit must not bypass the wall or log cap.
            let observed = bounds(started, stdout, stderr, limits)?;
            timed_out |= observed.0;
            log_overflow |= observed.1;
            ensure!(!timed_out && !log_overflow, "test wall/log bound exceeded");
            if observed_terminal {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    let primary_error = primary.err().map(|error| format!("{error:#}"));
    let mut cleanup_errors = Vec::new();
    let cleanup_started = Instant::now();
    let cleanup_deadline = cleanup_started + limits.cleanup;
    let mut natural_descendants = Vec::new();
    if let Err(error) =
        reap_terminal_descendants(&child.child, cleanup_deadline, &mut natural_descendants)
    {
        cleanup_errors.push(format!("natural descendant reap: {error:#}"));
    }
    // Retain the old success condition: leader exit with a live descendant
    // is a failure even when later owned cleanup successfully kills it.
    let before_members = pre_kill_census(pid);
    let natural_terminal_group = before_members
        .as_ref()
        .is_ok_and(|members| only_terminal_leader(pid, members, observed_terminal));
    let before_members = match before_members {
        Ok(members) => Some(members),
        Err(error) => {
            cleanup_errors.push(format!("pre-kill group census: {error:#}"));
            None
        }
    };
    // Revalidate that the process is still our unreaped child before signaling.
    // No code below sends any signal after final reap, including on error.
    let mut owned_group_kill = false;
    match exited_without_reap(&child.child) {
        Ok(_) => match kill(&child.child) {
            Ok(()) => owned_group_kill = true,
            Err(error) => cleanup_errors.push(format!("owned group kill: {error:#}")),
        },
        Err(error) => cleanup_errors.push(format!("pre-kill child ownership: {error:#}")),
    }
    let mut cleanup_descendants = Vec::new();
    if owned_group_kill {
        let mut after_kill_descendants = Vec::new();
        loop {
            let reaped = reap_terminal_descendants(
                &child.child,
                cleanup_deadline,
                &mut after_kill_descendants,
            );
            let reaping_failed = reaped.is_err();
            if let Err(error) = reaped {
                cleanup_errors.push(format!("cleanup descendant reap: {error:#}"));
            }
            match exited_without_reap(&child.child) {
                Ok(true)
                    if reaping_failed
                        || group_members(pid).is_ok_and(|members| members == [pid])
                        || Instant::now() >= cleanup_deadline =>
                {
                    // waitid retained the exited leader, so this final wait
                    // cannot wait for a running child. It releases PID ownership.
                    match child.child.wait() {
                        Ok(reaped) => {
                            status = Some(reaped);
                            child.reaped = true;
                        }
                        Err(error) => cleanup_errors.push(format!("reap: {error}")),
                    }
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    cleanup_errors.push(format!("terminal observation: {error:#}"));
                    break;
                }
            }
            if Instant::now() >= cleanup_deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // These were observed only after the cleanup signal. They can establish
        // terminal custody, but cannot satisfy the natural-success predicate.
        cleanup_descendants = after_kill_descendants;
    }
    let mut final_group_absent = None;
    loop {
        match absent(pid) {
            Ok(observed) => final_group_absent = Some(observed),
            Err(error) => {
                cleanup_errors.push(format!("terminal group readback: {error:#}"));
                break;
            }
        }
        if final_group_absent == Some(true) || Instant::now() >= cleanup_deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.cleanup_attempted = true;
    let cleanup_within_bound = Instant::now() < cleanup_deadline;
    let cleanup_complete =
        status.is_some() && final_group_absent == Some(true) && cleanup_within_bound;
    let final_bounds = bounds(started, stdout, stderr, limits);
    let bounds_error = match final_bounds {
        Ok(observed) => {
            timed_out |= observed.0;
            log_overflow |= observed.1;
            None
        }
        Err(error) => Some(format!("{error:#}")),
    };
    let passed = status.is_some_and(|status| status.success())
        && primary_error.is_none()
        && bounds_error.is_none()
        && !timed_out
        && !log_overflow
        && natural_terminal_group
        // A stale/missed census cannot promote a descendant observed only
        // after the cleanup signal into naturally completed work.
        && cleanup_descendants.is_empty()
        && cleanup_complete
        && cleanup_errors.is_empty();
    json!({
        "pid":pid, "raw_status":status.and_then(|status|status.code()),
        "signal":status.and_then(|status|status.signal()),
        "seconds":started.elapsed().as_secs_f64(),
        "timed_out":timed_out, "log_overflow":log_overflow,
        "primary_error":primary_error, "terminal_bounds_error":bounds_error,
        "terminal_observed_without_reap":observed_terminal,
        "naturally_reaped_descendants":natural_descendants,
        "cleanup_reaped_descendants":cleanup_descendants,
        "group_members_before_kill":before_members, "natural_terminal_group":natural_terminal_group,
        "owned_group_kill_before_reap":owned_group_kill,
        "cleanup_attempted":true, "cleanup_complete":cleanup_complete,
        "cleanup_within_bound":cleanup_within_bound,
        "unreaped_child_retained_until_receipt":!child.reaped,
        "cleanup_seconds":cleanup_started.elapsed().as_secs_f64(),
        "cleanup_errors":cleanup_errors, "final_group_absent":final_group_absent,
        "passed":passed
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parallel_census_reads_every_candidate_and_sorts_exact_members() {
        use std::sync::atomic::AtomicUsize;
        let candidates: Vec<_> = [9, 3, 8, 2, 7, 1]
            .into_iter()
            .map(|pid| (pid, PathBuf::from(pid.to_string())))
            .collect();
        let visits: Vec<_> = (0..10).map(|_| AtomicUsize::new(0)).collect();
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let members = scan_census(&candidates, 73, |pid, path| {
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(current, Ordering::SeqCst);
            assert_eq!(path, Path::new(&pid.to_string()));
            visits[pid as usize].fetch_add(1, Ordering::SeqCst);
            active.fetch_sub(1, Ordering::SeqCst);
            Ok(if pid == 8 {
                None
            } else {
                Some(if pid % 2 == 1 { 73 } else { 91 })
            })
        })
        .unwrap();
        assert_eq!(members, [1, 3, 7, 9]);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!((1..=2).contains(&peak.load(Ordering::SeqCst)));
        for (pid, visit) in visits.iter().enumerate() {
            assert_eq!(
                visit.load(Ordering::SeqCst),
                usize::from([9, 3, 8, 2, 7, 1].contains(&pid))
            );
        }
        assert!(
            scan_census(&[], 73, |_, _| panic!("empty census read"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parallel_census_errors_join_other_worker_and_never_return_partial_members() {
        use std::sync::atomic::AtomicUsize;
        let candidates: Vec<_> = (1..=4).map(|pid| (pid, PathBuf::new())).collect();
        for errno in [
            libc::EACCES,
            libc::EPERM,
            libc::EIO,
            libc::EINTR,
            libc::ECHILD,
        ] {
            let completed = AtomicUsize::new(0);
            let result = scan_census(&candidates, 73, |pid, _| {
                if pid == 1 {
                    return Err(std::io::Error::from_raw_os_error(errno).into());
                }
                completed.fetch_add(1, Ordering::SeqCst);
                Ok(Some(73))
            });
            assert_eq!(
                completed.load(Ordering::SeqCst),
                2,
                "other partition joined despite error"
            );
            assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .raw_os_error(),
                Some(errno)
            );
        }
    }

    #[test]
    fn parallel_census_panic_joins_other_worker_and_refuses_membership() {
        use std::sync::atomic::AtomicUsize;
        let candidates: Vec<_> = (1..=4).map(|pid| (pid, PathBuf::new())).collect();
        let completed = AtomicUsize::new(0);
        let result = scan_census(&candidates, 73, |pid, _| {
            assert_ne!(pid, 1, "injected census reader panic");
            completed.fetch_add(1, Ordering::SeqCst);
            Ok(Some(73))
        });
        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected census reader panic")
        );
    }

    fn fixture(script: &str) -> (OwnedChild, PathBuf) {
        own_descendants().unwrap();
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join(format!(
            "process-group-{}-{}-{}",
            module_path!(),
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(fs::File::create(path.join("stdout")).unwrap())
            .stderr(fs::File::create(path.join("stderr")).unwrap());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        (
            OwnedChild {
                child: command.spawn().unwrap(),
                cleanup_attempted: false,
                reaped: false,
            },
            path,
        )
    }

    fn terminal(child: &OwnedChild) {
        let until = Instant::now() + Duration::from_secs(2);
        while !exited_without_reap(&child.child).unwrap() {
            assert!(Instant::now() < until, "fixture leader did not exit");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn receipt(child: OwnedChild, path: &Path, started: Instant) -> Value {
        let result = supervise(
            child,
            started,
            &path.join("stdout"),
            &path.join("stderr"),
            Limits {
                wall: Duration::from_secs(60),
                logs: 4 * 1024 * 1024,
                cleanup: Duration::from_secs(2),
            },
        );
        fs::write(
            path.join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        result
    }

    fn stat_fixture(comm: &[u8], fields: &[&str]) -> Vec<u8> {
        let mut raw = b"123 (".to_vec();
        raw.extend_from_slice(comm);
        raw.extend_from_slice(b") ");
        raw.extend_from_slice(fields.join(" ").as_bytes());
        raw.push(b'\n');
        raw
    }

    #[test]
    fn proc_stat_opaque_comm_preserves_group_state_and_birth() {
        let mut fields = vec!["0"; 20];
        fields[0] = "Z";
        fields[1] = "91";
        fields[2] = "73";
        fields[19] = "18446744073709551615";
        for comm in [
            b"ordinary".as_slice(),
            b"spaces and ()",
            b"x) Z 5 8 (x) ",
            b"\xff ) (\x80\n () ",
        ] {
            let raw = stat_fixture(comm, &fields);
            assert_eq!(stat_group(&raw).unwrap(), 73);
            assert_eq!(
                terminal_stat_identity(&raw, 91, 73).unwrap(),
                ("Z", u64::MAX)
            );
        }
    }

    #[test]
    fn proc_stat_malformed_tail_and_numeric_identity_refuse() {
        for raw in [b"123 (missing".as_slice(), b"123 (x) ", b"123 (x) Z 91"] {
            assert!(stat_group(raw).is_err());
            assert!(terminal_stat_identity(raw, 91, 73).is_err());
        }
        let mut valid = vec!["0"; 20];
        valid[0] = "Z";
        valid[1] = "91";
        valid[2] = "73";
        valid[19] = "12345";
        for (index, malformed) in [
            (1, "parent"),
            (1, "-1"),
            (1, "4294967296"),
            (2, "group"),
            (2, "-1"),
            (2, "4294967296"),
            (19, "birth"),
            (19, "-1"),
            (19, "18446744073709551616"),
        ] {
            let mut fields = valid.clone();
            fields[index] = malformed;
            let raw = stat_fixture(b"\xff ) (", &fields);
            assert!(terminal_stat_identity(&raw, 91, 73).is_err());
            if index == 2 {
                assert!(stat_group(&raw).is_err());
            }
        }
        for count in 0..20 {
            assert!(terminal_stat_identity(&stat_fixture(b"x", &valid[..count]), 91, 73).is_err());
        }
        let raw = stat_fixture(b"\xff ( )", &valid);
        assert!(terminal_stat_identity(&raw, 92, 73).is_err());
        assert!(terminal_stat_identity(&raw, 91, 74).is_err());
        let mut malformed = raw;
        malformed.push(0xff);
        assert!(stat_group(&malformed).is_err());
        assert!(terminal_stat_identity(&malformed, 91, 73).is_err());
    }

    #[test]
    fn census_accepts_only_exact_removed_task_group_sentinel() {
        let mut fields = vec!["0"; 20];
        fields[0] = "X";
        fields[1] = "0";
        fields[2] = "-1";
        fields[3] = "-1";
        fields[19] = "12345";
        let removed = stat_fixture(b"removed task", &fields);
        assert_eq!(census_group(&removed).unwrap(), None);
        // A census sentinel never authenticates a retained terminal child.
        assert!(stat_group(&removed).is_err());
        assert!(terminal_stat_identity(&removed, 91, 73).is_err());
        // State is sampled before sighand locking; it is not removal authority.
        for state in ["R", "S", "Z"] {
            fields[0] = state;
            assert_eq!(
                census_group(&stat_fixture(b"removed", &fields)).unwrap(),
                None
            );
        }
        for (index, malformed) in [
            (1, "91"),
            (1, "-1"),
            (1, "parent"),
            (2, "-2"),
            (2, "group"),
            (3, "73"),
            (3, "-2"),
        ] {
            let mut wrong = fields.clone();
            wrong[index] = malformed;
            assert!(census_group(&stat_fixture(b"not removed", &wrong)).is_err());
        }
        fields[1] = "91";
        fields[2] = "73";
        fields[3] = "73";
        assert_eq!(
            census_group(&stat_fixture(b"owned", &fields)).unwrap(),
            Some(73)
        );
    }

    #[test]
    fn census_sentinel_rechecks_same_candidate_before_claiming_absence() {
        // Controlled proc observations, not a native de_thread race receipt.
        // Exercise the exact resolver used by group_members and its natural
        // success decision, including the old-task/live-PID exec transition.
        let mut fields = vec!["0"; 20];
        fields[0] = "X";
        fields[2] = "-1";
        fields[3] = "-1";
        let stale = stat_fixture(b"old leader", &fields);
        fields[0] = "R";
        fields[1] = "91";
        fields[2] = "73";
        fields[3] = "73";
        let live = stat_fixture(b"exec replacement", &fields);
        let mut reads = std::collections::VecDeque::from([Ok(stale.clone()), Ok(live)]);
        let group = resolve_census_group(123, || reads.pop_front().unwrap()).unwrap();
        assert_eq!(
            group,
            Some(73),
            "same PID can still name a live in-group task"
        );
        assert!(
            reads.is_empty(),
            "resolve the current PID, not only its old task"
        );
        let mut members = vec![73];
        if group == Some(73) {
            members.push(123);
        }
        assert!(!only_terminal_leader(73, &members, true));

        for errno in [libc::ENOENT, libc::ESRCH] {
            let mut reads = std::collections::VecDeque::from([
                Ok(stale.clone()),
                Err(std::io::Error::from_raw_os_error(errno)),
            ]);
            assert_eq!(
                resolve_census_group(123, || reads.pop_front().unwrap()).unwrap(),
                None
            );
            assert!(reads.is_empty());
        }
        let mut reads = std::collections::VecDeque::from([Ok(stale.clone()), Ok(stale.clone())]);
        assert!(resolve_census_group(123, || reads.pop_front().unwrap()).is_err());
        assert!(
            reads.is_empty(),
            "two observations, no retry-until-success loop"
        );
        for errno in [libc::EPERM, libc::EIO, libc::EINTR] {
            let mut reads = std::collections::VecDeque::from([
                Ok(stale.clone()),
                Err(std::io::Error::from_raw_os_error(errno)),
            ]);
            assert!(resolve_census_group(123, || reads.pop_front().unwrap()).is_err());
            assert!(reads.is_empty());
        }
    }

    #[test]
    fn non_utf8_terminal_descendant_preserves_exact_wait_ownership() {
        // Same mechanism as the selected bpftool wrapper: an intermediate
        // process leaves a naturally exited child unreaped, then exits itself.
        // WNOWAIT makes the fixture independent of sleep or scheduling luck.
        let started = Instant::now();
        let (mut unrelated, _) = fixture("import os\nos._exit(23)\n");
        terminal(&unrelated);
        let (child, path) = fixture(
            "import os, ctypes\nassert ctypes.CDLL(None).prctl(15, ctypes.c_char_p(b\"\\xff ) (\\x80\"), 0, 0, 0) == 0\npid = os.fork()\nif pid == 0:\n os._exit(0)\nos.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)\nos._exit(0)\n",
        );
        let leader = child.child.id();
        terminal(&child);
        let raw = fs::read(format!("/proc/{leader}/stat")).unwrap();
        assert!(std::str::from_utf8(&raw).is_err());
        assert_eq!(stat_group(&raw).unwrap(), leader);
        let result = receipt(child, &path, started);
        assert_eq!(result["passed"], true);
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["group_members_before_kill"], json!([leader]));
        assert_eq!(
            result["naturally_reaped_descendants"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["raw_wait_status"],
            0
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["state_at_wnowait"],
            "Z"
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["waitid_code"],
            libc::CLD_EXITED
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["waitid_status"],
            0
        );
        assert_eq!(result["cleanup_reaped_descendants"], json!([]));
        assert_eq!(result["natural_terminal_group"], true);
        assert_eq!(result["owned_group_kill_before_reap"], true);
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["final_group_absent"], true);
        assert!(exited_without_reap(&unrelated.child).unwrap());
        let status = unrelated.child.wait().unwrap();
        unrelated.reaped = true;
        assert_eq!(status.code(), Some(23));
    }

    #[test]
    fn naturally_terminal_descendant_is_reaped_without_consuming_another_group() {
        // Same mechanism as the selected bpftool wrapper: an intermediate
        // process leaves a naturally exited child unreaped, then exits itself.
        // WNOWAIT makes the fixture independent of sleep or scheduling luck.
        let started = Instant::now();
        let (mut unrelated, _) = fixture("import os\nos._exit(23)\n");
        terminal(&unrelated);
        let (child, path) = fixture(
            "import os\npid = os.fork()\nif pid == 0:\n os._exit(0)\nos.waitid(os.P_PID, pid, os.WEXITED | os.WNOWAIT)\nos._exit(0)\n",
        );
        let leader = child.child.id();
        terminal(&child);
        let result = receipt(child, &path, started);
        assert_eq!(result["passed"], true);
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["group_members_before_kill"], json!([leader]));
        assert_eq!(
            result["naturally_reaped_descendants"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["raw_wait_status"],
            0
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["state_at_wnowait"],
            "Z"
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["waitid_code"],
            libc::CLD_EXITED
        );
        assert_eq!(
            result["naturally_reaped_descendants"][0]["waitid_status"],
            0
        );
        assert_eq!(result["cleanup_reaped_descendants"], json!([]));
        assert_eq!(result["natural_terminal_group"], true);
        assert_eq!(result["owned_group_kill_before_reap"], true);
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["final_group_absent"], true);
        assert!(exited_without_reap(&unrelated.child).unwrap());
        let status = unrelated.child.wait().unwrap();
        unrelated.reaped = true;
        assert_eq!(status.code(), Some(23));
    }

    #[test]
    fn live_descendant_killed_by_cleanup_never_becomes_natural_success() {
        let started = Instant::now();
        let (child, path) = fixture(
            "import os, time\nr,w = os.pipe()\npid = os.fork()\nif pid == 0:\n os.close(r)\n os.write(w, b'r')\n time.sleep(30)\n os._exit(0)\nos.close(w)\nassert os.read(r, 1) == b'r'\nos._exit(0)\n",
        );
        terminal(&child);
        let result = receipt(child, &path, started);
        assert_eq!(result["passed"], false);
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["naturally_reaped_descendants"], json!([]));
        assert_eq!(
            result["group_members_before_kill"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(result["natural_terminal_group"], false);
        assert_eq!(
            result["cleanup_reaped_descendants"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            result["cleanup_reaped_descendants"][0]["raw_wait_status"],
            libc::SIGKILL
        );
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["final_group_absent"], true);
    }

    #[test]
    fn missed_live_descendant_census_cannot_override_actual_cleanup_reap() {
        let started = Instant::now();
        let (child, path) = fixture(
            "import os, time\nr,w = os.pipe()\npid = os.fork()\nif pid == 0:\n os.close(r)\n os.write(w, b'r')\n time.sleep(30)\n os._exit(0)\nos.close(w)\nassert os.read(r, 1) == b'r'\nos._exit(0)\n",
        );
        let leader = child.child.id();
        terminal(&child);
        let result = supervise_with_pre_kill_census(
            child,
            started,
            &path.join("stdout"),
            &path.join("stderr"),
            Limits {
                wall: Duration::from_secs(60),
                logs: 4 * 1024 * 1024,
                cleanup: Duration::from_secs(2),
            },
            |actual| {
                assert_eq!(actual, leader);
                // Controlled incomplete observation, NOT a native exec-race
                // receipt. No wait/signal/terminal authority is fabricated.
                assert_eq!(group_members(actual).unwrap().len(), 2);
                Ok(vec![leader])
            },
        );
        fs::write(
            path.join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["group_members_before_kill"], json!([leader]));
        assert_eq!(
            result["natural_terminal_group"], true,
            "controlled stale census alone"
        );
        assert_eq!(result["naturally_reaped_descendants"], json!([]));
        let reaped = result["cleanup_reaped_descendants"].as_array().unwrap();
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0]["raw_wait_status"], libc::SIGKILL);
        assert_eq!(reaped[0]["waitid_code"], libc::CLD_KILLED);
        assert_eq!(reaped[0]["waitid_status"], libc::SIGKILL);
        assert_eq!(result["owned_group_kill_before_reap"], true);
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["cleanup_errors"], json!([]));
        assert_eq!(result["final_group_absent"], true);
        assert_eq!(
            result["passed"], false,
            "actual post-signal reap vetoes natural success"
        );
    }

    #[test]
    fn only_census_disappearance_errors_are_classified_as_vanished() {
        for errno in [libc::ENOENT, libc::ESRCH] {
            assert!(vanished_census_task(&std::io::Error::from_raw_os_error(
                errno
            )));
        }
        for errno in [
            libc::EACCES,
            libc::EPERM,
            libc::EIO,
            libc::EINTR,
            libc::ECHILD,
        ] {
            assert!(!vanished_census_task(&std::io::Error::from_raw_os_error(
                errno
            )));
        }
        assert!(!vanished_census_task(&std::io::Error::other(
            "unknown census error"
        )));
    }

    #[test]
    fn proc_stat_opened_before_exact_reap_returns_esrch_after_reap() {
        use std::io::Read;
        use std::io::Seek;
        use std::io::SeekFrom;
        use std::io::Write;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "read token; test \"$token\" = release"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut owner = OwnedChild {
            child: command.spawn().unwrap(),
            cleanup_attempted: false,
            reaped: false,
        };
        let mut retained = fs::File::open(format!("/proc/{}/stat", owner.child.id())).unwrap();
        let mut before = String::new();
        retained.read_to_string(&mut before).unwrap();
        let (_, fields) = before.rsplit_once(") ").unwrap();
        assert_eq!(
            fields
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u32>()
                .unwrap(),
            std::process::id()
        );
        retained.seek(SeekFrom::Start(0)).unwrap();
        let mut input = owner.child.stdin.take().unwrap();
        input.write_all(b"release\n").unwrap();
        drop(input);
        while !exited_without_reap(&owner.child).unwrap() {
            assert!(Instant::now() < deadline, "original fixture deadline");
            std::thread::sleep(Duration::from_millis(1));
        }
        let status = owner.child.wait().unwrap();
        owner.reaped = true;
        assert_eq!(status.code(), Some(0));
        let error = retained.read_to_string(&mut String::new()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
        assert!(vanished_census_task(&error));
        drop(retained);
        assert!(Instant::now() < deadline, "original fixture deadline");
        assert!(absent(owner.child.id()).unwrap());
        // Losing an authenticated wait owner remains an error. The census
        // classifier cannot turn an already-reaped Child into signal authority.
        assert!(kill(&owner.child).is_err());
    }

    #[test]
    fn exact_unreaped_child_authorizes_signal_and_reaped_child_is_refused() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut owner = OwnedChild {
            child: command.spawn().unwrap(),
            cleanup_attempted: false,
            reaped: false,
        };
        let original = Instant::now() + Duration::from_secs(2);
        while !exited_without_reap(&owner.child).unwrap() {
            assert!(Instant::now() < original, "owned control failed to exit");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(exited_without_reap(&owner.child).unwrap());
        assert!(!absent(owner.child.id()).unwrap());
        // The exact wait owner still reserves group identity for this signal.
        kill(&owner.child).unwrap();
        owner.cleanup_attempted = true;
        let status = owner.child.wait().unwrap();
        owner.reaped = true;
        assert!(status.success());
        assert!(absent(owner.child.id()).unwrap());
        let error = kill(&owner.child).unwrap_err();
        assert!(error.to_string().contains("owned child waitid"));
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
