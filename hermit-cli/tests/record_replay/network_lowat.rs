/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Current LOWAT governs blocked poll; a receive keeps its entry target.
//!
//! The native check establishes Linux semantics. Hermit runs are appended to
//! the existing explicitly selected network acceptance test, under its wrapper
//! and fixed schedule population. TRACE observations establish the wait/update
//! ordering separately in both runs; strict verification still compares INFO
//! and IO without stripping, ignored lines, or comparator relaxations.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;

use regex::Regex;
use serde_json::Value;
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Case {
    Poll,
    Recv,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::Poll => "poll",
            Self::Recv => "recv",
        }
    }

    fn result(self) -> &'static str {
        match self {
            Self::Poll => "POLLIN,3:abc",
            Self::Recv => "10:abcdefghij",
        }
    }

    fn controller_report(self) -> String {
        let outbound = match self {
            Self::Poll => "poll,done",
            Self::Recv => "recv,tail,done",
        };
        format!(
            "controller=complete case={} outbound={outbound}\n",
            self.name()
        )
    }
}

#[derive(Debug)]
struct GuestResult {
    case: Case,
    reader: String,
    fd: String,
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

fn fixture_result(text: &str, case: Case) -> Result<GuestResult, String> {
    let pattern = Regex::new(
        r"(?m)^lowat-case=(poll|recv) reader=(\d+) fd=(\d+) prefix=abc result=(POLLIN,3:abc|10:abcdefghij)$",
    ).unwrap();
    // Strict verification may print the identical marker twice.
    let rows: BTreeSet<_> = pattern
        .captures_iter(text)
        .map(|c| {
            (
                c[1].to_owned(),
                c[2].to_owned(),
                c[3].to_owned(),
                c[4].to_owned(),
            )
        })
        .collect();
    require(rows.len() == 1, "missing or ambiguous exact guest result")?;
    let (mode, reader, fd, result) = rows.into_iter().next().unwrap();
    require(
        mode == case.name() && result == case.result(),
        "wrong guest mode, payload, or readiness",
    )?;
    Ok(GuestResult { case, reader, fd })
}

fn observations(text: &str, pattern: &str) -> Vec<(usize, Vec<String>)> {
    let pattern = Regex::new(pattern).unwrap();
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            pattern.captures(line).map(|c| {
                (
                    i,
                    c.iter()
                        .skip(1)
                        .map(|part| part.unwrap().as_str().to_owned())
                        .collect(),
                )
            })
        })
        .collect()
}

fn wait_evidence(text: &str, result: &GuestResult, policy: &str) -> Result<Value, String> {
    let decisions = observations(
        text,
        r"\[network-wait-decision\] policy=(Record|Replay) dtid=(\d+) mm=(.*?) syscall=(\d+) interests=(.*)$",
    );
    let parks = observations(
        text,
        r"\[network-wait-parked\] dtid=(\d+) mm=(.*?) interests=(.*)$",
    );
    let updates = observations(
        text,
        r"\[network-lowat-committed\] dtid=(\d+) mm=(.*?) ofd=(.*?) lease=(.*?) requested=(-?\d+) effective=(\d+)$",
    );
    require(updates.len() == 2, "missing or extra LOWAT commits")?;
    let (high_i, high) = &updates[0];
    let (low_i, low) = &updates[1];
    require(
        high[4] == "10" && high[5] == "10" && low[4] == "1" && low[5] == "1",
        "wrong requested or effective LOWAT",
    )?;
    require(
        high[..3] == low[..3],
        "LOWAT commits changed setter, MM, or OFD",
    )?;
    require(high[3] != low[3], "LOWAT commits reused a control")?;
    require(high[0] != result.reader, "LOWAT setter must be the sibling")?;

    // The fixture establishes abc before recv(maximum=10, flags=0). The
    // receive implementation fixes its target at entry, consumes that prefix,
    // and waits for target - accepted. Seven is the remaining deficit, not a
    // new entry threshold and not an alternative accepted threshold.
    let entry_target = 10;
    let initial_prefix = b"abc".len();
    let remaining = entry_target - initial_prefix;
    let kind = match result.case {
        Case::Poll => "PollReadable".to_owned(),
        Case::Recv => format!("ReadableAtLeast({remaining})"),
    };
    let interest_pattern = Regex::new(&format!(
        r"^\[\(NetworkStreamCallId\((\d+)\), {}\)\]$",
        regex::escape(&kind),
    ))
    .unwrap();
    let mut relevant = Vec::new();
    let mut calls = BTreeSet::new();
    for (i, row) in &decisions {
        if row[1] == result.reader {
            require(
                row[0] == policy && row[2] == high[1],
                "wrong reader policy or MM",
            )?;
            let call = interest_pattern
                .captures(&row[4])
                .ok_or("wrong reader wait target")?;
            calls.insert(call[1].to_owned());
            relevant.push((*i, row[3].as_str(), row[4].as_str()));
        }
    }
    let before: Vec<_> = relevant
        .iter()
        .filter(|(i, _, _)| high_i < i && i < low_i)
        .collect();
    require(
        !before.is_empty(),
        "reader did not wait before the LOWAT commit",
    )?;
    require(
        relevant
            .iter()
            .map(|(_, syscall, _)| syscall)
            .collect::<BTreeSet<_>>()
            .len()
            == 1,
        "mixed reader syscalls",
    )?;

    let mut receive = Value::Null;
    if result.case == Case::Recv {
        require(calls.len() == 1, "mixed reader receive call identities")?;
        let signature = format!(
            r"recvfrom\({}, [^,]+, {entry_target}, 0, NULL, NULL\)",
            result.fd
        );
        let prefix = format!(r"\[syscall\]\[detcore, dtid {}\] ", result.reader);
        let entries = observations(
            text,
            &format!(r"{prefix}inbound syscall: {signature} = \? DETLOG_RECORD="),
        );
        let finishes = observations(
            text,
            &format!(
                r"{prefix}finish syscall #(\d+): {signature} = Ok\({entry_target}\) DETLOG_RECORD="
            ),
        );
        require(
            entries.len() == 1 && finishes.len() == 1,
            "exact recv entry or result missing or ambiguous",
        )?;
        let entry_i = entries[0].0;
        let (finish_i, count) = &finishes[0];
        require(
            *high_i < entry_i && entry_i < before[0].0 && before[0].0 < *low_i && low_i < finish_i,
            "recv entry/wait/commit/finish order changed",
        )?;
        require(
            relevant
                .iter()
                .all(|(i, syscall, _)| entry_i < *i && i < finish_i && *syscall == count[0]),
            "recv counter or wait boundary changed",
        )?;
        if policy == "Record" {
            require(
                relevant.iter().any(|(i, _, _)| low_i < i && i < finish_i),
                "Record lost its remaining deficit after lowering LOWAT",
            )?;
        }
        receive = json!({
            "entry_target": entry_target, "initial_accepted_prefix": initial_prefix,
            "remaining_deficit": remaining, "flags": 0,
            "entry_line": entry_i + 1, "finish_line": finish_i + 1,
            "derivation": "fixed source and fixture; actual outer syscall and wait/result receipts",
        });
    }
    let mut parked = BTreeSet::new();
    if policy == "Replay" {
        for (i, _, interests) in &before {
            for (p, row) in &parks {
                if i < p
                    && p < low_i
                    && row[0] == result.reader
                    && row[1] == high[1]
                    && row[2] == *interests
                {
                    parked.insert(p + 1);
                }
            }
        }
        require(
            !parked.is_empty(),
            "wait intent alone does not prove Replay waiter insertion",
        )?;
    }
    Ok(json!({
        "policy": policy, "reader": result.reader, "ofd": high[2],
        "initial_commit_line": high_i + 1, "lower_commit_line": low_i + 1,
        "prior_wait_decision_lines": before.iter().map(|(i, _, _)| i + 1).collect::<Vec<_>>(),
        "parked_lines": parked, "receive_target_proof": receive,
        "observation": if policy == "Replay" { "actual modeled waiter insertion" } else { "modeled adapter wait decision; kernel sleep not claimed" },
    }))
}

fn assert_guard(evidence: &Path, label: &str) {
    let report = fs::read_to_string(evidence.join(format!("{label}.safehermit")))
        .expect("safehermit report");
    let mut fields = BTreeMap::new();
    for line in report
        .lines()
        .filter_map(|line| line.strip_prefix("safehermit: "))
    {
        if let Some((key, value)) = line.split_once('=') {
            assert!(
                fields.insert(key, value).is_none(),
                "duplicate guard field {key}"
            );
        }
    }
    assert_eq!(fields.get("exit_code"), Some(&"0"));
    assert_eq!(fields.get("bound.wall"), Some(&"APPLIED:30s"));
    assert_eq!(
        fields.get("bound.cgroup"),
        Some(&"APPLIED:MemoryMax=16G MemorySwapMax=0")
    );
    assert!(
        fields
            .get("bound.disk")
            .is_some_and(|v| v.starts_with("APPLIED:1G for log_dir only;"))
    );
    assert_eq!(
        fields.get("bound.bytes"),
        Some(&"APPLIED:4194304 (LETHAL: the run is cgroup-killed at the cap)")
    );
    assert_eq!(fields.get("truncated"), Some(&"false"));
}

fn assert_full_strict_report(path: &Path, label: &str) {
    super::assert_l2_report(path, label);
    let report: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    let comparison = &report["comparison"];
    assert_eq!(comparison["log_scope"], "info");
    for key in [
        "compare_logs",
        "compare_io_buffers",
        "virtualize_time",
        "full_trace",
        "exact_remainder",
    ] {
        assert_eq!(comparison[key], true, "{label}: {key}");
    }
    for key in ["strip_lines", "ignore_lines", "skip_commit", "skip_detlog"] {
        assert_eq!(comparison[key], false, "{label}: {key}");
    }
}

fn trace_arguments(seed: u64, timeslice: u64) -> Vec<String> {
    let mut arguments = super::common_run_arguments(seed, timeslice);
    assert_eq!(arguments[0], "--log=info");
    arguments[0] = "--log=trace".into();
    arguments
}

fn retain_proof(path: &Path, text: &str, result: &GuestResult, policy: &str) {
    let proof = wait_evidence(text, result, policy)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    fs::write(path, serde_json::to_vec_pretty(&proof).unwrap())
        .expect("retain LOWAT boundary proof");
}

pub(super) fn assert_record_replay_lowat(evidence: &Path) {
    // Hermit's default file-log limit is 1 GiB. Keep a stricter inherited
    // limit; reject zero/unbounded or enlarged overrides rather than mutating
    // process-global environment shared with other tests.
    if let Some(limit) = std::env::var_os("HERMIT_LOG_MAX_BYTES") {
        let limit: u64 = limit
            .to_str()
            .expect("numeric file log limit")
            .parse()
            .expect("numeric file log limit");
        assert!(
            limit > 0 && limit <= 1 << 30,
            "LOWAT file logs require a positive limit at most 1 GiB"
        );
    }
    fs::create_dir(evidence).expect("new LOWAT evidence directory");
    let fixture = &super::super::workload("c_network_poll_lowat").path;
    for case in [Case::Poll, Case::Recv] {
        let directory = evidence.join(case.name());
        fs::create_dir(&directory).unwrap();
        let controller_directory = directory.join("controller");
        fs::create_dir(&controller_directory).unwrap();
        let (controller, port) = super::Controller::start(fixture, &controller_directory);
        let trace = directory.join("network.trace");
        let log = directory.join("record.log");
        let mut arguments = trace_arguments(0, 1_000_000);
        arguments.insert(1, format!("--log-file={}", log.display()));
        arguments.push(format!("--record-networking={}", trace.display()));
        let recorded = super::safehermit_command(
            &directory,
            "record",
            &arguments,
            fixture,
            &["client", &port, case.name()],
        );
        super::assert_success(&recorded, "LOWAT record");
        assert_eq!(controller.finish(), case.controller_report());
        assert_guard(&directory, "record");
        let result = fixture_result(std::str::from_utf8(&recorded.stdout).unwrap(), case).unwrap();
        retain_proof(
            &directory.join("record.boundary.json"),
            &fs::read_to_string(&log).unwrap(),
            &result,
            "Record",
        );
        let original_trace = fs::read(&trace).expect("network trace");
        assert!(!original_trace.is_empty());

        // The controller has completed and been reaped. All four cells use
        // exactly this input, and each strict cell executes twice.
        for (seed, timeslice) in super::REPLAY_CELLS {
            let label = format!("replay-{seed}-{timeslice}");
            let report = directory.join(format!("{label}.verify.json"));
            let logs = directory.join(format!("{label}.logs"));
            fs::create_dir(&logs).unwrap();
            let mut arguments = trace_arguments(*seed, *timeslice);
            arguments.extend([
                "--verify".into(),
                "--verify-strict".into(),
                "--keep-logs".into(),
                format!("--verify-log-dir={}", logs.display()),
                format!("--verify-json={}", report.display()),
                format!("--replay-networking={}", trace.display()),
            ]);
            let replayed = super::safehermit_command(
                &directory,
                &label,
                &arguments,
                fixture,
                &["client", &port, case.name()],
            );
            super::assert_success(&replayed, &label);
            assert_guard(&directory, &label);
            assert_full_strict_report(&report, &label);
            let result =
                fixture_result(std::str::from_utf8(&replayed.stdout).unwrap(), case).unwrap();
            let paths: Vec<_> = fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            assert_eq!(paths.len(), 2, "exactly two retained verify logs required");
            for prefix in ["run1_log_", "run2_log_"] {
                let matching: Vec<_> = paths
                    .iter()
                    .filter(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(prefix)
                    })
                    .collect();
                assert_eq!(matching.len(), 1, "missing/ambiguous {prefix} log");
                let path = matching[0];
                assert!(path.is_file());
                retain_proof(
                    &directory.join(format!("{label}.{prefix}boundary.json")),
                    &fs::read_to_string(path).unwrap(),
                    &result,
                    "Replay",
                );
            }
            assert_eq!(
                fs::read(&trace).unwrap(),
                original_trace,
                "{label} mutated the fixed input"
            );
        }
    }
}

#[test]
fn lowat_fixture_has_native_blocked_poll_and_receive_contracts() {
    let _guard = super::super::hermit_record_lock();
    let fixture = &super::super::workload("c_network_poll_lowat").path;
    let evidence = tempfile::tempdir().expect("native LOWAT evidence");
    for case in [Case::Poll, Case::Recv] {
        let directory = evidence.path().join(case.name());
        fs::create_dir(&directory).unwrap();
        let (controller, port) = super::Controller::start(fixture, &directory);
        let output = super::bounded_command(
            fixture,
            &[
                OsStr::new("client"),
                OsStr::new(&port),
                OsStr::new(case.name()),
                OsStr::new("native-witness"),
            ],
            super::CONTROLLER_WALL_SECONDS,
        );
        super::assert_success(&output, "native LOWAT client");
        assert_eq!(controller.finish(), case.controller_report());
        let stdout = std::str::from_utf8(&output.stdout).unwrap();
        let result = fixture_result(stdout, case).unwrap();
        let syscall = match case {
            Case::Poll => 7,
            Case::Recv => 45,
        };
        let witness = format!("native-blocked reader={} syscall={syscall}", result.reader);
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with("native-blocked "))
                .collect::<Vec<_>>(),
            [witness.as_str()]
        );
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    const MM: &str = "MmId { creator: 3, generation: 0 }";
    const OFD: &str = "OpenFileId { creator: 3, sequence: 0 }";

    fn fixture(case: Case, policy: &str) -> String {
        let target = match case {
            Case::Poll => "PollReadable",
            Case::Recv => "ReadableAtLeast(7)",
        };
        let interest = format!("[(NetworkStreamCallId(7), {target})]");
        let decision = format!(
            "[network-wait-decision] policy={policy} dtid=7 mm={MM} syscall=45 interests={interest}"
        );
        let mut rows = vec![format!(
            "[network-lowat-committed] dtid=3 mm={MM} ofd={OFD} lease=NetworkStreamLeaseId(1) requested=10 effective=10"
        )];
        if case == Case::Recv {
            rows.push("[syscall][detcore, dtid 7] inbound syscall: recvfrom(3, 0x1000, 10, 0, NULL, NULL) = ? DETLOG_RECORD={}".into());
        }
        rows.extend([decision.clone(), format!("[network-wait-parked] dtid=7 mm={MM} interests={interest}"), format!("[network-lowat-committed] dtid=3 mm={MM} ofd={OFD} lease=NetworkStreamLeaseId(2) requested=1 effective=1")]);
        if case == Case::Recv {
            if policy == "Record" {
                rows.push(decision);
            }
            rows.push("[syscall][detcore, dtid 7] finish syscall #45: recvfrom(3, 0x1000, 10, 0, NULL, NULL) = Ok(10) DETLOG_RECORD={}".into());
        }
        rows.join("\n")
    }

    fn result(case: Case) -> GuestResult {
        fixture_result(
            &format!(
                "lowat-case={} reader=7 fd=3 prefix=abc result={}\n",
                case.name(),
                case.result()
            ),
            case,
        )
        .unwrap()
    }

    fn rows(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn record_is_only_a_modeled_wait_decision() {
        let proof = wait_evidence(
            &fixture(Case::Poll, "Record"),
            &result(Case::Poll),
            "Record",
        )
        .unwrap();
        assert_eq!(proof["parked_lines"], json!([]));
        assert!(
            proof["observation"]
                .as_str()
                .unwrap()
                .contains("kernel sleep not claimed")
        );
    }

    #[test]
    fn replay_requires_actual_parked_receipt() {
        let text = fixture(Case::Poll, "Replay");
        assert_eq!(
            wait_evidence(&text, &result(Case::Poll), "Replay").unwrap()["parked_lines"],
            json!([3])
        );
        let mut lines = rows(&text);
        lines.remove(2);
        assert!(wait_evidence(&lines.join("\n"), &result(Case::Poll), "Replay").is_err());
    }

    #[test]
    fn recv_entry_target_ten_retains_remaining_deficit_seven() {
        let text = fixture(Case::Recv, "Replay");
        let proof = wait_evidence(&text, &result(Case::Recv), "Replay").unwrap();
        assert_eq!(proof["receive_target_proof"]["entry_target"], 10);
        assert_eq!(proof["receive_target_proof"]["initial_accepted_prefix"], 3);
        assert_eq!(proof["receive_target_proof"]["remaining_deficit"], 7);
        for threshold in [1, 4, 10] {
            assert!(
                wait_evidence(
                    &text.replace(
                        "ReadableAtLeast(7)",
                        &format!("ReadableAtLeast({threshold})")
                    ),
                    &result(Case::Recv),
                    "Replay"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn update_before_park_is_rejected() {
        let lines = rows(&fixture(Case::Poll, "Replay"));
        let reordered = [lines[0].as_str(), &lines[3], &lines[1], &lines[2]].join("\n");
        assert!(wait_evidence(&reordered, &result(Case::Poll), "Replay").is_err());
    }

    #[test]
    fn different_ofd_and_effective_clamp_are_rejected() {
        let text = fixture(Case::Poll, "Replay");
        for (old, new) in [
            ("requested=1 effective=1", "requested=1 effective=8"),
            (
                "lease=NetworkStreamLeaseId(2)",
                "lease=NetworkStreamLeaseId(1)",
            ),
        ] {
            assert!(wait_evidence(&text.replace(old, new), &result(Case::Poll), "Replay").is_err());
        }
        let mut lines = rows(&text);
        lines[3] = lines[3].replace("sequence: 0", "sequence: 1");
        assert!(wait_evidence(&lines.join("\n"), &result(Case::Poll), "Replay").is_err());
    }

    #[test]
    fn mixed_reader_and_missing_output_rejected() {
        assert!(
            wait_evidence(
                &fixture(Case::Poll, "Replay").replace("dtid=7", "dtid=8"),
                &result(Case::Poll),
                "Replay"
            )
            .is_err()
        );
        assert!(fixture_result("", Case::Poll).is_err());
    }

    #[test]
    fn extra_update_is_not_ignored() {
        let text = fixture(Case::Poll, "Replay");
        let extra = format!("{text}\n{}", text.lines().last().unwrap());
        assert!(wait_evidence(&extra, &result(Case::Poll), "Replay").is_err());
    }

    #[test]
    fn recv_entry_size_flags_and_result_are_exact() {
        let text = fixture(Case::Recv, "Replay");
        for (old, new) in [
            ("10, 0, NULL", "9, 0, NULL"),
            ("10, 0, NULL", "10, 256, NULL"),
            ("Ok(10)", "Ok(3)"),
            ("finish syscall #45", "finish syscall #46"),
        ] {
            assert!(wait_evidence(&text.replace(old, new), &result(Case::Recv), "Replay").is_err());
        }
    }

    #[test]
    fn recv_finish_before_lower_commit_is_rejected() {
        let mut lines = rows(&fixture(Case::Recv, "Replay"));
        lines.swap(4, 5);
        assert!(wait_evidence(&lines.join("\n"), &result(Case::Recv), "Replay").is_err());
    }

    #[test]
    fn recv_record_must_still_wait_after_lower_commit() {
        let text = fixture(Case::Recv, "Record");
        wait_evidence(&text, &result(Case::Recv), "Record").unwrap();
        let mut lines = rows(&text);
        lines.remove(5);
        assert!(wait_evidence(&lines.join("\n"), &result(Case::Recv), "Record").is_err());
    }

    #[test]
    fn mixed_receive_call_identities_are_rejected() {
        let mut lines = rows(&fixture(Case::Recv, "Replay"));
        let repeat = lines[2].replace("CallId(7)", "CallId(8)");
        lines.insert(5, repeat);
        assert!(wait_evidence(&lines.join("\n"), &result(Case::Recv), "Replay").is_err());
    }

    #[test]
    fn poll_retries_may_use_distinct_pin_receipts() {
        let text = fixture(Case::Poll, "Record");
        let extra = format!(
            "{text}\n{}",
            text.lines()
                .nth(1)
                .unwrap()
                .replace("CallId(7)", "CallId(8)")
        );
        wait_evidence(&extra, &result(Case::Poll), "Record").unwrap();
    }
}
