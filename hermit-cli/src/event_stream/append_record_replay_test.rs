/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Guest integration control, selected by the official test.hermit_unit node.
//! It deliberately belongs beside the private recording reader so it consumes
//! the production event types without exporting or copying their schema.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::BufRead;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallInfo;
use serde_json::json;

use super::EventReader;
use super::EventStreamId;
use crate::event::ReplayFdKind;
use crate::event::SyscallEvent;

const PREFIX: &[u8] = b"prefix\n";
const EXPECTED: &[u8] = b"nrefix\nWVvPQq";

fn flags(file: &File) -> i32 {
    // SAFETY: the File owns the live descriptor throughout this call.
    let value = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(value >= 0, "cannot inspect held output flags");
    value
}

fn prepared_cli(repository: &Path, evidence: &Path) -> PathBuf {
    // Consume the existing verified artifact, independently of Cargo's internal
    // test-executable layout. Keep both this CLI and its install tree in the
    // runtime binding; verification failure is a setup failure, never coverage.
    let verifier = repository.join("ci/verify-hermit-e2e-artifact.sh");
    let pointer = repository.join("target/ci/hermit-e2e-artifact.path");
    let mut command = timed(&verifier, 10);
    command.arg(&pointer).current_dir(repository);
    let argv = std::iter::once(command.get_program().to_os_string())
        .chain(command.get_args().map(|arg| arg.to_os_string()))
        .collect::<Vec<_>>();
    let output = command.output().expect("run bounded artifact verifier");
    fs::write(
        evidence.join("artifact-verification.stdout"),
        &output.stdout,
    )
    .unwrap();
    fs::write(
        evidence.join("artifact-verification.stderr"),
        &output.stderr,
    )
    .unwrap();
    fs::write(
        evidence.join("artifact-verification.json"),
        serde_json::to_vec_pretty(&json!({
            "argv": argv, "cwd": repository, "pointer": pointer,
            "exit_code": output.status.code(), "signal": output.status.signal(),
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "published artifact verification failed"
    );
    let stdout = String::from_utf8(output.stdout).expect("artifact path is UTF-8");
    let bundle = stdout
        .strip_suffix('\n')
        .expect("one terminated artifact path");
    assert!(!bundle.is_empty() && !bundle.contains('\n'));
    let bundle = Path::new(bundle);
    assert!(bundle.is_absolute());
    let binary = bundle.join("hermit");
    let install = bundle.join("install");
    assert!(
        binary.is_file() && install.is_dir(),
        "complete artifact required"
    );
    assert_eq!(
        fs::read_to_string(bundle.join("kind")).unwrap(),
        "complete\n"
    );
    fs::write(
        evidence.join("artifact-bundle.json"),
        serde_json::to_vec_pretty(&json!({
            "bundle": bundle, "binary": binary, "install": install,
            "verified_binary_sha256": fs::read_to_string(bundle.join("hermit.sha256")).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    binary
}

fn timed(program: &Path, seconds: u32) -> Command {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=2s", &format!("{seconds}s")])
        .arg(program);
    command
}

fn initial_output(path: &Path) -> File {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    file.write_all(PREFIX).unwrap();
    file.seek(SeekFrom::Start(2)).unwrap();
    assert_eq!(flags(&file) & (libc::O_APPEND | libc::O_NONBLOCK), 0);
    file
}

fn phase(root: &Path, name: &str, mut command: Command, native: bool) {
    let output_path = root.join(format!("{name}.output"));
    let diagnostic_path = root.join(format!("{name}.stderr"));
    let mut output = initial_output(&output_path);
    let diagnostic = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&diagnostic_path)
        .unwrap();
    let output_flags = flags(&output);
    let diagnostic_flags = flags(&diagnostic);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(diagnostic.try_clone().unwrap()));
    let argv = std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let status = command
        .status()
        .expect("launch bounded official-node child");
    let actual_flags = flags(&output);
    let actual_diagnostic_flags = flags(&diagnostic);
    let position = output.stream_position().unwrap();
    let bytes = fs::read(&output_path).unwrap();
    // Persist the real terminal status and effects before any result assertion.
    fs::write(
        root.join(format!("{name}.json")),
        serde_json::to_vec_pretty(&json!({
            "argv": argv, "exit_code": status.code(), "signal": status.signal(),
            "output": output_path, "stderr": diagnostic_path,
            "output_flags_before": output_flags, "output_flags_after": actual_flags,
            "stderr_flags_before": diagnostic_flags, "stderr_flags_after": actual_diagnostic_flags,
            "shared_ofd_position": position, "file_bytes": bytes,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        status.code(),
        Some(37),
        "{name} did not preserve guest exit 37; artifacts: {}",
        root.display()
    );
    assert_eq!(bytes, EXPECTED, "{name} append/positioned output bytes");
    assert_eq!(
        position, 10,
        "{name} shared OFD offset (including zero write)"
    );
    assert_eq!(
        actual_flags,
        if native {
            output_flags | libc::O_APPEND
        } else {
            output_flags
        },
        "{name} host stdout flags"
    );
    assert_eq!(
        actual_diagnostic_flags, diagnostic_flags,
        "{name} host stderr flags"
    );
}

fn assert_recorded_effects(recording: &Path, output: &Path) {
    let mut reader = EventReader::open(recording, &EventStreamId::root()).unwrap();
    let mut writes = Vec::new();
    // Check EOF before decoding: a truncated final event must be an error, not
    // an end-of-stream success. Read the complete stream using the real types.
    while !reader.reader.fill_buf().unwrap().is_empty() {
        let event = reader.next_event().expect("decode complete recorded event");
        if let Ok(SyscallEvent::WriteV2(write)) = event.event {
            assert_eq!(
                write.output_fd,
                Some(libc::STDOUT_FILENO),
                "unexpected captured write"
            );
            writes.push(write);
        }
    }
    let mut calls = Vec::new();
    let mut raw_calls = Vec::new();
    while !reader.debug_events.fill_buf().unwrap().is_empty() {
        let debug = reader
            .next_debug_event()
            .expect("decode complete debug event");
        let syscall = debug.syscall();
        if matches!(
            syscall,
            Syscall::Write(_)
                | Syscall::Writev(_)
                | Syscall::Pwrite64(_)
                | Syscall::Pwritev(_)
                | Syscall::Pwritev2(_)
        ) {
            raw_calls.push(syscall.into_parts());
            let Syscall::Pwritev2(call) = syscall else {
                panic!("append recording retained an unconverted write: {syscall:?}");
            };
            assert_eq!(call.fd(), libc::STDOUT_FILENO);
            calls.push((call.iov_len(), call.pos_l(), call.pos_h(), call.flags()));
        }
    }
    fs::write(
        output.join("recorded-effects.json"),
        serde_json::to_vec_pretty(&json!({
            "writes": writes, "raw_calls": raw_calls, "converted_arguments": calls,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        writes.len(),
        6,
        "all six actual WriteV2 events must be present"
    );
    assert_eq!(
        calls,
        vec![
            (2, u64::MAX, 0, libc::RWF_APPEND),
            (2, u64::MAX, 0, libc::RWF_APPEND),
            (2, 2, 0, libc::RWF_APPEND),
            (2, 1, 0, libc::RWF_APPEND),
            (1, 0, 0, libc::RWF_NOAPPEND),
            (2, u64::MAX, 0, libc::RWF_APPEND),
        ],
        "recorded pwritev2 ABI must preserve scalar/vector and OFD-position semantics"
    );
    for (write, (count, offset, advances)) in writes.iter().zip([
        (1, 7, true),
        (2, 8, true),
        (1, 10, false),
        (2, 11, false),
        (1, 0, false),
        (0, 13, true),
    ]) {
        assert_eq!(write.result, Ok(count));
        assert_eq!(write.output_offset, Some(offset));
        assert_eq!(write.advances_output_offset, advances);
        assert!(!write.generated_sigpipe);
        assert_eq!(write.replay_fd_kind, ReplayFdKind::None);
        assert_eq!(write.replay_file_offset, None);
        assert!(!write.replay_file_advances_offset);
    }
}

#[test]
fn inherited_append_record_replay_preserves_recorded_offsets_and_ofd() {
    let base = std::env::var_os("VALIDATE_RUN_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&base).unwrap();
    // Keep success and failure evidence; each calling test owns a unique path.
    let root = tempfile::Builder::new()
        .prefix("append-record-replay-")
        .tempdir_in(base)
        .unwrap()
        .keep();
    eprintln!("append record/replay artifacts: {}", root.display());
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let guest = root.join("append-guest");
    let mut compiler = timed(Path::new("cc"), 10);
    compiler
        .args(["-O0", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/stdio_append_record_replay.c"))
        .arg("-o")
        .arg(&guest);
    let compiled = compiler.output().expect("compile append fixture");
    fs::write(root.join("compile.stdout"), &compiled.stdout).unwrap();
    fs::write(root.join("compile.stderr"), &compiled.stderr).unwrap();
    fs::write(
        root.join("compile.json"),
        serde_json::to_vec_pretty(
            &json!({"status": compiled.status.code(), "signal": compiled.status.signal()}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        compiled.status.success(),
        "fixture compilation failed: {}",
        root.display()
    );
    phase(&root, "native", timed(&guest, 5), true);

    let cli = prepared_cli(repository, &root);
    let data = root.join("recordings");
    let mut record = timed(&cli, 15);
    record.env("HERMIT_INSTALL_DIR", cli.parent().unwrap().join("install"));
    record
        .args(["--backend=ptrace", "--log=info", "--log-file"])
        .arg(root.join("record.log"))
        .args([
            "record",
            "start",
            "--strict",
            "--record-timeout=10",
            "--base-env=minimal",
            "--data-dir",
        ])
        .arg(&data);
    if let Some(marker) = std::env::var_os("HERMIT_E2E_EMPTY_WORKDIR") {
        assert_eq!(
            marker,
            std::ffi::OsString::from("/test"),
            "invalid official workdir marker"
        );
        record.args(["--mount=type=tmpfs,target=/test", "--workdir=/test"]);
    }
    record.arg("--").arg(&guest);
    phase(&root, "record", record, false);
    let hermit_data = crate::HermitData::from(Some(&data));
    let id = hermit_data
        .last_id()
        .expect("recording must publish its actual ID");
    assert_recorded_effects(&data.join(id.to_string()), &root);

    let mut replay = timed(&cli, 15);
    replay.env("HERMIT_INSTALL_DIR", cli.parent().unwrap().join("install"));
    replay
        .args(["--backend=ptrace", "--log=info", "--log-file"])
        .arg(root.join("replay.log"))
        .args(["replay", "--autopilot", "--data-dir"])
        .arg(&data)
        .arg(id.to_string());
    phase(&root, "replay", replay, false);
    assert_eq!(
        fs::read(root.join("record.output")).unwrap(),
        fs::read(root.join("replay.output")).unwrap()
    );
}
