#!/usr/bin/env rust-script
//! Descendant-complete wall watchdog for hosted strict-compat nodes.
//!
//! ```cargo
//! [dependencies]
//! libc = "0.2"
//! ```

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::io::{self};
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// The source revision compiled into `scripts/validate.rs` at this Hermit head.
///
/// This is intentionally duplicated in that driver: both the outer supervisor and
/// the Rust payload fail closed if the checked-out gitlink or initialized submodule
/// differs. A future pin bump must update both constants in the same commit.
const EXPECTED_AGENT_UTILS_REV: &str = "0f0d667a06f4e141879466caa77640344243f14d";
const AGENT_UTILS_MARKER: &str = "validate: agent-utils revision verified:";
const POLL: Duration = Duration::from_millis(100);
const KILL_VERIFY_LIMIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
struct ProcIdentity {
    pid: i32,
    ppid: i32,
    starttime: u64,
}

struct Tailer {
    raw: BufReader<File>,
    phase: File,
}

impl Tailer {
    fn open(raw: &Path, phase: &Path) -> Result<Self, String> {
        let raw =
            File::open(raw).map_err(|e| format!("open {} for reading: {e}", raw.display()))?;
        let phase = File::create(phase)
            .map_err(|e| format!("create timestamp log {}: {e}", phase.display()))?;
        Ok(Self {
            raw: BufReader::new(raw),
            phase,
        })
    }

    fn record(&mut self, message: &str) -> Result<(), String> {
        writeln!(self.phase, "{} {message}", utc_now())
            .and_then(|()| self.phase.flush())
            .map_err(|e| format!("write timestamp log: {e}"))
    }

    fn drain(&mut self) -> Result<(), String> {
        loop {
            let mut line = Vec::new();
            let n = self
                .raw
                .read_until(b'\n', &mut line)
                .map_err(|e| format!("read raw log: {e}"))?;
            if n == 0 {
                break;
            }
            write!(self.phase, "{} ", utc_now())
                .and_then(|()| self.phase.write_all(&line))
                .map_err(|e| format!("write timestamp log: {e}"))?;
            if !line.ends_with(b"\n") {
                self.phase
                    .write_all(b"\n")
                    .map_err(|e| format!("terminate timestamp log line: {e}"))?;
            }
            self.phase
                .flush()
                .map_err(|e| format!("flush timestamp log: {e}"))?;
        }
        Ok(())
    }
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    // SAFETY: `tm` is valid writable storage and `secs` lives for the call.
    let ok = unsafe { !libc::gmtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return "0000-00-00T00:00:00Z".to_string();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

fn command_line(cwd: &Path, program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("run {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn verify_agent_utils(root: &Path) -> Result<(), String> {
    let linked = command_line(root, "git", &["rev-parse", "HEAD:agent-utils"])?;
    let resolved = command_line(root, "git", &["-C", "agent-utils", "rev-parse", "HEAD"])?;
    let dirty = command_line(
        root,
        "git",
        &[
            "-C",
            "agent-utils",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ],
    )?;
    if linked != EXPECTED_AGENT_UTILS_REV || resolved != EXPECTED_AGENT_UTILS_REV {
        return Err(format!(
            "agent-utils revision mismatch: expected={EXPECTED_AGENT_UTILS_REV} linked={linked} resolved={resolved}"
        ));
    }
    if !dirty.is_empty() {
        return Err(
            "agent-utils has tracked modifications; compiled source is not the linked revision"
                .into(),
        );
    }
    eprintln!(
        "run-strict-watchdog: agent-utils revision verified: expected={EXPECTED_AGENT_UTILS_REV} linked={linked} resolved={resolved}"
    );
    Ok(())
}

fn become_subreaper() -> Result<(), String> {
    // SAFETY: PR_SET/GET_CHILD_SUBREAPER take an integer flag/pointer and do not
    // transfer ownership. Failure is fatal: process-group-only teardown is the
    // mechanism this supervisor exists to replace.
    let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if rc != 0 {
        return Err(format!(
            "PR_SET_CHILD_SUBREAPER failed: {}",
            io::Error::last_os_error()
        ));
    }
    let mut enabled: libc::c_int = 0;
    let rc = unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled, 0, 0, 0) };
    if rc != 0 || enabled != 1 {
        return Err(format!(
            "PR_GET_CHILD_SUBREAPER did not confirm ownership: rc={rc} enabled={enabled} error={}",
            io::Error::last_os_error()
        ));
    }
    eprintln!("run-strict-watchdog: PR_SET_CHILD_SUBREAPER verified");
    Ok(())
}

fn read_identity(pid: i32) -> Result<Option<ProcIdentity>, String> {
    let path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("read {path}: {e}")),
    };
    let close = stat
        .rfind(") ")
        .ok_or_else(|| format!("malformed {path}: missing command terminator"))?;
    let fields: Vec<&str> = stat[close + 2..].split_whitespace().collect();
    if fields.len() <= 19 {
        return Err(format!(
            "malformed {path}: only {} fields after comm",
            fields.len()
        ));
    }
    let ppid = fields[1]
        .parse::<i32>()
        .map_err(|e| format!("malformed {path} ppid: {e}"))?;
    let starttime = fields[19]
        .parse::<u64>()
        .map_err(|e| format!("malformed {path} starttime: {e}"))?;
    Ok(Some(ProcIdentity {
        pid,
        ppid,
        starttime,
    }))
}

fn proc_snapshot() -> Result<Vec<ProcIdentity>, String> {
    let entries = fs::read_dir("/proc").map_err(|e| format!("enumerate /proc: {e}"))?;
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("enumerate /proc entry: {e}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if let Some(identity) = read_identity(pid)? {
            out.push(identity);
        }
    }
    Ok(out)
}

fn descendants(root: i32) -> Result<Vec<ProcIdentity>, String> {
    let snapshot = proc_snapshot()?;
    if !snapshot.iter().any(|p| p.pid == root) {
        return Err(format!("supervisor identity /proc/{root}/stat disappeared"));
    }
    let mut children: HashMap<i32, Vec<ProcIdentity>> = HashMap::new();
    for proc in snapshot {
        children.entry(proc.ppid).or_default().push(proc);
    }
    let mut seen = HashSet::new();
    let mut stack = vec![(root, 0usize)];
    let mut found = Vec::new();
    while let Some((parent, depth)) = stack.pop() {
        for child in children.get(&parent).into_iter().flatten() {
            if seen.insert((child.pid, child.starttime)) {
                found.push((*child, depth + 1));
                stack.push((child.pid, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(found.into_iter().map(|(proc, _)| proc).collect())
}

fn signal_identity(proc: ProcIdentity, signal: i32) -> Result<(), String> {
    if proc.pid <= 1 || proc.pid == std::process::id() as i32 {
        return Err(format!("refusing unsafe signal target pid={}", proc.pid));
    }
    let Some(current) = read_identity(proc.pid)? else {
        return Ok(());
    };
    if current.starttime != proc.starttime {
        return Ok(()); // PID was recycled after the census; never signal the replacement.
    }
    // SAFETY: the target is an identity-rechecked descendant of this dedicated
    // supervisor. No name, command-line, user, or ambient process match is used.
    let rc = unsafe { libc::kill(proc.pid, signal) };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(format!("signal {} to pid {}: {error}", signal, proc.pid));
        }
    }
    Ok(())
}

fn reap_adopted() {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: WNOHANG never blocks. Every reapable child belongs to this
        // dedicated supervisor, including subreaper-adopted escapees.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

fn terminate_descendants(
    tailer: &mut Tailer,
    term_grace: Duration,
    reason: &str,
) -> Result<usize, String> {
    let root = std::process::id() as i32;
    reap_adopted();
    let initial = descendants(root)?;
    if initial.is_empty() {
        let _ = tailer.record(&format!(
            "run-strict-watchdog: verified-empty teardown reason={reason} initial=0"
        ));
        return Ok(0);
    }
    let _ = tailer.record(&format!(
        "run-strict-watchdog: TERM reason={reason} descendants={}",
        initial.len()
    ));
    eprintln!(
        "run-strict-watchdog: TERM reason={reason} descendants={} grace={}s",
        initial.len(),
        term_grace.as_secs()
    );

    let term_deadline = Instant::now() + term_grace;
    loop {
        reap_adopted();
        let live = descendants(root)?;
        if live.is_empty() {
            let _ = tailer.record(&format!(
                "run-strict-watchdog: verified-empty teardown reason={reason} phase=TERM initial={}",
                initial.len()
            ));
            return Ok(initial.len());
        }
        for proc in live {
            signal_identity(proc, libc::SIGTERM)?;
        }
        let _ = tailer.drain();
        if Instant::now() >= term_deadline {
            break;
        }
        std::thread::sleep(POLL);
    }

    let _ = tailer.record(&format!("run-strict-watchdog: KILL reason={reason}"));
    eprintln!("run-strict-watchdog: KILL reason={reason}");
    let kill_deadline = Instant::now() + KILL_VERIFY_LIMIT;
    loop {
        reap_adopted();
        let live = descendants(root)?;
        if live.is_empty() {
            let _ = tailer.record(&format!(
                "run-strict-watchdog: verified-empty teardown reason={reason} phase=KILL initial={}",
                initial.len()
            ));
            return Ok(initial.len());
        }
        for proc in live {
            signal_identity(proc, libc::SIGKILL)?;
        }
        let _ = tailer.drain();
        if Instant::now() >= kill_deadline {
            let remaining = descendants(root)?;
            let detail = remaining
                .iter()
                .map(|p| format!("{}@{}", p.pid, p.starttime))
                .collect::<Vec<_>>()
                .join(",");
            return Err(format!(
                "verified-empty teardown failed after KILL: {} descendant(s) remain [{detail}]",
                remaining.len()
            ));
        }
        std::thread::sleep(POLL);
    }
}

fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(libc::SIGKILL))
}

fn marker_present(path: &Path) -> Result<bool, String> {
    let file =
        File::open(path).map_err(|e| format!("open {} for marker scan: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    loop {
        let mut line = Vec::new();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("scan {} for marker: {e}", path.display()))?;
        if n == 0 {
            return Ok(false);
        }
        if line
            .windows(AGENT_UTILS_MARKER.len())
            .any(|w| w == AGENT_UTILS_MARKER.as_bytes())
            && line
                .windows(EXPECTED_AGENT_UTILS_REV.len())
                .any(|w| w == EXPECTED_AGENT_UTILS_REV.as_bytes())
        {
            return Ok(true);
        }
    }
}

fn usage() -> &'static str {
    "usage: ci/run-strict-watchdog.rs TIMEOUT_S TERM_GRACE_S RAW_LOG PHASE_LOG [--require-agent-utils-marker] -- COMMAND [ARG ...]"
}

fn run() -> Result<i32, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 6 {
        return Err(usage().into());
    }
    let timeout_s = args[0]
        .parse::<u64>()
        .map_err(|e| format!("invalid timeout {:?}: {e}", args[0]))?;
    let term_grace_s = args[1]
        .parse::<u64>()
        .map_err(|e| format!("invalid TERM grace {:?}: {e}", args[1]))?;
    if timeout_s == 0 || term_grace_s == 0 {
        return Err("timeout and TERM grace must both be positive".into());
    }
    let raw_path = PathBuf::from(&args[2]);
    let phase_path = PathBuf::from(&args[3]);
    let mut index = 4usize;
    let require_marker =
        args.get(index).map(String::as_str) == Some("--require-agent-utils-marker");
    if require_marker {
        index += 1;
    }
    if args.get(index).map(String::as_str) != Some("--") || index + 1 >= args.len() {
        return Err(usage().into());
    }
    let command = &args[index + 1];
    let command_args = &args[index + 2..];

    let root = std::env::current_dir().map_err(|e| format!("resolve checkout: {e}"))?;
    verify_agent_utils(&root)?;
    become_subreaper()?;
    if !descendants(std::process::id() as i32)?.is_empty() {
        return Err("supervisor had descendants before launching the worker".into());
    }

    if let Some(parent) = raw_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create raw-log directory {}: {e}", parent.display()))?;
    }
    if let Some(parent) = phase_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("create phase-log directory {}: {e}", parent.display()))?;
    }
    File::create(&raw_path).map_err(|e| format!("create raw log {}: {e}", raw_path.display()))?;
    let raw_out = OpenOptions::new()
        .append(true)
        .open(&raw_path)
        .map_err(|e| format!("open raw log {} for child output: {e}", raw_path.display()))?;
    let raw_err = raw_out
        .try_clone()
        .map_err(|e| format!("clone raw-log descriptor: {e}"))?;
    let mut tailer = Tailer::open(&raw_path, &phase_path)?;
    tailer.record(&format!(
        "run-strict-watchdog: start timeout={timeout_s}s term_grace={term_grace_s}s agent_utils={EXPECTED_AGENT_UTILS_REV} command={command}"
    ))?;

    let mut child = Command::new(command)
        .args(command_args)
        .stdout(Stdio::from(raw_out))
        .stderr(Stdio::from(raw_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("launch {command}: {e}"))?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_s);
    let mut next_heartbeat = started + Duration::from_secs(60);

    let supervised = (|| -> Result<i32, String> {
        let outcome = loop {
            tailer.drain()?;
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(format!("wait for worker pid {}: {e}", child.id())),
            }
            if Instant::now() >= deadline {
                break None;
            }
            if Instant::now() >= next_heartbeat {
                let count = descendants(std::process::id() as i32)?.len();
                eprintln!(
                    "run-strict-watchdog: heartbeat elapsed={}s descendants={count}",
                    started.elapsed().as_secs()
                );
                tailer.record(&format!(
                    "run-strict-watchdog: heartbeat elapsed={}s descendants={count}",
                    started.elapsed().as_secs()
                ))?;
                next_heartbeat += Duration::from_secs(60);
            }
            std::thread::sleep(POLL);
        };

        let code = match outcome {
            Some(status) => {
                tailer.drain()?;
                let leftovers = descendants(std::process::id() as i32)?;
                if leftovers.is_empty() {
                    status_code(status)
                } else {
                    let count = terminate_descendants(
                        &mut tailer,
                        Duration::from_secs(term_grace_s),
                        "worker-exited-with-descendants",
                    )?;
                    if count > 0 {
                        return Err(format!(
                            "worker exited but left {count} descendant(s); teardown succeeded but the run is not clean"
                        ));
                    }
                    // The first census raced a naturally exiting child. The teardown
                    // census observed zero without signalling anything, so this is a
                    // verified-empty normal completion rather than a leaked process.
                    status_code(status)
                }
            }
            None => {
                tailer.record(&format!(
                    "run-strict-watchdog: TIMEOUT after {timeout_s}s; beginning descendant-complete teardown"
                ))?;
                eprintln!(
                    "run-strict-watchdog: TIMEOUT after {timeout_s}s; beginning descendant-complete teardown"
                );
                terminate_descendants(
                    &mut tailer,
                    Duration::from_secs(term_grace_s),
                    "wall-timeout",
                )?;
                124
            }
        };
        tailer.drain()?;

        if require_marker && !marker_present(&raw_path)? {
            tailer.record(&format!(
                "run-strict-watchdog: ERROR required agent-utils marker missing for {EXPECTED_AGENT_UTILS_REV}"
            ))?;
            return Err(format!(
                "required runtime agent-utils marker missing for {EXPECTED_AGENT_UTILS_REV}"
            ));
        }
        tailer.record(&format!(
            "run-strict-watchdog: complete rc={code} verified_empty=true agent_utils={EXPECTED_AGENT_UTILS_REV}"
        ))?;
        Ok(code)
    })();

    match supervised {
        Ok(code) => Ok(code),
        Err(error) => match terminate_descendants(
            &mut tailer,
            Duration::from_secs(term_grace_s),
            "supervisor-error",
        ) {
            Ok(_) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; descendant cleanup after supervisor error also failed: {cleanup_error}"
            )),
        },
    }
}

fn main() {
    rust_script_prelude::init();
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("run-strict-watchdog: ERROR: {error}");
            125
        }
    };
    std::process::exit(code.clamp(0, 255));
}
