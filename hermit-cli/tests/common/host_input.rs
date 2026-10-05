/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A verify run across a host action between its two runs: the guests and the
//! FIFO-driven harness that name, or refuse to name, a host file changed
//! during one run (see `hermit::host_input_change`). Shared by the ptrace
//! tests in `verify_claim_names_its_limit.rs` and the SaBRe tests in
//! `sabre_examples.rs`.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// The guest: open `F`, wait for the host on `fifo`, print the line it sent,
/// open `F` again, and print the (virtual) inode number of `G`, which it sees
/// last.
pub const HOST_INPUT_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    cat "$1/F" > /dev/null || exit 3
    stat -c %i "$1/G" || exit 3
"#;

/// A guest that replaces `F` itself, in one run only: after the host's line
/// it moves `A` over `F` when the line is `moveA`, and otherwise moves `B` to
/// `H`, leaving `F` alone. Both are one `mv`, which opens no file named here,
/// so both runs make the same opens. Run 1 then finds a new host inode at its
/// second open of `F` where run 2 finds the one it found before, exactly as a
/// host replacement would show. But the runs diverged before that open: they
/// read different lines.
pub const SELF_REPLACING_GUEST: &str = r#"
    cat "$1/F" > /dev/null || exit 3
    read line < "$1/fifo" || exit 3
    echo "$line"
    if [ "$line" = moveA ]; then mv "$1/A" "$1/F"; else mv "$1/B" "$1/H"; fi || exit 3
    cat "$1/F" > /dev/null || exit 3
"#;

/// Open `fifo` for writing once a reader has it open, or panic at `deadline`.
fn open_fifo_writer(fifo: &Path, deadline: std::time::Instant) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo)
        {
            Ok(file) => return file,
            Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the guest never opened {} for reading",
                    fifo.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(error) => panic!("cannot open {}: {error}", fifo.display()),
        }
    }
}

/// Run `guest` ([`HOST_INPUT_GUEST`] or [`SELF_REPLACING_GUEST`]) under
/// `hermit` with `global_args` (such as `--backend=sabre`) and `envs`, as
/// `run --strict --verify --verify-strict`. The host sends `lines[0]` to run
/// 1 and `lines[1]` to run 2; before sending to run 1, it replaces `F` with
/// an identical copy when `replace_in_run1` is set. Returns the input
/// directory, stderr, and the typed report.
pub fn verify_across_host_action(
    hermit: &Path,
    global_args: &[&str],
    envs: &[(&str, &Path)],
    name: &str,
    guest: &str,
    replace_in_run1: bool,
    lines: [&str; 2],
) -> (
    std::path::PathBuf,
    String,
    hermit::canonical_verdict::VerificationReport,
) {
    use std::io::Write;

    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create test directory");
    std::fs::write(root.join("F"), "input\n").expect("write F");
    std::fs::write(root.join("G"), "late\n").expect("write G");
    std::fs::write(root.join("A"), "input\n").expect("write A");
    std::fs::write(root.join("B"), "input\n").expect("write B");
    let fifo = root.join("fifo");
    nix::unistd::mkfifo(
        &fifo,
        nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
    )
    .expect("mkfifo");
    let report_path = root.join("verify.json");
    let stderr_path = root.join("stderr");
    let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr file");
    let child = Command::new("timeout")
        .args(["--kill-after", "5s", "180s"])
        .arg(hermit)
        .args(global_args)
        .args([
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&report_path)
        .args([
            "--base-env=minimal",
            "--mount=type=tmpfs,target=/test",
            "--workdir=/test",
            "--",
            "/bin/sh",
            "-c",
            guest,
            "sh",
        ])
        .arg(&root)
        .envs(envs.iter().copied())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(stderr_file)
        .spawn()
        .expect("failed to start hermit");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(170);

    // Run 1: once its guest waits on the FIFO it has opened F once.
    let mut writer = open_fifo_writer(&fifo, deadline);
    if replace_in_run1 {
        let copy = root.join("F.new");
        std::fs::copy(root.join("F"), &copy).expect("copy F");
        std::fs::rename(&copy, root.join("F")).expect("replace F");
    }
    writeln!(writer, "{}", lines[0]).expect("send run 1 its line");
    drop(writer);

    // Run 2 starts only after run 1 has finished with the FIFO.
    while !std::fs::read_to_string(&stderr_path)
        .unwrap_or_default()
        .contains("Run2")
    {
        assert!(std::time::Instant::now() < deadline, "run 2 never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut writer = open_fifo_writer(&fifo, deadline);
    writeln!(writer, "{}", lines[1]).expect("send run 2 its line");
    drop(writer);

    let output = child.wait_with_output().expect("wait for hermit");
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert_eq!(
        output.status.code(),
        Some(hermit::HERMIT_VERIFICATION_DIVERGENCE_EXIT),
        "a divergence exits with HERMIT_VERIFICATION_DIVERGENCE_EXIT\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    let report = hermit::canonical_verdict::VerificationReport::from_current_json_slice(
        &std::fs::read(&report_path).expect("read verify json"),
    )
    .expect("parse verify json");
    (root, stderr, report)
}
