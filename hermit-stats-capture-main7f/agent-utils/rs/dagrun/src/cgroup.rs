//! Linux cgroup-v2 containment for complete DAG runs and individual steps.

// Two-level Linux cgroup-v2 containment for the DAG runner (Rust port of `cgroup.py`).
//
// The runner re-execs itself inside a transient `systemd-run --user --scope` (a DELEGATED
// outer cgroup), then carves one CHILD cgroup per step under it. Each step's bash leader
// self-moves into its child cgroup BEFORE forking any grandchild, so a per-step
// `cgroup.kill` SIGKILLs the WHOLE subtree atomically — including `setsid` / double-fork
// escapees a process-group kill misses. Per-step `memory.max` (+ `memory.swap.max=0`) makes a
// single runaway step OOM-killed at its own cap while the rest of the run and the host survive.
//
// GRACEFUL DEGRADATION + NO SILENT FAILURE: everything is best-effort, but a best-effort
// cgroupfs write that would drop a requested cap never fails silently — it emits a visible
// `warn()` on stderr. When cgroup-v2 + a systemd `--user` scope are unavailable the manager
// reports `enabled() == false` and the caller falls back to process-group teardown.
//
// This mirrors the OBSERVABLE behavior of the Python `cgroup.py`; the systemd/cgroupfs
// interactions (env sentinels, slice/unit names, the supervisor drain) are kept identical.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::sizing::{cpu_count, mem_available_bytes};

/// cgroup-v2 unified hierarchy mount point (Linux-only, matching the target).
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Shared parent slice for ALL concurrent runs (one aggregate CPUQuota bounds their sum).
const SLICE_NAME: &str = "dagrun.slice";
/// Prefix for the per-run transient scope unit (`<prefix>-<pid>`).
const UNIT_PREFIX: &str = "dagrun";
/// Environment sentinel set in the re-exec'd (in-scope) child.
const ENV_IN_SCOPE: &str = "DAGRUN_IN_SCOPE";
/// Exact parent-owned cgroup root offered to one nested dagrun scheduler.
pub const DELEGATED_CGROUP_ENV: &str = "DAGRUN_DELEGATED_CGROUP";
/// Exact marker for a graph-reviewed nested step when the outer scheduler is itself unboxed.
/// This authorizes nesting but is never evidence of cgroup containment.
pub const DELEGATED_UNBOXED_ENV: &str = "DAGRUN_DELEGATED_UNBOXED";
/// Env var carrying the outer scope unit name to the in-scope child.
const ENV_SCOPE_UNIT: &str = "DAGRUN_SCOPE_UNIT";
/// Optional caller override that may tighten, but never widen, the derived outer cap.
const OUTER_MEMORY_MAX_ENV: &str = "DAGRUN_OUTER_MEMORY_MAX_BYTES";
/// Exact cap carried across the systemd re-exec for in-scope readback.
const EXPECTED_OUTER_MEMORY_MAX_ENV: &str = "DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES";
/// Exact total CPU-core cap carried across the systemd re-exec for in-scope readback.
const EXPECTED_OUTER_CPU_COUNT_ENV: &str = "DAGRUN_EXPECTED_OUTER_CPU_COUNT";
/// Child cgroup the runner vacates into (cgroup-v2 "no internal processes" rule).
const SUPERVISOR: &str = "supervisor";
/// Per-step child cgroup directory prefix (also the normal-exit backstop scan key).
const STEP_PREFIX: &str = "step-";
const MAX_CGROUP_COMPONENT_BYTES: usize = 255;
/// Prefix for every log/warning line this module prints.
const LOG_PREFIX: &str = "[dagrun]";
/// Fraction of WHOLE-SYSTEM CPU the shared aggregate slice may use (leaves ~10% headroom).
const DEFAULT_CPU_BUDGET_FRACTION: f64 = 0.90;
/// Generous last-resort run boundary, leaving memory for neighbours and the OS.
const DEFAULT_MEMORY_BUDGET_FRACTION: f64 = 0.90;
/// Bounded settling window for systemd's asynchronous population of a new delegated scope.
const SCOPE_DRAIN_ATTEMPTS: usize = 50;
const SCOPE_DRAIN_RETRY: Duration = Duration::from_millis(10);
const SCOPE_DRAIN_EMPTY_SAMPLES: usize = 2;
const CONTROLLER_ENABLE_ATTEMPTS: usize = 3;
static NEXT_NESTED_ROOT: AtomicU64 = AtomicU64::new(0);
static NESTED_COORDINATOR_RESTORE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Monotonic process-local count used by the public CLI boundary to fail closed when an
/// otherwise-successful in-process invocation could not escape its temporary aggregate cgroup.
pub(crate) fn nested_coordinator_restore_failures() -> u64 {
    NESTED_COORDINATOR_RESTORE_FAILURES.load(Ordering::Relaxed)
}

/// Per-step containment operations used by the scheduler.
///
/// A single manager is shared across a run. Implementations create, cap, measure, and tear down
/// one child cgroup per step while keeping host-specific state outside the scheduler.
pub trait CgroupManager: Send + Sync {
    /// Whether per-step containment is actually usable on this host.
    fn enabled(&self) -> bool;
    /// What this manager actually contains, for the durable record.
    ///
    /// DEFAULTS TO `Unknown`, DELIBERATELY. An implementation that does not answer must not have
    /// its silence read as containment — the whole failure mode being repaired here is a missing
    /// field reading as normal. An implementor that boxes says so explicitly.
    fn containment_record(&self) -> RunContainment {
        RunContainment::Unknown {
            detail: "this CgroupManager does not report its containment".into(),
        }
    }
    /// Wrap a step's command so its bash leader joins the step's child cgroup before forking,
    /// applying the inner memory/CPU caps. Returns `cmd` unchanged when disabled.
    fn prepare_command(
        &self,
        tag: &str,
        cmd: &str,
        mem_max: Option<i64>,
        cpu_count: Option<i64>,
    ) -> String;
    /// Prepare a step as an empty cgroup root whose `supervisor` leaf contains the command, so a
    /// child dagrun may create descendants while this manager retains subtree kill ownership.
    fn prepare_delegated_command(
        &self,
        _tag: &str,
        _cmd: &str,
        _mem_max: Option<i64>,
        _cpu_count: Option<i64>,
    ) -> PreparedDelegation {
        PreparedDelegation {
            command: "echo 'ERROR: delegated_children requires active cgroup containment' >&2\n\
                      exit 125\n"
                .to_string(),
            root: None,
        }
    }
    /// SIGKILL the step's whole cgroup subtree (`cgroup.kill`); `true` if the write landed.
    fn kill(&self, tag: &str) -> bool;
    /// Remove the step's now-empty child cgroup dir (best-effort).
    fn cleanup(&self, tag: &str);
    /// Kernel OOM-kill events owned by the step's cgroup (the selected memory-events view's
    /// `oom_kill`; see [`CgroupManager::memory_events`]).
    fn oom_kills(&self, tag: &str) -> i64;
    /// Peak resident memory (bytes) of the step's cgroup (`memory.peak`).
    ///
    /// A peak alone does not say whether it measured DEMAND. A peak equal to the cap reported by
    /// [`CgroupManager::applied_memory_max`] is a censored observation — the step used all it was
    /// allowed — so read the two together before treating this as an observed maximum.
    fn peak_bytes(&self, tag: &str) -> Option<i64>;
    /// The step cgroup's owned memory events as counter -> value.
    ///
    /// These are per-step deltas without subtraction: the child cgroup is created for the step
    /// and removed after it, so every counter starts at zero. `max > 0` with every OOM counter
    /// zero is reclaim-at-cap — a step that PASSED while pinned to its ceiling. Ordinary steps read
    /// hierarchical `memory.events`. A root prepared by `prepare_delegated_command` reads
    /// `memory.events.local`, excluding child-step OOMs already owned by the nested scheduler
    /// while retaining events charged at the outer delegation boundary. At that boundary `oom`
    /// (or `oom_group_kill`) can be positive while the descendant victim owns `oom_kill`; every
    /// positive OOM facet is actionable even though sibling nested-run counters remain excluded.
    ///
    /// DEFAULTS TO `None`, DELIBERATELY: a manager that does not answer has said nothing, and
    /// nothing must not read as "no events occurred".
    fn memory_events(&self, _tag: &str) -> Option<BTreeMap<String, i64>> {
        None
    }
    /// The TIGHTEST `memory.max` in force over the step, read back from cgroupfs: a decimal byte
    /// count, or the literal `"max"` when no level the runner can see bounds it.
    ///
    /// cgroup-v2 limits are hierarchical, so the step's OWN `memory.max` is not on its own the
    /// ceiling it ran under; the runner always installs an outer scope cap, and a step given no
    /// inner cap reads `"max"` at its own level while the scope's ceiling is what held it. The
    /// minimum over the levels the runner can see is what `peak_bytes` is interpretable against.
    ///
    /// Read back rather than echoed from the requested cap, because the case that matters — a cap
    /// the kernel never accepted — is exactly where the two disagree.
    ///
    /// Nested schedulers additionally snapshot every readable `memory.max` from the validated
    /// parent delegation through the cgroup namespace root and stamp a finite minimum onto their
    /// own run root. Limits above a top-level run scope or outside the process's cgroup namespace
    /// remain outside that run's recorded view.
    ///
    /// DEFAULTS TO `None`, DELIBERATELY. `None` (cap unknown) and `Some("max")` (known unbounded)
    /// are different answers: only the second rules out censoring by the runner's own caps, so a
    /// silent manager must not be read as having reported an unbounded step.
    fn applied_memory_max(&self, _tag: &str) -> Option<String> {
        None
    }
    /// Per-step cgroup-v2 CPU counters from `cpu.stat`.
    fn cpu_stats(&self, tag: &str) -> Option<BTreeMap<String, i64>>;
    /// Whole-run cgroup-v2 CPU consumption from the outer scope's `cpu.stat`, in microseconds.
    ///
    /// DEFAULTS TO `None`, DELIBERATELY. A caller asking for a whole-run CPU budget must refuse
    /// when this measurement is absent; treating an unreadable counter as zero would make the
    /// guard silently stop enforcing itself.
    fn run_cpu_usage_usec(&self) -> Option<i64> {
        None
    }
    /// Per-step CPU pressure-stall averages (`cpu.pressure` `some` line: `avg10`, `avg60`),
    /// sampled at step start + end to attribute contention. `None` when disabled/unreadable.
    fn cpu_pressure(&self, tag: &str) -> Option<BTreeMap<String, f64>>;
    /// Current descendant thread count from the step's `cgroup.threads`.
    fn thread_count(&self, tag: &str) -> Option<i64>;
    /// NORMAL-EXIT backstop: `cgroup.kill` + `rmdir` every remaining step child cgroup.
    fn kill_all_remaining(&self) -> i64;
}

/// Command wrapper plus the exact cgroup root an outer scheduler delegates to its child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedDelegation {
    /// Fail-closed shell wrapper that moves the command into the delegation's supervisor leaf.
    pub command: String,
    /// Root to export through [`DELEGATED_CGROUP_ENV`], absent when delegation failed.
    pub root: Option<PathBuf>,
}

/// Shell prologue that migrates the step's bash leader into `child` before it forks anything.
///
/// `$$` is the leader's pid; writing it moves the leader, and every descendant then inherits this
/// cgroup AT FORK. That inheritance is the whole point: a `setsid` child changes session and pgid
/// but NOT cgroup membership, so it stays reachable by `cgroup.kill` even though a process-group
/// kill can no longer see it. Migration is fail-closed: executing the command under an empty cap
/// would falsely claim containment while leaving the actual process tree outside it.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn join_cgroup_command(child: &Path, cmd: &str) -> String {
    let procs = child.join("cgroup.procs");
    let quoted = shell_quote(&procs.to_string_lossy());
    format!(
        "if ! printf '%s\\n' \"$$\" 2>/dev/null > {quoted}; then\n  echo 'ERROR: step could not join its dagrun cgroup; refusing uncontained run' >&2\n  exit 125\nfi\n{cmd}"
    )
}

fn remove_empty_cgroup_descendants(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    for child in children {
        remove_empty_cgroup_descendants(&child);
        let _ = fs::remove_dir(child);
    }
}

/// Emit a visible degraded-enforcement warning (No Silent Failure).
fn warn(msg: &str) {
    eprintln!("{LOG_PREFIX} WARNING: degraded enforcement: {msg}");
}

/// An injective cgroup filename encoding for a UTF-8 step tag.
///
/// Replacing unsafe characters with `_` aliases distinct tags (`a/b.j` and `a_b.j`), allowing one
/// step's cgroup kill or cleanup to affect the other. `~HH` escapes are reversible and `~` itself
/// is escaped, so distinct byte strings remain distinct.
fn sanitize(tag: &str) -> String {
    let mut s = String::with_capacity(tag.len() * 3 + STEP_PREFIX.len());
    s.push_str(STEP_PREFIX);
    for byte in tag.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'.' | b'_' | b'-') {
            s.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(s, "~{byte:02x}");
        }
    }
    s
}

/// Filesystem path of THIS process's cgroup v2, via `/proc/self/cgroup` (`0::<path>`).
fn my_cgroup_path() -> Option<PathBuf> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(rel) = line.strip_prefix("0::") {
            let rel = rel.trim_start_matches('/');
            return Some(Path::new(CGROUP_ROOT).join(rel));
        }
    }
    None
}

/// Move this scheduler into its capped nested-run root and verify live membership.
///
/// Inner steps already join descendants of `run_root`. The coordinator must join as well or a
/// tighter nested `--max-mem`/`--max-cpus` would not be a whole-invocation boundary.
fn move_current_to_nested_supervisor(
    run_root: &Path,
) -> Result<NestedCoordinatorGuard, NestedCoordinatorMoveError> {
    let supervisor = run_root.join(SUPERVISOR);
    let return_supervisor = my_cgroup_path().ok_or_else(|| NestedCoordinatorMoveError {
        error: std::io::Error::other(
            "current cgroup is unreadable before nested scheduler migration",
        ),
        recovery: None,
    })?;
    let pid = std::process::id();
    let pid_text = pid.to_string();
    fs::write(supervisor.join("cgroup.procs"), &pid_text).map_err(|error| {
        NestedCoordinatorMoveError {
            error,
            // A failed cgroup.procs write did not migrate the caller.
            recovery: None,
        }
    })?;
    let guard = NestedCoordinatorGuard {
        run_root: run_root.to_path_buf(),
        return_supervisor,
        active: true,
    };
    let verified = (|| -> std::io::Result<()> {
        let current = my_cgroup_path().ok_or_else(|| {
            std::io::Error::other("current cgroup is unreadable after nested scheduler migration")
        })?;
        if fs::canonicalize(&current)? != fs::canonicalize(&supervisor)? {
            return Err(std::io::Error::other(format!(
                "nested scheduler pid {pid} did not enter {}; current cgroup is {}",
                supervisor.display(),
                current.display()
            )));
        }
        let roster = fs::read_to_string(supervisor.join("cgroup.procs"))?;
        if !roster.split_whitespace().any(|member| member == pid_text) {
            return Err(std::io::Error::other(format!(
                "nested scheduler pid {pid} is absent from {}",
                supervisor.join("cgroup.procs").display()
            )));
        }
        Ok(())
    })();
    if let Err(error) = verified {
        return Err(failed_nested_migration(guard, error));
    }
    Ok(guard)
}

struct NestedCoordinatorGuard {
    run_root: PathBuf,
    return_supervisor: PathBuf,
    active: bool,
}

struct NestedCoordinatorMoveError {
    error: std::io::Error,
    /// Present only when post-write verification failed and rollback could not be proven. Keeping
    /// the guard alive lets the manager retry on drop and report a persistent failure to the CLI.
    recovery: Option<NestedCoordinatorGuard>,
}

impl std::fmt::Display for NestedCoordinatorMoveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

fn failed_nested_migration_with<F>(
    mut guard: NestedCoordinatorGuard,
    migration_error: std::io::Error,
    restore: F,
) -> NestedCoordinatorMoveError
where
    F: FnOnce(&mut NestedCoordinatorGuard) -> std::io::Result<()>,
{
    match restore(&mut guard) {
        Ok(()) => NestedCoordinatorMoveError {
            error: migration_error,
            recovery: None,
        },
        Err(rollback_error) => NestedCoordinatorMoveError {
            error: std::io::Error::other(format!(
                "{migration_error}; rollback to the previous cgroup also failed: {rollback_error}"
            )),
            recovery: Some(guard),
        },
    }
}

fn failed_nested_migration(
    guard: NestedCoordinatorGuard,
    migration_error: std::io::Error,
) -> NestedCoordinatorMoveError {
    failed_nested_migration_with(guard, migration_error, NestedCoordinatorGuard::restore)
}

impl NestedCoordinatorGuard {
    fn restore(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let pid = std::process::id();
        let pid_text = pid.to_string();
        fs::write(self.return_supervisor.join("cgroup.procs"), &pid_text)?;
        let current = my_cgroup_path().ok_or_else(|| {
            std::io::Error::other("current cgroup is unreadable after nested scheduler restoration")
        })?;
        if fs::canonicalize(&current)? != fs::canonicalize(&self.return_supervisor)? {
            return Err(std::io::Error::other(format!(
                "nested scheduler pid {pid} did not return to {}; current cgroup is {}",
                self.return_supervisor.display(),
                current.display()
            )));
        }
        let roster = fs::read_to_string(self.return_supervisor.join("cgroup.procs"))?;
        if !roster.split_whitespace().any(|member| member == pid_text) {
            return Err(std::io::Error::other(format!(
                "nested scheduler pid {pid} is absent from {}",
                self.return_supervisor.join("cgroup.procs").display()
            )));
        }
        self.active = false;
        remove_empty_cgroup_descendants(&self.run_root);
        if let Err(error) = fs::remove_dir(&self.run_root) {
            warn(&format!(
                "could not remove completed nested scheduler cgroup {} ({error})",
                self.run_root.display()
            ));
        }
        Ok(())
    }
}

impl Drop for NestedCoordinatorGuard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            NESTED_COORDINATOR_RESTORE_FAILURES.fetch_add(1, Ordering::Relaxed);
            warn(&format!(
                "could not restore nested scheduler to delegated parent {} ({error}); outer \
                 owner must clean up {}",
                self.return_supervisor.display(),
                self.run_root.display()
            ));
        }
    }
}

fn validate_delegated_cgroup_root_at(
    root: &Path,
    current: &Path,
    pid: u32,
    cgroup_root: &Path,
) -> Result<PathBuf, String> {
    let base = fs::canonicalize(cgroup_root)
        .map_err(|e| format!("delegated cgroup hierarchy cannot be resolved: {e}"))?;
    let root = fs::canonicalize(root)
        .map_err(|e| format!("delegated cgroup path cannot be resolved: {e}"))?;
    let current = fs::canonicalize(current)
        .map_err(|e| format!("current cgroup path cannot be resolved: {e}"))?;
    if root == base || !root.starts_with(&base) {
        return Err(format!(
            "delegated cgroup {} is outside the cgroup-v2 hierarchy",
            root.display()
        ));
    }
    if current.parent() != Some(root.as_path())
        || current.file_name().and_then(|name| name.to_str()) != Some(SUPERVISOR)
    {
        return Err(format!(
            "current cgroup {} is not the exact '{SUPERVISOR}' leaf beneath delegated root {}",
            current.display(),
            root.display()
        ));
    }
    let roster_path = current.join("cgroup.procs");
    let roster = fs::read_to_string(&roster_path)
        .map_err(|e| format!("cannot read delegated supervisor roster: {e}"))?;
    if !roster
        .lines()
        .any(|line| line.trim().parse::<u32>() == Ok(pid))
    {
        return Err(format!(
            "pid {pid} is not listed in delegated supervisor {}",
            roster_path.display()
        ));
    }
    if !root.join("cgroup.kill").exists() {
        return Err(format!(
            "delegated root {} has no cgroup.kill ownership boundary",
            root.display()
        ));
    }
    Ok(root)
}

/// Validate the parent-owned cgroup root offered to this nested scheduler.
///
/// `Ok(None)` means no delegation was offered. A present but forged, stale, or non-ancestor path
/// is an error rather than permission to weaken the default nested-run refusal.
pub fn delegated_cgroup_root() -> Result<Option<PathBuf>, String> {
    let Some(raw) = std::env::var_os(DELEGATED_CGROUP_ENV) else {
        return Ok(None);
    };
    let root = PathBuf::from(raw);
    if !root.is_absolute() {
        return Err(format!(
            "${DELEGATED_CGROUP_ENV} must be an absolute live cgroup path"
        ));
    }
    let current = my_cgroup_path().ok_or_else(|| {
        "the current cgroup-v2 path is not observable from /proc/self/cgroup".to_string()
    })?;
    validate_delegated_cgroup_root_at(&root, &current, std::process::id(), Path::new(CGROUP_ROOT))
        .map(Some)
}

/// Read and trim a cgroup interface file, or `None`.
fn read_trim(group: &Path, name: &str) -> Option<String> {
    fs::read_to_string(group.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Drain a delegated scope root and require it to remain empty across two observations.
///
/// `systemd-run --scope` can still be finishing its own short-lived setup while the re-exec'd
/// runner starts. A single empty read is therefore not a stable hand-off: a late helper can enter
/// the root after that read, making the subsequent `cgroup.subtree_control` write fail with
/// `EBUSY`. This bounded settle loop moves every observed pid, waits, and confirms quiescence
/// before controller enablement.
///
/// The closures make the temporal contract executable without pretending an ordinary fixture
/// file has cgroupfs's process-migration semantics.
fn drain_scope_root_with<R, M, S>(
    mut read_pids: R,
    mut move_pid: M,
    mut sleep: S,
) -> std::io::Result<()>
where
    R: FnMut() -> std::io::Result<Vec<String>>,
    M: FnMut(&str) -> std::io::Result<()>,
    S: FnMut(Duration),
{
    let mut consecutive_empty = 0;
    for attempt in 0..SCOPE_DRAIN_ATTEMPTS {
        let pids = read_pids()?;
        if pids.is_empty() {
            consecutive_empty += 1;
            if consecutive_empty >= SCOPE_DRAIN_EMPTY_SAMPLES {
                return Ok(());
            }
        } else {
            consecutive_empty = 0;
            for pid in pids {
                // ESRCH is expected when a short-lived helper exits between the roster read and
                // migration. A process that really remains is caught by the next roster read.
                let _ = move_pid(&pid);
            }
        }
        if attempt + 1 < SCOPE_DRAIN_ATTEMPTS {
            sleep(SCOPE_DRAIN_RETRY);
        }
    }
    let remaining = read_pids()?;
    let remaining = if remaining.is_empty() {
        "<empty on one unconfirmed final sample>".to_string()
    } else {
        remaining.join(",")
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "scope root did not quiesce after {} ms; remaining pid(s): {}",
            (SCOPE_DRAIN_ATTEMPTS - 1) * SCOPE_DRAIN_RETRY.as_millis() as usize,
            remaining
        ),
    ))
}

fn enable_controller_with<R, M, W, S>(
    mut read_pids: R,
    mut move_pid: M,
    mut write_controller: W,
    mut sleep: S,
) -> std::io::Result<()>
where
    R: FnMut() -> std::io::Result<Vec<String>>,
    M: FnMut(&str) -> std::io::Result<()>,
    W: FnMut() -> std::io::Result<()>,
    S: FnMut(Duration),
{
    for attempt in 0..CONTROLLER_ENABLE_ATTEMPTS {
        drain_scope_root_with(&mut read_pids, &mut move_pid, &mut sleep)?;
        match write_controller() {
            Ok(()) => return Ok(()),
            Err(e)
                if e.raw_os_error() == Some(libc::EBUSY)
                    && attempt + 1 < CONTROLLER_ENABLE_ATTEMPTS =>
            {
                // A member arrived after the stable-empty observation but before the controller
                // write. Drain the newly observed member and retry the guarded transition.
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the final controller-enable attempt returns its error")
}

fn enable_controller(scope: &Path, supervisor: &Path, controller: &str) -> std::io::Result<()> {
    enable_controller_with(
        || {
            Ok(fs::read_to_string(scope.join("cgroup.procs"))?
                .split_whitespace()
                .map(String::from)
                .collect())
        },
        |pid| fs::write(supervisor.join("cgroup.procs"), pid),
        || {
            fs::write(
                scope.join("cgroup.subtree_control"),
                format!("+{controller}"),
            )
        },
        thread::sleep,
    )
}

// Whole granted cores from a `cpu.max` string (`"<quota> <period>"`), or `None` when
// unbounded (`"max ..."`) / unparseable. Floors to >=1 for any positive quota. Mirrors
// Python `_cpu_max_cores`.
fn cpu_max_cores(value: Option<String>) -> Option<i64> {
    let value = value?;
    let mut parts = value.split_whitespace();
    let quota = parts.next()?;
    let period = parts.next()?;
    if parts.next().is_some() || quota == "max" {
        return None;
    }
    let quota: i64 = quota.parse().ok()?;
    let period: i64 = period.parse().ok()?;
    if quota <= 0 || period <= 0 {
        return None;
    }
    Some((quota / period).max(1))
}

// Byte cap from a `memory.max` string, or `None` when unbounded/unparseable. Mirrors Python
// `_memory_max_bytes`.
fn memory_max_bytes(value: Option<String>) -> Option<i64> {
    let value = value?;
    if value == "max" {
        return None;
    }
    value.parse().ok()
}

/// Whether cgroup v2 retained `expected`, allowing only safe page rounding down.
fn memory_limit_matches(expected: i64, actual: Option<i64>) -> bool {
    actual.is_some_and(|actual| actual <= expected && expected - actual < 4096)
}

/// Tightest visible `memory.max` from an exact delegated parent through the cgroup namespace
/// root, plus whether every level was readable and parseable.
///
/// A nested scheduler creates a private run cgroup whose local limit initially reads `max`.
/// Without carrying this ancestor snapshot forward, its child profiles can call a peak
/// uncensored even though a finite parent step (or one of that step's ancestors) clamped it.
fn delegated_ancestor_memory_max(root: &Path, cgroup_root: &Path) -> (Option<i64>, bool) {
    let Ok(boundary) = fs::canonicalize(cgroup_root) else {
        return (None, false);
    };
    let Ok(mut group) = fs::canonicalize(root) else {
        return (None, false);
    };
    if group != boundary && !group.starts_with(&boundary) {
        return (None, false);
    }

    let mut tightest: Option<i64> = None;
    let mut complete = true;
    loop {
        match read_trim(&group, "memory.max") {
            Some(value) if value == "max" => {}
            // The hierarchy root has no parent to constrain and some kernels omit its controller
            // limit files entirely. That specific absence means unbounded, not unknown.
            None if group == boundary => {}
            Some(value) => match value.parse::<i64>() {
                Ok(bytes) if bytes >= 0 => {
                    tightest = Some(tightest.map_or(bytes, |prior| prior.min(bytes)));
                }
                _ => complete = false,
            },
            None => complete = false,
        }
        if group == boundary {
            break;
        }
        let Some(parent) = group.parent() else {
            return (tightest, false);
        };
        group = parent.to_path_buf();
    }
    (tightest, complete)
}

fn delegated_ancestor_cpu_count(root: &Path, cgroup_root: &Path) -> (Option<i64>, bool) {
    let Ok(boundary) = fs::canonicalize(cgroup_root) else {
        return (None, false);
    };
    let Ok(mut group) = fs::canonicalize(root) else {
        return (None, false);
    };
    if group != boundary && !group.starts_with(&boundary) {
        return (None, false);
    }

    let mut tightest: Option<i64> = None;
    let mut complete = true;
    loop {
        match read_trim(&group, "cpu.max") {
            None if group == boundary => {}
            Some(value) => {
                let fields: Vec<&str> = value.split_whitespace().collect();
                if fields.len() != 2 {
                    complete = false;
                } else if fields[0] == "max" {
                    if fields[1]
                        .parse::<i64>()
                        .ok()
                        .is_none_or(|period| period <= 0)
                    {
                        complete = false;
                    }
                } else if let Some(cpus) = cpu_max_cores(Some(value)) {
                    tightest = Some(tightest.map_or(cpus, |prior| prior.min(cpus)));
                } else {
                    complete = false;
                }
            }
            None => complete = false,
        }
        if group == boundary {
            break;
        }
        let Some(parent) = group.parent() else {
            return (tightest, false);
        };
        group = parent.to_path_buf();
    }
    (tightest, complete)
}

/// Effective parent envelope inherited by one nested scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DelegatedResourceLimits {
    pub(crate) memory_max_bytes: Option<i64>,
    pub(crate) memory_known: bool,
    pub(crate) max_cpus: Option<i64>,
    pub(crate) cpu_known: bool,
}

pub(crate) fn delegated_resource_limits(root: &Path) -> DelegatedResourceLimits {
    delegated_resource_limits_at(root, Path::new(CGROUP_ROOT))
}

fn delegated_resource_limits_at(root: &Path, cgroup_root: &Path) -> DelegatedResourceLimits {
    let (memory_max_bytes, memory_known) = delegated_ancestor_memory_max(root, cgroup_root);
    let (mut max_cpus, mut cpu_known) = delegated_ancestor_cpu_count(root, cgroup_root);
    // cpu.max bounds bandwidth; cpuset.cpus.effective independently bounds which CPUs are
    // usable. The latter already reflects every ancestor, so reading it at the delegation root
    // is sufficient.
    if let Some(cpuset) = read_trim(root, "cpuset.cpus.effective") {
        match crate::sizing::count_cpu_ranges(&cpuset) {
            Some(cpus) if cpus > 0 => {
                max_cpus = Some(max_cpus.map_or(cpus, |quota| quota.min(cpus)));
            }
            _ => cpu_known = false,
        }
    }
    DelegatedResourceLimits {
        memory_max_bytes,
        memory_known,
        max_cpus,
        cpu_known,
    }
}

/// Create a private nested-run root and stamp its effective delegated ancestor resource envelope.
///
/// The returned snapshot remains profiling provenance. Failure to mirror an inherited cap is
/// survivable because the physical ancestor still enforces it. Failure to apply a *tighter
/// requested* invocation cap is not: that would silently turn a nested `--max-cpus`/`--max-mem`
/// promise into modeling-only advice, so this helper returns an error.
fn create_nested_run_root(
    parent: &Path,
    run_root: &Path,
    cgroup_root: &Path,
    requested_max_cpus: Option<i64>,
    requested_max_mem_bytes: Option<i64>,
) -> std::io::Result<DelegatedResourceLimits> {
    let inherited = delegated_resource_limits_at(parent, cgroup_root);
    let effective = DelegatedResourceLimits {
        memory_max_bytes: [inherited.memory_max_bytes, requested_max_mem_bytes]
            .into_iter()
            .flatten()
            .min(),
        memory_known: inherited.memory_known,
        max_cpus: [inherited.max_cpus, requested_max_cpus]
            .into_iter()
            .flatten()
            .min(),
        cpu_known: inherited.cpu_known,
    };
    let memory_stamp_required = requested_max_mem_bytes.is_some_and(|requested| {
        inherited
            .memory_max_bytes
            .is_none_or(|parent| requested < parent)
    });
    let cpu_stamp_required = requested_max_cpus
        .is_some_and(|requested| inherited.max_cpus.is_none_or(|parent| requested < parent));
    fs::create_dir(run_root)?;
    if let Some(bytes) = effective.memory_max_bytes {
        if let Err(error) = stamp_nested_memory_limit(run_root, bytes) {
            if memory_stamp_required {
                return Err(std::io::Error::other(format!(
                    "could not enforce requested nested memory.max={bytes} on {}: {error}",
                    run_root.display()
                )));
            }
            warn(&format!(
                "could not stamp inherited memory.max={bytes} on nested scheduler cgroup {} \
                 ({error}); ancestor cap still applies",
                run_root.display()
            ));
        }
    }
    if let Some(cpus) = effective.max_cpus {
        if let Err(error) = stamp_nested_cpu_limit(run_root, cpus) {
            if cpu_stamp_required {
                return Err(std::io::Error::other(format!(
                    "could not enforce requested nested CPU ceiling {cpus} on {}: {error}",
                    run_root.display()
                )));
            }
            warn(&format!(
                "could not stamp inherited CPU ceiling {cpus} on nested scheduler cgroup {} \
                 ({error}); ancestor quota still applies",
                run_root.display()
            ));
        }
    }
    Ok(effective)
}

fn stamp_nested_memory_limit(run_root: &Path, bytes: i64) -> std::io::Result<()> {
    fs::write(run_root.join("memory.max"), bytes.to_string())?;
    let applied = memory_max_bytes(read_trim(run_root, "memory.max"));
    if memory_limit_matches(bytes, applied) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "memory.max readback was {applied:?}, expected {bytes}"
        )))
    }
}

fn stamp_nested_cpu_limit(run_root: &Path, cpus: i64) -> std::io::Result<()> {
    fs::write(
        run_root.join("cpu.max"),
        format!("{} 100000", cpus * 100_000),
    )?;
    let applied = cpu_max_cores(read_trim(run_root, "cpu.max"));
    if applied == Some(cpus) {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "cpu.max readback was {applied:?}, expected {cpus} CPUs"
        )))
    }
}

/// Create a cgroup directory, tolerating "already exists".
fn make_dir(path: &Path) -> std::io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn in_scope() -> bool {
    std::env::var(ENV_IN_SCOPE).ok().as_deref() == Some("1")
}

/// Generous outer-scope cap derived from current non-swap availability.
///
/// Per-step limits remain the precise controls. This boundary exists to keep a
/// runaway whole run from reaching the host-global OOM killer. The environment
/// override can tighten the cap for a constrained host or mutation test, but
/// cannot widen the derived 90%-of-MemAvailable boundary.
pub fn outer_memory_max_bytes() -> Option<i64> {
    outer_memory_max_bytes_capped(None)
}

/// The outer-scope cap: the SMALLEST of availability, the environment, and an explicit request.
///
/// `requested` is the caller's own ceiling — `--max-mem` — and obeys the same one-way rule as the
/// environment override: it can tighten the derived boundary, never widen it. Without it
/// `--max-mem 20G` only *sized* the schedule and the scope still admitted 90% of the host, so two
/// 20 GiB runs on one machine were 20 GiB each by arithmetic and by nothing else. A budget that is
/// a modelling input but not a containment limit is a share of a host nobody is holding.
///
/// A non-positive request is REFUSED (`None`) exactly like a non-positive environment value: the
/// caller asked for a ceiling this function cannot express, and returning the derived boundary
/// instead would hand back a WIDER scope than was asked for.
///
/// THAT IS A LIBRARY CONTRACT, NOT THE CLI'S BEHAVIOUR, and the difference is worth stating
/// plainly because the first version of this comment did not. `--max-mem 0` never reaches here:
/// `cli::requested_max_mem_bytes` drops a non-positive spec, and the run is then refused
/// by name by `select_max_steps` ("`--max-mem 0`: REFUSED — minimum runnable footprint … cannot
/// fit safely within budget 0 bytes", exit 2, before any step starts). One bad spec, one refusal:
/// refusing here as well would give the same typo two different exit codes depending on whether
/// boxing was attempted. The `requested <= 0` arm exists for callers of this function that are
/// not the CLI.
pub fn outer_memory_max_bytes_capped(requested: Option<i64>) -> Option<i64> {
    outer_memory_max_from(
        mem_available_bytes()?,
        std::env::var(OUTER_MEMORY_MAX_ENV).ok().as_deref(),
        requested,
    )
}

/// The pure core of [`outer_memory_max_bytes_capped`]: the smallest of the three ceilings.
///
/// Split out because the other two inputs are `/proc/meminfo` and the process environment, and a
/// rule about which ceiling wins is not worth much if it can only be exercised by arranging a
/// host.
fn outer_memory_max_from(
    available: i64,
    env_raw: Option<&str>,
    requested: Option<i64>,
) -> Option<i64> {
    if available <= 0 {
        return None;
    }
    let derived = ((available as f64) * DEFAULT_MEMORY_BUDGET_FRACTION) as i64;
    let mut cap = derived.max(1);
    if let Some(raw) = env_raw {
        let value: i64 = raw.parse().ok()?;
        if value <= 0 {
            return None;
        }
        cap = cap.min(value);
    }
    if let Some(value) = requested {
        if value <= 0 {
            return None;
        }
        cap = cap.min(value);
    }
    Some(cap.max(1))
}

/// Cap the parent requested, carried into the re-exec'd scope.
pub fn expected_outer_memory_max_bytes() -> Option<i64> {
    let value: i64 = std::env::var(EXPECTED_OUTER_MEMORY_MAX_ENV)
        .ok()?
        .parse()
        .ok()?;
    (value > 0).then_some(value)
}

/// Total CPU-core cap the parent requested, carried into the re-exec'd scope.
pub fn expected_outer_cpu_count() -> Option<i64> {
    let value: i64 = std::env::var(EXPECTED_OUTER_CPU_COUNT_ENV)
        .ok()?
        .parse()
        .ok()?;
    (value > 0).then_some(value)
}

/// Whether this process is already running inside the managed cgroup scope (the re-exec set the
/// `DAGRUN_IN_SCOPE` sentinel). The CLI checks this to decide whether to re-exec.
pub fn is_in_scope() -> bool {
    in_scope()
}

/// systemd `CPUQuota` percentage for `fraction` of ALL cores (min 100%).
fn cpu_quota_percent(fraction: f64) -> i64 {
    let ncpu = cpu_count().max(1);
    ((ncpu as f64 * fraction * 100.0).round() as i64).max(100)
}

/// Maximum CPU cores represented by the shared aggregate slice's quota.
///
/// A default per-run budget must not be wider than the parent slice every run is placed under;
/// otherwise the value changes after scope re-exec and the planner models cores the run cannot
/// receive. Explicit `--max-cpus` may still request a wider child quota; the parent remains
/// tighter.
pub fn aggregate_slice_max_cpus() -> i64 {
    (cpu_quota_percent(DEFAULT_CPU_BUDGET_FRACTION) / 100).max(1)
}

/// Compatibility alias for [`aggregate_slice_max_cpus`].
#[doc(hidden)]
#[deprecated(since = "0.13.1", note = "use aggregate_slice_max_cpus")]
pub fn aggregate_slice_cpu_jobs() -> i64 {
    aggregate_slice_max_cpus()
}

/// Pause between a failed scope probe and its one retry.
const SCOPE_PROBE_RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// True iff `systemd-run --user --scope` actually works here (cached).
fn systemd_scope_available() -> bool {
    static PROBE: OnceLock<bool> = OnceLock::new();
    *PROBE.get_or_init(|| probe_with_one_retry(run_scope_probe, SCOPE_PROBE_RETRY_BACKOFF))
}

/// Longest one scope probe may take. `systemd-run` waits on the user manager, and a manager that
/// never answers would otherwise hang the run before its first step.
const SCOPE_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// How long to keep collecting a finished probe's stderr. A descendant can hold the pipe open
/// after `systemd-run` itself has exited.
const SCOPE_PROBE_STDERR_GRACE: Duration = Duration::from_secs(1);

/// One `systemd-run --user --scope true`. The error names the exit status and carries the
/// probe's stderr, which is the only record of why systemd refused.
fn run_scope_probe(attempt: u32) -> Result<(), String> {
    let mut probe = Command::new("systemd-run");
    probe.args([
        "--user",
        "--scope",
        "--quiet",
        // A probe unit that fails is unloaded at once instead of staying listed as failed.
        "--collect",
        // A unit name per attempt, so a retry never collides with what its first attempt left.
        &format!("--unit=dagrun-probe-{}-{attempt}", std::process::id()),
        "true",
    ]);
    run_probe_bounded(probe, SCOPE_PROBE_TIMEOUT)
}

/// Run `probe` to completion, or kill it once `timeout` has passed. Either way the error keeps
/// whatever stderr the probe wrote, including the partial stderr of one that hung.
fn run_probe_bounded(mut probe: Command, timeout: Duration) -> Result<(), String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;

    let mut child = probe
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot launch systemd-run: {error}"))?;
    // Read stderr on its own thread, in chunks, so a full pipe cannot stall the child and the
    // output of a child that never exits is still available.
    let mut pipe = child.stderr.take().expect("stderr is piped");
    let (chunks, received) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = pipe.read(&mut buf) {
            if chunks.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => break Err(format!("timed out after {timeout:?}")),
            Err(error) => break Err(format!("cannot wait for systemd-run: {error}")),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut stderr = Vec::new();
    let grace_ends = Instant::now() + SCOPE_PROBE_STDERR_GRACE;
    while let Ok(chunk) =
        received.recv_timeout(grace_ends.saturating_duration_since(Instant::now()))
    {
        stderr.extend(chunk);
    }
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(describe_probe_failure(status, &stderr)),
        Err(why) => Err(format!(
            "{why}; stderr: {:?}",
            String::from_utf8_lossy(&stderr).trim()
        )),
    }
}

fn describe_probe_failure(status: std::process::ExitStatus, stderr: &[u8]) -> String {
    format!(
        "{status}; stderr: {:?}",
        String::from_utf8_lossy(stderr).trim()
    )
}

/// Run `probe`, and once more after `backoff` if it fails. Seen twice in about six weeks on a
/// shared host: the user manager refused one probe within a second while the same environment
/// set up scopes before and after it, and with nothing retried a whole validation failed closed.
/// A second refusal still fails closed. Every failure goes to stderr with its cause.
fn probe_with_one_retry(probe: impl Fn(u32) -> Result<(), String>, backoff: Duration) -> bool {
    for attempt in 1..=2 {
        match probe(attempt) {
            Ok(()) => return true,
            Err(why) => {
                eprintln!(
                    "{LOG_PREFIX} `systemd-run --user --scope` probe attempt {attempt} of 2 \
                     failed: {why}"
                );
                if attempt == 1 {
                    thread::sleep(backoff);
                }
            }
        }
    }
    false
}

/// Create/refresh the shared aggregate slice with a `CPUQuota` bounding the SUM of CPU across
/// every scope launched under it. Idempotent; best-effort (returns false when unavailable).
fn ensure_aggregate_slice(fraction: f64) -> bool {
    let quota = cpu_quota_percent(fraction);
    let start = Command::new("systemctl")
        .args(["--user", "start", SLICE_NAME])
        .output();
    let set = Command::new("systemctl")
        .args([
            "--user",
            "--runtime",
            "set-property",
            SLICE_NAME,
            &format!("CPUQuota={quota}%"),
        ])
        .output();
    matches!((start, set), (Ok(a), Ok(b)) if a.status.success() && b.status.success())
}

/// Evidence that THIS LIVE PROCESS is inside a specific cgroup, observed rather than declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainmentProof {
    /// The cgroup the running process is in, resolved from `/proc/self/cgroup`.
    pub cgroup: PathBuf,
    /// The pid whose membership was observed.
    pub pid: u32,
    /// The unit the parent promised, when one was carried in.
    pub unit: Option<String>,
}

/// Whether the running process could be OBSERVED inside the cgroup it is claimed to be in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainmentEvidence {
    /// Seen, from both directions.
    Observed(ContainmentProof),
    /// Not seen. The string says which check failed, never a guess.
    NotObserved {
        /// The specific observation that failed.
        detail: String,
    },
}

impl ContainmentEvidence {
    /// The proof, when there is one.
    pub fn proof(&self) -> Option<&ContainmentProof> {
        match self {
            ContainmentEvidence::Observed(p) => Some(p),
            ContainmentEvidence::NotObserved { .. } => None,
        }
    }
    /// One clause for a diagnostic.
    pub fn describe(&self) -> String {
        match self {
            ContainmentEvidence::Observed(p) => format!(
                "pid {} observed in {}{}",
                p.pid,
                p.cgroup.display(),
                match &p.unit {
                    Some(u) => format!(" (promised unit {u})"),
                    None => String::new(),
                }
            ),
            ContainmentEvidence::NotObserved { detail } => detail.clone(),
        }
    }
}

/// OBSERVE the running process inside its cgroup. A declaration of containment is not containment.
///
/// WHY THIS EXISTS AT ALL. `in_scope()` answers the question by reading an ENVIRONMENT VARIABLE
/// this process set for itself before re-exec-ing. That sentinel is a promise, and every consumer
/// downstream printed "cgroup boxing ACTIVE" on the strength of it. Anything can export
/// `DAGRUN_IN_SCOPE=1`; a scope can be stopped out from under a live process; systemd can place a
/// unit somewhere other than where it was asked to. None of those show up in an env var.
///
/// TWO DIRECTIONS, DELIBERATELY, because each catches what the other cannot:
///
/// 1. `/proc/self/cgroup` — the KERNEL'S view of where this task is. Being in the cgroup ROOT is
///    the tell that nothing contains us, and it is the case a one-sided check waves through, since
///    every process is in *some* cgroup.
/// 2. `<cgroup>/cgroup.procs` — the CGROUP'S OWN ROSTER. If the directory has been removed, or the
///    task was migrated after `/proc/self/cgroup` was read, the roster disagrees and the claim
///    fails. This is the direction that binds the claim to a live, existing container rather than
///    to a path string.
///
/// And when the parent carried a promised unit name, the observed path must actually end in it —
/// otherwise "we are contained" is true of some cgroup, but not of the one that was arranged, and
/// the caps and the kill path were configured on the other one.
pub fn observe_own_containment(expected_unit: Option<&str>) -> ContainmentEvidence {
    let pid = std::process::id();
    let Some(cgroup) = my_cgroup_path() else {
        return ContainmentEvidence::NotObserved {
            detail: "/proc/self/cgroup carries no cgroup-v2 (0::) entry for this process".into(),
        };
    };
    observe_containment_at(&cgroup, pid, expected_unit)
}

/// The checks of [`observe_own_containment`], against an explicit cgroup and pid.
///
/// Split out so the ROSTER direction can be exercised against a cgroup that genuinely exists but
/// does not list the pid — a process's own parent cgroup is exactly that. Without a seam here the
/// only available test re-derives the roster itself, and then deleting the product's check leaves
/// the test green, which is the inertness this whole change exists to refuse.
pub fn observe_containment_at(
    cgroup: &Path,
    pid: u32,
    expected_unit: Option<&str>,
) -> ContainmentEvidence {
    let cgroup = cgroup.to_path_buf();
    if cgroup == Path::new(CGROUP_ROOT) {
        return ContainmentEvidence::NotObserved {
            detail: format!("pid {pid} is in the cgroup ROOT ({CGROUP_ROOT}); nothing contains it"),
        };
    }
    if !cgroup.is_dir() {
        return ContainmentEvidence::NotObserved {
            detail: format!(
                "pid {pid} claims cgroup {} but that directory does not exist",
                cgroup.display()
            ),
        };
    }
    // THE ROSTER, read from the cgroup side.
    let procs = cgroup.join("cgroup.procs");
    let Ok(text) = fs::read_to_string(&procs) else {
        return ContainmentEvidence::NotObserved {
            detail: format!("cannot read {} to confirm membership", procs.display()),
        };
    };
    if !text.lines().any(|l| l.trim().parse::<u32>() == Ok(pid)) {
        return ContainmentEvidence::NotObserved {
            detail: format!(
                "pid {pid} is NOT listed in {}; the kernel and the cgroup disagree",
                procs.display()
            ),
        };
    }
    if let Some(unit) = expected_unit {
        let observed_here = cgroup
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == unit);
        // A step or supervisor child sits one level BELOW the scope, so an ancestor match counts.
        let observed_above = cgroup.ancestors().any(|a| {
            a.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == unit)
        });
        if !observed_here && !observed_above {
            return ContainmentEvidence::NotObserved {
                detail: format!(
                    "pid {pid} is contained in {}, but the promised unit was {unit}; the caps and \
                     the kill path were arranged on a different cgroup",
                    cgroup.display()
                ),
            };
        }
    }
    ContainmentEvidence::Observed(ContainmentProof {
        cgroup,
        pid,
        unit: expected_unit.map(str::to_string),
    })
}

/// The unit name the parent promised this child, if any.
///
/// Public so a consumer can observe against the SAME promise the re-exec made, rather than
/// inventing its own idea of which cgroup it ought to be in.
pub fn promised_unit() -> Option<String> {
    std::env::var(ENV_SCOPE_UNIT).ok().filter(|u| !u.is_empty())
}

/// Why a call to [`attempt_scope_reexec`] RETURNED instead of exec-ing into a scope.
///
/// THE BOOL THIS REPLACES RETURNED SUCCESS TO MEAN DID-NOT-ATTEMPT, and that single conflation
/// cost a day. `true` covered both "we are already boxed, proceed" and "policy said skip, we never
/// asked whether boxing was possible"; `false` covered both "the probe said no" and "the exec
/// failed". The only caller then folded ALL FOUR back into one error, choosing its wording from the
/// bool — so on the policy-skip path it printed "boxing was skipped (e.g. CI without a systemd
/// --user scope)", asserting a cause it had never tested. Four capability probes were run against
/// that sentence, on a branch that never executes in the environment being investigated.
///
/// So the outcomes are named, and a caller that wants to report or act on the difference now can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeAttempt {
    /// This process is ALREADY inside the managed scope, and that was OBSERVED, not read off an
    /// env var: the live pid was found in the cgroup, from both directions.
    AlreadyInScope {
        /// What was observed, so a caller can print the actual cgroup rather than a claim.
        proof: ContainmentProof,
    },
    /// The in-scope SENTINEL is set but the running process could NOT be observed inside a
    /// cgroup. The caller must proceed (re-exec-ing again would recurse forever on the same
    /// sentinel) but MUST NOT report containment: this is the case where every consumer used to
    /// print "boxing ACTIVE" on the strength of an environment variable.
    SentinelWithoutContainment {
        /// Which observation failed.
        detail: String,
    },
    /// No attempt was made, by policy — currently the `CI`/`GITHUB_ACTIONS` skip. Says nothing
    /// about whether boxing is possible here, because nothing was asked.
    SkippedByPolicy {
        /// The environment variable whose presence selected the skip.
        reason: &'static str,
    },
    /// An attempt WAS made and the environment cannot support it. The string is the specific
    /// failure, not a guess.
    Unavailable {
        /// What was tried and what it reported.
        detail: String,
    },
    /// The scope was buildable but `execve` came back, which means it failed.
    ExecFailed {
        /// The `execve` error.
        detail: String,
    },
}

impl ScopeAttempt {
    /// Whether the caller may proceed to run (as opposed to refusing).
    ///
    /// EXACTLY THE BOOL THIS REPLACES, so no policy rides in the change: the two "carry on"
    /// outcomes are the two that returned `true`. What is new is that a caller can ask WHICH,
    /// instead of being handed one bit that answers a different question.
    pub fn may_proceed(&self) -> bool {
        matches!(
            self,
            ScopeAttempt::AlreadyInScope { .. }
                | ScopeAttempt::SkippedByPolicy { .. }
                | ScopeAttempt::SentinelWithoutContainment { .. }
        )
    }

    /// Whether containment was actually established (only true when we are inside the scope).
    ///
    /// This is the predicate every caller that says "boxing ACTIVE" should have been using.
    pub fn is_contained(&self) -> bool {
        matches!(self, ScopeAttempt::AlreadyInScope { .. })
    }

    /// The observed containment evidence, when there is any. `None` for every other outcome —
    /// which is the point: skipped and unavailable can never produce a proof, so they can never
    /// qualify.
    pub fn proof(&self) -> Option<&ContainmentProof> {
        match self {
            ScopeAttempt::AlreadyInScope { proof } => Some(proof),
            _ => None,
        }
    }

    /// Whether the capability question was even asked. `false` for the policy skip.
    pub fn attempted(&self) -> bool {
        matches!(
            self,
            ScopeAttempt::Unavailable { .. } | ScopeAttempt::ExecFailed { .. }
        )
    }

    /// One clause naming what happened, for a caller composing a diagnostic.
    pub fn describe(&self) -> String {
        match self {
            ScopeAttempt::AlreadyInScope { proof } => format!(
                "pid {} observed in {}{}",
                proof.pid,
                proof.cgroup.display(),
                match &proof.unit {
                    Some(u) => format!(" (promised unit {u})"),
                    None => String::new(),
                }
            ),
            ScopeAttempt::SentinelWithoutContainment { detail } => format!(
                "the in-scope sentinel is set but containment could NOT be observed ({detail}); \
                 proceeding UNCONTAINED rather than claiming boxing on an environment variable"
            ),
            ScopeAttempt::SkippedByPolicy { reason } => format!(
                "scope setup was SKIPPED BY POLICY because ${reason} is set; whether boxing is \
                 possible here was NOT tested (set {FORCE_ATTEMPT_ENV}=1 to find out)"
            ),
            ScopeAttempt::Unavailable { detail } => {
                format!("scope setup was attempted and failed: {detail}")
            }
            ScopeAttempt::ExecFailed { detail } => {
                format!("the scope was created but exec failed: {detail}")
            }
        }
    }
}

/// Set to `1` to run the capability probe even under `CI`/`GITHUB_ACTIONS`.
///
/// THE MEASUREMENT INSTRUMENT THE POLICY SKIP REMOVED. Ephemeral hosted runners are a population
/// nobody has measured precisely because the skip means the probe never runs there. This makes the
/// question answerable on any runner without editing code or changing the default, which stays
/// exactly as it was.
pub const FORCE_ATTEMPT_ENV: &str = "DAGRUN_FORCE_SCOPE_ATTEMPT";

/// Whether policy says to skip scope setup here, and which variable said so.
fn policy_skip_reason() -> Option<&'static str> {
    if std::env::var(FORCE_ATTEMPT_ENV)
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return None;
    }
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        return Some("GITHUB_ACTIONS");
    }
    if std::env::var("CI").is_ok() {
        return Some("CI");
    }
    None
}

/// Re-execute this process inside a delegated transient user scope.
///
/// A successful `exec` replaces the process and does not return. A `true` return means the process
/// was already in scope or scope setup is intentionally skipped in CI. A `false` return means the
/// caller must refuse to continue because the requested containment could not be established.
/// Re-exec into a delegated transient user scope, reporting WHICH outcome occurred.
///
/// THE ONLY PUBLIC ENTRY POINT, and that is deliberate. The bool forms that used to sit beside it
/// returned `true` for both "already contained" and "skipped, never attempted", so every consumer
/// that read one could not tell containment from a no-op — and each of them printed "boxing
/// ACTIVE" anyway. Keeping a convenience bool "so out-of-tree callers keep compiling" would have
/// kept the ambiguity reachable, which is the defect, so the bools are gone rather than
/// deprecated. See [`ScopeAttempt`], and [`ScopeAttempt::proof`] for the only outcome that can
/// carry observed containment.
pub fn attempt_scope_reexec(
    memory_max: Option<i64>,
    cpu_count: Option<i64>,
    runtime_max_s: Option<i64>,
) -> ScopeAttempt {
    attempt_scope_reexec_inner(memory_max, cpu_count, runtime_max_s)
}

/// Env var carrying the outer scope's requested `RuntimeMaxSec` into the in-scope child.
const EXPECTED_RUNTIME_MAX_ENV: &str = "DAGRUN_EXPECTED_RUNTIME_MAX_SEC";

/// The `RuntimeMaxSec` the parent asked systemd to enforce on this run's scope, if any.
pub fn expected_scope_runtime_max_s() -> Option<i64> {
    std::env::var(EXPECTED_RUNTIME_MAX_ENV)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
}

/// Confirm the OUTER scope really carries the `RuntimeMaxSec` that was requested.
///
/// PROXY BINDING: passing `-p RuntimeMaxSec=N` to `systemd-run` is a request, not enforcement.
/// This reads the property back off the live unit (`systemctl --user show`, `RuntimeMaxUSec` in
/// microseconds) and compares it to what was asked for, so "the run is bounded" is a statement
/// about the running unit rather than about an argument vector. A mismatch is reported and
/// returns false; the caller decides whether that is fatal.
pub fn verify_scope_runtime_max(expected_s: i64) -> bool {
    let Some(unit) = std::env::var(ENV_SCOPE_UNIT).ok().filter(|u| !u.is_empty()) else {
        warn("outer RuntimeMaxSec audit unavailable: scope unit name not carried into this child");
        return false;
    };
    let out = Command::new("systemctl")
        .args([
            "--user",
            "show",
            &unit,
            "--property=RuntimeMaxUSec",
            "--value",
        ])
        .output();
    let Ok(out) = out else {
        warn("outer RuntimeMaxSec audit unavailable: systemctl could not be run");
        return false;
    };
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let actual_s = parse_systemd_duration_secs(&raw);
    match actual_s {
        Some(actual) if actual == expected_s => {
            eprintln!(
                "{LOG_PREFIX} outer scope run budget ENFORCED: {unit} RuntimeMaxSec={expected_s}s \
                 (read back from the live unit)."
            );
            true
        }
        _ => {
            warn(&format!(
                "outer RuntimeMaxSec readback MISMATCH on {unit}: requested {expected_s}s, unit \
                 reports {raw:?}; the outer scope bound is NOT proven"
            ));
            false
        }
    }
}

/// Seconds from a systemd-rendered duration, or `None` for `infinity`/unparsable.
///
/// A `*USec` property is NOT necessarily printed as a number. `systemctl show --value` renders it
/// however the running systemd chooses: this box (systemd 259) prints `"1min 6s"` where an integer
/// microsecond count was assumed, and an integer parser therefore read a correctly-enforced 66s
/// bound as unproven and failed the run closed. Accepts a bare microsecond integer and the
/// human form (`us`/`usec`, `ms`, `s`/`sec`, `min`, `h`, `d`, `w`), summing the parts.
pub fn parse_systemd_duration_secs(raw: &str) -> Option<i64> {
    let text = raw.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("infinity") {
        return None;
    }
    if let Ok(usec) = text.parse::<u64>() {
        return Some((usec / 1_000_000) as i64);
    }
    let mut total = 0f64;
    let mut saw_one = false;
    for token in text.split_whitespace() {
        let split = token
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(token.len());
        let (num, unit) = token.split_at(split);
        let Ok(value) = num.parse::<f64>() else {
            return None;
        };
        let scale = match unit.trim() {
            "us" | "usec" | "µs" => 1e-6,
            "ms" | "msec" => 1e-3,
            "" | "s" | "sec" | "seconds" | "second" => 1.0,
            "min" | "m" | "minutes" | "minute" => 60.0,
            "h" | "hr" | "hours" | "hour" => 3600.0,
            "d" | "days" | "day" => 86400.0,
            "w" | "weeks" | "week" => 604800.0,
            _ => return None,
        };
        total += value * scale;
        saw_one = true;
    }
    if saw_one {
        Some(total.round() as i64)
    } else {
        None
    }
}

/// Re-exec into a scope, optionally asking systemd to enforce an outer wall budget on it.
///
/// `runtime_max_s` becomes the scope's `RuntimeMaxSec`. This is the OUTERMOST bound the machine
/// itself will enforce, and it is a LAST RESORT rather than the working timeout: systemd kills the
/// whole scope, so anything the runner had not already flushed to disk is lost. Set it strictly
/// LARGER than the runner's own in-process run budget, so the ordering is
/// per-step < in-process run budget < scope RuntimeMaxSec < whatever the CI provider does. Each
/// level exists to stop the next one from being the thing that fires.
fn attempt_scope_reexec_inner(
    memory_max: Option<i64>,
    cpu_count: Option<i64>,
    runtime_max_s: Option<i64>,
) -> ScopeAttempt {
    if in_scope() {
        // THE SENTINEL SAYS WE ARE BOXED; GO AND LOOK. Everything downstream keyed "boxing ACTIVE"
        // off this branch, and this branch used to be an env-var read.
        // A PROMISED UNIT IS REQUIRED, and demanding it is what makes the observation mean
        // something: the re-exec sets sentinel and unit TOGETHER, and without a unit "observed in
        // some cgroup" is true of almost every process on a cgroup-v2 host — which would wave
        // through exactly the forged claim this check exists to catch.
        let evidence = match promised_unit() {
            Some(unit) => observe_own_containment(Some(&unit)),
            None => ContainmentEvidence::NotObserved {
                detail: "the in-scope sentinel is set but no scope unit was carried with it; the \
                         re-exec always sets both, so this claim names no cgroup to check"
                    .to_string(),
            },
        };
        return match evidence {
            ContainmentEvidence::Observed(proof) => ScopeAttempt::AlreadyInScope { proof },
            ContainmentEvidence::NotObserved { detail } => {
                ScopeAttempt::SentinelWithoutContainment { detail }
            }
        };
    }
    // POLICY SKIP, UNCHANGED IN EFFECT AND NOW HONEST ABOUT ITSELF. It is stated rather than
    // silent, it no longer borrows the capability probe's wording for a probe it did not run, and
    // FORCE_ATTEMPT_ENV can lift it for one run. Whether the skip is CORRECT is a separate
    // question, deliberately left open here.
    if let Some(reason) = policy_skip_reason() {
        eprintln!(
            "{LOG_PREFIX} scope setup SKIPPED BY POLICY (${reason} is set). This did NOT test \
             whether cgroup boxing is available here; set {FORCE_ATTEMPT_ENV}=1 to probe instead \
             of skipping."
        );
        return ScopeAttempt::SkippedByPolicy { reason };
    }
    let memory_max = memory_max.or_else(outer_memory_max_bytes);
    if memory_max.is_none() {
        eprintln!(
            "{LOG_PREFIX} ERROR: cannot derive a positive outer MemoryMax; refusing an \
             unbounded scope."
        );
        return ScopeAttempt::Unavailable {
            detail: "cannot derive a positive outer MemoryMax from MemAvailable/\
                     $DAGRUN_OUTER_MEMORY_MAX_BYTES"
                .to_string(),
        };
    }
    if !systemd_scope_available() {
        eprintln!(
            "{LOG_PREFIX} ERROR: systemd --user scope is unavailable; refusing advisory-only \
             containment."
        );
        return ScopeAttempt::Unavailable {
            detail: "`systemd-run --user --scope` probe failed".to_string(),
        };
    }

    let pid = std::process::id();
    let unit = format!("{UNIT_PREFIX}-{pid}");
    let mut args: Vec<String> = vec![
        "--user".into(),
        "--scope".into(),
        "--collect".into(),
        "--quiet".into(),
        format!("--unit={unit}"),
        "-p".into(),
        "Delegate=yes".into(),
        "-p".into(),
        "MemorySwapMax=0".into(),
    ];
    if let Some(cpu) = cpu_count {
        let Some(quota_percent) = cpu.checked_mul(100) else {
            eprintln!(
                "{LOG_PREFIX} ERROR: requested CPU count {cpu} is too large to encode as a \
                 systemd CPUQuota percentage."
            );
            return ScopeAttempt::Unavailable {
                detail: format!(
                    "requested CPU count {cpu} overflows the systemd CPUQuota percentage"
                ),
            };
        };
        args.push("-p".into());
        args.push(format!("CPUQuota={quota_percent}%"));
        eprintln!(
            "{LOG_PREFIX} per-run CPU cap: CPUQuota={quota_percent}% ({cpu} total core{}).",
            if cpu == 1 { "" } else { "s" }
        );
    }
    if ensure_aggregate_slice(DEFAULT_CPU_BUDGET_FRACTION) {
        args.push(format!("--slice={SLICE_NAME}"));
        eprintln!(
            "{LOG_PREFIX} CPU cap: shared {SLICE_NAME} CPUQuota={}% (~90% of {} cores, AGGREGATE \
             across concurrent runs).",
            cpu_quota_percent(DEFAULT_CPU_BUDGET_FRACTION),
            crate::sizing::cpu_count()
        );
    }
    if let Some(m) = memory_max {
        args.push("-p".into());
        args.push(format!("MemoryMax={m}"));
    }
    if let Some(secs) = runtime_max_s.filter(|s| *s > 0) {
        args.push("-p".into());
        args.push(format!("RuntimeMaxSec={secs}"));
        args.push(format!("--setenv={EXPECTED_RUNTIME_MAX_ENV}={secs}"));
        eprintln!(
            "{LOG_PREFIX} outer scope run budget: RuntimeMaxSec={secs}s (systemd terminates the \
             whole scope; the runner's own budget must fire first)."
        );
    }
    // Scope-wide build width. When the operator stated none, this is derived from the granted
    // cores + memory cap, so a command run directly in the scope (not via a per-step child) still
    // can't compute NUM_JOBS=<all cores>, and per-step prepare_command refines it downward. When
    // the operator DID state one, theirs wins.
    let build_jobs = crate::sizing::select_build_jobs(cpu_count, memory_max);
    eprintln!("{LOG_PREFIX} {}", build_jobs.describe());
    args.push(format!(
        "--setenv={}={}",
        crate::sizing::BUILD_JOBS_ENV,
        build_jobs.jobs
    ));
    // ACROSS THE RE-EXEC, INTENT TRAVELS UNDER ITS OWN NAME. The line above puts the runner's own
    // number into the child's CARGO_BUILD_JOBS; without this the child would read it back as an
    // operator instruction and stop refining per step.
    args.push(format!(
        "--setenv={}={}",
        crate::sizing::OPERATOR_BUILD_JOBS_ENV,
        build_jobs
            .operator
            .map_or_else(String::new, |value| value.to_string())
    ));

    args.push(format!("--setenv={ENV_IN_SCOPE}=1"));
    args.push(format!("--setenv={ENV_SCOPE_UNIT}={unit}.scope"));
    args.push(format!(
        "--setenv={EXPECTED_OUTER_MEMORY_MAX_ENV}={}",
        memory_max.map_or_else(String::new, |value| value.to_string())
    ));
    args.push(format!(
        "--setenv={EXPECTED_OUTER_CPU_COUNT_ENV}={}",
        cpu_count.map_or_else(String::new, |value| value.to_string())
    ));
    args.push("--".into());

    match std::env::current_exe() {
        Ok(exe) => args.push(exe.to_string_lossy().into_owned()),
        Err(e) => {
            eprintln!(
                "{LOG_PREFIX} ERROR: cannot resolve own executable ({e}); refusing to run \
                       without cgroup enforcement."
            );
            return ScopeAttempt::Unavailable {
                detail: format!("cannot resolve own executable ({e})"),
            };
        }
    }
    args.extend(std::env::args().skip(1));

    eprintln!(
        "{LOG_PREFIX} re-exec inside transient systemd scope {unit}.scope (two-level cgroup; \
         full-descendant cleanup on exit)…"
    );
    // exec replaces this process on success; it only returns on error.
    let err = Command::new("systemd-run").args(&args).exec();
    eprintln!(
        "{LOG_PREFIX} ERROR: systemd-run exec failed ({err}); refusing to run without cgroup \
         enforcement."
    );
    ScopeAttempt::ExecFailed {
        detail: err.to_string(),
    }
}

/// The OUTER scope's cgroup path, derived from THIS process's own cgroup (no systemctl call).
fn scope_cgroup_from_self() -> Option<PathBuf> {
    let mine = my_cgroup_path()?;
    let name = mine.file_name()?.to_str()?;
    if name == SUPERVISOR {
        return mine.parent().map(Path::to_path_buf);
    }
    if name.ends_with(".scope") {
        return Some(mine);
    }
    None
}

/// Verify that every requested outer control took effect in cgroup v2.
///
/// A successful systemd property write is not proof of enforcement. The
/// re-exec'd child reads the kernel files and refuses containment unless the
/// numeric cap, swap disable, and group-OOM bit all match.
pub fn verify_scope_limits(expected_memory_max: i64, expected_cpu_count: Option<i64>) -> bool {
    let Some(scope) = scope_cgroup_from_self() else {
        eprintln!("{LOG_PREFIX} ERROR: outer cgroup limit audit unavailable: scope not found");
        return false;
    };
    verify_scope_limits_at(&scope, expected_memory_max, expected_cpu_count)
}

fn verify_scope_limits_at(
    scope: &Path,
    expected_memory_max: i64,
    expected_cpu_count: Option<i64>,
) -> bool {
    let memory_max = read_trim(scope, "memory.max");
    let memory_swap_max = read_trim(scope, "memory.swap.max");
    let memory_oom_group = read_trim(scope, "memory.oom.group");
    let cpu_max = read_trim(scope, "cpu.max");
    let memory_ok = memory_limit_matches(
        expected_memory_max,
        memory_max
            .as_deref()
            .and_then(|value| value.parse::<i64>().ok()),
    );
    let swap_ok = memory_swap_max.as_deref() == Some("0");
    let oom_group_ok = memory_oom_group.as_deref() == Some("1");
    let cpu_ok = match expected_cpu_count {
        None => true,
        Some(expected) => cpu_max
            .as_deref()
            .and_then(|value| {
                let mut parts = value.split_whitespace();
                let quota = parts.next()?.parse::<i64>().ok()?;
                let period = parts.next()?.parse::<i64>().ok()?;
                (parts.next().is_none() && period > 0).then_some((quota, period))
            })
            .is_some_and(|(quota, period)| expected.checked_mul(period) == Some(quota)),
    };
    eprintln!(
        "{LOG_PREFIX} outer cgroup audit: memory.max={} ({}), memory.swap.max={} ({}), \
         memory.oom.group={} ({}), cpu.max={} ({})",
        memory_max.as_deref().unwrap_or("UNREADABLE"),
        if memory_ok { "bound" } else { "MISMATCH" },
        memory_swap_max.as_deref().unwrap_or("UNREADABLE"),
        if swap_ok { "disabled" } else { "MISMATCH" },
        memory_oom_group.as_deref().unwrap_or("UNREADABLE"),
        if oom_group_ok { "enabled" } else { "MISMATCH" },
        cpu_max.as_deref().unwrap_or("UNREADABLE"),
        if cpu_ok { "bound" } else { "MISMATCH" },
    );
    memory_ok && swap_ok && oom_group_ok && cpu_ok
}

/// Write and read back `memory.oom.group=1` on the outer scope.
///
/// The systemd version on supported hosts may reject `MemoryOOMGroup=` as a
/// unit property, so the scoped child writes cgroup v2 directly. The write is
/// not trusted until the kernel file reads back as `1`.
pub fn enable_outer_oom_group() -> bool {
    let Some(scope) = scope_cgroup_from_self() else {
        warn("outer memory.oom.group: scope not found");
        return false;
    };
    enable_outer_oom_group_at(&scope)
}

fn enable_outer_oom_group_at(scope: &Path) -> bool {
    let control = scope.join("memory.oom.group");
    if let Err(error) = fs::write(&control, "1") {
        warn(&format!("outer memory.oom.group=1 write failed ({error})"));
        return false;
    }
    let actual = read_trim(scope, "memory.oom.group");
    if actual.as_deref() != Some("1") {
        warn(&format!(
            "outer memory.oom.group readback mismatch: {}",
            actual.as_deref().unwrap_or("UNREADABLE")
        ));
        return false;
    }
    true
}

// --------------------------------------------------------------------------- //
// Opt-in size-K core box (whole-tree cpuset) — mirror of cgroup.py             //
// --------------------------------------------------------------------------- //
//
// `--cores K` constrains the whole run process tree to K least-busy free cores.
// It never pins a fixed core id (a fixed core may be busy):
// it reads THIS process's allowed set, samples `/proc/stat` briefly, and picks
// the K least-busy of them. Only an exact cgroup cpuset is accepted. A process
// affinity mask is not containment because a descendant can replace it.

/// Read THIS process's CPU-affinity mask (`sched_getaffinity`) as a sorted core list.
fn current_affinity() -> Vec<usize> {
    // SAFETY: a zeroed `cpu_set_t` is a valid empty mask; `sched_getaffinity` fills it for pid 0
    // (this process). We pass the type's exact size and a valid &mut, per the man page.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let rc =
        unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return Vec::new();
    }
    let mut cores = Vec::new();
    for c in 0..(8 * std::mem::size_of::<libc::cpu_set_t>()) {
        // SAFETY: `c` is within the cpu_set_t bit range; `CPU_ISSET` only reads a bit.
        if unsafe { libc::CPU_ISSET(c, &set) } {
            cores.push(c);
        }
    }
    cores
}

/// Cumulative DEVICE interrupt count per CPU from `/proc/interrupts`.
///
/// Only numerically-named rows are summed. That restriction is measured, not stylistic: on a
/// 316-CPU host the architectural/IPI rows (`LOC`, `RES`, `CAL`, `TLB`, ...) spanned only
/// 49-4846/s across CPUs — a 3.6x spread with ZERO CPUs above 4x the median — so including them
/// would re-rank cores by "how busy is this CPU", which `/proc/stat` sampling already measures.
/// The device rows on the same host spanned 0.0-1100.6/s with 45.6% of CPUs at exactly zero.
///
/// An empty map means the signal is UNAVAILABLE, never "this host has no interrupts".
fn device_irq_snapshot() -> BTreeMap<usize, u64> {
    match fs::read_to_string("/proc/interrupts") {
        Ok(text) => parse_device_irq_counts(&text),
        Err(_) => BTreeMap::new(),
    }
}

/// Pure parser for `/proc/interrupts` text.
///
/// Split out from the read so it can be exercised on fixture text: the core ordering guarantee
/// is only as good as agreement on what counts as a device row, and that is worth pinning down
/// directly rather than through a live `/proc` read.
pub(crate) fn parse_device_irq_counts(text: &str) -> BTreeMap<usize, u64> {
    let mut out = BTreeMap::new();
    let mut lines = text.lines();
    let header: Vec<&str> = match lines.next() {
        Some(h) => h.split_whitespace().collect(),
        None => return out,
    };
    let ncpu = header.len();
    if ncpu == 0 {
        return out;
    }
    let mut totals = vec![0u64; ncpu];
    for line in lines {
        let colon = match line.find(':') {
            Some(i) if i > 0 => i,
            _ => continue,
        };
        if !line[..colon].trim().chars().all(|c| c.is_ascii_digit())
            || line[..colon].trim().is_empty()
        {
            continue;
        }
        // Only the first `ncpu` whitespace fields are counts; the rest is the chip/name text.
        // A row with FEWER than `ncpu` fields is malformed for this header and is skipped
        // whole, matching Python -- counting its prefix would silently attribute one CPU's
        // interrupts to another and the two engines would then disagree.
        let fields: Vec<&str> = line[colon + 1..].split_whitespace().take(ncpu).collect();
        if fields.len() < ncpu {
            continue;
        }
        for (index, field) in fields.iter().enumerate() {
            if let Ok(value) = field.parse::<u64>() {
                totals[index] += value;
            }
        }
    }
    for (index, name) in header.iter().enumerate() {
        let lowered = name.to_ascii_lowercase();
        if let Some(digits) = lowered.strip_prefix("cpu") {
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                if let Ok(cpu) = digits.parse::<usize>() {
                    out.insert(cpu, totals[index]);
                }
            }
        }
    }
    out
}

/// Per-CPU (idle+iowait, total) jiffies snapshot from `/proc/stat` (keyed by cpu id).
fn proc_stat_snapshot() -> BTreeMap<usize, (u64, u64)> {
    let mut d = BTreeMap::new();
    let text = match fs::read_to_string("/proc/stat") {
        Ok(t) => t,
        Err(_) => return d,
    };
    for line in text.lines() {
        // Per-cpu lines are `cpuN ...`; skip the aggregate `cpu ...` line (no digit after "cpu").
        let rest = match line.strip_prefix("cpu") {
            Some(r) => r,
            None => continue,
        };
        if !rest
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let cid: usize = match parts[0][3..].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let nums: Vec<u64> = parts[1..].iter().filter_map(|p| p.parse().ok()).collect();
        if nums.len() < 5 {
            continue;
        }
        let idle = nums[3] + nums[4]; // idle + iowait (matches Python p[4]+p[5])
        let total: u64 = nums.iter().sum();
        d.insert(cid, (idle, total));
    }
    d
}

/// Compute one CPU's idle fraction only when both cumulative counters moved forward.
///
/// `/proc/stat` can be observed across CPU hotplug or a counter reset. Treating a rollback as a
/// zero delta would rank that CPU as perfectly idle, so an invalid interval is excluded instead.
fn idle_fraction_between(before: (u64, u64), after: (u64, u64)) -> Option<f64> {
    let (idle_before, total_before) = before;
    let (idle_after, total_after) = after;
    if idle_after < idle_before || total_after < total_before {
        return None;
    }
    let total_delta = total_after - total_before;
    Some(if total_delta == 0 {
        1.0
    } else {
        (idle_after - idle_before) as f64 / total_delta as f64
    })
}

fn parse_cpuset(value: Option<&str>) -> Option<HashSet<usize>> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    let mut cpus = HashSet::new();
    for item in value.split(',') {
        let mut bounds = item.splitn(2, '-');
        let start: usize = bounds.next()?.trim().parse().ok()?;
        let end: usize = match bounds.next() {
            Some(value) => value.trim().parse().ok()?,
            None => start,
        };
        if end < start {
            return None;
        }
        for core in start..=end {
            if !cpus.insert(core) {
                return None;
            }
        }
    }
    Some(cpus)
}

/// Pick `k` LEAST-BUSY cores from THIS process's allowed CPU set (never a fixed id).
///
/// Reads the allowed set (`sched_getaffinity`), samples per-CPU idle jiffies from `/proc/stat`
/// over `sample_s` seconds, and returns the `k` cores with the highest idle fraction. `k` is
/// clamped to `[1, len(allowed)]`.
pub fn pick_least_busy_free_cores(k: i64, sample_s: f64) -> Vec<usize> {
    pick_least_busy_free_cores_excluding(k, sample_s, &HashSet::new(), None)
}

/// Pick least-busy allowed cores while excluding cores held by another reservation.
///
/// This is the allocation primitive used by the durable reservation ledger.  Keeping the
/// exclusion in the same sampled selection path prevents two callers from independently choosing
/// the same apparently-idle CPUs.
pub fn pick_least_busy_free_cores_excluding(
    k: i64,
    sample_s: f64,
    exclude: &HashSet<usize>,
    max_irq_rate: Option<f64>,
) -> Vec<usize> {
    if k < 1 || !sample_s.is_finite() || sample_s < 0.0 {
        return Vec::new();
    }
    if max_irq_rate.is_some_and(|limit| !limit.is_finite() || limit < 0.0 || sample_s <= 0.0) {
        return Vec::new();
    }
    let allowed = current_affinity();
    if allowed.is_empty() {
        return Vec::new();
    }
    let available = allowed
        .iter()
        .filter(|core| !exclude.contains(core))
        .count();
    if available == 0 {
        return Vec::new();
    }
    let k = k.clamp(1, available as i64) as usize;
    // One sleep serves both signals, so IRQ awareness does not lengthen the
    // reservation ledger's critical section.
    let a = proc_stat_snapshot();
    let irq_a = device_irq_snapshot();
    let started = Instant::now();
    thread::sleep(Duration::from_secs_f64(sample_s));
    let b = proc_stat_snapshot();
    let irq_b = device_irq_snapshot();
    let elapsed = started.elapsed().as_secs_f64();
    let mut irq_rate: BTreeMap<usize, f64> = BTreeMap::new();
    if !irq_a.is_empty() && !irq_b.is_empty() && elapsed > 0.0 {
        for (cpu, first) in &irq_a {
            if let Some(second) = irq_b.get(cpu).filter(|second| *second >= first) {
                irq_rate.insert(*cpu, (*second - *first) as f64 / elapsed);
            }
        }
    }
    // An explicit budget must be CHECKED, not assumed. When the signal is missing there is
    // nothing to check it against, so refuse rather than return a full set that silently never
    // honoured it.
    if max_irq_rate.is_some() && irq_rate.is_empty() {
        return Vec::new();
    }
    let idle_frac = |c: usize| -> Option<f64> { idle_fraction_between(*a.get(&c)?, *b.get(&c)?) };
    let mut ranked: Vec<usize> = allowed
        .into_iter()
        .filter(|c| !exclude.contains(c) && idle_frac(*c).is_some())
        .filter(|c| match max_irq_rate {
            Some(limit) => irq_rate.get(c).is_some_and(|rate| *rate <= limit),
            None => true,
        })
        .collect();
    // Interrupt rate leads, idle fraction breaks its ties, core id breaks those. The order is
    // TOTAL, which is what lets the two engines agree exactly instead of merely agreeing on the
    // multiset of "quiet enough" cores. Matches Python's (irq_rate, -idle_frac, core_id).
    ranked.sort_by(|&x, &y| {
        let rx = irq_rate.get(&x).copied().unwrap_or(0.0);
        let ry = irq_rate.get(&y).copied().unwrap_or(0.0);
        irq_rate
            .contains_key(&x)
            .cmp(&irq_rate.contains_key(&y))
            .reverse()
            .then_with(|| rx.partial_cmp(&ry).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| {
                idle_frac(y)
                    .unwrap_or(1.0)
                    .partial_cmp(&idle_frac(x).unwrap_or(1.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| x.cmp(&y))
    });
    ranked.truncate(k);
    ranked
}

// Write `cpuset.cpus` on `scope` and VERIFY via `cpuset.cpus.effective` (mirror of Python).
//
// Returns true only when the `cpuset` controller is present on the scope and the effective set
// exactly equals `cores` after the write. On success, best-effort enables `+cpuset` in
// `subtree_control` so
// per-step child cgroups inherit it.
fn try_cgroup_cpuset(scope: &Path, cores: &[usize]) -> bool {
    let controllers: HashSet<String> = read_trim(scope, "cgroup.controllers")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    if !controllers.contains("cpuset") {
        return false;
    }
    let cpulist = cores
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if fs::write(scope.join("cpuset.cpus"), &cpulist).is_err() {
        return false;
    }
    let wanted: HashSet<usize> = cores.iter().copied().collect();
    if parse_cpuset(read_trim(scope, "cpuset.cpus.effective").as_deref()) != Some(wanted) {
        return false;
    }
    // Verified. Let per-step child cgroups inherit the cpuset constraint.
    let _ = fs::write(scope.join("cgroup.subtree_control"), "+cpuset");
    true
}

/// Constrain the WHOLE run process tree to `k` least-busy FREE cores.
///
/// Requires an exact cgroup `cpuset.cpus.effective` match on this runner's own managed scope.
/// Returns `None` with a warning when hard tree-wide pinning is unavailable. It never falls back
/// to process affinity because descendants can replace an inherited affinity mask.
///
/// Call this in the runner BEFORE the scheduler spawns worker threads or forks any step: threads
/// inherit the creator's affinity and forked steps inherit at fork, so an early application covers
/// the whole tree.
pub fn apply_core_box(k: i64) -> Option<(String, Vec<usize>)> {
    if k < 1 {
        warn(&format!("--cores {k}: core count must be >= 1"));
        return None;
    }
    let cores = pick_least_busy_free_cores(k, 0.3);
    if cores.is_empty() {
        warn(&format!(
            "--cores {k}: no allowed CPUs found (sched_getaffinity empty); cannot constrain the \
             run tree to a core box"
        ));
        return None;
    }
    apply_specific_cores(&cores, &format!("--cores {k}"))
}

/// Constrain the whole process tree to an exact, caller-reserved core set.
///
/// The caller must reserve the cores before calling this function. Success means the exact set is
/// the effective cpuset of this runner's own managed scope.
pub fn apply_specific_cores(cores: &[usize], label: &str) -> Option<(String, Vec<usize>)> {
    if cores.is_empty() {
        warn(&format!(
            "{label}: empty core set; cannot constrain the run tree"
        ));
        return None;
    }
    let want = cores.len();

    // Only a scope created and marked by this runner may be mutated. An arbitrary ambient .scope
    // can contain unrelated processes owned by the same user.
    if in_scope() {
        let Some(scope) = scope_cgroup_from_self() else {
            warn(&format!(
                "{label}: managed runner scope could not be resolved"
            ));
            return None;
        };
        if try_cgroup_cpuset(&scope, cores) {
            eprintln!(
                "{LOG_PREFIX} core box: constrained to {want} core(s) {cores:?} via cgroup cpuset"
            );
            return Some(("cgroup cpuset".to_string(), cores.to_vec()));
        }
    }
    warn(&format!(
        "{label}: exact cgroup cpuset unavailable; refusing a soft process-affinity fallback \
         because descendants can escape it"
    ));
    None
}

/// Install a SIGINT/SIGTERM handler that tears down the WHOLE outer scope, then exits.
///
/// Killing only the runner would leave `setsid`-escapee orphans alive in the scope cgroup
/// (killpg and systemd `--collect` both miss them). On signal we instead `cgroup.kill` the whole
/// scope subtree and `systemctl --user stop` the unit. No-op when not in-scope. Uses
/// `signal-hook` so the handler is registered without `unsafe` (safe-rust invariant).
pub fn install_scope_teardown() {
    if !in_scope() {
        return;
    }
    let unit = std::env::var(ENV_SCOPE_UNIT).ok();
    let scope_cg = scope_cgroup_from_self();
    if unit.is_none() && scope_cg.is_none() {
        return;
    }
    let mut signals = match signal_hook::iterator::Signals::new([SIGINT_NUM, SIGTERM_NUM]) {
        Ok(s) => s,
        Err(e) => {
            warn(&format!(
                "could not install scope-teardown signal handler ({e})"
            ));
            return;
        }
    };
    thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            eprintln!(
                "{LOG_PREFIX} signal {sig} — stopping outer scope (tears down all steps + orphans)…"
            );
            if let Some(cg) = &scope_cg {
                let _ = fs::write(cg.join("cgroup.kill"), "1");
            }
            if let Some(u) = &unit {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", u])
                    .output();
            }
            std::process::exit(128 + sig);
        }
    });
}

const SIGINT_NUM: i32 = 2;
const SIGTERM_NUM: i32 = 15;

/// The concrete per-step cgroup manager for a real Linux cgroup-v2 host.
/// What a run's containment ACTUALLY IS, in the form a durable record should carry it.
///
/// WHY THIS IS NOT A BOOL AND NOT `Option::is_some()`. From an `Arc<dyn CgroupManager>` alone a
/// consumer can learn only "there is a manager", and a record built from that reads "boxed" for a
/// manager that contains STEPS but not the run and applies no caps at all. That is the same overclaim this module spent the day removing, relocated into an
/// artifact — and an artifact outlives the run, so it misleads every later reader instead of one.
///
/// FOUR STATES, and the fourth is the load-bearing one. `Unknown` is a first-class value, never a
/// synonym for either of the others: a record that omits containment reads as normal, and "we
/// could not tell" is a different fact from "there was none".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunContainment {
    /// The RUN ITSELF is inside a delegated cgroup with controllers: per-step caps AND
    /// `cgroup.kill`. Carries the observation, so the claim can be rechecked from the record.
    RunBoxed {
        /// The observed cgroup of the running process.
        cgroup: PathBuf,
        /// The unit the parent promised, when one was carried.
        unit: Option<String>,
        /// The pid whose membership was observed.
        pid: u32,
    },
    /// STEPS are moved into a cgroup this process created; the RUNNER is not itself contained and
    /// NO resource caps are enforced. Teardown works; boxing does not. Recording this as "boxed"
    /// would blur exactly the distinction that makes the direct route honest.
    StepsContainedOnly {
        /// The cgroup steps are moved into.
        cgroup: PathBuf,
    },
    /// No containment at all.
    Unboxed {
        /// Why, in the terms the run knows.
        reason: String,
    },
    /// Could not be determined. NOT a default, NOT a synonym for boxed or unboxed.
    Unknown {
        /// What could not be established.
        detail: String,
    },
}

impl RunContainment {
    /// A short, stable token for a ledger column or a grep.
    pub fn label(&self) -> &'static str {
        match self {
            RunContainment::RunBoxed { .. } => "run-boxed",
            RunContainment::StepsContainedOnly { .. } => "steps-contained-only",
            RunContainment::Unboxed { .. } => "unboxed",
            RunContainment::Unknown { .. } => "unknown",
        }
    }

    /// Whether per-step resource CAPS were enforceable. False for every state but `RunBoxed`,
    /// including `Unknown` — an unproven cap is not a cap.
    pub fn caps_enforced(&self) -> bool {
        matches!(self, RunContainment::RunBoxed { .. })
    }

    /// Whether a step's whole subtree was killable via `cgroup.kill`. True for the direct route
    /// too: that route exists precisely because killing works without controllers.
    pub fn subtree_killable(&self) -> bool {
        matches!(
            self,
            RunContainment::RunBoxed { .. } | RunContainment::StepsContainedOnly { .. }
        )
    }

    /// One line for a durable log or banner. Always states which of the four it is.
    pub fn describe(&self) -> String {
        match self {
            RunContainment::RunBoxed { cgroup, unit, pid } => format!(
                "run-boxed: pid {pid} observed in {}{} (per-step caps enforced, subtree killable)",
                cgroup.display(),
                match unit {
                    Some(u) => format!(" (promised unit {u})"),
                    None => String::new(),
                }
            ),
            RunContainment::StepsContainedOnly { cgroup } => format!(
                "steps-contained-only: steps run in {} and the subtree is killable, but the RUNNER \
                 is not contained and NO per-step caps are enforced",
                cgroup.display()
            ),
            RunContainment::Unboxed { reason } => {
                format!("unboxed: no containment ({reason})")
            }
            RunContainment::Unknown { detail } => format!(
                "unknown: containment state could not be determined ({detail}) — this is NOT a \
                 claim that the run was boxed"
            ),
        }
    }
}

/// The containment of a run, from its manager — or its absence.
///
/// The `None` case is `Unboxed` rather than `Unknown` because a caller that holds no manager
/// knows, positively, that no containment was arranged.
pub fn run_containment(manager: Option<&dyn CgroupManager>) -> RunContainment {
    match manager {
        Some(m) if m.enabled() => m.containment_record(),
        Some(_) => RunContainment::Unboxed {
            reason: "a containment manager was supplied but reported itself disabled".into(),
        },
        None => RunContainment::Unboxed {
            reason: "no containment manager was established for this run".into(),
        },
    }
}

/// What a live [`Cgroups`] can actually enforce. These are NOT interchangeable and the difference
/// is deliberately visible at the type level, because "we have a cgroup" and "we can cap a step"
/// are different capabilities, and treating them as one is what hides an unkillable step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// A delegated systemd scope with controllers: per-step memory/CPU caps AND `cgroup.kill`.
    Full,
    /// A cgroup we made ourselves under our own cgroup, with NO controllers delegated. Teardown by
    /// `cgroup.kill` works — that needs no controller — but no resource cap can be applied.
    KillOnly,
}

/// Per-step cgroup-v2 containment for a run: an outer root plus one child cgroup per step.
pub struct Cgroups {
    enabled: bool,
    /// The delegated outer scope cgroup root (parent of the per-step child cgroups).
    root: Option<PathBuf>,
    /// What this manager can enforce. See [`Containment`].
    containment: Containment,
    /// Effective memory ceiling inherited from a validated parent delegation. This survives a
    /// failed local stamp because the physical ancestor still enforces the limit.
    delegated_ancestor_memory_max: Option<i64>,
    /// False when any visible ancestor's `memory.max` was unreadable or malformed. Such a row's
    /// cap is unknown and must never be admitted as an uncensored feedback sample.
    delegated_ancestor_memory_max_known: bool,
    delegated_ancestor_max_cpus: Option<i64>,
    delegated_ancestor_cpu_known: bool,
    /// Restores an in-process caller to the outer step supervisor when this invocation ends.
    nested_coordinator: Option<NestedCoordinatorGuard>,
    /// Tags whose roots were delegated to nested schedulers. Their hierarchical
    /// `memory.events` include child steps already classified by the nested scheduler, so the
    /// owning scheduler reads `memory.events.local` instead. Cleared on cleanup because sweeps
    /// may reuse a tag with the same manager.
    delegated: Mutex<HashSet<String>>,
}

impl Cgroups {
    /// Construct the manager. Only meaningful inside the re-exec'd scope; otherwise disabled.
    pub fn new() -> Self {
        // A process below a delegated outer step inherits the top-level in-scope sentinel but
        // does not own its current supervisor leaf. Only the CLI's validated adoption path may
        // create a manager here; otherwise repeated library calls would build
        // `supervisor/supervisor/...` and invalidate the delegation for their peers.
        if std::env::var_os(DELEGATED_CGROUP_ENV).is_some() {
            return Self::disabled(Containment::Full);
        }
        if !in_scope() {
            return Self::disabled(Containment::Full);
        }
        let scope = match my_cgroup_path() {
            Some(p) if p.is_dir() => p,
            _ => return Self::disabled(Containment::Full),
        };
        Self::at_root(scope, Containment::Full)
    }

    /// Adopt an exact parent-owned delegation root already validated against live membership.
    pub fn from_delegated_root(root: PathBuf) -> Self {
        Self::from_delegated_root_with_limits(root, None, None)
    }

    /// Adopt a delegation while making this invocation's tighter CLI limits physical at its
    /// private aggregate root.
    pub fn from_delegated_root_with_limits(
        root: PathBuf,
        max_cpus: Option<i64>,
        max_mem_bytes: Option<i64>,
    ) -> Self {
        // One outer command may launch Python and Rust schedulers concurrently. Give each live
        // process its own namespace so equal inner step tags cannot alias or clean up a peer.
        let sequence = NEXT_NESTED_ROOT.fetch_add(1, Ordering::Relaxed);
        let run_root = root.join(format!("nested-run-{}-{sequence}", std::process::id()));
        let inherited = match create_nested_run_root(
            &root,
            &run_root,
            Path::new(CGROUP_ROOT),
            max_cpus,
            max_mem_bytes,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn(&format!(
                    "could not create nested scheduler cgroup {} ({error})",
                    run_root.display()
                ));
                return Self::disabled(Containment::Full);
            }
        };
        let mut manager = Self::at_root(run_root, Containment::Full);
        let nested_coordinator = if manager.enabled {
            let run_root = manager
                .root
                .as_deref()
                .expect("an enabled cgroup manager has a root");
            match move_current_to_nested_supervisor(run_root) {
                Ok(guard) => Some(guard),
                Err(mut error) => {
                    warn(&format!(
                        "could not place nested scheduler inside aggregate cgroup {} ({error}); \
                         per-step containment disabled",
                        run_root.display()
                    ));
                    manager.enabled = false;
                    manager.root = None;
                    manager.nested_coordinator = error.recovery.take();
                    return manager;
                }
            }
        } else {
            None
        };
        manager.nested_coordinator = nested_coordinator;
        manager.delegated_ancestor_memory_max = inherited.memory_max_bytes;
        manager.delegated_ancestor_memory_max_known = inherited.memory_known;
        manager.delegated_ancestor_max_cpus = inherited.max_cpus;
        manager.delegated_ancestor_cpu_known = inherited.cpu_known;
        manager
    }

    /// Retry restoration after a partially successful nested migration. Disabled managers use
    /// this before honoring `--allow-cgroup-failure`, so no user step starts while the caller's
    /// placement is uncertain.
    pub(crate) fn restore_nested_coordinator_now(&mut self) -> std::io::Result<()> {
        let Some(mut guard) = self.nested_coordinator.take() else {
            return Ok(());
        };
        match guard.restore() {
            Ok(()) => Ok(()),
            Err(error) => {
                self.nested_coordinator = Some(guard);
                Err(error)
            }
        }
    }

    fn disabled(containment: Containment) -> Self {
        Cgroups {
            enabled: false,
            root: None,
            containment,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        }
    }

    fn at_root(scope: PathBuf, containment: Containment) -> Self {
        let mut cg = Self::disabled(containment);
        if !scope.is_dir() {
            return cg;
        };
        let controllers: HashSet<String> = match read_trim(&scope, "cgroup.controllers") {
            Some(s) => s.split_whitespace().map(String::from).collect(),
            None => {
                warn("outer scope cgroup.controllers unreadable; per-step containment disabled");
                return cg;
            }
        };
        // Drain EVERY process out of the scope root into `supervisor/` so the root holds no
        // processes and may enable controllers for children (cgroup-v2 "no internal processes").
        let sup = scope.join(SUPERVISOR);
        if let Err(e) = make_dir(&sup) {
            warn(&format!(
                "could not create supervisor cgroup {} ({e}); per-step containment disabled",
                sup.display()
            ));
            return cg;
        }
        // Enable each controller independently so one unavailable controller cannot make an
        // atomic multi-controller write fail wholesale. Every write is guarded by a fresh bounded
        // drain and an EBUSY re-drain, closing the check/act gap around late scope members.
        for c in ["memory", "cpu", "pids"] {
            if controllers.contains(c) {
                if let Err(e) = enable_controller(&scope, &sup, c) {
                    warn(&format!(
                        "could not delegate '{c}' controller to per-step cgroups after bounded \
                         drain/retry ({e}); per-step containment disabled"
                    ));
                    return cg;
                }
            }
        }
        cg.root = Some(scope);
        cg.enabled = true;
        cg
    }

    fn child(&self, tag: &str) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join(sanitize(tag)))
    }
}

/// Environment switch that opts a run into the kill-only direct-cgroupfs route.
///
/// OFF BY DEFAULT, deliberately. Turning containment on where it is currently off changes the
/// behaviour of every lane at once, and that is an owner decision, not a library default. This
/// makes the capability available and testable without altering a single existing run.
pub const DIRECT_CGROUP_ENV: &str = "DAGRUN_DIRECT_CGROUP";

impl Cgroups {
    /// Containment WITHOUT systemd: a cgroup we create under our own, torn down by `cgroup.kill`.
    ///
    /// WHY THIS EXISTS. Obtaining a systemd `--user` scope is one WAY to get a cgroup, not the
    /// definition of having one. Where that route is unavailable the runner concluded containment
    /// was impossible and fell back to a process-group kill — which cannot reach a `setsid` or
    /// double-fork escapee, because such a process changes session and pgid but NOT cgroup
    /// membership. The step then never exits, the run never returns, and no measurements are
    /// written, so the lane cannot report on its own failure.
    ///
    /// Measured on the self-hosted privileged runner (a container, root, no systemd as PID 1):
    /// creating a child cgroup and writing `cgroup.kill` terminated a `setsid` escapee that had
    /// demonstrably left the process group. Controller delegation on that same runner returned
    /// EIO at every level, with the cgroup holding no processes and no children — so per-step
    /// memory/CPU caps are NOT available there and this route does not pretend otherwise. It is
    /// [`Containment::KillOnly`]: kill correctly, do not claim to box.
    ///
    /// Returns a disabled manager when a usable cgroup cannot be made, so the caller degrades
    /// exactly as it does today rather than gaining a new failure mode.
    pub fn direct() -> Self {
        let disabled = Cgroups {
            enabled: false,
            root: None,
            containment: Containment::KillOnly,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        };
        let Some(own) = my_cgroup_path() else {
            return disabled;
        };
        if !own.is_dir() {
            return disabled;
        }
        let root = own.join(format!("{UNIT_PREFIX}-direct-{}", std::process::id()));
        if let Err(e) = make_dir(&root) {
            warn(&format!(
                "direct cgroupfs containment unavailable: cannot create {} ({e})",
                root.display()
            ));
            return disabled;
        }
        // PROVE the one capability this route claims, rather than assuming it. `cgroup.kill` is a
        // core cgroup-v2 file and needs no controller delegated; if it is absent the route buys
        // nothing over a process-group kill and must not advertise itself.
        if !root.join("cgroup.kill").exists() {
            warn(&format!(
                "direct cgroupfs containment unavailable: {} has no cgroup.kill",
                root.display()
            ));
            let _ = fs::remove_dir(&root);
            return disabled;
        }
        Cgroups {
            enabled: true,
            root: Some(root),
            containment: Containment::KillOnly,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        }
    }

    /// What this manager can enforce.
    pub fn containment(&self) -> Containment {
        self.containment
    }
}

impl Default for Cgroups {
    fn default() -> Self {
        Cgroups::new()
    }
}

impl CgroupManager for Cgroups {
    fn enabled(&self) -> bool {
        self.enabled
    }

    /// The two routes report DIFFERENT states, which is the point: `Full` boxes the run, `KillOnly`
    /// only moves steps into a cgroup it made. Collapsing them into "boxed" would put the overclaim
    /// back, this time in an artifact that outlives the run.
    fn containment_record(&self) -> RunContainment {
        let Some(root) = self.root.clone() else {
            return RunContainment::Unknown {
                detail: "the manager is enabled but carries no cgroup root".into(),
            };
        };
        match self.containment {
            Containment::KillOnly => RunContainment::StepsContainedOnly { cgroup: root },
            // Full means the RUN is in a delegated scope, so say so only if that is still
            // OBSERVABLE right now. A scope can be stopped under a live process, and a record
            // written from a stale assumption is worse than one that says it did not know.
            Containment::Full => match observe_own_containment(promised_unit().as_deref()) {
                ContainmentEvidence::Observed(p) => RunContainment::RunBoxed {
                    cgroup: p.cgroup,
                    unit: p.unit,
                    pid: p.pid,
                },
                ContainmentEvidence::NotObserved { detail } => RunContainment::Unknown {
                    detail: format!(
                        "the manager reports a delegated scope but containment is not observable \
                         now: {detail}"
                    ),
                },
            },
        }
    }

    fn prepare_command(
        &self,
        tag: &str,
        cmd: &str,
        mem_max: Option<i64>,
        cpu_count: Option<i64>,
    ) -> String {
        let root = match (&self.root, self.enabled) {
            (Some(r), true) => r,
            _ => return cmd.to_string(),
        };
        let encoded_tag = sanitize(tag);
        if encoded_tag.len() > MAX_CGROUP_COMPONENT_BYTES {
            return "echo 'ERROR: encoded step tag exceeds the cgroup filename limit; refusing \
                    uncontained run' >&2\nexit 125\n"
                .to_string();
        }
        let child = root.join(encoded_tag);
        if let Err(e) = make_dir(&child) {
            warn(&format!(
                "step {tag}: could not create child cgroup {} ({e}); refusing to run without the \
                 requested per-step cgroup",
                child.display()
            ));
            return "echo 'ERROR: step cgroup could not be created; refusing uncontained run' \
                    >&2\nexit 125\n"
                .to_string();
        }
        // KILL-ONLY: no controller is delegated on this route, so every cap write below would
        // fail — and the cpu.max branch FAILS THE STEP when it cannot apply. Applying caps is not
        // what this route promises; joining the cgroup so teardown can reach the whole subtree is.
        // Returning early keeps the step running exactly as an unboxed step would, except that it
        // is now killable.
        if self.containment == Containment::KillOnly {
            return join_cgroup_command(&child, cmd);
        }
        // Every step is swapless; also clear any inherited soft cap.
        if let Err(e) = fs::write(child.join("memory.swap.max"), "0") {
            warn(&format!(
                "step {tag}: could not disable swap ({e}); memory controller may not be delegated \
                 — outer cap still applies"
            ));
        }
        let _ = fs::write(child.join("memory.high"), "max");
        // Kill the WHOLE step cgroup as a unit on OOM (`memory.oom.group=1`). Without it the kernel
        // kills one victim process inside whichever cgroup it OOMs: a capped step is left half-dead,
        // and a runaway escalating past its own cap to a shared ancestor gets a victim chosen by
        // badness across ALL steps — so the kill can land on an INNOCENT NEIGHBOUR and be attributed
        // to the wrong PR. Best-effort: a kernel without oom.group must not drop the caps above —
        // but a FAILED write must not pass silently, or the step OOM lands on one victim with no
        // trace of why. Mirrors the Python per-step site so the engines stay at parity.
        if let Err(e) = fs::write(child.join("memory.oom.group"), "1") {
            warn(&format!(
                "step {tag}: could not set memory.oom.group ({e}); an OOM may kill a single \
                 process instead of the whole step (mis-attributed blast radius)"
            ));
        }
        // `memory_max` is one of the guards the `capabilities` manifest advertises, so the
        // decision to apply it is read from the same registry that publishes it: turning the
        // advertisement off turns the write off, and vice versa. See `crate::capabilities`. This
        // method returned early above unless boxing is on, so the lane is CONTAINED by
        // construction — there is no cgroup child to write to on the other one.
        if let Some(m) = mem_max.filter(|_| {
            crate::capabilities::is_enforced("memory_max", crate::capabilities::Lane::Contained)
        }) {
            if let Err(e) = fs::write(child.join("memory.max"), m.to_string()) {
                warn(&format!(
                    "step {tag}: could not apply inner memory cap memory.max={m} ({e}); step runs \
                     under the outer cap only"
                ));
            }
        }
        if let Some(cpu) = cpu_count {
            let period: i64 = 100_000;
            let expected = format!("{} {}", cpu * period, period);
            match fs::write(child.join("cpu.max"), &expected) {
                Ok(()) => {
                    let applied = read_trim(&child, "cpu.max").unwrap_or_default();
                    if applied != expected {
                        return format!(
                            "echo 'ERROR: step {tag} cpu.max mismatch: expected {expected}, got \
                             {applied}' >&2\nexit 1\n"
                        );
                    }
                }
                Err(e) => {
                    return format!(
                        "echo 'ERROR: step {tag} cpu.max could not be applied: {e}' >&2\nexit 1\n"
                    );
                }
            }
        }
        // Carry the build `-j` WITH the caps just written. cargo (and the NUM_JOBS it exports to
        // build scripts) auto-detects parallelism from the effective CPU quota; an UNPINNED step
        // (cpu_count None -> no per-step cpu.max) inherits the wide scope quota and computes
        // NUM_JOBS=<all-granted-cores> (observed 284), OOM-racing the linker (<repo>#1584
        // build.dbi_release, 8.0 GiB cap, oom_kill=2). Derive the cap here, where the quota is
        // granted, from the step's cores+mem if pinned else the SCOPE's effective
        // cpu.max/memory.max. Only CARGO_BUILD_JOBS (never MAKEFLAGS): a global make -j could
        // parallelize a determinism-sensitive target (cf. make -jN nondeterminism #1157). An
        // explicit `cargo -j` in the step command still overrides this env floor.
        //
        // An operator's OWN width, resolved once in the outermost process, is not refined: they
        // sized their caps against that pool. Only the derivation refines downward.
        let mut core_limits: Vec<i64> = [
            cpu_count,
            cpu_max_cores(read_trim(root, "cpu.max")),
            self.delegated_ancestor_max_cpus,
        ]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .collect();
        if !self.delegated_ancestor_cpu_known {
            // The CLI rejects an unreadable inherited envelope. Library callers still get the
            // narrowest representable integer width rather than host-width fan-out.
            core_limits.push(1);
        }
        let eff_cores = core_limits.into_iter().min();
        let eff_mem = [
            mem_max,
            memory_max_bytes(read_trim(root, "memory.max")),
            self.delegated_ancestor_memory_max,
        ]
        .into_iter()
        .flatten()
        .filter(|value| *value >= 0)
        .min();
        let jobs = crate::sizing::select_build_jobs(eff_cores, eff_mem).jobs;
        // $$ is the bash leader's pid; writing it migrates the leader so every subsequently
        // forked descendant inherits this cgroup at fork. A cap on an empty child is not
        // containment, so a failed migration refuses to execute the user command.
        join_cgroup_command(
            &child,
            &format!("export {}={jobs}\n{cmd}", crate::sizing::BUILD_JOBS_ENV),
        )
    }

    fn prepare_delegated_command(
        &self,
        tag: &str,
        cmd: &str,
        mem_max: Option<i64>,
        cpu_count: Option<i64>,
    ) -> PreparedDelegation {
        if !self.enabled || self.containment != Containment::Full {
            return PreparedDelegation {
                command: "echo 'ERROR: delegated_children requires full cgroup containment' >&2\n\
                          exit 125\n"
                    .to_string(),
                root: None,
            };
        }
        let command = self.prepare_command(tag, cmd, mem_max, cpu_count);
        let Some(child) = self.child(tag) else {
            return PreparedDelegation {
                command,
                root: None,
            };
        };
        let ordinary_procs = shell_quote(&child.join("cgroup.procs").to_string_lossy());
        if !command.contains(&ordinary_procs) {
            return PreparedDelegation {
                command,
                root: None,
            };
        }
        let supervisor = child.join(SUPERVISOR);
        let prepared = (|| -> Result<(), std::io::Error> {
            make_dir(&supervisor)?;
            let controllers: HashSet<String> = read_trim(&child, "cgroup.controllers")
                .unwrap_or_default()
                .split_whitespace()
                .map(String::from)
                .collect();
            for controller in ["memory", "cpu", "pids"] {
                if controllers.contains(controller) {
                    enable_controller(&child, &supervisor, controller)?;
                }
            }
            Ok(())
        })();
        if let Err(error) = prepared {
            warn(&format!(
                "step {tag}: could not prepare delegated child cgroup {} ({error}); refusing \
                 nested execution",
                child.display()
            ));
            return PreparedDelegation {
                command: "echo 'ERROR: delegated child cgroup could not be prepared; refusing \
                          nested execution' >&2\nexit 125\n"
                    .to_string(),
                root: None,
            };
        }
        let delegated_procs = shell_quote(&supervisor.join("cgroup.procs").to_string_lossy());
        self.delegated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tag.to_string());
        PreparedDelegation {
            command: command.replacen(&ordinary_procs, &delegated_procs, 1),
            root: Some(child),
        }
    }

    fn kill(&self, tag: &str) -> bool {
        let child = match self.child(tag) {
            Some(c) if self.enabled => c,
            _ => return false,
        };
        match fs::write(child.join("cgroup.kill"), "1") {
            Ok(()) => true,
            Err(e) => {
                warn(&format!(
                    "step {tag}: cgroup.kill write failed ({e}); falling back to process-group \
                     kill for this step"
                ));
                false
            }
        }
    }

    fn cleanup(&self, tag: &str) {
        if let Some(child) = self.child(tag) {
            remove_empty_cgroup_descendants(&child);
            let _ = fs::remove_dir(child); // EBUSY is fine; the outer-scope stop flushes it
        }
        self.delegated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(tag);
    }

    // Derived from `memory_events` rather than parsing `memory.events` a second time, so the OOM
    // count and the recorded event counters can never disagree about the same step.
    fn oom_kills(&self, tag: &str) -> i64 {
        self.memory_events(tag)
            .and_then(|events| events.get("oom_kill").copied())
            .unwrap_or(0)
    }

    fn memory_events(&self, tag: &str) -> Option<BTreeMap<String, i64>> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        // `memory.events` is hierarchical. For a delegated step it therefore includes an inner
        // step's independently handled OOM and would make the successful outer scheduler fail a
        // second time. `memory.events.local` excludes those descendant-owned events while still
        // counting an OOM charged at this delegation root's own memory.max (even when the victim
        // lives below it), preserving the outer cap as an actionable backstop.
        let delegated = self
            .delegated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(tag);
        let filename = if delegated {
            "memory.events.local"
        } else {
            "memory.events"
        };
        let text = read_trim(&child, filename)?;
        // A line that does not parse as `<name> <integer>` is SKIPPED, not fatal: the kernel is
        // free to add counters, and discarding the whole file over one unrecognised line would
        // blank all five CSV cells and silently drop the recorded OOM count. The Python build
        // skips per line too, and the two must not disagree on the same file.
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                if let Ok(n) = v.parse::<i64>() {
                    out.insert(k.to_string(), n);
                }
            }
        }
        Some(out)
    }

    fn applied_memory_max(&self, tag: &str) -> Option<String> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        if !self.delegated_ancestor_memory_max_known {
            return None;
        }
        let own = read_trim(&child, "memory.max").filter(|value| !value.is_empty())?;
        // Hierarchical: the step's OWN memory.max is not the ceiling it ran under. The runner
        // always installs an outer scope cap, so a step given no inner cap reads "max" here
        // while the scope's ceiling is what held it — and the child's memory.events does not
        // count an ancestor's limit events, so nothing else in the row would show it. Report the
        // tighter of the two, because that is the number peak_bytes is interpretable against.
        let root_limit = self
            .root
            .as_ref()
            .and_then(|root| memory_max_bytes(read_trim(root, "memory.max")));
        let tightest = [
            memory_max_bytes(Some(own.clone())),
            root_limit,
            self.delegated_ancestor_memory_max,
        ]
        .into_iter()
        .flatten()
        .min();
        Some(tightest.map_or(own, |bytes| bytes.to_string()))
    }

    fn peak_bytes(&self, tag: &str) -> Option<i64> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        read_trim(&child, "memory.peak").and_then(|s| s.parse().ok())
    }

    fn cpu_stats(&self, tag: &str) -> Option<BTreeMap<String, i64>> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        let text = read_trim(&child, "cpu.stat")?;
        let mut out = BTreeMap::new();
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                if let Ok(n) = v.parse::<i64>() {
                    out.insert(k.to_string(), n);
                }
            }
        }
        Some(out)
    }

    fn run_cpu_usage_usec(&self) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        let root = self.root.as_ref()?;
        let text = read_trim(root, "cpu.stat")?;
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            match (parts.next(), parts.next(), parts.next()) {
                (Some("usage_usec"), Some(value), None) => value.parse().ok(),
                _ => None,
            }
        })
    }

    fn cpu_pressure(&self, tag: &str) -> Option<BTreeMap<String, f64>> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        let text = read_trim(&child, "cpu.pressure")?;
        let some = text.lines().find(|l| l.starts_with("some "))?;
        let mut out = BTreeMap::new();
        for item in some.split_whitespace().skip(1) {
            if let Some((k, v)) = item.split_once('=') {
                if matches!(k, "avg10" | "avg60") {
                    if let Ok(n) = v.parse::<f64>() {
                        out.insert(k.to_string(), n);
                    }
                }
            }
        }
        // Match the Python reader: both avg10 and avg60 must be present, else None.
        if out.contains_key("avg10") && out.contains_key("avg60") {
            Some(out)
        } else {
            None
        }
    }

    fn thread_count(&self, tag: &str) -> Option<i64> {
        let child = self.child(tag)?;
        if !self.enabled {
            return None;
        }
        let text = read_trim(&child, "cgroup.threads")?;
        Some(text.lines().filter(|l| !l.is_empty()).count() as i64)
    }

    fn kill_all_remaining(&self) -> i64 {
        let root = match (&self.root, self.enabled) {
            (Some(r), true) => r,
            _ => return 0,
        };
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        let mut n = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let is_step = path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with(STEP_PREFIX));
            if !is_step {
                continue;
            }
            n += 1;
            if let Err(e) = fs::write(path.join("cgroup.kill"), "1") {
                warn(&format!(
                    "backstop: cgroup.kill on {} failed ({e}); a leftover orphan may survive",
                    path.display()
                ));
            }
            remove_empty_cgroup_descendants(&path);
            let _ = fs::remove_dir(&path);
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    #[test]
    fn delegated_root_requires_exact_supervisor_membership_and_kill_boundary() {
        let base =
            std::env::temp_dir().join(format!("dagrun-delegated-root-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("step-owner");
        let supervisor = root.join(SUPERVISOR);
        fs::create_dir_all(&supervisor).unwrap();
        fs::write(root.join("cgroup.kill"), "").unwrap();
        fs::write(
            supervisor.join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();

        assert_eq!(
            validate_delegated_cgroup_root_at(&root, &supervisor, std::process::id(), &base,)
                .unwrap(),
            root.canonicalize().unwrap()
        );
        let sibling = base.join("sibling");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(
            sibling.join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        assert!(
            validate_delegated_cgroup_root_at(&root, &sibling, std::process::id(), &base,)
                .unwrap_err()
                .contains("not the exact")
        );
        assert!(
            validate_delegated_cgroup_root_at(&base, &supervisor, std::process::id(), &base,)
                .unwrap_err()
                .contains("outside")
        );
        fs::write(supervisor.join("cgroup.procs"), "999999\n").unwrap();
        assert!(
            validate_delegated_cgroup_root_at(&root, &supervisor, std::process::id(), &base,)
                .unwrap_err()
                .contains("is not listed")
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scope_drain_waits_past_first_empty_sample_for_late_member() {
        // This is the production race: systemd's initial members are drained, one read reports
        // empty, then a late helper enters before subtree_control is enabled. Requiring two
        // empty samples makes the late pid observable and movable rather than returning early.
        let mut observations = VecDeque::from([
            vec!["runner".to_string()],
            vec![],
            vec!["late-systemd-helper".to_string()],
            vec![],
            vec![],
        ]);
        let mut moved = Vec::new();
        let mut sleeps = 0;
        drain_scope_root_with(
            || Ok(observations.pop_front().unwrap_or_default()),
            |pid| {
                moved.push(pid.to_string());
                Ok(())
            },
            |_| sleeps += 1,
        )
        .unwrap();
        assert_eq!(moved, ["runner", "late-systemd-helper"]);
        assert_eq!(sleeps, 4);
    }

    #[test]
    fn scope_drain_refuses_a_root_that_never_quiesces() {
        let mut moves = 0;
        let error = drain_scope_root_with(
            || Ok(vec!["persistent-member".to_string()]),
            |_| {
                moves += 1;
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "planted refusal",
                ))
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("persistent-member"), "{error}");
        assert_eq!(moves, SCOPE_DRAIN_ATTEMPTS);
    }

    #[test]
    fn scope_drain_propagates_an_unreadable_roster() {
        let mut moved = false;
        let error = drain_scope_root_with(
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "planted unreadable cgroup.procs",
                ))
            },
            |_| {
                moved = true;
                Ok(())
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!moved, "an unreadable roster must not be treated as empty");
    }

    #[test]
    fn controller_enable_redrains_a_member_that_arrives_in_the_check_act_gap() {
        let mut observations = VecDeque::from([
            vec![],
            vec![],
            vec!["late-between-check-and-write".to_string()],
            vec![],
            vec![],
        ]);
        let mut moved = Vec::new();
        let mut writes = 0;
        enable_controller_with(
            || Ok(observations.pop_front().unwrap_or_default()),
            |pid| {
                moved.push(pid.to_string());
                Ok(())
            },
            || {
                writes += 1;
                if writes == 1 {
                    Err(std::io::Error::from_raw_os_error(libc::EBUSY))
                } else {
                    Ok(())
                }
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(writes, 2);
        assert_eq!(moved, ["late-between-check-and-write"]);
    }

    /// A manager that does NOT override `containment_record`, i.e. an out-of-tree implementor.
    struct SilentManager {
        on: bool,
    }
    impl CgroupManager for SilentManager {
        fn enabled(&self) -> bool {
            self.on
        }
        fn prepare_command(&self, _t: &str, c: &str, _m: Option<i64>, _p: Option<i64>) -> String {
            c.to_string()
        }
        fn kill(&self, _t: &str) -> bool {
            false
        }
        fn cleanup(&self, _t: &str) {}
        fn oom_kills(&self, _t: &str) -> i64 {
            0
        }
        fn peak_bytes(&self, _t: &str) -> Option<i64> {
            None
        }
        fn cpu_stats(&self, _t: &str) -> Option<BTreeMap<String, i64>> {
            None
        }
        fn thread_count(&self, _t: &str) -> Option<i64> {
            None
        }
        fn cpu_pressure(&self, _t: &str) -> Option<BTreeMap<String, f64>> {
            None
        }
        fn kill_all_remaining(&self) -> i64 {
            0
        }
    }

    #[test]
    fn silence_records_unknown_and_never_boxed() {
        // THE NON-NEGOTIABLE CLAUSE. An implementor that says nothing must not have its silence
        // read as containment; an omitted field reads as normal, which is the whole defect.
        let silent = SilentManager { on: true };
        let r = run_containment(Some(&silent));
        assert_eq!(r.label(), "unknown", "{}", r.describe());
        assert!(!r.caps_enforced(), "unknown must never imply enforced caps");
        assert!(
            !r.subtree_killable(),
            "unknown must never imply a killable subtree"
        );
        assert!(
            r.describe().contains("NOT a"),
            "the record must say unknown is not a claim of boxing: {}",
            r.describe()
        );

        // A manager that reports itself disabled, and no manager at all, are both POSITIVE
        // knowledge that nothing was arranged -- unboxed, not unknown.
        let off = SilentManager { on: false };
        assert_eq!(run_containment(Some(&off)).label(), "unboxed");
        assert_eq!(run_containment(None).label(), "unboxed");
    }

    #[test]
    fn steps_contained_is_not_run_boxed() {
        // "steps contained" and "run contained" are different facts, and a record that blurs them
        // is the overclaim relocated into an artifact that outlives the run.
        let steps = RunContainment::StepsContainedOnly {
            cgroup: std::path::PathBuf::from("/sys/fs/cgroup/x"),
        };
        let boxed = RunContainment::RunBoxed {
            cgroup: std::path::PathBuf::from("/sys/fs/cgroup/x.scope"),
            unit: Some("x.scope".into()),
            pid: 7,
        };
        assert_ne!(steps.label(), boxed.label());
        // The direct route kills subtrees but enforces no caps; conflating the two loses exactly
        // the capability difference that decides whether a runaway step can be capped.
        assert!(steps.subtree_killable() && !steps.caps_enforced());
        assert!(boxed.subtree_killable() && boxed.caps_enforced());
        assert!(
            steps.describe().contains("RUNNER is not contained"),
            "{}",
            steps.describe()
        );
    }

    #[test]
    fn only_observed_containment_can_qualify() {
        // SKIPPED AND UNAVAILABLE CAN NEVER CARRY A PROOF. That is the whole certification
        // condition: a typed outcome is worthless if a no-attempt can still answer "contained".
        let skipped = ScopeAttempt::SkippedByPolicy { reason: "CI" };
        let unavail = ScopeAttempt::Unavailable {
            detail: "probe failed".into(),
        };
        let execfail = ScopeAttempt::ExecFailed {
            detail: "ENOENT".into(),
        };
        let sentinel = ScopeAttempt::SentinelWithoutContainment {
            detail: "root cgroup".into(),
        };
        for a in [&skipped, &unavail, &execfail, &sentinel] {
            assert!(!a.is_contained(), "{a:?} must not qualify as contained");
            assert!(
                a.proof().is_none(),
                "{a:?} must not carry a containment proof"
            );
        }
        // A sentinel we could not confirm must still let the caller PROCEED -- re-exec-ing again
        // would recurse forever on the same sentinel -- while never counting as containment.
        assert!(sentinel.may_proceed());
        assert!(!sentinel.is_contained());

        let proof = ContainmentProof {
            cgroup: std::path::PathBuf::from("/sys/fs/cgroup/x.scope"),
            pid: 1234,
            unit: Some("x.scope".into()),
        };
        let boxed = ScopeAttempt::AlreadyInScope {
            proof: proof.clone(),
        };
        assert!(boxed.is_contained());
        assert_eq!(boxed.proof(), Some(&proof));
        assert!(
            boxed.describe().contains("pid 1234"),
            "{}",
            boxed.describe()
        );
    }

    #[test]
    fn observation_of_this_live_process_is_two_directional() {
        // Runs against the REAL /proc and /sys/fs/cgroup of the test process, so it is a
        // statement about a live pid rather than about a fixture.
        let seen = observe_own_containment(None);
        match &seen {
            ContainmentEvidence::Observed(p) => {
                assert_eq!(p.pid, std::process::id());
                assert!(p.cgroup.starts_with(CGROUP_ROOT), "{:?}", p.cgroup);
                // The roster direction: our pid really is in that cgroup's own list.
                let roster = fs::read_to_string(p.cgroup.join("cgroup.procs")).unwrap_or_default();
                assert!(
                    roster.lines().any(|l| l.trim() == p.pid.to_string()),
                    "claimed {} but the roster does not list the pid",
                    p.cgroup.display()
                );
            }
            // A host without cgroup-v2 legitimately cannot observe; it must then say so rather
            // than answer yes.
            ContainmentEvidence::NotObserved { detail } => assert!(!detail.is_empty()),
        }
        // A unit this process is definitely not in must never be confirmed.
        let wrong = observe_own_containment(Some("dagrun-definitely-not-this-unit.scope"));
        assert!(wrong.proof().is_none(), "{}", wrong.describe());

        // THE ROSTER DIRECTION, bound to a cgroup that genuinely EXISTS but does not list us: our
        // own PARENT. `/proc/self/cgroup` would happily name a path; only the roster says whether
        // the pid is in it. Deleting the roster check makes this case pass, which is what makes
        // this assertion worth having.
        if let ContainmentEvidence::Observed(p) = &seen {
            if let Some(parent) = p.cgroup.parent() {
                if parent.is_dir() && parent != std::path::Path::new(CGROUP_ROOT) {
                    let up = observe_containment_at(parent, p.pid, None);
                    assert!(
                        up.proof().is_none(),
                        "pid {} must NOT be observable in its parent cgroup {}: {}",
                        p.pid,
                        parent.display(),
                        up.describe()
                    );
                }
            }
        }
    }

    #[test]
    fn scope_attempt_separates_the_four_outcomes() {
        // The bool this replaces answered "may I proceed", which is a DIFFERENT question from
        // "was containment established" and from "did we even ask". All three now have answers.
        let in_scope = ScopeAttempt::AlreadyInScope {
            proof: ContainmentProof {
                cgroup: std::path::PathBuf::from("/sys/fs/cgroup/x.scope"),
                pid: 1,
                unit: Some("x.scope".into()),
            },
        };
        let skipped = ScopeAttempt::SkippedByPolicy { reason: "CI" };
        let unavail = ScopeAttempt::Unavailable {
            detail: "probe failed".into(),
        };
        let execfail = ScopeAttempt::ExecFailed {
            detail: "ENOENT".into(),
        };

        // may_proceed reproduces the historical bool EXACTLY: no policy rides in this change.
        assert!(in_scope.may_proceed());
        assert!(skipped.may_proceed());
        assert!(!unavail.may_proceed());
        assert!(!execfail.may_proceed());

        // ...but only one of the two "proceed" outcomes is actually contained.
        assert!(in_scope.is_contained());
        assert!(!skipped.is_contained());

        // ...and the skip is the one outcome that never asked the capability question. That is
        // the distinction whose absence sent four probes after a branch CI never executes.
        assert!(!skipped.attempted());
        assert!(unavail.attempted());
        assert!(execfail.attempted());
    }

    #[test]
    fn a_refused_scope_probe_is_retried_once_after_the_backoff() {
        use std::cell::RefCell;
        let backoff = Duration::from_millis(50);
        let calls = RefCell::new(Vec::new());
        let start = Instant::now();
        let ok = probe_with_one_retry(
            |attempt| {
                calls.borrow_mut().push(attempt);
                if attempt == 1 {
                    Err("transient".into())
                } else {
                    Ok(())
                }
            },
            backoff,
        );
        assert!(ok, "a refusal followed by a success is available");
        assert_eq!(*calls.borrow(), vec![1, 2]);
        assert!(
            start.elapsed() >= backoff,
            "the retry waited {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_scope_probe_refused_twice_fails_closed_without_a_third_try() {
        use std::cell::RefCell;
        let backoff = Duration::from_millis(500);
        let calls = RefCell::new(Vec::new());
        let start = Instant::now();
        let ok = probe_with_one_retry(
            |attempt| {
                calls.borrow_mut().push(attempt);
                Err("refused".into())
            },
            backoff,
        );
        assert!(!ok);
        assert_eq!(*calls.borrow(), vec![1, 2]);
        // One pause between the two attempts, and none after the last: failing closed is not
        // delayed further.
        let waited = start.elapsed();
        assert!(
            waited >= backoff && waited < 2 * backoff,
            "two refusals waited {waited:?}, expected one {backoff:?} pause"
        );
    }

    #[test]
    fn a_passing_scope_probe_runs_once_and_does_not_wait() {
        use std::cell::RefCell;
        let calls = RefCell::new(Vec::new());
        let start = Instant::now();
        let ok = probe_with_one_retry(
            |attempt| {
                calls.borrow_mut().push(attempt);
                Ok(())
            },
            Duration::from_secs(30),
        );
        assert!(ok);
        assert_eq!(*calls.borrow(), vec![1]);
        assert!(start.elapsed() < Duration::from_secs(30));
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn a_hung_scope_probe_is_killed_at_the_timeout_and_keeps_its_partial_stderr() {
        let timeout = Duration::from_millis(300);
        let start = Instant::now();
        let why = run_probe_bounded(sh("echo 'waiting on the bus' >&2; kill -STOP $$"), timeout)
            .expect_err("a probe that never exits is a failure");
        let took = start.elapsed();
        assert!(why.contains("timed out after 300ms"), "{why}");
        assert!(why.contains("waiting on the bus"), "{why}");
        assert!(
            took >= timeout && took < timeout + SCOPE_PROBE_STDERR_GRACE,
            "the hung probe took {took:?}"
        );
    }

    #[test]
    fn a_scope_probe_that_exits_reports_its_status_and_stderr() {
        let why = run_probe_bounded(
            sh("echo 'Failed to connect to bus' >&2; exit 1"),
            Duration::from_secs(30),
        )
        .expect_err("exit 1 is a failure");
        assert!(why.contains("exit status: 1"), "{why}");
        assert!(why.contains("Failed to connect to bus"), "{why}");
        assert!(run_probe_bounded(sh("exit 0"), Duration::from_secs(30)).is_ok());
    }

    #[test]
    fn a_descendant_holding_stderr_does_not_stall_a_finished_probe() {
        let start = Instant::now();
        let why = run_probe_bounded(
            sh("echo refused >&2; sleep 30 & exit 1"),
            Duration::from_secs(30),
        )
        .expect_err("exit 1 is a failure");
        assert!(
            why.contains("exit status: 1") && why.contains("refused"),
            "{why}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "waited {:?} on a pipe the probe no longer owns",
            start.elapsed()
        );
    }

    #[test]
    fn a_scope_probe_that_cannot_launch_says_so() {
        let why = run_probe_bounded(
            Command::new("/nonexistent/systemd-run"),
            Duration::from_secs(30),
        )
        .expect_err("a missing binary is a failure");
        assert!(why.starts_with("cannot launch systemd-run: "), "{why}");
    }

    #[test]
    fn a_scope_probe_failure_keeps_the_exit_status_and_stderr() {
        use std::os::unix::process::ExitStatusExt;
        let why = describe_probe_failure(
            std::process::ExitStatus::from_raw(1 << 8),
            b"Failed to connect to bus: Resource temporarily unavailable\n",
        );
        assert!(why.contains("exit status: 1"), "{why}");
        assert!(
            why.contains("Failed to connect to bus: Resource temporarily unavailable"),
            "{why}"
        );
    }

    #[test]
    fn a_policy_skip_never_claims_the_scope_was_unavailable() {
        // The old wording, "boxing was skipped (e.g. CI without a systemd --user scope)", asserted
        // a cause it had not tested. A skip must describe itself as untested, and must point at
        // the instrument that would test it.
        let skipped = ScopeAttempt::SkippedByPolicy {
            reason: "GITHUB_ACTIONS",
        };
        let text = skipped.describe();
        assert!(text.contains("SKIPPED BY POLICY"), "{text}");
        assert!(text.contains("GITHUB_ACTIONS"), "{text}");
        assert!(text.contains("NOT tested"), "{text}");
        assert!(text.contains(FORCE_ATTEMPT_ENV), "{text}");
        assert!(
            !text.contains("unavailable"),
            "a skip must not claim unavailability it never measured: {text}"
        );
        // Whereas a real probe failure says so, and says what it tried.
        let unavail = ScopeAttempt::Unavailable {
            detail: "`systemd-run --user --scope` probe failed".into(),
        };
        assert!(
            unavail.describe().contains("attempted and failed"),
            "{}",
            unavail.describe()
        );
    }

    #[test]
    fn systemd_duration_parses_both_renderings() {
        // The integer form is what a USec property was ASSUMED to print...
        assert_eq!(parse_systemd_duration_secs("66000000"), Some(66));
        // ...and this is what systemd 259 actually printed, which failed a good run closed.
        assert_eq!(parse_systemd_duration_secs("1min 6s"), Some(66));
        assert_eq!(parse_systemd_duration_secs("15min"), Some(900));
        assert_eq!(parse_systemd_duration_secs("1h 5min"), Some(3900));
        assert_eq!(parse_systemd_duration_secs("500ms"), Some(1)); // rounds
                                                                   // No bound, and unparsable, must BOTH read as "not proven" rather than as some number.
        assert_eq!(parse_systemd_duration_secs("infinity"), None);
        assert_eq!(parse_systemd_duration_secs(""), None);
        assert_eq!(parse_systemd_duration_secs("later"), None);
        assert_eq!(parse_systemd_duration_secs("3 fortnights"), None);
    }
    use super::*;

    // Same fixture shape the paired test suite uses. The architectural rows carry a
    // deliberately huge uniform count: if either engine ever summed them they would swamp the
    // device signal and these assertions would fail. That is what makes the device-rows-only
    // rule observable rather than merely documented.
    fn interrupts_fixture(rates: &[u64], arch_per_cpu: u64) -> String {
        let ncpu = rates.len();
        let header: Vec<String> = (0..ncpu).map(|i| format!("CPU{i}")).collect();
        let row = |values: &[u64]| -> String {
            values
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        };
        let arch: Vec<u64> = vec![arch_per_cpu; ncpu];
        format!(
            "      {}\n  17: {}   PCI-MSI  nvme0q1\n 130: {}   IR-PCI-MSI  eth0-tx\n LOC: {}   Local timer interrupts\n RES: {}   Rescheduling interrupts\n ERR: 0\n",
            header.join(" "),
            row(rates),
            row(&vec![0u64; ncpu]),
            row(&arch),
            row(&arch),
        )
    }

    #[test]
    fn device_rows_are_counted_and_architectural_rows_are_not() {
        let text = interrupts_fixture(&[7, 0, 300], 99_000);
        let counts = parse_device_irq_counts(&text);
        assert_eq!(counts.get(&0), Some(&7));
        assert_eq!(counts.get(&1), Some(&0));
        assert_eq!(counts.get(&2), Some(&300));
        assert_eq!(counts.len(), 3);
    }

    #[test]
    fn empty_or_headerless_input_yields_an_absent_signal_not_zeroes() {
        assert!(parse_device_irq_counts("").is_empty());
        assert!(parse_device_irq_counts("\n").is_empty());
    }

    #[test]
    fn short_rows_are_skipped_whole_so_counts_are_not_misattributed() {
        // A numeric row with fewer fields than the header must not have its prefix counted.
        let text = "      CPU0 CPU1 CPU2\n  17: 5 5\n  18: 1 2 3   PCI-MSI  dev\n";
        let counts = parse_device_irq_counts(text);
        assert_eq!(counts.get(&0), Some(&1));
        assert_eq!(counts.get(&1), Some(&2));
        assert_eq!(counts.get(&2), Some(&3));
    }

    fn temp_scope(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("dagrun-cgroup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn sanitize_makes_safe_names() {
        assert_eq!(sanitize("build.app"), "step-build.app");
        assert_eq!(sanitize("weird/tag name"), "step-weird~2ftag~20name");
        assert_ne!(sanitize("a/b.j"), sanitize("a_b.j"));
        assert_ne!(sanitize("a/b.j"), sanitize("a~2fb.j"));
    }

    #[test]
    fn rolled_back_cpu_counters_are_invalid() {
        assert_eq!(idle_fraction_between((10, 20), (15, 30)), Some(0.5));
        assert_eq!(idle_fraction_between((10, 20), (10, 20)), Some(1.0));
        assert_eq!(idle_fraction_between((10, 20), (9, 30)), None);
        assert_eq!(idle_fraction_between((10, 20), (15, 19)), None);
    }

    #[test]
    fn overlong_encoded_cgroup_name_refuses_user_command() {
        let scope = temp_scope("overlong-name");
        fs::write(scope.join("cpu.max"), "100000 100000").unwrap();
        fs::write(scope.join("memory.max"), "4294967296").unwrap();
        let cg = Cgroups {
            enabled: true,
            root: Some(scope.clone()),
            containment: Containment::Full,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        };
        let marker = scope.join("user-command-ran");
        let wrapped = cg.prepare_command(
            &"/".repeat(100),
            &format!("touch {}", marker.display()),
            None,
            None,
        );
        let status = Command::new("bash")
            .arg("-c")
            .arg(wrapped)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(125));
        assert!(!marker.exists());
        let _ = fs::remove_dir_all(scope);
    }

    /// #79 derived-enforcement-manifest: the `memory_max` flag is load-bearing, not decorative.
    /// Flipping it off in the registry must stop the inner cap being written -- otherwise the
    /// manifest could say `"memory_max":false` while the kernel cap was still applied, which is
    /// the same class of lie the manifest used to be able to tell in the other direction.
    #[test]
    fn inner_memory_cap_follows_the_memory_max_capability_flag() {
        let scope = temp_scope("memory-max-capability");
        let cg = Cgroups {
            enabled: true,
            root: Some(scope.clone()),
            containment: Containment::Full,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        };
        let applied = scope.join(sanitize("g.j")).join("memory.max");

        crate::capabilities::with_registry_pinned(|| {
            cg.prepare_command("g.j", "true", Some(4096), None);
            assert_eq!(fs::read_to_string(&applied).unwrap(), "4096");
            assert!(crate::capabilities::enforcement_manifest().contains("\"memory_max\":true"));
        });

        fs::remove_file(&applied).unwrap();
        crate::capabilities::with_registry_override("memory_max", false, || {
            // ABSENCE of the `true`, not presence of a `false`: the manifest publishes two lanes
            // and the uncontained one already says false, so `contains("false")` would hold
            // whatever the bracket did.
            assert!(!crate::capabilities::enforcement_manifest().contains("\"memory_max\":true"));
            cg.prepare_command("g.j", "true", Some(4096), None);
        });
        assert!(
            !applied.exists(),
            "memory.max was written even though the registry says memory_max is unenforced"
        );

        let _ = fs::remove_dir_all(scope);
    }

    #[test]
    fn failed_cgroup_migration_refuses_user_command() {
        let scope = temp_scope("migration-failure");
        let child = scope.join(sanitize("g.j"));
        fs::create_dir_all(child.join("cgroup.procs")).unwrap();
        let marker = scope.join("user-command-ran");
        let wrapped = join_cgroup_command(&child, &format!("touch {}", marker.display()));
        let status = Command::new("bash")
            .arg("-c")
            .arg(wrapped)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(125));
        assert!(!marker.exists());
        let _ = fs::remove_dir_all(scope);
    }

    #[test]
    fn disabled_manager_is_a_noop_outside_a_scope() {
        // Constructed outside a scope -> disabled; every method a safe no-op.
        let cg = Cgroups::new();
        if !cg.enabled() {
            assert_eq!(cg.prepare_command("g.j", "true", Some(1), Some(1)), "true");
            assert!(!cg.kill("g.j"));
            assert_eq!(cg.oom_kills("g.j"), 0);
            assert_eq!(cg.peak_bytes("g.j"), None);
            assert_eq!(cg.kill_all_remaining(), 0);
        }
    }

    #[test]
    fn cpu_quota_percent_floor_is_one_core() {
        assert!(cpu_quota_percent(0.90) >= 100);
        assert!(aggregate_slice_max_cpus() >= 1);
        assert!(aggregate_slice_max_cpus() <= cpu_count().max(1));
        assert!(aggregate_slice_max_cpus() * 100 <= cpu_quota_percent(DEFAULT_CPU_BUDGET_FRACTION));
    }

    #[test]
    fn cpuset_parser_preserves_exact_identity() {
        assert_eq!(
            parse_cpuset(Some("0-2,7")),
            Some(HashSet::from([0, 1, 2, 7]))
        );
        assert_eq!(parse_cpuset(Some("0-2,2")), None);
        assert_ne!(parse_cpuset(Some("0-1")), parse_cpuset(Some("2-3")));
    }

    #[test]
    fn outer_oom_group_write_is_read_back() {
        let scope = temp_scope("oom-group");
        fs::write(scope.join("memory.oom.group"), "0").unwrap();
        assert!(enable_outer_oom_group_at(&scope));
        assert_eq!(
            fs::read_to_string(scope.join("memory.oom.group")).unwrap(),
            "1"
        );
        fs::remove_dir_all(scope).unwrap();
    }

    #[test]
    fn outer_scope_audit_requires_group_oom() {
        let scope = temp_scope("audit");
        fs::write(scope.join("memory.max"), "104857600").unwrap();
        fs::write(scope.join("memory.swap.max"), "0").unwrap();
        fs::write(scope.join("memory.oom.group"), "1").unwrap();
        fs::write(scope.join("cpu.max"), "200000 100000").unwrap();
        assert!(verify_scope_limits_at(&scope, 104857600, Some(2)));
        fs::write(scope.join("memory.oom.group"), "0").unwrap();
        assert!(!verify_scope_limits_at(&scope, 104857600, Some(2)));
        fs::write(scope.join("memory.oom.group"), "1").unwrap();
        fs::write(scope.join("cpu.max"), "300000 100000").unwrap();
        assert!(!verify_scope_limits_at(&scope, 104857600, Some(2)));
        fs::remove_dir_all(scope).unwrap();
    }

    // ------------------------------------------------- --max-mem as a real ceiling (#33)

    #[test]
    fn max_mem_becomes_the_outer_scope_ceiling_and_can_only_tighten() {
        // `--max-mem 20G` used to feed the sizing model only: the outer scope was still created
        // with 90% of MemAvailable, so "two runs with 20 GiB each" held for the arithmetic and
        // for nothing on the machine.
        assert_eq!(
            outer_memory_max_from(1_000_000, None, Some(400_000)),
            Some(400_000),
            "a request below the derived boundary must become the cap"
        );
        assert_eq!(
            outer_memory_max_from(1_000_000, None, Some(5_000_000)),
            Some(900_000),
            "a request above the derived boundary must NOT widen it"
        );
        assert_eq!(
            outer_memory_max_from(1_000_000, None, None),
            Some(900_000),
            "no request is byte-for-byte the previous behaviour"
        );
    }

    #[test]
    fn the_environment_override_and_max_mem_compose_to_the_smallest() {
        // They do not override each other; each is a one-way tightening.
        assert_eq!(
            outer_memory_max_from(1_000_000, Some("500000"), Some(400_000)),
            Some(400_000)
        );
        assert_eq!(
            outer_memory_max_from(1_000_000, Some("500000"), Some(700_000)),
            Some(500_000)
        );
        assert_eq!(
            outer_memory_max_from(1_000_000, Some("2000000"), Some(5_000_000)),
            Some(900_000)
        );
    }

    #[test]
    fn a_nonpositive_ceiling_request_is_refused_rather_than_widened() {
        // Same treatment as a non-positive environment value: the caller asked for a ceiling
        // this cannot express, and returning the derived boundary instead would hand back a
        // WIDER scope than was asked for.
        //
        // This is the LIBRARY contract for a caller of outer_memory_max_bytes_capped, not a
        // statement about `--max-mem 0`: the CLI drops a non-positive spec before this point and
        // refuses the run by name in select_max_steps instead. The end-to-end behaviour of
        // `--max-mem 0` is pinned in tests/max_mem_outer_scope_smoke.rs, because a rule stated
        // only in a comment is how the previous version of this one came to be wrong.
        assert_eq!(outer_memory_max_from(1_000_000, None, Some(0)), None);
        assert_eq!(outer_memory_max_from(1_000_000, None, Some(-1)), None);
        assert_eq!(outer_memory_max_from(1_000_000, Some("0"), None), None);
    }

    /// A manager pointed at a planted directory tree instead of a real cgroup hierarchy.
    fn planted_manager(root: PathBuf) -> Cgroups {
        Cgroups {
            enabled: true,
            root: Some(root),
            containment: Containment::Full,
            delegated_ancestor_memory_max: None,
            delegated_ancestor_memory_max_known: true,
            delegated_ancestor_max_cpus: None,
            delegated_ancestor_cpu_known: true,
            nested_coordinator: None,
            delegated: Mutex::new(HashSet::new()),
        }
    }

    #[test]
    fn whole_run_cpu_usage_reads_only_the_outer_usage_counter() {
        let root = temp_scope("run-cpu-usage");
        let cg = planted_manager(root.clone());

        assert_eq!(cg.run_cpu_usage_usec(), None, "absent cpu.stat is UNKNOWN");
        fs::write(
            root.join("cpu.stat"),
            "usage_usec 5195284\nuser_usec 3645063\nsystem_usec 1550222\nnr_periods 9\n",
        )
        .unwrap();
        assert_eq!(cg.run_cpu_usage_usec(), Some(5_195_284));

        fs::write(
            root.join("cpu.stat"),
            "usage_usec not-a-number\nuser_usec 7\n",
        )
        .unwrap();
        assert_eq!(
            cg.run_cpu_usage_usec(),
            None,
            "a malformed usage counter must not become measured zero"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_applied_cap_is_read_back_and_unbounded_is_not_unknown() {
        // `None` (we do not know the ceiling) and `Some("max")` (we know of none at any level
        // this manager can see) are different answers. Only the second rules out censoring by
        // the runner's own caps, so flattening either into the other is what makes a recorded
        // peak un-interpretable. The planted scope root here carries no memory.max of its own.
        let root = temp_scope("applied-cap");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        let cg = planted_manager(root.clone());

        assert_eq!(
            cg.applied_memory_max("g.job"),
            None,
            "absent file is UNKNOWN"
        );

        fs::write(child.join("memory.max"), "max\n").unwrap();
        assert_eq!(cg.applied_memory_max("g.job"), Some("max".to_string()));

        fs::write(child.join("memory.max"), "8589934592\n").unwrap();
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("8589934592".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_uncapped_step_reports_the_outer_scope_cap_it_actually_ran_under() {
        // An uncharacterised step gets no INNER cap, but it is never actually unbounded: the
        // runner refuses to run without an outer scope memory.max, and cgroup-v2 limits are
        // hierarchical. Reporting the child's own "max" would label it "known unbounded", and
        // reclaim at the ancestor's limit is not charged to the child's memory.events either,
        // so the whole row would read as a comfortable fit.
        let root = temp_scope("outer-cap");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("memory.max"), "8589934592\n").unwrap();
        fs::write(child.join("memory.max"), "max\n").unwrap();
        let cg = planted_manager(root.clone());

        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("8589934592".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_applied_cap_is_the_tighter_of_the_step_and_scope_caps() {
        // Both directions, so the answer is a MINIMUM and not "whichever we happened to read".
        let root = temp_scope("tighter-cap");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        fs::write(root.join("memory.max"), "8589934592\n").unwrap();
        let cg = planted_manager(root.clone());

        fs::write(child.join("memory.max"), "2147483648\n").unwrap();
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("2147483648".to_string())
        );

        fs::write(child.join("memory.max"), "17179869184\n").unwrap();
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("8589934592".to_string())
        );

        // An unbounded scope leaves the step's own cap untouched.
        fs::write(root.join("memory.max"), "max\n").unwrap();
        fs::write(child.join("memory.max"), "2147483648\n").unwrap();
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("2147483648".to_string())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_run_preserves_a_finite_delegated_ancestor_cap() {
        // Parent step -> private run root -> child. The finite limit deliberately lives ABOVE
        // the delegated parent step, reproducing the path that used to report `max` for the
        // child even though the kernel held it at 2 GiB.
        let hierarchy = temp_scope("nested-inherited-memory-cap");
        let outer = hierarchy.join("outer");
        let parent = outer.join("step-parent");
        fs::create_dir_all(&parent).unwrap();
        fs::write(hierarchy.join("memory.max"), "8589934592\n").unwrap();
        fs::write(outer.join("memory.max"), "2147483648\n").unwrap();
        fs::write(parent.join("memory.max"), "max\n").unwrap();
        fs::write(hierarchy.join("cpu.max"), "16 1\n").unwrap();
        fs::write(outer.join("cpu.max"), "2 1\n").unwrap();
        fs::write(parent.join("cpu.max"), "max 100000\n").unwrap();
        let run_root = parent.join("nested-run-fixture");

        let inherited = create_nested_run_root(&parent, &run_root, &hierarchy, None, None).unwrap();

        assert!(inherited.memory_known);
        assert_eq!(inherited.memory_max_bytes, Some(2_147_483_648));
        assert!(inherited.cpu_known);
        assert_eq!(inherited.max_cpus, Some(2));
        assert_eq!(
            fs::read_to_string(run_root.join("memory.max")).unwrap(),
            "2147483648"
        );
        assert_eq!(
            fs::read_to_string(run_root.join("cpu.max")).unwrap(),
            "200000 100000"
        );

        let tighter_root = parent.join("nested-run-tighter");
        let tighter = create_nested_run_root(
            &parent,
            &tighter_root,
            &hierarchy,
            Some(1),
            Some(1024_i64.pow(3)),
        )
        .unwrap();
        assert_eq!(tighter.max_cpus, Some(1));
        assert_eq!(tighter.memory_max_bytes, Some(1024_i64.pow(3)));
        assert_eq!(
            fs::read_to_string(tighter_root.join("cpu.max")).unwrap(),
            "100000 100000"
        );
        assert_eq!(
            fs::read_to_string(tighter_root.join("memory.max")).unwrap(),
            1024_i64.pow(3).to_string()
        );

        let child = run_root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        fs::write(child.join("memory.max"), "max\n").unwrap();
        let mut cg = planted_manager(run_root);
        cg.delegated_ancestor_memory_max = inherited.memory_max_bytes;
        cg.delegated_ancestor_memory_max_known = inherited.memory_known;
        cg.delegated_ancestor_max_cpus = inherited.max_cpus;
        cg.delegated_ancestor_cpu_known = inherited.cpu_known;
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("2147483648".to_string())
        );

        let command = cg.prepare_command("g.job", "true", Some(8 * 1024_i64.pow(3)), Some(8));
        assert!(command.contains("export CARGO_BUILD_JOBS=2"), "{command}");
        assert_eq!(
            cg.applied_memory_max("g.job"),
            Some("2147483648".to_string())
        );

        let _ = fs::remove_dir_all(&hierarchy);
    }

    #[test]
    fn tighter_nested_limits_fail_closed_when_control_files_are_unwritable() {
        let root = temp_scope("nested-required-stamp-failure");
        let memory_root = root.join("memory");
        fs::create_dir(&memory_root).unwrap();
        // A directory at the control-file path reliably makes the write fail on an ordinary
        // fixture filesystem, exercising the same required-stamp path as a refused cgroupfs
        // write without needing cgroup privileges in the unit test.
        fs::create_dir(memory_root.join("memory.max")).unwrap();
        let memory_error = stamp_nested_memory_limit(&memory_root, 1024_i64.pow(3)).unwrap_err();
        assert!(!memory_error.to_string().is_empty());

        let cpu_root = root.join("cpu");
        fs::create_dir(&cpu_root).unwrap();
        fs::create_dir(cpu_root.join("cpu.max")).unwrap();
        let cpu_error = stamp_nested_cpu_limit(&cpu_root, 1).unwrap_err();
        assert!(!cpu_error.to_string().is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn failed_nested_migration_retains_guard_when_rollback_also_fails() {
        let root = temp_scope("nested-rollback-failure");
        let return_supervisor = root.join("parent-supervisor");
        let run_root = root.join("nested-run-fixture");
        fs::create_dir_all(&return_supervisor).unwrap();
        fs::create_dir_all(&run_root).unwrap();
        let guard = NestedCoordinatorGuard {
            run_root: run_root.clone(),
            return_supervisor: return_supervisor.clone(),
            active: true,
        };
        let mut failure = failed_nested_migration_with(
            guard,
            std::io::Error::other("post-write verification failed"),
            |_guard| Err(std::io::Error::other("rollback write failed")),
        );

        assert!(failure.to_string().contains("rollback write failed"));
        let recovery = failure
            .recovery
            .take()
            .expect("an unproven rollback must retain a recovery guard");
        assert_eq!(recovery.return_supervisor, return_supervisor);
        assert!(recovery.active);
        let mut manager = Cgroups::disabled(Containment::Full);
        manager.nested_coordinator = Some(recovery);
        assert!(manager.restore_nested_coordinator_now().is_err());
        let recovery = manager
            .nested_coordinator
            .take()
            .expect("a failed retry must keep the recovery guard for Drop");
        // This planted fixture is not a real cgroup, so deliberately suppress its production
        // Drop retry after proving the exact recovery target was retained.
        std::mem::forget(recovery);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nested_run_inherits_effective_cpuset_when_cpu_quota_is_unbounded() {
        let hierarchy = temp_scope("nested-inherited-cpuset");
        let parent = hierarchy.join("step-parent");
        fs::create_dir(&parent).unwrap();
        fs::write(hierarchy.join("cpu.max"), "max 100000").unwrap();
        fs::write(parent.join("cpu.max"), "max 100000").unwrap();
        fs::write(parent.join("cpuset.cpus.effective"), "4-5").unwrap();
        let run_root = parent.join("nested-run-cpuset");

        let inherited = create_nested_run_root(&parent, &run_root, &hierarchy, None, None).unwrap();

        assert!(inherited.cpu_known);
        assert_eq!(inherited.max_cpus, Some(2));
        assert_eq!(
            fs::read_to_string(run_root.join("cpu.max")).unwrap(),
            "200000 100000"
        );
        let _ = fs::remove_dir_all(&hierarchy);
    }

    #[test]
    fn memory_events_carry_every_counter_and_the_oom_count_agrees() {
        let root = temp_scope("memory-events");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        let cg = planted_manager(root.clone());

        assert_eq!(cg.memory_events("g.job"), None, "absent file is UNKNOWN");
        assert_eq!(cg.oom_kills("g.job"), 0);

        fs::write(
            child.join("memory.events"),
            "low 0\nhigh 12\nmax 41\noom 3\noom_kill 2\noom_group_kill 1\n",
        )
        .unwrap();
        let events = cg.memory_events("g.job").unwrap();
        assert_eq!(events.get("low"), Some(&0));
        assert_eq!(events.get("high"), Some(&12));
        // Reclaim-at-cap: the step was held at its ceiling 41 times.
        assert_eq!(events.get("max"), Some(&41));
        assert_eq!(events.get("oom"), Some(&3));
        assert_eq!(events.get("oom_kill"), Some(&2));
        assert_eq!(events.get("oom_group_kill"), Some(&1));
        // One parse, one answer: the OOM count cannot drift from the recorded counters.
        assert_eq!(cg.oom_kills("g.job"), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delegated_memory_events_exclude_child_owned_ooms_but_keep_boundary_ooms() {
        // A nested scheduler owns its child caps. The parent must not reclassify those OOMs, but
        // an OOM charged at the parent's delegation boundary remains its actionable backstop.
        let root = temp_scope("delegated-memory-events");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        let cg = planted_manager(root.clone());
        cg.delegated.lock().unwrap().insert("g.job".to_string());
        fs::write(child.join("memory.events"), "max 9\noom 4\noom_kill 4\n").unwrap();
        fs::write(
            child.join("memory.events.local"),
            "max 0\noom 0\noom_kill 0\n",
        )
        .unwrap();

        assert_eq!(
            cg.memory_events("g.job"),
            Some(BTreeMap::from([
                ("max".to_string(), 0),
                ("oom".to_string(), 0),
                ("oom_kill".to_string(), 0),
            ]))
        );
        assert_eq!(cg.oom_kills("g.job"), 0);

        fs::write(
            child.join("memory.events.local"),
            "max 3\noom 2\noom_kill 2\n",
        )
        .unwrap();
        assert_eq!(cg.oom_kills("g.job"), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn one_unparsable_memory_events_line_does_not_discard_the_others() {
        // The kernel may add counters, and a blank trailing line costs nothing to skip.
        // Discarding the whole file would blank all five CSV cells AND drop the OOM count to 0
        // — a step that was actually killed would then read as one that was never pressured.
        // Both builds must skip PER LINE, or the same /sys/fs/cgroup file yields two CSV rows.
        let root = temp_scope("memory-events-partial");
        let child = root.join(sanitize("g.job"));
        fs::create_dir_all(&child).unwrap();
        let cg = planted_manager(root.clone());

        fs::write(
            child.join("memory.events"),
            "low 0\nhigh not-a-number\n\nmax 41\nfuture_counter 9\noom 3\noom_kill 2\n",
        )
        .unwrap();

        let events = cg.memory_events("g.job").unwrap();
        assert_eq!(events.get("low"), Some(&0));
        assert_eq!(events.get("max"), Some(&41));
        assert_eq!(events.get("oom"), Some(&3));
        assert_eq!(events.get("oom_kill"), Some(&2));
        assert_eq!(events.get("future_counter"), Some(&9));
        assert_eq!(events.get("high"), None, "the bad line alone is dropped");
        assert_eq!(cg.oom_kills("g.job"), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_silent_manager_reports_an_unknown_cap_rather_than_an_unbounded_one() {
        // The trait defaults must not let silence read as "nothing was capping this step".
        struct Quiet;
        impl CgroupManager for Quiet {
            fn enabled(&self) -> bool {
                true
            }
            fn prepare_command(
                &self,
                _t: &str,
                c: &str,
                _m: Option<i64>,
                _p: Option<i64>,
            ) -> String {
                c.to_string()
            }
            fn kill(&self, _t: &str) -> bool {
                false
            }
            fn cleanup(&self, _t: &str) {}
            fn oom_kills(&self, _t: &str) -> i64 {
                0
            }
            fn peak_bytes(&self, _t: &str) -> Option<i64> {
                None
            }
            fn cpu_stats(&self, _t: &str) -> Option<BTreeMap<String, i64>> {
                None
            }
            fn cpu_pressure(&self, _t: &str) -> Option<BTreeMap<String, f64>> {
                None
            }
            fn thread_count(&self, _t: &str) -> Option<i64> {
                None
            }
            fn kill_all_remaining(&self) -> i64 {
                0
            }
        }
        assert_eq!(Quiet.applied_memory_max("g.job"), None);
        assert_eq!(Quiet.memory_events("g.job"), None);
    }
}
