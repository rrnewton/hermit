// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! The access trace (`--record-access-trace-to`) end to end, on the ptrace
//! backend. A trace must observe without changing the run: replaying one
//! recorded schedule with and without it gives the same re-recorded
//! schedule, stdout, exit status and DETLOG INFO count. And every index in
//! it must name the matching event of the run's own recorded schedule.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

use detcore_model::access_trace::Access;
use detcore_model::access_trace::AccessTrace;
use detcore_model::access_trace::ChannelId;
use detcore_model::access_trace::FdOp;
use detcore_model::access_trace::LifetimeOp;
use detcore_model::access_trace::SignalDelivery;
use detcore_model::access_trace::SyscallRecord;
use detcore_model::access_trace::Task;
use detcore_model::access_trace::TimerOp;
use detcore_model::access_trace::TraceRecord;
use detcore_model::access_trace::read_access_trace;

use super::hermit_run_guard;
use super::hermit_test;

const EPOCH: &str = "--epoch=2026-01-01T00:00:00Z";

/// The guests, compiled once per test process beneath the target directory
/// (the run container replaces /tmp).
struct Guests {
    /// `tests/c/simple/racewrite_nostdlib.c`: two processes race to write.
    racewrite: PathBuf,
    /// `tests/fixtures/access_trace_mix.c`: two threads race over a mutex, a
    /// pipe, a file, a mapping, a timer, a signal and a child.
    mix: PathBuf,
}

fn guests() -> &'static Guests {
    static GUESTS: OnceLock<Guests> = OnceLock::new();
    GUESTS.get_or_init(|| {
        let root = tempfile::Builder::new()
            .prefix("access-trace-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("guest directory")
            .keep();
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli is inside the repository");
        let compile = |source: PathBuf, flags: &[&str], name: &str| {
            let output = root.join(name);
            let status = Command::new("cc")
                .args(flags)
                .arg(&source)
                .arg("-o")
                .arg(&output)
                .status()
                .expect("run cc");
            assert!(status.success(), "compiling {} failed", source.display());
            output
        };
        Guests {
            racewrite: compile(
                repository.join("tests/c/simple/racewrite_nostdlib.c"),
                &["-g", "-nostdlib"],
                "racewrite",
            ),
            mix: compile(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/access_trace_mix.c"),
                &["-O0", "-g", "-pthread", "-D_GNU_SOURCE"],
                "mix",
            ),
        }
    })
}

fn run(args: &[String]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(args);
    hermit_test::hermit_output(&format!("hermit {args:?}"), &mut command)
}

fn path_arg(flag: &str, path: &Path) -> String {
    format!("{flag}={}", path.display())
}

/// One run, its schedule recorded, and what it left.
struct Replay {
    output: Output,
    events: Vec<u8>,
    /// The INFO messages `--verify-strict` compares (`BitwiseInfoV1`: the
    /// wall-clock prefix removed, marked host addresses ordinalized).
    info: Vec<String>,
    trace: Option<AccessTrace>,
}

/// Record `guest` once under chaos, so the schedule interleaves its threads.
fn record(guest: &Path, directory: &Path) -> PathBuf {
    run_traced(guest, directory, "recorded", None, None);
    directory.join("recorded.events")
}

/// Run `guest` under chaos at INFO level, recording its schedule, replaying
/// `schedule` when given, and with the trace written when `trace` gives
/// its first index.
fn run_traced(
    guest: &Path,
    directory: &Path,
    name: &str,
    schedule: Option<&Path>,
    trace: Option<u64>,
) -> Replay {
    let events = directory.join(format!("{name}.events"));
    let log = directory.join(format!("{name}.log"));
    let trace_path = directory.join(format!("{name}.access"));
    let mut args = vec![
        "--log=info".to_string(),
        path_arg("--log-file", &log),
        "run".to_string(),
        EPOCH.to_string(),
        "--chaos".to_string(),
        "--sched-seed=3".to_string(),
        path_arg("--record-preemptions-to", &events),
    ];
    if let Some(schedule) = schedule {
        args.push(path_arg("--replay-schedule-from", schedule));
    }
    if let Some(from) = trace {
        args.push(path_arg("--record-access-trace-to", &trace_path));
        args.push(format!("--access-trace-from={from}"));
    }
    args.extend(["--".to_string(), guest.display().to_string()]);
    let output = run(&args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    let mut info = Vec::new();
    detcore::logdiff::write_bitwise_info_v1_bytes(
        &fs::read(&log).expect("no log"),
        name,
        &mut info,
    )
    .expect("the log is not comparable");
    Replay {
        output,
        events: fs::read(&events).expect("no recorded schedule"),
        info: String::from_utf8(info)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect(),
        trace: trace.map(|_| {
            read_access_trace(fs::read(&trace_path).expect("no trace").as_slice())
                .expect("trace does not parse")
        }),
    }
}

/// Replay `schedule`, re-recording it, with the trace from `trace`.
fn replay(
    guest: &Path,
    schedule: &Path,
    directory: &Path,
    name: &str,
    trace: Option<u64>,
) -> Replay {
    run_traced(guest, directory, name, Some(schedule), trace)
}

/// The one stderr line that measures the host: on a host whose
/// performance counters fail Reverie's validation, Hermit reports how many
/// locked instructions the check counted, which differs run to run.
const HOST_PMU_LINE: &str = "WARNING: this host's performance counters failed Reverie's validation";

/// Stderr without the host line, and how many lines that dropped.
fn guest_stderr(output: &Output) -> (Vec<String>, usize) {
    let all: Vec<String> = String::from_utf8_lossy(&output.stderr)
        .lines()
        .map(str::to_owned)
        .collect();
    let kept: Vec<String> = all
        .iter()
        .filter(|line| !line.starts_with(HOST_PMU_LINE))
        .cloned()
        .collect();
    let dropped = all.len() - kept.len();
    (kept, dropped)
}

/// The two runs show the same thing: exit status, stdout, stderr (but for
/// that host line), the
/// recorded schedule (every event, its thread and its virtual time) and
/// the INFO messages `--verify-strict` compares.
fn assert_same_run(plain: &Replay, traced: &Replay, what: &str) {
    assert_eq!(traced.output.status, plain.output.status, "{what}");
    assert_eq!(traced.output.stdout, plain.output.stdout, "{what}");
    // Both runs drop the same number of host lines, so the filter cannot
    // hide a line only one run printed.
    assert_eq!(
        guest_stderr(&traced.output),
        guest_stderr(&plain.output),
        "{what}"
    );
    assert!(
        traced.events == plain.events,
        "{what}: the traced run recorded another schedule"
    );
    assert!(!plain.info.is_empty(), "{what}: no INFO records to compare");
    assert!(
        traced.info == plain.info,
        "{what}: the INFO records differ, first at {:?}",
        traced
            .info
            .iter()
            .zip(&plain.info)
            .position(|(a, b)| a != b)
            .unwrap_or(traced.info.len().min(plain.info.len()))
    );
    let trace = traced.trace.as_ref().expect("traced run");
    assert!(trace.end.is_some(), "{what}: the trace has no end line");
}

fn syscalls(trace: &AccessTrace) -> impl Iterator<Item = &SyscallRecord> {
    trace.records.iter().filter_map(|record| match record {
        TraceRecord::Syscall(syscall) => Some(syscall),
        _ => None,
    })
}

/// The trace changes nothing a run shows, whether the run follows a
/// recorded schedule or makes its own under chaos: exit status, stdout,
/// stderr, the recorded schedule and the INFO messages are those of the
/// same run without it.
#[test]
fn an_access_trace_leaves_a_replayed_run_unchanged() {
    let _guard = hermit_run_guard();
    for guest in [&guests().racewrite, &guests().mix] {
        let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let schedule = record(guest, directory.path());
        let plain = replay(guest, &schedule, directory.path(), "plain", None);
        let traced = replay(guest, &schedule, directory.path(), "traced", Some(0));
        assert_same_run(&plain, &traced, &format!("replay of {guest:?}"));
        let fresh = run_traced(guest, directory.path(), "fresh", None, None);
        let fresh_traced = run_traced(guest, directory.path(), "fresh_traced", None, Some(0));
        assert_same_run(&fresh, &fresh_traced, &format!("chaos run of {guest:?}"));
    }
}

/// Every index names the matching event of the run's own recorded schedule:
/// a syscall's prehook and posthook events are its thread's, of its syscall,
/// and a signal's event is its delivery. The records name what the mixed
/// guest does.
#[test]
fn access_trace_indices_name_the_runs_own_events() {
    let _guard = hermit_run_guard();
    let guest = &guests().mix;
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let schedule = record(guest, directory.path());
    let traced = replay(guest, &schedule, directory.path(), "traced", Some(0));
    assert!(traced.output.status.success(), "{:?}", traced.output);
    let trace = traced.trace.expect("traced replay");
    let events: serde_json::Value = serde_json::from_slice(&traced.events).unwrap();
    let events = events["global"].as_array().expect("global events");
    assert_eq!(
        trace.end.as_ref().map(|end| end.events),
        Some(events.len() as u64)
    );

    let syscall_event = |index: u64, tid: i32, number: i64, phase: &str| {
        let event = &events[index as usize];
        assert_eq!(event["dettid"], tid, "event {index}: {event}");
        assert_eq!(
            event["op"]["Syscall"],
            serde_json::json!([number, phase]),
            "event {index}: {event}"
        );
    };
    let mut syscall_count = 0;
    for record in &trace.records {
        match record {
            TraceRecord::Syscall(call) => {
                syscall_count += 1;
                let tid = call.tid.as_raw();
                let number = call.sysno as i64;
                syscall_event(call.pre, tid, number, "Prehook");
                if let Some(post) = call.post {
                    assert!(post > call.pre);
                    syscall_event(post, tid, number, "Posthook");
                }
            }
            TraceRecord::Signal(signal) => {
                let event = &events[signal.event as usize];
                assert_eq!(event["dettid"], signal.tid.as_raw(), "{event}");
                assert!(event["op"].get("SignalReceived").is_some(), "{event}");
            }
            other => panic!("unexpected record {other:?}"),
        }
    }
    assert!(syscall_count > 50, "only {syscall_count} syscall records");

    // A thread's nth counts its syscalls: each thread's records count up by
    // one, in order of their prehooks.
    let mut by_thread: std::collections::BTreeMap<i32, Vec<(u64, u64)>> = Default::default();
    for call in syscalls(&trace) {
        by_thread
            .entry(call.tid.as_raw())
            .or_default()
            .push((call.pre, call.nth));
    }
    // Absolute, too: each thread's first syscall is its first, the unit of
    // `--happens-before` `SyscallCount`. The root thread's is the execve.
    for (tid, mut calls) in by_thread {
        calls.sort_unstable();
        assert_eq!(calls[0].1, 1, "thread {tid}'s first syscall: {calls:?}");
        for pair in calls.windows(2) {
            assert_eq!(pair[1].1, pair[0].1 + 1, "thread {tid}: {calls:?}");
        }
    }
    let first = syscalls(&trace).min_by_key(|call| call.pre).unwrap();
    assert_eq!((first.sysno.name(), first.nth), ("execve", 1), "{first:?}");

    let find = |name: &str| -> Vec<&SyscallRecord> {
        syscalls(&trace)
            .filter(|call| call.sysno.name() == name)
            .collect()
    };
    let has =
        |call: &SyscallRecord, wanted: &dyn Fn(&Access) -> bool| call.accesses.iter().any(wanted);
    // A dup2 onto a free number dups without a close; one onto an open
    // number closes it first.
    // aarch64's C library makes dup2(3) a dup3 call: the generic syscall
    // table (include/uapi/asm-generic/unistd.h) has no dup2.
    let dup2 = find(if cfg!(target_arch = "x86_64") {
        "dup2"
    } else {
        "dup3"
    });
    let ops = |call: &SyscallRecord| -> Vec<FdOp> {
        call.accesses
            .iter()
            .filter_map(|a| match a {
                Access::Fd { op, .. } => Some(op.clone()),
                _ => None,
            })
            .collect()
    };
    let dup = FdOp::Dup {
        source: dup2[0].args[0] as i32,
    };
    assert_eq!(ops(dup2[0]), vec![FdOp::Use, dup], "{dup2:?}");
    let onto_open = dup2
        .iter()
        .find(|call| ops(call).contains(&FdOp::Close))
        .expect("a dup2 onto an open number");
    assert_eq!(
        ops(onto_open),
        vec![
            FdOp::Use,
            FdOp::Close,
            FdOp::Dup {
                source: onto_open.args[0] as i32
            }
        ]
    );
    // The guest's file is created in the run's own /tmp, which no other run
    // shares: every run draws the same name for it, and in a shared working
    // directory a concurrent run's file made the O_EXCL create fail and the
    // schedule change. The trace names no paths, so read them from the INFO
    // messages (`openat(-100, <addr> -> "<path>", ...)`).
    let file_paths: Vec<&str> = traced
        .info
        .iter()
        .filter_map(|line| {
            let (_, rest) = line.split_once("-> \"")?;
            let (path, _) = rest.split_once('"')?;
            path.contains("access_trace_mix.").then_some(path)
        })
        .collect();
    assert!(
        !file_paths.is_empty(),
        "no INFO message names the guest's file"
    );
    assert!(
        file_paths.iter().all(|path| path.starts_with("/tmp/")),
        "{file_paths:?}"
    );
    // close_range closes both pipe ends, and the channel behind each.
    let close_range = find("close_range");
    assert_eq!(close_range.len(), 1, "{close_range:?}");
    assert_eq!(ops(close_range[0]), vec![FdOp::Close, FdOp::Close]);
    let channel_closes = close_range[0]
        .accesses
        .iter()
        .filter(|a| {
            matches!(
                a,
                Access::Channel {
                    id: Some(ChannelId::Pair(_)),
                    op: detcore_model::access_trace::ChannelOp::Close,
                    ..
                }
            )
        })
        .count();
    assert_eq!(channel_closes, 2, "{close_range:?}");
    // A write to the pipe names the pipe by its read end.
    assert!(
        find("write").iter().any(|call| has(call, &|a| matches!(
            a,
            Access::Channel {
                id: Some(ChannelId::Pair(_)),
                ..
            }
        ))),
        "no pipe write carries the pipe's identity"
    );
    // The racing lines on stdout are in the trace.
    let lines: Vec<Vec<u8>> = find("write")
        .iter()
        .filter_map(|call| call.out_bytes.clone())
        .collect();
    assert!(lines.contains(&b"a 0\n".to_vec()), "{lines:?}");
    assert!(lines.contains(&b"b 15\n".to_vec()), "{lines:?}");
    // A clock read carries the timespec it returned.
    let clocks = find("clock_gettime");
    assert!(!clocks.is_empty(), "no clock_gettime");
    for call in clocks.iter().filter(|call| call.result == Some(0)) {
        assert_eq!(call.returned.map(|r| r.len), Some(16), "{call:?}");
    }
    // The file read returned three bytes.
    assert!(
        find("read")
            .iter()
            .any(|call| call.returned.is_some_and(|r| r.len == 3)),
        "no three-byte read"
    );
    for (name, op) in [
        ("timer_create", TimerOp::Create),
        ("timer_settime", TimerOp::Set),
        ("timer_delete", TimerOp::Delete),
    ] {
        assert!(
            find(name).iter().any(|call| has(call, &|a| matches!(
                a,
                Access::Timer { op: got, .. } if *got == op
            ))),
            "{name}"
        );
    }
    assert!(
        syscalls(&trace).any(|call| has(call, &|a| matches!(
            a,
            Access::Lifetime {
                target: Task::Thread(_),
                op: LifetimeOp::Create
            }
        ))),
        "no thread creation"
    );
    assert!(
        find("wait4").iter().any(|call| has(call, &|a| matches!(
            a,
            Access::Lifetime {
                op: LifetimeOp::Wait,
                ..
            }
        ))),
        "no reaped child"
    );
    let exit_group = find("exit_group");
    assert!(
        exit_group
            .iter()
            .any(|call| call.post.is_none() && !call.accesses.is_empty()),
        "{exit_group:?}"
    );
    assert!(
        trace.records.iter().any(|record| matches!(
            record,
            TraceRecord::Signal(signal)
                if signal.signo == libc::SIGUSR1 && signal.delivery == SignalDelivery::Handler
        )),
        "no handled SIGUSR1"
    );
}

/// `--access-trace-from=N` leaves out exactly the records whose last event
/// comes before N; the rest are those of the whole trace. With
/// `--access-trace-max-records`, the trace stops after that many and has
/// no end line.
#[test]
fn access_trace_from_leaves_out_only_earlier_records() {
    let _guard = hermit_run_guard();
    let guest = &guests().mix;
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let schedule = record(guest, directory.path());
    let whole = replay(guest, &schedule, directory.path(), "whole", Some(0))
        .trace
        .unwrap();
    let from = 300;
    let part = replay(guest, &schedule, directory.path(), "part", Some(from))
        .trace
        .unwrap();
    let last_event = |record: &TraceRecord| match record {
        TraceRecord::Syscall(call) => call.post.unwrap_or(call.pre),
        TraceRecord::Signal(signal) => signal.event,
        other => panic!("unexpected record {other:?}"),
    };
    assert_eq!(part.header.from, from);
    let kept: Vec<&TraceRecord> = whole
        .records
        .iter()
        .filter(|record| last_event(record) >= from)
        .collect();
    assert!(!kept.is_empty() && kept.len() < whole.records.len());
    assert_eq!(part.records.iter().collect::<Vec<_>>(), kept);
    assert_eq!(part.end.unwrap().events, whole.end.unwrap().events);

    // A cap of five writes the part's first five records and no end line,
    // so a reader sees the trace cut short.
    let capped_path = directory.path().join("capped.access");
    let args = [
        "run".to_string(),
        EPOCH.to_string(),
        "--chaos".to_string(),
        "--sched-seed=3".to_string(),
        path_arg("--replay-schedule-from", &schedule),
        path_arg(
            "--record-preemptions-to",
            &directory.path().join("capped.events"),
        ),
        path_arg("--record-access-trace-to", &capped_path),
        format!("--access-trace-from={from}"),
        "--access-trace-max-records=5".to_string(),
        "--".to_string(),
        guest.display().to_string(),
    ];
    let output = run(&args);
    assert!(output.status.success(), "{args:?}: {output:?}");
    let capped = read_access_trace(fs::read(&capped_path).unwrap().as_slice()).unwrap();
    assert_eq!(capped.records, part.records[..5]);
    assert!(capped.end.is_none(), "{:?}", capped.end);
    // Hermit says once why the trace ends early, naming it and the cap.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("reached --access-trace-max-records=5"))
        .collect();
    assert_eq!(warnings.len(), 1, "{stderr}");
    assert!(
        warnings[0].contains(&capped_path.display().to_string()),
        "{stderr}"
    );
}
