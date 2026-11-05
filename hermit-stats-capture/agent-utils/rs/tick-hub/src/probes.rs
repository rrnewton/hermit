//! Production gate-command and filesystem-age probes.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::protocols::{FileAgeProbe, GateResult, GateRunner};

/// Default gate timeout in seconds.
pub const DEFAULT_GATE_TIMEOUT_SECS: u64 = 30;

/// Run gate commands through `bash -c`, capturing stdout under a timeout.
#[derive(Clone, Copy, Debug)]
pub struct SubprocessGateRunner {
    /// Maximum command duration.
    pub timeout: Duration,
}

impl Default for SubprocessGateRunner {
    fn default() -> Self {
        Self::new(DEFAULT_GATE_TIMEOUT_SECS)
    }
}

impl SubprocessGateRunner {
    /// Construct a gate runner with a timeout measured in seconds.
    pub const fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(timeout_secs),
        }
    }
}

impl GateRunner for SubprocessGateRunner {
    fn run(&self, cmd: &str, timeout_secs: Option<u64>) -> GateResult {
        // The gate's own bound when it declares one, else this runner's. Mirrors the Python
        // edition's `run(cmd, timeout=None)`.
        let effective = timeout_secs.map_or(self.timeout, Duration::from_secs);
        let mut command = Command::new("bash");
        command
            .args(["-c", cmd])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Give each gate an isolated process group so timeout cleanup includes descendants.
            command.process_group(0);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return GateResult::failed(error.to_string()),
        };
        // Drain both pipes while the command runs. Waiting first can deadlock when either pipe's
        // kernel buffer fills, so both streams are consumed concurrently.
        let stdout = child.stdout.take().expect("stdout was configured as piped");
        let stderr = child.stderr.take().expect("stderr was configured as piped");
        let stdout_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::BufReader::new(stdout).read_to_end(&mut bytes);
            bytes
        });
        let stderr_reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::BufReader::new(stderr).read_to_end(&mut bytes);
            bytes
        });
        let started = std::time::Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    while started.elapsed() < effective
                        && (!stdout_reader.is_finished() || !stderr_reader.is_finished())
                    {
                        thread::sleep(Duration::from_millis(10));
                    }
                    if !stdout_reader.is_finished() || !stderr_reader.is_finished() {
                        terminate(&mut child);
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        return GateResult::failed(format!(
                            "timed out after {}s",
                            effective.as_secs()
                        ));
                    }
                    let stdout = stdout_reader.join().unwrap_or_default();
                    let _ = stderr_reader.join();
                    let code = status.code().unwrap_or_else(|| signal_returncode(&status));
                    return GateResult::completed(code, String::from_utf8_lossy(&stdout));
                }
                Ok(None) if started.elapsed() < effective => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    terminate(&mut child);
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return GateResult::failed(format!("timed out after {}s", effective.as_secs()));
                }
                Err(error) => {
                    terminate(&mut child);
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    return GateResult::failed(error.to_string());
                }
            }
        }
    }
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: the negated freshly spawned child PID identifies only the process group created
        // above. SIGKILL takes no borrowed memory and errors are intentionally handled by the
        // direct-child fallback below.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn signal_returncode(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    -status.signal().unwrap_or(1)
}

#[cfg(not(unix))]
fn signal_returncode(_status: &std::process::ExitStatus) -> i32 {
    -1
}

/// Measure the newest mtime matching a filesystem glob.
#[derive(Clone, Copy, Debug, Default)]
pub struct GlobFileAgeProbe;

impl FileAgeProbe for GlobFileAgeProbe {
    fn newest_age_secs(&self, pattern: &str, now: i64) -> Option<i64> {
        // `glob.glob(..., recursive=False)` treats adjacent stars as one star, does not cross path
        // separators, and requires a literal leading dot for hidden path components.
        let pattern = collapse_adjacent_stars(pattern);
        let entries = glob::glob_with(
            &pattern,
            glob::MatchOptions {
                case_sensitive: !cfg!(windows),
                require_literal_separator: true,
                require_literal_leading_dot: true,
            },
        )
        .ok()?;
        let mut newest: Option<f64> = None;
        let mut matched = false;
        for entry in entries {
            let path = entry.ok()?;
            matched = true;
            let modified = path.metadata().ok()?.modified().ok()?;
            let seconds = match modified.duration_since(UNIX_EPOCH) {
                Ok(duration) => duration.as_secs_f64(),
                Err(error) => -error.duration().as_secs_f64(),
            };
            newest = Some(newest.map_or(seconds, |old| old.max(seconds)));
        }
        if !matched {
            return None;
        }
        let age = now as f64 - newest?;
        Some((age as i64).max(0))
    }
}

fn collapse_adjacent_stars(pattern: &str) -> String {
    let mut collapsed = String::with_capacity(pattern.len());
    let mut previous_star = false;
    for character in pattern.chars() {
        if character != '*' || !previous_star {
            collapsed.push(character);
        }
        previous_star = character == '*';
    }
    collapsed
}

/// Current Unix epoch seconds.
pub fn wall_clock_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temporary_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tick-hub-probe-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn subprocess_gate_captures_stdout_and_exit_status() {
        let runner = SubprocessGateRunner::new(2);
        let result = runner.run("printf 'count=7\\n'; exit 3", None);
        assert!(result.ok);
        assert_eq!(result.returncode, 3);
        assert_eq!(result.stdout, "count=7\n");
    }

    #[test]
    fn subprocess_gate_replaces_invalid_utf8() {
        let result = SubprocessGateRunner::new(2).run("printf '\\377'", None);
        assert!(result.ok);
        assert_eq!(result.returncode, 0);
        assert_eq!(result.stdout, "\u{fffd}");
    }

    #[test]
    fn subprocess_gate_times_out_loudly() {
        let runner = SubprocessGateRunner::new(0);
        let result = runner.run("sleep 1", None);
        assert!(!result.ok);
        assert_eq!(result.returncode, -1);
        assert_eq!(result.error.as_deref(), Some("timed out after 0s"));
    }

    #[test]
    fn timeout_kills_background_descendants_holding_capture_pipes() {
        let runner = SubprocessGateRunner::new(0);
        let started = std::time::Instant::now();
        let result = runner.run("sleep 30 &", None);
        assert!(!result.ok);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn file_glob_matches_nonrecursive_hidden_file_rules() {
        let root = temporary_path("glob");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join(".hidden.txt"), "hidden").unwrap();
        fs::write(root.join("nested/inside.txt"), "nested").unwrap();
        let probe = GlobFileAgeProbe;
        let pattern = format!("{}/*.txt", root.display());
        assert_eq!(probe.newest_age_secs(&pattern, wall_clock_now()), None);
        let recursive_looking = format!("{}/**/*.txt", root.display());
        assert!(probe
            .newest_age_secs(&recursive_looking, wall_clock_now())
            .is_some());
        let _ = fs::remove_dir_all(root);
    }

    // ---- per-gate timeout enforcement ------------------------------------------------
    // The point of these is that the gate's own bound is ENFORCED, not merely accepted.
    // A parser that takes timeout_secs and a runner that ignores it is worse than refusing
    // the field: the config would claim a bound that never fires.

    #[test]
    fn a_gate_bound_shorter_than_the_runner_default_is_the_one_enforced() {
        // Runner default 30s, gate bound 1s, command sleeps 5s. If the gate bound were
        // ignored this would complete; it must time out instead.
        let runner = SubprocessGateRunner::new(30);
        let started = std::time::Instant::now();
        let result = runner.run("sleep 5", Some(1));
        assert!(
            !result.ok,
            "a gate past its own bound must not report a completion"
        );
        assert_eq!(result.returncode, -1);
        assert_eq!(result.error.as_deref(), Some("timed out after 1s"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it must be terminated at its own bound, not at the command's own end"
        );
    }

    #[test]
    fn a_healthy_command_inside_its_gate_bound_still_succeeds() {
        let runner = SubprocessGateRunner::new(30);
        let result = runner.run("printf 'ok=1\n'; exit 0", Some(10));
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.returncode, 0);
        assert_eq!(result.stdout, "ok=1\n");
        assert!(result.error.is_none());
    }

    #[test]
    fn no_gate_bound_falls_back_to_the_runner_default() {
        // Runner default 1s, no gate bound, command sleeps 5s -> the runner's bound applies.
        let runner = SubprocessGateRunner::new(1);
        let result = runner.run("sleep 5", None);
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("timed out after 1s"));
    }

    #[test]
    fn a_gate_bound_longer_than_the_runner_default_is_also_honoured() {
        // Runner default 1s, gate bound 10s, command sleeps 2s. If the gate bound were
        // ignored the runner default would cut this off; it must complete.
        let runner = SubprocessGateRunner::new(1);
        let result = runner.run("sleep 2; printf 'done\n'", Some(10));
        assert!(result.ok, "{:?}", result.error);
        assert_eq!(result.stdout, "done\n");
    }
}
