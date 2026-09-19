/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end L2 coverage for standard command-line tools that are expected on
//! the portable CI runner.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const HERMIT_VERIFY_TIMEOUT: &str = "60s";
const HERMIT_VERIFY_KILL_AFTER: &str = "10s";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const VERIFY_RESULT_ROOT_ENV: &str = "E2E_RESULT_ROOT";
const VERIFY_FILE_LIMIT: &str = "--fsize=67108864:67108864";
const VERIFY_FILE_LIMIT_BYTES: u64 = 67108864;

/// The caller owns this explicit artifact root beyond the test and checkout
/// lifetime. Official validation bind-mounts its durable E2E_RESULT_ROOT here;
/// a standalone caller must provide a durable root, with no temporary fallback.
fn retained_verify_directory(
    requested: Option<&OsStr>,
    temporary_roots: &[&Path],
) -> Result<PathBuf, String> {
    let requested = requested.filter(|value| !value.is_empty()).ok_or_else(|| {
        format!("{VERIFY_RESULT_ROOT_ENV} must name an explicit durable artifact directory")
    })?;
    let requested = Path::new(requested);
    if !requested.is_absolute() {
        return Err(format!(
            "{VERIFY_RESULT_ROOT_ENV} must be absolute: {requested:?}"
        ));
    }
    let root = requested.canonicalize().map_err(|error| {
        format!("cannot resolve {VERIFY_RESULT_ROOT_ENV} {requested:?}: {error}")
    })?;
    if !root.is_dir() {
        return Err(format!(
            "{VERIFY_RESULT_ROOT_ENV} is not a directory: {root:?}"
        ));
    }
    for temporary in temporary_roots {
        let temporary = temporary.canonicalize().map_err(|error| {
            format!("cannot resolve temporary test directory {temporary:?}: {error}")
        })?;
        if root.starts_with(&temporary) {
            return Err(format!(
                "{VERIFY_RESULT_ROOT_ENV} is inside temporary test storage: {root:?}"
            ));
        }
    }
    let group = root.join("command-strict-verify");
    match std::fs::DirBuilder::new().mode(0o700).create(&group) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&group).map_err(|error| {
                format!("cannot inspect retained log directory {group:?}: {error}")
            })?;
            if !metadata.file_type().is_dir() {
                return Err(format!(
                    "retained log directory is not a real directory: {group:?}"
                ));
            }
        }
        Err(error) => {
            return Err(format!(
                "cannot create retained log directory {group:?}: {error}"
            ));
        }
    }
    // Keep only the artifact directory. HOME and workdir still have their normal
    // TempDir cleanup, including assertion unwinding after a failed comparison.
    tempfile::Builder::new()
        .prefix("comparison-")
        .tempdir_in(&group)
        .map(tempfile::TempDir::keep)
        .map_err(|error| {
            format!("cannot allocate retained verification logs in {group:?}: {error}")
        })
}

/// The kernel file bound can precede the logger's own truncation marker. Even
/// an exit-zero comparison must not qualify a capped or missing retained pair.
fn check_retained_log_pair(directory: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot scan retained logs {directory:?}: {error}"))?;
    let mut sides = [0, 0];
    for entry in entries {
        let entry = entry
            .map_err(|error| format!("cannot read retained log entry in {directory:?}: {error}"))?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect retained log {path:?}: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err(format!("retained log is not a regular file: {path:?}"));
        }
        if metadata.len() == 0 {
            return Err(format!("retained log is empty: {path:?}"));
        }
        if metadata.len() >= VERIFY_FILE_LIMIT_BYTES {
            return Err(format!(
                "retained log reached the file-size limit; completeness is unknown: {path:?}"
            ));
        }
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b"run1_log_") {
            sides[0] += 1;
        } else if name.as_encoded_bytes().starts_with(b"run2_log_") {
            sides[1] += 1;
        } else {
            return Err(format!("unexpected retained log entry: {path:?}"));
        }
    }
    if sides != [1, 1] {
        return Err(format!(
            "expected both retained verification logs in {directory:?}, found {sides:?}"
        ));
    }
    Ok(())
}

fn strict_verify_command(hermit: &OsStr, directory: &Path) -> Command {
    // The same per-file bound used by the isolated-workdir validation controls.
    // It covers explicit log files as well as other regular files. A cap hit is
    // a failed command and must pass through the unchanged status assertion.
    let mut command = Command::new("prlimit");
    command
        .args([
            VERIFY_FILE_LIMIT,
            "--",
            "timeout",
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(hermit)
        .args([
            "--log=info",
            "run",
            "--strict",
            "--verify",
            "--keep-logs",
            "--verify-log-dir",
        ])
        .arg(directory);
    command
}

struct StrictCommandCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: &'static [&'static str],
    stdin: Option<&'static [u8]>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn required_command(case: &StrictCommandCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "ERROR: required command {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn pinned_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
    match requested {
        None => Ok(Vec::new()),
        Some(value) if value == OsStr::new(HERMETIC_TEST_WORKDIR) => Ok(vec![
            "--base-env=minimal".into(),
            "--mount=type=tmpfs,target=/test".into(),
            "--workdir=/test".into(),
        ]),
        Some(value) => Err(format!(
            "{ISOLATED_WORKDIR_ENV} must be {HERMETIC_TEST_WORKDIR}, got {value:?}"
        )),
    }
}

fn configure_pinned_root(command: &mut Command) {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    let args = pinned_root_args(requested.as_deref())
        .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"));
    command.args(args);
}

fn assert_l2_under_strict_verify(case: &StrictCommandCase) {
    let program = required_command(case);
    let home = tempfile::tempdir().expect("failed to create isolated command HOME");
    std::fs::create_dir_all(home.path().join(".config/procps"))
        .expect("failed to preseed the isolated procps HOME");
    let working_directory = tempfile::Builder::new()
        .prefix("command-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create isolated command working directory");
    let requested = std::env::var_os(VERIFY_RESULT_ROOT_ENV);
    let retained = retained_verify_directory(
        requested.as_deref(),
        &[
            home.path(),
            working_directory.path(),
            Path::new(env!("CARGO_TARGET_TMPDIR")),
        ],
    )
    .unwrap_or_else(|error| panic!("VERIFY-LOG-RETENTION: {error}"));
    eprintln!("{} verification logs: {}", case.name, retained.display());
    let mut command = strict_verify_command(OsStr::new(env!("CARGO_BIN_EXE_hermit")), &retained);
    command
        .arg(format!("--env=HOME={}", home.path().display()))
        .arg(format!(
            "--env=XDG_CONFIG_HOME={}",
            home.path().join(".config").display()
        ));
    configure_pinned_root(&mut command);
    command
        .arg("--")
        .arg(&program)
        .args(case.args)
        .env("HOME", home.path())
        .current_dir(working_directory.path())
        .stdin(if case.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let rendered = format!("{command:?}");
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    if let Some(input) = case.stdin {
        child
            .stdin
            .take()
            .expect("piped stdin should be available")
            .write_all(input)
            .unwrap_or_else(|error| panic!("failed to write stdin for {rendered}: {error}"));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("failed to collect {rendered}: {error}"));
    assert_strict_verify_output(case, &rendered, &output);
    check_retained_log_pair(&retained)
        .unwrap_or_else(|error| panic!("VERIFY-LOG-RETENTION: {error}"));
}

fn assert_strict_verify_output(case: &StrictCommandCase, rendered: &str, output: &Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} did not reach L2 under strict verification ({rendered})\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "{} exited 0 without Hermit's determinism marker ({rendered})\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + standard Unix command tools"]
fn common_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "ls",
            candidates: &["/usr/bin/ls", "/bin/ls"],
            args: &["-1", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "cat",
            candidates: &["/usr/bin/cat", "/bin/cat"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "wc",
            candidates: &["/usr/bin/wc", "/bin/wc"],
            args: &["-l", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "head",
            candidates: &["/usr/bin/head", "/bin/head"],
            args: &["-n", "3", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sort",
            candidates: &["/usr/bin/sort", "/bin/sort"],
            args: &[],
            stdin: Some(b"gamma\nalpha\nbeta\n"),
        },
        StrictCommandCase {
            name: "uniq",
            candidates: &["/usr/bin/uniq", "/bin/uniq"],
            args: &["/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tail",
            candidates: &["/usr/bin/tail", "/bin/tail"],
            args: &["-n", "3", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "env",
            candidates: &["/usr/bin/env", "/bin/env"],
            args: &["-i", "HERMIT_COMMAND_COMPAT=1"],
            stdin: None,
        },
        StrictCommandCase {
            name: "date",
            candidates: &["/usr/bin/date", "/bin/date"],
            args: &["-u", "+%s"],
            stdin: None,
        },
        StrictCommandCase {
            name: "id",
            candidates: &["/usr/bin/id", "/bin/id"],
            args: &["-u"],
            stdin: None,
        },
        StrictCommandCase {
            name: "hostname",
            candidates: &["/usr/bin/hostname", "/bin/hostname"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "uname",
            candidates: &["/usr/bin/uname", "/bin/uname"],
            args: &["-a"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tr",
            candidates: &["/usr/bin/tr", "/bin/tr"],
            args: &["a-z", "A-Z"],
            stdin: Some(b"hello hermit\n"),
        },
        StrictCommandCase {
            name: "cut",
            candidates: &["/usr/bin/cut", "/bin/cut"],
            args: &["-d:", "-f1", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "tee",
            candidates: &["/usr/bin/tee", "/bin/tee"],
            args: &["/dev/null"],
            stdin: Some(b"tee-through-hermit\n"),
        },
        StrictCommandCase {
            name: "diff",
            candidates: &["/usr/bin/diff", "/bin/diff"],
            args: &["/etc/hostname", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "grep",
            candidates: &["/usr/bin/grep", "/bin/grep"],
            args: &["-m", "1", "root", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sed",
            candidates: &["/usr/bin/sed", "/bin/sed"],
            args: &["-n", "1,3p", "/etc/passwd"],
            stdin: None,
        },
        StrictCommandCase {
            name: "find",
            candidates: &["/usr/bin/find", "/bin/find"],
            args: &[
                "/etc",
                "-maxdepth",
                "1",
                "-type",
                "f",
                "-name",
                "hostname",
                "-print",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "xargs",
            candidates: &["/usr/bin/xargs", "/bin/xargs"],
            args: &["echo"],
            stdin: Some(b"one two three\n"),
        },
        StrictCommandCase {
            name: "basename",
            candidates: &["/usr/bin/basename", "/bin/basename"],
            args: &["/tmp/hermit-example.txt", ".txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "dirname",
            candidates: &["/usr/bin/dirname", "/bin/dirname"],
            args: &["/tmp/hermit-example.txt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "realpath",
            candidates: &["/usr/bin/realpath", "/bin/realpath"],
            args: &["/etc/../etc/passwd"],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        // TODO-HUMAN-REVIEW(#877)
        StrictCommandCase {
            name: "readlink mount namespace",
            candidates: &["/usr/bin/readlink", "/bin/readlink"],
            args: &["/proc/self/ns/mnt"],
            stdin: None,
        },
        StrictCommandCase {
            name: "readlink executable control",
            candidates: &["/usr/bin/readlink", "/bin/readlink"],
            args: &["/proc/self/exe"],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        StrictCommandCase {
            name: "Python PID namespace readlink",
            candidates: &["/usr/bin/python3"],
            args: &[
                "-c",
                "import os; d=os.open('/', os.O_RDONLY); \
                 print(os.readlink('/proc/self/ns/pid', dir_fd=d))",
            ],
            stdin: None,
        },
        // AUTONOMOUS-BOT-IMPLEMENTED
        StrictCommandCase {
            name: "Perl user namespace readlink",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &["-e", "print readlink('/proc/self/ns/user'), qq(\\n)"],
            stdin: None,
        },
        StrictCommandCase {
            name: "md5sum",
            candidates: &["/usr/bin/md5sum", "/bin/md5sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sha256sum",
            candidates: &["/usr/bin/sha256sum", "/bin/sha256sum"],
            args: &["/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "du",
            candidates: &["/usr/bin/du", "/bin/du"],
            args: &["-b", "/etc/hostname"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sqlite3",
            candidates: &["/usr/bin/sqlite3", "/usr/local/bin/sqlite3"],
            args: &[
                ":memory:",
                "CREATE TABLE t(v); INSERT INTO t VALUES(3),(1),(2); \
                 SELECT group_concat(v, ',') FROM (SELECT v FROM t ORDER BY v);",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "awk",
            candidates: &["/usr/bin/awk", "/bin/awk"],
            args: &["BEGIN { for (i = 1; i <= 10; ++i) sum += i; print sum }"],
            stdin: None,
        },
        StrictCommandCase {
            name: "perl",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: &["-e", "print join(',', map { $_ * $_ } 1..5), qq(\n)"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

#[test]
fn pinned_root_arguments_are_exact_and_fail_closed() {
    assert!(pinned_root_args(None).unwrap().is_empty());
    assert_eq!(
        pinned_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );
    let error = pinned_root_args(Some(OsStr::new("/tmp"))).unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces + whoami/groups"]
fn identity_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "whoami",
            candidates: &["/usr/bin/whoami", "/bin/whoami"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "groups",
            candidates: &["/usr/bin/groups", "/bin/groups"],
            args: &[],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-843): Review strict process-accounting command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + procps-ng tools"]
fn process_accounting_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "ps aux",
            candidates: &["/usr/bin/ps", "/bin/ps"],
            args: &["aux"],
            stdin: None,
        },
        StrictCommandCase {
            name: "free -m",
            candidates: &["/usr/bin/free", "/bin/free"],
            args: &["-m"],
            stdin: None,
        },
        StrictCommandCase {
            name: "vmstat -s",
            candidates: &["/usr/bin/vmstat", "/bin/vmstat"],
            args: &["-s"],
            stdin: None,
        },
        StrictCommandCase {
            name: "top batch",
            candidates: &["/usr/bin/top", "/bin/top"],
            args: &["-b", "-n", "1", "-p", "1", "-w", "80"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-861): Review strict I/O-accounting command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + sysstat tools"]
fn io_accounting_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "iostat disk",
            candidates: &["/usr/bin/iostat"],
            args: &["-d", "-x", "1", "1"],
            stdin: None,
        },
        StrictCommandCase {
            name: "vmstat disk",
            candidates: &["/usr/bin/vmstat"],
            args: &["-d", "1", "2"],
            stdin: None,
        },
        StrictCommandCase {
            name: "pidstat disk",
            candidates: &["/usr/bin/pidstat"],
            args: &["-d", "-p", "1", "1", "1"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-873): Review kernel pseudo-file command coverage.
#[test]
#[ignore = "e2e: requires hermit + mount namespaces + util-linux/procps/sysstat"]
// TODO(#2791): Remove the portable-DAG exclusion after #2801 determinizes mountinfo.
fn kernel_pseudofile_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "findmnt",
            candidates: &["/usr/bin/findmnt", "/bin/findmnt"],
            args: &[
                "--kernel",
                "--list",
                "--output",
                "TARGET,SOURCE,FSTYPE,OPTIONS",
            ],
            stdin: None,
        },
        StrictCommandCase {
            name: "sysctl random UUID",
            candidates: &["/usr/sbin/sysctl", "/usr/bin/sysctl"],
            args: &["kernel.random.uuid"],
            stdin: None,
        },
        StrictCommandCase {
            name: "sar resource tables",
            candidates: &["/usr/bin/sar"],
            args: &["-v", "1", "1"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-881)
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + util-linux ionice"]
fn ionice_query_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "ionice current-process query",
        candidates: &["/usr/bin/ionice", "/bin/ionice"],
        args: &["-p", "0"],
        stdin: None,
    };
    assert_l2_under_strict_verify(&case);
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-883): Review interrupt and module command coverage.
#[test]
#[ignore = "e2e: requires hermit + util-linux/sysstat/kmod"]
fn kernel_activity_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "lsirq",
            candidates: &["/usr/bin/lsirq"],
            args: &["--noheadings", "--output", "IRQ,TOTAL,NAME"],
            stdin: None,
        },
        StrictCommandCase {
            name: "mpstat softirqs",
            candidates: &["/usr/bin/mpstat"],
            args: &["-I", "SCPU", "1", "1"],
            stdin: None,
        },
        StrictCommandCase {
            name: "lsmod",
            candidates: &["/usr/sbin/lsmod", "/usr/bin/lsmod"],
            args: &[],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-865): Review NUMA and sensor command coverage.
#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + numactl/lm_sensors tools"]
fn hardware_accounting_commands_are_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let cases = [
        StrictCommandCase {
            name: "numastat",
            candidates: &["/usr/bin/numastat"],
            args: &[],
            stdin: None,
        },
        StrictCommandCase {
            name: "numactl hardware",
            candidates: &["/usr/bin/numactl"],
            args: &["--hardware"],
            stdin: None,
        },
    ];

    for case in &cases {
        assert_l2_under_strict_verify(case);
    }
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_prlimit64_query_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 prlimit64 query",
        candidates: &["/usr/bin/python3"],
        args: &[],
        stdin: None,
    };
    let python = required_command(&case);
    let query = "import resource; print(resource.getrlimit(resource.RLIMIT_NOFILE))";
    let working_directory = tempfile::Builder::new()
        .prefix("python-prlimit64-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create isolated Python working directory");

    let mut strict_command = Command::new("timeout");
    strict_command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "run", "--strict"]);
    configure_pinned_root(&mut strict_command);
    let strict_output = strict_command
        .arg("--")
        .arg(&python)
        .args(["-c", query])
        .current_dir(working_directory.path())
        .output()
        .expect("failed to start Python prlimit64 strict-mode value regression");
    let strict_stdout = String::from_utf8_lossy(&strict_output.stdout);
    let strict_stderr = String::from_utf8_lossy(&strict_output.stderr);
    assert!(
        strict_output.status.success(),
        "Python prlimit64 strict-mode query failed: {}\n\
         stdout:\n{strict_stdout}\nstderr:\n{strict_stderr}",
        strict_output.status
    );
    assert_eq!(
        strict_stdout.trim(),
        "(1048576, 1048576)",
        "Python observed a non-deterministic RLIMIT_NOFILE value"
    );

    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=info", "run", "--strict", "--verify"]);
    configure_pinned_root(&mut command);
    let output = command
        .arg("--")
        .arg(&python)
        .args(["-c", query])
        .current_dir(working_directory.path())
        .output()
        .expect("failed to start Python prlimit64 strict/verify regression");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Python prlimit64 query did not reach L2 under strict verification: {}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "Hermit exited 0 without its determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + /usr/bin/python3"]
fn python_getrandom_is_deterministic_under_strict_verify() {
    let _guard = hermit_run_lock();
    let case = StrictCommandCase {
        name: "python3 getrandom",
        candidates: &["/usr/bin/python3"],
        args: &["-c", "import os; print(os.urandom(16).hex())"],
        stdin: None,
    };
    assert_l2_under_strict_verify(&case);
}

#[cfg(test)]
mod retention_tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn case() -> StrictCommandCase {
        StrictCommandCase {
            name: "retention plumbing control",
            candidates: &[],
            args: &[],
            stdin: None,
        }
    }

    fn fake_verifier(root: &Path, body: &str) -> PathBuf {
        let executable = root.join("fake-verifier");
        let script = format!(
            "#!/bin/sh\nset -eu\nlogs=\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = --verify-log-dir ]; then shift; logs=$1; fi\n  shift\ndone\ntest -n \"$logs\"\n{body}\n"
        );
        std::fs::write(&executable, script).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }

    #[test]
    fn missing_relative_or_unavailable_root_refuses_without_fallback() {
        for root in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert!(retained_verify_directory(root, &[]).is_err());
        }
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("missing");
        assert!(retained_verify_directory(Some(missing.as_os_str()), &[]).is_err());
        assert!(!missing.exists());
        let regular_file = fixture.path().join("file");
        std::fs::write(&regular_file, b"keep me").unwrap();
        assert!(retained_verify_directory(Some(regular_file.as_os_str()), &[]).is_err());
        assert_eq!(std::fs::read(regular_file).unwrap(), b"keep me");
    }

    #[test]
    fn temporary_roots_and_aliases_refuse_retention() {
        let fixture = tempfile::tempdir().unwrap();
        for name in ["home", "workdir", "target-tmp"] {
            let temporary = fixture.path().join(name);
            let root = temporary.join("nested");
            std::fs::create_dir_all(&root).unwrap();
            let alias = fixture.path().join(format!("{name}-alias"));
            std::os::unix::fs::symlink(&root, &alias).unwrap();
            for requested in [&root, &alias] {
                let error = retained_verify_directory(Some(requested.as_os_str()), &[&temporary])
                    .unwrap_err();
                assert!(error.contains("inside temporary test storage"));
            }
            assert!(!root.join("command-strict-verify").exists());
        }
    }

    #[test]
    fn explicit_persistent_checkout_artifacts_are_allowed_and_unique() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("persistent-checkout/ignored/artifacts");
        let home = tempfile::tempdir_in(fixture.path()).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let first = retained_verify_directory(Some(root.as_os_str()), &[home.path()]).unwrap();
        let second = retained_verify_directory(Some(root.as_os_str()), &[home.path()]).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with(&root) && second.starts_with(&root));
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn retained_group_cannot_redirect_back_into_temporary_home() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(home.path(), root.path().join("command-strict-verify")).unwrap();
        let error =
            retained_verify_directory(Some(root.path().as_os_str()), &[home.path()]).unwrap_err();
        assert!(error.contains("not a real directory"));
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    }

    #[test]
    fn failed_verifier_pair_survives_home_and_workdir_unwinding() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("durable-artifacts");
        std::fs::create_dir(&root).unwrap();
        let home = tempfile::tempdir_in(fixture.path()).unwrap();
        let workdir = tempfile::tempdir_in(fixture.path()).unwrap();
        let home_path = home.path().to_owned();
        let workdir_path = workdir.path().to_owned();
        let logs =
            retained_verify_directory(Some(root.as_os_str()), &[home.path(), workdir.path()])
                .unwrap();
        let executable = fake_verifier(
            fixture.path(),
            "printf 'left evidence' >\"$logs/run1_log_control\"\nprintf 'right evidence' >\"$logs/run2_log_control\"\nprintf 'Determinism verified\\n'\nexit 17",
        );
        let mut command = strict_verify_command(executable.as_os_str(), &logs);
        let output = command
            .current_dir(workdir.path())
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(17));
        assert!(String::from_utf8_lossy(&output.stdout).contains("Determinism verified"));
        let refusal = std::panic::catch_unwind(move || {
            let _home = home;
            let _workdir = workdir;
            assert_strict_verify_output(&case(), "fake verifier: explicit failure", &output);
        });
        assert!(
            refusal.is_err(),
            "a success marker must not hide failed status"
        );
        assert!(!home_path.exists() && !workdir_path.exists());
        assert_eq!(
            std::fs::read(logs.join("run1_log_control")).unwrap(),
            b"left evidence"
        );
        assert_eq!(
            std::fs::read(logs.join("run2_log_control")).unwrap(),
            b"right evidence"
        );
    }

    #[test]
    fn retention_keeps_exact_verification_timeout_and_comparator_arguments() {
        let command =
            strict_verify_command(OsStr::new("/prepared/hermit"), Path::new("/durable/logs"));
        assert_eq!(command.get_program(), "prlimit");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--fsize=67108864:67108864",
                "--",
                "timeout",
                "--kill-after",
                "10s",
                "60s",
                "/prepared/hermit",
                "--log=info",
                "run",
                "--strict",
                "--verify",
                "--keep-logs",
                "--verify-log-dir",
                "/durable/logs",
            ]
        );
    }

    #[test]
    fn successful_marker_cannot_hide_a_capped_retained_pair() {
        let fixture = tempfile::tempdir().unwrap();
        let logs = retained_verify_directory(Some(fixture.path().as_os_str()), &[]).unwrap();
        let executable = fake_verifier(
            fixture.path(),
            "truncate -s 67108864 -- \"$logs/run1_log_control\"\nprintf 'right evidence' >\"$logs/run2_log_control\"\nprintf 'Determinism verified\\n'",
        );
        let output = strict_verify_command(executable.as_os_str(), &logs)
            .output()
            .unwrap();
        assert_strict_verify_output(&case(), "fake verifier: capped success marker", &output);
        let error = check_retained_log_pair(&logs).unwrap_err();
        assert!(error.contains("reached the file-size limit"));
        assert!(logs.is_dir());
        let left = logs.join("run1_log_control");
        let right = logs.join("run2_log_control");
        assert_eq!(
            std::fs::metadata(&left).unwrap().len(),
            VERIFY_FILE_LIMIT_BYTES
        );
        assert_eq!(std::fs::read(&right).unwrap(), b"right evidence");
        // The same real pair below the bound qualifies; reaching the boundary,
        // not a permanent failure fixture, discriminates the opposing control.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&left)
            .unwrap()
            .set_len(VERIFY_FILE_LIMIT_BYTES - 1)
            .unwrap();
        check_retained_log_pair(&logs).unwrap();
    }

    #[test]
    fn successful_marker_cannot_hide_an_empty_retained_pair() {
        let fixture = tempfile::tempdir().unwrap();
        let logs = retained_verify_directory(Some(fixture.path().as_os_str()), &[]).unwrap();
        let executable = fake_verifier(
            fixture.path(),
            ": >\"$logs/run1_log_control\"\n: >\"$logs/run2_log_control\"\nprintf 'Determinism verified\\n'",
        );
        let output = strict_verify_command(executable.as_os_str(), &logs)
            .output()
            .unwrap();
        assert_strict_verify_output(&case(), "fake verifier: empty success marker", &output);
        let error = check_retained_log_pair(&logs).unwrap_err();
        assert!(error.contains("retained log is empty"));
        let left = logs.join("run1_log_control");
        let right = logs.join("run2_log_control");
        assert_eq!(std::fs::metadata(&left).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(&right).unwrap().len(), 0);
        std::fs::write(&left, b"left evidence").unwrap();
        std::fs::write(&right, b"right evidence").unwrap();
        check_retained_log_pair(&logs).unwrap();
    }

    #[test]
    fn retained_pair_scan_errors_or_missing_side_cannot_qualify() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("missing");
        assert!(
            check_retained_log_pair(&missing)
                .unwrap_err()
                .contains("cannot scan")
        );
        let logs = retained_verify_directory(Some(fixture.path().as_os_str()), &[]).unwrap();
        assert!(
            check_retained_log_pair(&logs)
                .unwrap_err()
                .contains("expected both")
        );
        let left = logs.join("run1_log_control");
        let right = logs.join("run2_log_control");
        std::fs::write(&left, b"left evidence").unwrap();
        assert!(
            check_retained_log_pair(&logs)
                .unwrap_err()
                .contains("expected both")
        );
        std::os::unix::fs::symlink(&left, &right).unwrap();
        assert!(
            check_retained_log_pair(&logs)
                .unwrap_err()
                .contains("not a regular file")
        );
        assert_eq!(std::fs::read(&left).unwrap(), b"left evidence");
        assert!(
            std::fs::symlink_metadata(&right)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn file_limit_refusal_cannot_become_verified_success() {
        let fixture = tempfile::tempdir().unwrap();
        let logs = retained_verify_directory(Some(fixture.path().as_os_str()), &[]).unwrap();
        let executable = fake_verifier(
            fixture.path(),
            "trap '' XFSZ\ntruncate -s 67108865 -- \"$logs/oversize\"\nprintf 'Determinism verified\\n'",
        );
        let output = strict_verify_command(executable.as_os_str(), &logs)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("File too large"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Determinism verified"));
        assert!(std::fs::metadata(logs.join("oversize")).unwrap().len() <= 67108864);
        assert!(
            std::panic::catch_unwind(|| {
                assert_strict_verify_output(&case(), "fake verifier: file-size limit", &output);
            })
            .is_err()
        );
    }
}
