/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

// Keep examples with manifest-disabled SaBRe verify cells out of this ratchet.
// TODO(#1519): SaBRe does not yet determinize timed-progress-bar.py's busy-wait
// polling on virtual wall-clock advancement.
// TODO(#2895): SaBRe strict verification of rand.py retains different syscall
// streams across its two runs. Other enabled backends still cover both guests.
const NON_RACY_EXAMPLES: [&str; 2] = ["date.sh", "devrand.sh"];
const SABRE_BACKEND_FACT_PREFIX: &str = ":: Backend: sabre static rewriting + ptrace runtime;";
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";
const HERMETIC_TEST_WORKDIR: &str = "/test";
const COMPARISON_EPOCH: &str = "2026-09-23T02:39:52.970859833+00:00";
const NEXT_NANOSECOND_EPOCH: &str = "2026-09-23T02:39:52.970859834+00:00";

fn unique_sabre_backend_fact_field<'a>(fact: &'a str, name: &str) -> Option<&'a str> {
    let mut values = fact.split(';').filter_map(|field| {
        let (field_name, value) = field.trim().split_once('=')?;
        (field_name == name).then_some(value)
    });
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn unique_sabre_backend_fact_line(diagnostics: &str) -> Result<&str, String> {
    let mut facts = diagnostics
        .lines()
        .filter(|line| line.contains(SABRE_BACKEND_FACT_PREFIX));
    let Some(fact) = facts.next() else {
        return Err("expected exactly one SaBRe backend fact, found 0".to_owned());
    };
    let additional = facts.count();
    if additional != 0 {
        return Err(format!(
            "expected exactly one SaBRe backend fact, found {}",
            additional + 1
        ));
    }
    Ok(fact)
}

fn sabre_backend_fact_is_exercised(fact: &str) -> bool {
    unique_sabre_backend_fact_field(fact, "evidence_schema") == Some("1")
        && unique_sabre_backend_fact_field(fact, "ptrace_fallback_sites") == Some("0")
        && unique_sabre_backend_fact_field(fact, "trusted_shared_object_sites") == Some("0")
        && unique_sabre_backend_fact_field(fact, "guest_rpc_observed") == Some("true")
        && unique_sabre_backend_fact_field(fact, "reach_state") == Some("sabre-exercised")
}

fn sabre_backend_fact_reached_detcore(fact: &str) -> bool {
    unique_sabre_backend_fact_field(fact, "evidence_schema") == Some("1")
        && unique_sabre_backend_fact_field(fact, "ptrace_fallback_sites") == Some("0")
        && unique_sabre_backend_fact_field(fact, "guest_rpc_observed") == Some("true")
        && unique_sabre_backend_fact_field(fact, "reach_state") == Some("sabre-exercised")
}

fn assert_sabre_backend_fact(diagnostics: &str, label: &str) {
    let fact = unique_sabre_backend_fact_line(diagnostics).unwrap_or_else(|error| {
        panic!("SaBRe controller diagnostics did not contain exactly one backend fact for {label}: {error}\n{diagnostics}")
    });
    assert!(
        sabre_backend_fact_is_exercised(fact),
        "SaBRe backend fact did not prove schema-1 RPC reach with zero ptrace fallback for {label}:\n{fact}",
    );
}

#[test]
fn sabre_backend_fact_refuses_unreached_or_fallback_execution() {
    let complete = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=0; trusted_shared_object_sites=0; \
         guest_rpc_observed=true; reach_state=sabre-exercised"
    );
    let false_reach = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=0; guest_rpc_observed=false; \
         reach_state=no-detcore-reached"
    );
    let fallback = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=1; guest_rpc_observed=true; \
         reach_state=degraded-ptrace-fallback"
    );
    let trusted_escape = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=0; trusted_shared_object_sites=1; \
         guest_rpc_observed=true; reach_state=sabre-exercised"
    );
    let contradictory_reach = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=0; guest_rpc_observed=true; \
         guest_rpc_observed=false; reach_state=sabre-exercised"
    );
    let contradictory_fallback = format!(
        "{SABRE_BACKEND_FACT_PREFIX} evidence_schema=1; \
         ptrace_fallback_sites=0; ptrace_fallback_sites=1; \
         guest_rpc_observed=true; reach_state=sabre-exercised"
    );
    assert_eq!(unique_sabre_backend_fact_line(&complete).unwrap(), complete);
    let missing = unique_sabre_backend_fact_line("controller diagnostics without a backend fact")
        .unwrap_err();
    assert!(missing.contains("found 0"), "{missing}");
    for (label, diagnostics) in [
        ("valid plus unreached", format!("{complete}\n{false_reach}")),
        ("valid plus fallback", format!("{complete}\n{fallback}")),
        ("duplicate valid", format!("{complete}\n{complete}")),
    ] {
        let error = unique_sabre_backend_fact_line(&diagnostics).unwrap_err();
        assert!(
            error.contains("found 2"),
            "{label} did not fail as ambiguous: {error}"
        );
    }
    assert!(sabre_backend_fact_reached_detcore(&complete));
    assert!(sabre_backend_fact_is_exercised(&complete));
    for (label, fact) in [
        ("unreached", false_reach),
        ("fallback", fallback),
        ("contradictory reach", contradictory_reach),
        ("contradictory fallback", contradictory_fallback),
    ] {
        assert!(
            !sabre_backend_fact_reached_detcore(&fact),
            "{label} backend fact falsely proved RPC reach with zero fallback: {fact}",
        );
        assert!(
            !sabre_backend_fact_is_exercised(&fact),
            "{label} backend fact carried the complete exercised contract: {fact}",
        );
    }
    assert!(
        sabre_backend_fact_reached_detcore(&trusted_escape),
        "trusted shared-object execution still reached Detcore"
    );
    assert!(
        !sabre_backend_fact_is_exercised(&trusted_escape),
        "trusted shared-object escape falsely carried the complete exercised contract"
    );
}

fn assert_clock_progress_trajectory(output: &Output, backend_label: &str) {
    let rendered = std::str::from_utf8(&output.stdout)
        .unwrap_or_else(|error| panic!("{backend_label} trajectory was not UTF-8: {error}"))
        .trim();
    let mut fields = rendered.split_whitespace();
    assert_eq!(fields.next(), Some("clock-progress-deltas"));
    let samples: Vec<u64> = fields
        .map(|field| {
            field.parse().unwrap_or_else(|error| {
                panic!("{backend_label} emitted invalid clock delta {field:?}: {error}")
            })
        })
        .collect();
    assert_eq!(samples.len(), 8, "{backend_label} omitted clock samples");
    assert_eq!(samples[0], 0, "{backend_label} trajectory origin moved");
    assert!(
        samples.windows(2).all(|pair| pair[0] < pair[1]),
        "{backend_label} clock trajectory froze or rewound: {samples:?}",
    );
}

fn execution_root_args(requested: Option<&OsStr>) -> Result<Vec<OsString>, String> {
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

#[test]
fn sabre_non_racy_example_baseline_is_exact() {
    assert_eq!(NON_RACY_EXAMPLES, ["date.sh", "devrand.sh"]);
}

#[test]
fn sabre_pinned_root_arguments_are_exact_and_fail_closed() {
    assert!(execution_root_args(None).unwrap().is_empty());
    assert_eq!(
        execution_root_args(Some(OsStr::new("/test"))).unwrap(),
        [
            OsString::from("--base-env=minimal"),
            OsString::from("--mount=type=tmpfs,target=/test"),
            OsString::from("--workdir=/test"),
        ]
    );
    let error = execution_root_args(Some(OsStr::new("/tmp"))).unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));

    let command = example_command_with_execution_root(
        Path::new("/bin/true"),
        &[],
        COMPARISON_EPOCH,
        None,
        false,
        (None, None),
        Some(OsStr::new("/test")),
    )
    .unwrap();
    let args: Vec<_> = command.get_args().collect();
    assert_eq!(
        command_epoch_arguments(&command),
        [format!("--epoch={COMPARISON_EPOCH}")],
    );
    assert!(args.contains(&OsStr::new("--base-env=minimal")));
    assert!(args.windows(2).any(|args| {
        args == [
            OsStr::new("--mount=type=tmpfs,target=/test"),
            OsStr::new("--workdir=/test"),
        ]
    }));

    let error = example_command_with_execution_root(
        Path::new("/bin/true"),
        &[],
        COMPARISON_EPOCH,
        None,
        false,
        (None, None),
        Some(OsStr::new("/tmp")),
    )
    .unwrap_err();
    assert!(error.contains("HERMIT_E2E_EMPTY_WORKDIR must be /test"));
}

fn command_epoch_arguments(command: &Command) -> Vec<String> {
    command
        .get_args()
        .filter_map(|argument| {
            argument
                .to_str()
                .filter(|argument| argument.starts_with("--epoch="))
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn sabre_comparison_reuses_one_explicit_epoch_for_parity_and_repeatability() {
    let commands = [
        example_command(
            Path::new("/bin/true"),
            &[],
            COMPARISON_EPOCH,
            None,
            false,
            None,
            None,
        ),
        example_command(
            Path::new("/bin/true"),
            &[],
            COMPARISON_EPOCH,
            Some(Path::new("/sabre")),
            false,
            None,
            None,
        ),
        example_command(
            Path::new("/bin/true"),
            &[],
            COMPARISON_EPOCH,
            None,
            false,
            None,
            None,
        ),
    ];
    for command in &commands {
        assert_eq!(
            command_epoch_arguments(command),
            [format!("--epoch={COMPARISON_EPOCH}")],
        );
    }
}

#[test]
fn sabre_distinct_explicit_epochs_remain_distinct() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    for (backend, label) in [(None, "ptrace"), (Some(loader.as_path()), "SaBRe")] {
        let first = parity_run(
            Path::new("/bin/date"),
            &["+%s.%N"],
            COMPARISON_EPOCH,
            backend,
            &format!("{label} first explicit epoch"),
        );
        let second = parity_run(
            Path::new("/bin/date"),
            &["+%s.%N"],
            NEXT_NANOSECOND_EPOCH,
            backend,
            &format!("{label} next-nanosecond explicit epoch"),
        );
        let observed = |output: &Output| {
            let text = std::str::from_utf8(&output.stdout).unwrap().trim();
            let (seconds, nanos) = text
                .split_once('.')
                .unwrap_or_else(|| panic!("{label} date output lacked nanoseconds: {text}"));
            seconds.parse::<u128>().unwrap() * 1_000_000_000 + nanos.parse::<u128>().unwrap()
        };
        assert_eq!(
            observed(&second) - observed(&first),
            1,
            "{label} guest observation collapsed two explicit epochs one nanosecond apart",
        );
    }
}

fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_SABRE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

fn sabre_loader() -> Option<PathBuf> {
    let hermit = hermit_binary();
    let executable_dir = hermit.parent().expect("Hermit binary should have a parent");
    let target_dir = executable_dir
        .parent()
        .expect("Hermit binary should be inside a profile directory");
    let configured = std::env::var_os("HERMIT_SABRE_BINARY").map(PathBuf::from);
    let loader = configured
        .clone()
        .unwrap_or_else(|| target_dir.join("sabre/sabre"));
    let plugin = executable_dir.join("libdetcore_sabre.so");
    let revision_file = loader.with_file_name("sabre.revision");
    if !loader.is_file() || !plugin.is_file() {
        if configured.is_some() {
            panic!(
                "configured SaBRe artifacts are unavailable: loader={}, plugin={}, revision={}",
                loader.display(),
                plugin.display(),
                revision_file.display(),
            );
        }
        eprintln!(
            "skipping SaBRe example parity: artifacts are unavailable: loader={}, plugin={}, revision={}",
            loader.display(),
            plugin.display(),
            revision_file.display(),
        );
        return None;
    }

    let revision = if revision_file.is_file() {
        std::fs::read_to_string(&revision_file).unwrap_or_else(|error| {
            panic!(
                "failed to read SaBRe revision provenance {}: {error}",
                revision_file.display(),
            )
        })
    } else if configured.is_some() {
        panic!(
            "configured SaBRe revision provenance is unavailable: loader={}, plugin={}, revision={}",
            loader.display(),
            plugin.display(),
            revision_file.display(),
        )
    } else {
        "unavailable".to_owned()
    };
    let digest = Command::new("sha256sum")
        .arg(&loader)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|output| output.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unavailable".to_owned());
    eprintln!(
        "SaBRe loader: path={}, revision={}, sha256={digest}",
        loader.display(),
        revision.trim(),
    );
    Some(loader)
}

fn kill_process_group(pid: u32, label: &str) {
    let result = unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    if result == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            panic!("failed to kill {label} process group {pid}: {error}");
        }
    }
}

fn controller_diagnostics(path: Option<&Path>) -> String {
    path.map(|path| {
        std::fs::read_to_string(path).unwrap_or_else(|error| {
            format!(
                "failed to read controller diagnostics {}: {error}",
                path.display()
            )
        })
    })
    .unwrap_or_else(|| "unavailable".to_owned())
}

fn run_bounded(mut command: Command, label: &str, diagnostic_log: Option<&Path>) -> Output {
    // Verification observes the guest's current directory. The source checkout is shared by
    // concurrent validation nodes, so Cargo or another test can change its metadata or entries
    // between Run1 and Run2. Keep this invocation in one empty directory until both runs and
    // their comparison have completed.
    let working_directory = tempfile::Builder::new()
        .prefix("sabre-working-directory-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| panic!("failed to create {label} working directory: {error}"));
    command
        .current_dir(working_directory.path())
        // Bash validates inherited PWD and OLDPWD by statting their path components. Leave them
        // unset so it obtains the same directory through getcwd without observing mutable
        // ancestors of this validation checkout.
        .env_remove("PWD")
        .env_remove("OLDPWD")
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rendered = format!("{command:?}");
    let started = Instant::now();
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) if started.elapsed() >= Duration::from_secs(45) => {
                kill_process_group(child.id(), label);
                break true;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                kill_process_group(child.id(), label);
                let _ = child.wait();
                panic!(
                    "failed to poll {label}: {rendered}: {error}\ncontroller diagnostics:\n{}",
                    controller_diagnostics(diagnostic_log),
                );
            }
        }
    };
    // Hermit should drain its process tree before exiting. Kill any survivors anyway so leaked
    // guest processes cannot retain a pipe descriptor and hang `wait_with_output`.
    kill_process_group(child.id(), label);
    let output = child.wait_with_output().unwrap_or_else(|error| {
        panic!(
            "failed to collect {label}: {rendered}: {error}\ncontroller diagnostics:\n{}",
            controller_diagnostics(diagnostic_log),
        )
    });
    if timed_out || !output.status.success() {
        panic!(
            "{label} failed: {rendered}\nstatus: {}\ntimed out: {timed_out}\nstdout:\n{}\nstderr:\n{}\ncontroller diagnostics:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            controller_diagnostics(diagnostic_log),
        );
    }
    output
}

fn example_command(
    example: &Path,
    args: &[&str],
    epoch: &str,
    backend: Option<&Path>,
    verify: bool,
    diagnostic_log: Option<&Path>,
    retained_verify_log_dir: Option<&Path>,
) -> Command {
    let requested = std::env::var_os(ISOLATED_WORKDIR_ENV);
    example_command_with_execution_root(
        example,
        args,
        epoch,
        backend,
        verify,
        (diagnostic_log, retained_verify_log_dir),
        requested.as_deref(),
    )
    .unwrap_or_else(|error| panic!("PATH-CONTRACT: {error}"))
}

fn example_command_with_execution_root(
    example: &Path,
    args: &[&str],
    epoch: &str,
    backend: Option<&Path>,
    verify: bool,
    output_paths: (Option<&Path>, Option<&Path>),
    requested_workdir: Option<&OsStr>,
) -> Result<Command, String> {
    example_command_with_epoch_source(
        example,
        args,
        EpochSource::ExplicitCli(epoch),
        backend,
        verify,
        output_paths,
        requested_workdir,
    )
}

#[derive(Clone, Copy)]
enum EpochSource<'a> {
    ExplicitCli(&'a str),
    Environment(&'a str),
}

fn example_command_with_epoch_source(
    example: &Path,
    args: &[&str],
    epoch_source: EpochSource<'_>,
    backend: Option<&Path>,
    verify: bool,
    output_paths: (Option<&Path>, Option<&Path>),
    requested_workdir: Option<&OsStr>,
) -> Result<Command, String> {
    let (diagnostic_log, retained_verify_log_dir) = output_paths;
    let mut command = Command::new(hermit_binary());
    command.arg(if verify { "--log=info" } else { "--log=warn" });
    if let Some(path) = diagnostic_log {
        command.arg("--log-file").arg(path);
    }
    command.arg("run");
    if let Some(loader) = backend {
        command
            .env("HERMIT_SABRE_BINARY", loader)
            .args(["--backend", "sabre"]);
    }
    command.args([
        "--strict",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    match epoch_source {
        EpochSource::ExplicitCli(epoch) => {
            command.arg(format!("--epoch={epoch}"));
        }
        EpochSource::Environment(epoch) => {
            command.env("HERMIT_EPOCH", epoch);
        }
    }
    if verify {
        command.arg("--verify");
        if let Some(directory) = retained_verify_log_dir {
            command
                .args(["--verify-strict", "--keep-logs", "--verify-log-dir"])
                .arg(directory)
                .arg("--verify-json")
                .arg(directory.join("verify.json"));
        }
    }
    command.args(execution_root_args(requested_workdir)?);
    command.arg("--").arg(example).args(args);
    Ok(command)
}

fn epoch_diagnostic(epoch: &str) -> String {
    format!("hermit: virtual-time epoch={epoch} source=explicit; reproduce with --epoch={epoch}")
}

fn assert_epoch_diagnostic(output: &Output, diagnostics: &str, epoch: &str, label: &str) {
    let expected = epoch_diagnostic(epoch);
    assert_eq!(
        diagnostics.matches(&expected).count(),
        1,
        "{label} must record exactly one explicit-epoch provenance event:\n{diagnostics}",
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("virtual-time epoch="),
        "{label} leaked controller epoch provenance into guest stderr:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn parity_run(
    example: &Path,
    args: &[&str],
    epoch: &str,
    backend: Option<&Path>,
    label: &str,
) -> Output {
    parity_run_with_path_contract(example, args, epoch, backend, label, false)
}

fn parity_run_with_path_contract(
    example: &Path,
    args: &[&str],
    epoch: &str,
    backend: Option<&Path>,
    label: &str,
    require_no_escape_sites: bool,
) -> Output {
    // Hermit gives the guest a private /tmp, so keep the controller sidecar in the host-visible
    // Cargo target directory. The freshly created unique file prevents stale or cross-run logs.
    let diagnostic_log = tempfile::Builder::new()
        .prefix("sabre-parity-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| panic!("failed to create {label} diagnostic log: {error}"));
    // Keep the already-produced evidence through success and assertion failure.
    // These host test artifacts never enter either compared guest stream.
    let (_diagnostic_file, diagnostic_log) = diagnostic_log
        .keep()
        .unwrap_or_else(|error| panic!("failed to retain {label} diagnostic log: {error}"));
    eprintln!(
        "SaBRe parity controller log for {label}: {}",
        diagnostic_log.display()
    );
    let output = run_bounded(
        example_command(
            example,
            args,
            epoch,
            backend,
            false,
            Some(&diagnostic_log),
            None,
        ),
        label,
        Some(&diagnostic_log),
    );
    let diagnostics = controller_diagnostics(Some(&diagnostic_log));
    let guest_stderr = String::from_utf8_lossy(&output.stderr);
    assert_epoch_diagnostic(&output, &diagnostics, epoch, label);

    // Positive control: every SaBRe run emits the structured backend fact into
    // the controller sidecar. Negative controls: ptrace emits no SaBRe fact,
    // and neither backend lets that controller fact leak into captured guest
    // stderr. Guest stderr itself is still compared byte-for-byte below.
    if backend.is_some() {
        if require_no_escape_sites {
            assert_sabre_backend_fact(&diagnostics, label);
        } else {
            let fact = unique_sabre_backend_fact_line(&diagnostics).unwrap_or_else(|error| {
                panic!("SaBRe controller diagnostics did not contain exactly one backend fact for {label}: {error}\n{diagnostics}")
            });
            assert!(
                sabre_backend_fact_reached_detcore(fact),
                "SaBRe backend fact did not prove RPC reach for {label}:\n{fact}",
            );
        }
    } else {
        assert!(
            !diagnostics.contains(SABRE_BACKEND_FACT_PREFIX),
            "ptrace controller diagnostics unexpectedly carried a SaBRe fact for {label}:\n{diagnostics}",
        );
    }
    assert!(
        !guest_stderr.contains(SABRE_BACKEND_FACT_PREFIX),
        "controller backend fact leaked into captured guest stderr for {label}:\n{guest_stderr}",
    );
    output
}

#[test]
fn sabre_public_epoch_environment_reaches_matching_plugin() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let diagnostic = tempfile::Builder::new()
        .prefix("sabre-epoch-env-")
        .tempfile_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let (_file, path) = diagnostic.keep().unwrap();
    let command = example_command_with_epoch_source(
        Path::new("/bin/true"),
        &[],
        EpochSource::Environment(COMPARISON_EPOCH),
        Some(&loader),
        false,
        (Some(&path), None),
        None,
    )
    .unwrap();
    assert!(command_epoch_arguments(&command).is_empty());
    let output = run_bounded(command, "SaBRe HERMIT_EPOCH public-input run", Some(&path));
    let diagnostics = controller_diagnostics(Some(&path));
    assert_epoch_diagnostic(&output, &diagnostics, COMPARISON_EPOCH, "HERMIT_EPOCH run");
    assert_sabre_backend_fact(&diagnostics, "HERMIT_EPOCH run");
}

fn assert_controller_diagnostics_do_not_hide_guest_stderr(loader: &Path, epoch: &str) {
    let args = ["-c", "printf 'guest-stderr-control\\n' >&2"];
    let ptrace = parity_run(
        Path::new("/bin/sh"),
        &args,
        epoch,
        None,
        "ptrace controller/guest stderr separation control",
    );
    let sabre = parity_run(
        Path::new("/bin/sh"),
        &args,
        epoch,
        Some(loader),
        "SaBRe controller/guest stderr separation control",
    );

    let expected = "guest-stderr-control\n";
    assert_eq!(
        ptrace.stderr,
        expected.as_bytes(),
        "ptrace hid or rewrote its diagnostic or guest stderr",
    );
    assert_eq!(
        sabre.stderr,
        expected.as_bytes(),
        "SaBRe hid or rewrote its diagnostic or guest stderr",
    );
    assert_eq!(
        sabre.stderr, ptrace.stderr,
        "controller-diagnostic routing must not weaken guest stderr parity",
    );
}

fn assert_backend_parity_and_sabre_verify(
    program: &Path,
    args: &[&str],
    epoch: &str,
    loader: &Path,
    label: &str,
) {
    let ptrace = parity_run(
        program,
        args,
        epoch,
        None,
        &format!("ptrace strict portable reference for {label}"),
    );
    let sabre = parity_run(
        program,
        args,
        epoch,
        Some(loader),
        &format!("SaBRe strict portable parity run for {label}"),
    );
    assert_eq!(
        sabre.status.code(),
        ptrace.status.code(),
        "example: {label}"
    );
    assert_eq!(sabre.stdout, ptrace.stdout, "stdout parity: {label}");
    assert_eq!(sabre.stderr, ptrace.stderr, "stderr parity: {label}");

    assert_sabre_verify(program, args, epoch, loader, label);
}

fn assert_sabre_verify(program: &Path, args: &[&str], epoch: &str, loader: &Path, label: &str) {
    let retained_logs = tempfile::Builder::new()
        .prefix("sabre-canonical-verify-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap_or_else(|error| {
            panic!("failed to create canonical verification directory for {label}: {error}")
        })
        .keep();
    eprintln!(
        "SaBRe canonical verification artifacts for {label}: {}",
        retained_logs.display()
    );
    let verify = run_bounded(
        example_command(
            program,
            args,
            epoch,
            Some(loader),
            true,
            None,
            Some(&retained_logs),
        ),
        &format!("SaBRe strict portable verification for {label}"),
        None,
    );
    let expected_provenance = epoch_diagnostic(epoch);
    let verify_stderr = String::from_utf8_lossy(&verify.stderr);
    assert_eq!(
        verify_stderr.matches(&expected_provenance).count(),
        1,
        "SaBRe verify must expose exactly one top-level epoch reproducer for {label}:\n{verify_stderr}",
    );
    let mut comparison_logs = 0;
    for entry in std::fs::read_dir(&retained_logs)
        .unwrap_or_else(|error| panic!("failed to read retained logs for {label}: {error}"))
    {
        let path = entry.unwrap().path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with("run1_log_") || name.starts_with("run2_log_") {
            let diagnostics = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "failed to read retained verify log {}: {error}",
                    path.display()
                )
            });
            assert!(
                !diagnostics.contains("virtual-time epoch="),
                "SaBRe comparison log must exclude controller epoch provenance for {label}:\n{diagnostics}",
            );
            comparison_logs += 1;
        }
    }
    assert_eq!(
        comparison_logs, 2,
        "SaBRe verify must retain one comparison log per physical run for {label}",
    );
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    assert!(
        diagnostics.contains("Success: deterministic. Determinism verified."),
        "SaBRe verifier omitted its success verdict for {label}:\n{diagnostics}",
    );
    assert!(
        diagnostics.contains("SaBRe syscall DETLOG records included: run1="),
        "SaBRe verifier omitted its syscall DETLOG inclusion count for {label}:\n{diagnostics}",
    );

    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(retained_logs.join("verify.json")).unwrap_or_else(|error| {
            panic!("SaBRe verifier omitted its JSON report for {label}: {error}")
        }),
    )
    .unwrap_or_else(|error| panic!("SaBRe verifier wrote invalid JSON for {label}: {error}"));
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["verdict"] == "matched"
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["compare_logs"] == true
            && report["comparison"]["log_scope"] == "info",
        "SaBRe verification was not canonical matched INFO parity for {label}: {report}",
    );
}

fn assert_three_run_determinism(
    program: &Path,
    args: &[&str],
    epoch: &str,
    backend: Option<&Path>,
    backend_label: &str,
    label: &str,
) -> Output {
    let baseline = parity_run(
        program,
        args,
        epoch,
        backend,
        &format!("{backend_label} strict portable run 1 for {label}"),
    );
    for run in 2..=3 {
        let repeated = parity_run(
            program,
            args,
            epoch,
            backend,
            &format!("{backend_label} strict portable run {run} for {label}"),
        );
        assert_eq!(
            repeated.stdout, baseline.stdout,
            "{backend_label} stdout changed across strict runs: {label}, run {run}",
        );
        assert_eq!(
            repeated.stderr, baseline.stderr,
            "{backend_label} stderr changed across strict runs: {label}, run {run}",
        );
    }
    baseline
}

fn assert_date_output_is_sane(output: &Output, backend_label: &str) {
    let rendered = std::str::from_utf8(&output.stdout)
        .unwrap_or_else(|error| panic!("{backend_label} date output was not UTF-8: {error}"))
        .trim();
    let (date_and_time, nanos) = rendered
        .rsplit_once('_')
        .unwrap_or_else(|| panic!("{backend_label} date output lacked nanoseconds: {rendered}"));
    assert_eq!(
        date_and_time.len(),
        19,
        "{backend_label} date/time shape was unexpected: {rendered}",
    );
    assert!(
        nanos.len() == 9 && nanos.bytes().all(|byte| byte.is_ascii_digit()),
        "{backend_label} nanoseconds were malformed: {rendered}",
    );
}

#[test]
fn sabre_root_pid_matches_ptrace() {
    let Some(loader) = sabre_loader() else {
        return;
    };

    assert_controller_diagnostics_do_not_hide_guest_stderr(&loader, COMPARISON_EPOCH);

    // The SaBRe ptrace safety net must not consume the root guest's namespace PID before launch.
    // `printf` is a shell builtin, so this observes the root shell rather than a forked utility.
    assert_backend_parity_and_sabre_verify(
        Path::new("/bin/sh"),
        &["-c", "printf 'pid=%s\\n' \"$$\""],
        COMPARISON_EPOCH,
        &loader,
        "root-pid",
    );
}

#[test]
fn sabre_scheduler_empty_info_precedes_fallback_completed_info() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let retained_logs = tempfile::Builder::new()
        .prefix("sabre-verify-logs-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create retained SaBRe verification log directory")
        .keep();
    eprintln!(
        "SaBRe scheduler verification artifacts: {}",
        retained_logs.display()
    );
    let verify = run_bounded(
        example_command(
            Path::new("/bin/sh"),
            &["-c", "printf 'ok\\n'"],
            COMPARISON_EPOCH,
            Some(&loader),
            true,
            None,
            Some(&retained_logs),
        ),
        "SaBRe strict verification with retained logs",
        None,
    );
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr),
    );
    assert!(
        diagnostics.contains("Success: deterministic. Determinism verified."),
        "SaBRe strict verification omitted its success verdict:\n{diagnostics}",
    );

    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(retained_logs.join("verify.json"))
            .expect("SaBRe strict verification did not publish its report"),
    )
    .expect("SaBRe strict verification report was not valid JSON");
    assert!(
        report["verified"] == true
            && report["bitwise_parity"] == true
            && report["comparison"]["strictness"] == "canonical"
            && report["comparison"]["log_scope"] == "info",
        "SaBRe verification report was not canonical bitwise INFO parity: {report}",
    );

    let retained_log = |prefix: &str| {
        let mut matches = std::fs::read_dir(&retained_logs)
            .expect("failed to read retained SaBRe verification log directory")
            .map(|entry| {
                entry
                    .expect("failed to read retained SaBRe verification log entry")
                    .path()
            })
            .filter(|path| {
                path.is_file()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(prefix))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "SaBRe strict verification must retain exactly one {prefix} file: {matches:?}",
        );
        matches.pop().unwrap()
    };
    let logs = [retained_log("run1_log_"), retained_log("run2_log_")];

    const SCHEDULER_EMPTY: &str =
        " INFO detcore::scheduler: [scheduler] run queue empty, exiting sched_loop.";
    const FALLBACK_COMPLETED: &str =
        " INFO hermit::sabre::fallback: SaBRe ptrace fallback completed";
    for (index, path) in logs.iter().enumerate() {
        let log = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!(
                "failed to read retained run log {}: {error}",
                path.display()
            )
        });
        let scheduler_empty = log.match_indices(SCHEDULER_EMPTY).collect::<Vec<_>>();
        let fallback_completed = log.match_indices(FALLBACK_COMPLETED).collect::<Vec<_>>();
        assert_eq!(
            scheduler_empty.len(),
            1,
            "retained run {} must contain exactly one scheduler-empty INFO:\n{log}",
            index + 1,
        );
        assert_eq!(
            fallback_completed.len(),
            1,
            "retained run {} must contain exactly one fallback-completed INFO:\n{log}",
            index + 1,
        );
        assert!(
            scheduler_empty[0].0 < fallback_completed[0].0,
            "retained run {} logged fallback completion before scheduler completion:\n{log}",
            index + 1,
        );
    }
}

#[test]
fn sabre_non_racy_examples_verify_current_envelope() {
    let Some(loader) = sabre_loader() else {
        return;
    };
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");

    // race.sh is deliberately outside this ratchet: its output is the schedule itself, and the
    // in-process backend does not yet serialize arbitrary guest instructions between callbacks.
    // Both parity sides use the portable profile because this job intentionally runs without PMU
    // or CPUID-faulting support. Route Hermit diagnostics to failure-only sidecars while comparing
    // guest output; strict handling remains enabled, and SaBRe verification keeps info logs.
    for name in NON_RACY_EXAMPLES {
        let program = repository.join("examples").join(name);
        if name == "date.sh" {
            // Removing the #1095 exec reset intentionally exposes that ptrace and
            // SaBRe currently accumulate different, but individually deterministic,
            // continuous clock trajectories. Do not restore fake all-zero parity.
            // Cross-backend trajectory alignment is tracked by task
            // `cross-backend-continuous-clock-trajectory-parity`.
            let ptrace =
                assert_three_run_determinism(&program, &[], COMPARISON_EPOCH, None, "ptrace", name);
            let sabre = assert_three_run_determinism(
                &program,
                &[],
                COMPARISON_EPOCH,
                Some(&loader),
                "SaBRe",
                name,
            );
            assert_date_output_is_sane(&ptrace, "ptrace");
            assert_date_output_is_sane(&sabre, "SaBRe");
            assert_sabre_verify(&program, &[], COMPARISON_EPOCH, &loader, name);
        } else {
            assert_backend_parity_and_sabre_verify(&program, &[], COMPARISON_EPOCH, &loader, name);
        }
    }

    // A matching one-shot timestamp could conceal a frozen clock. This guest
    // requires CLOCK_MONOTONIC to advance across deterministic syscall work.
    let guest_dir = tempfile::Builder::new()
        .prefix("sabre-clock-progress-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create SaBRe clock-progress guest directory");
    let clock_progress = guest_dir.path().join("clock-progress");
    let build = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/liteinst_advanced.c"))
        .arg("-o")
        .arg(&clock_progress)
        .output()
        .expect("failed to compile clock-progress guest");
    assert!(
        build.status.success(),
        "clock-progress guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );
    let ptrace_trajectory = assert_three_run_determinism(
        &clock_progress,
        &["clock-progress"],
        COMPARISON_EPOCH,
        None,
        "ptrace",
        "clock-progress",
    );
    let sabre_trajectory = assert_three_run_determinism(
        &clock_progress,
        &["clock-progress"],
        COMPARISON_EPOCH,
        Some(&loader),
        "SaBRe",
        "clock-progress",
    );
    assert_clock_progress_trajectory(&ptrace_trajectory, "ptrace");
    assert_clock_progress_trajectory(&sabre_trajectory, "SaBRe");
    let complete_path = parity_run_with_path_contract(
        &clock_progress,
        &["clock-progress"],
        COMPARISON_EPOCH,
        Some(&loader),
        "SaBRe clock-progress complete path evidence",
        true,
    );
    assert_clock_progress_trajectory(&complete_path, "SaBRe complete-path");
    assert_sabre_verify(
        &clock_progress,
        &["clock-progress"],
        COMPARISON_EPOCH,
        &loader,
        "clock-progress",
    );
}

#[test]
fn sabre_libc_getrandom_is_deterministic() {
    let Some(loader) = sabre_loader() else {
        return;
    };

    // Compile a hermetic caller of the public glibc getrandom function. Host
    // patch packages do not share one implementation: some call the public
    // symbol that SaBRe detours, while others reach an internal libc alias or
    // raw syscall site. Those package-specific paths belong in the measured
    // compatibility corpus rather than this portable mechanism test.
    let guest_dir = tempfile::Builder::new()
        .prefix("sabre-libc-getrandom-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create SaBRe libc-getrandom guest directory");
    let source = guest_dir.path().join("libc-getrandom.c");
    let program = guest_dir.path().join("libc-getrandom");
    std::fs::write(
        &source,
        r#"#include <errno.h>
#include <stdio.h>
#include <sys/random.h>

int main(void) {
  unsigned char bytes[16];
  if (getrandom(bytes, sizeof(bytes), 0) != sizeof(bytes)) return 1;
  for (unsigned int i = 0; i < sizeof(bytes); i++) printf("%02x", bytes[i]);
  putchar('\n');
  errno = 0;
  if (getrandom(bytes, sizeof(bytes), 0x80000000u) != -1 || errno != EINVAL)
    return 2;
  return 0;
}
"#,
    )
    .expect("failed to write SaBRe libc-getrandom guest source");
    let build = Command::new("cc")
        .args(["-O2", "-g", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&program)
        .output()
        .expect("failed to compile SaBRe libc-getrandom guest");
    assert!(
        build.status.success(),
        "libc-getrandom guest compilation failed:\n{}",
        String::from_utf8_lossy(&build.stderr),
    );

    assert_backend_parity_and_sabre_verify(
        &program,
        &[],
        COMPARISON_EPOCH,
        &loader,
        "public libc getrandom",
    );
}
