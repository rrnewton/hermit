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
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
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

// Independent role executions use this fixed bound. Each worker owns its
// Command/Child and calls the unchanged stage supervisor with the action's
// original start and aggregate append-only logs. New failed-prefix sequences
// use the same join/failure discipline with a stricter two-compiler bound.
const ROLE_MUTANT_CONCURRENCY: usize = 4;

fn role_worker_failure(error: String) -> Value {
    // No child receipt is available on a thread admission failure or panic.
    // In particular, joining a panicked worker does not certify its cleanup.
    json!({"passed":false,"expected_refusal":true,"raw":null,"worker_error":error})
}

fn role_batches(
    roles: std::ops::RangeInclusive<u32>,
    execute: impl Fn(u32) -> Value + Sync,
) -> Vec<(u32, Value)> {
    bounded_batches(roles, ROLE_MUTANT_CONCURRENCY, execute)
}

fn bounded_batches(
    roles: std::ops::RangeInclusive<u32>,
    concurrency: usize,
    execute: impl Fn(u32) -> Value + Sync,
) -> Vec<(u32, Value)> {
    let roles: Vec<_> = roles.collect();
    let mut results = Vec::new();
    for batch in roles.chunks(concurrency) {
        let completed = std::thread::scope(|scope| {
            let mut workers = Vec::new();
            let mut admission_failure = None;
            for &role in batch {
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

fn execute_role_mutants(
    executable: &Path,
    roles: std::ops::RangeInclusive<u32>,
    started: Instant,
    stdout: &Path,
    stderr: &Path,
) -> Vec<(u32, Value)> {
    role_batches(roles, |role| {
        let mut command = Command::new(executable);
        command.env("AP_FTRACE_MUTATE_ROLE", role.to_string());
        role_mutant_receipt(execute_stage(&mut command, started, stdout, stderr))
    })
}

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

// Test-only facade identity: never add these files to the production DSO contract.
const DRIVER_FTRACE_CONTROL_INPUTS: &[&str] = &[
    "driver-ftrace-facade.h", "driver-ftrace-process.rs", "test.rs",
];
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
    let mut receipt = execute_stage(&mut command, started, &stdout_path, &stderr_path);
    let mut stages = vec![json!({ "name":"compression-parser", "receipt":receipt })];
    if receipt["passed"] == true && accepted {
        for name in ACCEPTED_CONTROLS {
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
            let compiled = execute_stage(&mut compile, started, &stdout_path, &stderr_path);
            stages.push(json!({ "name":format!("compile:{name}"), "receipt":compiled }));
            if compiled["passed"] != true {
                receipt = compiled;
                break;
            }
            if driver_ftrace {
                let mut fence = Command::new("python3");
                fence.arg("-c").arg(DRIVER_FTRACE_IMPORT_FENCE).arg(&driver_object);
                let checked = execute_stage(&mut fence, started, &stdout_path, &stderr_path);
                stages.push(json!({"name":"fence:driver-ftrace-imports","receipt":checked}));
                if checked["passed"] != true { receipt = checked; break; }
                let mut link = Command::new("clang");
                link.arg(&driver_object).arg("-Wl,--no-undefined").arg("-o").arg(&executable);
                let linked = execute_stage(&mut link, started, &stdout_path, &stderr_path);
                stages.push(json!({"name":"link:driver-ftrace-control","receipt":linked}));
                if linked["passed"] != true { receipt = linked; break; }
            }
            let tested = execute_stage(
                &mut Command::new(&executable),
                started,
                &stdout_path,
                &stderr_path,
            );
            stages.push(json!({ "name":format!("test:{name}"), "receipt":tested }));
            if tested["passed"] != true {
                receipt = tested;
                break;
            }

        }
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
            let compiled = execute_stage(&mut compile, started, &stdout_path, &stderr_path);
            stages.push(json!({"name":format!("compile:{name}"),"receipt":compiled}));
            if compiled["passed"] != true {
                receipt = compiled;
                break;
            }
            let tested = execute_stage(
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
        for (label, name) in [
            ("fd", "fd-effects-test.c"),
            ("stream-copy-v5", "stream-copy-v5-driver-test.c"),
            ("stream-membership-v5", "stream-membership-v5-test.c"),
        ] {
            let executable=output.join(format!("ftrace-production-{label}"));
            let mut compile=Command::new("clang");
            compile.args(["-O2","-Wall","-Wextra","-Werror","-UNDEBUG",
                "-DAP_FTRACE_PROVIDER=1"]);
            if name=="fd-effects-test.c" {compile.arg("-DAP_NATIVE_COPY_VERSION=5ULL");}
            compile
                .arg("-I").arg(&source)
                .arg(source.join(name)).arg("-o").arg(&executable);
            let compiled=execute_stage(&mut compile,started,&stdout_path,&stderr_path);
            stages.push(json!({"name":format!("compile:ftrace-production-{label}"),"receipt":compiled}));
            if compiled["passed"]!=true {receipt=compiled;break;}
            let tested=execute_stage(&mut Command::new(&executable),started,&stdout_path,&stderr_path);
            stages.push(json!({"name":format!("test:ftrace-production-{label}"),"receipt":tested}));
            if tested["passed"]!=true {receipt=tested;break;}
        }
    }
    if receipt["passed"] == true && accepted {
        let executable=output.join("ftrace-link-shape-mutant");
        let mut compile=Command::new("clang");
        compile.args(["-O2","-Wall","-Wextra","-Werror","-UNDEBUG",
            "-DAP_FTRACE_MUTATE_LINK_SHAPE=1"])
            .arg("-I").arg(&source)
            .arg(source.join("ftrace-coverage-test.c")).arg("-o").arg(&executable);
        let compiled=execute_stage(&mut compile,started,&stdout_path,&stderr_path);
        stages.push(json!({"name":"compile:ftrace-link-shape-mutant","receipt":compiled}));
        if compiled["passed"]==true {
            let tested=execute_stage(&mut Command::new(&executable),started,&stdout_path,&stderr_path);
            let refused=tested["passed"]==false &&
                tested["signal"].as_i64()==Some(libc::SIGABRT as i64) && tested["timed_out"]==false &&
                tested["log_overflow"]==false && tested["primary_error"].is_null() &&
                tested["terminal_bounds_error"].is_null() &&
                tested["natural_terminal_group"]==true && tested["cleanup_complete"]==true &&
                tested["cleanup_errors"].as_array().is_some_and(|errors| errors.is_empty());
            let accepted=json!({"passed":refused,"expected_refusal":true,"raw":tested});
            stages.push(json!({"name":"test:ftrace-link-shape-mutant","receipt":accepted}));
            if !refused {receipt=accepted;}
        } else {receipt=compiled;}
    }
    if receipt["passed"] == true && accepted {
        let executable=output.join("grouped-source-fragment-overread-mutant");
        let mut compile=Command::new("clang");
        compile.args(["-O0","-Wall","-Wextra","-Werror","-UNDEBUG",
            "-DAP_FTRACE_PROVIDER=1","-DAP_STREAM_COPY_MUTATE_FRAGMENT_LENGTH=1"])
            .arg("-I").arg(&source)
            .arg(source.join("stream-copy-v5-driver-test.c")).arg("-o").arg(&executable);
        let compiled=execute_stage(&mut compile,started,&stdout_path,&stderr_path);
        stages.push(json!({"name":"compile:grouped-source-fragment-overread-mutant","receipt":compiled}));
        if compiled["passed"]==true {
            let tested=execute_stage(&mut Command::new(&executable),started,&stdout_path,&stderr_path);
            let refused=tested["passed"]==false &&
                tested["signal"].as_i64()==Some(libc::SIGABRT as i64) && tested["timed_out"]==false &&
                tested["log_overflow"]==false && tested["primary_error"].is_null() &&
                tested["terminal_bounds_error"].is_null() &&
                tested["natural_terminal_group"]==true && tested["cleanup_complete"]==true &&
                tested["cleanup_errors"].as_array().is_some_and(|errors| errors.is_empty());
            let accepted=json!({"passed":refused,"expected_refusal":true,"raw":tested});
            stages.push(json!({"name":"test:grouped-source-fragment-overread-mutant","receipt":accepted}));
            if !refused {receipt=accepted;}
        } else {receipt=compiled;}
    }
    if receipt["passed"] == true && accepted {
        let baseline=output.join("stream-copy-fault-producer-test");
        let mutants=[
            ("DROP_DATA", "fault"), ("DROP_PRIOR", "two-fragment"),
            ("CX", "fault"), ("SOURCE", "fault"), ("WINDOW", "fault"), ("FRONTIER", "fault"),
        ];
        // Independent controls share the ORIGINAL aggregate timer and logs.
        // At most two compiler/test sequences run concurrently; no extra
        // timeout, stage, missing receipt or cleanup exemption is introduced.
        for (_, completed) in bounded_batches(0..=5, 2, |index| {
            let (label,selector)=mutants[index as usize];
            let mut local=Vec::new();
            let executable=output.join(format!("failed-prefix-mutant-{label}"));
            let mut compile=Command::new("clang");
            compile.args(["-O2","-Wall","-Wextra","-Werror","-UNDEBUG"])
                .arg(format!("-DAP_STREAM_FAULT_MUTATE_{label}=1"))
                .arg("-I").arg(&source).arg(source.join("stream-copy-fault-producer-test.c"))
                .arg("-o").arg(&executable);
            let compiled=execute_stage(&mut compile,started,&stdout_path,&stderr_path);
            local.push(json!({"name":format!("compile:failed-prefix-{label}"),"receipt":compiled}));
            if compiled["passed"]==true {
                let mut command=Command::new(&executable);command.arg(selector);
                let refused=role_mutant_receipt(execute_stage(&mut command,started,&stdout_path,&stderr_path));
                local.push(json!({"name":format!("test:failed-prefix-{label}"),"receipt":refused}));
                if refused["passed"]==true {
                    // SOURCE corrupts all bytes; its qualifying neighbor is
                    // the unmutated build, never a relaxed byte comparator.
                    let mut neighbor=Command::new(if label=="SOURCE" {&baseline} else {&executable});
                    neighbor.arg("full");
                    let tested=execute_stage(&mut neighbor,started,&stdout_path,&stderr_path);
                    local.push(json!({"name":format!("neighbor:failed-prefix-{label}"),"receipt":tested}));
                }
            }
            json!({"passed":local.len()==3 && local.iter().all(|s| s["receipt"]["passed"]==true),"stages":local})
        }) {
            if completed["passed"]!=true && receipt["passed"]==true {receipt=completed.clone();}
            if let Some(local)=completed["stages"].as_array() {stages.extend(local.iter().cloned());}
        }
        if receipt["passed"] == true {
            for (_,completed) in role_batches(12..=15, |role| {
                let (selector,neighbor)=if role<14 {("linear","fault")} else {("fault","linear")};
                let mut local=Vec::new();
                let mut command=Command::new(&baseline);
                command.arg(selector).env("AP_FTRACE_MUTATE_ROLE",role.to_string());
                let refused=role_mutant_receipt(execute_stage(&mut command,started,&stdout_path,&stderr_path));
                local.push(json!({"name":format!("test:failed-prefix-role-{role}"),"receipt":refused}));
                if refused["passed"]==true {
                    let mut command=Command::new(&baseline);
                    command.arg(neighbor).env("AP_FTRACE_MUTATE_ROLE",role.to_string());
                    let tested=execute_stage(&mut command,started,&stdout_path,&stderr_path);
                    local.push(json!({"name":format!("neighbor:failed-prefix-role-{role}"),"receipt":tested}));
                }
                json!({"passed":local.len()==2 && local.iter().all(|s| s["receipt"]["passed"]==true),"stages":local})
            }) {
                if completed["passed"]!=true && receipt["passed"]==true {receipt=completed.clone();}
                if let Some(local)=completed["stages"].as_array() {stages.extend(local.iter().cloned());}
            }
        }
    }
    if receipt["passed"] == true && accepted {
        for (label, source_name, roles) in [
            ("fd", "fd-effects-test.c", 1..=11),
            ("stream-copy", "stream-copy-v5-driver-test.c", 12..=15),
            ("stream-membership", "stream-membership-v5-test.c", 16..=17),
        ] {
            let executable=output.join(format!("ftrace-coverage-mutants-{label}"));
            let mut compile=Command::new("clang");
            // Compile each unchanged production helper/header path once, then
            // select one host-only role gate per execution. This preserves 17
            // independent aborting controls without charging 17 compiler
            // startups to the unchanged aggregate 60-second action.
            compile.args(["-O0","-Wall","-Wextra","-Werror","-UNDEBUG",
                    "-DAP_FTRACE_PROVIDER=1","-DAP_FTRACE_RUNTIME_MUTANT=1"]);
            if source_name=="fd-effects-test.c" {compile.arg("-DAP_NATIVE_COPY_VERSION=5ULL");}
            if source_name=="fd-effects-test.c" {compile.arg("-DAP_FTRACE_MUTANT_ONLY=1");}
            compile
                .arg("-I").arg(&source)
                .arg(source.join(source_name)).arg("-o").arg(&executable);
            let compiled=execute_stage(&mut compile,started,&stdout_path,&stderr_path);
            stages.push(json!({"name":format!("compile:ftrace-mutants-{label}"),"receipt":compiled}));
            if compiled["passed"]!=true {receipt=compiled;break;}
            append_role_receipts(&mut stages, &mut receipt, execute_role_mutants(
                &executable, roles, started, &stdout_path, &stderr_path,
            ));
            if receipt["passed"]!=true {break;}
        }
    }
    let expected_stages = 1 + if accepted {
        2 * ACCEPTED_CONTROLS.len()+32+26 // unchanged old stages plus six mutant triplets and four role pairs
    } else {
        2 * UNIX_CONTROLS.len()
    };
    let complete =
        stages.len() == expected_stages && stages.iter().all(|s| s["receipt"]["passed"] == true);
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
mod tests {
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
        assert_eq!(ACCEPTED_CONTROLS.len(), 23);
        assert_eq!(before.len(), 26);
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
                "fd-shared-predicate-test.c",
                "fd-table-test.c",
                "ftrace-coverage-test.c",
                "grouped-adoption-test.c",
                "grouped-owner-test.c",
                "grouped-recovery-test.c",
                "grouped-target-test.c",
                "grouped-wire-test.c",
                "remove-test.c",
                "retirement-target-test.c",
                "stream-copy-driver-test.c",
                "stream-copy-fault-producer-test.c",
                "stream-copy-v5-driver-test.c",
                "stream-frontier-test.c",
                "stream-membership-test.c",
                "stream-membership-v5-test.c",
                "test.rs"
            ]
        );
        for name in ["driver-ftrace-test.c", "driver-ftrace-facade.h", "driver-ftrace-process.rs", "test.rs"] {
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
        for name in ["stream-frontier-test.c", "stream-copy-v5-driver-test.c", "stream-copy-fault-producer-test.c", "stream-membership-v5-test.c"] {
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
