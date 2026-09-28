// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Exercise raw waitid through the real KVM CLI and Detcore. The guest emits
//! complete caller arenas because the generic waitid IO hook does not observe
//! every error write, padding byte, or overlapping output. Exact stdout plus
//! the full canonical INFO comparison covers these specific observations.
//! Immediate logical-child and CPU accounting invariants have component tests;
//! eventual guest ECHILD by itself would not establish their ordering.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

const TERMINAL_CASES: &[&str] = &[
    "writable",
    "null-info",
    "null-usage",
    "both-null",
    "protected-unused-tail",
    "read-only-info",
    "inaccessible-info",
    "read-only-usage",
    "inaccessible-usage",
    "usage-prefix-1",
    "usage-prefix-8",
    "usage-prefix-64",
    "first-scalar-split",
    "pid-scalar-split",
    "status-scalar-split",
    "aliased-outputs",
    "wnowait-writable",
    "wnowait-info-fault",
    "wnowait-usage-fault",
    "high-option-register-bits",
    "p-all-ignores-id",
    "fault-preserves-sibling",
];

const ERROR_CASES: &[&str] = &[
    "already-reaped",
    "invalid-options",
    "invalid-options-null",
    "invalid-options-protected",
    "pid-zero",
    "pid-zero-null",
    "pid-zero-protected",
    "pid-negative",
    "pgid-negative",
    "invalid-selector",
    "live-no-event",
    "live-no-event-null",
    "live-no-event-protected",
    "live-usage-protected",
    "live-high-options",
    "live-p-all-ignores-id",
    "interrupted-protected-info",
    "interrupted-writable-info",
    "restarted-writable-info",
    "ignored-sibling-sigurg",
];

fn assert_observations(stdout: &[u8], mode: &str, expected: &[&str], children: u64) {
    let text = std::str::from_utf8(stdout).expect("complete fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one complete JSON observation per line"))
        .collect();
    let mut names = BTreeSet::new();
    let mut checks = 0;
    let mut summaries = 0;
    let mut ignored_child = None;
    let mut ignored_checks = 0;
    for (index, row) in rows.iter().enumerate() {
        match row["type"].as_str().expect("observation type") {
            "case" => {
                let name = row["name"].as_str().expect("case name");
                assert!(names.insert(name), "duplicate case {name}");
                for (before, after) in [("info_before", "info_after"), ("aux_before", "aux_after")]
                {
                    let before = row[before].as_str().expect("entire before arena");
                    let after = row[after].as_str().expect("entire after arena");
                    assert!(!before.is_empty() && before.len() <= 4 * 65536);
                    assert_eq!(before.len(), after.len());
                    assert_eq!(before.len() % 2, 0);
                    assert!(
                        before
                            .as_bytes()
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .all(|pair| pair == b"a5")
                    );
                    assert!(after.bytes().all(|byte| byte.is_ascii_hexdigit()));
                }
                assert!(row["rc"].is_i64());
                assert!(row["errno"].is_i64());
                if name == "ignored-sibling-sigurg" {
                    assert!(ignored_child.replace(row["id"].as_u64().unwrap()).is_none());
                    for (key, expected) in [
                        ("which", 1),
                        ("rc", -1),
                        ("errno", libc::EFAULT),
                        ("alarms", 1),
                        ("info_mode", 2),
                        ("info_offset", 64),
                        ("usage_protection", 3),
                        ("null_usage", 1),
                        ("signal", libc::SIGURG),
                        ("wait_calls", 1),
                        ("send_calls", 1),
                        ("send_rc", 0),
                        ("send_errno", 0),
                        ("pre_returned", 0),
                        ("release_seen", 1),
                        ("release_rc", 1),
                        ("release_errno", 0),
                        ("ack_rc", 1),
                        ("ack_errno", 0),
                        ("sender_failure", 0),
                    ] {
                        assert_eq!(row[key].as_i64(), Some(i64::from(expected)), "{key}");
                    }
                    assert_eq!(row["options"], "0x4");
                    assert_eq!(row["signal_default"], true);
                    for key in ["id", "tgid", "waiter_tid", "sender_tid"] {
                        assert!(row[key].as_i64().unwrap() > 0, "{key}");
                    }
                    assert_ne!(row["waiter_tid"], row["sender_tid"]);
                    assert_eq!(row["tgid"], row["waiter_tid"]);
                    assert!(row["release_fd"].as_i64().unwrap() >= 0);
                    assert_eq!(row["pre_info"], row["info_before"]);
                    assert_eq!(row["pre_aux"], row["aux_before"]);
                    assert_eq!(row["info_after"], row["info_before"]);
                    assert_eq!(row["aux_after"], row["aux_before"]);
                }
                if matches!(
                    name,
                    "interrupted-writable-info" | "restarted-writable-info"
                ) {
                    let before = row["info_before"].as_str().unwrap();
                    let mut zero_fields = before.as_bytes().to_vec();
                    for offset in [0, 4, 8, 16, 20, 24] {
                        zero_fields[2 * (64 + offset)..2 * (68 + offset)].fill(b'0');
                    }
                    assert_eq!(row["alarms"], 1);
                    assert_eq!(row["aux_after"], row["aux_before"]);
                    if name == "interrupted-writable-info" {
                        assert_eq!(row["rc"], -1);
                        assert_eq!(row["errno"], libc::EINTR);
                        assert_eq!(
                            row["info_after"].as_str().unwrap().as_bytes(),
                            zero_fields.as_slice()
                        );
                    } else {
                        assert_eq!(row["rc"], 0);
                        assert_eq!(row["errno"], 0);
                        assert_eq!(row["null_usage"], 1);
                        assert_eq!(row["info_offset"], 64);
                        assert_eq!(row["sa_restart"], true);
                        assert_eq!(row["wait_calls"], 1);
                        assert_eq!(row["handler_seen"], 1);
                        assert_eq!(row["release_rc"], 1);
                        assert_eq!(row["release_errno"], 0);
                        assert_eq!(
                            row["handler_info"].as_str().unwrap().as_bytes(),
                            zero_fields.as_slice()
                        );
                    }
                }
            }
            "check" => {
                checks += 1;
                let arena = row["arena"].as_str().expect("full guarded follow-up arena");
                assert_eq!(arena.len(), 320);
                assert!(arena.bytes().all(|byte| byte.is_ascii_hexdigit()));
                if row["name"] == "ignored-child-ECHILD" {
                    ignored_checks += 1;
                    assert_eq!(row["pid"].as_u64(), ignored_child);
                    assert_eq!(row["rc"], -1);
                    assert_eq!(row["errno"], libc::ECHILD);
                    assert_eq!(row["options"], "0x5");
                    let mut expected = "a5".repeat(160).into_bytes();
                    for offset in [0, 4, 8, 16, 20, 24] {
                        expected[2 * (16 + offset)..2 * (20 + offset)].fill(b'0');
                    }
                    assert_eq!(arena.as_bytes(), expected.as_slice());
                }
            }
            "retained" => {
                assert_eq!(row["rc"], 0);
                assert_eq!(row["errno"], 0);
            }
            "summary" => {
                summaries += 1;
                assert_eq!(
                    index + 1,
                    rows.len(),
                    "summary must be the final observation"
                );
                assert_eq!(row["mode"], mode);
                assert_eq!(row["cases"].as_u64(), Some(expected.len() as u64));
                assert_eq!(row["children"].as_u64(), Some(children));
                assert_eq!(row["passed"], true);
                assert!(row["assertions"].as_u64().unwrap() > expected.len() as u64);
            }
            other => panic!("unexpected observation {other}"),
        }
    }
    let expected_names: BTreeSet<_> = expected.iter().copied().collect();
    assert_eq!(names, expected_names);
    assert!(
        checks > 0,
        "actual child peeks and reap checks must execute"
    );
    assert_eq!(summaries, 1);
    assert_eq!(ignored_checks, usize::from(mode == "errors"));
}

// Full INFO records can span lines (scheduler commits do). Retain their
// boundaries; do not treat a bare SIGURG substring as a signal grant.
fn info_records(text: &str) -> Vec<String> {
    assert!(text.ends_with('\n'), "complete retained INFO log");
    let header = regex::Regex::new(r"^\S+\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+\S+: (.*)$").unwrap();
    let mut records = Vec::new();
    let mut current: Option<(bool, String)> = None;
    for line in text.lines() {
        if let Some(capture) = header.captures(line) {
            if let Some((true, message)) = current.take() {
                records.push(message.trim_end_matches('\n').to_owned());
            }
            current = Some((&capture[1] == "INFO", capture[2].to_owned()));
        } else if let Some((_, message)) = current.as_mut() {
            message.push('\n');
            message.push_str(line);
        }
    }
    if let Some((true, message)) = current {
        records.push(message.trim_end_matches('\n').to_owned());
    }
    assert!(!records.is_empty());
    records
}

fn assert_ignored_signal_trace(log: &[u8], stdout: &[u8]) {
    let rows: Vec<serde_json::Value> = std::str::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let selected: Vec<_> = rows
        .iter()
        .filter(|row| row["name"] == "ignored-sibling-sigurg")
        .collect();
    assert_eq!(selected.len(), 1);
    let row = selected[0];
    let waiter = row["waiter_tid"].as_u64().unwrap();
    let sender = row["sender_tid"].as_u64().unwrap();
    let child = row["id"].as_u64().unwrap();
    let tgid = row["tgid"].as_u64().unwrap();
    let fd = row["release_fd"].as_u64().unwrap();
    assert!(waiter > 0 && sender > 0 && child > 0 && tgid > 0 && waiter != sender);
    let records = info_records(std::str::from_utf8(log).expect("complete UTF-8 INFO log"));
    let entry = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] inbound syscall: (.+) = \?$",
    )
    .unwrap();
    let finish = regex::Regex::new(
        r"^DETLOG \[syscall\]\[detcore, dtid (\d+)\] finish syscall #(\d+): (.+) = (.+)$",
    )
    .unwrap();
    let mut entries = Vec::new();
    let mut results = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Some((message, metadata)) = record.rsplit_once(" DETLOG_RECORD=") else {
            continue;
        };
        if let Some(capture) = entry.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "syscall");
            entries.push((
                index,
                capture[1].parse::<u64>().unwrap(),
                capture[2].to_owned(),
            ));
        } else if let Some(capture) = finish.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "syscall_result");
            assert_eq!(
                value["event"]["finished_syscall_number"].as_u64(),
                Some(capture[2].parse::<u64>().unwrap())
            );
            results.push((
                index,
                capture[1].parse::<u64>().unwrap(),
                capture[3].to_owned(),
                capture[4].to_owned(),
            ));
        }
    }
    let pair = |tid: u64, pattern: &str, result: &str| {
        let expression = regex::Regex::new(pattern).unwrap();
        let starts: Vec<_> = entries
            .iter()
            .filter(|(_, actual, call)| *actual == tid && expression.is_match(call))
            .collect();
        assert_eq!(starts.len(), 1, "one exact syscall entry: {pattern}");
        let (begin, _, call) = starts[0];
        let ends: Vec<_> = results
            .iter()
            .filter(|(_, actual, actual_call, _)| *actual == tid && actual_call == call)
            .collect();
        assert_eq!(ends.len(), 1, "one exact syscall result: {call}");
        assert_eq!(ends[0].3, result);
        assert!(*begin < ends[0].0);
        (*begin, ends[0].0)
    };
    let wait = pair(
        waiter,
        &format!(r"^waitid\(1, {child}, 0x[0-9a-f]+, 4, NULL\)$"),
        "Err(Errno(EFAULT))",
    );
    let send = pair(
        sender,
        &format!(r"^tgkill\({tgid}, {waiter}, 23\)$"),
        "Ok(0)",
    );
    let release = pair(
        sender,
        &format!(r"^write\({fd}, 0x[0-9a-f]+, 1\)$"),
        "Ok(1)",
    );
    let gone = pair(
        waiter,
        &format!(r"^waitid\(1, {child}, 0x[0-9a-f]+, 5, NULL\)$"),
        "Err(Errno(ECHILD))",
    );
    let park = regex::Regex::new(&format!(r"^\[scheduler\] NONCOMMIT turn \d+, parking dettid {waiter} for child ChildWaitSpec \{{ selector: Exact\(DetPid\({child}\)\), owner: None, exit_class: Sigchld \}}$")).unwrap();
    // This single-resource Debug spelling is bound to SigWrapper's actual
    // derive and scheduler formatter. Until a released run emits it, the
    // proposal makes no claim to have observed this particular event.
    let grant = regex::Regex::new(&format!(r"(?s)^\[sched-step5\] >>>>>>>\n\n COMMIT turn (\d+), dettid {waiter} using resources \{{WaitidSignals\(\[SigWrapper\(23\)\]\): W\}}, on previously committed [^\n]+$")).unwrap();
    let mut grants = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let Some((message, metadata)) = record.rsplit_once(" DETLOG_RECORD=") else {
            continue;
        };
        if let Some(capture) = grant.captures(message) {
            let value: serde_json::Value = serde_json::from_str(metadata).unwrap();
            assert_eq!(value["schema"], 1);
            assert_eq!(value["event"]["kind"], "scheduler_commit");
            assert_eq!(
                value["event"]["scheduler_turn"].as_u64(),
                Some(capture[1].parse::<u64>().unwrap())
            );
            grants.push(index);
        }
    }
    assert_eq!(grants.len(), 1, "one actual ignored-signal grant");
    let granted = grants[0];
    assert!(wait.0 < send.0 && send.0 < granted && send.1 < release.0);
    assert!(granted < release.0 && release.0 < wait.1 && wait.1 < gone.0);
    assert!(
        records
            .iter()
            .enumerate()
            .any(|(n, message)| wait.0 < n && n < send.0 && park.is_match(message)),
        "actual initial park before sibling send"
    );
    assert!(
        records
            .iter()
            .enumerate()
            .any(|(n, message)| granted < n && n < release.0 && park.is_match(message)),
        "actual re-park before child release"
    );
}

fn assert_sibling_observations(stdout: &[u8], direction: &str, selector: &str, consume: &str) {
    let text = std::str::from_utf8(stdout).expect("complete sibling fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("complete sibling JSON observation"))
        .collect();
    let fault = consume == "efault";
    assert_eq!(rows.len(), if fault { 11 } else { 10 });
    let summary = rows.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["direction"], direction);
    assert_eq!(summary["selector"], selector);
    assert_eq!(summary["consume"], consume);
    assert_eq!(summary["calls"], 9);
    assert_eq!(summary["passed"], true);
    let leader = summary["leader"].as_u64().unwrap();
    let worker = summary["worker"].as_u64().unwrap();
    let child = summary["child"].as_u64().unwrap();
    assert!(leader > 0 && worker > 0 && child > 0);
    assert!(leader != worker && leader != child && worker != child);
    let (creator, waiter) = if direction == "leader-child" {
        (leader, worker)
    } else {
        (worker, leader)
    };
    let calls: Vec<_> = rows[..rows.len() - 1]
        .iter()
        .filter(|row| row["type"] == "wait")
        .collect();
    assert_eq!(calls.len(), 9);
    let names = [
        if direction == "leader-child" {
            "creator leader proves readiness before pthread_create"
        } else {
            "creator worker proves child waitability"
        },
        "sibling owner restriction leaves child intact",
        "first sibling peek values and padding",
        "repeated sibling peek values and padding",
        if fault {
            "usage fault precedes every info store"
        } else {
            "sibling consumption values and padding"
        },
        "exact child consumed once",
        "all children consumed once",
        "exact child consumed once",
        "all children consumed once",
    ];
    let flags = [
        0x1000004_u64,
        0x21000004,
        0x1000004,
        0x1000004,
        4,
        5,
        5,
        5,
        5,
    ];
    let uid = u32::try_from(calls[0]["uid"].as_u64().unwrap()).unwrap();
    for (index, row) in calls.iter().enumerate() {
        assert_eq!(row["name"], names[index]);
        assert_eq!(row["child"].as_u64(), Some(child));
        assert_eq!(row["uid"].as_u64(), Some(u64::from(uid)));
        assert_eq!(row["options"].as_u64(), Some(flags[index]));
        let owner_call = index == 0 || index >= 7;
        assert_eq!(
            row["tid"].as_u64(),
            Some(if owner_call { creator } else { waiter })
        );
        let all = match index {
            0 | 5 | 7 => false,
            6 | 8 => true,
            _ => selector == "all",
        };
        assert_eq!(row["which"], if all { 0 } else { 1 });
        let error = if index == 1 || index >= 5 {
            libc::ECHILD
        } else if index == 4 && fault {
            libc::EFAULT
        } else {
            0
        };
        assert_eq!(row["errno"], error);
        assert_eq!(row["rc"], if error == 0 { 0 } else { -1 });
        assert_eq!(row["before"], "a5".repeat(160));
        let mut expected = [0xa5_u8; 160];
        if !(index == 4 && fault) {
            let fields = if error == 0 {
                [
                    libc::SIGCHLD as u32,
                    0,
                    libc::CLD_EXITED as u32,
                    u32::try_from(child).unwrap(),
                    uid,
                    73,
                ]
            } else {
                [0; 6]
            };
            for (offset, value) in [0, 4, 8, 16, 20, 24].into_iter().zip(fields) {
                expected[16 + offset..20 + offset].copy_from_slice(&value.to_le_bytes());
            }
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(320);
        for byte in expected {
            encoded.push(HEX[usize::from(byte >> 4)] as char);
            encoded.push(HEX[usize::from(byte & 15)] as char);
        }
        assert_eq!(row["after"], encoded, "entire caller arena at row {index}");
    }
    if fault {
        let usage = &rows[5];
        assert_eq!(usage["type"], "usage");
        let before = usage["before"].as_str().unwrap();
        assert!((8192..=131072).contains(&before.len()));
        assert_eq!(before.len() % 2, 0);
        assert!(
            before
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .all(|pair| pair == b"a5")
        );
        assert_eq!(usage["after"], before);
        assert!(
            rows[..5]
                .iter()
                .chain(rows[6..10].iter())
                .all(|row| row["type"] == "wait")
        );
    } else {
        assert!(rows[..9].iter().all(|row| row["type"] == "wait"));
    }
}

pub(super) fn run(mode: &str) {
    let (expected, children) = match mode {
        "terminal" => (TERMINAL_CASES, 23),
        "errors" => (ERROR_CASES, 5),
        _ => panic!("unknown waitid fixture mode"),
    };
    run_fixture(
        mode,
        "kvm_waitid_copyout.c",
        &[mode],
        |stdout| assert_observations(stdout, mode, expected, children),
        |full_log, stdout| {
            if mode == "errors" {
                assert_ignored_signal_trace(full_log, stdout);
            }
        },
    );
}

pub(super) fn run_sibling(direction: &str, selector: &str, consume: &str) {
    assert!(matches!(direction, "leader-child" | "worker-child"));
    assert!(matches!(selector, "pid" | "all"));
    assert!(matches!(consume, "success" | "efault"));
    let mode = format!("sibling-{direction}-{selector}-{consume}");
    run_fixture(
        &mode,
        "kvm_waitid_sibling.c",
        &[direction, selector, consume],
        |stdout| assert_sibling_observations(stdout, direction, selector, consume),
        |_, _| {},
    );
}

// Shared compile, resource bounds and complete typed verification. The callers
// retain their own additional guest assertions; none replaces the policy below.
fn run_fixture(
    mode: &str,
    fixture_name: &str,
    guest_args: &[&str],
    check_stdout: impl Fn(&[u8]),
    check_log: impl Fn(&[u8], &[u8]),
) {
    let _lock = super::hermit_run_guard();
    // One shared deadline includes fixture compilation and the two-guest
    // verification, inside the existing 57-second outer test timeout.
    let deadline = Instant::now() + Duration::from_secs(50);
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("selected waitid regression requires /dev/kvm; absence is not a pass");
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-waitid-copyout-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "KVM waitid copyout artifacts retained at {}",
        root.display()
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture_name);
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture source");
    let guest = root.join("program");
    let compile = root.join("compile");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O2",
                "-g",
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Wformat=2",
                "-Werror",
                "-fno-pie",
                "-no-pie",
                "-pthread",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &compile,
        Duration::from_secs(20),
    );
    assert!(status.success(), "waitid fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let directory = root.join(mode);
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).unwrap();
    let report_path = directory.join("verification.json");
    let mut args = vec![
        "--log=info",
        "run",
        "--base-env=minimal",
        "--backend=kvm",
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--",
        guest.to_str().unwrap(),
    ];
    args.extend_from_slice(guest_args);
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .expect("test deadline");
    let status = bounded_command_with_timeout(&mut command, &directory, remaining);
    assert_eq!(status.code(), Some(0), "both guests must actually succeed");
    let stdout = bounded_read(&directory.join("stdout"), 64 * MIB);
    check_stdout(&stdout);
    let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete current typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact output/status parity");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let counts = report.compared_log_messages.unwrap();
    assert!(counts.left > 0);
    assert_eq!(counts.left, counts.right);
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert!(policy.compare_logs);
    assert_eq!(policy.record_envelope, RecordEnvelopeReport::AllRecordsV1);
    assert_eq!(policy.compare_io_buffers, Some(true));
    assert_eq!(policy.virtualize_time, Some(true));
    assert_eq!(policy.strip_lines, Some(false));
    assert_eq!(policy.canonicalize_addresses, Some(true));
    assert_eq!(policy.full_trace, Some(true));
    assert_eq!(policy.exact_remainder, Some(true));
    assert_eq!(policy.ignore_lines, Some(false));
    assert_eq!(policy.skip_commit, Some(false));
    assert_eq!(policy.skip_detlog, Some(false));
    assert_eq!(policy.log_scope, Some(ComparedLogScope::Info));
    assert_eq!(
        policy.stripped_prefixes.as_deref(),
        Some(["real-wall-clock-prefix/v1".to_owned()].as_slice())
    );
    assert_eq!(
        policy.canonicalizations.as_deref(),
        Some(["host-address-to-first-appearance-ordinal/v1".to_owned()].as_slice())
    );
    let outputs = report.compared_outputs.as_ref().unwrap();
    for operand in [&outputs.left, &outputs.right] {
        assert_eq!(operand.exit_code, Some(0));
        assert!(operand.signal.is_none());
        assert_eq!(operand.stdout_bytes, stdout.len() as u64);
        assert_eq!(operand.stdout_sha256, Digest::new(&stdout).to_string());
        assert_eq!(operand.stderr_bytes, 0);
        assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
    }
    for prefix in ["run1_log_", "run2_log_"] {
        let paths: Vec<_> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        assert_eq!(paths.len(), 1, "one retained full log per actual guest");
        let full_log = bounded_read(&paths[0], 64 * MIB);
        assert!(!full_log.is_empty());
        check_log(&full_log, &stdout);
    }
    assert!(
        Instant::now() < deadline,
        "complete test stays inside its shared bound"
    );
}
