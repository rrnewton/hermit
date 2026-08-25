/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The KVM backend's INFO stream must not carry host-irreproducible values.
//!
//! `--verify-strict` compares INFO events under the production canonical policy:
//! it removes the real wall-clock prefix and ordinalizes only explicitly marked
//! host addresses. Every other byte remains exact. An unmarked host wall-clock
//! value inside a message body therefore makes two runs of the same guest differ
//! on that line alone, with no guest cause.
//!
//! This is currently invisible through `--verify` itself, because the KVM path
//! takes an output-only fallback (`compare_logs: false`, reported as
//! `bitwise_parity: false`) and never compares the internal stream. This test
//! compares it directly so the defect cannot hide behind that fallback.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use tempfile::NamedTempFile;

/// KVM runs are serialized for the same reason `kvm_harder.rs` serializes them.
static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

const LIFECYCLE_TIMING_EVENT: &str = "reverie-kvm lifecycle phase timings";

fn compile_guest(name: &str, extra_args: &[&str]) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-info-log-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let source = build_root.join(format!("{name}.c"));
    fs::write(&source, "int main(void) { return 0; }\n").expect("failed to write guest source");
    let binary = build_root.join(name);
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .args(extra_args)
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
    assert!(
        output.status.success(),
        "failed to compile {name}:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn run_kvm_log(binary: &Path, log_level: &str, extra_run_flags: &[&str]) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", log_level, "--backend", "kvm"])
        .args(["run", "--strict"])
        .args(extra_run_flags)
        .arg("--")
        .arg(binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to run guest under KVM: {error}"));
    assert!(
        output.status.success(),
        "KVM strict run failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[derive(Debug)]
struct CanonicalInfo {
    message_count: usize,
    text: String,
}

/// Run the same production canonical INFO extraction used by strict
/// verification. Keeping this call on the production implementation prevents
/// the test from drifting to a second approximation of the policy.
fn canonical_info(log: &str) -> CanonicalInfo {
    let mut input = NamedTempFile::new().expect("failed to create temporary KVM log");
    input
        .write_all(log.as_bytes())
        .expect("failed to write temporary KVM log");
    input.flush().expect("failed to flush temporary KVM log");

    let mut output = Vec::new();
    let reported = detcore::logdiff::write_canonical_info(input.path(), &mut output)
        .expect("failed to extract the canonical KVM INFO stream");
    let text = String::from_utf8(output).expect("canonical INFO stream was not UTF-8");
    CanonicalInfo {
        message_count: reported,
        text,
    }
}

/// Compare two captures with the production structured INFO comparator. Unlike
/// `write_canonical_info`, this retains message boundaries, including for
/// multiline messages that contain embedded newlines.
fn compare_canonical_info(left: &str, right: &str) -> detcore::logdiff::LogDiffSummary {
    let mut left_input = NamedTempFile::new().expect("failed to create first temporary KVM log");
    left_input
        .write_all(left.as_bytes())
        .expect("failed to write first temporary KVM log");
    left_input
        .flush()
        .expect("failed to flush first temporary KVM log");
    let mut right_input = NamedTempFile::new().expect("failed to create second temporary KVM log");
    right_input
        .write_all(right.as_bytes())
        .expect("failed to write second temporary KVM log");
    right_input
        .flush()
        .expect("failed to flush second temporary KVM log");

    let options = detcore::logdiff::LogDiffOpts {
        canonicalize_addresses: true,
        comparison: detcore::logdiff::LogComparisonMode::Info,
        no_color: true,
        ..Default::default()
    };
    detcore::logdiff::try_log_diff_detailed(left_input.path(), right_input.path(), &options)
        .expect("failed to compare canonical KVM INFO streams")
}

#[test]
fn kvm_info_stream_repeats_exactly_across_two_runs() {
    // Host limitation, not a product result: without /dev/kvm there is no KVM
    // backend to measure. Report the skip rather than passing silently.
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_info_stream_repeats_exactly_across_two_runs: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest("guest", &[]);
    let first_log = run_kvm_log(&binary, "info", &[]);
    let second_log = run_kvm_log(&binary, "info", &[]);
    let debug_log = run_kvm_log(&binary, "debug", &[]);

    let first = canonical_info(&first_log);
    let second = canonical_info(&second_log);
    let comparison = compare_canonical_info(&first_log, &second_log);

    // Guard the comparison itself: an empty or truncated capture would make the
    // equality below vacuously true, which is the classic false green.
    assert!(
        first.message_count > 50,
        "canonical KVM INFO capture is implausibly short ({} messages); the comparison below \
         would not discriminate anything",
        first.message_count,
    );
    assert_eq!(
        comparison.compared_left, comparison.compared_right,
        "KVM INFO stream changed message count between two runs of the same guest",
    );
    assert_eq!(first.message_count, comparison.compared_left);
    assert_eq!(second.message_count, comparison.compared_right);

    let first_lines = first.text.lines().collect::<Vec<_>>();
    let second_lines = second.text.lines().collect::<Vec<_>>();
    let divergences: Vec<(usize, &str, &str)> = first_lines
        .iter()
        .zip(second_lines.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, *a, *b))
        .collect();

    assert!(
        comparison.matched_with_evidence(),
        "KVM INFO stream is not reproducible across two runs of the same guest. \
         Every line below differs with no guest cause, so it carries a host value \
         that must not be in the compared stream:\n{}",
        divergences
            .iter()
            .map(|(index, a, b)| format!("  line {index}:\n    run 1: {a}\n    run 2: {b}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    assert!(
        !first.text.contains(LIFECYCLE_TIMING_EVENT),
        "host lifecycle timings leaked into the canonical INFO stream",
    );
    let debug_timing_lines = debug_log
        .lines()
        .filter(|line| line.contains(" DEBUG ") && line.contains(LIFECYCLE_TIMING_EVENT))
        .collect::<Vec<_>>();
    assert_eq!(
        debug_timing_lines.len(),
        1,
        "expected exactly one DEBUG lifecycle timing event; captured: {debug_timing_lines:?}",
    );
    for field in [
        "prepare_us=",
        "setup_us=",
        "execution_us=",
        "cleanup_us=",
        "teardown_us=",
        "lifecycle_us=",
    ] {
        assert!(
            debug_timing_lines[0].contains(field),
            "DEBUG lifecycle timing event is missing {field}: {}",
            debug_timing_lines[0],
        );
    }
}

#[test]
fn canonical_info_comparison_preserves_multiline_message_boundaries() {
    let left = concat!(
        "2026-08-14T01:00:00.000000Z INFO first\n",
        "INFO continuation\n",
        "2026-08-14T01:00:00.000001Z INFO second\n",
    );
    let right = concat!(
        "2026-08-14T01:00:00.000000Z INFO first\n",
        "2026-08-14T01:00:00.000001Z INFO continuation\n",
        "INFO second\n",
    );

    let flat_left = canonical_info(left);
    let flat_right = canonical_info(right);
    assert_eq!(flat_left.message_count, 2);
    assert_eq!(flat_right.message_count, 2);
    assert_eq!(
        flat_left.text, flat_right.text,
        "fixture must demonstrate why flattened canonical text is insufficient",
    );

    let structured = compare_canonical_info(left, right);
    assert_eq!(structured.compared_left, 2);
    assert_eq!(structured.compared_right, 2);
    assert!(
        structured.diff_found,
        "production comparison must reject a multiline message-boundary shift",
    );
}

#[derive(Debug, PartialEq, Eq)]
struct MemoryDigest {
    kind: &'static str,
    digest: String,
}

fn parse_memory_record(line: &str) -> Result<MemoryDigest, String> {
    let kind = if line.contains(" Stack ") {
        "Stack"
    } else if line.contains(" Heap ") {
        "Heap"
    } else {
        return Err(format!(
            "memory record names neither Stack nor Heap: {line}"
        ));
    };
    let (_, digest) = line
        .rsplit_once("->")
        .ok_or_else(|| format!("memory record has no digest separator: {line}"))?;
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "memory record has no 64-character hexadecimal content digest: {line}"
        ));
    }
    Ok(MemoryDigest {
        kind,
        digest: digest.to_owned(),
    })
}

/// Every parsed `DETLOG [memory]` content digest, in order.
fn memory_records(info: &str) -> Vec<MemoryDigest> {
    info.lines()
        .filter(|line| line.contains("DETLOG [memory]"))
        .map(|line| parse_memory_record(line).unwrap_or_else(|error| panic!("{error}")))
        .collect()
}

/// KVM must produce identical stack and heap CONTENT hashes across two runs of
/// the same guest.
///
/// SCOPE, stated because it is deliberately narrow and must not be read as more
/// than it is: this uses a **statically linked** guest. A dynamically linked
/// guest FAILS this property today, and not marginally — measured on devbig030,
/// `/bin/echo hello` under KVM has 98 of 113 stack-content hashes differing run
/// to run, while ptrace has 0 of 193 differing.
///
/// The cause is known and tracked, not papered over here: the KVM backend never
/// delivers `rdtsc` to the Reverie `Tool`, so Detcore's existing virtualization
/// never runs and the guest reads a raw host-derived cycle counter, which the
/// dynamic loader then leaves on the stack. See
/// <https://github.com/rrnewton/reverie/issues/448>. A static binary executes
/// zero `rdtsc` (measured: 0, versus 10 for every dynamically linked guest
/// tested), which is exactly why the property holds here and only here.
///
/// So this test pins the boundary of what is true rather than asserting a
/// broader claim. When #448 lands, the static restriction should be removed and
/// this test should pass for a dynamic guest too — that is the intended signal.
#[test]
fn kvm_memory_hashes_repeat_for_a_static_guest() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_memory_hashes_repeat_for_a_static_guest: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest("static_guest", &["-static"]);
    let flags = ["--detlog-stack", "--detlog-heap"];
    let first = canonical_info(&run_kvm_log(&binary, "info", &flags));
    let second = canonical_info(&run_kvm_log(&binary, "info", &flags));

    let first_records = memory_records(&first.text);
    let second_records = memory_records(&second.text);

    // Without this guard the equality below would pass vacuously if the flags
    // stopped emitting records at all — which is precisely how a memory
    // determinism check would silently stop checking anything.
    assert!(
        !first_records.is_empty(),
        "no DETLOG [memory] records were emitted under --detlog-stack --detlog-heap; \
         the comparison below would not discriminate anything",
    );
    assert!(
        first_records.iter().any(|record| record.kind == "Stack")
            && first_records.iter().any(|record| record.kind == "Heap"),
        "expected both Stack and Heap records; got {} record(s): {:?}",
        first_records.len(),
        first_records,
    );
    assert_eq!(
        first_records.len(),
        second_records.len(),
        "KVM emitted a different number of memory records between two runs",
    );

    let divergences: Vec<(usize, &MemoryDigest, &MemoryDigest)> = first_records
        .iter()
        .zip(second_records.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, a, b))
        .collect();

    assert!(
        divergences.is_empty(),
        "{} of {} KVM memory content hashes differ across two runs of the same \
         static guest:\n{}",
        divergences.len(),
        first_records.len(),
        divergences
            .iter()
            .map(|(index, a, b)| {
                format!("  record {index}:\n    run 1: {a:?}\n    run 2: {b:?}")
            })
            .collect::<Vec<_>>()
            .join("\n"),
    );
}

#[test]
fn memory_record_parser_requires_a_content_digest() {
    let without_digest = "INFO detcore: DETLOG [memory][dtid 1] Stack 0x1000-0x2000->";
    assert!(
        parse_memory_record(without_digest).is_err(),
        "a labeled memory record without a digest must not count as evidence",
    );
    let valid = "INFO detcore: DETLOG [memory][dtid 1] Heap 0x1000-0x2000->0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        parse_memory_record(valid),
        Ok(MemoryDigest {
            kind: "Heap",
            digest: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        }),
    );
}
