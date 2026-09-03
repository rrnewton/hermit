use std::path::Path;
use std::process::Command;
use std::process::Output;

fn run(binary: &str, arguments: &[&str]) -> Output {
    run_from(binary, arguments, None)
}

fn run_from(binary: &str, arguments: &[&str], current_dir: Option<&Path>) -> Output {
    let mut command = Command::new(binary);
    command.args(arguments);
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    command.output().expect("failed to execute manifest CLI")
}

fn assert_help(binary: &str, name: &str, current_dir: &Path) {
    let short = run_from(binary, &["-h"], Some(current_dir));
    let long = run_from(binary, &["--help"], Some(current_dir));
    for output in [&short, &long] {
        assert!(
            output.status.success(),
            "{name} help failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with(&format!("Usage: {name}")), "{stdout}");
        assert!(output.stderr.is_empty(), "help wrote stderr: {output:?}");
    }
    assert_eq!(short.stdout, long.stdout, "-h and --help must agree");
}

#[test]
fn every_manifest_cli_has_conventional_help() {
    let non_repo = std::env::temp_dir().join(format!(
        "hermit-manifest-cli-help-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&non_repo).expect("create non-repository working directory");
    std::fs::write(non_repo.join(".git"), "deliberately not a Git directory\n")
        .expect("create an explicit non-repository boundary");
    let git_probe = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&non_repo)
        .output()
        .expect("run non-repository control");
    assert!(
        !git_probe.status.success(),
        "negative control is inside a Git repository: {git_probe:?}"
    );
    for (name, binary) in [
        ("test-harness", env!("CARGO_BIN_EXE_test-harness")),
        (
            "hermit-manifest-plan",
            env!("CARGO_BIN_EXE_hermit-manifest-plan"),
        ),
        (
            "strict-green-authority",
            env!("CARGO_BIN_EXE_strict-green-authority"),
        ),
        (
            "generate-test-footprints",
            env!("CARGO_BIN_EXE_generate-test-footprints"),
        ),
    ] {
        assert_help(binary, name, &non_repo);
    }
    std::fs::remove_dir_all(&non_repo).expect("remove non-repository working directory");
}

#[test]
fn test_harness_help_names_every_public_environment_control() {
    let output = run(env!("CARGO_BIN_EXE_test-harness"), &["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help must be UTF-8");
    for variable in [
        "E2E_RESULT_ROOT",
        "E2E_BUILD_ROOT",
        "E2E_RUN_ID",
        "E2E_RUN_INDEX",
        "E2E_MACHINE_SHORTNAME",
        "E2E_KERNEL_VERSION",
        "HERMIT_BIN",
        "HERMIT_E2E_EMPTY_WORKDIR",
        "E2E_KEEP_VERIFY_LOGS",
        "HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER",
        "HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER",
    ] {
        assert!(stdout.contains(variable), "help omitted {variable}");
    }
    for internal in ["DAGRUN_TEST_COUNTS_PATH", "HERMIT_E2E_SCHEDULED_JOBS"] {
        assert!(
            !stdout.contains(internal),
            "internal runner plumbing leaked into public help: {internal}"
        );
    }
}

#[test]
fn help_does_not_turn_missing_or_unknown_arguments_into_success() {
    let missing_command = run(env!("CARGO_BIN_EXE_test-harness"), &[]);
    assert_eq!(missing_command.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&missing_command.stderr);
    assert!(stderr.contains("missing command"), "{stderr}");
    assert!(stderr.contains("test-harness --help"), "{stderr}");

    let unknown_option = run(
        env!("CARGO_BIN_EXE_test-harness"),
        &["validate", "--definitely-unknown"],
    );
    assert_eq!(unknown_option.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown_option.stderr).contains("unknown option"),
        "{unknown_option:?}"
    );

    let missing_authority_args = run(env!("CARGO_BIN_EXE_strict-green-authority"), &[]);
    assert_eq!(missing_authority_args.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&missing_authority_args.stderr).contains("missing --claims"),
        "{missing_authority_args:?}"
    );

    let unknown_plan_option = run(
        env!("CARGO_BIN_EXE_hermit-manifest-plan"),
        &["--definitely-unknown"],
    );
    assert_eq!(unknown_plan_option.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&unknown_plan_option.stderr).contains("unknown argument"),
        "{unknown_plan_option:?}"
    );
}

#[test]
fn manifest_plan_no_arguments_remains_the_default_text_plan() {
    let output = run(env!("CARGO_BIN_EXE_hermit-manifest-plan"), &[]);
    assert!(
        output.status.success(),
        "default plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "default text plan was empty");
    assert!(!stdout.starts_with("Usage:"), "no arguments became help");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("manifest(s)"),
        "default plan omitted its validation summary: {output:?}"
    );
}
