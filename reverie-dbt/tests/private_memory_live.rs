/* Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * Licensed under the BSD-style license in the root LICENSE file.
 */

//! Actual native ownership callback controls, including followed-exec backing loss.

use std::path::Path;
use std::process::Command;

use reverie_dbt::DbtRunner;

fn observe(mode: &str, exec: bool) -> Vec<String> {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("private_memory_exec");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_memory_exec.c");
    let compile = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&program)
        .output()
        .unwrap();
    assert!(compile.status.success(), "{compile:?}");
    let runner = DbtRunner::from_env()
        .expect("built DynamoRIO and native client")
        .client_argument("-test-private-memory")
        .client_argument(mode);
    let mut command = Command::new(program);
    if exec {
        command.arg("exec");
    }
    let output = runner.output(&command).unwrap();
    eprintln!("private memory {mode}, exec={exec}: {output:?}");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"private memory guest completed\n");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let bootstrap: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("PRIVATE_MEMORY_BOOTSTRAP "))
        .collect();
    let coverage = "PRIVATE_MEMORY_BOOTSTRAP loads=4 bss=1 covered=1";
    let mut expected = vec![coverage.to_owned()];
    if mode == "ready" && exec {
        expected.push(coverage.to_owned());
    }
    if mode == "old-size-exec" || mode == "old-magic-exec" {
        let layout = if mode == "old-size-exec" { 1 } else { 2 };
        expected.push(format!(
            "PRIVATE_MEMORY_BOOTSTRAP old_layout={layout} injected=1"
        ));
    }
    if mode == "routes" {
        expected.push("PRIVATE_MEMORY_BOOTSTRAP derivation_controls=1".to_owned());
        for (name, notifications) in [
            ("bootstrap-left-edge", 1),
            ("bootstrap-right-edge", 1),
            ("bootstrap-interior", 1),
            ("bootstrap-left-crossing", 1),
            ("bootstrap-right-crossing", 1),
            ("bootstrap-left-adjacent", 0),
            ("bootstrap-right-adjacent", 0),
            ("bootstrap-fixed-overlap", 1),
            ("bootstrap-noreplace-overlap", 0),
            ("bootstrap-mremap-source", 1),
            ("bootstrap-mremap-destination", 1),
            ("bootstrap-gap-unmap", 0),
            ("bootstrap-gap-fixed", 0),
        ] {
            expected.push(format!(
                "PRIVATE_MEMORY_BOOTSTRAP route={name} notifications={notifications}"
            ));
        }
        expected.push("PRIVATE_MEMORY_BOOTSTRAP routes=13 physical_gap=1".to_owned());
    }
    assert_eq!(
        bootstrap, expected,
        "required bootstrap controls did not run"
    );
    stderr
        .lines()
        .filter(|line| line.starts_with("PRIVATE_MEMORY_TEST "))
        .map(str::to_owned)
        .collect()
}

const FIRST_READY: &str =
    "PRIVATE_MEMORY_TEST ready=1 followed_exec=0 wrong_owner_rejected=1 wrong_version_rejected=1";
const EXEC_READY: &str =
    "PRIVATE_MEMORY_TEST ready=1 followed_exec=1 wrong_owner_rejected=1 wrong_version_rejected=1";

#[test]
#[ignore = "requires a built DynamoRIO and native client; run explicitly with --ignored"]
fn private_memory_current_and_followed_exec_tokens_are_validated() {
    assert_eq!(observe("ready", true), [FIRST_READY, EXEC_READY]);
}

#[test]
#[ignore = "requires a built DynamoRIO and native client; run explicitly with --ignored"]
fn private_memory_retirement_rejects_saved_and_current_tokens() {
    assert_eq!(
        observe("retire", false),
        [FIRST_READY, "PRIVATE_MEMORY_TEST retired=1"]
    );
}

#[test]
#[ignore = "requires a built DynamoRIO and native client; run explicitly with --ignored"]
fn private_memory_followed_exec_with_lost_backing_is_unavailable() {
    // The native option explicitly injects transport loss before the real exec;
    // it does not claim to exercise a normal guest close admission route.
    assert_eq!(
        observe("lost-exec", true),
        [
            FIRST_READY,
            "PRIVATE_MEMORY_TEST control-backing-loss-injected=1",
            "PRIVATE_MEMORY_TEST followed-exec-unavailable=1",
        ]
    );
    for legacy in ["old-size-exec", "old-magic-exec"] {
        assert_eq!(
            observe(legacy, true),
            [
                FIRST_READY,
                "PRIVATE_MEMORY_TEST followed-exec-unavailable=1"
            ]
        );
    }
}

#[test]
#[ignore = "requires a built DynamoRIO and native client; run explicitly with --ignored"]
fn private_memory_mapping_routes_retire_only_on_replacement() {
    // Synthetic operands enter the same dispatcher/overlap path used before
    // native syscall transfer. No kernel operation unmaps the running runtime.
    let mut expected = vec![FIRST_READY.to_owned()];
    for (name, notifications) in [
        ("mmap-fixed-overlap", 1),
        ("mmap-nonfixed-overlap", 0),
        ("mmap-noreplace-overlap", 0),
        ("mmap-fixed-disjoint", 0),
        ("mmap-fixed-overflow", 1),
        ("munmap-overlap", 1),
        ("munmap-disjoint", 0),
        ("munmap-left-adjacent", 0),
        ("munmap-right-adjacent", 0),
        ("munmap-left-crossing", 1),
        ("munmap-right-crossing", 1),
        ("munmap-zero", 0),
        ("munmap-overflow", 1),
        ("mremap-source", 1),
        ("mremap-source-shrink", 1),
        ("mremap-source-growth", 1),
        ("mremap-zero-size-alias", 1),
        ("mremap-zero-extents", 0),
        ("mremap-fixed-destination", 1),
        ("mremap-ignored-destination", 0),
        ("mremap-disjoint", 0),
        ("mremap-source-overflow", 1),
        ("mremap-destination-overflow", 1),
        ("mremap-both-overlap", 2),
        ("mprotect-commit", 0),
        ("shmat-remap", 1),
        ("shmat-nonremap", 0),
    ] {
        expected.push(format!(
            "PRIVATE_MEMORY_TEST route={name} notifications={notifications}"
        ));
    }
    expected.push("PRIVATE_MEMORY_TEST routes=27 live_token=1".to_owned());
    assert_eq!(observe("routes", false), expected);
}

#[test]
#[ignore = "requires a built DynamoRIO SDK; run explicitly with --ignored"]
fn private_memory_observer_is_withdrawn_before_client_shutdown() {
    let temporary = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let directory = std::env::var_os("REVERIE_DBT_PRIVATE_MEMORY_SHUTDOWN_ARTIFACT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().join("shutdown"));
    std::fs::create_dir(&directory).expect("fresh shutdown fixture directory");
    let run = |name: &str, command: &mut Command| {
        let output = command.output().unwrap();
        std::fs::write(directory.join(format!("{name}.stdout")), &output.stdout).unwrap();
        std::fs::write(directory.join(format!("{name}.stderr")), &output.stderr).unwrap();
        std::fs::write(
            directory.join(format!("{name}.status")),
            format!("{:?}\n", output.status),
        )
        .unwrap();
        eprintln!("{name}: {command:?}: {output:?}");
        output
    };
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_memory_shutdown");
    let sdk = reverie_dbt::bundled_dynamorio_cmake_dir();
    let cmake = std::env::var_os("CMAKE").unwrap_or_else(|| "cmake".into());
    let configured = run(
        "configure",
        Command::new(&cmake)
            .arg("-S")
            .arg(source)
            .arg("-B")
            .arg(&directory)
            .arg(format!("-DDynamoRIO_DIR={}", sdk.display())),
    );
    assert!(configured.status.success(), "{configured:?}");
    let built = run(
        "build",
        Command::new(&cmake)
            .arg("--build")
            .arg(&directory)
            .args(["--parallel", "2"]),
    );
    assert!(built.status.success(), "{built:?}");
    let drrun = sdk.parent().unwrap().join("bin64/drrun");
    for (mode, code, stdout) in [
        ("exit101", 101, b"".as_slice()),
        (
            "normal",
            0,
            b"private memory shutdown guest completed\n".as_slice(),
        ),
    ] {
        let record = directory.join(format!("{mode}.record"));
        let file = std::fs::File::create(&record).unwrap();
        file.set_len(4096).unwrap();
        let output = run(
            mode,
            Command::new("timeout")
                .args(["--signal=TERM", "--kill-after=2s", "20s"])
                .arg(&drrun)
                .arg("-c")
                .arg(directory.join("libprivate_memory_shutdown_client.so"))
                .arg(mode)
                .arg(&record)
                .arg("--")
                .arg(directory.join("private_memory_shutdown_guest")),
        );
        let bytes = std::fs::read(record).unwrap();
        let words: Vec<_> = bytes[..64]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect();
        eprintln!("{mode} shared shutdown observations: {words:?}");
        assert_eq!(output.status.code(), Some(code), "{mode}: {output:?}");
        assert_eq!(output.stdout, stdout, "{mode}: complete guest output");
        assert_eq!(
            words,
            [0x5348_5554_444f_574e, 1, 1, 1, 1, 0, 1, 0],
            "{mode}: initialized guest, one final withdrawal, no callback after exit"
        );
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let observed: Vec<_> = stderr
            .lines()
            .filter(|line| line.starts_with("PRIVATE_MEMORY_SHUTDOWN "))
            .collect();
        let mut expected = Vec::new();
        if mode == "exit101" {
            expected.push("PRIVATE_MEMORY_SHUTDOWN requested-exit=101");
        }
        expected.push("PRIVATE_MEMORY_SHUTDOWN exit retirement=1 failed=0");
        assert_eq!(observed, expected, "{mode}: complete lifecycle diagnostics");
        assert!(!stderr.contains("Internal Error"), "{stderr}");
        assert!(!stderr.contains("SIGSEGV"), "{stderr}");
    }
}

fn child_test_directory(variable: &str, name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temporary = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let directory = std::env::var_os(variable)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| temporary.path().join(name));
    std::fs::create_dir(&directory).expect("fresh child ownership fixture directory");
    (temporary, directory)
}

fn child_test_run(directory: &Path, name: &str, command: &mut Command) -> std::process::Output {
    let output = command.output().unwrap();
    child_test_retain(directory, name, &output);
    eprintln!("{name}: {command:?}: {output:?}");
    output
}

fn child_test_retain(directory: &Path, name: &str, output: &std::process::Output) {
    std::fs::write(directory.join(format!("{name}.stdout")), &output.stdout).unwrap();
    std::fs::write(directory.join(format!("{name}.stderr")), &output.stderr).unwrap();
    std::fs::write(
        directory.join(format!("{name}.status")),
        format!("{:?}\n", output.status),
    )
    .unwrap();
}

fn child_test_words(bytes: &[u8]) -> Vec<u64> {
    bytes[..128]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect()
}

fn disconnect_observation(mode: &str) {
    let sdk = std::env::var_os("REVERIE_DBT_PRIVATE_OBSERVER_WITNESS_SDK")
        .map(std::path::PathBuf::from)
        .expect("build the private witness SDK using fixtures/private_memory_disconnect/README.md")
        .canonicalize()
        .expect("existing private witness SDK");
    let (_temporary, directory) =
        child_test_directory("REVERIE_DBT_PRIVATE_MEMORY_DISCONNECT_ARTIFACT_DIR", mode);
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_memory_disconnect");
    let cmake = std::env::var_os("CMAKE").unwrap_or_else(|| "cmake".into());
    let configured = child_test_run(
        &directory,
        "configure",
        Command::new(&cmake)
            .arg("-S")
            .arg(source)
            .arg("-B")
            .arg(&directory)
            .arg(format!("-DDynamoRIO_DIR={}", sdk.join("cmake").display())),
    );
    assert!(configured.status.success(), "{configured:?}");
    let built = child_test_run(
        &directory,
        "build",
        Command::new(&cmake)
            .arg("--build")
            .arg(&directory)
            .args(["--parallel", "2"]),
    );
    assert!(built.status.success(), "{built:?}");
    let record = directory.join("observation.record");
    let output = child_test_run(
        &directory,
        mode,
        Command::new("timeout")
            .args(["--signal=TERM", "--kill-after=2s", "20s"])
            .arg(directory.join("private_memory_disconnect_supervisor"))
            .arg(sdk.join("bin64/drrun"))
            .arg(directory.join("libprivate_memory_disconnect_client.so"))
            .arg(&record)
            .arg(directory.join("private_memory_disconnect_guest"))
            .arg(mode),
    );
    let bytes = std::fs::read(record).expect("actual shared observer record");
    let words: Vec<_> = bytes[..24 * 8]
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| u64::from_le_bytes(*word))
        .collect();
    eprintln!("{mode} private SDK disconnect observations: {words:?}");
    assert!(output.status.success(), "{mode}: {output:?}");
    assert!(output.stdout.is_empty(), "{mode}: unexpected guest output");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(stderr.contains("PRIVATE_MEMORY_DISCONNECT entered=1 completed=1 early=0 destroyed=1"));
    assert!(!stderr.contains("Internal Error"), "{stderr}");
    assert!(!stderr.contains("SIGSEGV"), "{stderr}");
}

#[test]
#[ignore = "requires the private witness SDK recipe in fixtures/private_memory_disconnect/README.md"]
fn private_memory_observer_disconnect_waits_for_client_callback() {
    disconnect_observation("close");
}

#[test]
#[ignore = "requires the private witness SDK recipe in fixtures/private_memory_disconnect/README.md"]
fn private_memory_observer_disconnect_resists_unrelated_suspension() {
    disconnect_observation("sync");
}

fn sdk_fork_observation(mode: &str) {
    let (_temporary, directory) =
        child_test_directory("REVERIE_DBT_PRIVATE_MEMORY_FORK_ARTIFACT_DIR", mode);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/private_memory_fork");
    let sdk = reverie_dbt::bundled_dynamorio_cmake_dir();
    let cmake = std::env::var_os("CMAKE").unwrap_or_else(|| "cmake".into());
    let configured = child_test_run(
        &directory,
        "configure",
        Command::new(&cmake)
            .arg("-S")
            .arg(source)
            .arg("-B")
            .arg(&directory)
            .arg(format!("-DDynamoRIO_DIR={}", sdk.display())),
    );
    assert!(configured.status.success(), "{configured:?}");
    let built = child_test_run(
        &directory,
        "build",
        Command::new(&cmake)
            .arg("--build")
            .arg(&directory)
            .args(["--parallel", "2"]),
    );
    assert!(built.status.success(), "{built:?}");
    let record = directory.join("observation.record");
    std::fs::File::create(&record)
        .unwrap()
        .set_len(4096)
        .unwrap();
    let drrun = sdk.parent().unwrap().join("bin64/drrun");
    let client = directory.join("libprivate_memory_fork_client.so");
    let guest = directory.join("private_memory_fork_guest");
    let mut command = Command::new("timeout");
    command.args(["--signal=TERM", "--kill-after=2s", "20s"]);
    if mode == "shared" {
        command
            .arg(directory.join("private_memory_fork_supervisor"))
            .arg(drrun)
            .arg(client)
            .arg(&record)
            .arg(guest);
    } else {
        command
            .arg(drrun)
            .arg("-c")
            .arg(client)
            .arg(mode)
            .arg(&record)
            .arg("--")
            .arg(guest)
            .arg(mode);
    }
    let output = child_test_run(&directory, mode, &mut command);
    let words = child_test_words(&std::fs::read(record).unwrap());
    eprintln!("{mode} SDK owner observations: {words:?}");
    assert!(output.status.success(), "{mode}: {output:?}");
    assert_eq!(output.stdout, b"private memory fork guest completed\n");
    assert_eq!(words[0], 0x464f_524b_4f57_4e52);
    assert!(words[1] > 0, "actual registering process");
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(!stderr.contains("Internal Error"), "{stderr}");
    assert!(!stderr.contains("SIGSEGV"), "{stderr}");
    let lines: Vec<_> = stderr
        .lines()
        .filter(|line| line.starts_with("PRIVATE_MEMORY_FORK "))
        .collect();
    if mode == "shared" {
        assert!(
            words[8] > 0 && words[8] != words[1],
            "actual last surviving child"
        );
        assert_eq!(
            words,
            [
                0x464f_524b_4f57_4e52,
                words[1],
                2,
                0,
                1,
                1,
                0,
                0,
                words[8],
                0,
                0,
                1,
                1,
                0,
                0,
                0
            ],
            "no premature notification, one final withdrawal by the last shared-mm task"
        );
        assert_eq!(
            lines,
            [
                "PRIVATE_MEMORY_FORK shared early=0 terminal=1 failed=0",
                "PRIVATE_MEMORY_FORK supervisor parent=0 child=0 reaped=1",
            ]
        );
    } else {
        assert_eq!(words[9], 1, "one real separate-mm fork callback");
        assert_eq!(
            words[10], 0,
            "child must not invoke inherited parent observer"
        );
        assert_eq!((words[11], words[12], words[7]), (1, 1, 0));
        assert_eq!(lines, ["PRIVATE_MEMORY_FORK separate delta=0 failed=0"]);
    }
}

#[test]
#[ignore = "requires a built DynamoRIO SDK; run explicitly with --ignored"]
fn separate_mm_child_does_not_inherit_parent_private_memory_observer() {
    sdk_fork_observation("separate");
}

#[test]
#[ignore = "requires a built DynamoRIO SDK; run explicitly with --ignored"]
fn last_shared_mm_task_withdraws_the_original_private_memory_observer() {
    sdk_fork_observation("shared");
}

fn native_child_observation(mode: &str) {
    use std::os::unix::fs::PermissionsExt;
    let (_temporary, directory) =
        child_test_directory("REVERIE_DBT_PRIVATE_MEMORY_CHILD_ARTIFACT_DIR", mode);
    let fixture = if mode == "refused" {
        "pid_namespace_admission"
    } else {
        "private_memory_child"
    };
    let guest = directory.join(fixture);
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{fixture}.c"));
    let compiled = child_test_run(
        &directory,
        "compile",
        Command::new(std::env::var_os("CC").unwrap_or_else(|| "cc".into()))
            .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest),
    );
    assert!(compiled.status.success(), "{compiled:?}");
    let record = directory.join("observation.record");
    std::fs::File::create(&record)
        .unwrap()
        .set_len(4096)
        .unwrap();
    let launcher = directory.join("bounded-drrun");
    let prefix = if mode == "refused" {
        "unshare --user --map-root-user --pid --fork --mount-proc "
    } else {
        ""
    };
    std::fs::write(&launcher, format!(
        "#!/bin/sh\nexec timeout --signal=TERM --kill-after=2s 10s {prefix}\"$REVERIE_DBT_TEST_DRRUN\" \"$@\"\n"
    )).unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let runner = DbtRunner::new(
        launcher,
        std::env::var_os("REVERIE_DBT_CLIENT").expect("built actual native client"),
    )
    .unwrap()
    .client_argument("-test-wait-for-background")
    .client_argument("-test-private-memory-child")
    .client_argument(mode)
    .client_argument(record.as_os_str())
    .private_environment_key("HERMIT_DBT_NOOP")
    .unwrap();
    let environment = [
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HERMIT_DBT_NOOP".into(), "1".into()),
        (
            "REVERIE_DBT_TEST_DRRUN".into(),
            reverie_dbt::bundled_drrun_path().as_os_str().to_os_string(),
        ),
        (
            "REVERIE_DBT_TEST_PID_NAMESPACE_CASE".into(),
            "separate-vm".into(),
        ),
    ]
    .into_iter()
    .collect();
    let output = runner
        .output_with_environment(&Command::new(guest), &environment)
        .unwrap();
    child_test_retain(&directory, mode, &output);
    let bytes = std::fs::read(record).unwrap();
    let words = child_test_words(&bytes);
    eprintln!("{mode} actual native owner observations: {words:?}; {output:?}");
    assert!(output.status.success(), "{mode}: {output:?}");
    assert_eq!(words[0], 0x4348_494c_444f_574e);
    assert_eq!(
        &words[1..4],
        [1, 1, 1],
        "parent Ready at all three actual boundaries"
    );
    assert!(
        words[4..7].iter().all(|value| *value <= 2),
        "raw eligibility, 2 means absent slot"
    );
    assert_eq!(
        &words[10..14],
        [1, 1, 1, 0],
        "both barriers and checked child"
    );
    let snapshot = |index: usize| &bytes[128 + index * 176..128 + (index + 1) * 176];
    assert_eq!(
        snapshot(1),
        snapshot(2),
        "parent token unchanged after real child wait"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    assert!(stderr.contains(&format!(
        "PRIVATE_MEMORY_CHILD mode={} before_ready=1 setup_ready=1 after_ready=1 ",
        if mode == "refused" { 1 } else { 2 }
    )));
    if mode == "refused" {
        assert_eq!(
            output.stdout,
            b"nested-pid-child-refused-before-application\n"
        );
        assert!(stderr.contains("refusing nested PID namespace task admission: container-observed child identity transport is not implemented"));
        assert_eq!(
            &words[7..10],
            [0, 0, 0],
            "refused child never owns a private token"
        );
    } else {
        assert_eq!(output.stdout, b"private memory child completed\n");
        assert_eq!(
            &words[7..10],
            [1, 0, 1],
            "real child observer retires child only before exit"
        );
        assert_ne!(
            &snapshot(1)[16..24],
            &snapshot(3)[16..24],
            "distinct real child owner slot"
        );
        assert_eq!(snapshot(4), [0; 176], "unavailable query must zero output");
        assert_eq!(
            snapshot(1),
            snapshot(5),
            "parent survives actual child callback"
        );
    }
    assert!(!stderr.contains("Internal Error"), "{stderr}");
    assert!(!stderr.contains("SIGSEGV"), "{stderr}");
}

#[test]
#[ignore = "requires built SDK/native client and user/PID namespaces; run explicitly with --ignored"]
fn refused_separate_mm_child_preserves_parent_private_memory_token() {
    native_child_observation("refused");
}

#[test]
#[ignore = "requires a built DynamoRIO SDK and native client; run explicitly with --ignored"]
fn accepted_separate_mm_child_installs_its_actual_private_memory_observer() {
    native_child_observation("accepted");
}
