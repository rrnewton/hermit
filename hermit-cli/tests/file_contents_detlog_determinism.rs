/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A guest that writes its own regular file must produce the same detlog twice.
//!
//! Regression for: the scheduler's `COMMIT` records name the resources a turn
//! holds, and `ResourceID::FileContents` was built from the fd's *host* inode.
//! A freshly created file gets a fresh inode on every run, so two otherwise
//! identical runs differed in exactly one `COMMIT` record.
//!
//! # Why this test compares detlogs directly instead of using `--verify`
//!
//! `--strict --verify` does **not** catch this bug. On the unfixed binary it
//! reports "no substantive differences found. Success: deterministic." — its
//! `Stripped` comparator tolerates the changed inode, which is precisely why
//! the defect survived. A regression test built on `--verify` would be vacuous:
//! it would pass with the bug present. So this test diffs the two `--log-file`
//! outputs itself.
//!
//! # Why the guest is `python3 -c`
//!
//! The four affected sites serve `sendfile`, `pwrite64`, `pwritev` and
//! `pwritev2` — all *positional* writes, so a shell redirect never reaches
//! them. `os.pwrite` issues `pwrite64` directly, which avoids adding a guest
//! binary (and therefore avoids editing the autocargo-generated `tests`
//! manifest) while still exercising the real path.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Creates a regular file and issues a positional write and read against it.
const GUEST_SOURCE: &str = "\
import os
fd = os.open('fc_detlog.dat', os.O_RDWR | os.O_CREAT | os.O_TRUNC, 0o644)
os.pwrite(fd, b'deterministic', 0)
print(os.pread(fd, 13, 0).decode())
os.close(fd)
";

fn python3() -> Option<PathBuf> {
    ["/usr/bin/python3", "/bin/python3"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// Every line carries a real wall-clock prefix, which is nondeterministic by
/// design and is exactly what the repository's `BitwiseInfoV1` policy removes.
/// Strip only that first whitespace-delimited field; compare the whole
/// remainder exactly, so a changed inode still shows up as a difference.
fn without_wall_clock_prefix(log: &str) -> Vec<&str> {
    log.lines()
        .map(|line| line.split_once(' ').map_or(line, |(_, rest)| rest))
        .collect()
}

fn run_capturing_detlog(python: &Path, dir: &Path, log_path: &Path) -> String {
    let _ = fs::remove_file(dir.join("fc_detlog.dat"));
    let output = Command::new("timeout")
        .args(["--kill-after", "5s", "180s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .arg("--log=info")
        .arg("--log-file")
        .arg(log_path)
        .args(["run", "--backend=ptrace", "--strict", "--"])
        .arg(python)
        .arg("-c")
        .arg(GUEST_SOURCE)
        .current_dir(dir)
        .output()
        .expect("failed to launch hermit");
    assert!(
        output.status.success(),
        "guest run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    fs::read_to_string(log_path).expect("hermit wrote no log file")
}

#[test]
fn file_writing_guest_produces_an_identical_detlog_twice() {
    let Some(python) = python3() else {
        eprintln!("skipping: no python3 on this host");
        return;
    };
    // Not under /tmp: hermit replaces the guest's /tmp, so a log written there
    // silently never appears.
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).expect("tempdir");
    let first = run_capturing_detlog(&python, dir.path(), &dir.path().join("run1.log"));
    let second = run_capturing_detlog(&python, dir.path(), &dir.path().join("run2.log"));

    // Anti-vacuity: the runs must actually have exercised the path under test.
    assert!(
        first.contains("FileContents("),
        "guest emitted no FileContents record, so this run proves nothing about \
         the resource identity; the test would pass for the wrong reason"
    );

    let first_lines = without_wall_clock_prefix(&first);
    let second_lines = without_wall_clock_prefix(&second);
    let divergences: Vec<_> = first_lines
        .iter()
        .zip(second_lines.iter())
        .filter(|(a, b)| a != b)
        .take(4)
        .collect();
    assert!(
        divergences.is_empty() && first_lines.len() == second_lines.len(),
        "detlog differs between two identical runs ({} vs {} lines); first \
         divergences: {divergences:#?}\n\
         A raw host inode reaching ResourceID::FileContents is the known cause.",
        first_lines.len(),
        second_lines.len(),
    );
}
