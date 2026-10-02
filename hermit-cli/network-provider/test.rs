#!/usr/bin/env -S rust-script --force
//! Test the maintained parser and required provider controls against package.rs output.
//!
//! ```cargo
//! [package]
//! edition = "2024"
//! [dependencies]
//! anyhow = "=1.0.104"
//! libc = "=0.2.189"
//! serde_json = "=1.0.149"
//! sha2 = "=0.10.9"
//! ```

/* SPDX-License-Identifier: BSD-3-Clause */

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

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
        expected.contains_key("package.rs")
            && expected.contains_key("package_support.rs")
            && expected.contains_key("process_group.rs"),
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

#[path = "process_group.rs"]
mod process_group;
#[path = "driver-ftrace-process.rs"]
mod driver_ftrace_process;
use driver_ftrace_process::{DRIVER_FTRACE_IMPORT_FENCE, execute_stage};

// Diagnostic records share the original stderr and combined 4-MiB budget.
// They never substitute for the final result or original child custody.
static DIAGNOSTIC_LOCK: Mutex<()> = Mutex::new(());
static DIAGNOSTIC_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn append_stage_diagnostic_with(
    event: &Value, started: Instant, stdout: &Path, stderr: &Path,
    persist: impl FnOnce(&File) -> std::io::Result<()>,
) -> Result<()> {
    append_stage_diagnostic_ops(event, started, stdout, stderr, persist, |fd| {
        if unsafe { libc::close(fd) } == 0 { Ok(()) }
        else { Err(std::io::Error::last_os_error()) }
    })
}

fn append_stage_diagnostic_ops(
    event: &Value, started: Instant, stdout: &Path, stderr: &Path,
    persist: impl FnOnce(&File) -> std::io::Result<()>,
    close: impl FnOnce(std::os::fd::RawFd) -> std::io::Result<()>,
) -> Result<()> {
    let _guard = DIAGNOSTIC_LOCK.lock().map_err(|_| anyhow::anyhow!("diagnostic lock poisoned"))?;
    let limits = driver_ftrace_process::LIMITS;
    ensure!(process_group::bounds(started, stdout, stderr, limits)? == (false, false),
        "original stage diagnostic wall/log bound");
    let mut bytes = b"PROVIDER_STAGE ".to_vec();
    serde_json::to_writer(&mut bytes, event)?;
    bytes.push(b'\n');
    let mut file = File::options().append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK).open(stderr)?;
    let result = (|| -> Result<()> {
        let held = file.metadata()?;
        let named = fs::symlink_metadata(stderr)?;
        ensure!(held.is_file() && named.is_file() &&
            (held.dev(), held.ino()) == (named.dev(), named.ino()), "diagnostic file changed");
        let total = fs::metadata(stdout)?.len().checked_add(held.len())
            .and_then(|n| n.checked_add(bytes.len() as u64)).context("diagnostic size overflow")?;
        ensure!(total <= limits.logs, "diagnostics exceed original combined log bound");
        // One append syscall keeps a complete record contiguous with other
        // child stderr writes. A short append is retained as a failed record,
        // never retried into an apparently complete but interleaved event.
        ensure!(file.write(&bytes)? == bytes.len(), "short stage diagnostic append");
        file.flush()?;
        persist(&file)?;
        let named = fs::symlink_metadata(stderr)?;
        ensure!(named.is_file() && (held.dev(), held.ino()) == (named.dev(), named.ino()),
            "diagnostic pathname rebound");
        Ok(())
    })();
    // Explicit close, including the error path; a readable complete record is
    // not evidence that fsync/close or the original deadline succeeded.
    let fd = file.into_raw_fd();
    let closed = close(fd);
    result?;
    closed.context("close stage diagnostic")?;
    ensure!(process_group::bounds(started, stdout, stderr, limits)? == (false, false),
        "original stage diagnostic terminal wall/log bound");
    Ok(())
}

fn diagnostic_failure(mut receipt: Value, error: String) -> Value {
    receipt["passed"] = json!(false);
    receipt["diagnostic_error"] = json!(error);
    // Expected-abort controls must not accept a failed diagnostic publication.
    if receipt["terminal_bounds_error"].is_null() {
        receipt["terminal_bounds_error"] = receipt["diagnostic_error"].clone();
    }
    receipt
}

fn execute_logged_stage_with(
    command: &mut Command, started: Instant, stdout: &Path, stderr: &Path,
    mut append: impl FnMut(&Value) -> Result<()>,
) -> Value {
    let sequence = DIAGNOSTIC_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let event = json!({"schema":1,"sequence":sequence,"event":"start",
        "elapsed_ns":started.elapsed().as_nanos(),"command":format!("{command:?}")});
    if let Err(error) = append(&event) {
        return diagnostic_failure(json!({"passed":false,"pid":null,"raw_status":null,
            "signal":null,"cleanup_attempted":false,"cleanup_complete":null,
            "primary_error":"stage diagnostic admission refused; no child spawned"}), format!("{error:#}"));
    }
    let receipt = execute_stage(command, started, stdout, stderr);
    let event = json!({"schema":1,"sequence":sequence,"event":"complete",
        "elapsed_ns":started.elapsed().as_nanos(),"receipt":receipt});
    match append(&event) {
        Ok(()) => receipt,
        Err(error) => diagnostic_failure(receipt, format!("{error:#}")),
    }
}

fn execute_logged_stage(command: &mut Command, started: Instant, stdout: &Path, stderr: &Path) -> Value {
    execute_logged_stage_with(command, started, stdout, stderr, |event| {
        append_stage_diagnostic_with(event, started, stdout, stderr, File::sync_all)
    })
}

// Independent role executions use this fixed bound. Each worker owns its
// Command/Child and calls the unchanged stage supervisor with the action's
// original start and aggregate append-only logs. New failed-prefix sequences
// use the same join/failure discipline with a stricter two-compiler bound.
#[cfg(test)]
const ROLE_MUTANT_CONCURRENCY: usize = 4;

fn role_worker_failure(error: String) -> Value {
    // No child receipt is available on a thread admission failure or panic.
    // In particular, joining a panicked worker does not certify its cleanup.
    json!({"passed":false,"expected_refusal":true,"raw":null,"worker_error":error})
}

#[cfg(test)]
fn role_batches(
    roles: std::ops::RangeInclusive<u32>,
    execute: impl Fn(u32) -> Value + Sync,
) -> Vec<(u32, Value)> {
    bounded_batches(roles, ROLE_MUTANT_CONCURRENCY, execute)
}

#[cfg(test)]
fn bounded_batches(
    roles: std::ops::RangeInclusive<u32>,
    concurrency: usize,
    execute: impl Fn(u32) -> Value + Sync,
) -> Vec<(u32, Value)> {
    bounded_batches_until(roles, concurrency, || false, execute)
}

fn bounded_batches_until(
    roles: std::ops::RangeInclusive<u32>,
    concurrency: usize,
    cancelled: impl Fn() -> bool,
    execute: impl Fn(u32) -> Value + Sync,
) -> Vec<(u32, Value)> {
    let roles: Vec<_> = roles.collect();
    let mut results = Vec::new();
    for batch in roles.chunks(concurrency) {
        if cancelled() { break; }
        let completed = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            let mut admission_failure = None;
            for &role in batch {
                if cancelled() { break; }
                let execute = &execute;
                match std::thread::Builder::new().spawn_scoped(scope, move || execute(role)) {
                    Ok(worker) => workers.push((role, worker)),
                    Err(error) => {
                        admission_failure = Some((role, role_worker_failure(format!(
                            "role worker spawn: {error}"
                        ))));
                        break;
                    }
                }
            }
            // Join ALL launched workers, including after a failed receipt or
            // panic. Preserve canonical role order rather than completion order.
            let mut completed: Vec<_> = workers.into_iter().map(|(role, worker)| {
                let receipt = worker.join().unwrap_or_else(|panic| {
                    let message = panic.downcast_ref::<String>().map(String::as_str)
                        .or_else(|| panic.downcast_ref::<&str>().copied())
                        .unwrap_or("non-string panic");
                    role_worker_failure(format!("role worker panic: {message}"))
                });
                (role, receipt)
            }).collect();
            completed.extend(admission_failure);
            completed
        });
        let failed = completed.iter().any(|(_, receipt)| receipt["passed"] != true);
        results.extend(completed);
        if failed { break; }
    }
    results
}

fn role_mutant_receipt(tested: Value) -> Value {
    let refused=tested["passed"]==false &&
        tested["signal"].as_i64()==Some(libc::SIGABRT as i64) && tested["timed_out"]==false &&
        tested["log_overflow"]==false && tested["primary_error"].is_null() &&
        tested["terminal_bounds_error"].is_null() &&
        tested["natural_terminal_group"]==true && tested["cleanup_complete"]==true &&
        tested["cleanup_errors"].as_array().is_some_and(|errors| errors.is_empty());
    json!({"passed":refused,"expected_refusal":true,"raw":tested})
}

#[cfg(test)]
fn execute_role_mutants(
    executable: &Path,
    roles: std::ops::RangeInclusive<u32>,
    started: Instant,
    stdout: &Path,
    stderr: &Path,
) -> Vec<(u32, Value)> {
    execute_role_mutants_with(executable, roles, started, stdout, stderr, execute_stage)
}

#[cfg(test)]
fn execute_role_mutants_with(
    executable: &Path, roles: std::ops::RangeInclusive<u32>, started: Instant,
    stdout: &Path, stderr: &Path,
    execute: impl Fn(&mut Command, Instant, &Path, &Path) -> Value + Sync,
) -> Vec<(u32, Value)> {
    role_batches(roles, |role| {
        let mut command = Command::new(executable);
        command.env("AP_FTRACE_MUTATE_ROLE", role.to_string());
        role_mutant_receipt(execute(&mut command, started, stdout, stderr))
    })
}

#[cfg(test)]
fn append_role_receipts(
    stages: &mut Vec<Value>,
    receipt: &mut Value,
    completed: Vec<(u32, Value)>,
) {
    for (role, accepted) in completed {
        // A later success in this already-started batch cannot erase failure.
        if accepted["passed"] != true && receipt["passed"] == true {
            *receipt = accepted.clone();
        }
        stages.push(json!({"name":format!("test:ftrace-mutant-{role}"),"receipt":accepted}));
    }
}

const ACCEPTED_CONTROLS: &[&str] = &[
    "ftrace-coverage-test.c",
    "driver-ftrace-test.c",
    "fd-effects-test.c",
    "fd-effects-driver-test.c",
    "fd-table-test.c",
    "remove-test.c",
    "retirement-target-test.c",
    "fd-enrollment-test.c",
    "fd-enrollment-driver-test.c",
    "birth-cleanup-driver-test.c",
    "stream-copy-driver-test.c",
    "grouped-owner-test.c",
    "stream-membership-test.c",
    "grouped-target-test.c",
    "stream-frontier-test.c",
    "stream-copy-v5-driver-test.c",
    "stream-copy-fault-producer-test.c",
    "stream-membership-v5-test.c",
    "stream-tx-test.c",
    "fd-journal-publish-test.c",
    "fd-shared-predicate-test.c",
    "grouped-recovery-test.c",
    "grouped-adoption-test.c",
    "grouped-wire-test.c",
];
const UNIX_CONTROLS: &[(&str, &[&str])] = &[
    (
        "keeper-terminal-tests.c",
        &["syscall", "poll", "write", "fdatasync", "close", "unlinkat", "fstat", "fstatat"],
    ),
    (
        "keeper-phase-tests.c",
        &[
            "syscall", "poll", "write", "fdatasync", "close", "unlinkat", "fstat", "fstatat",
            "fcntl", "clock_gettime",
        ],
    ),
    (
        "keeper-readback-tests.c",
        &["fcntl", "fstat", "pread", "clock_gettime", "nanosleep", "syscall", "close"],
    ),
];

// Test-only channel implementation inputs are not part of the production
// DSO contract. Hash every separately compiled module/header independently.
const CHANNEL_CONTROL_INPUTS: &[&str] = &[
    "grouped-owner.c", "grouped-io.c",
    "grouped-keeper-wire.c", "grouped-keeper-wire.h",
    "grouped-keeper-dual.c", "grouped-keeper-dual.h",
    "grouped-guardian-bootstrap.c", "grouped-guardian-bootstrap.h",
    "grouped-adoption-wire.c", "grouped-adoption-wire.h",
];
fn channel_control_inputs(source: &Path, accepted: bool) -> Result<BTreeMap<String, String>> {
    if !accepted { return Ok(BTreeMap::new()); }
    CHANNEL_CONTROL_INPUTS.iter().map(|name| {
        Ok(((*name).to_owned(), digest(&read(&source.join(name), MIB)?)))
    }).collect()
}

// Complete control identity: shared owned-metadata headers are also bound by
// the production package; facade/process/probe files remain test-only inputs.
const DRIVER_FTRACE_CONTROL_INPUTS: &[&str] = &[
    "driver-ftrace-facade.h", "driver-ftrace-process.rs", "test.rs",
    "legacy-id-probe.h", "owned-metadata-driver.h", "owned-metadata.h",
    "provider-open-observation.h",
    "fd-session-dispatch-test.c",
];
const OWNED_DRIVER_CONTROLS: &[&str] = &[
    "owned-inventory", "owned-identity", "owned-fault", "owned-gate",
];

fn expected_stage_count(accepted: bool) -> usize {
    1 + if accepted {
        // Preserve all original stages and add the four owned-metadata controls.
        2 * ACCEPTED_CONTROLS.len() + 32 + 26 + OWNED_DRIVER_CONTROLS.len()
    } else {
        2 * UNIX_CONTROLS.len()
    }
}

fn stages_complete(stages: &[Value], expected: usize) -> bool {
    stages.len() == expected && stages.iter().all(|s| s["receipt"]["passed"] == true)
}
fn control_sources(source: &Path, accepted: bool) -> Result<BTreeMap<String, String>> {
    if !accepted {
        return [
            "unix/keeper-terminal-tests.c",
            "unix/keeper-phase-tests.c",
            "unix/keeper-readback-tests.c",
        ]
        .into_iter()
        .map(|name| Ok((name.to_owned(), digest(&read(&source.join(name), MIB)?))))
        .collect();
    }
    ACCEPTED_CONTROLS
        .iter()
        .chain(DRIVER_FTRACE_CONTROL_INPUTS)
        .map(|name| Ok(((*name).to_owned(), digest(&read(&source.join(name), MIB)?))))
        .collect()
}

// Each sequence has unique output paths and its original child owners. The
// forking/SCM grouped-wire leaf is deliberately excluded from this batch.
const INITIAL_CONTROL_CONCURRENCY: usize = 2;

// Cancellation only stops admission. Actual Child/group ownership stays in
// execute_stage; no atomic bit grants wait, kill, cleanup or timing authority.
fn initial_branch(stop: &AtomicBool, execute: impl FnOnce() -> Value) -> Value {
    let receipt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(execute))
        .unwrap_or_else(|panic| {
            let message = panic.downcast_ref::<String>().map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            role_worker_failure(format!("initial branch panic: {message}"))
        });
    if receipt["passed"] != true { stop.store(true, Ordering::Release); }
    receipt
}

fn overlap_initial_gate(
    stop: &AtomicBool,
    indices: std::ops::RangeInclusive<u32>,
    parser: impl FnOnce() -> Value + Send,
    control: impl Fn(u32) -> Value + Sync,
) -> (Value, Vec<(u32, Value)>) {
    std::thread::scope(|scope| {
        let parser_stop = stop;
        let parser = match std::thread::Builder::new().spawn_scoped(scope, move || {
            initial_branch(parser_stop, parser)
        }) {
            Ok(parser) => parser,
            Err(error) => return (role_worker_failure(format!("parser worker spawn: {error}")), Vec::new()),
        };
        // Parser failure/panic publishes cancellation before its final join.
        // No unadmitted C batch starts once that failure has been observed.
        let controls = bounded_batches_until(indices, INITIAL_CONTROL_CONCURRENCY,
            || stop.load(Ordering::Acquire),
            |index| initial_branch(stop, || control(index)));
        // All admitted C workers were joined, even on failure. Always join
        // the parser too; never publish partial success or detach it on a C
        // failure. The outer original action deadline still bounds both.
        let parser = parser.join().unwrap_or_else(|_| {
            role_worker_failure("parser join panic; cleanup unproven".into())
        });
        (parser, controls)
    })
}

fn initial_gate_receipts(parser: Value, controls: Vec<(u32, Value)>) -> (Value, Vec<Value>) {
    // Canonical parser-first order is independent of completion order.
    // A late parser failure wins over a C failure; otherwise retain the first
    // failed C receipt in original control order, never a later success.
    let mut receipt = parser;
    let mut stages = vec![json!({"name":"compression-parser","receipt":receipt})];
    for (_, completed) in controls {
        append_control_receipts(&mut stages, &mut receipt, completed);
    }
    (receipt, stages)
}

fn accepted_control_sequence(
    name: &str, source: &Path, output: &Path, started: Instant,
    stdout_path: &Path, stderr_path: &Path,
) -> Value {
    let name = &name;
    let mut stages = Vec::new();
    let mut receipt = json!({"passed":true});
    'sequence: {
        let executable = output.join(name.strip_suffix(".c").unwrap());
        let mut compile = Command::new("clang");
        let driver_ftrace = *name == "driver-ftrace-test.c";
        let driver_object = output.join("driver-ftrace-test.o");
        if driver_ftrace {
            // No libbpf, dynamic lookup, GC of unreachable effects or native
            // syscall fallback. Inspect every undefined symbol before run.
            compile.args(["-std=gnu11", "-fno-builtin", "-DAP_FTRACE_PROVIDER=1",
                "-DAP_NATIVE_COPY_VERSION=5ULL", "-c"]);
        }
        if *name == "birth-cleanup-driver-test.c" {
            // This existing control includes driver.c and mocks only the
            // reached boundaries. Keep its reviewed standalone flags;
            // its stages share this action's original aggregate limits.
            compile.args([
                "-std=gnu11",
                "-ffunction-sections",
                "-fdata-sections",
                "-Wl,--gc-sections",
            ]);
        }
        if matches!(*name, "grouped-owner-test.c" | "grouped-recovery-test.c" | "grouped-adoption-test.c") {
            compile.arg(source.join("grouped-owner.c"));
        }
        if *name == "grouped-wire-test.c" {
            for module in ["grouped-owner.c", "grouped-io.c", "grouped-keeper-wire.c",
                "grouped-keeper-dual.c", "grouped-guardian-bootstrap.c", "grouped-adoption-wire.c"] {
                compile.arg(source.join(module));
            }
            compile.arg("-l:libcrypto.so.3");
        }
        compile
            .args(["-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG"])
            .arg("-I")
            .arg(&source)
            .arg(source.join(name))
            .arg("-o")
            .arg(if driver_ftrace { &driver_object } else { &executable });
        let compiled = execute_logged_stage(&mut compile, started, &stdout_path, &stderr_path);
        stages.push(json!({ "name":format!("compile:{name}"), "receipt":compiled }));
        if compiled["passed"] != true {
            receipt = compiled;
            break 'sequence;
        }
        if driver_ftrace {
            let mut fence = Command::new("python3");
            fence.arg("-c").arg(DRIVER_FTRACE_IMPORT_FENCE).arg(&driver_object);
            let checked = execute_logged_stage(&mut fence, started, &stdout_path, &stderr_path);
            stages.push(json!({"name":"fence:driver-ftrace-imports","receipt":checked}));
            if checked["passed"] != true { receipt = checked; break 'sequence; }
            let mut link = Command::new("clang");
            link.arg(&driver_object).arg("-Wl,--no-undefined").arg("-o").arg(&executable);
            let linked = execute_logged_stage(&mut link, started, &stdout_path, &stderr_path);
            stages.push(json!({"name":"link:driver-ftrace-control","receipt":linked}));
            if linked["passed"] != true { receipt = linked; break 'sequence; }
        }
        let tested = execute_logged_stage(
            &mut Command::new(&executable),
            started,
            &stdout_path,
            &stderr_path,
        );
        stages.push(json!({ "name":format!("test:{name}"), "receipt":tested }));
        if tested["passed"] != true {
            receipt = tested;
            break 'sequence;
        }

        if driver_ftrace {
            for selector in OWNED_DRIVER_CONTROLS {
                let tested = execute_logged_stage(
                    Command::new(&executable).arg(selector),
                    started,
                    &stdout_path,
                    &stderr_path,
                );
                stages.push(json!({"name":format!("test:driver-ftrace:{selector}"),"receipt":tested}));
                if tested["passed"] != true {
                    receipt = tested;
                    break 'sequence;
                }
            }
            if receipt["passed"] != true { break 'sequence; }
        }
    }
    json!({"passed":receipt["passed"] == true,"receipt":receipt,"stages":stages})
}

fn append_control_receipts(stages: &mut Vec<Value>, receipt: &mut Value, completed: Value) {
    if completed["passed"] != true && receipt["passed"] == true {
        *receipt = completed.get("receipt").cloned().unwrap_or_else(|| completed.clone());
    }
    if let Some(local) = completed["stages"].as_array() {
        stages.extend(local.iter().cloned());
    }
}

// Post-wire work has one flat admission boundary: no worker starts a nested
// pool. A node owns precisely one original execute_logged_stage invocation.
const POSTWIRE_COMPILERS: usize = 2;
const POSTWIRE_CHILDREN: usize = 4;

struct PostwireStage {
    name: String,
    command: Command,
    compiler: bool,
    prerequisite: Option<usize>,
    expected_abort: bool,
}

fn push_postwire(
    stages: &mut Vec<PostwireStage>, name: String, command: Command,
    compiler: bool, prerequisite: Option<usize>, expected_abort: bool,
) -> usize {
    let index = stages.len();
    stages.push(PostwireStage { name, command, compiler, prerequisite, expected_abort });
    index
}

fn postwire_ready(
    pending: &[Option<PostwireStage>], completed: &[Option<Value>],
) -> Vec<usize> {
    let mut ready = Vec::new();
    let mut compilers = 0;
    for (index, stage) in pending.iter().enumerate() {
        let Some(stage) = stage else { continue };
        if stage.prerequisite.is_some_and(|dependency|
            completed[dependency].as_ref().is_none_or(|r| r["passed"] != true)) {
            continue;
        }
        if stage.compiler && compilers == POSTWIRE_COMPILERS { continue; }
        compilers += usize::from(stage.compiler);
        ready.push(index);
        if ready.len() == POSTWIRE_CHILDREN { break; }
    }
    ready
}

fn postwire_schedule(
    plan: Vec<PostwireStage>,
    execute: impl Fn(usize, &mut Command) -> Value + Sync,
) -> Result<Vec<Value>> {
    // Validate the complete graph before any child can be admitted. All edges
    // point backwards in canonical order, ruling out cycles and foreign nodes.
    ensure!(plan.iter().enumerate().all(|(i, node)|
        node.prerequisite.is_none_or(|dependency| dependency < i)),
        "invalid post-wire prerequisite");
    let names: Vec<_> = plan.iter().map(|node| node.name.clone()).collect();
    let mut pending: Vec<_> = plan.into_iter().map(Some).collect();
    let mut completed: Vec<Option<Value>> = vec![None; pending.len()];
    let stop = AtomicBool::new(false);
    while pending.iter().any(Option::is_some) && !stop.load(Ordering::Acquire) {
        let ready = postwire_ready(&pending, &completed);
        ensure!(!ready.is_empty(), "post-wire graph has no admissible node");
        let batch = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            let mut refused = None;
            for index in ready {
                // Failure/panic only cancels admission. It never substitutes
                // for original child terminal/reap/group/stream evidence.
                if stop.load(Ordering::Acquire) { break; }
                let mut node = pending[index].take().unwrap();
                let execute = &execute;
                let stop = &stop;
                match std::thread::Builder::new().spawn_scoped(scope, move || {
                    initial_branch(stop, || {
                        let raw = execute(index, &mut node.command);
                        if node.expected_abort { role_mutant_receipt(raw) } else { raw }
                    })
                }) {
                    Ok(worker) => workers.push((index, worker)),
                    Err(error) => {
                        stop.store(true, Ordering::Release);
                        refused = Some((index, role_worker_failure(format!(
                            "post-wire worker spawn: {error}"))));
                        break;
                    }
                }
            }
            // No early return: every admitted worker is joined, including
            // after panic or failed spawn. Unwind Drop remains failure-only.
            let mut results: Vec<_> = workers.into_iter().map(|(index, worker)| {
                let receipt = worker.join().unwrap_or_else(|_| {
                    stop.store(true, Ordering::Release);
                    role_worker_failure("post-wire join panic; cleanup unproven".into())
                });
                (index, receipt)
            }).collect();
            results.extend(refused);
            results
        });
        for (index, receipt) in batch { completed[index] = Some(receipt); }
        // This batch barrier bounds total children even when a failed worker
        // returned before a slower original owner finished its cleanup.
        if completed.iter().flatten().any(|r| r["passed"] != true) { break; }
    }
    Ok(names.into_iter().zip(completed).filter_map(|(name, receipt)|
        receipt.map(|receipt| json!({"name":name,"receipt":receipt}))).collect())
}

fn append_postwire_receipts(stages: &mut Vec<Value>, receipt: &mut Value, completed: Vec<Value>) {
    for stage in completed {
        if stage["receipt"]["passed"] != true && receipt["passed"] == true {
            *receipt = stage["receipt"].clone();
        }
        stages.push(stage);
    }
}

fn postwire_plan(source: &Path, output: &Path) -> Vec<PostwireStage> {
    let mut stages = Vec::new();
    for (label, name) in [
        ("fd", "fd-effects-test.c"),
        ("stream-copy-v5", "stream-copy-v5-driver-test.c"),
        ("stream-membership-v5", "stream-membership-v5-test.c"),
    ] {
        let executable = output.join(format!("ftrace-production-{label}"));
        let mut compile = Command::new("clang");
        compile.args(["-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG", "-DAP_FTRACE_PROVIDER=1"]);
        if name == "fd-effects-test.c" {
            compile.args(["-DAP_NATIVE_COPY_VERSION=5ULL", "-DAP_FD_SESSION_DISPATCH_EMBEDDED=1"]);
        }
        compile.arg("-I").arg(source).arg(source.join(name));
        if name == "fd-effects-test.c" { compile.arg(source.join("fd-session-dispatch-test.c")); }
        compile.arg("-o").arg(&executable);
        let compiled = push_postwire(&mut stages, format!("compile:ftrace-production-{label}"),
            compile, true, None, false);
        push_postwire(&mut stages, format!("test:ftrace-production-{label}"),
            Command::new(&executable), false, Some(compiled), false);
    }
    let executable = output.join("ftrace-link-shape-mutant");
    let mut compile = Command::new("clang");
    compile.args(["-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG", "-DAP_FTRACE_MUTATE_LINK_SHAPE=1"])
        .arg("-I").arg(source).arg(source.join("ftrace-coverage-test.c")).arg("-o").arg(&executable);
    let compiled = push_postwire(&mut stages, "compile:ftrace-link-shape-mutant".into(),
        compile, true, None, false);
    push_postwire(&mut stages, "test:ftrace-link-shape-mutant".into(),
        Command::new(&executable), false, Some(compiled), true);

    let executable = output.join("grouped-source-fragment-overread-mutant");
    let mut compile = Command::new("clang");
    compile.args(["-O0", "-Wall", "-Wextra", "-Werror", "-UNDEBUG", "-DAP_FTRACE_PROVIDER=1",
        "-DAP_STREAM_COPY_MUTATE_FRAGMENT_LENGTH=1"])
        .arg("-I").arg(source).arg(source.join("stream-copy-v5-driver-test.c")).arg("-o").arg(&executable);
    let compiled = push_postwire(&mut stages, "compile:grouped-source-fragment-overread-mutant".into(),
        compile, true, None, false);
    push_postwire(&mut stages, "test:grouped-source-fragment-overread-mutant".into(),
        Command::new(&executable), false, Some(compiled), true);

    let baseline = output.join("stream-copy-fault-producer-test");
    for (label, selector) in [
        ("DROP_DATA", "fault"), ("DROP_PRIOR", "two-fragment"), ("CX", "fault"),
        ("SOURCE", "fault"), ("WINDOW", "fault"), ("FRONTIER", "fault"),
    ] {
        let executable = output.join(format!("failed-prefix-mutant-{label}"));
        let mut compile = Command::new("clang");
        compile.args(["-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG"])
            .arg(format!("-DAP_STREAM_FAULT_MUTATE_{label}=1"))
            .arg("-I").arg(source).arg(source.join("stream-copy-fault-producer-test.c"))
            .arg("-o").arg(&executable);
        let compiled = push_postwire(&mut stages, format!("compile:failed-prefix-{label}"),
            compile, true, None, false);
        let mut command = Command::new(&executable); command.arg(selector);
        let refused = push_postwire(&mut stages, format!("test:failed-prefix-{label}"),
            command, false, Some(compiled), true);
        // Preserve the ordinary unmutated SOURCE neighbor and exact full
        // comparator. No neighbor is admitted until its own negative passed.
        let mut neighbor = Command::new(if label == "SOURCE" { &baseline } else { &executable });
        neighbor.arg("full");
        push_postwire(&mut stages, format!("neighbor:failed-prefix-{label}"),
            neighbor, false, Some(refused), false);
    }
    for role in 12..=15 {
        let (selector, neighbor) = if role < 14 { ("linear", "fault") } else { ("fault", "linear") };
        let mut command = Command::new(&baseline);
        command.arg(selector).env("AP_FTRACE_MUTATE_ROLE", role.to_string());
        let refused = push_postwire(&mut stages, format!("test:failed-prefix-role-{role}"),
            command, false, None, true);
        let mut command = Command::new(&baseline);
        command.arg(neighbor).env("AP_FTRACE_MUTATE_ROLE", role.to_string());
        push_postwire(&mut stages, format!("neighbor:failed-prefix-role-{role}"),
            command, false, Some(refused), false);
    }
    for (label, source_name, roles) in [
        ("fd", "fd-effects-test.c", 1..=11),
        ("stream-copy", "stream-copy-v5-driver-test.c", 12..=15),
        ("stream-membership", "stream-membership-v5-test.c", 16..=17),
    ] {
        let executable = output.join(format!("ftrace-coverage-mutants-{label}"));
        let mut compile = Command::new("clang");
        compile.args(["-O0", "-Wall", "-Wextra", "-Werror", "-UNDEBUG",
            "-DAP_FTRACE_PROVIDER=1", "-DAP_FTRACE_RUNTIME_MUTANT=1"]);
        if source_name == "fd-effects-test.c" { compile.arg("-DAP_NATIVE_COPY_VERSION=5ULL"); }
        if source_name == "fd-effects-test.c" { compile.arg("-DAP_FTRACE_MUTANT_ONLY=1"); }
        compile.arg("-I").arg(source).arg(source.join(source_name)).arg("-o").arg(&executable);
        let compiled = push_postwire(&mut stages, format!("compile:ftrace-mutants-{label}"),
            compile, true, None, false);
        for role in roles {
            let mut command = Command::new(&executable);
            command.env("AP_FTRACE_MUTATE_ROLE", role.to_string());
            push_postwire(&mut stages, format!("test:ftrace-mutant-{role}"),
                command, false, Some(compiled), true);
        }
    }
    stages
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
    let accepted = manifest["kind"] == "hermit-accepted-provider";
    let control_hashes = control_sources(&source, accepted)?;
    let channel_hashes = channel_control_inputs(&source, accepted)?;
    fs::create_dir(&output).context("test output must be a new directory")?;
    fs::write(
        output.join("inputs.json"),
        serde_json::to_vec_pretty(&json!({
            "manifest_sha256":digest(&manifest_bytes), "source":source, "sources":source_hashes,
            "required_c_controls":control_hashes, "channel_control_inputs":channel_hashes,
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
        .env("DAGRUN_LOG_DIR", &output);
    File::create(&stdout_path)?;
    File::create(&stderr_path)?;
    let started = Instant::now();
    let (parser, initial) = if accepted {
        let (wire, independent) = ACCEPTED_CONTROLS.split_last().context("missing controls")?;
        ensure!(*wire == "grouped-wire-test.c" && independent.len() == 23,
            "initial control partition changed");
        overlap_initial_gate(&AtomicBool::new(false), 0..=22,
            || execute_logged_stage(&mut command, started, &stdout_path, &stderr_path),
            |index| accepted_control_sequence(independent[index as usize], &source, &output,
                started, &stdout_path, &stderr_path))
    } else {
        (execute_logged_stage(&mut command, started, &stdout_path, &stderr_path), Vec::new())
    };
    let (mut receipt, mut stages) = initial_gate_receipts(parser, initial);
    if receipt["passed"] == true && accepted {
        // Both independent branches have joined before this fork/SCM leaf.
        append_control_receipts(&mut stages, &mut receipt,
            accepted_control_sequence("grouped-wire-test.c", &source, &output,
                started, &stdout_path, &stderr_path));
    }
    if receipt["passed"] == true && !accepted {
        for (name, wraps) in UNIX_CONTROLS {
            let executable = output.join(name.strip_suffix(".c").unwrap());
            let mut compile = Command::new("clang");
            compile.args([
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-UNDEBUG",
                "-ffunction-sections",
                "-fdata-sections",
                "-Wl,--gc-sections",
            ]);
            for wrapped in *wraps {
                compile.arg(format!("-Wl,--wrap={wrapped}"));
            }
            compile
                .arg("-I")
                .arg(source.join("unix"))
                .arg(source.join("unix").join(name))
                .arg("-o")
                .arg(&executable);
            let compiled = execute_logged_stage(&mut compile, started, &stdout_path, &stderr_path);
            stages.push(json!({"name":format!("compile:{name}"),"receipt":compiled}));
            if compiled["passed"] != true {
                receipt = compiled;
                break;
            }
            let tested = execute_logged_stage(
                &mut Command::new(&executable),
                started,
                &stdout_path,
                &stderr_path,
            );
            stages.push(json!({"name":format!("test:{name}"),"receipt":tested}));
            if tested["passed"] != true {
                receipt = tested;
                break;
            }
        }
    }
    if receipt["passed"] == true && accepted {
        let plan = postwire_plan(&source, &output);
        ensure!(plan.len() == 56, "post-wire stage population changed");
        let completed = postwire_schedule(plan, |_, command| {
            execute_logged_stage(command, started, &stdout_path, &stderr_path)
        })?;
        append_postwire_receipts(&mut stages, &mut receipt, completed);
    }
    let expected_stages = expected_stage_count(accepted);
    let complete = stages_complete(&stages, expected_stages);
    receipt["passed"] = json!(complete);
    receipt["stages"] = json!(stages);
    receipt["expected_stages"] = json!(expected_stages);
    receipt["seconds"] = json!(started.elapsed().as_secs_f64());
    let fence = (|| -> Result<bool> {
        Ok(before_hash == digest(&read(&before, 16 * MIB)?)
            && after_hash == digest(&read(&after, MIB)?)
            && manifest_bytes == read(&package.join("manifest.json"), 32768)?
            && source_hashes == sources(&source, &manifest)?
            && control_hashes == control_sources(&source, accepted)?
            && channel_hashes == channel_control_inputs(&source, accepted)?)
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
        "package parser/provider test action failed; raw evidence in {}",
        output.display()
    );
    println!("{}", json!({"passed":true,"evidence":output}));
    Ok(())
}

fn main() {
    rust_script_prelude::init();
    if let Err(error) = run() {
        eprintln!("network provider tests failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "role_mutant_batch_tests.rs"]
mod role_mutant_batch_tests;

#[cfg(test)]
mod initial_gate_batch_tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Barrier, mpsc};
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn logs() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("initial-gate-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        let stdout = root.join("stdout");
        let stderr = root.join("stderr");
        File::create_new(&stdout).unwrap();
        File::create_new(&stderr).unwrap();
        (root, stdout, stderr)
    }

    fn events(stderr: &Path) -> Vec<Value> {
        fs::read_to_string(stderr).unwrap().lines().map(|line| {
            serde_json::from_str(line.strip_prefix("PROVIDER_STAGE ").unwrap()).unwrap()
        }).collect()
    }

    fn reaped(receipt: &Value) {
        assert_eq!(receipt["cleanup_complete"], true);
        assert_eq!(receipt["final_group_absent"], true);
        assert_eq!(receipt["cleanup_errors"], json!([]));
        let pid = receipt["pid"].as_u64().unwrap() as libc::id_t;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    fn initial_partition_keeps_all_controls_and_serial_wire() {
        assert_eq!(INITIAL_CONTROL_CONCURRENCY, 2);
        let (wire, independent) = ACCEPTED_CONTROLS.split_last().unwrap();
        assert_eq!(*wire, "grouped-wire-test.c");
        assert_eq!(independent.len(), 23);
        assert!(!independent.contains(&"grouped-wire-test.c"));
        assert_eq!(expected_stage_count(true), 111);
    }

    #[test]
    fn two_workers_overlap_but_receipts_follow_input_order() {
        // Explicitly modeled work, not compiler or native evidence.
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let barrier = Barrier::new(2);
        let channels: Vec<_> = (0..2).map(|_| {
            let (tx, rx) = mpsc::channel(); (tx, Mutex::new(rx))
        }).collect();
        let completion = Mutex::new(Vec::new());
        let results = bounded_batches(0..=3, INITIAL_CONTROL_CONCURRENCY, |index| {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            maximum.fetch_max(count, Ordering::SeqCst);
            barrier.wait();
            let (tx, rx) = &channels[index as usize / 2];
            if index % 2 == 0 { rx.lock().unwrap().recv_timeout(Duration::from_secs(1)).unwrap(); }
            completion.lock().unwrap().push(index);
            if index % 2 == 1 { tx.send(()).unwrap(); }
            active.fetch_sub(1, Ordering::SeqCst);
            json!({"passed":true})
        });
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(*completion.lock().unwrap(), [1, 0, 3, 2]);
        assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1, 2, 3]);
    }

    #[test]
    fn failed_batch_joins_neighbor_and_cancels_unlaunched_batches() {
        let joined = AtomicUsize::new(0);
        let results = bounded_batches(0..=5, INITIAL_CONTROL_CONCURRENCY, |index| {
            joined.fetch_add(1, Ordering::SeqCst);
            json!({"passed":index != 0,"receipt":{"passed":index != 0,"original":index},
                "stages":[{"name":format!("fixture:{index}"),"receipt":{"passed":index != 0}}]})
        });
        assert_eq!(joined.load(Ordering::SeqCst), 2);
        assert_eq!(results.len(), 2);
        let mut stages = Vec::new(); let mut receipt = json!({"passed":true});
        for (_, result) in results { append_control_receipts(&mut stages, &mut receipt, result); }
        assert_eq!(stages.len(), 2);
        assert_eq!(receipt, json!({"passed":false,"original":0}));
    }

    #[test]
    fn panicked_worker_does_not_abandon_admitted_neighbor() {
        let barrier = Barrier::new(2); let finished = AtomicUsize::new(0);
        let results = bounded_batches(0..=3, INITIAL_CONTROL_CONCURRENCY, |index| {
            barrier.wait();
            if index == 0 { panic!("intentional modeled worker failure"); }
            finished.fetch_add(1, Ordering::SeqCst);
            json!({"passed":true})
        });
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1["passed"], false);
        assert_eq!(finished.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn actual_child_has_durable_start_completion_and_original_reap() {
        let (_, stdout, stderr) = logs();
        let receipt = execute_logged_stage(Command::new("/bin/sh").args(["-c", "printf child"]),
            Instant::now(), &stdout, &stderr);
        assert_eq!(receipt["passed"], true); reaped(&receipt);
        assert_eq!(fs::read(&stdout).unwrap(), b"child");
        let records = events(&stderr);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "start"); assert_eq!(records[1]["event"], "complete");
        assert_eq!(records[0]["sequence"], records[1]["sequence"]);
        assert_eq!(records[1]["receipt"], receipt);
    }

    #[test]
    fn actual_nonzero_child_stays_failed_and_reaped() {
        let (_, stdout, stderr) = logs();
        let receipt = execute_logged_stage(Command::new("/bin/sh").args(["-c", "exit 7"]),
            Instant::now(), &stdout, &stderr);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7); reaped(&receipt);
        assert_eq!(events(&stderr)[1]["receipt"]["raw_status"], 7);
    }

    #[test]
    fn actual_two_child_failure_joins_both_and_stops_later_admission() {
        let (_, stdout, stderr) = logs(); let started = Instant::now();
        let results = bounded_batches(0..=3, INITIAL_CONTROL_CONCURRENCY, |index| {
            let raw = execute_logged_stage(Command::new("/bin/sh")
                .args(["-c", if index == 0 { "exit 7" } else { "exit 0" }]),
                started, &stdout, &stderr);
            json!({"passed":raw["passed"],"receipt":raw,"stages":[]})
        });
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].1["receipt"]["raw_status"], 7);
        assert_eq!(results[1].1["receipt"]["raw_status"], 0);
        assert_ne!(results[0].1["receipt"]["pid"], results[1].1["receipt"]["pid"]);
        for (_, value) in &results { reaped(&value["receipt"]); }
        let records = events(&stderr); assert_eq!(records.len(), 4);
        let mut stages = Vec::new(); let mut receipt = json!({"passed":true});
        for (_, value) in results { append_control_receipts(&mut stages, &mut receipt, value); }
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7);
    }

    #[test]
    fn missing_log_refuses_before_spawn() {
        let (root, stdout, _) = logs(); let marker = root.join("must-not-exist");
        let receipt = execute_logged_stage(Command::new("/usr/bin/touch").arg(&marker),
            Instant::now(), &stdout, &root.join("missing"));
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["pid"], Value::Null);
        assert!(!marker.exists());
    }

    #[test]
    fn diagnostic_bytes_use_existing_combined_four_mib_cap() {
        let (root, stdout, stderr) = logs(); let marker = root.join("must-not-exist");
        File::options().write(true).open(&stdout).unwrap().set_len(driver_ftrace_process::LIMITS.logs).unwrap();
        let receipt = execute_logged_stage(Command::new("/usr/bin/touch").arg(&marker),
            Instant::now(), &stdout, &stderr);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["pid"], Value::Null);
        assert!(!marker.exists()); assert_eq!(fs::metadata(&stderr).unwrap().len(), 0);
    }

    #[test]
    fn original_expired_deadline_refuses_before_spawn() {
        let (root, stdout, stderr) = logs(); let marker = root.join("must-not-exist");
        let started = Instant::now() - driver_ftrace_process::LIMITS.wall;
        let receipt = execute_logged_stage(Command::new("/usr/bin/touch").arg(&marker), started, &stdout, &stderr);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["pid"], Value::Null);
        assert!(!marker.exists());
    }

    #[test]
    fn visible_complete_record_with_persist_failure_cannot_pass() {
        let (_, stdout, stderr) = logs(); let started = Instant::now(); let mut writes = 0;
        let receipt = execute_logged_stage_with(Command::new("/bin/sh").args(["-c", "exit 0"]),
            started, &stdout, &stderr, |event| {
                writes += 1;
                append_stage_diagnostic_with(event, started, &stdout, &stderr, |file| {
                    if writes == 2 { Err(std::io::Error::from_raw_os_error(libc::EIO)) }
                    else { file.sync_all() }
                })
            });
        assert_eq!(writes, 2); assert_eq!(receipt["passed"], false);
        assert_eq!(receipt["raw_status"], 0); reaped(&receipt);
        let records = events(&stderr); assert_eq!(records.len(), 2);
        assert_eq!(records[1]["receipt"]["passed"], true, "visible bytes do not authenticate persistence");
        assert!(!receipt["terminal_bounds_error"].is_null());
    }

    #[test]
    fn persistence_cannot_refresh_original_deadline() {
        let (_, stdout, stderr) = logs();
        let started = Instant::now() - (driver_ftrace_process::LIMITS.wall - Duration::from_secs(1));
        let result = append_stage_diagnostic_with(&json!({"late":true}), started, &stdout, &stderr, |file| {
            file.sync_all()?;
            std::thread::sleep(Duration::from_millis(1100));
            Ok(())
        });
        assert!(result.is_err()); assert_eq!(events(&stderr), [json!({"late":true})]);
    }

    #[test]
    fn close_failure_refuses_even_after_visible_durable_bytes() {
        let (_, stdout, stderr) = logs();
        let result = append_stage_diagnostic_ops(&json!({"close_error":true}), Instant::now(),
            &stdout, &stderr, File::sync_all, |fd| {
                assert_eq!(unsafe { libc::close(fd) }, 0);
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            });
        assert!(result.is_err());
        assert_eq!(events(&stderr), [json!({"close_error":true})]);
    }

    #[test]
    fn diagnostic_failure_is_not_an_accepted_abort_mutant() {
        // Outcome-only model; actual child/persistence paths are covered above.
        let raw = json!({"passed":false,"signal":libc::SIGABRT,"timed_out":false,
            "log_overflow":false,"primary_error":null,"terminal_bounds_error":null,
            "natural_terminal_group":true,"cleanup_complete":true,"cleanup_errors":[]});
        assert_eq!(role_mutant_receipt(raw.clone())["passed"], true);
        assert_eq!(role_mutant_receipt(diagnostic_failure(raw, "fsync".into()))["passed"], false);
    }
}

#[cfg(test)]
mod parser_overlap_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn logs() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("parser-overlap-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        let stdout = root.join("stdout");
        let stderr = root.join("stderr");
        File::create_new(&stdout).unwrap();
        File::create_new(&stderr).unwrap();
        (root, stdout, stderr)
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < until, "host-control handshake timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn reaped(receipt: &Value) {
        assert_eq!(receipt["cleanup_complete"], true);
        assert_eq!(receipt["final_group_absent"], true);
        assert_eq!(receipt["cleanup_errors"], json!([]));
        let pid = receipt["pid"].as_u64().unwrap() as libc::id_t;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    fn actual_child(root: &Path, name: &str, status: i32, started: Instant,
        stdout: &Path, stderr: &Path) -> Value {
        // Three independently supervised real children publish their own PID
        // and cannot exit until all three are physically alive. These files
        // synchronize this host test only; they convey no wait/kill authority.
        let script = r#"
import os, pathlib, sys, time
root, name, code = pathlib.Path(sys.argv[1]), sys.argv[2], int(sys.argv[3])
(root / (name + '.ready')).write_text(str(os.getpid()))
until = time.monotonic() + 5
while not all((root / (n + '.ready')).is_file() for n in ('parser', 'c0', 'c1')):
    if time.monotonic() >= until:
        raise SystemExit(97)
    time.sleep(0.001)
raise SystemExit(code)
"#;
        let receipt = execute_logged_stage(Command::new("/usr/bin/python3")
            .arg("-c").arg(script).arg(root).arg(name).arg(status.to_string()),
            started, stdout, stderr);
        reaped(&receipt);
        assert_eq!(receipt["raw_status"], status);
        assert_eq!(fs::read_to_string(root.join(format!("{name}.ready"))).unwrap(),
            receipt["pid"].as_u64().unwrap().to_string());
        receipt
    }

    fn control(index: u32, raw: Value) -> Value {
        json!({"passed":raw["passed"],"receipt":raw,
            "stages":[{"name":format!("host-control:{index}"),"receipt":raw}]})
    }

    fn assert_children_joined(parser: &Value, controls: &[(u32, Value)]) {
        reaped(parser);
        let mut pids = vec![parser["pid"].as_u64().unwrap()];
        for (_, completed) in controls {
            reaped(&completed["receipt"]);
            pids.push(completed["receipt"]["pid"].as_u64().unwrap());
        }
        let count = pids.len(); pids.sort_unstable(); pids.dedup();
        assert_eq!(pids.len(), count, "each original child has distinct custody");
    }

    #[test]
    fn actual_parser_and_two_c_children_overlap_and_retain_canonical_order() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false); let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=1,
            || actual_child(&root, "parser", 0, started, &stdout, &stderr),
            |index| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                let raw = actual_child(&root, &format!("c{index}"), 0, started, &stdout, &stderr);
                active.fetch_sub(1, Ordering::SeqCst);
                control(index, raw)
            });
        assert_children_joined(&parser, &controls);
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(!stop.load(Ordering::Acquire));
        let (receipt, stages) = initial_gate_receipts(parser, controls);
        assert_eq!(receipt["passed"], true);
        assert_eq!(stages.iter().map(|r| r["name"].as_str().unwrap()).collect::<Vec<_>>(),
            ["compression-parser", "host-control:0", "host-control:1"]);
        assert!(stages.iter().all(|r| r["receipt"]["passed"] == true));
    }

    #[test]
    fn actual_parser_failure_cancels_unlaunched_c_batches_after_full_join() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || actual_child(&root, "parser", 7, started, &stdout, &stderr),
            |index| {
                let raw = actual_child(&root, &format!("c{index}"), 0, started, &stdout, &stderr);
                // Demand observed production cancellation before returning the
                // first C batch, making the no-next-batch assertion causal.
                wait_until(|| stop.load(Ordering::Acquire));
                control(index, raw)
            });
        assert_children_joined(&parser, &controls);
        assert_eq!(controls.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1]);
        assert!(!root.join("c2.ready").exists()); assert!(!root.join("c3.ready").exists());
        let (receipt, stages) = initial_gate_receipts(parser, controls);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7);
        assert_eq!(stages.len(), 3); assert_eq!(stages[0]["receipt"]["raw_status"], 7);
    }

    #[test]
    fn actual_c_failure_joins_parser_and_retains_first_canonical_failure() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false); let later_complete = AtomicBool::new(false);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || actual_child(&root, "parser", 0, started, &stdout, &stderr),
            |index| {
                let raw = actual_child(&root, &format!("c{index}"), if index == 0 { 7 } else { 9 },
                    started, &stdout, &stderr);
                if index == 0 { wait_until(|| later_complete.load(Ordering::Acquire)); }
                else { later_complete.store(true, Ordering::Release); }
                control(index, raw)
            });
        assert_children_joined(&parser, &controls);
        assert_eq!(controls.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1]);
        let (receipt, stages) = initial_gate_receipts(parser, controls);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7);
        assert_eq!(stages[0]["receipt"]["raw_status"], 0);
        assert_eq!(stages[2]["receipt"]["raw_status"], 9);
    }

    #[test]
    fn actual_late_parser_failure_has_canonical_priority_over_c_failure() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || {
                let raw = actual_child(&root, "parser", 7, started, &stdout, &stderr);
                wait_until(|| stop.load(Ordering::Acquire));
                raw
            },
            |index| control(index, actual_child(&root, &format!("c{index}"),
                if index == 0 { 9 } else { 0 }, started, &stdout, &stderr)));
        assert_children_joined(&parser, &controls);
        let (receipt, stages) = initial_gate_receipts(parser, controls);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7);
        assert_eq!(stages[1]["receipt"]["raw_status"], 9);
    }

    #[test]
    fn parser_panic_drops_real_live_owner_and_joins_other_original_children() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false); let pid = AtomicU64::new(0);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || {
                // Actual retained owner on an unwind path, not invented
                // cleanup=true data. Its unchanged Drop is a failed-action
                // backstop and does not manufacture a successful receipt.
                process_group::own_descendants().unwrap();
                let mut command = Command::new("/bin/sh");
                command.args(["-c", "exec sleep 30"]).stdin(Stdio::null())
                    .stdout(File::options().append(true).open(&stdout).unwrap())
                    .stderr(File::options().append(true).open(&stderr).unwrap());
                unsafe { command.pre_exec(|| {
                    if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
                }); }
                let owner = process_group::OwnedChild { child:command.spawn().unwrap(),
                    cleanup_attempted:false,reaped:false };
                pid.store(owner.child.id() as u64, Ordering::Release);
                fs::write(root.join("parser.ready"), owner.child.id().to_string()).unwrap();
                wait_until(|| root.join("c0.ready").exists() && root.join("c1.ready").exists());
                panic!("intentional live-owner parser panic");
            },
            |index| {
                let raw = actual_child(&root, &format!("c{index}"), 0, started, &stdout, &stderr);
                wait_until(|| stop.load(Ordering::Acquire));
                control(index, raw)
            });
        assert_eq!(parser["passed"], false); assert!(parser["worker_error"].as_str().unwrap().contains("parser panic"));
        assert!(parser["cleanup_complete"].is_null(), "panic is not a successful cleanup receipt");
        assert_eq!(controls.len(), 2);
        for (_, value) in &controls { reaped(&value["receipt"]); }
        let pid = pid.load(Ordering::Acquire) as libc::id_t; assert_ne!(pid, 0);
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        assert!(process_group::absent(pid).unwrap());
        assert_eq!(initial_gate_receipts(parser, controls).0["passed"], false);
    }

    #[test]
    fn c_panic_after_real_reap_does_not_discard_parser_or_peer_outcomes() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stop = AtomicBool::new(false); let retained = Mutex::new(None);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || actual_child(&root, "parser", 0, started, &stdout, &stderr),
            |index| {
                let raw = actual_child(&root, &format!("c{index}"), 0, started, &stdout, &stderr);
                if index == 0 {
                    *retained.lock().unwrap() = Some(raw);
                    panic!("intentional C branch panic after original reap");
                }
                control(index, raw)
            });
        reaped(&parser); reaped(retained.lock().unwrap().as_ref().unwrap());
        assert_eq!(controls.len(), 2); assert_eq!(controls[0].1["passed"], false);
        reaped(&controls[1].1["receipt"]);
        let (receipt, stages) = initial_gate_receipts(parser, controls);
        assert_eq!(receipt["passed"], false); assert!(receipt["worker_error"].is_string());
        assert_eq!(stages[0]["receipt"]["passed"], true);
    }

    #[test]
    fn both_branches_retain_the_original_expired_start_without_spawning() {
        let (root, stdout, stderr) = logs();
        let started = Instant::now() - driver_ftrace_process::LIMITS.wall;
        let stop = AtomicBool::new(false);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || execute_logged_stage(Command::new("/usr/bin/touch").arg(root.join("parser-child")),
                started, &stdout, &stderr),
            |index| control(index, execute_logged_stage(Command::new("/usr/bin/touch")
                .arg(root.join(format!("c{index}-child"))), started, &stdout, &stderr)));
        assert_eq!(parser["passed"], false); assert!(parser["pid"].is_null());
        for (_, row) in &controls { assert!(row["receipt"]["pid"].is_null()); }
        assert!(!root.join("parser-child").exists());
        for i in 0..4 { assert!(!root.join(format!("c{i}-child")).exists()); }
        assert_eq!(initial_gate_receipts(parser, controls).0["passed"], false);
    }

    #[test]
    fn both_branches_share_the_original_combined_log_cap() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        File::options().write(true).open(&stdout).unwrap().set_len(driver_ftrace_process::LIMITS.logs).unwrap();
        let stop = AtomicBool::new(false);
        let (parser, controls) = overlap_initial_gate(&stop, 0..=3,
            || execute_logged_stage(Command::new("/usr/bin/touch").arg(root.join("parser-child")),
                started, &stdout, &stderr),
            |index| control(index, execute_logged_stage(Command::new("/usr/bin/touch")
                .arg(root.join(format!("c{index}-child"))), started, &stdout, &stderr)));
        assert_eq!(parser["passed"], false); assert!(parser["pid"].is_null());
        for (_, row) in &controls { assert!(row["receipt"]["pid"].is_null()); }
        assert_eq!(fs::metadata(&stderr).unwrap().len(), 0);
        assert!(!root.join("parser-child").exists());
        for i in 0..4 { assert!(!root.join(format!("c{i}-child")).exists()); }
        assert_eq!(initial_gate_receipts(parser, controls).0["passed"], false);
    }
}

#[cfg(test)]
mod postwire_dag_tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn logs() -> (PathBuf, PathBuf, PathBuf) {
        let root = std::env::var_os("HERMIT_TEST_ACTION_RESULTS")
            .map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
            .join(format!("postwire-dag-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir(&root).unwrap();
        let stdout = root.join("stdout"); let stderr = root.join("stderr");
        File::create_new(&stdout).unwrap(); File::create_new(&stderr).unwrap();
        (root, stdout, stderr)
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < until, "ordinary-child rendezvous expired");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn reaped(raw: &Value) {
        assert_eq!(raw["cleanup_complete"], true);
        assert_eq!(raw["final_group_absent"], true);
        assert_eq!(raw["cleanup_errors"], json!([]));
        let pid = raw["pid"].as_u64().unwrap() as libc::id_t;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    fn child(root: &Path, index: usize, status: i32, bytes: usize) -> Command {
        // Exact directly executed interpreter, not the PATH launcher. Four
        // physical children must all exist before any can finish. Markers are
        // host-test synchronization only, never ownership/kill authority.
        let script = r#"
import os, pathlib, sys, time
root, index, status, count = pathlib.Path(sys.argv[1]), sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
(root / (index + '.ready')).write_text(str(os.getpid()))
until = time.monotonic() + 5
while len(list(root.glob('*.ready'))) < 4:
    if time.monotonic() >= until:
        raise SystemExit(97)
    time.sleep(0.001)
if count:
    data = b'x' * count
    while data:
        data = data[os.write(1, data):]
raise SystemExit(status)
"#;
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-c").arg(script).arg(root).arg(index.to_string())
            .arg(status.to_string()).arg(bytes.to_string());
        command
    }

    fn physical_plan(root: &Path, statuses: &[i32], compilers: usize, bytes: usize) -> Vec<PostwireStage> {
        statuses.iter().enumerate().map(|(index, &status)| PostwireStage {
            name: format!("physical:{index}"), command: child(root, index, status, bytes),
            compiler: index < compilers, prerequisite: None, expected_abort: false,
        }).collect()
    }

    fn assert_physical(root: &Path, stages: &[Value]) {
        let mut pids = Vec::new();
        for stage in stages {
            let raw = &stage["receipt"]; reaped(raw);
            let index = stage["name"].as_str().unwrap().strip_prefix("physical:").unwrap();
            assert_eq!(fs::read_to_string(root.join(format!("{index}.ready"))).unwrap(),
                raw["pid"].as_u64().unwrap().to_string());
            pids.push(raw["pid"].as_u64().unwrap());
        }
        let count = pids.len(); pids.sort_unstable(); pids.dedup();
        assert_eq!(pids.len(), count, "no cross-owner PID/reap substitution");
    }

    #[test]
    fn exact_production_population_paths_and_dependency_edges() {
        let plan = postwire_plan(Path::new("/source"), Path::new("/output"));
        assert_eq!(plan.len(), 56);
        assert_eq!(plan.iter().filter(|s| s.compiler).count(), 14);
        assert_eq!(plan.iter().filter(|s| s.expected_abort).count(), 29);
        assert_eq!(plan.iter().filter(|s| s.prerequisite.is_none()).count(), 18);
        let mut names = Vec::new();
        for label in ["fd", "stream-copy-v5", "stream-membership-v5"] {
            names.push(format!("compile:ftrace-production-{label}"));
            names.push(format!("test:ftrace-production-{label}"));
        }
        for label in ["ftrace-link-shape-mutant", "grouped-source-fragment-overread-mutant"] {
            names.push(format!("compile:{label}")); names.push(format!("test:{label}"));
        }
        for label in ["DROP_DATA", "DROP_PRIOR", "CX", "SOURCE", "WINDOW", "FRONTIER"] {
            for kind in ["compile", "test", "neighbor"] { names.push(format!("{kind}:failed-prefix-{label}")); }
        }
        for role in 12..=15 {
            names.push(format!("test:failed-prefix-role-{role}"));
            names.push(format!("neighbor:failed-prefix-role-{role}"));
        }
        for (label, roles) in [("fd", 1..=11), ("stream-copy", 12..=15), ("stream-membership", 16..=17)] {
            names.push(format!("compile:ftrace-mutants-{label}"));
            for role in roles { names.push(format!("test:ftrace-mutant-{role}")); }
        }
        assert_eq!(plan.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), names);
        // Independently derived from all 56 actual 1027 post-wire start
        // records, normalized only for source/output roots and restored to
        // canonical old order. The timed-out run is NOT passing evidence.
        // Require the exact additive dispatcher TU/define below, then retain
        // the old whole-plan oracle after removing only those two arguments.
        let descriptors: Vec<Value> = plan.iter().map(|node| {
            assert!(node.command.get_current_dir().is_none());
            json!([node.command.get_program().to_str().unwrap(),
                node.command.get_args().map(|arg| arg.to_str().unwrap()).collect::<Vec<_>>(),
                node.command.get_envs().map(|(key, value)|
                    [key.to_str().unwrap(), value.unwrap().to_str().unwrap()]).collect::<Vec<_>>()])
        }).collect();
        assert_eq!(descriptors[0], json!(["clang", [
            "-O2", "-Wall", "-Wextra", "-Werror", "-UNDEBUG", "-DAP_FTRACE_PROVIDER=1",
            "-DAP_NATIVE_COPY_VERSION=5ULL", "-DAP_FD_SESSION_DISPATCH_EMBEDDED=1",
            "-I", "/source", "/source/fd-effects-test.c", "/source/fd-session-dispatch-test.c",
            "-o", "/output/ftrace-production-fd"
        ], []]));
        let mut historical_descriptors = descriptors.clone();
        let args = historical_descriptors[0][1].as_array_mut().unwrap();
        assert_eq!(args.remove(11), json!("/source/fd-session-dispatch-test.c"));
        assert_eq!(args.remove(7), json!("-DAP_FD_SESSION_DISPATCH_EMBEDDED=1"));
        assert_eq!(digest(&serde_json::to_vec(&historical_descriptors).unwrap()),
            "19061778cd36b86f313a851fb8fd5adcafdb468a242df59a3e4795af5aa07e6e");
        let mut outputs = Vec::new();
        for (index, node) in plan.iter().enumerate() {
            assert!(node.prerequisite.is_none_or(|dep| dep < index));
            if node.compiler {
                assert_eq!(node.command.get_program(), "clang");
                let args: Vec<_> = node.command.get_args().collect();
                let output = args[args.iter().position(|arg| *arg == "-o").unwrap() + 1];
                assert!(Path::new(output).starts_with("/output"));
                outputs.push(output.to_owned());
                for required in ["-Wall", "-Wextra", "-Werror", "-UNDEBUG"] { assert!(args.iter().any(|arg| *arg == required)); }
            } else if let Some(dep) = node.prerequisite {
                let prerequisite = &plan[dep];
                assert!(prerequisite.compiler || prerequisite.expected_abort);
            } else {
                assert!(node.name.starts_with("test:failed-prefix-role-"));
                assert_eq!(node.command.get_program(), "/output/stream-copy-fault-producer-test");
            }
        }
        outputs.sort(); outputs.dedup(); assert_eq!(outputs.len(), 14);
        let source_neighbor = plan.iter().find(|s| s.name == "neighbor:failed-prefix-SOURCE").unwrap();
        assert_eq!(source_neighbor.command.get_program(), "/output/stream-copy-fault-producer-test");
        assert_eq!(source_neighbor.command.get_args().map(|arg| arg.to_str().unwrap()).collect::<Vec<_>>(), ["full"]);
        // The new TX compile/run pair precedes the unchanged postwire DAG.
        assert_eq!(expected_stage_count(true), 55 + plan.len());
    }

    #[test]
    fn actual_four_children_obey_two_compiler_slots_and_canonical_order() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let active = AtomicUsize::new(0); let maximum = AtomicUsize::new(0);
        let compilers = AtomicUsize::new(0); let compiler_maximum = AtomicUsize::new(0);
        // Ordinary children exercise the real admission slots and supervisors;
        // these are not compiler-performance or provider-native observations.
        let stages = postwire_schedule(physical_plan(&root, &[0; 6], 3, 0), |index, command| {
            maximum.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            if index < 3 { compiler_maximum.fetch_max(compilers.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst); }
            let raw = execute_logged_stage(command, started, &stdout, &stderr);
            if index < 3 { compilers.fetch_sub(1, Ordering::SeqCst); }
            active.fetch_sub(1, Ordering::SeqCst); raw
        }).unwrap();
        assert_physical(&root, &stages); // All real reaps precede omission assertions.
        assert_eq!(stages.len(), 6); assert!(stages.iter().all(|s| s["receipt"]["passed"] == true));
        assert_eq!(maximum.load(Ordering::SeqCst), 4);
        assert_eq!(compiler_maximum.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0); assert_eq!(compilers.load(Ordering::SeqCst), 0);
        assert_eq!(stages.iter().map(|s| s["name"].as_str().unwrap()).collect::<Vec<_>>(),
            ["physical:0", "physical:1", "physical:2", "physical:3", "physical:4", "physical:5"]);
    }

    #[test]
    fn actual_late_first_failure_joins_all_and_cancels_unlaunched_nodes() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let later_failed = AtomicBool::new(false);
        let stages = postwire_schedule(physical_plan(&root, &[7, 9, 0, 0, 0, 0], 2, 0), |index, command| {
            let raw = execute_logged_stage(command, started, &stdout, &stderr);
            if index == 0 { wait_until(|| later_failed.load(Ordering::Acquire)); }
            if index == 1 { later_failed.store(true, Ordering::Release); }
            raw
        }).unwrap();
        assert_physical(&root, &stages);
        assert_eq!(stages.len(), 4);
        assert!(!root.join("4.ready").exists()); assert!(!root.join("5.ready").exists());
        let mut all = Vec::new(); let mut receipt = json!({"passed":true});
        append_postwire_receipts(&mut all, &mut receipt, stages);
        assert_eq!(receipt["passed"], false); assert_eq!(receipt["raw_status"], 7);
        assert_eq!(all[1]["receipt"]["raw_status"], 9);
        assert_eq!(all[3]["receipt"]["raw_status"], 0);
    }

    #[test]
    fn actual_four_child_positive_neighbor() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stages = postwire_schedule(physical_plan(&root, &[0; 4], 2, 0), |_, command|
            execute_logged_stage(command, started, &stdout, &stderr)).unwrap();
        assert_physical(&root, &stages); assert_eq!(stages.len(), 4);
        let mut all = Vec::new(); let mut receipt = json!({"passed":true});
        append_postwire_receipts(&mut all, &mut receipt, stages);
        assert_eq!(receipt["passed"], true); assert_eq!(all.len(), 4);
    }

    #[test]
    fn actual_expected_abort_unlocks_only_its_reaped_ordinary_neighbor() {
        let (_, stdout, stderr) = logs(); let started = Instant::now();
        let mut plan = Vec::new();
        let mut negative = Command::new("/bin/sh"); negative.args(["-c", "kill -ABRT $$"]);
        let dependency = push_postwire(&mut plan, "negative".into(), negative, false, None, true);
        let mut neighbor = Command::new("/bin/sh"); neighbor.args(["-c", "exit 0"]);
        push_postwire(&mut plan, "neighbor".into(), neighbor, false, Some(dependency), false);
        let stages = postwire_schedule(plan, |_, command|
            execute_logged_stage(command, started, &stdout, &stderr)).unwrap();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0]["receipt"]["passed"], true);
        assert_eq!(stages[0]["receipt"]["raw"]["passed"], false);
        assert_eq!(stages[0]["receipt"]["raw"]["signal"], libc::SIGABRT);
        reaped(&stages[0]["receipt"]["raw"]);
        assert_eq!(stages[1]["receipt"]["passed"], true);
        reaped(&stages[1]["receipt"]);
        assert_ne!(stages[0]["receipt"]["raw"]["pid"], stages[1]["receipt"]["pid"]);
    }

    #[test]
    fn actual_live_owner_panic_is_failed_and_all_original_peers_are_joined() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let pid = AtomicU64::new(0);
        let stages = postwire_schedule(physical_plan(&root, &[0; 6], 2, 0), |index, command| {
            if index != 0 { return execute_logged_stage(command, started, &stdout, &stderr); }
            process_group::own_descendants().unwrap();
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exec sleep 30"]).stdin(Stdio::null())
                .stdout(File::options().append(true).open(&stdout).unwrap())
                .stderr(File::options().append(true).open(&stderr).unwrap());
            unsafe { command.pre_exec(|| {
                if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            }); }
            let owner = process_group::OwnedChild { child: command.spawn().unwrap(), cleanup_attempted:false, reaped:false };
            pid.store(owner.child.id() as u64, Ordering::Release);
            fs::write(root.join("0.ready"), owner.child.id().to_string()).unwrap();
            wait_until(|| (1..=3).all(|i| root.join(format!("{i}.ready")).exists()));
            panic!("intentional post-wire live-owner panic");
        }).unwrap();
        assert_eq!(stages.len(), 4);
        assert_eq!(stages[0]["receipt"]["passed"], false);
        assert!(stages[0]["receipt"]["cleanup_complete"].is_null(), "unwind is not successful cleanup evidence");
        assert_physical(&root, &stages[1..]);
        let pid = pid.load(Ordering::Acquire) as libc::id_t; assert_ne!(pid, 0);
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        assert_eq!(unsafe { libc::waitid(libc::P_PID, pid, &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        assert!(process_group::absent(pid).unwrap());
        assert!(!root.join("4.ready").exists()); assert!(!root.join("5.ready").exists());
    }

    #[test]
    fn actual_children_share_one_four_mib_output_budget() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let stages = postwire_schedule(physical_plan(&root, &[0; 4], 2, MIB as usize + 4096), |_, command|
            execute_logged_stage(command, started, &stdout, &stderr)).unwrap();
        assert_physical(&root, &stages); assert_eq!(stages.len(), 4);
        assert!(fs::metadata(&stdout).unwrap().len() + fs::metadata(&stderr).unwrap().len() > 4 * MIB);
        assert!(stages.iter().any(|s| s["receipt"]["log_overflow"] == true));
        let mut all = Vec::new(); let mut receipt = json!({"passed":true});
        append_postwire_receipts(&mut all, &mut receipt, stages);
        assert_eq!(receipt["passed"], false);
    }

    #[test]
    fn original_expired_clock_refuses_postwire_children_without_new_window() {
        let (root, stdout, stderr) = logs();
        let started = Instant::now() - driver_ftrace_process::LIMITS.wall;
        let stages = postwire_schedule(physical_plan(&root, &[0; 6], 3, 0), |_, command|
            execute_logged_stage(command, started, &stdout, &stderr)).unwrap();
        assert!(!stages.is_empty()); assert!(stages.len() <= POSTWIRE_CHILDREN);
        assert!(stages.iter().all(|s| s["receipt"]["passed"] == false && s["receipt"]["pid"].is_null()));
        assert!((0..6).all(|i| !root.join(format!("{i}.ready")).exists()));
    }

    #[test]
    fn failed_real_child_never_admits_its_dependent_neighbor() {
        let (root, stdout, stderr) = logs(); let started = Instant::now();
        let mut plan = physical_plan(&root, &[7, 0, 0, 0, 0], 2, 0);
        plan[4].prerequisite = Some(0);
        let stages = postwire_schedule(plan, |_, command|
            execute_logged_stage(command, started, &stdout, &stderr)).unwrap();
        assert_physical(&root, &stages); assert_eq!(stages.len(), 4);
        assert_eq!(stages[0]["receipt"]["raw_status"], 7);
        assert!(!root.join("4.ready").exists());
    }

    #[test]
    fn invalid_forward_dependency_refuses_before_any_child() {
        let (root, _, _) = logs(); let called = AtomicUsize::new(0);
        let mut plan = physical_plan(&root, &[0; 4], 2, 0);
        plan[0].prerequisite = Some(1);
        assert!(postwire_schedule(plan, |_, _| { called.fetch_add(1, Ordering::SeqCst); json!({"passed":true}) }).is_err());
        assert_eq!(called.load(Ordering::SeqCst), 0);
        assert!((0..4).all(|i| !root.join(format!("{i}.ready")).exists()));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owned_and_tx_controls_preserve_the_original_population() {
        assert_eq!(super::ACCEPTED_CONTROLS.len(), 24);
        assert_eq!(super::OWNED_DRIVER_CONTROLS,
            ["owned-inventory", "owned-identity", "owned-fault", "owned-gate"]);
        assert_eq!(super::expected_stage_count(true), 111);
        assert_eq!(super::expected_stage_count(false), 7);
    }

    #[test]
    fn exact_stage_completion_refuses_omitted_extra_and_failed_stages() {
        let good = serde_json::json!({"receipt":{"passed":true}});
        let expected = super::expected_stage_count(true);
        assert!(super::stages_complete(&vec![good.clone(); 111], expected));
        assert!(!super::stages_complete(&vec![good.clone(); 108], expected));
        assert!(!super::stages_complete(&vec![good.clone(); 109], expected));
        assert!(!super::stages_complete(&vec![good.clone(); 110], expected));
        assert!(!super::stages_complete(&vec![good.clone(); 112], expected));
        assert!(!super::stages_complete(&vec![good.clone(); 105], expected));
        let mut failed = vec![good; 111];
        failed[110]["receipt"]["passed"] = serde_json::json!(false);
        assert!(!super::stages_complete(&failed, expected));
    }
    use super::*;
    use super::driver_ftrace_process::LIMITS;
    use super::process_group::{Limits, OwnedChild, absent, exited_without_reap, only_terminal_leader, supervise};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::Duration;
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
    fn accepted_controls_are_required_and_fenced_as_an_exact_population() {
        let case = Case::new();
        assert!(control_sources(&case.path, true).is_err());
        // Both actual populations require their declared files. An empty
        // fixture is not a valid empty Unix population.
        assert!(control_sources(&case.path, false).is_err());
        fs::create_dir(case.path.join("unix")).unwrap();
        let unix_names = [
            "unix/keeper-phase-tests.c",
            "unix/keeper-readback-tests.c",
            "unix/keeper-terminal-tests.c",
        ];
        for name in unix_names {
            fs::write(case.path.join(name), name).unwrap();
        }
        let unix_expected: BTreeMap<String, String> = unix_names
            .into_iter()
            .map(|name| (name.to_owned(), digest(name.as_bytes())))
            .collect();
        assert_eq!(unix_expected.len(), 3);
        assert_eq!(control_sources(&case.path, false).unwrap(), unix_expected);
        for name in unix_names {
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(control_sources(&case.path, false).is_err());
            fs::write(case.path.join(name), "changed actual Unix control").unwrap();
            assert_ne!(control_sources(&case.path, false).unwrap(), unix_expected);
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(control_sources(&case.path, false).unwrap(), unix_expected);
        }
        for name in ACCEPTED_CONTROLS.iter().chain(DRIVER_FTRACE_CONTROL_INPUTS) {
            fs::write(case.path.join(name), name).unwrap();
        }
        let before = control_sources(&case.path, true).unwrap();
        assert_eq!(ACCEPTED_CONTROLS.len(), 24);
        assert_eq!(before.len(), 32);
        assert_eq!(
            before.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "birth-cleanup-driver-test.c",
                "driver-ftrace-facade.h",
                "driver-ftrace-process.rs",
                "driver-ftrace-test.c",
                "fd-effects-driver-test.c",
                "fd-effects-test.c",
                "fd-enrollment-driver-test.c",
                "fd-enrollment-test.c",
                "fd-journal-publish-test.c",
                "fd-session-dispatch-test.c",
                "fd-shared-predicate-test.c",
                "fd-table-test.c",
                "ftrace-coverage-test.c",
                "grouped-adoption-test.c",
                "grouped-owner-test.c",
                "grouped-recovery-test.c",
                "grouped-target-test.c",
                "grouped-wire-test.c",
                "legacy-id-probe.h",
                "owned-metadata-driver.h",
                "owned-metadata.h",
                "provider-open-observation.h",
                "remove-test.c",
                "retirement-target-test.c",
                "stream-copy-driver-test.c",
                "stream-copy-fault-producer-test.c",
                "stream-copy-v5-driver-test.c",
                "stream-frontier-test.c",
                "stream-membership-test.c",
                "stream-membership-v5-test.c",
                "stream-tx-test.c",
                "test.rs"
            ]
        );
        for name in ["driver-ftrace-test.c", "driver-ftrace-facade.h", "driver-ftrace-process.rs", "test.rs",
            "legacy-id-probe.h", "owned-metadata-driver.h", "owned-metadata.h", "provider-open-observation.h",
            "fd-session-dispatch-test.c"] {
            fs::write(case.path.join(name), "changed facade or closed import fence").unwrap();
            assert_ne!(before, control_sources(&case.path, true).unwrap());
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(control_sources(&case.path, true).is_err());
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(before, control_sources(&case.path, true).unwrap());
        }
        fs::write(case.path.join("grouped-wire-test.c"), "changed actual SCM assertion").unwrap();
        assert_ne!(before, control_sources(&case.path, true).unwrap());
        fs::remove_file(case.path.join("grouped-wire-test.c")).unwrap();
        assert!(control_sources(&case.path, true).is_err());
        fs::write(case.path.join("grouped-wire-test.c"), "grouped-wire-test.c").unwrap();
        assert_eq!(before, control_sources(&case.path, true).unwrap());
        for name in ["grouped-adoption-test.c", "grouped-recovery-test.c"] {
            fs::write(case.path.join(name), "changed retained adoption or recovery assertion").unwrap();
            assert_ne!(before, control_sources(&case.path, true).unwrap());
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(control_sources(&case.path, true).is_err());
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(before, control_sources(&case.path, true).unwrap());
        }
        for name in ["grouped-owner-test.c", "stream-membership-test.c", "grouped-target-test.c"] {
            fs::write(case.path.join(name), "changed grouped ownership or membership assertion").unwrap();
            assert_ne!(before, control_sources(&case.path, true).unwrap());
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(control_sources(&case.path, true).is_err());
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(before, control_sources(&case.path, true).unwrap());
        }
        for name in ["stream-frontier-test.c", "stream-copy-v5-driver-test.c", "stream-copy-fault-producer-test.c", "stream-membership-v5-test.c", "stream-tx-test.c"] {
            fs::write(case.path.join(name), "changed frontier or copy-version assertion").unwrap();
            assert_ne!(before, control_sources(&case.path, true).unwrap());
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(control_sources(&case.path, true).is_err());
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(before, control_sources(&case.path, true).unwrap());
        }
        fs::write(case.path.join("fd-shared-predicate-test.c"), "changed shared predicate assertion").unwrap();
        assert_ne!(before, control_sources(&case.path, true).unwrap());
        fs::remove_file(case.path.join("fd-shared-predicate-test.c")).unwrap();
        assert!(control_sources(&case.path, true).is_err());
        fs::write(case.path.join("fd-shared-predicate-test.c"), "fd-shared-predicate-test.c").unwrap();
        assert_eq!(before, control_sources(&case.path, true).unwrap());
        fs::write(case.path.join("fd-journal-publish-test.c"), "changed publication assertion").unwrap();
        assert_ne!(before, control_sources(&case.path, true).unwrap());
        fs::remove_file(case.path.join("fd-journal-publish-test.c")).unwrap();
        assert!(control_sources(&case.path, true).is_err());
        fs::write(case.path.join("fd-journal-publish-test.c"), "fd-journal-publish-test.c").unwrap();
        assert_eq!(before, control_sources(&case.path, true).unwrap());
        fs::write(case.path.join("birth-cleanup-driver-test.c"), "changed ownership assertion")
            .unwrap();
        assert_ne!(before, control_sources(&case.path, true).unwrap());
        fs::remove_file(case.path.join("birth-cleanup-driver-test.c")).unwrap();
        // Every required control remains part of the exact population.
        assert!(control_sources(&case.path, true).is_err());
        fs::write(
            case.path.join("birth-cleanup-driver-test.c"),
            "birth-cleanup-driver-test.c",
        )
        .unwrap();
        assert_eq!(before, control_sources(&case.path, true).unwrap());
        fs::write(case.path.join("fd-enrollment-test.c"), "changed assertion").unwrap();
        assert_ne!(before, control_sources(&case.path, true).unwrap());
        fs::remove_file(case.path.join("fd-table-test.c")).unwrap();
        assert!(control_sources(&case.path, true).is_err());
    }

    #[test]
    fn every_channel_module_and_header_is_required_and_fenced() {
        let case = Case::new();
        assert!(channel_control_inputs(&case.path, true).is_err());
        assert_eq!(channel_control_inputs(&case.path, false).unwrap(), BTreeMap::new());
        for name in CHANNEL_CONTROL_INPUTS { fs::write(case.path.join(name), name).unwrap(); }
        let before = channel_control_inputs(&case.path, true).unwrap();
        assert_eq!(before.len(), 10);
        assert_eq!(before.keys().map(String::as_str).collect::<Vec<_>>(), [
            "grouped-adoption-wire.c", "grouped-adoption-wire.h",
            "grouped-guardian-bootstrap.c", "grouped-guardian-bootstrap.h",
            "grouped-io.c",
            "grouped-keeper-dual.c", "grouped-keeper-dual.h",
            "grouped-keeper-wire.c", "grouped-keeper-wire.h",
            "grouped-owner.c",
        ]);
        for name in CHANNEL_CONTROL_INPUTS {
            fs::write(case.path.join(name), "changed actual channel implementation").unwrap();
            assert_ne!(before, channel_control_inputs(&case.path, true).unwrap());
            fs::remove_file(case.path.join(name)).unwrap();
            assert!(channel_control_inputs(&case.path, true).is_err());
            fs::write(case.path.join(name), name).unwrap();
            assert_eq!(before, channel_control_inputs(&case.path, true).unwrap());
        }
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
