/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One real Detcore<DbtGuest> strict progress gate. DEBUG transaction records
//! authenticate the exercised integration; this is not repeat-parity evidence.
//! The original fork-tree strict verifier remains the determinism regression.

#![cfg(feature = "dbt")]

use std::fs;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

enum Line {
    Stdout(String),
    Stderr(String),
}

#[derive(Debug)]
struct DrainOutcome {
    reached_eof: bool,
    retained_bytes: usize,
    pending_line_bytes: usize,
}

fn streaming_stdin() -> (fs::File, fs::File) {
    // DBT's maintained nonterminal CLI path captures Output until exit. A raw
    // PTY selects its existing status_with_global path so the ready marker and
    // scheduler records remain observable while the real parent is parked.
    // No controlling terminal, extra guest task, or helper OFD is introduced.
    let mut master = -1;
    let mut slave = -1;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(result, 0, "create fixture input PTY");
    let master = unsafe { fs::File::from_raw_fd(master) };
    let slave = unsafe { fs::File::from_raw_fd(slave) };
    let mut settings = unsafe { std::mem::zeroed::<libc::termios>() };
    assert_eq!(
        unsafe { libc::tcgetattr(slave.as_raw_fd(), &mut settings) },
        0
    );
    unsafe { libc::cfmakeraw(&mut settings) };
    assert_eq!(
        unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &settings) },
        0
    );
    // Unlike File::open, openpty does not promise CLOEXEC. Only the explicitly
    // configured stdin slave may cross exec; the host controller owns master.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
            0
        );
    }
    (master, slave)
}

fn drain(
    mut input: impl Read + AsRawFd,
    output: &Path,
    sender: mpsc::Sender<Line>,
    stdout: bool,
    stop: Arc<AtomicBool>,
) -> std::io::Result<DrainOutcome> {
    // A DBT process can outlive the independently reaped launcher and retain
    // its stdout/stderr writers. Never wait for its EOF without a bound. This
    // changes only the host's reader: owned-process cleanup belongs to the
    // enclosing containment, and an incomplete stream cannot pass the test.
    let fd = input.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut output = fs::File::create(output)?;
    let mut pending = Vec::new();
    let mut retained_bytes = 0;
    let mut stop_at = None;
    let send_line = |mut bytes: Vec<u8>| -> std::io::Result<()> {
        // Match BufRead::lines for controller messages; the evidence file
        // separately retains the literal bytes, including a partial last line.
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        let line = String::from_utf8(bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let _ = sender.send(if stdout {
            Line::Stdout(line)
        } else {
            Line::Stderr(line)
        });
        Ok(())
    };
    let stopped = |retained_bytes, pending_line_bytes| DrainOutcome {
        reached_eof: false,
        retained_bytes,
        pending_line_bytes,
    };
    let mut buffer = [0u8; 8192];
    loop {
        if false && stop.load(Ordering::Acquire) && stop_at.is_none() {
            stop_at = Some(Instant::now() + Duration::from_secs(1));
        }
        // Even a surviving writer producing continuously cannot keep the
        // reader alive forever after the launcher has been reaped.
        if stop_at.is_some_and(|deadline| Instant::now() >= deadline) {
            return Ok(stopped(retained_bytes, pending.len()));
        }
        match input.read(&mut buffer) {
            Ok(0) => {
                if !pending.is_empty() {
                    send_line(std::mem::take(&mut pending))?;
                }
                return Ok(DrainOutcome {
                    reached_eof: true,
                    retained_bytes,
                    pending_line_bytes: 0,
                });
            }
            Ok(length) => {
                output.write_all(&buffer[..length])?;
                retained_bytes += length;
                pending.extend_from_slice(&buffer[..length]);
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    send_line(pending.drain(..=newline).collect())?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop_at.is_some() {
                    // Retained all bytes currently readable, but did not get
                    // EOF. Do not manufacture a completed controller line.
                    return Ok(stopped(retained_bytes, pending.len()));
                }
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN | libc::POLLHUP,
                    revents: 0,
                };
                let result = unsafe { libc::poll(&mut pollfd, 1, 100) };
                if result < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                } else if pollfd.revents & libc::POLLNVAL != 0 {
                    return Err(std::io::Error::from_raw_os_error(libc::EBADF));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn finish_guest_readers(
    root: &Path,
    status: std::process::ExitStatus,
    controller: &str,
    stop: Arc<AtomicBool>,
    readers: [std::thread::JoinHandle<std::io::Result<DrainOutcome>>; 2],
) {
    let [stdout_reader, stderr_reader] = readers;
    stop.store(true, Ordering::Release);
    // Record errors without unwinding past a still-owned reader. Both joins
    // precede every status-write or reader-result assertion.
    let status_record = fs::write(root.join("hermit.status"), status.to_string());
    let controller_record = fs::write(root.join("controller.status"), format!("{controller}\n"));
    let stdout_result = stdout_reader.join();
    let stderr_result = stderr_reader.join();
    let describe = |result: &std::thread::Result<std::io::Result<DrainOutcome>>| match result {
        Ok(Ok(outcome)) => format!(
            "reached_eof={} retained_bytes={} pending_line_bytes={}",
            outcome.reached_eof, outcome.retained_bytes, outcome.pending_line_bytes,
        ),
        Ok(Err(error)) => format!("read_error={error:?}"),
        Err(_) => "reader_panicked".to_owned(),
    };
    let reader_record = fs::write(
        root.join("reader.status"),
        format!(
            "stdout={}\nstderr={}\n",
            describe(&stdout_result),
            describe(&stderr_result),
        ),
    );
    status_record.expect("retain guest status");
    controller_record.expect("retain controller status");
    reader_record.expect("retain both reader results");
    assert!(
        status.success(),
        "real Detcore DBT guest failed: status={status}, controller={controller}; evidence={}",
        root.display()
    );
    let stdout_end = stdout_result
        .expect("stdout reader")
        .expect("retain stdout");
    let stderr_end = stderr_result
        .expect("stderr reader")
        .expect("retain full DEBUG log");
    assert!(
        stdout_end.reached_eof && stderr_end.reached_eof,
        "real guest output incomplete: stdout={stdout_end:?}, stderr={stderr_end:?}; evidence={}",
        root.display()
    );
}

#[test]
fn owned_publication_reader_stops_without_eof_and_retains_partial_bytes() {
    let root = tempfile::tempdir().unwrap();
    let pipe = || {
        let mut descriptors = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        unsafe {
            (
                fs::File::from_raw_fd(descriptors[0]),
                fs::File::from_raw_fd(descriptors[1]),
            )
        }
    };
    let (stdout, mut stdout_writer) = pipe();
    let (stderr, mut stderr_writer) = pipe();
    let stdout_bytes = b"complete\r\npartial";
    let stderr_bytes = b"diagnostic\npartial-diagnostic";
    stdout_writer.write_all(stdout_bytes).unwrap();
    stderr_writer.write_all(stderr_bytes).unwrap();
    // The independently bounded native launcher really times out. Its child
    // exits before wait returns; only these authenticated host-owned writer
    // handles keep EOF unavailable. No Detcore/backend execution is involved.
    let status = Command::new("timeout")
        .args(["0.1s", "sleep", "10"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_writer.try_clone().unwrap()))
        .stderr(Stdio::from(stderr_writer.try_clone().unwrap()))
        .status()
        .expect("run bounded native timeout control");
    assert_eq!(status.code(), Some(124), "native timeout control setup");
    let stop = Arc::new(AtomicBool::new(false));
    let stdout_stop = stop.clone();
    let stderr_stop = stop.clone();
    let (send, receive) = mpsc::channel();
    let stdout_send = send.clone();
    let stdout_path = root.path().join("guest.stdout");
    let stderr_path = root.path().join("hermit.stderr");
    let stdout_reader =
        std::thread::spawn(move || drain(stdout, &stdout_path, stdout_send, true, stdout_stop));
    let stderr_reader =
        std::thread::spawn(move || drain(stderr, &stderr_path, send, false, stderr_stop));
    let (finished, completion) = mpsc::channel();
    let retained = root.path().to_owned();
    let finisher = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finish_guest_readers(
                &retained,
                status,
                "native-timeout-holder",
                stop,
                [stdout_reader, stderr_reader],
            );
        }));
        let _ = finished.send(result);
    });
    // This is a hang backstop for unavailable EOF, not a performance bound.
    // The production reader's stop horizon remains one second.
    let result_while_writers_open = completion.recv_timeout(Duration::from_secs(10));
    // The failing-first inverse ignores only stop. It must miss this result
    // deadline, but dropping our own holders still lets both readers be joined.
    drop(stdout_writer);
    drop(stderr_writer);
    finisher.join().expect("join output finisher");
    let failure = result_while_writers_open
        .expect("reader required inherited writer EOF after native timeout")
        .expect_err("native timeout was accepted as guest success");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .expect("guest failure panic text");
    assert!(
        message.starts_with(&format!("real Detcore DBT guest failed: status={status},")),
        "setup/read error replaced original launcher timeout: {message}",
    );
    assert_eq!(
        fs::read_to_string(root.path().join("hermit.status")).unwrap(),
        status.to_string()
    );
    assert_eq!(
        fs::read(root.path().join("guest.stdout")).unwrap(),
        stdout_bytes
    );
    assert_eq!(
        fs::read(root.path().join("hermit.stderr")).unwrap(),
        stderr_bytes
    );
    let readers = fs::read_to_string(root.path().join("reader.status")).unwrap();
    assert_eq!(readers.matches("reached_eof=false").count(), 2, "{readers}");
    assert!(readers.contains("pending_line_bytes=7"), "{readers}");
    assert!(readers.contains("pending_line_bytes=18"), "{readers}");
    let lines = receive.try_iter().collect::<Vec<_>>();
    assert_eq!(
        lines.len(),
        2,
        "partial line fabricated as controller progress"
    );
    assert!(
        lines
            .iter()
            .any(|line| matches!(line, Line::Stdout(text) if text == "complete"))
    );
    assert!(
        lines
            .iter()
            .any(|line| matches!(line, Line::Stderr(text) if text == "diagnostic"))
    );
}

fn number(line: &str, name: &str) -> Option<u64> {
    line.split_whitespace()
        .find_map(|field| field.strip_prefix(name))?
        .parse()
        .ok()
}

fn release_when_parked(
    receiver: &mpsc::Receiver<Line>,
    input: &mut impl Write,
) -> Result<(u64, u64, u64), String> {
    let until = Instant::now() + Duration::from_secs(30);
    let mut identities = None;
    let mut scheduler = Vec::new();
    loop {
        let remaining = until
            .checked_duration_since(Instant::now())
            .ok_or("parent did not park within 30 seconds")?;
        match receiver
            .recv_timeout(remaining)
            .map_err(|e| e.to_string())?
        {
            Line::Stdout(line) if line.starts_with("wait-later-child ready ") => {
                let ids = (
                    number(&line, "parent=").ok_or("parent identity missing")?,
                    number(&line, "first=").ok_or("first identity missing")?,
                    number(&line, "second=").ok_or("second identity missing")?,
                );
                if identities.replace(ids).is_some() {
                    return Err("duplicate ready marker".to_owned());
                }
            }
            Line::Stderr(line) if line.contains("parking dettid ") => scheduler.push(line),
            _ => {}
        }
        if let Some((parent, first, second)) = identities {
            let parked = format!(
                "parking dettid {parent} for child ChildWaitSpec {{ selector: Exact(DetPid({second}))"
            );
            if scheduler.iter().any(|line| line.contains(&parked)) {
                if parent == 0
                    || first == 0
                    || second == 0
                    || first == second
                    || parent == first
                    || parent == second
                {
                    return Err("invalid logical process identities".to_owned());
                }
                input.write_all(b"A").map_err(|e| e.to_string())?;
                return Ok((parent, first, second));
            }
        }
    }
}

fn transaction(log: &str, parent: u64, child: u64) -> (usize, usize) {
    let rows: Vec<_> = log
        .lines()
        .enumerate()
        .filter(|(_, line)| {
            line.contains("[child-exit-capture]")
                && number(line, "parent=") == Some(parent)
                && number(line, "child=") == Some(child)
        })
        .collect();
    let mut selected = Vec::new();
    for phase in ["reserved", "armed", "captured", "released"] {
        let found: Vec<_> = rows
            .iter()
            .filter(|(_, line)| line.contains(&format!("[child-exit-capture] {phase} ")))
            .collect();
        assert_eq!(
            found.len(),
            1,
            "expected one genuine {phase} for parent {parent}/child {child}: {rows:?}"
        );
        selected.push(*found[0]);
    }
    let reservation = number(selected[0].1, "reservation=").expect("reservation identity");
    assert_ne!(reservation, 0);
    assert!(
        selected.windows(2).all(|pair| pair[0].0 < pair[1].0),
        "transaction order changed: {selected:?}"
    );
    assert!(
        selected
            .iter()
            .all(|(_, line)| number(line, "reservation=") == Some(reservation)),
        "reservation changed"
    );
    assert_eq!(
        rows.len(),
        4,
        "unsupported/failed/duplicate transition cannot count as capture: {rows:?}"
    );
    let captured = selected[2].1;
    for field in ["domain=", "interval=", "allocation="] {
        assert!(
            number(captured, field).is_some_and(|value| value != 0),
            "capture omitted {field}: {captured}"
        );
    }
    // Each positive effect names the unchanged ordinary queue policy. This
    // fixture asserts progress/ownership, not one callback per process exit.
    assert!(
        ["Linked", "Coalesced", "Replaced"]
            .iter()
            .any(|effect| captured
                .split_whitespace()
                .any(|field| field == format!("effect={effect}"))),
        "capture lacked a typed authentic queue effect: {captured}"
    );
    (selected[0].0, selected[3].0)
}

#[test]
fn dbt_wait_later_child_captures_dependency_exit_without_parent_guest_progress() {
    let root = tempfile::Builder::new()
        .prefix("dbt-owned-publication-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("evidence directory")
        .keep();
    eprintln!("DBT owned publication evidence: {}", root.display());
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let guest = root.join("dbt_sigchld_wait_later_child");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let compiled = Command::new(compiler)
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repo.join("tests/c/dbt_sigchld_wait_later_child.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("compile real guest");
    fs::write(root.join("compile.stdout"), &compiled.stdout).unwrap();
    fs::write(root.join("compile.stderr"), &compiled.stderr).unwrap();
    assert!(
        compiled.status.success(),
        "guest compilation failed: {compiled:?}"
    );
    let launcher = root.join("bounded-launcher");
    fs::write(
        &launcher,
        r#"#!/usr/bin/env python3
import os, signal, subprocess, sys
child = subprocess.Popen(sys.argv[1:], start_new_session=True)
try:
    try:
        result = child.wait(timeout=30)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        result = 124
finally:
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    child.wait()
sys.exit(result)
"#,
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
    let (mut input, terminal) = streaming_stdin();
    let mut child = Command::new(launcher)
        .arg(hermit_binary::hermit_binary())
        .args([
            "--backend=dbt",
            "--log=debug",
            "run",
            "--strict",
            "--base-env=minimal",
            "--epoch=2026-10-12T07:49:05+00:00",
            "--",
        ])
        .arg(&guest)
        .env_remove("RUST_LOG")
        .env_remove("HERMIT_LOG")
        .env_remove("HERMIT_LOG_FILE")
        .stdin(Stdio::from(terminal))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch actual Detcore DBT");
    let (send, receive) = mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_path = root.join("guest.stdout");
    let stderr_path = root.join("hermit.stderr");
    let stdout_send = send.clone();
    let stop_readers = Arc::new(AtomicBool::new(false));
    let stdout_stop = stop_readers.clone();
    let stderr_stop = stop_readers.clone();
    let stdout_reader =
        std::thread::spawn(move || drain(stdout, &stdout_path, stdout_send, true, stdout_stop));
    let stderr_reader =
        std::thread::spawn(move || drain(stderr, &stderr_path, send, false, stderr_stop));
    let release = release_when_parked(&receive, &mut input);
    // Always reap the independently bounded process and join log drainers before
    // reporting a predicate failure. No deadline is treated as a valid outcome.
    let status = child.wait().expect("join bounded real guest");
    drop(input);
    finish_guest_readers(
        &root,
        status,
        &format!("{release:?}"),
        stop_readers,
        [stdout_reader, stderr_reader],
    );
    let (parent, first, second) =
        release.expect("actual WaitChild dependency boundary not reached");
    let stdout = fs::read_to_string(root.join("guest.stdout")).unwrap();
    let log = fs::read_to_string(root.join("hermit.stderr")).unwrap();
    assert_eq!(
        stdout,
        format!(
            "wait-later-child ready parent={parent} first={first} second={second}\nwait-later-child completed second=47 first=31\n"
        )
    );
    let a = transaction(&log, parent, first);
    let b = transaction(&log, parent, second);
    assert!(
        a.1 < b.0,
        "first native capture/release must permit the dependency's later Exit"
    );
    let park = log.lines().position(|line| line.contains(&format!("parking dettid {parent} for child ChildWaitSpec {{ selector: Exact(DetPid({second}))")))
        .expect("retained real WaitChild park");
    assert!(
        park < a.0,
        "first Exit was not captured while parent waited on later child"
    );
    let hook = log
        .lines()
        .filter(|line| {
            line.contains(&format!("dtid {parent}] handling inbound signal (#"))
                && line.contains(") SIGCHLD")
        })
        .count();
    let done = log
        .lines()
        .filter(|line| {
            line.contains(&format!("dtid {parent}] finish delivering signal (#"))
                && line.contains(") SIGCHLD")
        })
        .count();
    assert!(
        hook > 0 && hook == done,
        "real shared default-signal Tool hooks did not complete"
    );
}
