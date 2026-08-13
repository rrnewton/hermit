/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for a blocking `waitid` that never returned and SPUN.
//!
//! WHY THIS TEST IS SHAPED THE WAY IT IS. The defect is an unbounded polling
//! loop, so the naive test does not FAIL when the bug returns -- it HANGS, and a
//! hang teaches a future reader nothing except that CI was slow. An in-process
//! test cannot rescue itself either: under the defect the polling thread stays
//! perpetually runnable, so an in-guest watchdog thread would be starved by the
//! very spin it is meant to detect.
//!
//! So the diagnosis here is a COUNT, not a clock. Detcore emits one
//! `Retry #<n> for waitid` line per poll. The fixed implementation performs
//! exactly 1999 of them, which is also what `wait4` -- the known-good reference
//! implementation of the same behaviour, sitting beside it in the same file --
//! performs. The regression performs them without bound: 312,885 in a 30 second
//! trace, still climbing, never terminating. Those two populations are four
//! orders of magnitude apart, so a threshold between them is not a tuning
//! exercise.
//!
//! The retry count is a LOGICAL quantity. It does not move when the box is
//! loaded, which matters because devbig030 is shared and a wall-clock deadline
//! false-reds under load. The elapsed-time limit below is a BACKSTOP ONLY, and
//! it reports itself as such so that a slow-box false red is never mistaken for
//! the defect.

use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// The fixed implementation performs 1999 retries, matching `wait4`. Ten times
/// that is far above any legitimate run and is reached by the regression in
/// roughly two seconds, so the test fails fast and for the right reason.
const MAX_RETRIES: usize = 20_000;

/// Pure backstop, sized about sixty times the observed 1.01s traced run. Only
/// fires if the retry stream stalls without either finishing or spinning.
const BACKSTOP: Duration = Duration::from_secs(60);

#[test]
fn waitid_interrupted_by_a_signal_returns_instead_of_spinning() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("waitid-signal-interrupt");
    fs::create_dir_all(&build_root).expect("failed to create waitid guest build directory");

    // The guest must not live under /tmp: Hermit replaces guest /tmp with an
    // isolated directory and refuses to run a program from the host's.
    let guest = build_root.join("waitid_signal_interrupt");
    let source = repository.join("tests/c/waitid_signal_interrupt.c");
    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile the waitid guest: {error}"));
    assert!(
        compile.status.success(),
        "failed to compile {}\nstderr:\n{}",
        source.display(),
        String::from_utf8_lossy(&compile.stderr),
    );

    let stdout_path = build_root.join("guest.stdout");
    let stdout_file = File::create(&stdout_path).expect("failed to create guest stdout file");

    // Scope tracing to the module that emits the retry line. Full `--log=trace`
    // produces 17 MB for the passing case and would grow without bound for the
    // regression; scoped, the same run is 234 KB.
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .env("RUST_LOG", "detcore::syscalls::threads=trace")
        .args(["run", "--backend=ptrace", "--strict", "--"])
        .arg(&guest)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn hermit: {error}"));

    let stderr = child.stderr.take().expect("stderr was piped");
    let started = Instant::now();
    let mut retries = 0usize;
    let mut overrun = None;

    for line in BufReader::new(stderr).lines() {
        let Ok(line) = line else { break };
        if line.contains("Retry #") && line.contains("waitid") {
            retries += 1;
            if retries > MAX_RETRIES {
                overrun = Some(format!(
                    "waitid retried more than {MAX_RETRIES} times without returning. The fixed \
                     implementation performs 1999 retries, the same as wait4 beside it; the \
                     regression performs them without bound because the polling loop is never \
                     presented to the scheduler as a blocking request, so every turn is a \
                     SkipTurn and logical time never reaches the pending timer."
                ));
                break;
            }
        }
        if started.elapsed() > BACKSTOP {
            overrun = Some(format!(
                "BACKSTOP: hermit did not finish within {}s after {retries} waitid retries. This \
                 is the wall-clock limit rather than the retry threshold, so on a loaded shared \
                 machine treat it as inconclusive and re-run before reading it as the spin \
                 regression, which instead exceeds {MAX_RETRIES} retries within seconds.",
                BACKSTOP.as_secs(),
            ));
            break;
        }
    }

    if let Some(reason) = overrun {
        let _ = child.kill();
        let _ = child.wait();
        panic!("{reason}");
    }

    let status = child.wait().expect("failed to wait for hermit");
    let guest_stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");

    assert!(
        status.success(),
        "hermit exited with {status}\nguest stdout:\n{guest_stdout}",
    );

    // Linux returns EINTR and RUNS THE HANDLER. Asserting the return value alone
    // would not distinguish a delivered signal from the wait ending some other
    // way, which is the distinction the regression turns on.
    assert!(
        guest_stdout.contains("waitid-signal-interrupt rc=-1 errno=4 handler=1"),
        "waitid did not report EINTR with its handler run\nguest stdout:\n{guest_stdout}",
    );
    assert!(
        guest_stdout.contains("waitid-signal-interrupt-done"),
        "the guest did not reach its child cleanup\nguest stdout:\n{guest_stdout}",
    );

    // Returning is necessary but not sufficient: a future change could return
    // correctly while still burning a core on the way.
    assert!(
        retries <= MAX_RETRIES,
        "waitid returned correctly but performed {retries} retries, far above the 1999 that the \
         fixed implementation and wait4 both perform",
    );
}
