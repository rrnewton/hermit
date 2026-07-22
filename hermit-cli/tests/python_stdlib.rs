/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

const MODULES: [&str; 5] = [
    "test.test_math",
    "test.test_string",
    "test.test_re",
    "test.test_json",
    "test.test_hashlib",
];

const UNITTEST_DRIVER: &str = r#"
import sys
import test.support
import unittest

test.support.use_resources = []
suite = unittest.defaultTestLoader.loadTestsFromNames(sys.argv[1:])
print("python-stdlib-modules=" + ",".join(sys.argv[1:]))
print(f"python-stdlib-cases={suite.countTestCases()}")
result = unittest.TextTestRunner(verbosity=2).run(suite)
print(f"python-stdlib-success={result.wasSuccessful()}")
raise SystemExit(not result.wasSuccessful())
"#;

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn python_interpreter() -> PathBuf {
    if let Some(path) = env::var_os("HERMIT_PYTHON") {
        return PathBuf::from(path);
    }

    // Some system python3 commands are launchers that use CLONE_VFORK. Ask the
    // launcher for the real interpreter before entering Hermit.
    let mut command = Command::new("python3");
    command.args(["-c", "import sys; print(sys.executable)"]);
    let output = command_output(command, "system Python interpreter discovery");
    let path = PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("system Python path was not UTF-8")
            .trim(),
    );
    assert!(
        path.is_file(),
        "resolved Python interpreter does not exist: {}",
        path.display()
    );
    path
}

fn run_stdlib_tests(python: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--strict",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(python)
        .args(["-c", UNITTEST_DRIVER])
        .args(MODULES);
    command_output(command, "Python stdlib tests under strict Hermit")
}

#[test]
#[ignore = "requires system CPython with its full Lib/test package"]
fn strict_python_stdlib_is_deterministic() {
    let _guard = HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let python = python_interpreter();

    let first = run_stdlib_tests(&python);
    let second = run_stdlib_tests(&python);

    assert_eq!(
        first.stdout, second.stdout,
        "Python stdlib stdout differed across strict runs"
    );
    assert_eq!(
        first.stderr, second.stderr,
        "Python stdlib stderr differed across strict runs"
    );

    let stdout = String::from_utf8(first.stdout).expect("Python stdlib stdout was not UTF-8");
    let case_count = stdout
        .lines()
        .find_map(|line| line.strip_prefix("python-stdlib-cases="))
        .expect("missing Python stdlib case count")
        .parse::<usize>()
        .expect("Python stdlib case count was not an integer");
    assert!(
        case_count >= 300,
        "expected substantial stdlib coverage, got {case_count} cases"
    );
    assert!(stdout.contains("python-stdlib-success=True"));
    for module in MODULES {
        assert!(
            stdout.contains(module),
            "missing module marker for {module}"
        );
    }

    let stderr = String::from_utf8(first.stderr).expect("Python stdlib stderr was not UTF-8");
    assert!(stderr.contains("\nOK"), "Python unittest did not report OK");
}
