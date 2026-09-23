#!/usr/bin/env -S rust-script --force
//! Test the maintained parser against the actual output of package.rs.
//!
//! ```cargo
//! [package]
//! edition = "2024"
//! [dependencies]
//! anyhow = "=1.0.100"
//! libc = "=0.2.189"
//! serde_json = "=1.0.149"
//! sha2 = "=0.10.9"
//! ```

/* SPDX-License-Identifier: BSD-3-Clause */

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MIB: u64 = 1024 * 1024;

fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    let stat = file.metadata()?;
    ensure!(
        stat.is_file() && stat.len() <= limit,
        "input is not a bounded regular file"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "input grew beyond bound");
    Ok(bytes)
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn sources(source: &Path, manifest: &Value) -> Result<BTreeMap<String, String>> {
    let expected = manifest["sources"]
        .as_object()
        .context("package source map")?;
    ensure!(
        expected.contains_key("package.rs") && expected.contains_key("package_support.rs"),
        "package omitted its maintained producer/parser identity"
    );
    let mut result = BTreeMap::new();
    for (name, expected) in expected {
        ensure!(
            !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))),
            "invalid source path"
        );
        let hash = digest(&read(&source.join(name), MIB)?);
        ensure!(
            expected.as_str() == Some(hash.as_str()),
            "stale source: {name}; produce a fresh package first"
        );
        result.insert(name.clone(), hash);
    }
    Ok(result)
}

fn absent(pid: u32) -> Result<bool> {
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

fn kill(pid: u32) -> Result<()> {
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
struct OwnedChild {
    child: std::process::Child,
    cleanup_attempted: bool,
    reaped: bool,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.cleanup_attempted && !self.reaped {
            let result = kill(self.child.id());
            eprintln!(
                "package test supervisor unwound: pid={}, kill={result:?}, terminal_state=unconfirmed",
                self.child.id()
            );
        }
    }
}

#[derive(Clone, Copy)]
struct Limits {
    wall: Duration,
    logs: u64,
    cleanup: Duration,
}
const LIMITS: Limits = Limits {
    wall: Duration::from_secs(60),
    logs: 4 * MIB,
    cleanup: Duration::from_secs(2),
};

fn bounds(started: Instant, stdout: &Path, stderr: &Path, limits: Limits) -> Result<(bool, bool)> {
    let bytes = fs::metadata(stdout)?
        .len()
        .checked_add(fs::metadata(stderr)?.len())
        .context("log size overflow")?;
    Ok((started.elapsed() >= limits.wall, bytes > limits.logs))
}

// Observe, but do not reap, the exact owned child. Its unreaped PID reserves
// the numeric process-group identity through the last possible group signal.
fn exited_without_reap(child: &std::process::Child) -> Result<bool> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    ensure!(
        result == 0,
        "owned child waitid: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { info.si_pid() } == child.id() as i32)
}

fn group_members(group: u32) -> Result<Vec<u32>> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let raw = match fs::read_to_string(entry.path().join("stat")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let (_, fields) = raw.rsplit_once(") ").context("invalid process stat")?;
        let actual = fields
            .split_whitespace()
            .nth(2)
            .context("missing process group")?
            .parse::<u32>()?;
        if actual == group {
            members.push(pid);
        }
    }
    members.sort_unstable();
    Ok(members)
}

fn only_terminal_leader(group: u32, members: &[u32], terminal: bool) -> bool {
    terminal && members == [group]
}

fn supervise(
    mut child: OwnedChild,
    started: Instant,
    stdout: &Path,
    stderr: &Path,
    limits: Limits,
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
    // Retain the old success condition: leader exit with a live descendant
    // is a failure even when later owned cleanup successfully kills it.
    let before_members = group_members(pid);
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
        Ok(_) => match kill(pid) {
            Ok(()) => owned_group_kill = true,
            Err(error) => cleanup_errors.push(format!("owned group kill: {error:#}")),
        },
        Err(error) => cleanup_errors.push(format!("pre-kill child ownership: {error:#}")),
    }
    if owned_group_kill {
        loop {
            match exited_without_reap(&child.child) {
                Ok(true) => {
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
                Ok(false) => {}
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
        && cleanup_complete
        && cleanup_errors.is_empty();
    json!({
        "pid":pid, "raw_status":status.and_then(|status|status.code()),
        "signal":status.and_then(|status|status.signal()),
        "seconds":started.elapsed().as_secs_f64(),
        "timed_out":timed_out, "log_overflow":log_overflow,
        "primary_error":primary_error, "terminal_bounds_error":bounds_error,
        "terminal_observed_without_reap":observed_terminal,
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

fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut values = BTreeMap::new();
    while let Some(key) = args.next() {
        ensure!(
            ["--package-dir", "--output-dir", "--source-dir"]
                .iter()
                .any(|name| key == *name),
            "usage: test.rs --package-dir PACKAGE --output-dir NEW-DIRECTORY [--source-dir DIRECTORY]"
        );
        let value = args.next().context("option requires value")?;
        ensure!(values.insert(key, value).is_none(), "repeated option");
    }
    let package = PathBuf::from(
        values
            .remove(std::ffi::OsStr::new("--package-dir"))
            .context("--package-dir required")?,
    )
    .canonicalize()?;
    let output = std::path::absolute(PathBuf::from(
        values
            .remove(std::ffi::OsStr::new("--output-dir"))
            .context("--output-dir required")?,
    ))?;
    let source = PathBuf::from(
        values
            .remove(std::ffi::OsStr::new("--source-dir"))
            .unwrap_or_else(|| Path::new(file!()).parent().unwrap().into()),
    )
    .canonicalize()?;
    let manifest_bytes = read(&package.join("manifest.json"), 32768)?;
    let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
    ensure!(
        manifest["schema"] == 1 && manifest["compile_only"] == true,
        "unsupported package manifest"
    );
    let object = match manifest["kind"].as_str() {
        Some("hermit-accepted-provider") => "accepted-provider.bpf.o",
        Some("hermit-unix-guard") => "unix-guard.bpf.o",
        _ => anyhow::bail!("unsupported package kind"),
    };
    ensure!(manifest["object"] == object, "object name changed");
    let before = package.join("build/uncompressed.bpf.o");
    let after = package.join(object);
    let before_hash = digest(&read(&before, 16 * MIB)?);
    let after_hash = digest(&read(&after, MIB)?);
    ensure!(
        manifest["object_sha256"] == after_hash,
        "packaged object hash mismatch"
    );
    let source_hashes = sources(&source, &manifest)?;
    fs::create_dir(&output).context("test output must be a new directory")?;
    fs::write(
        output.join("inputs.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_sha256":digest(&manifest_bytes), "source":source, "sources":source_hashes,
            "before":before, "before_sha256":before_hash, "after":after, "after_sha256":after_hash,
            "new_bpf_load":false, "guest_executed":false
        }))?,
    )?;
    let stdout_path = output.join("stdout");
    let stderr_path = output.join("stderr");
    let mut command = Command::new("rust-script");
    // rust-script's test mode always invokes Cargo, including dependency
    // freshness; its --force option is both redundant and incompatible here.
    // The environment applies the serial harness setting through both the
    // normal cargo-test path and the official prepared-harness dispatcher.
    command
        .arg("--test")
        .arg(source.join("package.rs"))
        .env("RUST_TEST_THREADS", "1")
        .current_dir(&source)
        .env("HERMIT_PACKAGE_TEST_BEFORE", &before)
        .env("HERMIT_PACKAGE_TEST_AFTER", &after)
        .env("CARGO_BUILD_JOBS", "2")
        .env("CARGO_NET_OFFLINE", "true")
        .env("DAGRUN_LOG_DIR", &output)
        .stdin(Stdio::null())
        .stdout(File::create(&stdout_path)?)
        .stderr(File::create(&stderr_path)?);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for (resource, limit) in [
                (libc::RLIMIT_CORE, 0),
                (libc::RLIMIT_AS, 4 * 1024 * MIB),
                (libc::RLIMIT_FSIZE, 64 * MIB),
            ] {
                let value = libc::rlimit {
                    rlim_cur: limit,
                    rlim_max: limit,
                };
                if libc::setrlimit(resource, &value) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let mut receipt = match command.spawn() {
        Ok(child) => supervise(
            OwnedChild {
                child,
                cleanup_attempted: false,
                reaped: false,
            },
            started,
            &stdout_path,
            &stderr_path,
            LIMITS,
        ),
        Err(error) => json!({
            "pid":null, "raw_status":null, "signal":null,
            "primary_error":format!("spawn: {error}"),
            "cleanup_attempted":false, "cleanup_complete":null,
            "passed":false
        }),
    };
    let fence = (|| -> Result<bool> {
        Ok(before_hash == digest(&read(&before, 16 * MIB)?)
            && after_hash == digest(&read(&after, MIB)?)
            && manifest_bytes == read(&package.join("manifest.json"), 32768)?
            && source_hashes == sources(&source, &manifest)?)
    })();
    let unchanged = matches!(fence, Ok(true));
    let input_error = fence.err().map(|error| format!("{error:#}"));
    let passed = receipt["passed"] == true && unchanged;
    receipt["inputs_unchanged"] = json!(unchanged);
    receipt["input_error"] = json!(input_error);
    receipt["passed"] = json!(passed);
    let bytes = serde_json::to_vec_pretty(&receipt)?;
    if let Err(error) = fs::write(output.join("result.json"), &bytes) {
        // Preserve the primary and cleanup results even if the result file
        // cannot be written; never turn that failure into successful cleanup.
        eprintln!("package test result persistence failed: {error}; receipt={receipt}");
        return Err(error.into());
    }
    ensure!(
        passed,
        "package parser test action failed; raw evidence in {}",
        output.display()
    );
    println!("{}", json!({"passed":true,"evidence":output}));
    Ok(())
}

fn main() {
    rust_script_prelude::init();
    if let Err(error) = run() {
        eprintln!("network provider parser tests failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Case {
        path: PathBuf,
    }
    impl Case {
        fn new() -> Self {
            let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let path = root.join(format!(
                "package-test-action-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
        fn spawn(&self, script: &str) -> OwnedChild {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(File::create(self.path.join("stdout")).unwrap())
                .stderr(File::create(self.path.join("stderr")).unwrap());
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            OwnedChild {
                child: command.spawn().unwrap(),
                cleanup_attempted: false,
                reaped: false,
            }
        }
        fn check(&self, child: OwnedChild, started: Instant, limits: Limits) -> Value {
            let result = supervise(
                child,
                started,
                &self.path.join("stdout"),
                &self.path.join("stderr"),
                limits,
            );
            fs::write(
                self.path.join("result.json"),
                serde_json::to_vec_pretty(&result).unwrap(),
            )
            .unwrap();
            result
        }
    }

    fn wait_for_exit(child: &OwnedChild) {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !exited_without_reap(&child.child).unwrap() {
            assert!(Instant::now() < deadline, "control child did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Repeated WNOWAIT must still return the same exact owned child.
        assert!(exited_without_reap(&child.child).unwrap());
    }

    #[test]
    fn fast_successful_output_cannot_escape_terminal_log_limit() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("printf '0123456789abcdef'");
        wait_for_exit(&child); // actually exited before first supervision poll; still unreaped
        let result = case.check(child, started, Limits { logs: 8, ..LIMITS });
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["log_overflow"], true);
        assert_eq!(result["passed"], false);
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["final_group_absent"], true);
    }

    #[test]
    fn terminal_wall_deadline_remains_required_after_successful_exit() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("exit 0");
        wait_for_exit(&child);
        let result = case.check(
            child,
            started,
            Limits {
                wall: Duration::ZERO,
                ..LIMITS
            },
        );
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["passed"], false);
        assert_eq!(result["cleanup_complete"], true);
    }

    #[test]
    fn monitoring_error_retains_primary_and_proven_cleanup() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("exec /bin/sleep 10");
        fs::remove_file(case.path.join("stdout")).unwrap();
        let result = case.check(child, started, LIMITS);
        assert!(
            result["primary_error"]
                .as_str()
                .unwrap()
                .contains("No such file")
        );
        assert!(result["terminal_bounds_error"].is_string());
        assert_eq!(result["signal"], libc::SIGKILL);
        assert_eq!(result["cleanup_attempted"], true);
        assert_eq!(result["cleanup_complete"], true);
        assert_eq!(result["final_group_absent"], true);
        assert_eq!(result["passed"], false);
    }

    #[test]
    fn raw_nonzero_status_is_preserved() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("exit 7");
        let result = case.check(child, started, LIMITS);
        assert_eq!(result["raw_status"], 7);
        assert_eq!(result["passed"], false);
        assert_eq!(result["cleanup_complete"], true);
    }

    #[test]
    fn bounded_success_requires_natural_terminal_group() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("printf ok");
        let result = case.check(child, started, LIMITS);
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["passed"], true);
        assert_eq!(result["terminal_observed_without_reap"], true);
        assert_eq!(result["natural_terminal_group"], true);
        assert_eq!(result["owned_group_kill_before_reap"], true);
        assert_eq!(result["final_group_absent"], true);
    }
    #[test]
    fn cleanup_observation_after_deadline_cannot_pass() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("exit 0");
        wait_for_exit(&child);
        let result = case.check(
            child,
            started,
            Limits {
                cleanup: Duration::ZERO,
                ..LIMITS
            },
        );
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["final_group_absent"], true);
        assert_eq!(result["cleanup_within_bound"], false);
        assert_eq!(result["cleanup_complete"], false);
        assert_eq!(result["passed"], false);
    }

    #[test]
    fn leader_identity_is_retained_through_kill_and_reaped_exactly_once() {
        let case = Case::new();
        let started = Instant::now();
        let child = case.spawn("exit 0");
        let pid = child.child.id();
        wait_for_exit(&child);
        // The zombie still reserves this process-group identity before supervision.
        assert!(!absent(pid).unwrap());
        let result = case.check(child, started, LIMITS);
        assert_eq!(result["owned_group_kill_before_reap"], true);
        assert_eq!(result["raw_status"], 0);
        assert_eq!(result["passed"], true);
        assert_eq!(result["unreaped_child_retained_until_receipt"], false);
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
    #[test]
    fn remaining_descendant_is_not_relabelled_success_after_cleanup() {
        assert!(only_terminal_leader(7, &[7], true));
        assert!(!only_terminal_leader(7, &[7, 9], true));
        assert!(!only_terminal_leader(7, &[9], true));
        assert!(!only_terminal_leader(7, &[7], false));
        assert!(!only_terminal_leader(7, &[], true));
    }
}
