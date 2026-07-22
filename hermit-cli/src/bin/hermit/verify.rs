/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use colored::Colorize;
use detcore::logdiff;
use hermit::Error;
use pretty_assertions::Comparison;
use reverie::process::ExitStatus;
use reverie::process::Output;
use tempfile::NamedTempFile;
use tempfile::TempPath;
use tracing::metadata::LevelFilter;

use super::global_opts::GlobalOpts;

pub fn temp_log_files(name1: &str, name2: &str) -> io::Result<(NamedTempFile, NamedTempFile)> {
    let file1 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name1))
        .rand_bytes(5)
        .tempfile()?;
    let file2 = tempfile::Builder::new()
        .prefix(&format!("{}_log_", name2))
        .rand_bytes(5)
        .tempfile()?;

    Ok((file1, file2))
}

pub fn setup_double_run(
    global: &GlobalOpts,
    name1: &str,
    name2: &str,
) -> ((GlobalOpts, NamedTempFile), (GlobalOpts, NamedTempFile)) {
    let (file1, file2) = temp_log_files(name1, name2).unwrap();

    let path1 = PathBuf::from(file1.path());
    let path2 = PathBuf::from(file2.path());

    // Override global settings.  Unfortunately we lose the log output to the
    // screen.
    let mut global = global.clone();
    global.log_file = Some(path1);
    global.log = Some(LevelFilter::DEBUG);

    let mut global2 = global.clone();
    global2.log_file = Some(path2);
    ((global, file1), (global2, file2))
}

/// Append a suffix to a path's file name (e.g. `/tmp/log` -> `/tmp/log.run2`).
/// This is used to distinguish the two per-run logs when the user requested a
/// persistent `--log-file`.
fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub fn compare_two_runs(
    out1: &Output,
    log1: TempPath,
    out2: &Output,
    log2: TempPath,
    success_msg: &str,
    failure_msg: &str,
    // If the user passed `--log-file`, the per-run logs are copied here so they
    // survive the temp-file cleanup that otherwise deletes them on success. The
    // first run is written to this exact path (so `--log-file=X` yields `X`) and
    // the second run to `X.run2`.
    keep_log_file: Option<&Path>,
) -> Result<ExitStatus, Error> {
    let mut failed = false;

    if out1.stdout != out2.stdout {
        failed = true;
        eprintln!("Mismatch in stdout between runs:",);
        let str1 = String::from_utf8_lossy(&out1.stdout);
        let str2 = String::from_utf8_lossy(&out2.stdout);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    if out1.stderr != out2.stderr {
        failed = true;
        eprintln!("Mismatch in stderr between runs:",);
        let str1 = String::from_utf8_lossy(&out1.stderr);
        let str2 = String::from_utf8_lossy(&out2.stderr);
        if str1.lines().count() > 1 {
            display_diff(&str1, &str2);
        } else {
            eprintln!("{}", Comparison::new(&str1, &str2));
        }
    }

    eprintln!(
        ":: {} {} and {}",
        "Comparing logs...".yellow().bold(),
        log1.display(),
        log2.display()
    );

    // TODO(T103558443) stripping logs until this task is completely closed:
    if logdiff::log_diff(
        log1.as_ref(),
        log2.as_ref(),
        &logdiff::LogDiffOpts {
            strip_lines: true,
            syscall_history: 5,
            ..Default::default()
        },
    ) {
        failed = true;
        eprintln!(":: {}", "Log differences found between runs.".red().bold());
        eprintln!(
            ":: {}: {} {}",
            "Respective Logs retained for further inspection".red(),
            log1.display(),
            log2.display()
        );
    }

    if out1.status != out2.status {
        failed = true;
        eprintln!(
            "Mismatch in exit status between runs: {}",
            Comparison::new(&out1.status, &out2.status)
        );
    }

    // If the user requested a persistent `--log-file`, copy the per-run logs
    // there before the temporary files are cleaned up. This runs in both the
    // success and failure cases so `--log-file` is always honored.
    if let Some(dest) = keep_log_file {
        let dest1 = dest.to_path_buf();
        let dest2 = append_suffix(dest, ".run2");
        let src1: &Path = log1.as_ref();
        let src2: &Path = log2.as_ref();
        fs::copy(src1, &dest1).map_err(|e| {
            Error::msg(format!(
                "Failed to copy verify log to {}: {}",
                dest1.display(),
                e
            ))
        })?;
        fs::copy(src2, &dest2).map_err(|e| {
            Error::msg(format!(
                "Failed to copy verify log to {}: {}",
                dest2.display(),
                e
            ))
        })?;
        eprintln!(
            ":: {} {} and {}",
            "Execution logs written to".green().bold(),
            dest1.display(),
            dest2.display()
        );
    }

    if failed {
        eprintln!(":: {}", failure_msg.red().bold());
        // On failure, also retain the temp logs at their original locations
        // (unless the user already has persistent copies via --log-file).
        if keep_log_file.is_none() {
            let _ = log1.keep()?;
            let _ = log2.keep()?;
        }
        Err(Error::msg(
            "Mismatch between run1 and run2 outputs (logs retained).",
        ))
    } else {
        // Allow the NamedTempFiles to be deleted in this case:
        eprintln!(":: {}", success_msg.green().bold());
        Ok(out2.status)
    }
}

fn display_diff(left: &str, right: &str) {
    for result in diff::lines(left, right) {
        match result {
            diff::Result::Left(s) => {
                eprintln!("- {}", s.red());
            }
            diff::Result::Right(s) => {
                eprintln!("+ {}", s.green());
            }
            diff::Result::Both(s, _) => {
                eprintln!("  {}", s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: ExitStatus::Exited(status),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    fn empty_logs() -> (TempPath, TempPath) {
        let (left, right) = temp_log_files("verify_left", "verify_right").unwrap();
        (left.into_temp_path(), right.into_temp_path())
    }

    #[test]
    fn identical_outputs_verify_successfully() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        assert_eq!(
            compare_two_runs(&left, log1, &right, log2, "verified", "failed", None).unwrap(),
            ExitStatus::Exited(0)
        );
    }

    #[test]
    fn log_file_destination_receives_both_run_logs() {
        let left = output(0, b"hello\n", b"");
        let right = left.clone();
        let (log1, log2) = empty_logs();

        let dest_dir = tempfile::tempdir().unwrap();
        let dest = dest_dir.path().join("keep.log");
        let dest_run2 = super::append_suffix(&dest, ".run2");

        assert_eq!(
            compare_two_runs(&left, log1, &right, log2, "verified", "failed", Some(&dest)).unwrap(),
            ExitStatus::Exited(0)
        );

        // On success the temp logs are cleaned up, but the requested --log-file
        // destination (and its .run2 sibling) must persist so `--log-file` is
        // honored even under `--verify`.
        assert!(
            dest.is_file(),
            "run1 log should be copied to the exact --log-file path: {}",
            dest.display()
        );
        assert!(
            dest_run2.is_file(),
            "run2 log should be copied to <--log-file>.run2: {}",
            dest_run2.display()
        );
    }

    #[test]
    fn stdout_stderr_and_status_mismatches_fail_verification() {
        let baseline = output(0, b"hello\n", b"");
        let mismatches = [
            output(0, b"different\n", b""),
            output(0, b"hello\n", b"different\n"),
            output(1, b"hello\n", b""),
        ];

        for mismatch in mismatches {
            let (log1, log2) = empty_logs();
            let path1 = log1.to_path_buf();
            let path2 = log2.to_path_buf();

            assert!(
                compare_two_runs(&baseline, log1, &mismatch, log2, "verified", "failed", None)
                    .is_err()
            );

            let _ = fs::remove_file(path1);
            let _ = fs::remove_file(path2);
        }
    }
}
