/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! `hermit run --fatal-core-dir`: the ptrace backend keeps a capped,
//! compressed core for each guest process a core-dumping signal kills, without
//! changing what the run reports.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

const MARKER: &[u8] = b"HERMIT-FATAL-CORE-MARKER";
const MIB: u64 = 1 << 20;

/// Builds the guest once per test into its own directory and returns it with
/// that directory.
fn guest(test: &str) -> (PathBuf, PathBuf) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("fatal-core-capture")
        .join(test);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("failed to create the test directory");
    let guest = root.join("fatal_core_capture");
    let output = Command::new("cc")
        .args(["-O1", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/fatal_core_capture.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to start cc");
    assert!(
        output.status.success(),
        "guest compilation failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (guest, root)
}

/// Runs `hermit <global> run <run_args> -- <guest> <guest_args>`.
fn hermit(run_args: &[&str], guest: &Path, guest_args: &[&str]) -> Output {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["run", "--base-env=minimal"])
        .args(run_args)
        .arg("--")
        .arg(guest)
        .args(guest_args);
    command.output().expect("failed to start hermit")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The files in `dir`, by name, with their sizes. An absent directory has none.
fn files(dir: &Path) -> Vec<(String, u64)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .map(|entry| {
            let entry = entry.expect("failed to read the core directory");
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.metadata().expect("failed to stat a core").len())
        })
        .collect();
    files.sort();
    files
}

/// Decompresses a kept core, a sequence of zstd frames, and checks that it is
/// an ELF core file.
fn decode_core(path: &Path) -> Vec<u8> {
    let stored = fs::read(path).expect("failed to read a kept core");
    let mut rest = stored.as_slice();
    let mut core = Vec::new();
    while !rest.is_empty() {
        let mut frame = ruzstd::decoding::StreamingDecoder::new(&mut rest)
            .unwrap_or_else(|error| panic!("{} is not zstd: {error}", path.display()));
        frame
            .read_to_end(&mut core)
            .unwrap_or_else(|error| panic!("{} is not zstd: {error}", path.display()));
    }
    assert_eq!(&core[..4], b"\x7fELF", "{} is not ELF", path.display());
    assert_eq!(
        u16::from_le_bytes([core[16], core[17]]),
        4,
        "{} is not an ET_CORE file",
        path.display()
    );
    core
}

#[test]
fn a_segfault_keeps_one_core_and_reports_what_it_would_without_one() {
    let (guest, root) = guest("segfault");
    let cores = root.join("cores");
    let dir = cores.to_str().unwrap();

    let without = hermit(&[], &guest, &["segv"]);
    let with = hermit(&["--fatal-core-dir", dir], &guest, &["segv"]);

    assert!(
        !without.status.success(),
        "the guest must die:\n{}",
        stderr(&without)
    );
    assert_eq!(
        with.status,
        without.status,
        "capture changed the exit status\nwith:\n{}\nwithout:\n{}",
        stderr(&with),
        stderr(&without)
    );
    assert_eq!(with.stdout, without.stdout);
    assert!(
        !stderr(&without).contains("fatal core"),
        "a run without --fatal-core-dir must keep nothing:\n{}",
        stderr(&without)
    );

    let kept = files(&cores);
    assert_eq!(kept.len(), 1, "expected one core, found {kept:?}");
    let (name, _) = &kept[0];
    assert!(
        name.starts_with("hermit-") && name.ends_with(".SIGSEGV.zst"),
        "unexpected core name {name}"
    );
    assert!(
        stderr(&with).contains(&format!("kept as {name}")),
        "hermit did not report the core:\n{}",
        stderr(&with)
    );
    let core = decode_core(&cores.join(name));
    assert!(
        core.windows(MARKER.len()).any(|window| window == MARKER),
        "the core does not hold the guest's heap"
    );
}

#[test]
fn cores_stay_within_the_per_core_and_total_caps() {
    let (guest, root) = guest("caps");
    let cores = root.join("cores");
    let max_core = 9 * MIB;
    let max_total = 20 * MIB;
    // 36 MiB of incompressible memory: the 12 MiB child cannot fit the
    // per-core cap, and the four together cannot fit the total.
    let children = ["12", "8", "8", "8"];
    let mut guest_args = vec!["children"];
    guest_args.extend(children);

    let output = hermit(
        &[
            "--fatal-core-dir",
            cores.to_str().unwrap(),
            "--fatal-core-max-bytes",
            &max_core.to_string(),
            "--fatal-core-total-max-bytes",
            &max_total.to_string(),
        ],
        &guest,
        &guest_args,
    );

    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    let kept = files(&cores);
    assert_eq!(
        kept.len(),
        children.len(),
        "every child should keep a core, if only a smaller one: {kept:?}\n{}",
        stderr(&output)
    );
    for (name, bytes) in &kept {
        assert!(*bytes <= max_core, "{name} is {bytes} bytes");
        decode_core(&cores.join(name));
    }
    let total: u64 = kept.iter().map(|(_, bytes)| bytes).sum();
    assert!(total <= max_total, "the cores take {total} bytes: {kept:?}");
    assert!(
        stderr(&output).contains(", Full)") && !stderr(&output).contains("not kept"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn verify_parity_is_unchanged_with_capture_on() {
    let (guest, root) = guest("verify");
    let cores = root.join("cores");
    let mut reports = Vec::new();
    for capture in [false, true] {
        let json = root.join(format!("verify-{capture}.json"));
        let mut args = vec![
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            json.to_str().unwrap(),
        ];
        if capture {
            args.extend(["--fatal-core-dir", cores.to_str().unwrap()]);
        }
        let output = hermit(&args, &guest, &["child-segv-ok"]);
        assert!(output.status.success(), "{}", stderr(&output));
        if capture {
            for run in ["run1", "run2"] {
                assert!(
                    stderr(&output).contains(&format!("-{run}-core.")),
                    "{run} kept no core, so this compares nothing:\n{}",
                    stderr(&output)
                );
            }
        }
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&json).expect("no --verify-json report"))
                .expect("--verify-json is not JSON");
        assert_eq!(report["bitwise_parity"], true, "{report:#}");
        assert_eq!(report["verdict"], "matched", "{report:#}");
        reports.push(report);
    }
    for field in ["compared_log_messages", "compared_outputs"] {
        assert_eq!(
            reports[0][field], reports[1][field],
            "capture changed {field}"
        );
    }
    assert_eq!(
        files(&cores),
        Vec::new(),
        "a matched --verify keeps nothing"
    );
}

#[test]
fn a_green_run_keeps_nothing() {
    let (guest, root) = guest("green");
    let cores = root.join("cores");
    let output = hermit(
        &["--fatal-core-dir", cores.to_str().unwrap()],
        &guest,
        &["child-segv-ok"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("kept as") && stderr(&output).contains("were removed"),
        "the child's core was never written, so nothing was tested:\n{}",
        stderr(&output)
    );
    assert_eq!(files(&cores), Vec::new());
}

#[test]
fn backends_without_the_ptrace_exit_stop_are_refused() {
    let (guest, root) = guest("refused");
    let cores = root.join("cores");
    let output = hermit(
        &[
            "--namespace-only",
            "--fatal-core-dir",
            cores.to_str().unwrap(),
        ],
        &guest,
        &["child-segv-ok"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--fatal-core-dir"),
        "{}",
        stderr(&output)
    );
    assert!(!cores.exists(), "a refused run created the core directory");
}
