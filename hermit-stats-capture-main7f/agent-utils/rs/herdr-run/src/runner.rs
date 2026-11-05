//! Serialized pane execution with byte-preserving spool collection.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use fs2::FileExt;
use serde_json::{json, Value};

use crate::allowlist::Admission;
use crate::client::HerdrApi;
use crate::config::{Config, MAX_TIMEOUT_SECONDS};
use crate::error::{HerdrRunError, Result};
use crate::readiness::{assess, infer_prompt_tail, Readiness};
use crate::retention::{prune_runs, runs_root};
use crate::session::Target;
use crate::state::{open_lock_file, pane_lock_path};
use crate::textfmt::{json_pretty, number_text, parse_exit_marker, round3, MAX_EXIT_MARKER_BYTES};
use crate::timefmt;

/// Files holding one run's command, byte streams, and completion status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpoolPaths {
    /// Exclusive directory allocated to this run.
    pub directory: PathBuf,
    /// Raw standard-output file.
    pub stdout: PathBuf,
    /// Raw standard-error file.
    pub stderr: PathBuf,
    /// Completion marker containing the wrapped command's exit code.
    pub exit_code: PathBuf,
}

/// One completed command and the evidence used to launch it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    /// Wrapped command exit code.
    pub exit_code: i32,
    /// Byte-exact standard output.
    pub stdout: Vec<u8>,
    /// Byte-exact standard error.
    pub stderr: Vec<u8>,
    /// Unique human-readable spool identifier.
    pub run_id: String,
    /// Paths allocated to this result.
    pub spool: SpoolPaths,
    /// Live-validated destination pane.
    pub target: Target,
    /// Two-signal readiness evidence.
    pub readiness: Readiness,
    /// Elapsed collection time after submission.
    pub duration_seconds: f64,
}

impl RunResult {
    /// Decode stdout for human-readable JSON, replacing invalid UTF-8 sequences.
    #[must_use]
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Decode stderr for human-readable JSON, replacing invalid UTF-8 sequences.
    #[must_use]
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Resolve the spool paths for `run_id` without creating them.
#[must_use]
pub fn spool_paths(config: &Config, run_id: &str) -> SpoolPaths {
    let root = spool_root(config);
    let directory = root.join("runs").join(run_id);
    SpoolPaths {
        stdout: directory.join("stdout"),
        stderr: directory.join("stderr"),
        exit_code: directory.join("exit_code"),
        directory,
    }
}

fn spool_root(config: &Config) -> PathBuf {
    let root = Path::new(&config.spool_dir);
    if root.is_absolute() {
        root.to_path_buf()
    } else {
        Path::new(&config.project_root).join(root)
    }
}

fn ensure_private_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("run spool path is not a real directory: {}", path.display()),
        ));
    }
    Ok(())
}

fn create_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)
}

/// Build a readable base run ID from UTC time, agent, and process ID.
#[must_use]
pub fn make_run_id(agent: &str, now: SystemTime, pid: u32) -> String {
    // ASCII letters, digits, '-' and '_' only: Unicode character properties differ between
    // language and library versions, and a run ID names a directory both editions must agree on.
    let safe_agent: String = agent
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let safe_agent = if safe_agent.is_empty() {
        "agent"
    } else {
        &safe_agent
    };
    format!("{}-{safe_agent}-{pid}", timefmt::compact_utc(now))
}

fn allocate_spool(config: &Config, base_run_id: &str) -> Result<(String, SpoolPaths)> {
    let root = spool_root(config);
    let runs = root.join("runs");
    ensure_private_directory(&root).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot create private run spool root {}: {error}",
            root.display()
        ))
    })?;
    ensure_private_directory(&runs).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot create private run spool parent {}: {error}",
            runs.display()
        ))
    })?;
    for collision in 0..10_000_u32 {
        let run_id = if collision == 0 {
            base_run_id.to_owned()
        } else {
            format!("{base_run_id}-{collision}")
        };
        let spool = spool_paths(config, &run_id);
        let create = fs::DirBuilder::new().mode(0o700).create(&spool.directory);
        match create {
            Ok(()) => {
                return Ok((run_id, spool));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(HerdrRunError::unavailable(format!(
                    "cannot create run spool {}: {error}",
                    spool.directory.display()
                )));
            }
        }
    }
    Err(HerdrRunError::unavailable(format!(
        "could not allocate a unique run spool after 10000 collisions for {base_run_id}"
    )))
}

fn apply_retention(config: &Config) {
    let _ = prune_runs(
        &runs_root(
            Path::new(&config.spool_dir),
            Path::new(&config.project_root),
        ),
        config.retention_days,
    );
}

/// Poll until readiness holds for two consecutive samples.
pub fn wait_ready<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    target: &Target,
    prompt_tail: Option<&str>,
    timeout: f64,
) -> Result<Readiness> {
    validate_timeout(timeout, "readiness timeout")?;
    wait_ready_with(
        client,
        config,
        target,
        prompt_tail,
        timeout,
        Duration::from_millis(250),
        2,
    )
}

fn wait_ready_with<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    target: &Target,
    prompt_tail: Option<&str>,
    timeout: f64,
    poll_interval: Duration,
    required_consecutive: usize,
) -> Result<Readiness> {
    let deadline = Instant::now().checked_add(seconds_duration(timeout));
    let mut consecutive = 0;
    let mut readings = 0;
    let last = loop {
        let reading = assess(client, &target.pane_id, config, prompt_tail, 4)?;
        readings += 1;
        if reading.ready {
            consecutive += 1;
            if consecutive >= required_consecutive {
                return Ok(reading);
            }
        } else {
            consecutive = 0;
        }
        if readings >= required_consecutive && deadline.is_some_and(|value| Instant::now() >= value)
        {
            break reading;
        }
        thread::sleep(poll_interval);
    };
    if last.ready {
        return Err(HerdrRunError::busy(format!(
            "pane {} ({}) looked idle but not for {required_consecutive} consecutive checks within {}s: {}",
            target.pane_id,
            target.tab_label,
            number_text(timeout),
            last.describe()
        )));
    }
    Err(HerdrRunError::busy(format!(
        "pane {} ({}) is not ready after {}s: {}. Nothing was executed.",
        target.pane_id,
        target.tab_label,
        number_text(timeout),
        last.describe()
    )))
}

fn acquire_pane_lock(target: &Target, timeout: f64) -> Result<File> {
    let path = pane_lock_path(&target.pane_id)?;
    let file = open_lock_file(&path)?;
    let deadline = Instant::now().checked_add(seconds_duration(timeout));
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if deadline.is_some_and(|value| Instant::now() >= value) {
                    return Err(HerdrRunError::busy(format!(
                        "pane {} ({}) is already reserved by another herdr-run. Nothing was executed.",
                        target.pane_id, target.tab_label
                    )));
                }
                thread::sleep(Duration::from_millis(250));
            }
            Err(error) => {
                return Err(HerdrRunError::unavailable(format!(
                    "cannot lock pane {}: {error}",
                    target.pane_id
                )));
            }
        }
    }
}

fn first_terminal_control(value: &str) -> Option<u32> {
    value
        .chars()
        .map(u32::from)
        .find(|codepoint| matches!(codepoint, 0x00..=0x1f | 0x7f..=0x9f))
}

fn quote_word(value: &str) -> String {
    if value.is_empty() {
        return "''".to_owned();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    }) {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Render the line typed into the interactive pane.
///
/// The outer command is accepted by common interactive shells, while all grouping and redirection
/// is deliberately parsed by `sh`.
pub fn build_shell_command(
    admission: &Admission,
    spool: &SpoolPaths,
    cwd: &Path,
) -> Result<String> {
    let values = [
        ("working directory", cwd),
        ("stdout spool path", spool.stdout.as_path()),
        ("stderr spool path", spool.stderr.as_path()),
        ("exit-code spool path", spool.exit_code.as_path()),
    ];
    for (description, value) in values {
        let text = value.to_str().ok_or_else(|| {
            HerdrRunError::unavailable(format!("{description} is not valid UTF-8"))
        })?;
        if let Some(codepoint) = first_terminal_control(text) {
            return Err(HerdrRunError::unavailable(format!(
                "{description} contains terminal control U+{codepoint:04X}; control characters cannot be injected into a shared pane"
            )));
        }
    }
    let inner = format!(
        "umask 077; {{ cd {} && {} ; }} >{} 2>{}; printf '%s\\n' \"$?\" >{}",
        quote_word(cwd.to_str().expect("validated UTF-8")),
        admission.rendered(),
        quote_word(spool.stdout.to_str().expect("validated UTF-8")),
        quote_word(spool.stderr.to_str().expect("validated UTF-8")),
        quote_word(spool.exit_code.to_str().expect("validated UTF-8"))
    );
    Ok(format!("command sh -c {}", quote_word(&inner)))
}

/// Execute one admitted command while holding the pane's inter-process lock.
#[allow(clippy::too_many_arguments)]
pub fn execute<A: HerdrApi + ?Sized>(
    client: &A,
    config: &Config,
    target: &Target,
    admission: &Admission,
    agent: &str,
    cwd: &Path,
    ready_timeout: f64,
    timeout: f64,
) -> Result<RunResult> {
    validate_timeout(ready_timeout, "readiness timeout")?;
    validate_timeout(timeout, "command timeout")?;
    let lock_started = Instant::now();
    let lock = acquire_pane_lock(target, ready_timeout)?;
    let elapsed = lock_started.elapsed().as_secs_f64();
    let remaining_ready = (ready_timeout - elapsed).max(0.0);
    let prompt_tail = infer_prompt_tail(config, None);
    let readiness = wait_ready(
        client,
        config,
        target,
        prompt_tail.as_deref(),
        remaining_ready,
    )?;

    let base_run_id = make_run_id(agent, SystemTime::now(), std::process::id());
    // Write-triggered retention cannot silently stop the way an external timer can. It remains
    // best effort so housekeeping can never mask the command that caused this pass. Prune before
    // allocation so `retention_days: 0` cannot select the brand-new run directory itself.
    apply_retention(config);
    let (run_id, spool) = allocate_spool(config, &base_run_id)?;
    create_private_file(&spool.stdout, &[]).map_err(|error| spool_error(&spool, error))?;
    create_private_file(&spool.stderr, &[]).map_err(|error| spool_error(&spool, error))?;
    create_private_file(
        &spool.directory.join("command"),
        format!("{}\n", admission.rendered()).as_bytes(),
    )
    .map_err(|error| spool_error(&spool, error))?;

    let started = Instant::now();
    client.run(
        &target.pane_id,
        &build_shell_command(admission, &spool, cwd)?,
    )?;
    let deadline = started.checked_add(seconds_duration(timeout));
    loop {
        if let Some(exit_code) = read_exit_code(&spool.exit_code) {
            let stdout =
                fs::read(&spool.stdout).map_err(|error| spool_read_error(&spool.stdout, error))?;
            let stderr =
                fs::read(&spool.stderr).map_err(|error| spool_read_error(&spool.stderr, error))?;
            let result = RunResult {
                exit_code,
                stdout,
                stderr,
                run_id,
                spool,
                target: target.clone(),
                readiness,
                duration_seconds: started.elapsed().as_secs_f64(),
            };
            FileExt::unlock(&lock).ok();
            return Ok(result);
        }
        if deadline.is_some_and(|value| Instant::now() >= value) {
            break;
        }
        thread::sleep(Duration::from_millis(250));
    }
    FileExt::unlock(&lock).ok();
    // Record the run BEFORE reporting the timeout, with a null exit code. This is the only writer
    // that can produce the state [`crate::reap`] calls IN FLIGHT, and it is the literal truth: the
    // command is still running in a pane this process does not own. Without it the reaper's "the
    // agent is thinking" rule has no way of ever being true, and the one pane that provably still
    // has work in it would be the one pane leaving no evidence. Best effort: a failure to record
    // must not replace the timeout the caller actually needs to hear about.
    write_meta_document(
        &run_id,
        &spool,
        agent,
        admission,
        config,
        target,
        &readiness,
        Value::Null,
        started.elapsed().as_secs_f64(),
    )
    .ok();
    let message = format!(
        "command did not finish within {}s. It is STILL RUNNING in pane {} ({}) and was not killed. Partial output is in {}; the exit code will appear in {} when it finishes.",
        number_text(timeout),
        target.pane_id,
        target.tab_label,
        spool.directory.display(),
        spool.exit_code.display()
    );
    Err(timeout_error(&spool, message))
}

fn timeout_error(spool: &SpoolPaths, message: String) -> HerdrRunError {
    let stdout = fs::read(&spool.stdout).unwrap_or_default();
    let stderr = fs::read(&spool.stderr).unwrap_or_default();
    HerdrRunError::timeout_with_partial(
        message,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
        spool.directory.clone(),
    )
}

fn spool_error(spool: &SpoolPaths, error: std::io::Error) -> HerdrRunError {
    HerdrRunError::unavailable(format!(
        "cannot initialize run spool {}: {error}",
        spool.directory.display()
    ))
}

fn spool_read_error(path: &Path, error: std::io::Error) -> HerdrRunError {
    HerdrRunError::unavailable(format!(
        "cannot read run spool output {}: {error}",
        path.display()
    ))
}

/// Read a completion marker: the recorded exit code, or `None` while it is absent, incompletely
/// written, or not a marker at all (see [`parse_exit_marker`]).
pub(crate) fn read_exit_code(path: &Path) -> Option<i32> {
    let mut payload = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_EXIT_MARKER_BYTES as u64 + 1)
        .read_to_end(&mut payload)
        .ok()?;
    parse_exit_marker(&payload)
}

/// Write the completed run's structured evidence and return its path.
///
/// The readiness block records the pane shell's `boot_id` and `shell_start_ticks` alongside its
/// pid. That triple is not decoration: [`crate::reap`] refuses to call a tab stale on a bare pid,
/// so a record without it can never authorise anything, and the reaper would be inert no matter how
/// many runs it had to look at. Either field may be `null` when `/proc` could not be read, which
/// the policy reads as UNKNOWN — the safe direction.
pub fn write_meta(
    result: &RunResult,
    admission: &Admission,
    config: &Config,
    agent: &str,
) -> Result<PathBuf> {
    write_meta_document(
        &result.run_id,
        &result.spool,
        agent,
        admission,
        config,
        &result.target,
        &result.readiness,
        Value::from(result.exit_code),
        result.duration_seconds,
    )
}

/// Write one run record. `exit_code` is `Value::Null` only for a run still running in the pane.
#[allow(clippy::too_many_arguments)]
fn write_meta_document(
    run_id: &str,
    spool: &SpoolPaths,
    agent: &str,
    admission: &Admission,
    config: &Config,
    target: &Target,
    readiness: &Readiness,
    exit_code: Value,
    duration_seconds: f64,
) -> Result<PathBuf> {
    let path = spool.directory.join("meta.json");
    let document = json!({
        "agent": agent,
        "argv": admission.argv,
        "config_source": config.source_path,
        "created": target.created,
        "duration_seconds": round_three(duration_seconds),
        "exit_code": exit_code,
        "from_cache": target.from_cache,
        "pane_id": target.pane_id,
        "prefix": admission.prefix,
        "program": admission.program,
        "readiness": {
            "boot_id": crate::identity::current_boot_id(std::path::Path::new("/proc")),
            "foreground_pgid": readiness.process.foreground_pgid,
            "process_idle": readiness.process.idle,
            "process_reason": readiness.process.reason,
            "prompt_reason": readiness.prompt.reason,
            "prompt_tail": readiness.prompt.tail,
            "prompt_verdict": readiness.prompt.verdict,
            "shell_pid": readiness.process.shell_pid,
            "shell_start_ticks": crate::identity::process_start_ticks(
                readiness.process.shell_pid,
                std::path::Path::new("/proc"),
            ),
        },
        "rendered": admission.rendered(),
        "run_id": run_id,
        "subcommand": admission.subcommand,
        "tab": {"id": target.tab_id, "label": target.tab_label},
        "workspace": {"id": target.workspace_id, "label": target.workspace_label},
    });
    let mut encoded = json_pretty(&document).into_bytes();
    encoded.push(b'\n');
    create_private_file(&path, &encoded).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot write run metadata {}: {error}",
            path.display()
        ))
    })?;
    Ok(path)
}

/// Build the stable success JSON object used by the CLI.
#[must_use]
pub fn result_json(result: &RunResult, meta: Option<&Path>) -> Value {
    use base64::Engine as _;
    json!({
        "created": result.target.created,
        "duration_seconds": round_three(result.duration_seconds),
        "exit_code": result.exit_code,
        "meta": meta,
        "pane_id": result.target.pane_id,
        "run_id": result.run_id,
        "spool_dir": result.spool.directory,
        "stderr": result.stderr_text(),
        "stderr_base64": base64::engine::general_purpose::STANDARD.encode(&result.stderr),
        "stdout": result.stdout_text(),
        "stdout_base64": base64::engine::general_purpose::STANDARD.encode(&result.stdout),
        "tab": result.target.tab_label,
    })
}

fn round_three(value: f64) -> f64 {
    round3(value)
}

fn seconds_duration(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Duration::ZERO;
    }
    if seconds >= Duration::MAX.as_secs_f64() {
        return Duration::MAX;
    }
    Duration::from_secs_f64(seconds)
}

fn validate_timeout(value: f64, what: &str) -> Result<()> {
    if !value.is_finite() {
        return Err(HerdrRunError::config(format!("{what}: must be finite")));
    }
    if value < 0.0 {
        return Err(HerdrRunError::config(format!(
            "{what}: must not be negative"
        )));
    }
    if value > MAX_TIMEOUT_SECONDS {
        return Err(HerdrRunError::config(format!(
            "{what}: must not exceed {MAX_TIMEOUT_SECONDS:.0} seconds"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::allowlist::admit;
    use crate::client::{Pane, ProcessInfo};

    /// Run IDs shared with the other edition.
    #[test]
    fn golden_run_ids() {
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/text_cases.json"))
                .expect("golden text cases");
        for row in document["run_id"].as_array().expect("run id rows") {
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(row[1].as_u64().unwrap());
            let pid = u32::try_from(row[2].as_u64().unwrap()).unwrap();
            assert_eq!(
                make_run_id(row[0].as_str().unwrap(), now, pid),
                row[3],
                "{row:?}"
            );
        }
    }

    struct FakeApi;

    impl HerdrApi for FakeApi {
        fn ensure_server(&self) -> Result<bool> {
            Ok(false)
        }
        fn server_running(&self) -> bool {
            true
        }
        fn workspace_id_for_label(&self, _label: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn create_workspace(&self, _label: &str, _cwd: &str) -> Result<(String, String, String)> {
            unreachable!()
        }
        fn tab_id_for_label(&self, _workspace: &str, _label: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn create_tab(&self, _workspace: &str, _label: &str, _cwd: &str) -> Result<String> {
            unreachable!()
        }
        fn rename_tab(&self, _tab: &str, _label: &str) -> Result<()> {
            unreachable!()
        }
        fn panes(&self, _workspace: Option<&str>) -> Result<Vec<Pane>> {
            Ok(Vec::new())
        }
        fn pane_exists(&self, _pane: &str) -> bool {
            true
        }
        fn process_info(&self, pane: &str) -> Result<ProcessInfo> {
            Ok(ProcessInfo {
                pane_id: pane.to_owned(),
                shell_pid: 7,
                foreground_pgid: 7,
                foreground: vec![(7, "bash".to_owned(), "bash".to_owned())],
            })
        }
        fn read(&self, _pane: &str, _source: &str, _lines: Option<usize>) -> Result<String> {
            Ok(String::new())
        }
        fn run(&self, _pane: &str, command: &str) -> Result<()> {
            let status = Command::new("bash")
                .args(["-lc", command])
                .status()
                .map_err(|error| HerdrRunError::unavailable(error.to_string()))?;
            if status.success() {
                Ok(())
            } else {
                Err(HerdrRunError::unavailable(format!(
                    "fake pane failed: {status}"
                )))
            }
        }
    }

    fn target() -> Target {
        Target {
            workspace_id: "w1".to_owned(),
            tab_id: "t1".to_owned(),
            pane_id: "p1".to_owned(),
            workspace_label: "agent-cmds".to_owned(),
            tab_label: "agent".to_owned(),
            created: Vec::new(),
            from_cache: false,
        }
    }

    #[test]
    fn shell_wrapper_is_portable_outer_command_and_quotes_paths() {
        let config = Config {
            allow: vec!["git".to_owned()],
            ..Config::default()
        };
        let admission = admit("git status", &config).expect("admitted");
        let spool = spool_paths(
            &Config {
                project_root: "/tmp/root x".to_owned(),
                spool_dir: "sp ool".to_owned(),
                ..Config::default()
            },
            "run 1",
        );
        let line =
            build_shell_command(&admission, &spool, Path::new("/tmp/work dir")).expect("rendered");
        assert!(line.starts_with("command sh -c "));
        assert!(line.contains("'\"'\"'/tmp/work dir'\"'\"'"));
    }

    #[test]
    fn pane_lock_is_account_global_across_project_configs() {
        let root =
            std::env::temp_dir().join(format!("herdr-pane-lock-projects-{}", std::process::id()));
        let first_config = Config {
            project_root: root.join("one").to_string_lossy().into_owned(),
            spool_dir: "first-spool".to_owned(),
            ..Config::default()
        };
        let second_config = Config {
            project_root: root.join("two").to_string_lossy().into_owned(),
            spool_dir: "second-spool".to_owned(),
            ..Config::default()
        };
        assert_ne!(
            spool_paths(&first_config, "run").directory,
            spool_paths(&second_config, "run").directory
        );

        let mut target = target();
        target.pane_id = "pane-global-across-project-configs".to_owned();
        let first = acquire_pane_lock(&target, 0.0).expect("first account-global lock");
        let second = acquire_pane_lock(&target, 0.0).unwrap_err();
        assert_eq!(second.kind(), crate::error::ErrorKind::Busy);
        drop(first);
    }

    /// A pane whose shell is THIS process, so a recorded identity binds against the real `/proc`.
    struct SelfShellApi;

    impl HerdrApi for SelfShellApi {
        fn ensure_server(&self) -> Result<bool> {
            FakeApi.ensure_server()
        }
        fn server_running(&self) -> bool {
            FakeApi.server_running()
        }
        fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
            FakeApi.workspace_id_for_label(label)
        }
        fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)> {
            FakeApi.create_workspace(label, cwd)
        }
        fn tab_id_for_label(&self, workspace: &str, label: &str) -> Result<Option<String>> {
            FakeApi.tab_id_for_label(workspace, label)
        }
        fn create_tab(&self, workspace: &str, label: &str, cwd: &str) -> Result<String> {
            FakeApi.create_tab(workspace, label, cwd)
        }
        fn rename_tab(&self, tab: &str, label: &str) -> Result<()> {
            FakeApi.rename_tab(tab, label)
        }
        fn panes(&self, workspace: Option<&str>) -> Result<Vec<Pane>> {
            FakeApi.panes(workspace)
        }
        fn pane_exists(&self, pane: &str) -> bool {
            FakeApi.pane_exists(pane)
        }
        fn process_info(&self, pane: &str) -> Result<ProcessInfo> {
            let shell_pid = i64::from(std::process::id());
            Ok(ProcessInfo {
                pane_id: pane.to_owned(),
                shell_pid,
                foreground_pgid: shell_pid,
                foreground: vec![(shell_pid, "bash".to_owned(), "bash".to_owned())],
            })
        }
        fn read(&self, pane: &str, source: &str, lines: Option<usize>) -> Result<String> {
            FakeApi.read(pane, source, lines)
        }
        fn run(&self, pane: &str, command: &str) -> Result<()> {
            FakeApi.run(pane, command)
        }
    }

    /// A run record with only a shell PID can never authorise closing anything.
    ///
    /// [`crate::reap`] requires `(pid, boot_id, start_ticks)` before it will call a tab stale, so a
    /// writer that records the pid alone makes the whole reaper inert -- it would answer UNKNOWN
    /// for every pane forever, and "reaped 0" would look exactly like a healthy workspace.
    #[test]
    fn meta_records_the_identity_the_reaper_needs() {
        let root = std::env::temp_dir().join(format!("herdr-meta-identity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: "spool".to_owned(),
            allow: vec!["printf".to_owned()],
            ..Config::default()
        };
        let admission = admit("printf ok", &config).expect("admitted");
        let mut isolated_target = target();
        isolated_target.pane_id = "p-meta-identity".to_owned();
        let result = execute(
            &SelfShellApi,
            &config,
            &isolated_target,
            &admission,
            "agent",
            &root,
            0.0,
            5.0,
        )
        .expect("run");
        let path = write_meta(&result, &admission, &config, "agent").expect("metadata");
        let document: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).expect("read meta")).expect("parse");

        assert_eq!(
            document["readiness"]["shell_pid"].as_i64(),
            Some(i64::from(std::process::id()))
        );
        assert!(document["readiness"]["boot_id"]
            .as_str()
            .is_some_and(|boot| !boot.is_empty()));
        assert!(document["readiness"]["shell_start_ticks"]
            .as_u64()
            .is_some());

        let (flags, identity) = crate::sweep::evidence_from_runs("p-meta-identity", &[document]);
        assert_eq!(flags, [true]);
        assert!(identity.is_some_and(|identity| identity.is_bound()));
        let _ = fs::remove_dir_all(root);
    }

    /// Like [`SelfShellApi`], except the pane never actually runs the line, so no exit code ever
    /// appears and collection must time out.
    struct NeverRunsApi;

    impl HerdrApi for NeverRunsApi {
        fn ensure_server(&self) -> Result<bool> {
            SelfShellApi.ensure_server()
        }
        fn server_running(&self) -> bool {
            SelfShellApi.server_running()
        }
        fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
            SelfShellApi.workspace_id_for_label(label)
        }
        fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)> {
            SelfShellApi.create_workspace(label, cwd)
        }
        fn tab_id_for_label(&self, workspace: &str, label: &str) -> Result<Option<String>> {
            SelfShellApi.tab_id_for_label(workspace, label)
        }
        fn create_tab(&self, workspace: &str, label: &str, cwd: &str) -> Result<String> {
            SelfShellApi.create_tab(workspace, label, cwd)
        }
        fn rename_tab(&self, tab: &str, label: &str) -> Result<()> {
            SelfShellApi.rename_tab(tab, label)
        }
        fn panes(&self, workspace: Option<&str>) -> Result<Vec<Pane>> {
            SelfShellApi.panes(workspace)
        }
        fn pane_exists(&self, pane: &str) -> bool {
            SelfShellApi.pane_exists(pane)
        }
        fn process_info(&self, pane: &str) -> Result<ProcessInfo> {
            SelfShellApi.process_info(pane)
        }
        fn read(&self, pane: &str, source: &str, lines: Option<usize>) -> Result<String> {
            SelfShellApi.read(pane, source, lines)
        }
        fn run(&self, _pane: &str, _command: &str) -> Result<()> {
            Ok(())
        }
    }

    /// The ONLY writer of the state the reaper calls IN FLIGHT.
    ///
    /// A command that outlived its deadline is still running in a pane nobody owns. If the timeout
    /// left no record, the one pane that provably still has work in it would be the one pane the
    /// reaper had no evidence about, and `exit_code: null` would be a state no writer could produce.
    #[test]
    fn a_timed_out_run_is_recorded_as_unfinished() {
        let root =
            std::env::temp_dir().join(format!("herdr-meta-unfinished-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: "spool".to_owned(),
            allow: vec!["printf".to_owned()],
            ..Config::default()
        };
        let admission = admit("printf ok", &config).expect("admitted");
        let mut isolated_target = target();
        isolated_target.pane_id = "p-meta-unfinished".to_owned();
        let error = execute(
            &NeverRunsApi,
            &config,
            &isolated_target,
            &admission,
            "agent",
            &root,
            0.0,
            0.05,
        )
        .expect_err("collection must time out");
        assert_eq!(error.kind(), crate::error::ErrorKind::Timeout);

        let runs = root.join("spool").join("runs");
        let entry = fs::read_dir(&runs)
            .expect("runs root")
            .next()
            .expect("one run directory")
            .expect("readable entry");
        let document: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(entry.path().join("meta.json")).expect("read meta"),
        )
        .expect("parse");
        assert_eq!(
            document["exit_code"],
            Value::Null,
            "a run still running must not record a status"
        );
        assert_eq!(document["pane_id"], json!("p-meta-unfinished"));

        let (flags, identity) = crate::sweep::evidence_from_runs("p-meta-unfinished", &[document]);
        assert_eq!(
            flags,
            [false],
            "the reaper must read this as work in flight"
        );
        assert!(identity.is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn public_runtime_apis_reject_invalid_timeouts_before_launch() {
        let config = Config {
            allow: vec!["git".to_owned()],
            ..Config::default()
        };
        let admission = admit("git status", &config).unwrap();
        for timeout in [f64::NAN, f64::INFINITY, -1.0, MAX_TIMEOUT_SECONDS + 1.0] {
            let ready_error = wait_ready(&FakeApi, &config, &target(), None, timeout).unwrap_err();
            assert_eq!(ready_error.kind(), crate::error::ErrorKind::Config);

            let command_error = execute(
                &FakeApi,
                &config,
                &target(),
                &admission,
                "agent",
                Path::new("/tmp"),
                0.0,
                timeout,
            )
            .unwrap_err();
            assert_eq!(command_error.kind(), crate::error::ErrorKind::Config);

            let readiness_error = execute(
                &FakeApi,
                &config,
                &target(),
                &admission,
                "agent",
                Path::new("/tmp"),
                timeout,
                1.0,
            )
            .unwrap_err();
            assert_eq!(readiness_error.kind(), crate::error::ErrorKind::Config);
        }
    }

    #[test]
    fn timeout_context_carries_lossy_partial_output_and_spool_path() {
        let root =
            std::env::temp_dir().join(format!("herdr-timeout-context-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let spool = SpoolPaths {
            directory: root.clone(),
            stdout: root.join("stdout"),
            stderr: root.join("stderr"),
            exit_code: root.join("exit_code"),
        };
        fs::write(&spool.stdout, b"partial-out\n\xff").unwrap();
        fs::write(&spool.stderr, b"partial-err\n").unwrap();

        let error = timeout_error(&spool, "still running".to_owned());
        assert_eq!(error.kind(), crate::error::ErrorKind::Timeout);
        assert_eq!(error.partial_stdout(), "partial-out\n\u{fffd}");
        assert_eq!(error.partial_stderr(), "partial-err\n");
        assert_eq!(error.spool_directory(), Some(root.as_path()));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_hook_prunes_old_run_but_preserves_recent_run() {
        let root =
            std::env::temp_dir().join(format!("herdr-runner-retention-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let runs = root.join("spool/runs");
        let old = runs.join("old");
        let recent = runs.join("recent");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&recent).unwrap();
        fs::write(old.join("exit_code"), "0\n").unwrap();
        let old_time = SystemTime::now() - Duration::from_secs(9 * 86_400);
        std::fs::File::open(old.join("exit_code"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old_time))
            .unwrap();
        let config = Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: "spool".to_owned(),
            retention_days: 4,
            ..Config::default()
        };

        apply_retention(&config);
        assert!(!old.exists());
        assert!(recent.is_dir());
        assert!(runs.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execution_captures_bytes_exit_and_uses_exclusive_collision_suffix() {
        let root = std::env::temp_dir().join(format!("herdr-runner-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let config = Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: "spool".to_owned(),
            allow: vec!["printf".to_owned()],
            ..Config::default()
        };
        let admission = admit("printf '\\377'", &config).expect("admitted");
        let mut isolated_target = target();
        isolated_target.pane_id = "p-execution-captures".to_owned();
        let result = execute(
            &FakeApi,
            &config,
            &isolated_target,
            &admission,
            "agent",
            &root,
            0.0,
            5.0,
        )
        .expect("run");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout, vec![0xff]);
        let meta = write_meta(&result, &admission, &config, "agent").expect("metadata");
        assert!(result_json(&result, None)["meta"].is_null());
        for directory in [
            root.join("spool"),
            root.join("spool/runs"),
            result.spool.directory.clone(),
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        for file in [
            result.spool.stdout.clone(),
            result.spool.stderr.clone(),
            result.spool.exit_code.clone(),
            result.spool.directory.join("command"),
            meta,
        ] {
            assert_eq!(
                fs::metadata(file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let base = make_run_id("agent", SystemTime::now(), std::process::id());
        let first = spool_paths(&config, &base);
        fs::create_dir_all(&first.directory).expect("plant collision");
        fs::write(&first.exit_code, "99\n").expect("plant stale status");
        let (second_id, second) = allocate_spool(&config, &base).expect("new spool");
        assert_ne!(second_id, base);
        assert!(!second.exit_code.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn existing_configured_spool_root_is_never_chmoded() {
        let root =
            std::env::temp_dir().join(format!("herdr-spool-existing-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let config = Config {
            project_root: root.to_string_lossy().into_owned(),
            spool_dir: ".".to_owned(),
            allow: vec!["printf".to_owned()],
            ..Config::default()
        };
        let admission = admit("printf ok", &config).unwrap();

        let mut isolated_target = target();
        isolated_target.pane_id = "p-existing-spool-root".to_owned();
        let result = execute(
            &FakeApi,
            &config,
            &isolated_target,
            &admission,
            "agent",
            &root,
            0.0,
            5.0,
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(root.join("runs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::remove_dir_all(root).unwrap();
    }
}
