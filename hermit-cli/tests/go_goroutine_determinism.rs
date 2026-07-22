/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! NONDET_SOURCE: Go goroutine scheduling.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

const NATIVE_RUNS: usize = 16;
const STRICT_RUNS: usize = 6;
const TIMEOUT_SECONDS: u64 = 60;

static GO_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn go_version() -> String {
    let mut command = Command::new("go");
    command.arg("version");
    let output = command_output(command, "Go version probe");
    String::from_utf8(output.stdout)
        .expect("go version output should be UTF-8")
        .trim()
        .to_owned()
}

fn go_guest() -> &'static Path {
    GO_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("go-goroutine");
            fs::create_dir_all(&build_root).expect("failed to create Go goroutine build directory");
            let binary = build_root.join("goroutine-channel-order");

            let mut command = Command::new("go");
            command
                .args(["build", "-trimpath", "-o"])
                .arg(&binary)
                .arg(repository.join("experiments/go-goroutine/main.go"));
            command_output(command, "goroutine-channel-order compilation");
            binary
        })
        .as_path()
}

fn run_with_timeout(command: Command, label: &str) -> Vec<u8> {
    let mut timeout = Command::new("timeout");
    timeout
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(command.get_program())
        .args(command.get_args());
    command_output(timeout, label).stdout
}

fn run_native(iteration: usize) -> Vec<u8> {
    run_with_timeout(
        Command::new(go_guest()),
        &format!("native goroutine ordering iteration {}", iteration + 1),
    )
}

fn run_strict(iteration: usize) -> Vec<u8> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "run",
        "--strict",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--preemption-timeout=disabled",
        "--tmp=/tmp",
        "--",
    ]);
    command.arg(go_guest());
    run_with_timeout(
        command,
        &format!("strict goroutine ordering iteration {}", iteration + 1),
    )
}

fn parse_hash(output: &[u8]) -> String {
    let output = std::str::from_utf8(output)
        .expect("goroutine-channel-order output should be UTF-8")
        .trim();
    assert!(
        output.starts_with("program=goroutine-channel-order go="),
        "unexpected program identity: {output}"
    );
    assert!(
        output.contains(" workers=32 order="),
        "missing worker count or receive order: {output}"
    );
    let hash = output
        .rsplit_once(" sha256=")
        .map(|(_, hash)| hash)
        .expect("goroutine-channel-order output should contain sha256");
    assert_eq!(hash.len(), 64, "unexpected SHA-256 length: {output}");
    assert!(
        hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "SHA-256 should be hexadecimal: {output}"
    );
    hash.to_owned()
}

#[test]
fn goroutine_channel_order_varies_natively_and_stabilizes_under_strict() {
    let version = go_version();
    eprintln!("compiler={version} program=goroutine-channel-order");

    let mut native = BTreeMap::<String, String>::new();
    for iteration in 0..NATIVE_RUNS {
        let output = run_native(iteration);
        let hash = parse_hash(&output);
        native.insert(hash, String::from_utf8_lossy(&output).trim().to_owned());
    }
    assert!(
        native.len() >= 2,
        "native goroutine scheduling produced only {} unique hash in {NATIVE_RUNS} runs: {native:?}",
        native.len(),
    );

    let expected = run_strict(0);
    let mut strict_hashes = BTreeSet::from([parse_hash(&expected)]);
    for iteration in 1..STRICT_RUNS {
        let output = run_strict(iteration);
        strict_hashes.insert(parse_hash(&output));
        assert_eq!(
            output,
            expected,
            "strict goroutine receive order changed on iteration {}",
            iteration + 1,
        );
    }
    assert_eq!(
        strict_hashes.len(),
        1,
        "strict mode produced more than one goroutine-order hash: {strict_hashes:?}"
    );
}
