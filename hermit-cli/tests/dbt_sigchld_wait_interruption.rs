/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Real caught-SIGCHLD WaitChild(B) dependency gates with flags0/SA_RESTART.
//! PTY input establishes the actual parked boundary. These are strict causal
//! observations, not canonical paired evidence; existing canonical gates remain.

#![cfg(feature = "dbt")]

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

enum Line {
    Stdout(String),
    Stderr(String),
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
    input: impl Read,
    output: &Path,
    sender: mpsc::Sender<Line>,
    stdout: bool,
) -> std::io::Result<()> {
    let mut output = fs::File::create(output)?;
    for line in BufReader::new(input).lines() {
        let line = line?;
        writeln!(output, "{line}")?;
        // All lines remain in the file even after the bounded controller stops.
        let _ = sender.send(if stdout {
            Line::Stdout(line)
        } else {
            Line::Stderr(line)
        });
    }
    Ok(())
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
            Line::Stdout(line) if line.starts_with("wait-interruption ready ") => {
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

fn run_wait_interruption(mode: &str) {
    let root = tempfile::Builder::new()
        .prefix(&format!("dbt-wait-interruption-{mode}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("evidence directory")
        .keep();
    eprintln!("DBT wait interruption evidence: {}", root.display());
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let guest = root.join("dbt_sigchld_wait_interruption");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let compiled = Command::new(compiler)
        .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repo.join("tests/c/dbt_sigchld_wait_interruption.c"))
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
        .arg(mode)
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
    let stdout_reader = std::thread::spawn(move || drain(stdout, &stdout_path, stdout_send, true));
    let stderr_reader = std::thread::spawn(move || drain(stderr, &stderr_path, send, false));
    let release = release_when_parked(&receive, &mut input);
    // Always reap the independently bounded process and join log drainers before
    // reporting a predicate failure. No deadline is treated as a valid outcome.
    let status = child.wait().expect("join bounded real guest");
    drop(input);
    fs::write(root.join("hermit.status"), status.to_string()).unwrap();
    stdout_reader
        .join()
        .expect("stdout reader")
        .expect("retain stdout");
    stderr_reader
        .join()
        .expect("stderr reader")
        .expect("retain full DEBUG log");
    let (parent, first, second) =
        release.expect("actual WaitChild dependency boundary not reached");
    let stdout = fs::read_to_string(root.join("guest.stdout")).unwrap();
    let log = fs::read_to_string(root.join("hermit.stderr")).unwrap();
    assert!(
        status.success(),
        "real Detcore DBT guest failed; evidence={}",
        root.display()
    );
    assert_eq!(
        stdout,
        format!(
            "wait-interruption ready parent={parent} first={first} second={second}\nwait-interruption completed {mode} second=47 first=31 handlers=2\n"
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
        hook == 2 && hook == done,
        "two real shared SIGCHLD Tool hooks did not complete"
    );
}

#[test]
fn dbt_caught_sigchld_interrupts_wait_for_other_child() {
    run_wait_interruption("interrupt");
}

#[test]
fn dbt_caught_sigchld_restarts_wait_after_actual_handler() {
    run_wait_interruption("restart");
}
