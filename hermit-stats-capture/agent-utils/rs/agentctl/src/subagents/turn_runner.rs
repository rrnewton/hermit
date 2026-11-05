//! Headless turn-runner records.
//!
//! This agentctl does not drive turn runners. It can retire one whose
//! runner is provably dead: `stop NAME --expected-token TOKEN` reads the runtime's
//! `registry.json` row under the runtime's shared registry lock, checks every process the
//! runtime recorded for the worker (the runner in its row and in `runner.pid`, a running
//! harness, a staged replacement runner) against `/proc/PID/stat`, and archives the agentctl
//! record, which contains the runtime home, only when all of them are gone or their PIDs now
//! belong to processes with a different start time. A live process, or evidence that cannot be
//! read or trusted, refuses with exit status 69 and changes nothing. The presentation (tmux
//! window or Herdr tab) is never touched; the receipt names it.
//!
//! The proof needs `/proc` to show every process, so it is refused inside a nested PID
//! namespace. It relies on start ticks rather than a boot identity, which
//! the runtime does not record.

use super::*;

/// The `adapter` value of a record whose runtime is a persistent turn runner.
pub(super) const ADAPTER: &str = "turn-runner";

/// How long the runtime's registry may stay exclusively locked before the proof gives up.
#[cfg(not(test))]
const REGISTRY_LOCK_WAIT: Duration = Duration::from_secs(10);
#[cfg(test)]
const REGISTRY_LOCK_WAIT: Duration = Duration::from_millis(200);
const MAX_RUNTIME_REGISTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RUNTIME_FILE_BYTES: u64 = 64 * 1024;
const MAX_PROCESS_STAT_BYTES: u64 = 8 * 1024;
const MAX_PROCESS_ID: i64 = (1 << 31) - 1;
/// A worker row younger than this whose runner has recorded no PID may still be
/// starting: the runtime writes the row first and launches the runner outside its lock. Same
/// window as the runtime's own reaper.
const STARTUP_GRACE_SECONDS: i64 = 20;
/// Linux process states that `/proc/PID/stat` can report.
const PROCESS_STATES: [&str; 12] = ["R", "S", "D", "Z", "T", "t", "X", "x", "K", "W", "P", "I"];

#[cfg(not(test))]
fn proc_root() -> PathBuf {
    PathBuf::from("/proc")
}

#[cfg(test)]
thread_local! {
    /// Replacement for `/proc` on this test thread, so start times can be faked.
    static PROC_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn proc_root() -> PathBuf {
    PROC_ROOT
        .with(|root| root.borrow().clone())
        .unwrap_or_else(|| PathBuf::from("/proc"))
}

/// Run `body` with `/proc` replaced by `root` on this thread.
#[cfg(test)]
pub(super) fn with_proc_root<T>(root: &Path, body: impl FnOnce() -> T) -> T {
    PROC_ROOT.with(|value| *value.borrow_mut() = Some(root.to_path_buf()));
    let result = body();
    PROC_ROOT.with(|value| *value.borrow_mut() = None);
    result
}

/// Why a record is outside the Herdr adapters this agentctl drives.
pub(super) fn unsupported_message(record: &AgentRecord) -> String {
    if record.adapter == ADAPTER {
        format!(
            "agent {:?} is a headless turn-runner worker (mode {:?}, backend {:?}); this agentctl does not drive turn runners. Once its runner has exited, retire it with: agentctl stop {} --expected-token={}",
            record.name, record.mode, record.backend, record.name, record.token
        )
    } else {
        format!(
            "agent {:?} uses adapter {:?}, mode {:?}, backend {:?}, which this agentctl does not run",
            record.name, record.adapter, record.mode, record.backend
        )
    }
}

/// What the runtime's records prove about the worker's processes.
enum Liveness {
    /// Every recorded process is gone.
    Dead(Evidence),
    /// A recorded process is still running.
    Alive(String),
    /// The evidence could not be read or trusted.
    Inconclusive(String),
}

struct Evidence {
    runner_pid: Option<i64>,
    runner_started_at: Option<String>,
    detail: String,
    presentation: Value,
}

/// One process the runtime recorded, with the start tick it recorded, if any.
struct RecordedProcess {
    label: String,
    pid: i64,
    started_at: Option<String>,
}

enum ProcessState {
    Gone,
    Running { starttime: u64, state: String },
}

/// Validated identity fields of one `/proc/PID/stat` record.
struct ProcessStat {
    pid: i64,
    state: String,
    starttime: u64,
}

/// Parse `/proc/PID/stat`, whose command name is opaque bytes up to the last `)`; `None` when
/// the record is malformed or out of range.
fn parse_process_stat(raw: &[u8]) -> Option<ProcessStat> {
    let opening = raw.windows(2).position(|window| window == b" (")?;
    let closing = raw.iter().rposition(|byte| *byte == b')')?;
    if opening == 0 || closing <= opening + 1 || raw.get(closing + 1) != Some(&b' ') {
        return None;
    }
    let tail = std::str::from_utf8(&raw[closing + 2..]).ok()?;
    let fields: Vec<&str> = tail.split_ascii_whitespace().collect();
    if fields.len() <= 19 || !PROCESS_STATES.contains(&fields[0]) {
        return None;
    }
    let number = |text: &str| text.parse::<i64>().ok();
    let pid = number(std::str::from_utf8(&raw[..opening]).ok()?)?;
    let ppid = number(fields[1])?;
    let pgrp = number(fields[2])?;
    let session = number(fields[3])?;
    let starttime = fields[19].parse::<u64>().ok()?;
    let in_range = |value: i64| (0..=MAX_PROCESS_ID).contains(&value);
    if !(1..=MAX_PROCESS_ID).contains(&pid)
        || !in_range(ppid)
        || !in_range(pgrp)
        || !in_range(session)
    {
        return None;
    }
    Some(ProcessStat {
        pid,
        state: fields[0].to_owned(),
        starttime,
    })
}

/// Read a regular file of at most `limit` bytes without following a final symlink; `None` when
/// it does not exist.
fn read_regular(path: &Path, limit: u64) -> io::Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("larger than {limit} bytes"),
        ));
    }
    Ok(Some(bytes))
}

/// Whether `pid` exists and, if so, its start tick. Only positive disappearance counts as gone;
/// unreadable or malformed evidence is an error.
fn process_state(proc: &Path, pid: i64) -> std::result::Result<ProcessState, String> {
    let path = proc.join(pid.to_string()).join("stat");
    match read_regular(&path, MAX_PROCESS_STAT_BYTES) {
        Ok(None) => Ok(ProcessState::Gone),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(ProcessState::Gone),
        Err(error) => Err(format!("cannot read process {pid} identity: {error}")),
        Ok(Some(raw)) => match parse_process_stat(&raw) {
            Some(stat) if stat.pid == pid => Ok(ProcessState::Running {
                starttime: stat.starttime,
                state: stat.state,
            }),
            _ => Err(format!("cannot parse process {pid} identity")),
        },
    }
}

/// `/proc` shows every process only from the initial PID namespace.
fn initial_pid_namespace(proc: &Path) -> std::result::Result<(), String> {
    let status = read_regular(&proc.join("self/status"), MAX_RUNTIME_FILE_BYTES)
        .ok()
        .flatten()
        .ok_or_else(|| {
            "cannot read this process's PID namespace from /proc/self/status".to_owned()
        })?;
    let status = String::from_utf8_lossy(&status);
    let nested = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .map(|pids| pids.split_whitespace().count())
        .ok_or_else(|| "/proc/self/status reports no PID namespace".to_owned())?;
    if nested == 1 {
        Ok(())
    } else {
        Err(
            "agentctl runs inside a nested PID namespace, so /proc cannot show every process"
                .to_owned(),
        )
    }
}

fn canonical_ticks(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|ticks| ticks.to_string() == value)
}

fn process_id(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .filter(|pid| (1..=MAX_PROCESS_ID).contains(pid))
}

/// Check one recorded process: `Ok(None)` when it is gone or its PID was reused.
fn recorded_process_alive(
    proc: &Path,
    process: &RecordedProcess,
) -> std::result::Result<Option<String>, String> {
    match process_state(proc, process.pid)? {
        ProcessState::Gone => Ok(None),
        ProcessState::Running { starttime, state } => match process.started_at.as_deref() {
            None => Err(format!(
                "{} (pid {}) exists and has no recorded start time, so it may still be that process",
                process.label, process.pid
            )),
            Some(recorded) => match canonical_ticks(recorded) {
                None => Err(format!(
                    "{} has an unreadable start time {recorded:?}",
                    process.label
                )),
                Some(ticks) if ticks == starttime => Ok(Some(format!(
                    "{} (pid {}, start tick {ticks}, state {state}) is still running",
                    process.label, process.pid
                ))),
                Some(_) => Ok(None),
            },
        },
    }
}

/// The presentation the runtime row names, which retirement leaves in place.
fn presentation(row: &Value) -> Value {
    let target = row.get("tmux_target").and_then(Value::as_str);
    let backend = row
        .get("backend")
        .and_then(Value::as_str)
        .filter(|backend| !backend.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            // The other edition's fallback for rows without a backend.
            let herdr_tab = target
                .and_then(|target| target.strip_prefix('w'))
                .and_then(|target| target.split_once(':'))
                .is_some_and(|(workspace, tab)| {
                    !workspace.is_empty()
                        && tab.strip_prefix('t').is_some_and(|digits| {
                            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                        })
                });
            if herdr_tab { "herdr" } else { "tmux" }.to_owned()
        });
    json!({
        "backend": backend,
        "target": target,
        "pane": row.get("presentation_pane").and_then(Value::as_str),
    })
}

/// Read `state/NAME/runner.pid`, the PID a runner writes before it records itself.
fn runner_pid_file(state: &Path) -> std::result::Result<Option<i64>, String> {
    let path = state.join("runner.pid");
    let raw = read_regular(&path, MAX_RUNTIME_FILE_BYTES)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    raw.map(|raw| {
        std::str::from_utf8(&raw)
            .ok()
            .and_then(|text| text.trim().parse::<i64>().ok())
            .filter(|pid| (1..=MAX_PROCESS_ID).contains(pid))
            .ok_or_else(|| format!("{} is malformed", path.display()))
    })
    .transpose()
}

/// Processes recorded beside the row: a running harness and staged replacement runners.
fn side_processes(state: &Path) -> std::result::Result<Vec<RecordedProcess>, String> {
    let mut processes = Vec::new();
    let mut files = vec![(state.join("active-harness.json"), "its harness".to_owned())];
    match fs::read_dir(state) {
        Ok(entries) => {
            for entry in entries {
                let entry =
                    entry.map_err(|error| format!("cannot list {}: {error}", state.display()))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("MIGRATION_STAGED_") && name.ends_with(".json") {
                    files.push((entry.path(), "a staged replacement runner".to_owned()));
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(processes),
        Err(error) => return Err(format!("cannot list {}: {error}", state.display())),
    }
    for (path, label) in files {
        let Some(raw) = read_regular(&path, MAX_RUNTIME_FILE_BYTES)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        else {
            continue;
        };
        let identity: Value = serde_json::from_slice(&raw)
            .map_err(|error| format!("{} is malformed: {error}", path.display()))?;
        let pid = identity
            .get("pid")
            .and_then(process_id)
            .filter(|pid| *pid > 1)
            .ok_or_else(|| format!("{} has no valid pid", path.display()))?;
        let started_at = match identity.get("started_at") {
            None | Some(Value::Null) => None,
            Some(Value::String(started_at)) => Some(started_at.clone()),
            Some(_) => return Err(format!("{} has an invalid started_at", path.display())),
        };
        processes.push(RecordedProcess {
            label,
            pid,
            started_at,
        });
    }
    Ok(processes)
}

/// Take the runtime's shared registry lock and prove whether every process of `name` is gone.
/// The returned lock, when present, must be held until the record is archived.
fn inspect_runtime(record: &AgentRecord, proc: &Path) -> (Option<File>, Liveness) {
    match inspect_runtime_inner(record, proc) {
        Ok((lock, liveness)) => (Some(lock), liveness),
        Err(reason) => (None, Liveness::Inconclusive(reason)),
    }
}

fn inspect_runtime_inner(
    record: &AgentRecord,
    proc: &Path,
) -> std::result::Result<(File, Liveness), String> {
    initial_pid_namespace(proc)?;
    let home = record
        .runtime_home
        .as_deref()
        .map(Path::new)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| "the record names no absolute runtime home".to_owned())?;
    match fs::symlink_metadata(home) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "runtime home {} is not a directory",
                home.display()
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "runtime home {} does not exist, so its runner cannot be identified",
                home.display()
            ))
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect runtime home {}: {error}",
                home.display()
            ))
        }
    }
    // The Python runtime creates this lock file the same way on every registry read. A home this
    // process cannot write is still locked through an existing file opened read-only.
    let lock_path = home.join(".registry.lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .or_else(|error| match error.raw_os_error() {
            Some(libc::EACCES | libc::EROFS | libc::EPERM) => OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&lock_path),
            _ => Err(error),
        })
        .map_err(|error| {
            format!(
                "cannot open runtime registry lock {}: {error}",
                lock_path.display()
            )
        })?;
    let deadline = Instant::now() + REGISTRY_LOCK_WAIT;
    while FileExt::try_lock_shared(&lock).is_err() {
        if Instant::now() >= deadline {
            return Err(format!(
                "runtime registry lock {} stayed held for {} seconds",
                lock_path.display(),
                REGISTRY_LOCK_WAIT.as_secs_f64()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let registry = home.join("registry.json");
    let rows = match read_regular(&registry, MAX_RUNTIME_REGISTRY_BYTES)
        .map_err(|error| format!("cannot read {}: {error}", registry.display()))?
    {
        None => Vec::new(),
        Some(raw) => match serde_json::from_slice::<Value>(&raw) {
            Ok(Value::Array(rows)) if rows.iter().all(Value::is_object) => rows,
            Ok(_) => {
                return Err(format!(
                    "{} is not a JSON array of objects",
                    registry.display()
                ))
            }
            Err(error) => return Err(format!("{} is not valid JSON: {error}", registry.display())),
        },
    };
    let mut matching = rows
        .iter()
        .filter(|row| row.get("name").and_then(Value::as_str) == Some(record.name.as_str()));
    let row = matching.next();
    if matching.next().is_some() {
        return Err(format!(
            "{} has more than one row for {:?}",
            registry.display(),
            record.name
        ));
    }
    let state = home.join("state").join(&record.name);
    let mut processes = Vec::new();
    let mut evidence = Evidence {
        runner_pid: None,
        runner_started_at: None,
        detail: String::new(),
        presentation: Value::Null,
    };
    if let Some(row) = row {
        match row.get("mode") {
            Some(Value::String(mode)) if mode == "headless" => {}
            Some(Value::String(mode)) if mode == "tui" => return Err(
                "the runtime row is an interactive TUI, whose liveness this proof does not cover"
                    .to_owned(),
            ),
            None | Some(Value::Null) => {
                if fs::symlink_metadata(state.join("presentation.json")).is_ok() {
                    return Err(
                        "the runtime row has no mode and a presentation sidecar marks it as a possible TUI"
                            .to_owned(),
                    );
                }
            }
            Some(other) => return Err(format!("the runtime row has an unsupported mode {other}")),
        }
        let started_at = match row.get("runner_started_at") {
            None | Some(Value::Null) => None,
            Some(Value::String(started_at)) => Some(started_at.clone()),
            Some(_) => return Err("the runtime row has an invalid runner_started_at".to_owned()),
        };
        let runner_pid = match row.get("runner_pid") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                process_id(value)
                    .ok_or_else(|| "the runtime row has an invalid runner_pid".to_owned())?,
            ),
        };
        evidence.presentation = presentation(row);
        evidence.runner_started_at.clone_from(&started_at);
        evidence.runner_pid = runner_pid;
        if let Some(pid) = runner_pid {
            processes.push(RecordedProcess {
                label: "its runner".to_owned(),
                pid,
                started_at,
            });
        }
    }
    if let Some(pid) = runner_pid_file(&state)? {
        if evidence.runner_pid != Some(pid) {
            processes.push(RecordedProcess {
                label: "the runner named in runner.pid".to_owned(),
                pid,
                started_at: None,
            });
        }
    }
    processes.extend(side_processes(&state)?);
    let mut gone = Vec::new();
    for process in &processes {
        if let Some(alive) = recorded_process_alive(proc, process)? {
            return Ok((lock, Liveness::Alive(alive)));
        }
        gone.push(format!("{} (pid {})", process.label, process.pid));
    }
    if let Some(row) = row {
        if evidence.runner_pid.is_none() {
            starting_window_closed(row)?;
        }
    }
    evidence.detail = if gone.is_empty() {
        if row.is_some() {
            "the runtime row records no runner process and no runner recorded a process ID"
                .to_owned()
        } else {
            format!(
                "{} has no row for {:?} and no runner recorded a process ID",
                registry.display(),
                record.name
            )
        }
    } else {
        format!(
            "every recorded process has exited or its PID now names a newer process: {}",
            gone.join(", ")
        )
    };
    Ok((lock, Liveness::Dead(evidence)))
}

/// Refuse while a row without a runner PID may belong to a runner that is still starting: its
/// `created_at` is unreadable, in the future, or under `STARTUP_GRACE_SECONDS` old.
fn starting_window_closed(row: &Value) -> std::result::Result<(), String> {
    let created_at = row.get("created_at").and_then(Value::as_str);
    let Some(created) = created_at.and_then(crate::prompt_time::rfc3339_instant) else {
        return Err(format!(
            "the runtime row records no runner process and its created_at {} cannot be read, so a runner that is still starting cannot be ruled out",
            created_at.map_or_else(|| "is missing and".to_owned(), |value| format!("{value:?}"))
        ));
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let age = now.saturating_sub(created.seconds);
    if age < STARTUP_GRACE_SECONDS {
        return Err(format!(
            "the runtime row was created {age} seconds ago and records no runner process yet; its runner may still be starting, so retry after {STARTUP_GRACE_SECONDS} seconds"
        ));
    }
    Ok(())
}

/// Keep path receipts truthful when the containing canonical directory moves.
pub(super) fn relocate_receipt_paths(value: Value, source: &Path, destination: &Path) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .into_iter()
                .map(|(key, item)| {
                    let item = match (key.as_str(), item) {
                        ("archived_to" | "state_path" | "runtime_home", Value::String(path)) => {
                            // Joined component by component, so the result is normalized the
                            // way Python's pathlib writes `destination / relative`.
                            Value::String(match Path::new(&path).strip_prefix(source) {
                                Ok(relative) => relative
                                    .components()
                                    .fold(destination.to_path_buf(), |joined, component| {
                                        joined.join(component)
                                    })
                                    .to_string_lossy()
                                    .into_owned(),
                                Err(_) => path,
                            })
                        }
                        (_, item) => relocate_receipt_paths(item, source, destination),
                    };
                    (key, item)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| relocate_receipt_paths(value, source, destination))
                .collect(),
        ),
        other => other,
    }
}

fn unavailable(message: String) -> AgentError {
    AgentError::Client(AdapterError::unavailable(message))
}

impl<A: ManagedApi + ?Sized> ManagedAgents<'_, A> {
    /// Archive a turn-runner record whose runner is provably dead. A live or unprovable runner
    /// refuses with exit status 69 and doctor advice; a dead one without `--expected-token`
    /// refuses with the generation-bound retry. Runs under the caller's name lock.
    pub(super) fn retire_turn_runner(
        &self,
        record: AgentRecord,
        options: &StopOptions,
        recovery: &mut RecoveryAction,
    ) -> Result<Value> {
        let name = record.name.clone();
        if options.retire_dead_adoption {
            return Err(fail(
                "dead-adoption retirement is supported only for interactive herdr-foreign records",
            ));
        }
        if options.recover_legacy_adoption || options.expected_record_sha256.is_some() {
            return Err(fail(
                "legacy adoption recovery is supported only for interactive herdr-foreign records",
            ));
        }
        if options.skip_cloud_halt {
            return Err(fail("--skip-cloud-halt applies only to agentcloud agents"));
        }
        if options
            .expected_token
            .as_deref()
            .is_some_and(|expected| expected != record.token)
        {
            return Err(fail(format!(
                "agent {name:?} was replaced before this operation"
            )));
        }
        let pinned = self.pinned_agent_directory(&name)?;
        let initial = self.managed_record_snapshot(&pinned, &record.token)?;
        if initial.record != record {
            return Err(fail(format!("agent {name:?} record changed before stop")));
        }
        let (runtime_lock, liveness) = inspect_runtime(&initial.record, &proc_root());
        let home = initial.record.runtime_home.clone().unwrap_or_default();
        let evidence = match liveness {
            Liveness::Dead(evidence) => evidence,
            Liveness::Alive(reason) => {
                return Err(unavailable(format!(
                    "refusing to retire headless worker {name:?}: {reason}. This agentctl does not stop live turn runners; stop it first (for example `HERDR_SUBAGENTS_HOME={home} herdr-subagents down {name}`), then rerun this stop. The record, runtime and presentation are unchanged"
                )))
            }
            Liveness::Inconclusive(reason) => {
                return Err(unavailable(format!(
                    "refusing to retire headless worker {name:?}: cannot prove that its runner has exited: {reason}. The record, runtime and presentation are unchanged"
                )))
            }
        };
        if options.expected_token.is_none() {
            *recovery = RecoveryAction::Stop {
                name: name.clone(),
                token: record.token.clone(),
                record_sha256: None,
                skip_cloud_halt: false,
            };
            return Err(fail(format!(
                "retiring headless worker {name:?}, whose runner has exited, requires --expected-token"
            )));
        }
        let (_archive, destination) = self.archive_destination(&initial.record)?;
        let current = self.managed_record_snapshot(&pinned, &record.token)?;
        if !current.same_generation(&initial) {
            return Err(fail(format!(
                "refusing to retire headless worker {name:?}: registry record changed"
            )));
        }
        let mut stopped = current.record;
        stopped.lifecycle = "stopped".to_owned();
        let mut stopped_bytes = serde_json::to_vec_pretty(&stopped)
            .map_err(|error| fail(format!("cannot serialize agent record: {error}")))?;
        stopped_bytes.push(b'\n');
        if stopped_bytes.len() > MAX_AGENT_RECORD_BYTES {
            return Err(fail(format!(
                "refusing stopped agent record larger than {MAX_AGENT_RECORD_BYTES} bytes"
            )));
        }
        let state = Path::new(&home).join("state").join(&name);
        let state_path = fs::symlink_metadata(&state)
            .is_ok()
            .then(|| state.to_string_lossy().into_owned());
        atomic_replace_bytes(&pinned, "agent.json", &stopped_bytes)
            .map_err(|error| *error.error)?;
        self.publish_pinned_directory(&pinned, &destination, &stopped_bytes)?;
        drop(runtime_lock);
        let runtime = relocate_receipt_paths(
            json!({
                "runtime_home": home,
                "state_path": state_path,
                "runner_pid": evidence.runner_pid,
                "runner_started_at": evidence.runner_started_at,
                "evidence": evidence.detail,
                "presentation_left_in_place": evidence.presentation,
            }),
            &pinned.path,
            &destination,
        );
        Ok(json!({
            "name": name,
            "archive": destination,
            "pane_closed": false,
            "tab_closed": false,
            "turn_runner_dead": true,
            "runtime": runtime,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(pid: i64, starttime: u64) -> String {
        format!("{pid} (agent (runner)) S 1 {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {starttime} 1000 10\n")
    }

    #[test]
    fn process_stat_parsing_matches_the_python_procstat() {
        let parsed = parse_process_stat(stat(4242, 555).as_bytes()).unwrap();
        assert_eq!(
            (parsed.pid, parsed.state.as_str(), parsed.starttime),
            (4242, "S", 555)
        );
        for malformed in [
            String::new(),
            "4242 agent S 1".to_owned(),
            "(agent) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5".to_owned(),
            "4242 (agent)S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5".to_owned(),
            "4242 (agent) Q 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5".to_owned(),
            "4242 (agent) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0".to_owned(),
            "0 (agent) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5".to_owned(),
            "4242 (agent) S -1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 5".to_owned(),
            "4242 (agent) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 -5".to_owned(),
        ] {
            assert!(
                parse_process_stat(malformed.as_bytes()).is_none(),
                "{malformed}"
            );
        }
    }

    #[test]
    fn receipt_paths_are_relocated_like_the_python_edition() {
        let source = Path::new("/registry/worker");
        let destination = Path::new("/registry/archive/worker-token");
        let relocated = relocate_receipt_paths(
            json!({
                "runtime_home": "/registry/worker/runtime",
                "state_path": "/registry/worker",
                "archived_to": "/elsewhere/state",
                "other": "/registry/worker/runtime",
                "nested": [{"runtime_home": "/registry/worker//runtime/./state"}],
                "runtime": {"archived_to": null},
            }),
            source,
            destination,
        );
        assert_eq!(
            relocated,
            json!({
                "runtime_home": "/registry/archive/worker-token/runtime",
                "state_path": "/registry/archive/worker-token",
                "archived_to": "/elsewhere/state",
                "other": "/registry/worker/runtime",
                "nested": [{"runtime_home": "/registry/archive/worker-token/runtime/state"}],
                "runtime": {"archived_to": null},
            })
        );
    }

    #[test]
    fn presentation_backend_falls_back_like_the_python_edition() {
        for (row, backend) in [
            (json!({"backend": "tmux", "tmux_target": "w1:t2"}), "tmux"),
            (json!({"tmux_target": "w1:t2"}), "herdr"),
            (json!({"backend": "", "tmux_target": "wspace:t20"}), "herdr"),
            (json!({"tmux_target": "subagents:worker"}), "tmux"),
            (json!({"tmux_target": "w1:t"}), "tmux"),
            (json!({}), "tmux"),
        ] {
            assert_eq!(presentation(&row)["backend"], backend, "{row}");
        }
    }
}
